//! Talking to a Nextcloud server over WebDAV: list, download, upload, create and delete folders.
//!
//! Built on `lightcraft_fetch::http` (pure Rust, no `ring`), one connection per request. Runs on
//! whatever thread calls it; every call takes the cancel flag and waits at most the stall
//! timeout for progress, so a dead server or a cancelled job never hangs the caller.
//!
//! What protects the account's app password:
//! - it is sent only to the server address that was given, never to a redirect: a `3xx` answer is
//!   an error ([`ClientError::Redirect`]) and nothing is sent anywhere else;
//! - plain `http://` is refused for anything but an address on the local network
//!   ([`ClientError::InsecureServer`]); [`Client::is_plain_http`] lets the caller show a warning
//!   for the local case, where Basic authentication crosses the network unencrypted;
//! - it never appears in an error message or in `Debug` output.
//!
//! What protects the data: a download goes to `<name>.part` and is renamed only when it has the
//! size the listing announced (and, if known, the ETag it announced); an upload carries
//! `If-Match`/`If-None-Match` so it cannot overwrite a change it has not seen.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lightcraft_fetch::http::{self, Body, HttpError, Limits, Request, Response, Url};

use crate::auth::Credentials;
use crate::dav::{self, DavError, DavRoot, Depth, Entry, Listing};
use crate::path::{PathError, RemotePath};

/// Timeouts and size limits.
#[derive(Clone, Debug)]
pub struct Options {
    /// Longest wait to connect.
    pub connect_timeout: Duration,
    /// Longest wait for the next bytes once connected.
    pub stall_timeout: Duration,
    /// Largest upload. The default stays under the 100 MB per request that Cloudflare lets
    /// through; raise it for a server reached directly on the local network.
    pub max_upload_bytes: u64,
    /// Largest download accepted.
    pub max_download_bytes: u64,
    /// Largest folder listing read.
    pub max_listing_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            connect_timeout: Duration::from_secs(10),
            stall_timeout: Duration::from_secs(30),
            max_upload_bytes: 95 << 20,
            max_download_bytes: 8 << 30,
            max_listing_bytes: 32 << 20,
        }
    }
}

/// Why a call failed. No variant ever holds the app password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The server address can't be used.
    BadServer(String),
    /// `http://` to a host that is not on the local network.
    InsecureServer(String),
    /// The network failed (connect, TLS, stall, protocol).
    Network(HttpError),
    /// 401: wrong user name or app password.
    BadCredentials,
    /// 403.
    Forbidden,
    /// 404.
    NotFound,
    /// 409: a folder above does not exist.
    Conflict,
    /// 412, or a file that differs from what was listed: it changed on the server.
    Changed,
    /// 423.
    Locked,
    /// 507: the account is out of space.
    NoSpace,
    /// Above the size limit, here or on the server (413).
    TooLarge {
        size: Option<u64>,
        limit: Option<u64>,
    },
    /// A redirect, which is not followed because credentials would go along.
    Redirect(String),
    /// Any other unexpected status.
    Server(u16),
    /// The answer could not be read.
    Dav(DavError),
    /// A local file problem.
    Io(String),
    /// The file being uploaded changed while it was sent.
    SourceChanged,
    /// The server sent a different number of bytes than the listing announced.
    SizeMismatch {
        expected: u64,
        got: u64,
    },
    Cancelled,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::BadServer(e) => write!(f, "unusable server address: {e}"),
            ClientError::InsecureServer(h) => write!(f, "{h} is not on the local network, so it needs an https:// address"),
            ClientError::Network(e) => write!(f, "{e}"),
            ClientError::BadCredentials => write!(f, "the server did not accept the user name or app password"),
            ClientError::Forbidden => write!(f, "the server refused this (no permission)"),
            ClientError::NotFound => write!(f, "not found on the server"),
            ClientError::Conflict => write!(f, "the folder above does not exist on the server"),
            ClientError::Changed => write!(f, "the file on the server has changed in the meantime"),
            ClientError::Locked => write!(f, "the file is locked on the server"),
            ClientError::NoSpace => write!(f, "the Nextcloud account is out of space"),
            ClientError::TooLarge { size: Some(s), limit: Some(l) } => write!(f, "{s} bytes is more than the {l} bytes allowed in one request"),
            ClientError::TooLarge { .. } => write!(f, "the file is too large for the server"),
            ClientError::Redirect(to) => write!(f, "the server redirected to {to}; check the server address (https://, no extra path)"),
            ClientError::Server(code) => write!(f, "unexpected answer from the server (HTTP {code})"),
            ClientError::Dav(e) => write!(f, "{e}"),
            ClientError::Io(e) => write!(f, "{e}"),
            ClientError::SourceChanged => write!(f, "the file changed while it was being uploaded; upload it again"),
            ClientError::SizeMismatch { expected, got } => write!(f, "the server sent {got} bytes, not the {expected} it announced"),
            ClientError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<HttpError> for ClientError {
    fn from(e: HttpError) -> Self {
        match e {
            HttpError::Cancelled => ClientError::Cancelled,
            other => ClientError::Network(other),
        }
    }
}

