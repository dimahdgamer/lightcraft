//! The Nextcloud dialog (File ▸ Nextcloud…) against an in-memory WebDAV server on 127.0.0.1.
//!
//! Scenarios, in the words of someone who edits on a laptop and wants it on the desktop too:
//!
//! * Open the dialog, type the address, the user name and an app password, press Connect: it signs
//!   in, and the password is gone from the dialog.
//! * Add the server's Photos folder and a folder on this computer, press Sync now: the sidecar
//!   goes up, and the dialog says what was sent and received.
//! * When both sides were edited, the dialog lists the file with two buttons; either one ends it.
//! * The password never shows in the dialog's debug text or in what the control channel can read.

use std::time::Duration;

use lightcraft_nextcloud::testserver::{Fake, SECRET, USER};
use serde_json::{Value, json};

use crate::headless::Headless;
use crate::state::Dialog;
use crate::{LightcraftApp, Services};

const T: Duration = Duration::from_secs(20);
const SETTLE: Duration = Duration::from_secs(60);

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("lc-ui-nc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn app() -> Headless {
    let session = lightcraft_engine::Session::new().with_fs();
    let app = LightcraftApp::new(session, Services { png: None, ..Default::default() });
    let mut h = Headless::new(app, [1400.0, 900.0], 1.0);
    h.settle(SETTLE);
    h
}

fn open(h: &mut Headless) {
    let r = h.request("ui.menu.invoke", json!({"command": "app.nextcloud"}), T);
    assert_eq!(r["ok"], true, "{r}");
    h.step();
    assert!(matches!(h.app.ui.dialog, Some(Dialog::Nextcloud { .. })), "the dialog opens: {:?}", h.app.ui.dialog);
}

fn click(h: &mut Headless, id: &str) {
    assert!(h.step_until(T, |h| h.app.widgets.iter().any(|(w, _)| *w == id)), "no widget {id}");
    let r = h.request("ui.clickWidget", json!({"id": id}), T);
    assert_eq!(r["ok"], true, "{r}");
    h.step();
}

/// Type into the dialog's fields (the dialog's state is what the fields edit).
fn fill(h: &mut Headless, f: impl FnOnce(&mut String, &mut String, &mut String, &mut String, &mut String)) {
    if let Some(Dialog::Nextcloud { url, user, password, remote, local, .. }) = &mut h.app.ui.dialog {
        f(url, user, &mut password.0, remote, local);
    } else {
        panic!("the dialog is not open");
    }
    h.step();
}

fn status(h: &mut Headless) -> Value {
    h.app.session.execute("nextcloud.status", &json!({})).unwrap()
}

/// Wait until no Nextcloud job runs any more.
fn idle(h: &mut Headless) {
    assert!(h.step_until(Duration::from_secs(30), |h| !h.app.session.nextcloud.running()), "the job ends");
    // one more frame: the dialog picks up the outcome
    h.step();
    h.step();
}

fn shows(h: &Headless, needle: &str) -> bool {
    h.painted_text().iter().any(|t| t.contains(needle))
}

fn connect(h: &mut Headless, fake: &Fake) {
    open(h);
    fill(h, |url, user, password, _, _| {
        *url = fake.url.clone();
        *user = USER.into();
        *password = SECRET.into();
    });
    click(h, "button:ncConnect");
    idle(h);
}

#[test]
fn connecting_signs_in_and_clears_the_password_from_the_dialog() {
    let fake = Fake::start();
    let mut h = app();
    connect(&mut h, &fake);
    let st = status(&mut h);
    assert_eq!(st["servers"][0]["user"], USER);
    assert_eq!(st["job"]["ok"], true, "{st}");
    assert!(shows(&h, "Signed in as dima"), "{:?}", h.painted_text());
    match &h.app.ui.dialog {
        Some(Dialog::Nextcloud { password, .. }) => assert!(password.0.is_empty(), "the password does not stay in the dialog"),
        other => panic!("{other:?}"),
    }
    // it is in no text anyone could log or read back
    let debug = format!("{:?}", h.app.ui.dialog);
    assert!(!debug.contains(SECRET), "{debug}");
    let json = serde_json::to_string(&h.app.ui.dialog).unwrap();
    assert!(!json.contains(SECRET), "{json}");
    let inspect = h.request("ui.inspect", json!({}), T);
    assert!(!inspect.to_string().contains(SECRET));
    assert!(!h.painted_text().iter().any(|t| t.contains(SECRET)));
}

