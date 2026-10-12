//! The sync of a folder pair against the in-memory WebDAV server: a first run in every
//! combination, edits in both directions, conflicts, deletions, a cancel, a change on the server
//! between planning and acting, and that an unchanged server costs one request.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::auth::Credentials;
use crate::client::{Client, ClientError, Options};
use crate::pair::{FolderPair, State, is_sidecar};
use crate::plan::Conflict;
use crate::sync::{Event, Keep, Report, execute, prepare, resolve, sync_pair};
use crate::testserver::*;

const T0: i64 = 1_700_000_000;

struct Setup {
    fake: Fake,
    dir: TempDir,
    pair: FolderPair,
    state: State,
}

fn setup() -> Setup {
    let fake = Fake::start();
    let dir = TempDir::new();
    let pair = FolderPair::new(rp("/Photos"), dir.0.clone());
    Setup { fake, dir, pair, state: State::default() }
}

impl Setup {
    fn run(&mut self) -> Report {
        sync_pair(&self.fake.client(), &self.pair, &mut self.state, is_sidecar, &no_cancel(), &mut |_| {}).unwrap()
    }

    /// A local file with a modification time.
    fn local(&self, name: &str, data: &[u8], mtime: i64) {
        self.dir.write(name, data);
        self.dir.set_mtime(name, mtime);
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.dir.path(name)).unwrap()
    }

    /// PROPFINDs that listed a folder (depth 1), as server paths below the account.
    fn listings(&self) -> Vec<String> {
        self.fake
            .requests()
            .into_iter()
            .filter(|r| r.method == "PROPFIND" && r.header("depth") == Some("1"))
            .map(|r| r.path.trim_start_matches("/remote.php/dav/files/dima/").trim_end_matches('/').to_string())
            .collect()
    }

    fn methods(&self) -> Vec<String> {
        self.fake.requests().into_iter().map(|r| format!("{} {}", r.method, r.header("depth").unwrap_or(""))).collect()
    }
}

fn names(v: &[crate::path::RemotePath]) -> Vec<String> {
    v.iter().map(ToString::to_string).collect()
}

/// A pair whose two sides agree on three files.
fn in_step() -> Setup {
    let mut s = setup();
    for (name, data, t) in [("a.xmp", &b"A"[..], T0 + 10), ("sub/b.xmp", b"BB", T0 + 20), ("c.xmp", b"CCC", T0 + 30)] {
        s.local(name, data, t);
        s.fake.put_at(&format!("Photos/{name}"), data, t);
    }
    let first = s.run();
    assert_eq!(names(&first.adopted), ["/Photos/a.xmp", "/Photos/c.xmp", "/Photos/sub/b.xmp"]);
    assert!(first.is_clean());
    s.fake.clear_requests();
    s
}

#[test]
fn a_first_run_uploads_downloads_and_adopts_and_creates_the_server_folders() {
    let mut s = setup();
    s.local("a.xmp", b"local only", T0 + 100);
    s.local("sub/deep/b.xmp", b"deep local", T0 + 110);
    s.local("same.xmp", b"identical", T0 + 120);
    s.local("x.cr2", b"not a sidecar", T0 + 130);
    s.fake.put_at("Photos/c.xmp", b"server only", T0 + 200);
    s.fake.put_at("Photos/same.xmp", b"identical", T0 + 120);
    s.fake.put_at("Photos/img.jpg", b"not a sidecar", T0 + 210);

    let r = s.run();
    assert_eq!(names(&r.uploaded), ["/Photos/a.xmp", "/Photos/sub/deep/b.xmp"]);
    assert_eq!(names(&r.downloaded), ["/Photos/c.xmp"]);
    assert_eq!(names(&r.adopted), ["/Photos/same.xmp"]);
    assert!(r.is_clean() && r.listed && r.failed.is_empty());

    assert_eq!(s.fake.file("Photos/a.xmp").unwrap(), b"local only");
    assert_eq!(s.fake.mtime("Photos/a.xmp"), Some(T0 + 100), "the file's time travels with it");
    assert_eq!(s.fake.file("Photos/sub/deep/b.xmp").unwrap(), b"deep local");
    assert_eq!(s.read("c.xmp"), b"server only");
    assert_eq!(crate::client::file_mtime(&s.dir.path("c.xmp")), Some(T0 + 200), "and so does the server's");
    assert!(s.fake.file("Photos/x.cr2").is_none() && !s.dir.path("img.jpg").exists(), "only sidecars take part");
    assert_eq!(s.state.files().len(), 4);
}

#[test]
fn a_server_folder_that_does_not_exist_yet_is_created_by_the_first_upload() {
    let mut s = setup();
    s.local("a.xmp", b"A", T0);
    let r = s.run();
    assert_eq!(names(&r.uploaded), ["/Photos/a.xmp"]);
    assert!(dir_names(&s.fake).contains("Photos"));
    assert_eq!(s.fake.file("Photos/a.xmp").unwrap(), b"A");
}

