//! A small in-memory WebDAV server on 127.0.0.1 for the tests of the client and the sync (std
//! `TcpListener`; never the internet). It speaks enough of Nextcloud's WebDAV: Basic login,
//! `PROPFIND` (depth 0 and 1) with ETags, modification times and folder ETags that change when
//! anything below changes, `GET` with `Range`/`If-Range`, `PUT` with `If-Match`/`If-None-Match`
//! and `X-OC-Mtime`, `MKCOL` and `DELETE`, plus switches to misbehave (redirect, ignore ranges,
//! cut a download short, drop every connection, lose the answer to an upload).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::auth::Credentials;
use crate::client::{Client, Options};
use crate::dav::DavRoot;
use crate::path::{RemotePath, decode_segment};

pub const USER: &str = "dima";
pub const SECRET: &str = "app-pw-123-secret";
const PREFIX: &str = "/remote.php/dav/files/dima/";
/// The modification time files get unless another is given: Sun, 11 Oct 2026 10:00:00 GMT.
pub const MTIME: i64 = 1_791_712_800;

#[derive(Clone)]
pub struct FileData {
    pub data: Vec<u8>,
    pub etag: String,
    pub mtime: i64,
}

#[derive(Clone, Debug)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
pub struct State {
    pub files: BTreeMap<String, FileData>,
    /// Folders by path without leading or trailing `/` (`""` is the top folder) and their ETags,
    /// which change whenever anything below them changes.
    pub dirs: BTreeMap<String, String>,
    pub counter: u64,
    pub requests: Vec<Req>,
    pub redirect: bool,
    pub ignore_range: bool,
    /// The next GET announces the whole file but sends only this many bytes.
    pub cut_next_get: Option<usize>,
    /// Every request is read and then the connection is closed without an answer: the Wi-Fi is gone.
    pub offline: bool,
    /// The next PUT is stored, but the connection is closed without telling the client.
    pub lose_next_put_answer: bool,
}

impl State {
    fn next_etag(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("\"{prefix}{}\"", self.counter)
    }

    /// A change at `path`: give every folder above it a new ETag.
    fn touch_above(&mut self, path: &str) {
        let mut dir = parent_of(path);
        loop {
            let etag = self.next_etag("d");
            self.dirs.insert(dir.clone(), etag);
            if dir.is_empty() {
                break;
            }
            dir = parent_of(&dir);
        }
    }

    fn add_dir(&mut self, path: &str) {
        let etag = self.next_etag("d");
        self.dirs.insert(path.to_string(), etag);
    }
}

pub struct Fake {
    pub url: String,
    pub state: Arc<Mutex<State>>,
}

impl Fake {
    pub fn start() -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        state.lock().unwrap().add_dir("");
        let st = state.clone();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let st = st.clone();
                std::thread::spawn(move || {
                    let _ = serve(s, &st);
                });
            }
        });
        Fake { url, state }
    }

    /// Put a file there as another client would have (creating the folders above it).
    pub fn put(&self, path: &str, data: &[u8]) {
        self.put_at(path, data, MTIME);
    }

    pub fn put_at(&self, path: &str, data: &[u8], mtime: i64) {
        let mut s = self.state.lock().unwrap();
        let mut dir = String::new();
        for seg in path.split('/').take(path.split('/').count() - 1) {
            dir = if dir.is_empty() { seg.to_string() } else { format!("{dir}/{seg}") };
            if !s.dirs.contains_key(&dir) {
                s.add_dir(&dir);
            }
        }
        let etag = s.next_etag("e");
        s.files.insert(path.to_string(), FileData { data: data.to_vec(), etag, mtime });
        s.touch_above(path);
    }

    pub fn remove(&self, path: &str) {
        let mut s = self.state.lock().unwrap();
        s.files.remove(path);
        s.touch_above(path);
    }

    pub fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().files.get(path).map(|f| f.data.clone())
    }

    pub fn mtime(&self, path: &str) -> Option<i64> {
        self.state.lock().unwrap().files.get(path).map(|f| f.mtime)
    }

    pub fn etag(&self, path: &str) -> String {
        self.state.lock().unwrap().files.get(path).map(|f| f.etag.clone()).unwrap()
    }

    pub fn requests(&self) -> Vec<Req> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn clear_requests(&self) {
        self.state.lock().unwrap().requests.clear();
    }

    pub fn client(&self) -> Client {
        Client::new(&self.url, Credentials::new(USER, SECRET).unwrap(), Options::default()).unwrap()
    }
}