fn io_err(what: &str, e: std::io::Error) -> ClientError {
    ClientError::Io(format!("{what}: {e}"))
}

/// Whether `host` is an address or name on the local network, where `http://` is acceptable:
/// loopback, private and link-local addresses, `localhost`, `*.local` / `*.lan` / `*.home.arpa`,
/// and single-label names (a router or a Raspberry Pi called `dima`).
pub fn is_local_host(host: &str) -> bool {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
        };
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    !host.contains('.') || host.ends_with(".local") || host.ends_with(".lan") || host.ends_with(".home.arpa")
}

/// What an upload must find on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precondition {
    /// Replace whatever is there (or create it).
    Overwrite,
    /// Create only: fail with [`ClientError::Changed`] if it exists.
    Create,
    /// Replace only the version with this ETag (as listed, quotes included).
    Replace(String),
}

/// What the server reported after an upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uploaded {
    /// The new ETag, when the server sent one.
    pub etag: Option<String>,
    /// A new file (201), not a replaced one (204).
    pub created: bool,
}

/// What a download should be: taken from the listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub size: u64,
    pub etag: Option<String>,
    /// Seconds since 1970; the downloaded file gets this modification time.
    pub modified: Option<i64>,
}

impl From<&Entry> for Remote {
    fn from(e: &Entry) -> Self {
        Remote { size: e.size, etag: e.etag.clone(), modified: e.modified }
    }
}

/// A Nextcloud account on a server.
#[derive(Debug)]
pub struct Client {
    base: Url,
    root: DavRoot,
    credentials: Credentials,
    options: Options,
}

/// Counts the bytes an upload reads and reports them.
struct Counting<'a, R> {
    inner: R,
    done: u64,
    total: u64,
    report: &'a mut dyn FnMut(u64, u64),
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(out)?;
        self.done = self.done.saturating_add(n as u64);
        (self.report)(self.done, self.total);
        Ok(n)
    }
}

/// `bytes 100-199/200` → (100, Some(200)).
fn parse_content_range(v: &str) -> Option<(u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let start = range.split_once('-')?.0.trim().parse().ok()?;
    Some((start, total.trim().parse().ok()))
}

/// Compare two ETags, ignoring a weak-validator prefix.
fn same_etag(a: &str, b: &str) -> bool {
    a.trim().trim_start_matches("W/") == b.trim().trim_start_matches("W/")
}

/// 64-bit FNV-1a, to put a version's ETag into a file name.
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// The partial file of one version of `dest`: `<dest>.part-<hash of the ETag>`. A partial file
/// may only be continued with the bytes of the *same* version, so the version is part of its
/// name (`If-Range` alone would not help: it compares with the server's current version, not
/// with the one the partial bytes came from). Without an ETag nothing is continued.
fn part_path(dest: &Path, etag: Option<&str>) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    match etag {
        Some(e) => s.push(format!(".part-{:016x}", fnv1a(e.trim().trim_start_matches("W/")))),
        None => s.push(".part-noetag"),
    }
    PathBuf::from(s)
}

