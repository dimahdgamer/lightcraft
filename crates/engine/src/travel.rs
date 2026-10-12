//! Travel packs: take a part of a library on a Nextcloud, so it can be edited on a laptop that does
//! not have the originals. See `docs/nextcloud.md`.
//!
//! A pack is a server folder `<base>/<name>/`:
//!
//! ```text
//! manifest.json            what each photo is (a [`Manifest`])
//! previews/<xx>/<hash>.lcsp   the smart previews, named as the library names them (by content)
//! photos/<path>/IMG.xmp    the sidecars, laid out like the folders the originals are in
//! ```
//!
//! The photo folder of the library and a folder on the laptop are then two ends of one ordinary
//! folder pair on `photos/` (see `lightcraft_nextcloud::sync`): edits made on either side travel as
//! sidecars. The laptop keeps no original: its photos point at a file that is not there and are
//! edited from their smart previews, as photos on an unplugged drive are.
//!
//! Everything read from a pack is untrusted: the manifest is validated entry by entry (a bad entry
//! is skipped and counted, never trusted), every path in it goes through
//! [`lightcraft_nextcloud::RemotePath`], and sizes and counts are capped.

use std::path::{Component, Path, PathBuf};

use lightcraft_catalog::{MediaKind, Meta, Op, Photo, PhotoId, Source};
use lightcraft_nextcloud::RemotePath;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Session;

/// The file name of a pack's manifest.
pub const MANIFEST: &str = "manifest.json";
/// The format version this build writes and reads.
pub const VERSION: u32 = 1;
/// Most photos one pack may hold.
pub const MAX_PHOTOS: usize = 200_000;
/// Largest manifest read, in bytes.
pub const MAX_MANIFEST_BYTES: usize = 256 << 20;

/// What the library knows of one photo, enough to make an offline record of it elsewhere. The
/// edits themselves travel in the sidecar.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackPhoto {
    /// Where the original is, below the pack's root, with `/` between the names.
    pub path: String,
    pub kind: MediaKind,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub captured: Option<String>,
    pub meta: Meta,
    pub as_shot_wb: Option<(f64, f64)>,
    /// The hash of the original's bytes: it names the smart preview, so both machines find the same file.
    pub content_hash: String,
    pub embedded_lens: Option<lightcraft_develop::EmbeddedLens>,
    pub preview_only: Option<String>,
}

/// A pack's manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub version: u32,
    pub name: String,
    /// When it was made (ISO 8601, the publisher's clock).
    pub created: String,
    pub photos: Vec<PackPhoto>,
}

/// The name of a smart preview of `hash`, the way the library files it.
pub fn preview_file(hash: &str) -> Option<(String, String)> {
    let mut p = Photo::new(PhotoId(0), Source::File { path: String::new() }, "x", "x", 1, 1, "");
    p.content_hash = Some(hash.to_string());
    let name = crate::smart::file_name(&p);
    let shard = name.get(..2)?.to_string();
    Some((shard, name))
}

/// A hash that is safe to put in a file name and an address.
fn valid_hash(h: &str) -> bool {
    !h.is_empty() && h.len() <= 128 && h.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl PackPhoto {
    /// Whether the entry can be trusted: a path of checked names in its exact form, sane sizes, a
    /// usable hash.
    fn valid(&self) -> bool {
        let exact = RemotePath::parse(&self.path).is_ok_and(|p| !p.is_root() && p.to_string().trim_start_matches('/') == self.path);
        exact
            && valid_hash(&self.content_hash)
            && self.width > 0
            && self.height > 0
            && self.width <= 1_000_000
            && self.height <= 1_000_000
            && self.format.len() <= 32
    }

    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

impl Manifest {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Read a manifest. The number is how many entries were skipped as unusable or repeated.
    pub fn from_json(text: &str) -> Result<(Manifest, usize), String> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err("the pack's manifest is too large".into());
        }
        let mut m: Manifest = serde_json::from_str(text).map_err(|e| format!("the pack's manifest is unreadable: {e}"))?;
        if m.version > VERSION {
            return Err(format!("the pack was made by a newer LightCraft (format {}); this one reads format {VERSION}", m.version));
        }
        if m.photos.len() > MAX_PHOTOS {
            return Err(format!("the pack lists more than {MAX_PHOTOS} photos"));
        }
        let before = m.photos.len();
        let mut seen = std::collections::HashSet::new();
        m.photos.retain(|p| p.valid() && seen.insert(p.path.to_lowercase()));
        let skipped = before - m.photos.len();
        Ok((m, skipped))
    }
}

