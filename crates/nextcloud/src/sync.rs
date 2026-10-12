//! Keeping a folder pair in step: look at both sides, plan, carry the plan out.
//!
//! [`prepare`] lists the server and scans the local folder and returns a [`Prepared`] plan that
//! nothing has acted on, so a caller can show what would happen. [`execute`] then carries it out
//! with the client and records every transfer in the [`State`]. [`sync_pair`] does both.
//!
//! Listing a big folder is slow on a small server (a folder of 2,400 photos took six seconds on a
//! Raspberry Pi), so the listing is skipped when it can be: Nextcloud gives a folder a new ETag
//! whenever anything below it changes, and when the ETags seen at the last *complete* run are
//! still the same, what the server has is exactly what the [`State`] records. A run is complete
//! when it left nothing unresolved. The ETags kept are the ones seen *before* the run acted, so
//! a run that uploaded something is followed by one that lists once more (its uploads changed
//! the ETags), and a change by someone else during a run is never mistaken for the run's own.
//!
//! Errors that concern the whole run (no network, wrong password, a redirect, no space, cancel)
//! end it; an error that concerns one file (it changed on the server meanwhile, no permission)
//! is reported for that file and the rest go on. The [`State`] is always up to date with what was
//! done, also when the run ends with an error, so the caller should save it either way.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::client::{Client, ClientError, Precondition, Remote};
use crate::dav::Entry;
use crate::pair::{FolderPair, ScanLimits, State, scan_local};
use crate::path::{MAX_DEPTH, RemotePath};
use crate::plan::{Action, Base, Conflict, LocalFile, Plan, RemoteFile, plan};

/// Most folders listed in one run.
const MAX_FOLDERS: usize = 20_000;
/// Most files one run may see on the server.
const MAX_REMOTE_FILES: usize = 200_000;

/// What a run reports while it works.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// Listing this server folder.
    Listing(&'a RemotePath),
    /// The plan is made.
    Planned { uploads: usize, downloads: usize, conflicts: usize },
    /// Bytes of one transfer so far.
    Transfer { path: &'a RemotePath, upload: bool, done: u64, total: u64 },
    /// One file is done.
    Finished { path: &'a RemotePath },
}

/// What a run found and did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub uploaded: Vec<RemotePath>,
    pub downloaded: Vec<RemotePath>,
    pub adopted: Vec<RemotePath>,
    pub forgotten: Vec<RemotePath>,
    /// Needs someone to choose; nothing was changed for these.
    pub conflicts: Vec<(RemotePath, Conflict)>,
    /// Deleted on the server since the last run; the local file was kept.
    pub remote_deleted: Vec<RemotePath>,
    /// Deleted here since the last run; the server's file was kept.
    pub local_deleted: Vec<RemotePath>,
    pub failed: Vec<(RemotePath, ClientError)>,
    /// Local files left out of the scan (unusable names, too deep, too many).
    pub skipped_local: usize,
    /// The server was listed (false: the quick check found nothing changed).
    pub listed: bool,
}

impl Report {
    /// Nothing is left unresolved.
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty() && self.remote_deleted.is_empty() && self.local_deleted.is_empty() && self.failed.is_empty()
    }
}

/// Both sides and what to do about them, before anything is done.
#[derive(Debug)]
pub struct Prepared {
    pub plan: Plan,
    local: BTreeMap<RemotePath, LocalFile>,
    remote: BTreeMap<RemotePath, RemoteFile>,
    /// The server's folder ETags as listed, for the next quick check.
    folders: BTreeMap<RemotePath, String>,
    /// Whether the server was listed (false: the quick check found nothing changed).
    listed: bool,
    skipped_local: usize,
}

