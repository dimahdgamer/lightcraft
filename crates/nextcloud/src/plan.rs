//! Deciding what to transfer, as pure functions.
//!
//! A folder on this machine and a folder on the server are kept in step file by file. For each
//! file three facts are compared: what is here now, what the server has now, and what both were
//! right after the last successful transfer (the *base*). A side that differs from the base has
//! changed; the other did not:
//!
//! | here | server | action |
//! |---|---|---|
//! | changed | unchanged | upload, only if the server still has the base version (`If-Match`) |
//! | unchanged | changed | download |
//! | changed | changed | **conflict**: nothing is overwritten, the caller decides |
//! | deleted | unchanged | reported, never deleted for you |
//! | unchanged | deleted | reported, never deleted for you |
//!
//! Nothing here reads a disk or a network, so every case is a plain test. The plan never loses a
//! change: whenever both sides moved, or one side moved while the other was deleted, it says
//! [`Action::Conflict`] instead of picking a winner.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::dav::Entry;
use crate::path::RemotePath;

/// Compare two ETags, ignoring a weak-validator prefix (`W/`).
pub fn same_etag(a: &str, b: &str) -> bool {
    a.trim().trim_start_matches("W/") == b.trim().trim_start_matches("W/")
}

/// A file on this machine. The time is whole seconds, because that is what the server keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalFile {
    pub size: u64,
    pub modified: i64,
}

/// A file on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFile {
    pub etag: Option<String>,
    pub size: u64,
    pub modified: Option<i64>,
}

impl From<&Entry> for RemoteFile {
    fn from(e: &Entry) -> Self {
        RemoteFile { etag: e.etag.clone(), size: e.size, modified: e.modified }
    }
}

/// What was true of a file right after it was last transferred or recognised as equal. Downloads
/// and uploads give the local file the server's size and time, so after a transfer the local
/// file *is* the base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    /// The server's ETag then (quotes included), when it sent one.
    pub etag: Option<String>,
    pub local: LocalFile,
}

/// Why two sides can't be reconciled without someone choosing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    /// Both changed since the base.
    BothChanged,
    /// Changed here, deleted on the server.
    ChangedHereDeletedThere,
    /// Deleted here, changed on the server.
    DeletedHereChangedThere,
    /// Both exist but nothing is recorded and they differ.
    BothNew,
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Conflict::BothChanged => "changed here and on the server",
            Conflict::ChangedHereDeletedThere => "changed here, deleted on the server",
            Conflict::DeletedHereChangedThere => "deleted here, changed on the server",
            Conflict::BothNew => "different versions here and on the server, none recorded before",
        })
    }
}

/// What an upload may find on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Replaces {
    /// Nothing: the file must not exist there yet (`If-None-Match: *`).
    Nothing,
    /// Exactly this version, as the server named it (`If-Match`). Anyone else's change since
    /// makes the upload fail instead of being overwritten.
    Version(String),
    /// Whatever is there. Only when the server names no versions at all, so none can be
    /// checked; the file's size and time showed it unchanged.
    Unchecked,
}

/// What to do about one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send the local file, replacing what `replaces` says is on the server.
    Upload { replaces: Replaces },
    /// Fetch the server's file over the local one (or to a new place).
    Download,
    /// Both sides already hold the same file (equal size and time), whether nothing is recorded or
    /// both moved on to it (a transfer that finished without being recorded): record it, transfer nothing.
    Adopt,
    /// Needs a decision; nothing is transferred or overwritten.
    Conflict(Conflict),
    /// Deleted on the server, unchanged here. Reported; nothing is deleted.
    RemoteDeleted,
    /// Deleted here, unchanged on the server. Reported; nothing is deleted.
    LocalDeleted,
    /// Gone on both sides: drop the record.
    Forget,
}

impl Action {
    /// Whether this moves bytes or changes the record (everything except conflicts and the
    /// two "deleted" reports).
    pub fn is_work(&self) -> bool {
        !matches!(self, Action::Conflict(_) | Action::RemoteDeleted | Action::LocalDeleted)
    }
}

/// The actions for a set of files, ordered by path. Files that need nothing are not listed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub items: Vec<(RemotePath, Action)>,
}

impl Plan {
    pub fn uploads(&self) -> impl Iterator<Item = &RemotePath> {
        self.items.iter().filter(|(_, a)| matches!(a, Action::Upload { .. })).map(|(p, _)| p)
    }

