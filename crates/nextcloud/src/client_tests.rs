//! The client against a small in-memory WebDAV server on 127.0.0.1 (std `TcpListener`; never the
//! internet): login, listing, uploads with preconditions, downloads that resume, redirects,
//! size limits, and that the app password stays where it belongs.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::auth::Credentials;
use crate::client::*;
use crate::dav::DavRoot;
use crate::path::{RemotePath, decode_segment};

const USER: &str = "dima";
const SECRET: &str = "app-pw-123-secret";
const PREFIX: &str = "/remote.php/dav/files/dima/";
/// The modification time every listed file reports: Sat, 11 Oct 2026 10:00:00 GMT.
const MTIME: i64 = 1_791_712_800;

#[derive(Clone)]
struct FileData {
    data: Vec<u8>,
    etag: String,
}

#[derive(Clone, Debug)]
struct Req {
    method: String,
    headers: Vec<(String, String)>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct State {
    files: BTreeMap<String, FileData>,
    /// Folders by path without leading or trailing `/`; `""` is the account's top folder.
    dirs: BTreeSet<String>,
    counter: u64,
    requests: Vec<Req>,
    redirect: bool,
    ignore_range: bool,
    /// The next GET announces the whole file but sends only this many bytes.
    cut_next_get: Option<usize>,
}

struct Fake {
    url: String,
    state: Arc<Mutex<State>>,
}

impl Fake {
    fn start() -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        state.lock().unwrap().dirs.insert(String::new());
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

    fn put(&self, path: &str, data: &[u8]) {
        let mut s = self.state.lock().unwrap();
        let mut dir = String::new();
        for seg in path.split('/').take(path.split('/').count() - 1) {
            dir = if dir.is_empty() { seg.to_string() } else { format!("{dir}/{seg}") };
            s.dirs.insert(dir.clone());
        }
        s.counter += 1;
        let etag = format!("\"e{}\"", s.counter);
        s.files.insert(path.to_string(), FileData { data: data.to_vec(), etag });
    }

    fn file(&self, path: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().files.get(path).map(|f| f.data.clone())
    }

    fn etag(&self, path: &str) -> String {
        self.state.lock().unwrap().files.get(path).map(|f| f.etag.clone()).unwrap()
    }

    fn requests(&self) -> Vec<Req> {
        self.state.lock().unwrap().requests.clone()
    }

