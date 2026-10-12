//! Two machines that edit the same photo (two libraries, two photo folders, one in-memory WebDAV
//! server): what arrives is read into the catalog, nothing is sent back and forth, a conflict
//! waits for a decision, and the automatic sync starts when it should.

use std::path::{Path, PathBuf};

use lightcraft_catalog::PhotoId;
use lightcraft_nextcloud::testserver::Fake;
use serde_json::{Value, json};

use crate::Session;
use crate::tests_nextcloud::{T0, connect_params, set_mtime, temp_dir};

fn write_png(path: &Path) {
    let (w, h) = (40usize, 24usize);
    let data: Vec<[u8; 4]> = (0..w * h).map(|i| [(i % w * 6) as u8, (i / w * 9) as u8, 7, 255]).collect();
    let img = lightcraft_raster::Rgba8 { width: w, height: h, data };
    let bytes = lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(&img), &lightcraft_codecs::EncodeMeta::default()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(-1, |d| d.as_secs() as i64)
}

/// One machine: its own library and photo folder, the same photo, connected to the server.
struct Machine {
    s: Session,
    folder: PathBuf,
    id: PhotoId,
}

fn machine(tag: &str, fake: &Fake) -> Machine {
    let folder = temp_dir(&format!("{tag}-photos"));
    write_png(&folder.join("dune.png"));
    let lib = temp_dir(&format!("{tag}-lib")).join("Library");
    let mut s = Session::new().with_fs();
    s.open_library(&lib, false).unwrap();
    s.execute("library.import", &json!({"paths": [folder.join("dune.png").to_string_lossy()]})).unwrap();
    let id = s.catalog.photos().next().unwrap().id;
    s.execute("library.select", &json!({"ids": [id.0]})).unwrap();
    s.execute("library.xmpPreferences", &json!({"autoWrite": true})).unwrap();
    s.execute("nextcloud.connect", &connect_params(fake)).unwrap();
    s.execute("nextcloud.pair.add", &json!({"server": "Home", "remote": "/Photos", "local": folder.to_string_lossy()})).unwrap();
    Machine { s, folder, id }
}

impl Machine {
    fn rating(&self) -> u8 {
        self.s.catalog.photo(self.id).unwrap().rating
    }

    fn exposure(&self) -> f64 {
        self.s.catalog.photo(self.id).unwrap().develop.light.exposure
    }

    fn rate(&mut self, rating: u8) {
        self.s.execute("photo.rate", &json!({"rating": rating})).unwrap();
    }

    fn sidecar(&self) -> PathBuf {
        self.folder.join("dune.xmp")
    }

    /// Give the sidecar a fixed time, so that a rewrite (which would change it) can be told apart.
    fn stamp_sidecar(&self, secs: i64) {
        set_mtime(&self.sidecar(), secs);
    }

    fn sync(&mut self) -> Value {
        self.s.execute("nextcloud.sync", &json!({"wait": true})).unwrap()["pairs"][0].clone()
    }
}

fn count(v: &Value, key: &str) -> u64 {
    v[key]["count"].as_u64().unwrap()
}

#[test]
fn an_edit_on_one_machine_arrives_on_the_other_and_nothing_bounces_back() {
    let fake = Fake::start();
    let (mut a, mut b) = (machine("a", &fake), machine("b", &fake));

    // A edits; auto-write makes the sidecar; A sends it
    a.rate(5);
    a.s.execute("develop.set", &json!({"control": "light.exposure", "value": 0.8})).unwrap();
    assert!(a.sidecar().is_file(), "auto-write made the sidecar");
    a.stamp_sidecar(T0);
    assert_eq!(count(&a.sync(), "uploaded"), 1);
    assert_eq!((b.rating(), b.exposure()), (0, 0.0));

    // B syncs: the sidecar arrives and is read into B's catalog in the same call
    let r = b.s.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!(count(&r["pairs"][0], "downloaded"), 1, "{r}");
    assert_eq!(r["applied"]["read"], 1, "{r}");
    assert_eq!((b.rating(), b.exposure()), (5, 0.8));
    assert!(b.s.activity.list().is_empty());

    // B has auto-write on too, but reading must not rewrite the sidecar it just received: that
    // would change its time and send it back, and A would then do the same, for ever
    assert_eq!(mtime(&b.sidecar()), T0, "the downloaded sidecar was not rewritten");
    assert_eq!(count(&b.sync(), "uploaded"), 0, "nothing bounces back");
    assert_eq!(count(&a.sync(), "downloaded"), 0, "and A hears nothing of it");

    // B edits; A gets it
    b.rate(2);
    b.stamp_sidecar(T0 + 50);
    assert_eq!(count(&b.sync(), "uploaded"), 1);
    let r = a.s.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!(r["applied"]["read"], 1, "{r}");
    assert_eq!(a.rating(), 2);
    assert_eq!(count(&a.sync(), "uploaded"), 0);
    assert_eq!(count(&b.sync(), "downloaded"), 0);
}