/// A folder that is not on the server yet is an empty folder.
fn list_or_empty(client: &Client, dir: &RemotePath, cancel: &AtomicBool) -> Result<Vec<Entry>, ClientError> {
    match client.list(dir, cancel) {
        Ok(l) => Ok(l.entries),
        Err(ClientError::NotFound) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn from_state(state: &State, below: &RemotePath, into: &mut BTreeMap<RemotePath, RemoteFile>) {
    for (path, base) in state.files().iter().filter(|(p, _)| p.starts_with(below)) {
        into.insert(path.clone(), RemoteFile { etag: base.etag.clone(), size: base.local.size, modified: Some(base.local.modified) });
    }
}

struct Tree {
    files: BTreeMap<RemotePath, RemoteFile>,
    folders: BTreeMap<RemotePath, String>,
    listed: bool,
}

/// What the server has below `root`, listing as little as the recorded folder versions allow.
fn list_tree(
    client: &Client,
    root: &RemotePath,
    state: &State,
    select: fn(&str) -> bool,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&Event<'_>),
) -> Result<Tree, ClientError> {
    let mut tree = Tree { files: BTreeMap::new(), folders: BTreeMap::new(), listed: false };
    // the quick check: the top folder's version is the one recorded at the last complete run
    let top = match client.stat(root, cancel) {
        Ok(e) => e,
        Err(ClientError::NotFound) => return Ok(tree),
        Err(e) => return Err(e),
    };
    if let Some(etag) = &top.etag {
        tree.folders.insert(root.clone(), etag.clone());
        if state.is_complete() && state.folders().get(root).is_some_and(|known| crate::plan::same_etag(known, etag)) {
            tree.folders.clone_from(state.folders());
            from_state(state, root, &mut tree.files);
            return Ok(tree);
        }
    }
    tree.listed = true;
    let mut todo = vec![(root.clone(), 0usize)];
    let mut listed_folders = 0;
    while let Some((dir, depth)) = todo.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(ClientError::Cancelled);
        }
        listed_folders += 1;
        if listed_folders > MAX_FOLDERS {
            return Err(ClientError::Dav(crate::dav::DavError::TooMany));
        }
        report(&Event::Listing(&dir));
        for e in list_or_empty(client, &dir, cancel)? {
            if e.is_dir {
                if depth + 1 > MAX_DEPTH {
                    continue;
                }
                if let Some(etag) = &e.etag {
                    let unchanged = state.is_complete() && state.folders().get(&e.path).is_some_and(|known| crate::plan::same_etag(known, etag));
                    tree.folders.insert(e.path.clone(), etag.clone());
                    if unchanged {
                        // nothing below changed since the last complete run: take its files from the record
                        from_state(state, &e.path, &mut tree.files);
                        for (p, v) in state.folders().iter().filter(|(p, _)| p.starts_with(&e.path) && **p != e.path) {
                            tree.folders.insert(p.clone(), v.clone());
                        }
                        continue;
                    }
                }
                todo.push((e.path.clone(), depth + 1));
            } else if e.path.file_name().is_some_and(select) {
                if tree.files.len() >= MAX_REMOTE_FILES {
                    return Err(ClientError::Dav(crate::dav::DavError::TooMany));
                }
                tree.files.insert(e.path.clone(), RemoteFile::from(&e));
            }
        }
    }
    Ok(tree)
}

/// Look at both sides and plan; nothing is changed on either.
pub fn prepare(
    client: &Client,
    pair: &FolderPair,
    state: &State,
    select: fn(&str) -> bool,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&Event<'_>),
) -> Result<Prepared, ClientError> {
    let tree = list_tree(client, &pair.remote, state, select, cancel, report)?;
    let scan = scan_local(pair, select, ScanLimits::default()).map_err(|e| ClientError::Io(format!("can't read the local folder: {e}")))?;
    let plan = plan(&scan.files, &tree.files, state.files());
    let count = |f: fn(&Action) -> bool| plan.items.iter().filter(|(_, a)| f(a)).count();
    report(&Event::Planned {
        uploads: count(|a| matches!(a, Action::Upload { .. })),
        downloads: count(|a| matches!(a, Action::Download)),
        conflicts: count(|a| matches!(a, Action::Conflict(_))),
    });
    Ok(Prepared { plan, local: scan.files, remote: tree.files, folders: tree.folders, listed: tree.listed, skipped_local: scan.skipped_total })
}

/// Whether an error ends the whole run rather than one file.
fn ends_the_run(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Cancelled
            | ClientError::BadCredentials
            | ClientError::Network(_)
            | ClientError::Redirect(_)
            | ClientError::NoSpace
            | ClientError::InsecureServer(_)
            | ClientError::BadServer(_)
    )
}

