//! Travel packs between two machines: A has the photos and publishes a pack; B has no originals,
//! opens it, edits from the smart previews, and what B edited arrives at A as sidecars.

use std::path::{Path, PathBuf};

use lightcraft_nextcloud::testserver::{Fake, SECRET, USER};
use serde_json::{Value, json};

use crate::Session;
use crate::tests_nextcloud::{T0, connect_params, set_mtime, temp_dir};

fn write_png(path: &Path, seed: u8) {
    let (w, h) = (48usize, 32usize);
    let data: Vec<[u8; 4]> = (0..w * h).map(|i| [(i % w * 5) as u8, (i / w * 7) as u8, seed, 255]).collect();
    let img = lightcraft_raster::Rgba8 { width: w, height: h, data };
    let bytes = lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(&img), &lightcraft_codecs::EncodeMeta::default()).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

struct Pc {
    s: Session,
    folder: PathBuf,
    lib: PathBuf,
}

/// The machine with the photos: `trip/dune.png` and `trip/Italy/sea.png`, in a library on disk.
fn pc(fake: &Fake, tag: &str) -> Pc {
    let folder = temp_dir(&format!("{tag}-photos")).join("trip");
    write_png(&folder.join("dune.png"), 1);
    write_png(&folder.join("Italy").join("sea.png"), 2);
    let lib = temp_dir(&format!("{tag}-lib")).join("Library");
    let mut s = Session::new().with_fs();
    s.open_library(&lib, false).unwrap();
    s.execute("library.import", &json!({"paths": [folder.to_string_lossy()]})).unwrap();
    assert_eq!(s.catalog.len(), 2);
    s.execute("nextcloud.connect", &connect_params(fake)).unwrap();
    Pc { s, folder, lib }
}

/// A machine without the photos: an empty library on disk, connected to the same server.
fn laptop(fake: &Fake, tag: &str) -> Session {
    let lib = temp_dir(&format!("{tag}-lib")).join("Library");
    let mut s = Session::new().with_fs();
    s.open_library(&lib, false).unwrap();
    s.execute("nextcloud.connect", &connect_params(fake)).unwrap();
    s
}

fn photo_by_name(s: &Session, name: &str) -> lightcraft_catalog::PhotoId {
    s.catalog.photos().find(|p| p.file_name == name).unwrap_or_else(|| panic!("no photo {name}")).id
}

fn select(s: &mut Session, id: lightcraft_catalog::PhotoId) {
    s.execute("library.select", &json!({"ids": [id.0]})).unwrap();
}

/// Build the smart previews of every photo and wait for it (without `ids` only the selection gets one).
fn build_smart_previews(s: &mut Session) {
    let ids: Vec<u64> = s.catalog.photos().map(|p| p.id.0).collect();
    let r = s.execute("library.smartPreviews", &json!({"ids": ids})).unwrap();
    assert_eq!(r["built"].as_u64(), Some(ids.len() as u64), "{r}");
}

fn publish(s: &mut Session) -> Value {
    s.execute("nextcloud.pack.publish", &json!({"name": "Trip", "all": true, "wait": true})).unwrap()
}

