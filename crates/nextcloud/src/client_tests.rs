//! The client against a small in-memory WebDAV server on 127.0.0.1 (std `TcpListener`; never the
//! internet): login, listing, uploads with preconditions, downloads that resume, redirects,
//! size limits, and that the app password stays where it belongs.

use std::io::Write;
use std::sync::atomic::AtomicBool;

use crate::auth::Credentials;
use crate::client::*;
use crate::path::RemotePath;
use crate::testserver::*;

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
