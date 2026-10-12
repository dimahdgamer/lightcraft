//! The Nextcloud commands against an in-memory WebDAV server on 127.0.0.1 (the test server of
//! `lightcraft-nextcloud`; never the internet): connecting, what is and is not saved, folder pairs,
//! listing, syncing, and that the app password appears nowhere it should not.

use std::path::PathBuf;

use lightcraft_nextcloud::testserver::{Fake, SECRET, USER};
use serde_json::{Value, json};

use crate::{EngineError, Session};

const T0: i64 = 1_700_000_000;

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-nc-engine-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn connect_params(fake: &Fake) -> Value {
    json!({"url": fake.url, "user": USER, "password": SECRET, "name": "Home", "wait": true})
}

fn connected(fake: &Fake) -> Session {
    let mut s = Session::new();
    let r = s.execute("nextcloud.connect", &connect_params(fake)).unwrap();
    assert_eq!(r["user"], USER);
    s
}

fn set_mtime(path: &std::path::Path, secs: i64) {
    let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs.unsigned_abs())).unwrap();
}

fn write(dir: &std::path::Path, name: &str, data: &[u8], mtime: i64) {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, data).unwrap();
    set_mtime(&p, mtime);
}

#[test]
fn connecting_checks_the_login_and_the_password_is_nowhere_else() {
    let fake = Fake::start();
    let mut s = Session::new();
    let mut replies = vec![s.execute("nextcloud.connect", &connect_params(&fake)).unwrap()];
    replies.push(s.execute("nextcloud.status", &json!({})).unwrap());
    replies.push(s.execute("nextcloud.result", &json!({})).unwrap());
    replies.push(s.execute("nextcloud.list", &json!({"server": "Home", "wait": true})).unwrap());
    replies.push(s.execute("query.journal", &json!({})).unwrap_or(Value::Null));
    let st = &replies[1];
    assert_eq!(st["servers"][0]["name"], "Home");
    assert_eq!(
        (st["servers"][0]["user"].as_str(), st["servers"][0]["hasPassword"].as_bool(), st["servers"][0]["plainHttp"].as_bool()),
        (Some(USER), Some(true), Some(true))
    );
    assert_eq!(st["rememberPasswords"], false, "without an installed store the password lives in memory only");
    // no reply, no journal entry and no debug text holds the secret
    for r in &replies {
        assert!(!r.to_string().contains(SECRET), "{r}");
    }
    assert!(!format!("{:?}", s.journal).contains(SECRET));
    assert!(s.journal.iter().all(|(id, _)| !id.starts_with("nextcloud.")), "nextcloud commands are not journaled");
    assert!(s.activity.list().is_empty(), "the activity row is gone when the job is");
}

#[test]
fn a_wrong_password_is_reported_by_the_job_and_a_public_http_address_is_refused_at_once() {
    let fake = Fake::start();
    let mut s = Session::new();
    let wrong = json!({"url": fake.url, "user": USER, "password": "not-it", "name": "Home", "wait": true});
    let e = s.execute("nextcloud.connect", &wrong).unwrap_err();
    assert!(e.to_string().contains("did not accept"), "{e}");
    let r = s.execute("nextcloud.result", &json!({})).unwrap();
    assert_eq!((r["running"].as_bool(), r["ok"].as_bool()), (Some(false), Some(false)));
    assert!(!r.to_string().contains("not-it"));

    let mut s = Session::new();
    for bad in [
        json!({"url": "http://cloud.example.org", "user": "u", "password": "p"}),
        json!({"url": "ftp://x", "user": "u", "password": "p"}),
        json!({"url": "https://x.example", "user": "u:v", "password": "p"}),
        json!({"url": "https://x.example", "user": "u"}),
        json!({"user": "u", "password": "p"}),
        json!({"url": "https://x.example", "password": "p"}),
        json!({"url": "https://x.example", "user": "u", "password": "p", "name": "bad\nname"}),
    ] {
        let e = s.execute("nextcloud.connect", &bad).unwrap_err();
        assert!(matches!(e, EngineError::BadParams { .. }), "{bad}: {e}");
    }
    assert!(s.nextcloud.prefs.servers.is_empty(), "nothing was stored");
    assert!(s.execute("nextcloud.result", &json!({})).unwrap().is_null(), "nothing was sent");
}