#[test]
fn an_empty_new_local_folder_is_filled_from_the_server() {
    let mut s = setup();
    s.fake.put_at("Photos/2026/Italy/a.xmp", b"A", T0 + 5);
    s.fake.put_at("Photos/b.xmp", b"B", T0 + 6);
    s.pair.local = s.dir.path("laptop").join("Photos");
    let r = s.run();
    assert_eq!(names(&r.downloaded), ["/Photos/2026/Italy/a.xmp", "/Photos/b.xmp"]);
    assert_eq!(std::fs::read(s.pair.local.join("2026").join("Italy").join("a.xmp")).unwrap(), b"A");
}

#[test]
fn an_unchanged_server_costs_one_small_request() {
    let mut s = in_step();
    // the first run sent nothing, so it left the pair complete and the next run can use the quick check
    let r = s.run();
    assert!(!r.listed && r.is_clean());
    assert_eq!(s.methods(), ["PROPFIND 0"], "{:?}", s.methods());
    s.fake.clear_requests();
    s.run();
    assert_eq!(s.methods(), ["PROPFIND 0"]);
}

#[test]
fn a_record_that_is_not_complete_never_gets_the_quick_check() {
    let mut s = in_step();
    // folder versions are there, but the record says the last run left something open
    let folders = s.state.folders().clone();
    assert!(!folders.is_empty());
    s.state.set_tree(folders, false);
    assert!(s.run().listed);
    // and the same after the record went through a file
    let mut s = in_step();
    let folders = s.state.folders().clone();
    s.state.set_tree(folders, false);
    let (loaded, _) = State::from_json(&s.state.to_json()).unwrap();
    s.state = loaded;
    assert!(s.run().listed);
}

#[test]
fn after_an_upload_the_next_run_lists_once_and_then_goes_back_to_the_quick_check() {
    let mut s = in_step();
    s.local("new.xmp", b"N", T0 + 500);
    assert_eq!(names(&s.run().uploaded), ["/Photos/new.xmp"]);
    s.fake.clear_requests();
    let r = s.run();
    assert!(r.listed && r.is_clean() && r.uploaded.is_empty(), "the upload changed the folder versions, so list once more");
    s.fake.clear_requests();
    assert!(!s.run().listed);
}

#[test]
fn edits_flow_in_both_directions() {
    let mut s = in_step();
    // an edit here: uploaded, replacing exactly the version that was seen
    let seen = s.fake.etag("Photos/a.xmp");
    s.local("a.xmp", b"A, edited here", T0 + 1000);
    let r = s.run();
    assert_eq!(names(&r.uploaded), ["/Photos/a.xmp"]);
    assert_eq!(s.fake.file("Photos/a.xmp").unwrap(), b"A, edited here");
    let put = s.fake.requests().into_iter().find(|q| q.method == "PUT").unwrap();
    assert_eq!(put.header("if-match"), Some(seen.as_str()));
    // an edit on the server: downloaded
    s.fake.put_at("Photos/c.xmp", b"C, edited there", T0 + 2000);
    let r = s.run();
    assert_eq!(names(&r.downloaded), ["/Photos/c.xmp"]);
    assert_eq!(s.read("c.xmp"), b"C, edited there");
    // both quiet again
    let r = s.run();
    assert!(r.uploaded.is_empty() && r.downloaded.is_empty() && r.is_clean());
    // a new sidecar for a photo in a new folder, made on the server
    s.fake.put_at("Photos/new/d.xmp", b"D", T0 + 3000);
    assert_eq!(names(&s.run().downloaded), ["/Photos/new/d.xmp"]);
}

#[test]
fn an_edit_on_both_sides_is_a_conflict_and_changes_neither() {
    let mut s = in_step();
    s.local("sub/b.xmp", b"BB, my edit", T0 + 1000);
    s.fake.put_at("Photos/sub/b.xmp", b"BB, their edit", T0 + 1500);
    let r = s.run();
    assert_eq!(r.conflicts, [(rp("/Photos/sub/b.xmp"), Conflict::BothChanged)]);
    assert!(!r.is_clean() && r.uploaded.is_empty() && r.downloaded.is_empty());
    assert_eq!(s.read("sub/b.xmp"), b"BB, my edit");
    assert_eq!(s.fake.file("Photos/sub/b.xmp").unwrap(), b"BB, their edit");
    // it stays a conflict, and is listed again, until someone decides
    s.fake.clear_requests();
    let again = s.run();
    assert!(again.listed);
    assert_eq!(again.conflicts.len(), 1);
    // the other files are not held up by it
    s.local("a.xmp", b"A2", T0 + 2000);
    assert_eq!(names(&s.run().uploaded), ["/Photos/a.xmp"]);
}