#[test]
fn the_password_field_shows_dots_not_the_password() {
    let fake = Fake::start();
    let mut h = app();
    open(&mut h);
    fill(&mut h, |url, user, password, _, _| {
        *url = fake.url.clone();
        *user = USER.into();
        *password = SECRET.into();
    });
    h.step();
    h.step();
    let texts = h.painted_text();
    assert!(texts.iter().any(|t| t.contains(&fake.url)), "the address field is drawn: {texts:?}");
    assert!(!texts.iter().any(|t| t.contains(SECRET)), "the password is drawn as dots: {texts:?}");
    // while it is typed, it is in no text anyone could log or read back either
    let debug = format!("{:?}", h.app.ui.dialog);
    assert!(!debug.contains(SECRET), "{debug}");
    assert!(!serde_json::to_string(&h.app.ui.dialog).unwrap().contains(SECRET));
    assert!(!h.request("ui.inspect", json!({}), T).to_string().contains(SECRET));
}

#[test]
fn the_frame_loop_runs_the_automatic_sync_by_itself() {
    let fake = Fake::start();
    let dir = Scratch::new("auto");
    std::fs::write(dir.0.join("a.xmp"), b"sent without a click").unwrap();
    let mut h = app();
    connect(&mut h, &fake);
    let local = dir.0.to_string_lossy().to_string();
    fill(&mut h, |_, _, _, remote, loc| {
        *remote = "/Photos".into();
        *loc = local.clone();
    });
    click(&mut h, "button:ncAdd");
    // nothing is sent until the automatic sync is switched on
    h.step();
    h.step();
    assert!(fake.file("Photos/a.xmp").is_none());
    click(&mut h, "check:ncAuto");
    assert!(h.step_until(Duration::from_secs(30), |_| fake.file("Photos/a.xmp").is_some()), "the frame loop started the sync");
    assert_eq!(fake.file("Photos/a.xmp").unwrap(), b"sent without a click");
}

#[test]
fn a_wrong_password_is_shown_as_an_error_in_the_dialog() {
    let fake = Fake::start();
    let mut h = app();
    open(&mut h);
    fill(&mut h, |url, user, password, _, _| {
        *url = fake.url.clone();
        *user = USER.into();
        *password = "not-the-password".into();
    });
    click(&mut h, "button:ncConnect");
    idle(&mut h);
    assert!(shows(&h, "did not accept"), "{:?}", h.painted_text());
}

#[test]
fn the_connect_button_waits_for_all_three_fields() {
    let mut h = app();
    open(&mut h);
    fill(&mut h, |url, user, _, _, _| {
        *url = "https://cloud.example.org".into();
        *user = "dima".into();
    });
    // no password yet: the button does nothing
    let r = h.request("ui.clickWidget", json!({"id": "button:ncConnect"}), T);
    assert_eq!(r["ok"], true, "{r}");
    h.step();
    assert!(status(&mut h)["servers"].as_array().unwrap().is_empty());
}

