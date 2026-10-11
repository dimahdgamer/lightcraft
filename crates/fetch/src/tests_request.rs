//! `http::request` against a local recording server (std `TcpListener`; never the internet):
//! methods, headers, bodies held in memory and streamed, empty PUTs, HEAD, chunked 207 answers,
//! a source that ends early, headers that would break the request framing, and cancel.

use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::http::{Body, HttpError, Limits, Request, Url, request};

/// What the server received.
#[derive(Clone, Debug, Default)]
struct Seen {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

struct Server {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("{}{path}", self.base)).unwrap()
    }

    fn only_request(&self) -> Seen {
        let seen = self.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "one request expected");
        seen[0].clone()
    }
}

fn read_request(s: &mut TcpStream) -> Option<Seen> {
    let mut r = BufReader::new(s.try_clone().ok()?);
    let mut seen = Seen::default();
    r.read_line(&mut seen.request_line).ok()?;
    seen.request_line = seen.request_line.trim_end().to_string();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (k, v) = line.split_once(':')?;
        seen.headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    if let Some(n) = seen.header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        seen.body = vec![0; n];
        r.read_exact(&mut seen.body).ok()?;
    }
    Some(seen)
}

/// Serve on 127.0.0.1: `reply` turns each request into the raw bytes of the response.
fn server(reply: impl Fn(&Seen) -> Vec<u8> + Send + Sync + 'static) -> Server {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let reply = Arc::new(reply);
    let record = seen.clone();
    std::thread::spawn(move || {
        for mut s in l.incoming().flatten() {
            let (record, reply) = (record.clone(), reply.clone());
            std::thread::spawn(move || {
                let Some(got) = read_request(&mut s) else { return };
                let out = reply(&got);
                // recorded before the answer goes out, so a test that has the answer sees it
                record.lock().unwrap().push(got);
                let _ = s.write_all(&out);
            });
        }
    });
    Server { base, seen }
}

fn reply(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n").into_bytes();
    out.extend_from_slice(body);
    out
}