#[test]
fn deletions_are_reported_and_nothing_is_deleted() {
    let mut s = in_step();
    s.fake.remove("Photos/c.xmp");
    std::fs::remove_file(s.dir.path("a.xmp")).unwrap();
    let r = s.run();
    assert_eq!(names(&r.remote_deleted), ["/Photos/c.xmp"]);
    assert_eq!(names(&r.local_deleted), ["/Photos/a.xmp"]);
    assert!(s.dir.path("c.xmp").exists(), "the local copy of a file deleted on the server is kept");
    assert!(s.fake.file("Photos/a.xmp").is_some(), "the server's copy of a file deleted here is kept");
    assert!(r.uploaded.is_empty() && r.downloaded.is_empty());
}

#[test]
fn a_change_on_the_server_after_planning_is_refused_for_that_file_only() {
    let mut s = in_step();
    s.local("a.xmp", b"A, mine", T0 + 1000);
    s.local("c.xmp", b"C, mine", T0 + 1000);
    let client = s.fake.client();
    let prepared = prepare(&client, &s.pair, &s.state, is_sidecar, &no_cancel(), &mut |_| {}).unwrap();
    assert_eq!(prepared.plan.uploads().count(), 2);
    // someone else changes one of them after the plan was made
    s.fake.put_at("Photos/a.xmp", b"A, theirs", T0 + 1100);
    let r = execute(&client, &s.pair, &mut s.state, prepared, &no_cancel(), &mut |_| {}).unwrap();
    assert_eq!(names(&r.uploaded), ["/Photos/c.xmp"]);
    assert_eq!(r.failed, [(rp("/Photos/a.xmp"), ClientError::Changed)]);
    assert_eq!(s.fake.file("Photos/a.xmp").unwrap(), b"A, theirs", "their change was not overwritten");
    // the next run sees both sides changed
    assert_eq!(s.run().conflicts, [(rp("/Photos/a.xmp"), Conflict::BothChanged)]);
}

#[test]
fn a_cancel_keeps_what_was_done_and_the_next_run_finishes() {
    let mut s = setup();
    for i in 0..4 {
        s.local(&format!("f{i}.xmp"), format!("data {i}").as_bytes(), T0 + i);
    }
    let cancel = AtomicBool::new(false);
    let mut finished = 0;
    let r = sync_pair(&s.fake.client(), &s.pair, &mut s.state, is_sidecar, &cancel, &mut |e| {
        if let Event::Finished { .. } = e {
            finished += 1;
            if finished == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        }
    });
    assert_eq!(r.unwrap_err(), ClientError::Cancelled);
    assert_eq!(s.state.files().len(), 2, "what was finished is recorded");
    assert!(!s.state.is_complete());
    let r = s.run();
    assert_eq!(r.uploaded.len(), 2, "only the rest is sent: {:?}", names(&r.uploaded));
    assert_eq!(s.state.files().len(), 4);
}

#[test]
fn only_folders_whose_version_changed_are_listed() {
    let mut s = setup();
    for name in ["a.xmp", "x/b.xmp", "y/c.xmp", "z/d.xmp"] {
        s.local(name, name.as_bytes(), T0 + 7);
        s.fake.put_at(&format!("Photos/{name}"), name.as_bytes(), T0 + 7);
    }
    assert_eq!(s.run().adopted.len(), 4);
    assert!(!s.run().listed, "quick check");
    s.fake.clear_requests();
    s.fake.put_at("Photos/y/new.xmp", b"new", T0 + 9);
    let r = s.run();
    assert_eq!(names(&r.downloaded), ["/Photos/y/new.xmp"]);
    let mut listed = s.listings();
    listed.sort();
    assert_eq!(listed, ["Photos", "Photos/y"], "the folders x and z did not change and were not listed");
}

#[test]
fn a_wrong_password_ends_the_run_and_leaves_the_record_alone() {
    let mut s = in_step();
    s.local("a.xmp", b"A, edited", T0 + 1000);
    let wrong = Client::new(&s.fake.url, Credentials::new(USER, "wrong").unwrap(), Options::default()).unwrap();
    let before = s.state.clone();
    let r = sync_pair(&wrong, &s.pair, &mut s.state, is_sidecar, &no_cancel(), &mut |_| {});
    assert_eq!(r.unwrap_err(), ClientError::BadCredentials);
    assert_eq!(s.state, before);
    assert_eq!(s.fake.file("Photos/a.xmp").unwrap(), b"A");
}

#[test]
fn the_record_survives_being_saved_and_loaded_between_runs() {
    let mut s = in_step();
    let (loaded, dropped) = State::from_json(&s.state.to_json()).unwrap();
    assert_eq!(dropped, 0);
    s.state = loaded;
    let r = s.run();
    assert!(!r.listed && r.is_clean(), "the quick check works from a reloaded record");
    s.local("a.xmp", b"A, edited", T0 + 1000);
    assert_eq!(names(&s.run().uploaded), ["/Photos/a.xmp"]);
}

