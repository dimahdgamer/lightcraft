//! A folder pair: a folder on this machine and the folder on the server it is kept in step with,
//! the record of what both looked like at the last transfer, and a scan of the local side.
//!
//! The record ([`State`]) is saved as JSON. It is read back as hostile input too: a hand-edited or
//! damaged record must not be able to name a file outside the pair, so every path in it goes
//! through [`RemotePath::parse`], and what fails is dropped (and counted) rather than trusted.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::path::RemotePath;
use crate::plan::{Base, LocalFile};

/// Largest record read back.
pub const MAX_STATE_BYTES: usize = 64 << 20;
/// Most files one record may hold.
pub const MAX_STATE_FILES: usize = 1_000_000;
/// The version of the record's format.
const STATE_VERSION: u32 = 1;

/// The server folder and the local folder that belong together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderPair {
    pub remote: RemotePath,
    pub local: PathBuf,
}

/// `path` below `base`, comparing names without regard to ASCII case on Windows, where a file
/// dialog and a stored path may spell the same folder differently.
fn strip_local_prefix(path: &Path, base: &Path) -> Option<PathBuf> {
    if let Ok(rest) = path.strip_prefix(base) {
        return Some(rest.to_path_buf());
    }
    if !cfg!(windows) {
        return None;
    }
    let mut rest = path.components();
    for b in base.components() {
        let p = rest.next()?;
        if !p.as_os_str().to_string_lossy().eq_ignore_ascii_case(&b.as_os_str().to_string_lossy()) {
            return None;
        }
    }
    Some(rest.as_path().to_path_buf())
}

impl FolderPair {
    pub fn new(remote: RemotePath, local: PathBuf) -> FolderPair {
        FolderPair { remote, local }
    }

    /// Where a file of the server's folder belongs on this machine (`None` if `path` is not
    /// inside the server folder). `path` is checked, so the result stays below [`Self::local`].
    pub fn local_for(&self, path: &RemotePath) -> Option<PathBuf> {
        path.strip_prefix(&self.remote).map(|rest| rest.to_local(&self.local))
    }

    /// The server path of a local file (`None` if it is not inside the local folder, or its
    /// name could not be used on the server).
    pub fn remote_for(&self, local: &Path) -> Option<RemotePath> {
        let rest = strip_local_prefix(local, &self.local)?;
        let mut out = self.remote.clone();
        for c in rest.components() {
            match c {
                Component::Normal(name) => out = out.join(name.to_str()?).ok()?,
                _ => return None,
            }
        }
        Some(out)
    }
}

/// Why a record could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateError {
    TooLarge,
    TooMany,
    /// Not JSON of the expected shape.
    Unreadable(String),
    /// Written by a newer version that this one does not understand.
    NewerVersion(u32),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateError::TooLarge => write!(f, "the sync record is larger than {} MiB", MAX_STATE_BYTES >> 20),
            StateError::TooMany => write!(f, "the sync record lists more than {MAX_STATE_FILES} files"),
            StateError::Unreadable(e) => write!(f, "the sync record is unreadable: {e}"),
            StateError::NewerVersion(v) => write!(f, "the sync record has format {v}, which this version does not know"),
        }
    }
}

impl std::error::Error for StateError {}

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    files: BTreeMap<String, Base>,
}

/// What both sides looked like after the last successful transfer of each file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    files: BTreeMap<RemotePath, Base>,
}

impl State {
    pub fn files(&self) -> &BTreeMap<RemotePath, Base> {
        &self.files
    }

    pub fn record(&mut self, path: RemotePath, base: Base) {
        self.files.insert(path, base);
    }

    pub fn forget(&mut self, path: &RemotePath) {
        self.files.remove(path);
    }

    pub fn to_json(&self) -> String {
        let files = self.files.iter().map(|(p, b)| (p.to_string(), b.clone())).collect();
        // a map with string keys and plain fields always serialises
        serde_json::to_string_pretty(&Record { version: STATE_VERSION, files }).unwrap_or_default()
    }