fn disk_state(path: &std::path::Path) -> Result<LocalFile, ClientError> {
    let meta = std::fs::metadata(path).map_err(|e| ClientError::Io(format!("can't read {}: {e}", path.display())))?;
    let modified =
        meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).and_then(|d| i64::try_from(d.as_secs()).ok()).unwrap_or(0);
    Ok(LocalFile { size: meta.len(), modified })
}

/// Carry out a prepared plan.
pub fn execute(
    client: &Client,
    pair: &FolderPair,
    state: &mut State,
    prepared: Prepared,
    cancel: &AtomicBool,
    report_event: &mut dyn FnMut(&Event<'_>),
) -> Result<Report, ClientError> {
    let Prepared { plan, local, remote, folders, listed, skipped_local } = prepared;
    let mut report = Report { listed, skipped_local, ..Report::default() };
    let mut ensured: BTreeSet<RemotePath> = BTreeSet::new();
    let mut failure = None;
    for (path, action) in &plan.items {
        if cancel.load(Ordering::Relaxed) {
            failure = Some(ClientError::Cancelled);
            break;
        }
        let done = match action {
            Action::Upload { replaces } => {
                let (Some(here), Some(src)) = (local.get(path), pair.local_for(path)) else { continue };
                let result = (|| {
                    if let Some(dir) = path.parent()
                        && !ensured.contains(&dir)
                    {
                        client.mkcol_all(&dir, cancel)?;
                        ensured.insert(dir);
                    }
                    let up = client.upload(
                        path,
                        &src,
                        &Precondition::from(replaces),
                        Some(here.modified),
                        &mut |d, t| report_event(&Event::Transfer { path, upload: true, done: d, total: t }),
                        cancel,
                    )?;
                    let etag = match up.etag {
                        Some(e) => Some(e),
                        None => client.stat(path, cancel)?.etag,
                    };
                    Ok::<_, ClientError>(Base { etag, local: *here })
                })();
                result.map(|base| {
                    state.record(path.clone(), base);
                    report.uploaded.push(path.clone());
                })
            }
            Action::Download => {
                let (Some(theirs), Some(dest)) = (remote.get(path), pair.local_for(path)) else { continue };
                let expect = Remote { size: theirs.size, etag: theirs.etag.clone(), modified: theirs.modified };
                client
                    .download(path, &dest, &expect, &mut |d, t| report_event(&Event::Transfer { path, upload: false, done: d, total: t }), cancel)
                    .and_then(|()| disk_state(&dest))
                    .map(|here| {
                        state.record(path.clone(), Base { etag: theirs.etag.clone(), local: here });
                        report.downloaded.push(path.clone());
                    })
            }
            Action::Adopt => {
                if let (Some(here), Some(theirs)) = (local.get(path), remote.get(path)) {
                    state.record(path.clone(), Base { etag: theirs.etag.clone(), local: *here });
                    report.adopted.push(path.clone());
                }
                Ok(())
            }
            Action::Forget => {
                state.forget(path);
                report.forgotten.push(path.clone());
                Ok(())
            }
            Action::Conflict(c) => {
                report.conflicts.push((path.clone(), *c));
                continue;
            }
            Action::RemoteDeleted => {
                report.remote_deleted.push(path.clone());
                continue;
            }
            Action::LocalDeleted => {
                report.local_deleted.push(path.clone());
                continue;
            }
        };
        match done {
            Ok(()) => report_event(&Event::Finished { path }),
            Err(e) if ends_the_run(&e) => {
                failure = Some(e);
                break;
            }
            Err(e) => report.failed.push((path.clone(), e)),
        }
    }
    // the quick check may be used next time only if this run left nothing open
    let complete = failure.is_none() && report.is_clean();
    state.set_tree(if complete { folders } else { BTreeMap::new() }, complete);
    match failure {
        Some(e) => Err(e),
        None => Ok(report),
    }
}

/// Bring the folder pair in step: [`prepare`], then [`execute`]. `select` says which file names
/// take part (see [`crate::pair::is_sidecar`]).
pub fn sync_pair(
    client: &Client,
    pair: &FolderPair,
    state: &mut State,
    select: fn(&str) -> bool,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(&Event<'_>),
) -> Result<Report, ClientError> {
    let prepared = prepare(client, pair, state, select, cancel, report)?;
    execute(client, pair, state, prepared, cancel, report)
}