/// Remove the partial files of other versions of `dest` (they can never be continued).
fn remove_other_parts(dest: &Path, keep: &Path) {
    let (Some(dir), Some(name)) = (dest.parent(), dest.file_name()) else { return };
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let prefix = format!("{}.part-", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p != keep && e.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl Client {
    /// `server_url` is the address of the Nextcloud, such as `https://cloud.example.org` or
    /// `http://192.168.178.84`; a path is allowed when it is installed in a subfolder.
    pub fn new(server_url: &str, credentials: Credentials, options: Options) -> Result<Client, ClientError> {
        let base = Url::parse(server_url).map_err(|e| ClientError::BadServer(e.to_string()))?;
        if base.path.contains(['?', '#']) {
            return Err(ClientError::BadServer("the address must not have a query".into()));
        }
        if !base.tls && !is_local_host(&base.host) {
            return Err(ClientError::InsecureServer(base.host.clone()));
        }
        let root = DavRoot::files(&base.path, credentials.user()).map_err(|e: PathError| ClientError::BadServer(e.to_string()))?;
        Ok(Client { base, root, credentials, options })
    }

    /// Whether the app password crosses the network unencrypted (`http://` on the local
    /// network). The caller should say so.
    pub fn is_plain_http(&self) -> bool {
        !self.base.tls
    }

    pub fn user(&self) -> &str {
        self.credentials.user()
    }

    fn limits<'a>(&self, cancel: &'a AtomicBool) -> Limits<'a> {
        Limits { connect: self.options.connect_timeout, stall: self.options.stall_timeout, cancel }
    }

    fn send(&self, method: &str, url_path: String, headers: &[(&str, String)], body: Body<'_>, cancel: &AtomicBool) -> Result<Response, ClientError> {
        let url = Url { path: url_path, ..self.base.clone() };
        let mut all: Vec<(&str, String)> = vec![("Authorization", self.credentials.authorization())];
        all.extend(headers.iter().map(|(k, v)| (*k, v.clone())));
        Ok(http::request(Request { method, url: &url, headers: &all, body }, &self.limits(cancel))?)
    }

    /// Turn a status that is not one of `ok` into the error it means.
    fn check(resp: &Response, ok: &[u16]) -> Result<(), ClientError> {
        let status = resp.status;
        if ok.contains(&status) {
            return Ok(());
        }
        Err(match status {
            401 => ClientError::BadCredentials,
            403 => ClientError::Forbidden,
            404 => ClientError::NotFound,
            409 => ClientError::Conflict,
            412 => ClientError::Changed,
            413 => ClientError::TooLarge { size: None, limit: None },
            423 => ClientError::Locked,
            507 => ClientError::NoSpace,
            300..=399 => ClientError::Redirect(resp.header("location").unwrap_or("another address").chars().take(100).collect()),
            other => ClientError::Server(other),
        })
    }

    fn propfind(&self, path: &RemotePath, depth: Depth, collection: bool, cancel: &AtomicBool) -> Result<Listing, ClientError> {
        let headers: Vec<(&str, String)> = dav::propfind_headers(depth).iter().map(|(k, v)| (*k, (*v).to_string())).collect();
        let mut resp = self.send("PROPFIND", self.root.url_path(path, collection), &headers, Body::Bytes(dav::PROPFIND_BODY.as_bytes()), cancel)?;
        Self::check(&resp, &[207])?;
        let xml = resp.read_all(self.options.max_listing_bytes, &self.limits(cancel))?;
        dav::parse_multistatus(&xml, &self.root).map_err(ClientError::Dav)
    }

    /// Whether the server accepts the credentials: asks for the account's top folder.
    pub fn check_login(&self, cancel: &AtomicBool) -> Result<(), ClientError> {
        self.propfind(&RemotePath::root(), Depth::Zero, true, cancel).map(|_| ())
    }

    /// What is directly inside `dir`, without the folder itself.
    pub fn list(&self, dir: &RemotePath, cancel: &AtomicBool) -> Result<Listing, ClientError> {
        let mut listing = self.propfind(dir, Depth::One, true, cancel)?;
        listing.entries.retain(|e| e.path != *dir);
        Ok(listing)
    }

    /// One file or folder.
    pub fn stat(&self, path: &RemotePath, cancel: &AtomicBool) -> Result<Entry, ClientError> {
        let listing = self.propfind(path, Depth::Zero, false, cancel)?;
        listing.entries.into_iter().find(|e| e.path == *path).ok_or(ClientError::NotFound)
    }

    /// Create the folder `path` (its parent must exist). `Ok(false)` if it already exists.
    pub fn mkcol(&self, path: &RemotePath, cancel: &AtomicBool) -> Result<bool, ClientError> {
        let resp = self.send("MKCOL", self.root.url_path(path, true), &[], Body::None, cancel)?;
        match resp.status {
            201 => Ok(true),
            405 => Ok(false),
            _ => Self::check(&resp, &[]).map(|()| false),
        }
    }

    /// Create `path` and every folder above it that is missing.
    pub fn mkcol_all(&self, path: &RemotePath, cancel: &AtomicBool) -> Result<(), ClientError> {
        let mut dir = RemotePath::root();
        for name in path.names() {
            dir = dir.join(name).map_err(|e| ClientError::BadServer(e.to_string()))?;
            self.mkcol(&dir, cancel)?;
        }
        Ok(())
    }

    /// Delete a file or a folder with everything in it.
    pub fn delete(&self, path: &RemotePath, cancel: &AtomicBool) -> Result<(), ClientError> {
        let resp = self.send("DELETE", self.root.url_path(path, false), &[], Body::None, cancel)?;
        Self::check(&resp, &[200, 204])
    }

    /// Send the file `src` to `path`. `src` must not change while it is sent (a sidecar written
    /// by LightCraft is replaced atomically, so it does not): if it did, the call fails with
    /// [`ClientError::SourceChanged`] and the server's copy must not be trusted. `progress`
    /// gets (bytes sent, total).
    pub fn upload(
        &self,
        path: &RemotePath,
        src: &Path,
        precondition: &Precondition,
        modified: Option<i64>,
        progress: &mut dyn FnMut(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<Uploaded, ClientError> {
        let file = File::open(src).map_err(|e| io_err("can't open the file to upload", e))?;
        let before = file.metadata().map_err(|e| io_err("can't read the file to upload", e))?;
        if !before.is_file() {
            return Err(ClientError::Io("the file to upload is not a file".into()));
        }
        let len = before.len();
        if len > self.options.max_upload_bytes {
            return Err(ClientError::TooLarge { size: Some(len), limit: Some(self.options.max_upload_bytes) });
        }
        let mut headers = vec![("Content-Type", "application/octet-stream".to_string())];
        match precondition {
            Precondition::Overwrite => {}
            Precondition::Create => headers.push(("If-None-Match", "*".into())),
            Precondition::Replace(etag) => headers.push(("If-Match", etag.clone())),
        }
        if let Some(t) = modified.filter(|t| *t >= 0) {
            headers.push(("X-OC-Mtime", t.to_string()));
        }
        let reader = Counting { inner: file, done: 0, total: len, report: progress };
        let resp = self.send("PUT", self.root.url_path(path, false), &headers, Body::Reader { len, reader: Box::new(reader) }, cancel)?;
        Self::check(&resp, &[201, 204])?;
        let after = std::fs::metadata(src).map_err(|e| io_err("can't read the uploaded file", e))?;
        if after.len() != len || after.modified().ok() != before.modified().ok() {
            return Err(ClientError::SourceChanged);
        }
        let etag = resp.header("etag").or_else(|| resp.header("oc-etag")).map(str::to_string);
        Ok(Uploaded { etag, created: resp.status == 201 })
    }

    /// Fetch `path` to `dest`, which is replaced only when the whole file has arrived. A
    /// download that was interrupted continues from its `<dest>.part`, as long as the file has
    /// not changed on the server. `progress` gets (bytes so far, total).
    pub fn download(
        &self,
        path: &RemotePath,
        dest: &Path,
        expect: &Remote,
        progress: &mut dyn FnMut(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<(), ClientError> {
        if expect.size > self.options.max_download_bytes {
            return Err(ClientError::TooLarge { size: Some(expect.size), limit: Some(self.options.max_download_bytes) });
        }
        if let Some(dir) = dest.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| io_err("can't create the download folder", e))?;
        }
        let part = part_path(dest, expect.etag.as_deref());
        remove_other_parts(dest, &part);
        // without an ETag there is no telling which version the partial bytes belong to
        let mut have = if expect.etag.is_some() { std::fs::metadata(&part).map_or(0, |m| m.len()) } else { 0 };
        if have > expect.size {
            let _ = std::fs::remove_file(&part);
            have = 0;
        }
        // one retry from scratch when the server cannot continue (416) or ignores the range
        for attempt in 0..2 {
            match self.fetch_into(path, &part, have, expect, progress, cancel)? {
                Fetched::Complete => return finish(&part, dest, expect),
                Fetched::StartOver if attempt == 0 => {
                    let _ = std::fs::remove_file(&part);
                    have = 0;
                }
                Fetched::StartOver => break,
            }
        }
        Err(ClientError::Server(416))
    }

    fn fetch_into(
        &self,
        path: &RemotePath,
        part: &Path,
        have: u64,
        expect: &Remote,
        progress: &mut dyn FnMut(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<Fetched, ClientError> {
        let mut headers = Vec::new();
        if have > 0 {
            headers.push(("Range", format!("bytes={have}-")));
            if let Some(etag) = &expect.etag {
                // only continue the same version: otherwise the server sends the whole file
                headers.push(("If-Range", etag.clone()));
            }
        }
        let mut resp = self.send("GET", self.root.url_path(path, false), &headers, Body::None, cancel)?;
        if resp.status == 416 && have > 0 {
            return Ok(Fetched::StartOver);
        }
        Self::check(&resp, &[200, 206])?;
        if let (Some(want), Some(got)) = (&expect.etag, resp.header("etag"))
            && !same_etag(want, got)
        {
            let _ = std::fs::remove_file(part);
            return Err(ClientError::Changed);
        }
        let mut offset = 0;
        if resp.status == 206 {
            let (start, _) = parse_content_range(resp.header("content-range").unwrap_or_default()).ok_or(ClientError::Server(206))?;
            if start != have {
                return Ok(Fetched::StartOver);
            }
            offset = have;
        }
        let mut out =
            OpenOptions::new().write(true).create(true).truncate(offset == 0).open(part).map_err(|e| io_err("can't write the download", e))?;
        if offset > 0 {
            std::io::Seek::seek(&mut out, std::io::SeekFrom::Start(offset)).map_err(|e| io_err("can't continue the download", e))?;
        }
        let limits = self.limits(cancel);
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = offset;
        loop {
            let n = resp.read(&mut buf, &limits)?;
            if n == 0 {
                break;
            }
            done = done.saturating_add(n as u64);
            if done > expect.size {
                drop(out);
                let _ = std::fs::remove_file(part);
                return Err(ClientError::SizeMismatch { expected: expect.size, got: done });
            }
            out.write_all(buf.get(..n).unwrap_or_default()).map_err(|e| io_err("can't write the download", e))?;
            progress(done, expect.size);
        }
        if done != expect.size {
            // shorter than announced: the part is kept to continue from, unless it is hopeless
            return Err(ClientError::SizeMismatch { expected: expect.size, got: done });
        }
        if let Some(t) = expect.modified.filter(|t| *t >= 0) {
            // best effort: a file system that can't set it only loses the original time
            let _ = out.set_modified(UNIX_EPOCH + Duration::from_secs(t.unsigned_abs()));
        }
        out.sync_all().map_err(|e| io_err("can't save the download", e))?;
        Ok(Fetched::Complete)
    }
}

enum Fetched {
    Complete,
    StartOver,
}

/// Move the finished `part` into place.
fn finish(part: &Path, dest: &Path, expect: &Remote) -> Result<(), ClientError> {
    let size = std::fs::metadata(part).map_err(|e| io_err("can't read the download", e))?.len();
    if size != expect.size {
        let _ = std::fs::remove_file(part);
        return Err(ClientError::SizeMismatch { expected: expect.size, got: size });
    }
    std::fs::rename(part, dest).map_err(|e| io_err("can't move the download into place", e))
}

/// The modification time of `path` as seconds since 1970, for [`Client::upload`].
pub fn file_mtime(path: &Path) -> Option<i64> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    i64::try_from(t.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs()).ok()
}