    /// Read a record. The number is how many entries were dropped because their path was not
    /// usable (nothing was trusted blindly).
    pub fn from_json(text: &str) -> Result<(State, usize), StateError> {
        if text.len() > MAX_STATE_BYTES {
            return Err(StateError::TooLarge);
        }
        let rec: Record = serde_json::from_str(text).map_err(|e| StateError::Unreadable(e.to_string()))?;
        if rec.version > STATE_VERSION {
            return Err(StateError::NewerVersion(rec.version));
        }
        if rec.files.len() > MAX_STATE_FILES {
            return Err(StateError::TooMany);
        }
        let mut state = State::default();
        let mut dropped = 0;
        for (key, base) in rec.files {
            match RemotePath::parse(&key) {
                // a path must be written the way it is displayed, or it is not the one it names
                Ok(p) if !p.is_root() && p.to_string() == key => {
                    state.files.insert(p, base);
                }
                _ => dropped += 1,
            }
        }
        Ok((state, dropped))
    }
}

/// What a scan of the local folder found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub files: BTreeMap<RemotePath, LocalFile>,
    /// The first few files left out, with the reason.
    pub skipped: Vec<(PathBuf, String)>,
    pub skipped_total: usize,
}

/// Limits of a scan.
#[derive(Clone, Copy, Debug)]
pub struct ScanLimits {
    pub max_files: usize,
    pub max_depth: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        ScanLimits { max_files: 200_000, max_depth: crate::path::MAX_DEPTH }
    }
}

const MAX_SKIPPED_KEPT: usize = 50;

fn skip(scan: &mut Scan, path: PathBuf, why: &str) {
    scan.skipped_total = scan.skipped_total.saturating_add(1);
    if scan.skipped.len() < MAX_SKIPPED_KEPT {
        scan.skipped.push((path, why.to_string()));
    }
}

/// The files below `dir` whose name `select` accepts, keyed by their server path under
/// `pair.remote`. Symbolic links are not followed. A folder that does not exist yet is an empty
/// scan. A file whose name can't be used on the server, or that is deeper than the limit, is left
/// out and reported.
pub fn scan_local(pair: &FolderPair, select: impl Fn(&str) -> bool, limits: ScanLimits) -> std::io::Result<Scan> {
    let mut scan = Scan::default();
    if !pair.local.is_dir() {
        return Ok(scan);
    }
    let mut stack = vec![(pair.local.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if depth + 1 > limits.max_depth {
                    skip(&mut scan, path, "nested too deeply");
                } else {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                skip(&mut scan, path, "the name is not valid text");
                continue;
            };
            if !kind.is_file() || !select(name) {
                continue;
            }
            let Some(remote) = pair.remote_for(&path) else {
                skip(&mut scan, path, "the name can't be used on the server");
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                skip(&mut scan, path, "can't read it");
                continue;
            };
            let Some(modified) = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).and_then(|d| i64::try_from(d.as_secs()).ok())
            else {
                skip(&mut scan, path, "its time is not usable");
                continue;
            };
            if scan.files.len() >= limits.max_files {
                skip(&mut scan, path, "too many files");
                continue;
            }
            scan.files.insert(remote, LocalFile { size: meta.len(), modified });
        }
    }
    Ok(scan)
}