/// A pack name is one folder name on the server.
pub fn valid_pack_name(name: &str) -> bool {
    RemotePath::root().join(name).is_ok()
}

/// What [`collect`] found.
#[derive(Debug, Default)]
pub struct Collected {
    pub photos: Vec<PackPhoto>,
    /// Each smart preview to send: the file here and its place in the pack (`<xx>/<name>`).
    pub previews: Vec<(PathBuf, String)>,
    /// The photos in the pack that have no smart preview yet.
    pub missing_previews: Vec<PhotoId>,
    /// The photos in the pack (their sidecars are written for it, so the laptop gets their edits).
    pub ids: Vec<PhotoId>,
    /// Photos left out, and why.
    pub skipped: Vec<(String, &'static str)>,
}

fn norm(p: &Path) -> String {
    let t = p.to_string_lossy().replace('\\', "/");
    if cfg!(windows) { t.to_lowercase() } else { t }
}

/// `path` below `root`, as `/`-separated names, or `None` if it is not inside.
fn relative(path: &Path, root: &Path) -> Option<String> {
    let rest = path.strip_prefix(root).ok().map(Path::to_path_buf).or_else(|| {
        if !cfg!(windows) {
            return None;
        }
        let mut c = path.components();
        for r in root.components() {
            if !c.next()?.as_os_str().to_string_lossy().eq_ignore_ascii_case(&r.as_os_str().to_string_lossy()) {
                return None;
            }
        }
        Some(c.as_path().to_path_buf())
    })?;
    let names: Option<Vec<&str>> = rest.components().map(|c| if let Component::Normal(n) = c { n.to_str() } else { None }).collect();
    let names = names?;
    (!names.is_empty()).then(|| names.join("/"))
}

/// The deepest folder that holds every one of `paths`' files (`None` when they share none).
pub fn common_root(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut dirs = paths.iter().filter_map(|p| p.parent());
    let first: Vec<Component<'_>> = dirs.next()?.components().collect();
    let mut keep = first.len();
    for d in dirs {
        let c: Vec<Component<'_>> = d.components().collect();
        keep = keep.min(first.iter().zip(&c).take_while(|(a, b)| norm(Path::new(a.as_os_str())) == norm(Path::new(b.as_os_str()))).count());
    }
    let root: PathBuf = first.iter().take(keep).map(|c| c.as_os_str()).collect();
    // a bare drive or the file system's root is not a folder worth pairing
    (root.components().filter(|c| matches!(c, Component::Normal(_))).count() > 0).then_some(root)
}

/// The library's photos `ids` as pack entries below `root`.
pub fn collect(s: &Session, ids: &[PhotoId], root: &Path) -> Result<Collected, String> {
    let smart_dir = s.media.smart_dir.clone().ok_or("a travel pack needs a library on disk (it keeps the smart previews)")?;
    let mut out = Collected::default();
    for id in ids {
        let Some(p) = s.catalog.photo(*id) else { continue };
        let skip = |out: &mut Collected, why: &'static str| out.skipped.push((p.file_name.clone(), why));
        if p.deleted {
            skip(&mut out, "in Recently Deleted");
            continue;
        }
        if p.copy_of.is_some() {
            skip(&mut out, "a virtual copy (its edits live only in the library)");
            continue;
        }
        let Source::File { path } = &p.source else {
            skip(&mut out, "not a file");
            continue;
        };
        let Some(hash) = p.content_hash.clone().filter(|h| valid_hash(h)) else {
            skip(&mut out, "its content has no hash yet");
            continue;
        };
        let Some(rel) = relative(Path::new(path), root) else {
            skip(&mut out, "outside the pack's folder");
            continue;
        };
        let entry = PackPhoto {
            path: rel,
            kind: p.kind,
            format: p.format.clone(),
            width: p.width,
            height: p.height,
            file_size: p.file_size,
            captured: p.captured.clone(),
            meta: p.meta.clone(),
            as_shot_wb: p.as_shot_wb,
            content_hash: hash,
            embedded_lens: p.embedded_lens,
            preview_only: p.preview_only.clone(),
        };
        if !entry.valid() {
            skip(&mut out, "its name or size can't go in a pack");
            continue;
        }
        // the preview's place in the pack is its name by content, the same on every machine
        let local = crate::smart::file_path(&smart_dir, p);
        match preview_file(&entry.content_hash) {
            Some((shard, name)) if crate::smart::is_valid(&local) => out.previews.push((local, format!("{shard}/{name}"))),
            _ => out.missing_previews.push(*id),
        }
        out.ids.push(*id);
        out.photos.push(entry);
    }
    Ok(out)
}

/// What opening a pack did to the catalog.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Added {
    pub added: usize,
    /// Photos the library has already (same content, or a record at that very path).
    pub existing: usize,
}