fn respond(s: &mut TcpStream, status: &str, headers: &[(&str, String)], body: &[u8]) {
    let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(b"\r\n");
    let _ = s.write_all(body);
}

/// The path below the account's top folder, or `None` for any other address.
fn relative(target: &str) -> Option<String> {
    let rest = target.strip_prefix(PREFIX).or_else(|| (target == PREFIX.trim_end_matches('/')).then_some(""))?;
    let names: Vec<String> = rest.split('/').filter(|s| !s.is_empty()).map(|s| decode_segment(s).unwrap_or_default()).collect();
    Some(names.join("/"))
}

fn parent_of(path: &str) -> String {
    path.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default()
}

/// Days since 1970-01-01 to (year, month, day); Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `Sun, 11 Oct 2026 10:00:00 GMT`.
pub fn http_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let weekday = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][usize::try_from((days + 4).rem_euclid(7)).unwrap()];
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][usize::try_from(m - 1).unwrap()];
    format!("{weekday}, {d:02} {month} {y} {:02}:{:02}:{:02} GMT", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn entry_xml(rel: &str, dir_etag: Option<&str>, file: Option<&FileData>) -> String {
    let p = RemotePath::parse(rel).unwrap();
    let is_dir = dir_etag.is_some();
    let href = DavRoot::files("", USER).unwrap().url_path(&p, is_dir);
    let props = match (dir_etag, file) {
        (Some(etag), _) => format!("<d:resourcetype><d:collection/></d:resourcetype><d:getetag>{etag}</d:getetag><oc:size>0</oc:size>"),
        (None, Some(f)) => format!(
            "<d:resourcetype/><d:getcontentlength>{}</d:getcontentlength><d:getetag>{}</d:getetag><d:getlastmodified>{}</d:getlastmodified>",
            f.data.len(),
            f.etag,
            http_date(f.mtime)
        ),
        (None, None) => String::new(),
    };
    format!("<d:response><d:href>{href}</d:href><d:propstat><d:prop>{props}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>")
}

fn serve(mut s: TcpStream, state: &Mutex<State>) -> std::io::Result<()> {
    let mut r = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let req = Req { method: method.clone(), path: target.clone(), headers };
    let mut body = vec![0; req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0)];
    r.read_exact(&mut body)?;

    let mut st = state.lock().unwrap();
    st.requests.push(req.clone());
    if st.offline {
        return Ok(());
    }
    if st.redirect {
        respond(&mut s, "302 Found", &[("Location", "http://other.example/".into())], b"");
        return Ok(());
    }
    if req.header("authorization") != Some(Credentials::new(USER, SECRET).unwrap().authorization().as_str()) {
        respond(&mut s, "401 Unauthorized", &[("WWW-Authenticate", "Basic realm=\"Nextcloud\"".into())], b"");
        return Ok(());
    }
    let Some(rel) = relative(&target) else {
        respond(&mut s, "404 Not Found", &[], b"");
        return Ok(());
    };
    match method.as_str() {
        "PROPFIND" => {
            let depth_one = req.header("depth") == Some("1");
            let dir_etag = st.dirs.get(&rel).cloned();
            let is_file = st.files.contains_key(&rel);
            if dir_etag.is_none() && !is_file {
                respond(&mut s, "404 Not Found", &[], b"");
                return Ok(());
            }
            let mut xml = String::from(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">"#);
            xml.push_str(&entry_xml(&rel, dir_etag.as_deref(), st.files.get(&rel)));
            if dir_etag.is_some() && depth_one {
                for (d, etag) in st.dirs.iter().filter(|(d, _)| !d.is_empty() && **d != rel && parent_of(d) == rel) {
                    xml.push_str(&entry_xml(d, Some(etag), None));
                }
                for (p, f) in st.files.iter().filter(|(p, _)| parent_of(p) == rel) {
                    xml.push_str(&entry_xml(p, None, Some(f)));
                }
            }
            xml.push_str("</d:multistatus>");
            respond(&mut s, "207 Multi-Status", &[("Content-Type", "application/xml; charset=utf-8".into())], xml.as_bytes());
        }
        "GET" => {
            let Some(f) = st.files.get(&rel).cloned() else {
                respond(&mut s, "404 Not Found", &[], b"");
                return Ok(());
            };
            let etag = ("ETag", f.etag.clone());
            let range = req.header("range").and_then(|v| v.strip_prefix("bytes=")).and_then(|v| v.trim_end_matches('-').parse::<usize>().ok());
            let range = range.filter(|_| !st.ignore_range && req.header("if-range").is_none_or(|e| e == f.etag));
            if let Some(from) = range {
                if from >= f.data.len() {
                    respond(&mut s, "416 Range Not Satisfiable", &[], b"");
                } else {
                    let tail = &f.data[from..];
                    respond(
                        &mut s,
                        "206 Partial Content",
                        &[etag, ("Content-Range", format!("bytes {from}-{}/{}", f.data.len() - 1, f.data.len()))],
                        tail,
                    );
                }
            } else if let Some(cut) = st.cut_next_get.take() {
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {}\r\nConnection: close\r\n\r\n", f.data.len(), f.etag);
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&f.data[..cut.min(f.data.len())]);
            } else {
                respond(&mut s, "200 OK", &[etag], &f.data);
            }
        }
        "PUT" => {
            if st.dirs.contains_key(&rel) {
                respond(&mut s, "405 Method Not Allowed", &[], b"");
            } else if !st.dirs.contains_key(&parent_of(&rel)) {
                respond(&mut s, "409 Conflict", &[], b"");
            } else {
                let existing = st.files.get(&rel).map(|f| f.etag.clone());
                let if_match_fails = req.header("if-match").is_some_and(|m| existing.as_deref() != Some(m));
                let if_none_fails = req.header("if-none-match") == Some("*") && existing.is_some();
                if if_match_fails || if_none_fails {
                    respond(&mut s, "412 Precondition Failed", &[], b"");
                } else {
                    let etag = st.next_etag("e");
                    let mtime = req.header("x-oc-mtime").and_then(|v| v.parse().ok()).unwrap_or(MTIME);
                    st.files.insert(rel.clone(), FileData { data: body, etag: etag.clone(), mtime });
                    st.touch_above(&rel);
                    if std::mem::take(&mut st.lose_next_put_answer) {
                        return Ok(());
                    }
                    respond(&mut s, if existing.is_some() { "204 No Content" } else { "201 Created" }, &[("ETag", etag)], b"");
                }
            }
        }
        "MKCOL" => {
            if st.dirs.contains_key(&rel) || st.files.contains_key(&rel) {
                respond(&mut s, "405 Method Not Allowed", &[], b"");
            } else if !st.dirs.contains_key(&parent_of(&rel)) {
                respond(&mut s, "409 Conflict", &[], b"");
            } else {
                st.add_dir(&rel);
                st.touch_above(&rel);
                respond(&mut s, "201 Created", &[], b"");
            }
        }
        "DELETE" => {
            let below = format!("{rel}/");
            let had = st.files.remove(&rel).is_some() | st.dirs.remove(&rel).is_some();
            st.files.retain(|p, _| !p.starts_with(&below));
            st.dirs.retain(|p, _| !p.starts_with(&below));
            if had {
                st.touch_above(&rel);
            }
            respond(&mut s, if had { "204 No Content" } else { "404 Not Found" }, &[], b"");
        }
        _ => respond(&mut s, "405 Method Not Allowed", &[], b""),
    }
    Ok(())
}