    fn client(&self) -> Client {
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

fn entry_xml(rel: &str, dir: bool, file: Option<&FileData>) -> String {
    let p = RemotePath::parse(rel).unwrap();
    let href = DavRoot::files("", USER).unwrap().url_path(&p, dir);
    let props = match file {
        Some(f) if !dir => format!(
            "<d:resourcetype/><d:getcontentlength>{}</d:getcontentlength><d:getetag>{}</d:getetag><d:getlastmodified>Sat, 11 Oct 2026 10:00:00 GMT</d:getlastmodified>",
            f.data.len(),
            f.etag
        ),
        _ => "<d:resourcetype><d:collection/></d:resourcetype><d:getetag>\"folder\"</d:getetag><oc:size>0</oc:size>".to_string(),
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
    let req = Req { method: method.clone(), headers };
    let mut body = vec![0; req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0)];
    r.read_exact(&mut body)?;

    let mut st = state.lock().unwrap();
    st.requests.push(req.clone());
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
            let is_dir = st.dirs.contains(&rel);
            let is_file = st.files.contains_key(&rel);
            if !is_dir && !is_file {
                respond(&mut s, "404 Not Found", &[], b"");
                return Ok(());
            }
            let mut xml = String::from(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">"#);
            xml.push_str(&entry_xml(&rel, is_dir, st.files.get(&rel)));
            if is_dir && depth_one {
                for d in st.dirs.iter().filter(|d| !d.is_empty() && **d != rel && parent_of(d) == rel) {
                    xml.push_str(&entry_xml(d, true, None));
                }
                for (p, f) in st.files.iter().filter(|(p, _)| parent_of(p) == rel) {
                    xml.push_str(&entry_xml(p, false, Some(f)));
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
            if st.dirs.contains(&rel) {
                respond(&mut s, "405 Method Not Allowed", &[], b"");
            } else if !st.dirs.contains(&parent_of(&rel)) {
                respond(&mut s, "409 Conflict", &[], b"");
            } else {
                let existing = st.files.get(&rel).map(|f| f.etag.clone());
                let if_match_fails = req.header("if-match").is_some_and(|m| existing.as_deref() != Some(m));
                let if_none_fails = req.header("if-none-match") == Some("*") && existing.is_some();
                if if_match_fails || if_none_fails {
                    respond(&mut s, "412 Precondition Failed", &[], b"");
                } else {
                    st.counter += 1;
                    let etag = format!("\"e{}\"", st.counter);
                    st.files.insert(rel, FileData { data: body, etag: etag.clone() });
                    respond(&mut s, if existing.is_some() { "204 No Content" } else { "201 Created" }, &[("ETag", etag)], b"");
                }
            }
        }
        "MKCOL" => {
            if st.dirs.contains(&rel) || st.files.contains_key(&rel) {
                respond(&mut s, "405 Method Not Allowed", &[], b"");
            } else if !st.dirs.contains(&parent_of(&rel)) {
                respond(&mut s, "409 Conflict", &[], b"");
            } else {
                st.dirs.insert(rel);
                respond(&mut s, "201 Created", &[], b"");
            }
        }
        "DELETE" => {
            let below = format!("{rel}/");
            let had = st.files.remove(&rel).is_some() | st.dirs.remove(&rel);
            st.files.retain(|p, _| !p.starts_with(&below));
            st.dirs.retain(|p| !p.starts_with(&below));
            respond(&mut s, if had { "204 No Content" } else { "404 Not Found" }, &[], b"");
        }
        _ => respond(&mut s, "405 Method Not Allowed", &[], b""),
    }
    Ok(())
}

/// A folder for test files, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!("lightcraft-nextcloud-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn write(&self, name: &str, data: &[u8]) -> PathBuf {
        let p = self.path(name);
        std::fs::write(&p, data).unwrap();
        p
    }

    fn names(&self) -> Vec<String> {
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

fn pattern(n: usize) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect()
}

fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

fn rp(s: &str) -> RemotePath {
    RemotePath::parse(s).unwrap()
}

fn silent(_: u64, _: u64) {}

#[test]
fn login_succeeds_and_a_wrong_password_is_refused() {
    let fake = Fake::start();
    fake.client().check_login(&no_cancel()).unwrap();
    let wrong = Client::new(&fake.url, Credentials::new(USER, "not-the-password").unwrap(), Options::default()).unwrap();
    assert_eq!(wrong.check_login(&no_cancel()), Err(ClientError::BadCredentials));
    let other_user = Client::new(&fake.url, Credentials::new("max", SECRET).unwrap(), Options::default()).unwrap();
    assert_eq!(other_user.check_login(&no_cancel()), Err(ClientError::BadCredentials));
}

#[test]
fn lists_a_folder_without_the_folder_itself() {
    let fake = Fake::start();
    fake.put("Photos/a.jpg", &pattern(100));
    fake.put("Photos/Möwe 1 & 2.cr2", &pattern(2000));
    fake.put("Photos/2026/deep.jpg", b"x");
    fake.put("Other/x.jpg", b"y");
    let c = fake.client();
    let l = c.list(&rp("/Photos"), &no_cancel()).unwrap();
    let mut got: Vec<(String, bool, u64)> = l.entries.iter().map(|e| (e.path.to_string(), e.is_dir, e.size)).collect();
    got.sort();
    // sorted by bytes: the capital M comes before the lower-case a
    assert_eq!(got, [("/Photos/2026".to_string(), true, 0), ("/Photos/Möwe 1 & 2.cr2".into(), false, 2000), ("/Photos/a.jpg".into(), false, 100)]);
    assert_eq!(l.entries.iter().find(|e| !e.is_dir).and_then(|e| e.modified), Some(MTIME));
    // the top folder, a file, and what is not there
    assert_eq!(c.list(&RemotePath::root(), &no_cancel()).unwrap().entries.len(), 2);
    assert_eq!(c.stat(&rp("/Photos/a.jpg"), &no_cancel()).unwrap().size, 100);
    assert_eq!(c.stat(&rp("/Photos/missing.jpg"), &no_cancel()), Err(ClientError::NotFound));
    assert_eq!(c.list(&rp("/Nope"), &no_cancel()), Err(ClientError::NotFound));
    assert!(c.stat(&rp("/Photos"), &no_cancel()).unwrap().is_dir);
}

#[test]
fn uploads_create_replace_and_refuse_a_stale_etag() {
    let fake = Fake::start();
    fake.put("Photos/seed.txt", b"seed");
    let dir = TempDir::new();
    let c = fake.client();
    let target = rp("/Photos/a.xmp");
    let src = dir.write("a.xmp", &pattern(70_000));

    let mut seen = Vec::new();
    let first = c.upload(&target, &src, &Precondition::Create, Some(MTIME), &mut |d, t| seen.push((d, t)), &no_cancel()).unwrap();
    assert!(first.created);
    assert_eq!(first.etag.as_deref(), Some(fake.etag("Photos/a.xmp").as_str()));
    assert_eq!(fake.file("Photos/a.xmp").unwrap(), pattern(70_000));
    assert_eq!(seen.last(), Some(&(70_000, 70_000)), "progress reaches the total");
    assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0));
    let put = fake.requests().into_iter().find(|r| r.method == "PUT").unwrap();
    assert_eq!(put.header("x-oc-mtime"), Some("1791712800"));
    assert_eq!(put.header("content-length"), Some("70000"));