#[test]
fn a_pack_goes_to_a_laptop_without_originals_and_its_edits_come_back() {
    let fake = Fake::start();
    let mut a = pc(&fake, "pk-a");
    // an edit on the PC that only the pack's sidecar carries (auto-write is off)
    let dune = photo_by_name(&a.s, "dune.png");
    select(&mut a.s, dune);
    a.s.execute("photo.rate", &json!({"rating": 5})).unwrap();
    a.s.execute("develop.set", &json!({"control": "light.exposure", "value": 0.8})).unwrap();
    assert!(!a.folder.join("dune.xmp").exists(), "no sidecar yet");

    // no smart previews yet: they are made first, in the background, and nothing is sent
    let r = a.s.execute("nextcloud.pack.publish", &json!({"name": "Trip", "all": true, "wait": true})).unwrap();
    assert_eq!((r["needsSmartPreviews"].as_u64(), r["building"].as_bool()), (Some(2), Some(true)), "{r}");
    assert!(fake.file("LightCraft/Trip/manifest.json").is_none());
    assert!(a.s.execute("nextcloud.status", &json!({})).unwrap()["pairs"].as_array().unwrap().is_empty(), "no pair before the pack is sent");
    // (a background build, like the app's; wait for it)
    for _ in 0..300 {
        let b = a.s.execute("library.previewProgress", &json!({})).unwrap();
        if b.is_null() || b["running"] == false {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let r = publish(&mut a.s);
    assert_eq!((r["photos"].as_u64(), r["previewsSent"].as_u64(), r["previewsFailed"].as_u64()), (Some(2), Some(2), Some(0)), "{r}");
    assert_eq!(r["sync"]["pairs"][0]["uploaded"]["count"], 2, "both sidecars: {r}");
    assert!(a.folder.join("dune.xmp").is_file(), "publishing wrote the sidecars the pack carries");
    // what the server has
    assert!(fake.file("LightCraft/Trip/manifest.json").is_some());
    assert!(fake.file("LightCraft/Trip/photos/dune.xmp").is_some() && fake.file("LightCraft/Trip/photos/Italy/sea.xmp").is_some());
    let previews = fake.state.lock().unwrap().files.keys().filter(|k| k.starts_with("LightCraft/Trip/previews/") && k.ends_with(".lcsp")).count();
    assert_eq!(previews, 2);
    assert!(!fake.state.lock().unwrap().files.keys().any(|k| k.ends_with(".png")), "no original ever went up");

    // a second publish sends nothing again: not even a request that the server would refuse
    fake.clear_requests();
    let again = publish(&mut a.s);
    assert_eq!(again["previewsSent"], 0, "{again}");
    let preview_puts = fake.requests().iter().filter(|r| r.method == "PUT" && r.path.contains("/previews/")).count();
    assert_eq!(preview_puts, 0, "the previews already there are not sent again");

    // ---- the laptop
    let mut b = laptop(&fake, "pk-b");
    let listed = b.execute("nextcloud.pack.list", &json!({"wait": true})).unwrap();
    assert_eq!(listed["packs"][0]["name"], "Trip", "{listed}");
    let opened = b.execute("nextcloud.pack.open", &json!({"name": "Trip", "wait": true})).unwrap();
    assert_eq!(
        (opened["photos"].as_u64(), opened["previewsFetched"].as_u64(), opened["previewsFailed"].as_u64()),
        (Some(2), Some(2), Some(0)),
        "{opened}"
    );
    assert_eq!((opened["applied"]["pack"]["added"].as_u64(), opened["applied"]["read"].as_u64()), (Some(2), Some(2)), "{opened}");
    assert_eq!(b.catalog.len(), 2);
    let offline = photo_by_name(&b, "dune.png");
    let p = b.catalog.photo(offline).unwrap().clone();
    assert_eq!((p.rating, p.develop.light.exposure), (5, 0.8), "the PC's edit came with the sidecar");
    let path = match &p.source {
        lightcraft_catalog::Source::File { path } => path.clone(),
        other => panic!("{other:?}"),
    };
    assert!(!Path::new(&path).exists(), "there is no original on the laptop: {path}");
    let status = b.execute("photo.smartPreview", &json!({"id": offline.0})).unwrap();
    assert_eq!((status["smartPreview"].as_bool(), status["originalOnline"].as_bool()), (Some(true), Some(false)), "{status}");
    // the photos render from their smart previews
    let built = b.execute("library.buildPreviews", &json!({"wait": true})).unwrap();
    assert_eq!(built["failed"].as_u64().unwrap_or(0), 0, "{built}");

    // ---- an edit on the laptop, without the original, goes back to the PC
    b.execute("library.xmpPreferences", &json!({"autoWrite": true})).unwrap();
    select(&mut b, offline);
    b.execute("photo.rate", &json!({"rating": 2})).unwrap();
    b.execute("develop.set", &json!({"control": "light.exposure", "value": -0.4})).unwrap();
    let sidecar = Path::new(&path).with_extension("xmp");
    assert!(sidecar.is_file(), "the edit was written as a sidecar next to the absent original");
    set_mtime(&sidecar, T0 + 500);
    let sent = b.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!(sent["pairs"][0]["uploaded"]["count"], 1, "{sent}");
    let received = a.s.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!(received["applied"]["read"], 1, "{received}");
    let back = a.s.catalog.photo(dune).unwrap();
    assert_eq!((back.rating, back.develop.light.exposure), (2, -0.4), "the PC has what the laptop edited");
    assert!(a.folder.join("dune.xmp").is_file());
    let _ = &a.lib;
}

#[test]
fn opening_twice_adds_nothing_twice_and_a_pc_opening_its_own_pack_adds_nothing() {
    let fake = Fake::start();
    let mut a = pc(&fake, "pk-twice-a");
    build_smart_previews(&mut a.s);
    publish(&mut a.s);
    let mut b = laptop(&fake, "pk-twice-b");
    let first = b.execute("nextcloud.pack.open", &json!({"name": "Trip", "wait": true})).unwrap();
    assert_eq!(first["applied"]["pack"]["added"], 2);
    let second = b.execute("nextcloud.pack.open", &json!({"name": "Trip", "wait": true})).unwrap();
    assert_eq!((second["applied"]["pack"]["added"].as_u64(), second["applied"]["pack"]["existing"].as_u64()), (Some(0), Some(2)), "{second}");
    assert_eq!(b.catalog.len(), 2);
    assert_eq!(b.nextcloud.prefs.pairs.len(), 1, "one pair for the pack, not one per opening");
    // the PC already has these photos (same content): nothing is added there
    let own = a.s.execute("nextcloud.pack.open", &json!({"name": "Trip", "wait": true})).unwrap();
    assert_eq!((own["applied"]["pack"]["added"].as_u64(), own["applied"]["pack"]["existing"].as_u64()), (Some(0), Some(2)), "{own}");
    assert_eq!(a.s.catalog.len(), 2);
}

#[test]
fn publishing_and_opening_check_their_input() {
    let fake = Fake::start();
    let mut a = pc(&fake, "pk-check-a");
    let name = |n: &str| json!({"name": n, "all": true, "wait": true});
    for bad in ["", "a/b", "..", "x:y", "trailing."] {
        assert!(a.s.execute("nextcloud.pack.publish", &name(bad)).is_err(), "{bad:?}");
        assert!(a.s.execute("nextcloud.pack.open", &json!({"name": bad, "wait": true})).is_err(), "{bad:?}");
    }
    // no photos at all (without `ids` or `all` the selection, or the active photo, is what is published)
    let mut empty = laptop(&fake, "pk-check-empty");
    let e = empty.execute("nextcloud.pack.publish", &json!({"name": "Trip", "wait": true})).unwrap_err();
    assert!(e.to_string().contains("no photos"), "{e}");
    // a root that is not absolute, a folder the photos are not in
    assert!(a.s.execute("nextcloud.pack.publish", &json!({"name": "Trip", "all": true, "root": "relative", "wait": true})).is_err());
    let elsewhere = temp_dir("pk-elsewhere");
    let e =
        a.s.execute("nextcloud.pack.publish", &json!({"name": "Trip", "all": true, "root": elsewhere.to_string_lossy(), "wait": true})).unwrap_err();
    assert!(e.to_string().contains("nothing to publish"), "{e}");
    // a folder that is not a pack, and listing an empty server
    let e = a.s.execute("nextcloud.pack.open", &json!({"name": "Nope", "wait": true})).unwrap_err();
    assert!(e.to_string().contains("not a travel pack"), "{e}");
    let listed = a.s.execute("nextcloud.pack.list", &json!({"wait": true})).unwrap();
    assert!(listed["packs"].as_array().unwrap().is_empty(), "{listed}");
    // a session without a library on disk cannot keep smart previews
    let mut memory = Session::new();
    memory.execute("nextcloud.connect", &connect_params(&fake)).unwrap();
    assert!(memory.execute("nextcloud.pack.publish", &name("Trip")).is_err());
    assert!(memory.execute("nextcloud.pack.open", &json!({"name": "Trip", "wait": true})).is_err());
    let _ = (USER, SECRET);
}

#[test]
fn a_hostile_manifest_cannot_name_files_outside_the_pack() {
    let fake = Fake::start();
    let evil = |path: &str| {
        json!({
            "path": path, "kind": "image", "format": "PNG", "width": 48, "height": 32, "fileSize": 100, "captured": null,
            "meta": {}, "asShotWb": null, "contentHash": "0123456789abcdef0123456789abcdef", "embeddedLens": null, "previewOnly": null
        })
    };
    let manifest = json!({"version": 1, "name": "Evil", "created": "2026-10-12T09:00:00", "photos": [
        evil("../../escape.png"), evil("/absolute.png"), evil("C:/drive.png"), evil("a/../b.png"), evil("fine/ok.png")
    ]});
    fake.put("LightCraft/Evil/manifest.json", manifest.to_string().as_bytes());
    let mut b = laptop(&fake, "pk-evil-b");
    let r = b.execute("nextcloud.pack.open", &json!({"name": "Evil", "wait": true})).unwrap();
    assert_eq!((r["photos"].as_u64(), r["skippedEntries"].as_u64()), (Some(1), Some(4)), "{r}");
    assert_eq!(b.catalog.len(), 1);
    let p = b.catalog.photos().next().unwrap();
    let lightcraft_catalog::Source::File { path } = &p.source else { panic!() };
    assert!(path.replace('\\', "/").ends_with("Travel Packs/Evil/photos/fine/ok.png"), "{path}");
    // a manifest that is not JSON, or from the future
    fake.put("LightCraft/Broken/manifest.json", b"{ not json");
    assert!(b.execute("nextcloud.pack.open", &json!({"name": "Broken", "wait": true})).is_err());
    fake.put("LightCraft/Future/manifest.json", br#"{"version": 99, "name": "F", "created": "", "photos": []}"#);
    let e = b.execute("nextcloud.pack.open", &json!({"name": "Future", "wait": true})).unwrap_err();
    assert!(e.to_string().contains("newer"), "{e}");
}

#[test]
fn photos_a_pack_cannot_carry_are_left_out_and_reported() {
    let fake = Fake::start();
    let mut a = pc(&fake, "pk-skip-a");
    build_smart_previews(&mut a.s);
    // one photo in the trash is not published
    let sea = photo_by_name(&a.s, "sea.png");
    a.s.execute("library.select", &json!({"ids": [sea.0]})).unwrap();
    a.s.execute("photo.delete", &json!({})).unwrap();
    let r = a.s.execute("nextcloud.pack.publish", &json!({"name": "Trip", "all": true, "wait": true}));
    // `all` means the library's photos: the deleted one is not among them
    let r = r.unwrap();
    assert_eq!(r["photos"], 1, "{r}");
    // naming it explicitly reports why it stayed out
    let r = a.s.execute("nextcloud.pack.publish", &json!({"name": "Trip2", "ids": [sea.0], "wait": true}));
    assert!(r.is_err(), "{r:?}");
    assert!(r.unwrap_err().to_string().contains("Recently Deleted"));
}

#[test]
fn opening_a_pack_does_not_send_its_sidecars_back() {
    let fake = Fake::start();
    let mut a = pc(&fake, "pk-bounce-a");
    build_smart_previews(&mut a.s);
    publish(&mut a.s);
    // a laptop that writes sidecars after every change: reading what arrived must not rewrite it
    let mut b = laptop(&fake, "pk-bounce-b");
    b.execute("library.xmpPreferences", &json!({"autoWrite": true})).unwrap();
    let opened = b.execute("nextcloud.pack.open", &json!({"name": "Trip", "wait": true})).unwrap();
    assert_eq!(opened["applied"]["pack"]["added"], 2, "{opened}");
    let later = b.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!(later["pairs"][0]["uploaded"]["count"], 0, "nothing goes back up: {later}");
    // the PC has nothing to take from it either
    let r = a.s.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!((r["pairs"][0]["downloaded"]["count"].as_u64(), r["pairs"][0]["conflictCount"].as_u64()), (Some(0), Some(0)), "{r}");
}

#[test]
fn a_pack_made_from_one_folder_holds_that_folder_only() {
    let fake = Fake::start();
    let mut a = pc(&fake, "pk-root-a");
    build_smart_previews(&mut a.s);
    let italy = a.folder.join("Italy");
    let r = a.s.execute("nextcloud.pack.publish", &json!({"name": "Italy", "all": true, "root": italy.to_string_lossy(), "wait": true})).unwrap();
    assert_eq!((r["photos"].as_u64(), r["skippedCount"].as_u64()), (Some(1), Some(1)), "{r}");
    assert_eq!(r["skipped"][0]["name"], "dune.png");
    let manifest: Value = serde_json::from_slice(&fake.file("LightCraft/Italy/manifest.json").unwrap()).unwrap();
    assert_eq!(manifest["photos"][0]["path"], "sea.png", "paths are below the pack's root");
    assert!(fake.file("LightCraft/Italy/photos/sea.xmp").is_some() && fake.file("LightCraft/Italy/photos/dune.xmp").is_none());
    // the default root is the deepest folder the photos share
    let all = a.s.execute("nextcloud.pack.publish", &json!({"name": "Whole", "all": true, "wait": true})).unwrap();
    assert_eq!(all["root"].as_str().map(|r| r.replace('\\', "/").ends_with("/trip")), Some(true), "{all}");
    let m2: Value = serde_json::from_slice(&fake.file("LightCraft/Whole/manifest.json").unwrap()).unwrap();
    let mut paths: Vec<&str> = m2["photos"].as_array().unwrap().iter().filter_map(|p| p["path"].as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["Italy/sea.png", "dune.png"]);
}

#[test]
fn a_pack_without_sidecars_does_not_make_the_laptop_write_default_ones() {
    let fake = Fake::start();
    let mut a = pc(&fake, "pk-nosc-a");
    build_smart_previews(&mut a.s);
    publish(&mut a.s);
    // the sidecars are gone from the server (a pack made by hand, or cleaned up)
    fake.remove("LightCraft/Trip/photos/dune.xmp");
    fake.remove("LightCraft/Trip/photos/Italy/sea.xmp");
    let mut b = laptop(&fake, "pk-nosc-b");
    b.execute("library.xmpPreferences", &json!({"autoWrite": true})).unwrap();
    let opened = b.execute("nextcloud.pack.open", &json!({"name": "Trip", "wait": true})).unwrap();
    assert_eq!(opened["applied"]["pack"]["added"], 2, "{opened}");
    // reading nothing, writing nothing: a default sidecar sent to the PC would overwrite its edits
    let root = b.nextcloud.prefs.pairs[0].local.clone();
    let found: Vec<String> = walk(Path::new(&root));
    assert!(found.iter().all(|f| !f.ends_with(".xmp")), "{found:?}");
    let later = b.execute("nextcloud.sync", &json!({"wait": true})).unwrap();
    assert_eq!(later["pairs"][0]["uploaded"]["count"], 0, "{later}");
}

/// Every file below `dir`, as text.
fn walk(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p.to_string_lossy().to_string());
            }
        }
    }
    out
}