#[test]
fn folder_pairs_are_checked_when_they_are_added() {
    let fake = Fake::start();
    let mut s = connected(&fake);
    let dir = temp_dir("pairs");
    let local = dir.to_string_lossy().to_string();
    let add = |s: &mut Session, p: Value| s.execute("nextcloud.pair.add", &p);
    assert!(add(&mut s, json!({"server": "Nobody", "remote": "/Photos", "local": local})).is_err());
    assert!(add(&mut s, json!({"server": "Home", "remote": "/../x", "local": local})).is_err());
    assert!(add(&mut s, json!({"server": "Home", "remote": "/Photos", "local": "relative/folder"})).is_err());
    assert!(add(&mut s, json!({"server": "Home", "remote": "/Photos"})).is_err());
    let ok = add(&mut s, json!({"server": "Home", "remote": "Photos/", "local": local})).unwrap();
    assert_eq!((ok["id"].as_str(), ok["remote"].as_str()), (Some("p1"), Some("/Photos")), "the server path is stored in its normal form");
    assert!(add(&mut s, json!({"server": "Home", "remote": "/Photos", "local": local})).is_err(), "the same pair twice");
    let second = add(&mut s, json!({"server": "Home", "remote": "/Other", "local": local})).unwrap();
    assert_eq!(second["id"], "p2");
    s.execute("nextcloud.pair.remove", &json!({"id": "p1"})).unwrap();
    assert!(s.execute("nextcloud.pair.remove", &json!({"id": "p1"})).is_err());
    let ids: Vec<String> = s.nextcloud.prefs.pairs.iter().map(|p| p.id.clone()).collect();
    assert_eq!(ids, ["p2"]);
    // an id is never used again
    assert_eq!(add(&mut s, json!({"server": "Home", "remote": "/Third", "local": local})).unwrap()["id"], "p3");
}

#[test]
fn listing_gives_folders_first_and_by_name() {
    let fake = Fake::start();
    fake.put("Photos/b.xmp", b"b");
    fake.put("Photos/A.xmp", b"a");
    fake.put("Photos/zeta/x.xmp", b"x");
    fake.put("Photos/alpha/y.xmp", b"y");
    let mut s = connected(&fake);
    let r = s.execute("nextcloud.list", &json!({"server": "Home", "path": "/Photos", "wait": true})).unwrap();
    let names: Vec<(&str, bool)> =
        r["entries"].as_array().unwrap().iter().map(|e| (e["name"].as_str().unwrap(), e["dir"].as_bool().unwrap())).collect();
    assert_eq!(names, [("alpha", true), ("zeta", true), ("A.xmp", false), ("b.xmp", false)]);
    assert_eq!((r["total"].as_u64(), r["truncated"].as_bool()), (Some(4), Some(false)));
    assert!(s.execute("nextcloud.list", &json!({"server": "Home", "path": "/Nope", "wait": true})).is_err());
    assert!(s.execute("nextcloud.list", &json!({"server": "Nobody", "wait": true})).is_err());
    assert!(s.execute("nextcloud.list", &json!({"server": "Home", "path": "/a/../b", "wait": true})).is_err());
}