/// Whether a file is an XMP sidecar (`IMG_0001.xmp`, `IMG_0001.CR2.xmp`).
pub fn is_sidecar(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("xmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Base;

    fn p(s: &str) -> RemotePath {
        RemotePath::parse(s).unwrap()
    }

    fn base(etag: &str, size: u64, modified: i64) -> Base {
        Base { etag: Some(etag.into()), local: LocalFile { size, modified } }
    }

    #[test]
    fn maps_paths_both_ways_and_never_leaves_the_pair() {
        let pair = FolderPair::new(p("/Photos/2026"), PathBuf::from("trip"));
        let local = pair.local_for(&p("/Photos/2026/Italy/a.xmp")).unwrap();
        assert_eq!(local, Path::new("trip").join("Italy").join("a.xmp"));
        assert_eq!(pair.remote_for(&local), Some(p("/Photos/2026/Italy/a.xmp")));
        assert_eq!(pair.local_for(&p("/Photos/2026")), Some(PathBuf::from("trip")));
        // outside the server folder (also a sibling that only shares a name prefix)
        assert_eq!(pair.local_for(&p("/Photos/2027/a.xmp")), None);
        assert_eq!(pair.local_for(&p("/Photos/2026x/a.xmp")), None);
        assert_eq!(pair.local_for(&p("/Other/a.xmp")), None);
        // outside the local folder, or climbing out of it
        assert_eq!(pair.remote_for(Path::new("elsewhere").join("a.xmp").as_path()), None);
        assert_eq!(pair.remote_for(&Path::new("trip").join("..").join("a.xmp")), None);
        assert_eq!(pair.remote_for(&Path::new("trip").join("CON").join("a.xmp")), None, "a name the server side can't use");
        assert_eq!(pair.remote_for(Path::new("trip")), Some(p("/Photos/2026")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_match_without_regard_to_case() {
        let pair = FolderPair::new(p("/Photos"), PathBuf::from(r"D:\Fotos\Reise"));
        assert_eq!(pair.remote_for(Path::new(r"d:\fotos\REISE\Italy\a.xmp")), Some(p("/Photos/Italy/a.xmp")));
        assert_eq!(pair.remote_for(Path::new(r"d:\fotos\Reisen\a.xmp")), None);
    }

    #[test]
    fn sidecar_names() {
        for yes in ["IMG_0001.xmp", "IMG_0001.CR2.xmp", "a.XMP", "x.y.Xmp"] {
            assert!(is_sidecar(yes), "{yes}");
        }
        for no in ["IMG_0001.CR2", "xmp", ".xmp", "a.xmp.bak", "a.xmpx", "", "a."] {
            assert!(!is_sidecar(no), "{no}");
        }
    }

    #[test]
    fn the_record_round_trips() {
        let mut s = State::default();
        s.record(p("/Photos/a b.xmp"), base("\"v1\"", 10, 100));
        s.record(p("/Photos/ü/c.xmp"), Base { etag: None, local: LocalFile { size: 0, modified: 0 } });
        let (back, dropped) = State::from_json(&s.to_json()).unwrap();
        assert_eq!((back.clone(), dropped), (s.clone(), 0));
        s.forget(&p("/Photos/a b.xmp"));
        assert_eq!(s.files().len(), 1);
        assert_eq!(State::from_json(&State::default().to_json()).unwrap().0, State::default());
    }

    #[test]
    fn a_damaged_or_hostile_record_is_not_trusted() {
        let one = |key: &str| {
            format!(r#"{{"version":1,"files":{{{}:{{"etag":"\"x\"","local":{{"size":1,"modified":2}}}}}}}}"#, serde_json::to_string(key).unwrap())
        };
        // paths that are not the exact, checked form of a file path are dropped and counted
        for bad in
            ["/../etc/passwd", "../x.xmp", "/a/../b.xmp", "//a.xmp", "/a//b.xmp", "a/b.xmp", "/", "", "/con/x.xmp", "/a/b\\c.xmp", "/a/\u{7}.xmp"]
        {
            let (s, dropped) = State::from_json(&one(bad)).unwrap();
            assert_eq!((s.files().len(), dropped), (0, 1), "{bad:?}");
        }
        let (s, dropped) = State::from_json(&one("/Photos/ok.xmp")).unwrap();
        assert_eq!((s.files().len(), dropped), (1, 0));
        assert!(matches!(State::from_json(""), Err(StateError::Unreadable(_))));
        assert!(matches!(State::from_json("{}"), Err(StateError::Unreadable(_))));
        assert!(matches!(State::from_json("[1,2]"), Err(StateError::Unreadable(_))));
        assert!(matches!(State::from_json(r#"{"version":1,"files":{"/a.xmp":{"etag":1}}}"#), Err(StateError::Unreadable(_))));
        assert_eq!(State::from_json(r#"{"version":2,"files":{}}"#), Err(StateError::NewerVersion(2)));
        assert_eq!(State::from_json(&" ".repeat(MAX_STATE_BYTES + 1)), Err(StateError::TooLarge));
    }

    mod disk {
        use super::*;
        use std::io::Write;

        struct Dir(PathBuf);
        impl Dir {
            fn new(tag: &str) -> Dir {
                let d = std::env::temp_dir().join(format!("lightcraft-nc-pair-{}-{tag}", std::process::id()));
                let _ = std::fs::remove_dir_all(&d);
                std::fs::create_dir_all(&d).unwrap();
                Dir(d)
            }
            fn file(&self, rel: &str, data: &[u8]) -> PathBuf {
                let f = self.0.join(rel);
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::File::create(&f).unwrap().write_all(data).unwrap();
                f
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn scans_selected_files_with_their_size_and_time() {
            let d = Dir::new("scan");
            d.file("IMG_1.xmp", b"12345");
            d.file("Italy/IMG_2.CR2.xmp", b"1234567");
            d.file("Italy/IMG_2.CR2", b"not selected");
            d.file("Italy/deep/IMG_3.XMP", b"x");
            let pair = FolderPair::new(p("/Photos"), d.0.clone());
            let scan = scan_local(&pair, is_sidecar, ScanLimits::default()).unwrap();
            let got: Vec<(String, u64)> = scan.files.iter().map(|(k, v)| (k.to_string(), v.size)).collect();
            assert_eq!(
                got,
                [("/Photos/IMG_1.xmp".to_string(), 5), ("/Photos/Italy/IMG_2.CR2.xmp".into(), 7), ("/Photos/Italy/deep/IMG_3.XMP".into(), 1)]
            );
            assert_eq!(scan.skipped_total, 0);
            let t = scan.files.values().next().unwrap().modified;
            assert!(t > 1_700_000_000, "the time is a plausible Unix time: {t}");
        }

        #[test]
        fn a_missing_folder_is_an_empty_scan_and_limits_hold() {
            let d = Dir::new("limits");
            let pair = FolderPair::new(p("/Photos"), d.0.join("not-there"));
            assert_eq!(scan_local(&pair, is_sidecar, ScanLimits::default()).unwrap(), Scan::default());
            for i in 0..5 {
                d.file(&format!("f{i}.xmp"), b"x");
            }
            d.file("a/b/c/d.xmp", b"x");
            let pair = FolderPair::new(p("/Photos"), d.0.clone());
            let few = scan_local(&pair, is_sidecar, ScanLimits { max_files: 3, max_depth: 64 }).unwrap();
            assert_eq!((few.files.len(), few.skipped_total), (3, 3));
            let shallow = scan_local(&pair, is_sidecar, ScanLimits { max_files: 100, max_depth: 1 }).unwrap();
            assert!(shallow.files.keys().all(|k| k.names().len() <= 2), "{:?}", shallow.files.keys().collect::<Vec<_>>());
            assert!(shallow.skipped.iter().any(|(_, why)| why.contains("deeply")));
        }

        #[test]
        fn names_the_server_cannot_hold_are_reported_not_scanned() {
            let d = Dir::new("names");
            d.file("ok.xmp", b"x");
            // legal on some systems, never on the server side (trailing dot is stripped by Windows, so use a colon-free case)
            if !cfg!(windows) {
                d.file("bad:name.xmp", b"x");
                let pair = FolderPair::new(p("/Photos"), d.0.clone());
                let scan = scan_local(&pair, is_sidecar, ScanLimits::default()).unwrap();
                assert_eq!(scan.files.len(), 1);
                assert_eq!(scan.skipped_total, 1);
            }
        }
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn from_json_never_panics(s in "\\PC{0,300}") {
                let _ = State::from_json(&s);
            }

            #[test]
            fn any_record_that_parses_holds_only_checked_paths(key in "\\PC{0,40}") {
                let text = format!(r#"{{"version":1,"files":{{{}:{{"etag":null,"local":{{"size":1,"modified":2}}}}}}}}"#, serde_json::to_string(&key).unwrap());
                if let Ok((s, _)) = State::from_json(&text) {
                    for k in s.files().keys() {
                        prop_assert!(!k.is_root());
                        prop_assert_eq!(RemotePath::parse(&k.to_string()).unwrap(), k.clone());
                    }
                }
            }

            #[test]
            fn remote_for_and_local_for_agree(names in proptest::collection::vec("[a-z0-9]{1,8}", 0..5)) {
                let pair = FolderPair::new(RemotePath::parse("/Base").unwrap(), PathBuf::from("loc"));
                let mut remote = pair.remote.clone();
                for n in &names { remote = remote.join(n).unwrap(); }
                let local = pair.local_for(&remote).unwrap();
                prop_assert!(local.starts_with("loc"));
                prop_assert_eq!(pair.remote_for(&local), Some(remote));
            }
        }
    }
}