    pub fn downloads(&self) -> impl Iterator<Item = &RemotePath> {
        self.items.iter().filter(|(_, a)| matches!(a, Action::Download)).map(|(p, _)| p)
    }

    pub fn conflicts(&self) -> impl Iterator<Item = (&RemotePath, Conflict)> {
        self.items.iter().filter_map(|(p, a)| if let Action::Conflict(c) = a { Some((p, *c)) } else { None })
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

fn remote_changed(r: &RemoteFile, b: &Base) -> bool {
    match (&r.etag, &b.etag) {
        (Some(re), Some(be)) => !same_etag(re, be),
        // no ETag to compare: after a transfer the local file has the server's size and time
        _ => r.size != b.local.size || r.modified.is_some_and(|m| m != b.local.modified),
    }
}

fn same_file(l: &LocalFile, r: &RemoteFile) -> bool {
    l.size == r.size && r.modified == Some(l.modified)
}

fn decide(local: Option<&LocalFile>, remote: Option<&RemoteFile>, base: Option<&Base>) -> Option<Action> {
    match (local, remote, base) {
        (None, None, None) => None,
        (None, None, Some(_)) => Some(Action::Forget),
        (Some(_), None, None) => Some(Action::Upload { replaces: Replaces::Nothing }),
        (None, Some(_), None) => Some(Action::Download),
        (Some(l), None, Some(b)) => Some(if *l == b.local { Action::RemoteDeleted } else { Action::Conflict(Conflict::ChangedHereDeletedThere) }),
        (None, Some(r), Some(b)) => {
            Some(if remote_changed(r, b) { Action::Conflict(Conflict::DeletedHereChangedThere) } else { Action::LocalDeleted })
        }
        (Some(l), Some(r), None) => Some(if same_file(l, r) { Action::Adopt } else { Action::Conflict(Conflict::BothNew) }),
        (Some(l), Some(r), Some(b)) => match (*l != b.local, remote_changed(r, b)) {
            (false, false) => None,
            (true, false) => {
                Some(Action::Upload { replaces: b.etag.clone().or_else(|| r.etag.clone()).map_or(Replaces::Unchecked, Replaces::Version) })
            }
            (false, true) => Some(Action::Download),
            // both moved on, but to the very same file: a transfer that finished without being recorded
            // (the answer to an upload was lost, or the app was closed after a download)
            (true, true) if same_file(l, r) => Some(Action::Adopt),
            (true, true) => Some(Action::Conflict(Conflict::BothChanged)),
        },
    }
}

/// Compare the three views of the files and say what to do about each.
pub fn plan(local: &BTreeMap<RemotePath, LocalFile>, remote: &BTreeMap<RemotePath, RemoteFile>, base: &BTreeMap<RemotePath, Base>) -> Plan {
    let mut paths: Vec<&RemotePath> = local.keys().chain(remote.keys()).chain(base.keys()).collect();
    paths.sort();
    paths.dedup();
    let items = paths.into_iter().filter_map(|p| decide(local.get(p), remote.get(p), base.get(p)).map(|a| (p.clone(), a))).collect();
    Plan { items }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RemotePath {
        RemotePath::parse(s).unwrap()
    }

    fn lf(size: u64, modified: i64) -> LocalFile {
        LocalFile { size, modified }
    }

    fn rf(etag: &str, size: u64, modified: i64) -> RemoteFile {
        RemoteFile { etag: Some(etag.into()), size, modified: Some(modified) }
    }

    fn base(etag: &str, size: u64, modified: i64) -> Base {
        Base { etag: Some(etag.into()), local: lf(size, modified) }
    }

    /// The action for one file `a.xmp` from the three views.
    fn one(local: Option<LocalFile>, remote: Option<RemoteFile>, base: Option<Base>) -> Option<Action> {
        let name = p("/a.xmp");
        let l: BTreeMap<_, _> = local.into_iter().map(|v| (name.clone(), v)).collect();
        let r: BTreeMap<_, _> = remote.into_iter().map(|v| (name.clone(), v)).collect();
        let b: BTreeMap<_, _> = base.into_iter().map(|v| (name.clone(), v)).collect();
        plan(&l, &r, &b).items.into_iter().next().map(|(_, a)| a)
    }

    #[test]
    fn every_case_of_the_table() {
        use Action::{Adopt, Download, Forget, LocalDeleted, RemoteDeleted, Upload};
        use Conflict::{BothChanged, BothNew, ChangedHereDeletedThere, DeletedHereChangedThere};
        let b = || Some(base("\"v1\"", 10, 100));
        // nothing known, nothing there
        assert_eq!(one(None, None, None), None);
        // new on one side only
        assert_eq!(one(Some(lf(10, 100)), None, None), Some(Upload { replaces: Replaces::Nothing }));
        assert_eq!(one(None, Some(rf("\"v1\"", 10, 100)), None), Some(Download));
        // both exist, nothing recorded: equal means adopt, different needs a decision
        assert_eq!(one(Some(lf(10, 100)), Some(rf("\"v9\"", 10, 100)), None), Some(Adopt));
        assert_eq!(one(Some(lf(10, 100)), Some(rf("\"v9\"", 11, 100)), None), Some(Action::Conflict(BothNew)));
        assert_eq!(one(Some(lf(10, 100)), Some(rf("\"v9\"", 10, 101)), None), Some(Action::Conflict(BothNew)));
        // all agree
        assert_eq!(one(Some(lf(10, 100)), Some(rf("\"v1\"", 10, 100)), b()), None);
        // changed here only: upload, replacing exactly the version the base saw
        assert_eq!(one(Some(lf(12, 105)), Some(rf("\"v1\"", 10, 100)), b()), Some(Upload { replaces: Replaces::Version("\"v1\"".into()) }));
        assert_eq!(
            one(Some(lf(10, 101)), Some(rf("\"v1\"", 10, 100)), b()),
            Some(Upload { replaces: Replaces::Version("\"v1\"".into()) }),
            "a newer time alone counts as changed"
        );
        // changed on the server only
        assert_eq!(one(Some(lf(10, 100)), Some(rf("\"v2\"", 14, 200)), b()), Some(Download));
        // both changed
        assert_eq!(one(Some(lf(12, 105)), Some(rf("\"v2\"", 14, 200)), b()), Some(Action::Conflict(BothChanged)));
        // both changed, to the very same file (an upload whose answer was lost, a download that was not
        // recorded): nothing to decide. The same size at another time is two edits.
        assert_eq!(one(Some(lf(14, 200)), Some(rf("\"v2\"", 14, 200)), b()), Some(Adopt));
        assert_eq!(one(Some(lf(14, 201)), Some(rf("\"v2\"", 14, 200)), b()), Some(Action::Conflict(BothChanged)));
        // deleted on one side
        assert_eq!(one(Some(lf(10, 100)), None, b()), Some(RemoteDeleted));
        assert_eq!(one(None, Some(rf("\"v1\"", 10, 100)), b()), Some(LocalDeleted));
        // deleted on one side while the other changed
        assert_eq!(one(Some(lf(12, 105)), None, b()), Some(Action::Conflict(ChangedHereDeletedThere)));
        assert_eq!(one(None, Some(rf("\"v2\"", 14, 200)), b()), Some(Action::Conflict(DeletedHereChangedThere)));
        // gone everywhere: drop the record
        assert_eq!(one(None, None, b()), Some(Forget));
    }

    #[test]
    fn an_existing_file_is_never_treated_as_a_new_one() {
        // found by the property test: changed here, on the server with no ETag anywhere. This must
        // not become a create-only upload (If-None-Match: *), which the server would refuse.
        let r = RemoteFile { etag: None, size: 0, modified: None };
        let b = Base { etag: None, local: lf(0, 0) };
        assert_eq!(one(Some(lf(0, 2)), Some(r), Some(b)), Some(Action::Upload { replaces: Replaces::Unchecked }));
    }

    #[test]
    fn etags_compare_without_the_weak_prefix_and_surrounding_space() {
        assert!(same_etag("\"a\"", " W/\"a\" "));
        assert!(!same_etag("\"a\"", "\"b\""));
        // a weak ETag from the server does not look like a change
        assert_eq!(one(Some(lf(10, 100)), Some(rf("W/\"v1\"", 10, 100)), Some(base("\"v1\"", 10, 100))), None);
    }

    #[test]
    fn without_an_etag_the_size_and_time_decide() {
        let no_etag = |size, modified| RemoteFile { etag: None, size, modified: Some(modified) };
        let b = Some(Base { etag: None, local: lf(10, 100) });
        assert_eq!(one(Some(lf(10, 100)), Some(no_etag(10, 100)), b.clone()), None);
        assert_eq!(one(Some(lf(10, 100)), Some(no_etag(11, 100)), b.clone()), Some(Action::Download));
        assert_eq!(one(Some(lf(10, 100)), Some(no_etag(10, 150)), b.clone()), Some(Action::Download));
        // the replace condition falls back to the server's current ETag, if it has one
        let r = RemoteFile { etag: Some("\"now\"".into()), size: 10, modified: Some(100) };
        assert_eq!(one(Some(lf(12, 105)), Some(r), b), Some(Action::Upload { replaces: Replaces::Version("\"now\"".into()) }));
        // a server that sends neither ETag nor time: only the size can tell
        let silent = RemoteFile { etag: None, size: 10, modified: None };
        assert_eq!(one(Some(lf(10, 100)), Some(silent), Some(Base { etag: None, local: lf(10, 100) })), None);
    }

    #[test]
    fn the_plan_is_sorted_by_path_and_lists_only_work() {
        let mut local = BTreeMap::new();
        let mut remote = BTreeMap::new();
        let mut known = BTreeMap::new();
        local.insert(p("/z/new.xmp"), lf(1, 1));
        remote.insert(p("/b/server.xmp"), rf("\"s\"", 2, 2));
        local.insert(p("/a/same.xmp"), lf(5, 5));
        remote.insert(p("/a/same.xmp"), rf("\"x\"", 5, 5));
        known.insert(p("/a/same.xmp"), base("\"x\"", 5, 5));
        known.insert(p("/c/gone.xmp"), base("\"g\"", 3, 3));
        let plan = plan(&local, &remote, &known);
        let order: Vec<String> = plan.items.iter().map(|(p, _)| p.to_string()).collect();
        assert_eq!(order, ["/b/server.xmp", "/c/gone.xmp", "/z/new.xmp"]);
        assert_eq!(plan.uploads().map(ToString::to_string).collect::<Vec<_>>(), ["/z/new.xmp"]);
        assert_eq!(plan.downloads().map(ToString::to_string).collect::<Vec<_>>(), ["/b/server.xmp"]);
        assert_eq!(plan.conflicts().count(), 0);
        assert!(!plan.is_empty() && super::plan(&BTreeMap::new(), &BTreeMap::new(), &BTreeMap::new()).is_empty());
    }

    #[test]
    fn what_counts_as_work() {
        assert!(
            Action::Download.is_work()
                && Action::Adopt.is_work()
                && Action::Forget.is_work()
                && Action::Upload { replaces: Replaces::Nothing }.is_work()
        );
        assert!(!Action::Conflict(Conflict::BothChanged).is_work() && !Action::RemoteDeleted.is_work() && !Action::LocalDeleted.is_work());
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        #[derive(Clone, Debug)]
        struct World {
            local: Option<(u64, i64)>,
            remote: Option<(Option<u8>, u64, Option<i64>)>,
            base: Option<(Option<u8>, u64, i64)>,
        }

        fn world() -> impl Strategy<Value = World> {
            (
                proptest::option::of((0u64..3, 0i64..3)),
                proptest::option::of((proptest::option::of(0u8..3), 0u64..3, proptest::option::of(0i64..3))),
                proptest::option::of((proptest::option::of(0u8..3), 0u64..3, 0i64..3)),
            )
                .prop_map(|(local, remote, base)| World { local, remote, base })
        }

        fn etag(n: u8) -> String {
            format!("\"e{n}\"")
        }

        struct Maps {
            local: BTreeMap<RemotePath, LocalFile>,
            remote: BTreeMap<RemotePath, RemoteFile>,
            base: BTreeMap<RemotePath, Base>,
        }

        fn maps(worlds: &[World]) -> Maps {
            let mut m = Maps { local: BTreeMap::new(), remote: BTreeMap::new(), base: BTreeMap::new() };
            for (i, w) in worlds.iter().enumerate() {
                let name = p(&format!("/f{i}.xmp"));
                if let Some((size, modified)) = w.local {
                    m.local.insert(name.clone(), lf(size, modified));
                }
                if let Some((e, size, modified)) = w.remote {
                    m.remote.insert(name.clone(), RemoteFile { etag: e.map(etag), size, modified });
                }
                if let Some((e, size, modified)) = w.base {
                    m.base.insert(name, Base { etag: e.map(etag), local: lf(size, modified) });
                }
            }
            m
        }

        /// Do what the plan says, the way an executor would, to see where it leads.
        fn apply(m: &mut Maps, plan: &Plan, fresh: &mut u32) {
            for (path, action) in &plan.items {
                match action {
                    Action::Upload { .. } => {
                        let l = m.local[path];
                        *fresh += 1;
                        let e = format!("\"n{fresh}\"");
                        m.remote.insert(path.clone(), RemoteFile { etag: Some(e.clone()), size: l.size, modified: Some(l.modified) });
                        m.base.insert(path.clone(), Base { etag: Some(e), local: l });
                    }
                    Action::Download => {
                        let r = m.remote[path].clone();
                        let l = lf(r.size, r.modified.unwrap_or(0));
                        m.local.insert(path.clone(), l);
                        m.base.insert(path.clone(), Base { etag: r.etag, local: l });
                    }
                    Action::Adopt => {
                        let (l, r) = (m.local[path], m.remote[path].clone());
                        m.base.insert(path.clone(), Base { etag: r.etag, local: l });
                    }
                    Action::Forget => {
                        m.base.remove(path);
                    }
                    Action::Conflict(_) | Action::RemoteDeleted | Action::LocalDeleted => {}
                }
            }
        }

        proptest! {
            /// An action never refers to a file that is not there, and an upload only ever
            /// replaces the version the base saw.
            #[test]
            fn actions_only_use_what_exists(worlds in proptest::collection::vec(world(), 1..8)) {
                let m = maps(&worlds);
                for (path, action) in &plan(&m.local, &m.remote, &m.base).items {
                    match action {
                        Action::Upload { replaces } => {
                            prop_assert!(m.local.contains_key(path));
                            let known = m.base.get(path).and_then(|b| b.etag.clone()).or_else(|| m.remote.get(path).and_then(|r| r.etag.clone()));
                            match replaces {
                                Replaces::Nothing => prop_assert!(!m.remote.contains_key(path), "create-only upload to a file that exists"),
                                Replaces::Version(e) => prop_assert_eq!(Some(e), known.as_ref()),
                                Replaces::Unchecked => {
                                    prop_assert!(m.remote.contains_key(path) && known.is_none(), "an unchecked upload although a version could be checked");
                                }
                            }
                        }
                        Action::Download => prop_assert!(m.remote.contains_key(path)),
                        Action::Adopt => prop_assert!(m.local.contains_key(path) && m.remote.contains_key(path)),
                        Action::Forget => prop_assert!(!m.local.contains_key(path) && !m.remote.contains_key(path)),
                        Action::RemoteDeleted => prop_assert!(m.local.contains_key(path) && !m.remote.contains_key(path)),
                        Action::LocalDeleted => prop_assert!(!m.local.contains_key(path) && m.remote.contains_key(path)),
                        Action::Conflict(_) => {}
                    }
                }
            }

            /// Doing the planned work reaches a state where nothing is left to do, except
            /// conflicts and deletions, which are reported and stay as they are.
            #[test]
            fn doing_the_work_converges(worlds in proptest::collection::vec(world(), 1..8)) {
                let mut m = maps(&worlds);
                let mut fresh = 0;
                let first = plan(&m.local, &m.remote, &m.base);
                apply(&mut m, &first, &mut fresh);
                let second = plan(&m.local, &m.remote, &m.base);
                prop_assert!(second.items.iter().all(|(_, a)| !a.is_work()), "left over: {:?}", second.items.iter().filter(|(_, a)| a.is_work()).collect::<Vec<_>>());
                // and the conflicts and reports are exactly the ones of the first plan
                let kept: Vec<_> = first.items.iter().filter(|(_, a)| !a.is_work()).cloned().collect();
                prop_assert_eq!(second.items, kept);
            }

            /// A conflict never changes either side: both survive until someone decides.
            #[test]
            fn a_conflict_moves_nothing(worlds in proptest::collection::vec(world(), 1..8)) {
                let m = maps(&worlds);
                let before = (m.local.clone(), m.remote.clone());
                let mut after = maps(&worlds);
                let mut fresh = 0;
                let only_conflicts = Plan { items: plan(&m.local, &m.remote, &m.base).items.into_iter().filter(|(_, a)| !a.is_work()).collect() };
                apply(&mut after, &only_conflicts, &mut fresh);
                prop_assert_eq!((after.local, after.remote), before);
            }
        }
    }
}