#[test]
fn syncing_through_the_commands_in_memory_and_with_a_preview() {
    let fake = Fake::start();
    let dir = temp_dir("sync-mem");
    write(&dir, "a.xmp", b"local sidecar", T0 + 10);
    write(&dir, "ignored.cr2", b"not a sidecar", T0 + 11);
    fake.put_at("Photos/b.xmp", b"server sidecar", T0 + 20);
    let mut s = connected(&fake);
    s.execute("nextcloud.pair.add", &json!({"server": "Home", "remote": "/Photos", "local": dir.to_string_lossy()})).unwrap();
    assert!(s.execute("nextcloud.sync", &json!({"pair": "p9", "wait": true})).is_err());

    // a preview plans and changes nothing on either side
    let prev = s.execute("nextcloud.sync", &json!({"preview": true, "wait": true})).unwrap();
    assert_eq!((prev["pairs"][0]["uploads"]["count"].as_u64(), prev["pairs"][0]["downloads"]["count"].as_u64()), (Some(1), Some(1)), "{prev}");
    assert!(fake.file("Photos/a.xmp").is_none() && !dir.join("b.xmp").exists());

    let r = s.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    let p = &r["pairs"][0];
    assert_eq!(
        (p["uploaded"]["paths"][0].as_str(), p["downloaded"]["paths"][0].as_str(), p["clean"].as_bool()),
        (Some("/Photos/a.xmp"), Some("/Photos/b.xmp"), Some(true)),
        "{r}"
    );
    assert_eq!(p["localFolder"].as_str(), Some(dir.to_string_lossy().as_ref()));
    assert_eq!(fake.file("Photos/a.xmp").unwrap(), b"local sidecar");
    assert_eq!(std::fs::read(dir.join("b.xmp")).unwrap(), b"server sidecar");
    assert!(fake.file("Photos/ignored.cr2").is_none());

    // the record is kept (in memory here), so the second run has nothing to do
    let again = s.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!((again["pairs"][0]["uploaded"]["count"].as_u64(), again["pairs"][0]["downloaded"]["count"].as_u64()), (Some(0), Some(0)));
    assert!(s.activity.list().is_empty());
}

#[test]
fn a_library_keeps_servers_pairs_and_records_but_not_the_password() {
    let fake = Fake::start();
    let lib = temp_dir("lib").join("Library");
    let folder = temp_dir("lib-photos");
    write(&folder, "one.xmp", b"1", T0 + 1);
    {
        let mut s = Session::new();
        s.open_library(&lib, false).unwrap();
        s.execute("nextcloud.connect", &connect_params(&fake)).unwrap();
        s.execute("nextcloud.pair.add", &json!({"server": "Home", "remote": "/Photos", "local": folder.to_string_lossy()})).unwrap();
        s.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    }
    let prefs = std::fs::read_to_string(lib.join("prefs.json")).unwrap();
    assert!(prefs.contains("\"Home\"") && prefs.contains("/Photos") && !prefs.contains(SECRET), "{prefs}");
    let record = std::fs::read_to_string(lib.join("Nextcloud").join("p1.json")).unwrap();
    assert!(record.contains("/Photos/one.xmp") && !record.contains(SECRET));
    assert_eq!(fake.file("Photos/one.xmp").unwrap(), b"1");

    // a new session remembers the server and the pair, but has no password
    let mut s = Session::new();
    s.open_library(&lib, false).unwrap();
    let st = s.execute("nextcloud.status", &json!({})).unwrap();
    assert_eq!(st["servers"][0]["name"], "Home");
    assert_eq!(st["servers"][0]["hasPassword"], false);
    assert_eq!(st["pairs"][0]["id"], "p1");
    let e = s.execute("nextcloud.sync", &json!({"wait": true})).unwrap_err();
    assert!(e.to_string().contains("no app password"), "{e}");
    // connecting again (no password given and none stored) asks for one
    let again = json!({"url": fake.url, "user": USER, "name": "Home", "wait": true});
    assert!(s.execute("nextcloud.connect", &again).is_err());
    s.execute("nextcloud.connect", &connect_params(&fake)).unwrap();
    // the stored record is used: nothing to send. The first session uploaded, so the server's
    // folder versions changed after they were noted: one more listing, then the quick check
    let r = s.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!(
        (r["pairs"][0]["uploaded"]["count"].as_u64(), r["pairs"][0]["listed"].as_bool(), r["pairs"][0]["clean"].as_bool()),
        (Some(0), Some(true), Some(true)),
        "{r}"
    );
    let r = s.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!(r["pairs"][0]["listed"].as_bool(), Some(false), "{r}");
}