#[test]
fn a_conflict_waits_for_a_decision_and_either_choice_ends_it() {
    let fake = Fake::start();
    let (mut a, mut b) = (machine("ca", &fake), machine("cb", &fake));
    a.rate(1);
    a.stamp_sidecar(T0);
    a.sync();
    b.sync();
    assert_eq!(b.rating(), 1);

    // both edit before either syncs
    a.rate(3);
    a.stamp_sidecar(T0 + 100);
    b.rate(4);
    b.stamp_sidecar(T0 + 200);
    a.sync();
    let r = b.sync();
    assert_eq!(r["conflictCount"], 1, "{r}");
    assert_eq!(r["conflicts"][0]["path"], "/Photos/dune.xmp");
    assert_eq!((b.rating(), count(&r, "uploaded"), count(&r, "downloaded")), (4, 0, 0), "nothing moved without a decision");
    assert_eq!(b.sync()["conflictCount"], 1, "an undecided conflict stays one");

    // keep the server's: B takes A's rating
    let bad = b.s.execute("nextcloud.resolve", &json!({"pair": "p1", "path": "/Photos/dune.xmp", "keep": "both", "wait": true}));
    assert!(bad.is_err(), "only local or server");
    let r = b.s.execute("nextcloud.resolve", &json!({"pair": "p1", "path": "/Photos/dune.xmp", "keep": "server", "wait": true})).unwrap();
    assert_eq!((r["kept"].as_str(), r["applied"]["read"].as_u64()), (Some("server"), Some(1)), "{r}");
    assert_eq!(b.rating(), 3);
    assert_eq!(b.sync()["conflictCount"], 0);
    assert_eq!(count(&b.sync(), "uploaded"), 0);

    // again, and this time keep this machine's
    a.rate(5);
    a.stamp_sidecar(T0 + 300);
    b.rate(2);
    b.stamp_sidecar(T0 + 400);
    a.sync();
    assert_eq!(b.sync()["conflictCount"], 1);
    b.s.execute("nextcloud.resolve", &json!({"pair": "p1", "path": "/Photos/dune.xmp", "keep": "local", "wait": true})).unwrap();
    assert_eq!(b.rating(), 2);
    let r = a.s.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!(r["applied"]["read"], 1, "{r}");
    assert_eq!(a.rating(), 2, "A now has B's choice");
    // a path that is not inside the pair, a pair that does not exist
    assert!(b.s.execute("nextcloud.resolve", &json!({"pair": "p1", "path": "/Elsewhere/x.xmp", "keep": "local", "wait": true})).is_err());
    assert!(b.s.execute("nextcloud.resolve", &json!({"pair": "p9", "path": "/Photos/dune.xmp", "keep": "local", "wait": true})).is_err());
}

#[test]
fn sidecars_of_photos_that_are_not_in_the_library_are_left_for_the_import() {
    let fake = Fake::start();
    let mut a = machine("na", &fake);
    a.rate(4);
    a.stamp_sidecar(T0);
    a.sync();
    // B has the pair but not the photo yet
    let folder = temp_dir("nb-photos");
    let lib = temp_dir("nb-lib").join("Library");
    let mut b = Session::new().with_fs();
    b.open_library(&lib, false).unwrap();
    b.execute("nextcloud.connect", &connect_params(&fake)).unwrap();
    b.execute("nextcloud.pair.add", &json!({"server": "Home", "remote": "/Photos", "local": folder.to_string_lossy()})).unwrap();
    let r = b.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!(r["applied"]["photos"], 0, "{r}");
    assert!(folder.join("dune.xmp").is_file());
    // importing the photo later reads that sidecar, as import always does
    write_png(&folder.join("dune.png"));
    b.execute("library.import", &json!({"paths": [folder.join("dune.png").to_string_lossy()]})).unwrap();
    assert_eq!(b.catalog.photos().next().unwrap().rating, 4);
}

#[test]
fn the_automatic_sync_starts_when_it_is_due_and_not_before() {
    let fake = Fake::start();
    let mut a = machine("ta", &fake);
    let mut b = machine("tb", &fake);
    // off by default
    assert!(!a.s.nextcloud_tick() && !a.s.nextcloud.rt.running());
    assert!(a.s.execute("nextcloud.settings", &json!({"intervalSecs": 5})).is_err(), "too short");
    assert!(a.s.execute("nextcloud.settings", &json!({"intervalSecs": "soon"})).is_err());
    let set = a.s.execute("nextcloud.settings", &json!({"auto": true, "intervalSecs": 600})).unwrap();
    assert_eq!((set["auto"].as_bool(), set["intervalSecs"].as_u64()), (Some(true), Some(600)));
    assert_eq!(a.s.execute("nextcloud.status", &json!({})).unwrap()["autoSync"], true);
    assert_eq!(a.s.execute("nextcloud.settings", &json!({"intervalSecs": 0})).unwrap()["intervalSecs"], 300, "0 is the default");

    // the first tick starts one (nothing was synced yet) and sends the edit
    a.rate(4);
    a.stamp_sidecar(T0);
    assert!(a.s.nextcloud_tick());
    a.s.nextcloud.rt.wait().unwrap();
    assert!(fake.file("Photos/dune.xmp").is_some(), "the edit was sent");
    // right after, nothing is due
    assert!(!a.s.nextcloud_tick());
    // an edit alone does not start one at once: it waits for a quiet moment
    a.rate(5);
    assert!(!a.s.nextcloud_tick() && !a.s.nextcloud.rt.running());
    a.s.nextcloud.rt.dirty_since = Some(web_time::Instant::now() - std::time::Duration::from_secs(crate::nextcloud::EDIT_QUIET_SECS + 1));
    assert!(a.s.nextcloud_tick());
    a.s.nextcloud.rt.wait().unwrap();

    // B hears of it with its own tick, and a later tick reads the downloads in
    b.s.execute("nextcloud.settings", &json!({"auto": true})).unwrap();
    assert!(b.s.nextcloud_tick());
    b.s.nextcloud.rt.wait().unwrap();
    assert!(b.s.nextcloud_tick(), "the finished downloads are applied");
    assert_eq!(b.rating(), 5);
}

#[test]
fn the_automatic_sync_does_nothing_without_pairs() {
    let mut s = Session::new();
    s.execute("nextcloud.settings", &json!({"auto": true})).unwrap();
    assert!(!s.nextcloud_tick(), "no pair, no sync");
    assert!(s.execute("nextcloud.status", &json!({})).unwrap()["job"].is_null());
}