/// A folder for test files, removed when dropped.
pub struct TempDir(pub PathBuf);

impl Default for TempDir {
    fn default() -> Self {
        TempDir::new()
    }
}

impl TempDir {
    pub fn new() -> TempDir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!("lightcraft-nextcloud-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    pub fn write(&self, name: &str, data: &[u8]) -> PathBuf {
        let p = self.path(name);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(&p, data).unwrap();
        p
    }

    /// Set a file's modification time (seconds since 1970).
    pub fn set_mtime(&self, name: &str, secs: i64) {
        let f = std::fs::OpenOptions::new().write(true).open(self.path(name)).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs.unsigned_abs())).unwrap();
    }

    /// The names directly inside, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.0).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn pattern(n: usize) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect()
}

pub fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

pub fn rp(s: &str) -> RemotePath {
    RemotePath::parse(s).unwrap()
}

pub fn silent(_: u64, _: u64) {}

/// All the folder paths the server has, for assertions.
pub fn dir_names(fake: &Fake) -> BTreeSet<String> {
    fake.state.lock().unwrap().dirs.keys().cloned().collect()
}

#[test]
fn http_dates_match_the_parser() {
    for t in [0, 1, MTIME, 1_835_481_599, 253_402_300_799, 951_782_400] {
        assert_eq!(crate::dav::parse_http_date(&http_date(t)), Some(t), "{t}");
    }
    assert_eq!(http_date(MTIME), "Sun, 11 Oct 2026 10:00:00 GMT");
}