#[test]
fn disconnecting_forgets_the_server_its_pairs_and_records() {
    let fake = Fake::start();
    let lib = temp_dir("disc").join("Library");
    let folder = temp_dir("disc-photos");
    write(&folder, "one.xmp", b"1", T0 + 1);
    let mut s = Session::new();
    s.open_library(&lib, false).unwrap();
    s.execute("nextcloud.connect", &connect_params(&fake)).unwrap();
    s.execute("nextcloud.pair.add", &json!({"server": "Home", "remote": "/Photos", "local": folder.to_string_lossy()})).unwrap();
    s.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert!(lib.join("Nextcloud").join("p1.json").exists());
    let r = s.execute("nextcloud.disconnect", &json!({"name": "Home"})).unwrap();
    assert_eq!(r["removedPairs"], json!(["p1"]));
    assert!(!lib.join("Nextcloud").join("p1.json").exists());
    let st = s.execute("nextcloud.status", &json!({})).unwrap();
    assert!(st["servers"].as_array().unwrap().is_empty() && st["pairs"].as_array().unwrap().is_empty());
    assert!(folder.join("one.xmp").exists() && fake.file("Photos/one.xmp").is_some(), "no file on either side is touched");
    assert!(s.execute("nextcloud.disconnect", &json!({"name": "Home"})).is_err());
    assert!(s.execute("nextcloud.sync", &json!({"wait": true})).is_err());
}

#[test]
fn a_damaged_prefs_file_cannot_name_files_outside_the_library() {
    let lib = temp_dir("evil").join("Library");
    {
        let mut s = Session::new();
        s.open_library(&lib, false).unwrap();
        s.save_prefs().unwrap();
    }
    let hostile = json!({
        "nextcloud": {
            "servers": [
                {"name": "Home", "url": "https://x.example", "user": "u"},
                {"name": "Home", "url": "https://dup.example", "user": "u"},
                {"name": "bad\nname", "url": "https://x.example", "user": "u"},
                {"name": "NoUser", "url": "https://x.example", "user": ""}
            ],
            "pairs": [
                {"id": "../../evil", "server": "Home", "remote": "/a", "local": "/b"},
                {"id": "a/b", "server": "Home", "remote": "/a", "local": "/b"},
                {"id": "", "server": "Home", "remote": "/a", "local": "/b"},
                {"id": "ok", "server": "Home", "remote": "/a", "local": "/b"},
                {"id": "ok", "server": "Home", "remote": "/c", "local": "/d"},
                {"id": "orphan", "server": "Nobody", "remote": "/a", "local": "/b"}
            ]
        }
    });
    std::fs::write(lib.join("prefs.json"), hostile.to_string()).unwrap();
    let mut s = Session::new();
    s.open_library(&lib, false).unwrap();
    let st = s.execute("nextcloud.status", &json!({})).unwrap();
    let servers: Vec<&str> = st["servers"].as_array().unwrap().iter().map(|v| v["name"].as_str().unwrap()).collect();
    let pairs: Vec<&str> = st["pairs"].as_array().unwrap().iter().map(|v| v["id"].as_str().unwrap()).collect();
    assert_eq!((servers, pairs), (vec!["Home"], vec!["ok"]));
    assert_eq!(s.nextcloud.prefs.servers[0].url, "https://x.example", "the first of two servers with one name wins");
}

#[test]
fn cancel_and_result_are_harmless_when_nothing_runs() {
    let mut s = Session::new();
    assert_eq!(s.execute("nextcloud.cancel", &json!({})).unwrap()["cancelled"], false);
    assert!(s.execute("nextcloud.result", &json!({})).unwrap().is_null());
    let st = s.execute("nextcloud.status", &json!({})).unwrap();
    assert!(st["servers"].as_array().unwrap().is_empty() && st["job"].is_null());
    // the commands are listed, without a menu entry
    let ids: Vec<String> = s.commands().into_iter().filter(|c| c.id.starts_with("nextcloud.")).map(|c| c.id.to_string()).collect();
    assert_eq!(ids.len(), 9, "{ids:?}");
}