/// Make an offline record of each photo of `manifest` that the library does not have: its original
/// is expected at `photos_root/<path>` (not there on this machine), its smart preview is already in
/// the smart previews folder, its edits come with the sidecar. One undo step. Adding a photo does not
/// write a sidecar (only changing one does), so nothing here touches the pack's sidecars; reading them
/// happens next, without writing them back (see `Session::nextcloud_apply_pending`).
pub fn add_records(s: &mut Session, manifest: &Manifest, photos_root: &Path) -> Result<Added, String> {
    let now = (s.clock)();
    let have_hash: std::collections::HashSet<String> = s.catalog.photos().filter(|p| !p.deleted).filter_map(|p| p.content_hash.clone()).collect();
    let have_path: std::collections::HashSet<String> = s
        .catalog
        .photos()
        .filter(|p| !p.deleted)
        .filter_map(|p| if let Source::File { path } = &p.source { Some(norm(Path::new(path))) } else { None })
        .collect();
    let mut added = Added::default();
    let mut ops = Vec::new();
    for e in &manifest.photos {
        let Ok(rel) = RemotePath::parse(&e.path) else { continue };
        let dest = rel.to_local(photos_root);
        let dest_text = dest.to_string_lossy().to_string();
        if have_hash.contains(&e.content_hash) || have_path.contains(&norm(&dest)) {
            added.existing += 1;
            continue;
        }
        // the sidecar of this photo is written (by auto-write, or after an edit) next to it
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir).map_err(|err| format!("can't create {}: {err}", dir.display()))?;
        }
        let id = s.catalog.alloc_photo_id();
        let mut p = Photo::new(id, Source::File { path: dest_text }, e.name(), &e.format, e.width, e.height, &now);
        p.kind = e.kind;
        p.file_size = e.file_size;
        p.captured = e.captured.clone();
        p.meta = e.meta.clone();
        p.as_shot_wb = e.as_shot_wb;
        p.content_hash = Some(e.content_hash.clone());
        p.embedded_lens = e.embedded_lens;
        p.preview_only = e.preview_only.clone();
        ops.push(Op::AddPhoto { photo: Box::new(p) });
        added.added += 1;
    }
    if !ops.is_empty() {
        s.commit("Open Travel Pack", Op::Batch { ops }).map_err(|e| e.to_string())?;
    }
    Ok(added)
}