#[test]
fn syncing_from_the_dialog_sends_and_receives_and_says_so() {
    let fake = Fake::start();
    let dir = Scratch::new("sync");
    std::fs::write(dir.0.join("a.xmp"), b"local sidecar").unwrap();
    fake.put("Photos/b.xmp", b"server sidecar");
    let mut h = app();
    connect(&mut h, &fake);
    let local = dir.0.to_string_lossy().to_string();
    fill(&mut h, |_, _, _, remote, loc| {
        *remote = "/Photos".into();
        *loc = local.clone();
    });
    click(&mut h, "button:ncAdd");
    assert_eq!(status(&mut h)["pairs"][0]["remote"], "/Photos");
    click(&mut h, "button:ncSyncAll");
    idle(&mut h);
    assert_eq!(fake.file("Photos/a.xmp").unwrap(), b"local sidecar");
    assert_eq!(std::fs::read(dir.0.join("b.xmp")).unwrap(), b"server sidecar");
    assert!(shows(&h, "Sent 1, received 1."), "{:?}", h.painted_text());
    // the dialog's own Sync button for the pair, and turning on the automatic sync
    click(&mut h, "button:ncSync:p1");
    idle(&mut h);
    assert!(shows(&h, "Sent 0, received 0."), "{:?}", h.painted_text());
    click(&mut h, "check:ncAuto");
    assert_eq!(status(&mut h)["autoSync"], true);
    // (switching it on starts a sync at once, and the buttons wait while one runs)
    idle(&mut h);
    // removing the pair leaves the files alone
    click(&mut h, "button:ncRemove:p1");
    assert!(status(&mut h)["pairs"].as_array().unwrap().is_empty());
    assert!(dir.0.join("a.xmp").exists() && fake.file("Photos/a.xmp").is_some());
}

#[test]
fn a_conflict_is_listed_with_two_buttons_and_either_ends_it() {
    let fake = Fake::start();
    let dir = Scratch::new("conflict");
    std::fs::write(dir.0.join("c.xmp"), b"v1").unwrap();
    let mut h = app();
    connect(&mut h, &fake);
    let local = dir.0.to_string_lossy().to_string();
    fill(&mut h, |_, _, _, remote, loc| {
        *remote = "/Photos".into();
        *loc = local.clone();
    });
    click(&mut h, "button:ncAdd");
    click(&mut h, "button:ncSyncAll");
    idle(&mut h);
    // both sides edit
    std::fs::write(dir.0.join("c.xmp"), b"mine, edited here").unwrap();
    fake.put_at("Photos/c.xmp", b"theirs, edited there", 1_800_000_000);
    click(&mut h, "button:ncSyncAll");
    idle(&mut h);
    assert!(shows(&h, "/Photos/c.xmp"), "{:?}", h.painted_text());
    assert_eq!(std::fs::read(dir.0.join("c.xmp")).unwrap(), b"mine, edited here", "nothing moved without a decision");

    click(&mut h, "button:ncKeepServer:0");
    idle(&mut h);
    assert_eq!(std::fs::read(dir.0.join("c.xmp")).unwrap(), b"theirs, edited there");
    click(&mut h, "button:ncSyncAll");
    idle(&mut h);
    assert!(!shows(&h, "/Photos/c.xmp"), "the conflict is gone: {:?}", h.painted_text());

    // and the other way
    std::fs::write(dir.0.join("c.xmp"), b"mine again").unwrap();
    fake.put_at("Photos/c.xmp", b"theirs again", 1_800_000_100);
    click(&mut h, "button:ncSyncAll");
    idle(&mut h);
    click(&mut h, "button:ncKeepLocal:0");
    idle(&mut h);
    assert_eq!(fake.file("Photos/c.xmp").unwrap(), b"mine again");
}

#[test]
fn an_unencrypted_connection_says_so() {
    let fake = Fake::start();
    let mut h = app();
    connect(&mut h, &fake);
    assert!(shows(&h, "not encrypted"), "{:?}", h.painted_text());
}

#[test]
fn disconnecting_returns_to_the_connect_form() {
    let fake = Fake::start();
    let mut h = app();
    connect(&mut h, &fake);
    click(&mut h, "button:ncDisconnect");
    assert!(status(&mut h)["servers"].as_array().unwrap().is_empty());
    h.step();
    assert!(shows(&h, "App password"), "{:?}", h.painted_text());
}

#[test]
fn the_menu_has_the_entry() {
    let h = app();
    let tree = crate::menubar::menu_bar(&h.app);
    let file = &tree.iter().find(|(t, _)| t == "File").expect("File menu").1;
    assert!(format!("{file:?}").contains("app.nextcloud"), "{file:?}");
}