    // create-only must not replace
    assert_eq!(c.upload(&target, &src, &Precondition::Create, None, &mut silent, &no_cancel()), Err(ClientError::Changed));
    // replace exactly the version that was seen
    let src2 = dir.write("b.xmp", b"second version");
    let second = c.upload(&target, &src2, &Precondition::Replace(first.etag.clone().unwrap()), None, &mut silent, &no_cancel()).unwrap();
    assert!(!second.created);
    assert_eq!(fake.file("Photos/a.xmp").unwrap(), b"second version");
    // a stale ETag (someone else changed it since) is refused and nothing is overwritten
    let src3 = dir.write("c.xmp", b"third, based on a stale version");
    assert_eq!(c.upload(&target, &src3, &Precondition::Replace(first.etag.unwrap()), None, &mut silent, &no_cancel()), Err(ClientError::Changed));
    assert_eq!(fake.file("Photos/a.xmp").unwrap(), b"second version");
    // overwrite does what it says
    c.upload(&target, &src3, &Precondition::Overwrite, None, &mut silent, &no_cancel()).unwrap();
    assert_eq!(fake.file("Photos/a.xmp").unwrap(), b"third, based on a stale version");
    // a missing parent folder
    assert_eq!(c.upload(&rp("/Nowhere/x.xmp"), &src, &Precondition::Create, None, &mut silent, &no_cancel()), Err(ClientError::Conflict));
}

#[test]
fn an_upload_that_is_too_large_is_refused_before_connecting() {
    let fake = Fake::start();
    let dir = TempDir::new();
    let small = Options { max_upload_bytes: 10, ..Options::default() };
    let c = Client::new(&fake.url, Credentials::new(USER, SECRET).unwrap(), small).unwrap();
    let src = dir.write("big", &[0u8; 11]);
    let r = c.upload(&rp("/big"), &src, &Precondition::Overwrite, None, &mut silent, &no_cancel());
    assert_eq!(r, Err(ClientError::TooLarge { size: Some(11), limit: Some(10) }));
    assert!(fake.requests().is_empty(), "nothing was sent");
    assert!(matches!(c.upload(&rp("/x"), &dir.path("missing"), &Precondition::Overwrite, None, &mut silent, &no_cancel()), Err(ClientError::Io(_))));
}

#[test]
fn an_upload_notices_when_its_source_changes_while_it_is_sent() {
    let fake = Fake::start();
    let dir = TempDir::new();
    let src = dir.write("grows", &pattern(200_000));
    let mut grown = false;
    let r = fake.client().upload(
        &rp("/grows"),
        &src,
        &Precondition::Overwrite,
        None,
        &mut |done, _| {
            if !grown && done > 50_000 {
                grown = true;
                let mut f = std::fs::OpenOptions::new().append(true).open(&src).unwrap();
                f.write_all(b"more").unwrap();
            }
        },
        &no_cancel(),
    );
    assert_eq!(r, Err(ClientError::SourceChanged));
}

#[test]
fn mkcol_all_creates_missing_parents_and_is_repeatable() {
    let fake = Fake::start();
    let c = fake.client();
    assert_eq!(c.mkcol(&rp("/A"), &no_cancel()), Ok(true));
    assert_eq!(c.mkcol(&rp("/A"), &no_cancel()), Ok(false), "an existing folder is not an error");
    assert_eq!(c.mkcol(&rp("/X/Y"), &no_cancel()), Err(ClientError::Conflict));
    c.mkcol_all(&rp("/A/B/C"), &no_cancel()).unwrap();
    c.mkcol_all(&rp("/A/B/C"), &no_cancel()).unwrap();
    assert!(c.stat(&rp("/A/B/C"), &no_cancel()).unwrap().is_dir);
}

#[test]
fn delete_removes_files_and_folders_and_reports_missing() {
    let fake = Fake::start();
    fake.put("Photos/a.jpg", b"a");
    fake.put("Photos/sub/b.jpg", b"b");
    let c = fake.client();
    c.delete(&rp("/Photos/a.jpg"), &no_cancel()).unwrap();
    assert_eq!(c.delete(&rp("/Photos/a.jpg"), &no_cancel()), Err(ClientError::NotFound));
    c.delete(&rp("/Photos/sub"), &no_cancel()).unwrap();
    assert!(fake.file("Photos/sub/b.jpg").is_none());
}

#[test]
fn downloads_a_file_and_gives_it_the_server_time() {
    let fake = Fake::start();
    let data = pattern(300_000);
    fake.put("Photos/big.cr2", &data);
    let dir = TempDir::new();
    let c = fake.client();
    let entry = c.stat(&rp("/Photos/big.cr2"), &no_cancel()).unwrap();
    let dest = dir.path("big.cr2");
    let mut last = (0, 0);
    c.download(&entry.path, &dest, &Remote::from(&entry), &mut |d, t| last = (d, t), &no_cancel()).unwrap();
    assert!(std::fs::read(&dest).unwrap() == data);
    assert_eq!(last, (300_000, 300_000));
    assert_eq!(dir.names(), ["big.cr2"], "no partial file is left behind");
    assert_eq!(file_mtime(&dest), Some(MTIME));
    // into a folder that does not exist yet
    let deep = dir.path("a").join("b").join("big.cr2");
    c.download(&entry.path, &deep, &Remote::from(&entry), &mut silent, &no_cancel()).unwrap();
    assert!(deep.is_file());
}

#[test]
fn an_interrupted_download_continues_from_its_partial_file() {
    let fake = Fake::start();
    let data = pattern(100_000);
    fake.put("Photos/big.cr2", &data);
    fake.state.lock().unwrap().cut_next_get = Some(30_000);
    let dir = TempDir::new();
    let c = fake.client();
    let entry = c.stat(&rp("/Photos/big.cr2"), &no_cancel()).unwrap();
    let dest = dir.path("big.cr2");
    let first = c.download(&entry.path, &dest, &Remote::from(&entry), &mut silent, &no_cancel());
    assert!(matches!(first, Err(ClientError::Network(_))), "{first:?}");
    assert!(!dest.exists(), "an incomplete file never takes the real name");
    let part = dir.names().into_iter().find(|n| n.starts_with("big.cr2.part-")).expect("a partial file is kept");
    assert_eq!(std::fs::metadata(dir.path(&part)).unwrap().len(), 30_000);

    c.download(&entry.path, &dest, &Remote::from(&entry), &mut silent, &no_cancel()).unwrap();
    assert!(std::fs::read(&dest).unwrap() == data);
    assert_eq!(dir.names(), ["big.cr2"]);
    let gets: Vec<Req> = fake.requests().into_iter().filter(|r| r.method == "GET").collect();
    assert_eq!(gets.len(), 2);
    assert_eq!(gets[1].header("range"), Some("bytes=30000-"));
    assert_eq!(gets[1].header("if-range"), Some(entry.etag.as_deref().unwrap()));
}

#[test]
fn bytes_of_an_older_version_are_never_continued_with_a_newer_one() {
    let fake = Fake::start();
    fake.put("Photos/p.cr2", &pattern(50_000));
    let dir = TempDir::new();
    let c = fake.client();
    let old = c.stat(&rp("/Photos/p.cr2"), &no_cancel()).unwrap();
    // a download of the old version was interrupted ...
    fake.state.lock().unwrap().cut_next_get = Some(20_000);
    let dest = dir.path("p.cr2");
    assert!(c.download(&old.path, &dest, &Remote::from(&old), &mut silent, &no_cancel()).is_err());
    // ... then the file changed on the server
    let new_data: Vec<u8> = pattern(50_000).iter().map(|b| b.wrapping_add(1)).collect();
    fake.put("Photos/p.cr2", &new_data);
    let new = c.stat(&old.path, &no_cancel()).unwrap();
    assert_ne!(old.etag, new.etag);
    c.download(&new.path, &dest, &Remote::from(&new), &mut silent, &no_cancel()).unwrap();
    assert!(std::fs::read(&dest).unwrap() == new_data, "no mix of the two versions");
    assert_eq!(dir.names(), ["p.cr2"], "the old version's partial file was removed");
}

#[test]
fn a_server_that_ignores_the_range_restarts_cleanly() {
    let fake = Fake::start();
    let data = pattern(80_000);
    fake.put("f.bin", &data);
    fake.state.lock().unwrap().cut_next_get = Some(10_000);
    let dir = TempDir::new();
    let c = fake.client();
    let entry = c.stat(&rp("/f.bin"), &no_cancel()).unwrap();
    let dest = dir.path("f.bin");
    assert!(c.download(&entry.path, &dest, &Remote::from(&entry), &mut silent, &no_cancel()).is_err());
    fake.state.lock().unwrap().ignore_range = true;
    c.download(&entry.path, &dest, &Remote::from(&entry), &mut silent, &no_cancel()).unwrap();
    assert!(std::fs::read(&dest).unwrap() == data, "the whole file replaced the partial bytes");
}

#[test]
fn a_download_without_an_etag_never_resumes() {
    let fake = Fake::start();
    let data = pattern(40_000);
    fake.put("f.bin", &data);
    fake.state.lock().unwrap().cut_next_get = Some(5_000);
    let dir = TempDir::new();
    let c = fake.client();
    let entry = c.stat(&rp("/f.bin"), &no_cancel()).unwrap();
    let remote = Remote { etag: None, ..Remote::from(&entry) };
    let dest = dir.path("f.bin");
    assert!(c.download(&entry.path, &dest, &remote, &mut silent, &no_cancel()).is_err());
    c.download(&entry.path, &dest, &remote, &mut silent, &no_cancel()).unwrap();
    let gets: Vec<Req> = fake.requests().into_iter().filter(|r| r.method == "GET").collect();
    assert!(gets.iter().all(|g| g.header("range").is_none()), "no way to know which version the partial bytes are");
    assert!(std::fs::read(&dest).unwrap() == data);
}

#[test]
fn a_server_that_sends_more_than_announced_is_cut_off() {
    let fake = Fake::start();
    fake.put("f.bin", &pattern(10_000));
    let dir = TempDir::new();
    let c = fake.client();
    let entry = c.stat(&rp("/f.bin"), &no_cancel()).unwrap();
    let remote = Remote { size: 9_000, ..Remote::from(&entry) };
    let r = c.download(&entry.path, &dir.path("f.bin"), &remote, &mut silent, &no_cancel());
    assert!(matches!(r, Err(ClientError::SizeMismatch { expected: 9_000, .. })), "{r:?}");
    assert!(dir.names().is_empty(), "neither the file nor a partial one is kept");
    let remote = Remote { size: u64::MAX, ..Remote::from(&entry) };
    assert!(matches!(c.download(&entry.path, &dir.path("g.bin"), &remote, &mut silent, &no_cancel()), Err(ClientError::TooLarge { .. })));
}

#[test]
fn a_file_that_changed_since_it_was_listed_is_refused() {
    let fake = Fake::start();
    fake.put("f.bin", &pattern(1000));
    let dir = TempDir::new();
    let c = fake.client();
    let stale = c.stat(&rp("/f.bin"), &no_cancel()).unwrap();
    fake.put("f.bin", &pattern(1000).iter().map(|b| b ^ 0xFF).collect::<Vec<_>>());
    let r = c.download(&stale.path, &dir.path("f.bin"), &Remote::from(&stale), &mut silent, &no_cancel());
    assert_eq!(r, Err(ClientError::Changed));
    assert!(dir.names().is_empty());
    assert_eq!(
        c.download(&rp("/gone"), &dir.path("g"), &Remote { size: 1, etag: None, modified: None }, &mut silent, &no_cancel()),
        Err(ClientError::NotFound)
    );
}

#[test]
fn plain_http_is_allowed_only_on_the_local_network() {
    let new = |url: &str| Client::new(url, Credentials::new(USER, SECRET).unwrap(), Options::default());
    for ok in [
        "http://192.168.178.84",
        "http://10.0.0.2:8080",
        "http://172.16.0.1",
        "http://172.31.255.254",
        "http://169.254.1.1",
        "http://127.0.0.1",
        "http://localhost",
        "http://dima.local",
        "http://nas.lan",
        "http://raspberrypi",
        "http://[::1]:80",
        "http://[fd12:3456::1]",
        "https://cloud.example.org",
        "https://8.8.8.8",
        "http://192.168.1.5/nextcloud/",
    ] {
        assert!(new(ok).is_ok(), "{ok}");
    }
    for bad in [
        "http://cloud.example.org",
        "http://8.8.8.8",
        "http://172.32.0.1",
        "http://192.169.0.1",
        "http://[2001:db8::1]",
        "http://dima.local.example.org",
    ] {
        assert!(matches!(new(bad), Err(ClientError::InsecureServer(_))), "{bad}");
    }
    for bad in ["ftp://x", "cloud.example.org", "https://u:p@cloud.example.org", "https://cloud.example.org/?a=1", ""] {
        assert!(matches!(new(bad), Err(ClientError::BadServer(_))), "{bad}");
    }
    assert!(new("http://192.168.1.5").unwrap().is_plain_http());
    assert!(!new("https://cloud.example.org").unwrap().is_plain_http());
    assert_eq!(new("https://cloud.example.org").unwrap().user(), USER);
}

#[test]
fn a_redirect_is_an_error_and_nothing_is_sent_elsewhere() {
    let fake = Fake::start();
    fake.state.lock().unwrap().redirect = true;
    let r = fake.client().list(&RemotePath::root(), &no_cancel());
    assert_eq!(r, Err(ClientError::Redirect("http://other.example/".into())));
    assert_eq!(fake.requests().len(), 1, "the redirect was not followed");
}

#[test]
fn the_password_stays_out_of_errors_and_debug_output() {
    let fake = Fake::start();
    let c = fake.client();
    assert!(!format!("{c:?}").contains(SECRET));
    let wrong = Client::new(&fake.url, Credentials::new(USER, SECRET).unwrap(), Options::default()).unwrap();
    fake.state.lock().unwrap().redirect = true;
    for e in [
        wrong.check_login(&no_cancel()).unwrap_err(),
        ClientError::BadCredentials,
        ClientError::Redirect("x".into()),
        ClientError::InsecureServer("example.org".into()),
        ClientError::Io("disk".into()),
    ] {
        assert!(!format!("{e} {e:?}").contains(SECRET));
    }
    // the header does reach the server, and only there
    fake.state.lock().unwrap().redirect = false;
    c.check_login(&no_cancel()).unwrap();
    let last = fake.requests().pop().unwrap();
    assert_eq!(last.header("authorization"), Some(Credentials::new(USER, SECRET).unwrap().authorization().as_str()));
}

#[test]
fn a_cancel_stops_a_call_before_it_connects() {
    let fake = Fake::start();
    let cancel = AtomicBool::new(true);
    assert_eq!(fake.client().check_login(&cancel), Err(ClientError::Cancelled));
    let dir = TempDir::new();
    let remote = Remote { size: 1, etag: None, modified: None };
    assert_eq!(fake.client().download(&rp("/x"), &dir.path("x"), &remote, &mut silent, &cancel), Err(ClientError::Cancelled));
    assert!(fake.requests().is_empty());
}