fn chunked(status: &str, parts: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").into_bytes();
    for p in parts {
        out.extend_from_slice(format!("{:x}\r\n", p.len()).as_bytes());
        out.extend_from_slice(p);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

fn limits(cancel: &AtomicBool) -> Limits<'_> {
    Limits { connect: Duration::from_secs(5), stall: Duration::from_secs(5), cancel }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect()
}

/// A reader that hands out at most `piece` bytes per call, like a slow disk or a pipe.
struct Trickle<'a> {
    data: &'a [u8],
    piece: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.data.len().min(out.len()).min(self.piece);
        out[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

#[test]
fn propfind_sends_method_headers_and_body_and_reads_a_chunked_207() {
    let xml = b"<?xml version=\"1.0\"?><d:propfind xmlns:d=\"DAV:\"><d:allprop/></d:propfind>";
    let answer = b"<d:multistatus xmlns:d=\"DAV:\"><d:response/></d:multistatus>";
    let s = server(move |_| chunked("207 Multi-Status", &[&answer[..20], &answer[20..]]));
    let cancel = AtomicBool::new(false);
    let url = s.url("/remote.php/dav/files/dima/");
    let headers = [("Depth", "1".to_string()), ("Content-Type", "application/xml".to_string()), ("Authorization", "Basic ZGltYTpwdw==".to_string())];
    let mut resp = request(Request { method: "PROPFIND", url: &url, headers: &headers, body: Body::Bytes(xml) }, &limits(&cancel)).unwrap();
    assert_eq!(resp.status, 207);
    assert_eq!(resp.read_all(1 << 20, &limits(&cancel)).unwrap(), answer);
    let got = s.only_request();
    assert_eq!(got.request_line, "PROPFIND /remote.php/dav/files/dima/ HTTP/1.1");
    assert_eq!(got.header("depth"), Some("1"));
    assert_eq!(got.header("authorization"), Some("Basic ZGltYTpwdw=="));
    assert_eq!(got.header("content-length"), Some(xml.len().to_string().as_str()));
    assert_eq!(got.body, xml);
}

#[test]
fn put_streams_a_reader_with_its_announced_length() {
    let data = pattern(200_000);
    let s = server(|_| reply("201 Created", "Content-Length: 0\r\n", b""));
    let cancel = AtomicBool::new(false);
    let url = s.url("/up/photo.xmp");
    let mut src = Trickle { data: &data, piece: 1000 };
    let r = request(
        Request { method: "PUT", url: &url, headers: &[], body: Body::Reader { len: data.len() as u64, reader: Box::new(&mut src) } },
        &limits(&cancel),
    )
    .unwrap();
    assert_eq!(r.status, 201);
    let got = s.only_request();
    assert_eq!(got.request_line, "PUT /up/photo.xmp HTTP/1.1");
    assert_eq!(got.header("content-length"), Some("200000"));
    assert!(got.body == data, "the body arrived intact");
}

#[test]
fn a_body_can_be_built_before_the_url_it_goes_to() {
    // Compile-time regression: with a `&'a mut dyn Read` the body was invariant in `'a`, so a body
    // declared before its URL (the natural order in a loop over files) did not borrow-check.
    let data = pattern(100);
    let mut src = Cursor::new(data.clone());
    let body = Body::Reader { len: 100, reader: Box::new(&mut src) };
    let s = server(|_| reply("201 Created", "Content-Length: 0\r\n", b""));
    let cancel = AtomicBool::new(false);
    let url = s.url("/f");
    let headers = [("X-Test", "1".to_string())];
    request(Request { method: "PUT", url: &url, headers: &headers, body }, &limits(&cancel)).unwrap();
    assert!(s.only_request().body == data);
}

#[test]
fn a_reader_with_more_data_than_its_length_sends_only_that_length() {
    let data = pattern(20_000);
    let s = server(|_| reply("204 No Content", "", b""));
    let cancel = AtomicBool::new(false);
    let url = s.url("/f");
    let mut src = Cursor::new(data.clone());
    request(Request { method: "PUT", url: &url, headers: &[], body: Body::Reader { len: 5_000, reader: Box::new(&mut src) } }, &limits(&cancel))
        .unwrap();
    assert!(s.only_request().body == data[..5_000]);
}

#[test]
fn a_reader_that_ends_early_fails_the_request() {
    let s = server(|_| reply("201 Created", "Content-Length: 0\r\n", b""));
    let cancel = AtomicBool::new(false);
    let url = s.url("/f");
    let mut src = Cursor::new(pattern(10_000));
    let Err(e) =
        request(Request { method: "PUT", url: &url, headers: &[], body: Body::Reader { len: 50_000, reader: Box::new(&mut src) } }, &limits(&cancel))
    else {
        panic!("a short source must not look like a finished upload");
    };
    assert!(matches!(&e, HttpError::Io(m) if m.contains("ended before")), "{e}");
}

#[test]
fn only_put_and_post_announce_an_empty_body() {
    let s = server(|_| reply("204 No Content", "", b""));
    let cancel = AtomicBool::new(false);
    for method in ["PUT", "POST", "DELETE", "GET", "MKCOL"] {
        let url = s.url("/x");
        request(Request { method, url: &url, headers: &[], body: Body::None }, &limits(&cancel)).unwrap();
    }
    let seen = s.seen.lock().unwrap();
    let length = |method: &str| seen.iter().find(|r| r.request_line.starts_with(method)).and_then(|r| r.header("content-length")).map(str::to_string);
    assert_eq!(length("PUT"), Some("0".into()));
    assert_eq!(length("POST"), Some("0".into()));
    assert_eq!(length("DELETE"), None);
    assert_eq!(length("GET"), None);
    assert_eq!(length("MKCOL"), None);
}

#[test]
fn a_head_response_has_no_body_even_with_a_length() {
    let s = server(|_| reply("200 OK", "Content-Length: 1234\r\nETag: \"abc\"\r\n", b""));
    let cancel = AtomicBool::new(false);
    // a client that waited for the announced body would stall; fail fast instead of after 5 s
    let quick = Limits { connect: Duration::from_secs(5), stall: Duration::from_secs(2), cancel: &cancel };
    let url = s.url("/f");
    let mut r = request(Request { method: "HEAD", url: &url, headers: &[], body: Body::None }, &quick).unwrap();
    assert_eq!((r.status, r.header("etag")), (200, Some("\"abc\"")));
    assert_eq!(r.read(&mut [0u8; 16], &quick).unwrap(), 0);
}

#[test]
fn callers_cannot_set_headers_the_client_manages() {
    let cancel = AtomicBool::new(false);
    let url = Url::parse("http://127.0.0.1:1/").unwrap();
    for name in ["Host", "host", "Content-Length", "CONTENT-LENGTH", "Transfer-Encoding", "Connection"] {
        let headers = [(name, "x".to_string())];
        let r = request(Request { method: "PUT", url: &url, headers: &headers, body: Body::None }, &limits(&cancel));
        assert!(matches!(r, Err(HttpError::BadRequest(_))), "{name}");
    }
}

#[test]
fn bad_methods_and_headers_are_refused_before_connecting() {
    let cancel = AtomicBool::new(false);
    let url = Url::parse("http://127.0.0.1:1/").unwrap();
    for method in ["", "get", "PROP FIND", "GET /x HTTP/1.1\r\nX:", "ABCDEFGHIJKLMNOPQ"] {
        let r = request(Request { method, url: &url, headers: &[], body: Body::None }, &limits(&cancel));
        assert!(matches!(r, Err(HttpError::BadRequest(_))), "method {method:?}");
    }
    for (name, value) in [("X Y", "1"), ("", "1"), ("X-A:", "1"), ("X-A", "1\r\nInjected: 1"), ("X-A", "a\0b"), ("X-A", "a\nb")] {
        let headers = [(name, value.to_string())];
        let r = request(Request { method: "GET", url: &url, headers: &headers, body: Body::None }, &limits(&cancel));
        assert!(matches!(r, Err(HttpError::BadRequest(_))), "header {name:?}: {value:?}");
    }
}

#[test]
fn read_all_stops_at_its_cap() {
    let cancel = AtomicBool::new(false);
    // announced too large: refused before reading
    let s = server(|_| reply("200 OK", "Content-Length: 10000\r\n", &pattern(10_000)));
    let url = s.url("/f");
    let mut r = request(Request { method: "GET", url: &url, headers: &[], body: Body::None }, &limits(&cancel)).unwrap();
    assert!(matches!(r.read_all(5_000, &limits(&cancel)), Err(HttpError::Protocol(m)) if m.contains("larger than 5000")));
    let mut r = request(Request { method: "GET", url: &url, headers: &[], body: Body::None }, &limits(&cancel)).unwrap();
    assert_eq!(r.read_all(10_000, &limits(&cancel)).unwrap().len(), 10_000);
    // no length announced (chunked): refused as soon as it passes the cap
    let big = pattern(4_000);
    let s = server(move |_| chunked("200 OK", &[&big, &big, &big]));
    let url = s.url("/f");
    let mut r = request(Request { method: "GET", url: &url, headers: &[], body: Body::None }, &limits(&cancel)).unwrap();
    assert!(matches!(r.read_all(10_000, &limits(&cancel)), Err(HttpError::Protocol(_))));
}

#[test]
fn a_cancel_raised_while_streaming_stops_the_upload() {
    /// Raises the cancel flag after its first read.
    struct Cancelling<'a> {
        cancel: &'a AtomicBool,
        reads: usize,
    }
    impl Read for Cancelling<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            self.cancel.store(true, Ordering::Relaxed);
            out.fill(7);
            Ok(out.len())
        }
    }
    let s = server(|_| reply("201 Created", "Content-Length: 0\r\n", b""));
    let cancel = AtomicBool::new(false);
    let url = s.url("/big");
    let mut src = Cancelling { cancel: &cancel, reads: 0 };
    let r = request(
        Request { method: "PUT", url: &url, headers: &[], body: Body::Reader { len: 100 << 20, reader: Box::new(&mut src) } },
        &limits(&cancel),
    );
    assert!(matches!(r, Err(HttpError::Cancelled)));
    assert!(src.reads < 10, "stopped after {} reads", src.reads);
}