#[test]
fn progress_is_reported() {
    let mut s = setup();
    s.local("a.xmp", &pattern(50_000), T0);
    let mut events = Vec::new();
    sync_pair(&s.fake.client(), &s.pair, &mut s.state, is_sidecar, &no_cancel(), &mut |e| {
        events.push(match e {
            Event::Listing(p) => format!("listing {p}"),
            Event::Planned { uploads, downloads, conflicts } => format!("plan {uploads}/{downloads}/{conflicts}"),
            Event::Transfer { done, total, upload, .. } if done == total => format!("{} {done}", if *upload { "sent" } else { "got" }),
            Event::Transfer { .. } => return,
            Event::Finished { path } => format!("finished {path}"),
        });
    })
    .unwrap();
    assert_eq!(events, ["plan 1/0/0", "sent 50000", "finished /Photos/a.xmp"], "{events:?}");
}

/// Both sides edited `sub/b.xmp`.
fn in_conflict() -> Setup {
    let mut s = in_step();
    s.local("sub/b.xmp", b"BB, my edit", T0 + 1000);
    s.fake.put_at("Photos/sub/b.xmp", b"BB, their edit", T0 + 1500);
    assert_eq!(s.run().conflicts.len(), 1);
    s
}

fn decide(s: &mut Setup, path: &str, keep: Keep) -> Result<(), ClientError> {
    resolve(&s.fake.client(), &s.pair, &mut s.state, &rp(path), keep, &no_cancel(), &mut |_| {})
}

#[test]
fn keeping_the_local_file_replaces_the_servers_and_ends_the_conflict() {
    let mut s = in_conflict();
    decide(&mut s, "/Photos/sub/b.xmp", Keep::Local).unwrap();
    assert_eq!(s.fake.file("Photos/sub/b.xmp").unwrap(), b"BB, my edit");
    assert_eq!(s.fake.mtime("Photos/sub/b.xmp"), Some(T0 + 1000));
    let r = s.run();
    assert!(r.is_clean() && r.uploaded.is_empty() && r.downloaded.is_empty(), "{r:?}");
    assert!(s.run().conflicts.is_empty());
    assert!(!s.run().listed, "and the pair is back to the quick check");
}

#[test]
fn keeping_the_servers_file_replaces_the_local_one_and_ends_the_conflict() {
    let mut s = in_conflict();
    decide(&mut s, "/Photos/sub/b.xmp", Keep::Server).unwrap();
    assert_eq!(s.read("sub/b.xmp"), b"BB, their edit");
    let r = s.run();
    assert!(r.is_clean() && r.uploaded.is_empty() && r.downloaded.is_empty(), "{r:?}");
    assert_eq!(s.fake.file("Photos/sub/b.xmp").unwrap(), b"BB, their edit", "the server's file was not touched");
}

#[test]
fn a_deleted_side_can_be_restored_but_not_kept() {
    // deleted on the server while changed here: keep local restores it, keep server has nothing to keep
    let mut s = in_step();
    s.local("c.xmp", b"C, changed here", T0 + 1000);
    s.fake.remove("Photos/c.xmp");
    assert_eq!(s.run().conflicts.len(), 1);
    assert_eq!(decide(&mut s, "/Photos/c.xmp", Keep::Server), Err(ClientError::NotFound));
    assert_eq!(s.read("c.xmp"), b"C, changed here", "nothing was lost by the refused choice");
    decide(&mut s, "/Photos/c.xmp", Keep::Local).unwrap();
    assert_eq!(s.fake.file("Photos/c.xmp").unwrap(), b"C, changed here");
    // deleted here while changed on the server: keep server restores it, keep local has nothing
    let mut s = in_step();
    std::fs::remove_file(s.dir.path("a.xmp")).unwrap();
    s.fake.put_at("Photos/a.xmp", b"A, changed there", T0 + 2000);
    assert_eq!(s.run().conflicts.len(), 1);
    assert_eq!(decide(&mut s, "/Photos/a.xmp", Keep::Local), Err(ClientError::NotFound));
    decide(&mut s, "/Photos/a.xmp", Keep::Server).unwrap();
    assert_eq!(s.read("a.xmp"), b"A, changed there");
    assert!(s.run().is_clean());
}

#[test]
fn a_path_outside_the_pair_or_a_folder_is_refused() {
    let mut s = in_step();
    assert!(matches!(decide(&mut s, "/Elsewhere/x.xmp", Keep::Local), Err(ClientError::BadServer(_))));
    assert!(matches!(decide(&mut s, "/Photos/sub", Keep::Server), Err(ClientError::Io(_))));
}