/// A summary of [`Added`] for a reply.
pub fn added_json(a: &Added) -> Value {
    json!({"added": a.added, "existing": a.existing})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> PackPhoto {
        PackPhoto {
            path: path.into(),
            kind: MediaKind::Raw,
            format: "CR2".into(),
            width: 6000,
            height: 4000,
            file_size: 25_000_000,
            captured: Some("2026-10-01T10:00:00".into()),
            meta: Meta::default(),
            as_shot_wb: Some((5200.0, 4.0)),
            content_hash: "0123456789abcdef0123456789abcdef".into(),
            embedded_lens: None,
            preview_only: None,
        }
    }

    fn manifest(photos: Vec<PackPhoto>) -> Manifest {
        Manifest { version: VERSION, name: "Trip".into(), created: "2026-10-12T09:00:00".into(), photos }
    }

    #[test]
    fn a_manifest_round_trips() {
        let m = manifest(vec![entry("2026/Italy/IMG_0001.CR2"), entry("IMG_0002.CR2")]);
        let (back, skipped) = Manifest::from_json(&m.to_json()).unwrap();
        assert_eq!((back, skipped), (m, 0));
    }

    #[test]
    fn unusable_entries_are_skipped_and_counted_not_trusted() {
        let mut bad_hash = entry("a/b.CR2");
        bad_hash.content_hash = "../../etc".into();
        let mut zero = entry("c.CR2");
        zero.width = 0;
        let mut huge = entry("d.CR2");
        huge.height = 2_000_000;
        let m = manifest(vec![
            entry("ok/one.CR2"),
            entry("../escape.CR2"),
            entry("/absolute.CR2"),
            entry("a//b.CR2"),
            entry("C:/drive.CR2"),
            entry("con/x.CR2"),
            entry("trailing/dot."),
            entry(""),
            bad_hash,
            zero,
            huge,
            entry("ok/ONE.cr2"),
            entry("fine/two.CR2"),
        ]);
        let (back, skipped) = Manifest::from_json(&m.to_json()).unwrap();
        let kept: Vec<&str> = back.photos.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(kept, ["ok/one.CR2", "fine/two.CR2"], "the second spelling of one path is a repeat");
        assert_eq!(skipped, 11);
    }

    #[test]
    fn a_damaged_or_foreign_manifest_is_an_error() {
        assert!(Manifest::from_json("").is_err());
        assert!(Manifest::from_json("{}").is_err());
        assert!(Manifest::from_json("[1]").is_err());
        let newer = Manifest { version: VERSION + 1, ..manifest(vec![]) };
        assert!(Manifest::from_json(&newer.to_json()).unwrap_err().contains("newer"));
        assert!(Manifest::from_json(&" ".repeat(MAX_MANIFEST_BYTES + 1)).is_err());
    }

    #[test]
    fn pack_names_are_one_checked_folder_name() {
        for ok in ["Trip", "Trip 2026", "Reise Italien", "a.b"] {
            assert!(valid_pack_name(ok), "{ok}");
        }
        for bad in ["", "a/b", "..", ".", "con", "x:y", "trailing.", "a\\b", &"x".repeat(300)] {
            assert!(!valid_pack_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_preview_name_is_the_librarys_own() {
        let (shard, name) = preview_file("0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(shard.len(), 2);
        assert!(name.starts_with(&shard) && name.ends_with(".lcsp"), "{name}");
        // the same hash gives the same name on any machine
        assert_eq!(preview_file("0123456789abcdef0123456789abcdef").unwrap(), (shard, name));
        assert_ne!(preview_file("fedcba9876543210fedcba9876543210").unwrap().1, preview_file("0123456789abcdef0123456789abcdef").unwrap().1);
    }

    #[test]
    fn the_common_root_is_the_deepest_shared_folder() {
        let p = |s: &str| PathBuf::from(s);
        let root = common_root(&[p("/photos/2026/Italy/a.CR2"), p("/photos/2026/Italy/rome/b.CR2"), p("/photos/2026/Italy/c.CR2")]);
        assert_eq!(root, Some(p("/photos/2026/Italy")));
        assert_eq!(common_root(&[p("/photos/2026/a.CR2"), p("/photos/2025/b.CR2")]), Some(p("/photos")));
        assert_eq!(common_root(&[p("/a/x.CR2")]), Some(p("/a")));
        assert_eq!(common_root(&[]), None);
        // nothing in common but the file system's root
        assert_eq!(common_root(&[p("/a/x.CR2"), p("/b/y.CR2")]), None);
    }

    #[test]
    fn relative_paths_stay_inside_their_root() {
        let root = Path::new("photos").join("2026");
        assert_eq!(relative(&root.join("Italy").join("a.CR2"), &root), Some("Italy/a.CR2".into()));
        assert_eq!(relative(&root.join("a.CR2"), &root), Some("a.CR2".into()));
        assert_eq!(relative(Path::new("elsewhere").join("a.CR2").as_path(), &root), None);
        assert_eq!(relative(&root, &root), None, "the root itself is not a photo");
        assert_eq!(relative(&Path::new("photos").join("20261").join("a.CR2"), &root), None, "a sibling that shares a name prefix");
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn from_json_never_panics(s in "\\PC{0,300}") {
                let _ = Manifest::from_json(&s);
            }

            #[test]
            fn every_entry_that_survives_has_a_checked_path(path in "\\PC{0,40}") {
                let m = manifest(vec![entry(&path)]);
                let (back, _) = Manifest::from_json(&m.to_json()).unwrap();
                for p in &back.photos {
                    let rp = RemotePath::parse(&p.path).unwrap();
                    prop_assert!(!rp.is_root());
                    let shown = rp.to_string();
                    prop_assert_eq!(shown.trim_start_matches('/'), p.path.as_str());
                }
            }
        }
    }
}
