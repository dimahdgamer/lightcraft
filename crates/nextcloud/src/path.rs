//! Paths inside a Nextcloud account's files.
//!
//! Everything a server sends is hostile input: a name that climbs out of the folder it is saved
//! in (`..`), an absolute path, a Windows device name (`NUL`, `COM1`), a name too long for the
//! disk, a path nested without end. A [`RemotePath`] can only be built from names that pass
//! [`check_name`], so code that saves a file under a [`RemotePath::to_local`] path cannot leave
//! its target folder, and the same name is valid on Windows, macOS and Linux.

use std::fmt;
use std::path::{Path, PathBuf};

/// Longest name of one file or folder (the common limit of file systems and Nextcloud).
pub const MAX_NAME_BYTES: usize = 255;
/// Longest whole path, in bytes of the names and the `/` between them.
pub const MAX_PATH_BYTES: usize = 4096;
/// Most folder levels.
pub const MAX_DEPTH: usize = 64;

/// Why a name or path was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// One name, with the reason.
    BadName { name: String, why: &'static str },
    /// More than [`MAX_DEPTH`] levels.
    TooDeep,
    /// More than [`MAX_PATH_BYTES`] bytes.
    TooLong,
    /// A `%` escape that is not valid UTF-8 or not complete.
    BadEncoding(String),
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::BadName { name, why } => write!(f, "unusable name `{name}`: {why}"),
            PathError::TooDeep => write!(f, "folders nested deeper than {MAX_DEPTH} levels"),
            PathError::TooLong => write!(f, "path longer than {MAX_PATH_BYTES} bytes"),
            PathError::BadEncoding(s) => write!(f, "bad % escape in `{s}`"),
        }
    }
}

impl std::error::Error for PathError {}

/// A name cut to a length a message can carry (at a character boundary).
fn shorten(s: &str) -> String {
    s.chars().take(40).collect()
}

/// Whether `name` is a Windows device name (`CON`, `NUL.txt`, `com1`…), which no file may have
/// on Windows, with or without an extension.
fn is_reserved_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    let numbered = upper.strip_prefix("COM").or_else(|| upper.strip_prefix("LPT"));
    numbered.is_some_and(|n| n.len() == 1 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Check one file or folder name (not a path).
pub fn check_name(name: &str) -> Result<(), PathError> {
    let bad = |why: &'static str| Err(PathError::BadName { name: shorten(name), why });
    if name.is_empty() {
        return bad("empty");
    }
    if name.len() > MAX_NAME_BYTES {
        return bad("longer than 255 bytes");
    }
    if name == "." || name == ".." {
        return bad("`.` and `..` are not file names");
    }
    if name.chars().any(char::is_control) {
        return bad("contains a control character");
    }
    if name.chars().any(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return bad("contains a character that is not allowed in file names");
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return bad("ends with a space or a dot");
    }
    if is_reserved_device(name) {
        return bad("is a reserved Windows device name");
    }
    Ok(())
}

/// A folder or file inside the account's files, as checked names from the root. The root itself
/// is the path without names.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemotePath {
    names: Vec<String>,
}

impl RemotePath {
    /// The account's top folder.
    pub fn root() -> RemotePath {
        RemotePath::default()
    }

    /// Parse `/Photos/2026/IMG_1.CR2`. A leading, trailing or doubled `/` is ignored; any name
    /// that fails [`check_name`] (this includes `..`) refuses the whole path.
    pub fn parse(s: &str) -> Result<RemotePath, PathError> {
        if s.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        let mut path = RemotePath::root();
        for name in s.split('/').filter(|n| !n.is_empty()) {
            path = path.join(name)?;
        }
        Ok(path)
    }

    /// This path with one more name.
    pub fn join(&self, name: &str) -> Result<RemotePath, PathError> {
        check_name(name)?;
        if self.names.len() >= MAX_DEPTH {
            return Err(PathError::TooDeep);
        }
        if self.byte_len().saturating_add(1).saturating_add(name.len()) > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        let mut names = self.names.clone();
        names.push(name.to_string());
        Ok(RemotePath { names })
    }

    fn byte_len(&self) -> usize {
        self.names.iter().map(|n| n.len().saturating_add(1)).sum()
    }

    pub fn is_root(&self) -> bool {
        self.names.is_empty()
    }

    /// The names from the root.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The last name (`None` for the root).
    pub fn file_name(&self) -> Option<&str> {
        self.names.last().map(String::as_str)
    }

    /// The folder this path is in (`None` for the root).
    pub fn parent(&self) -> Option<RemotePath> {
        let (_, rest) = self.names.split_last()?;
        Some(RemotePath { names: rest.to_vec() })
    }

    /// Whether `base` is this path or a folder above it.
    pub fn starts_with(&self, base: &RemotePath) -> bool {
        self.names.starts_with(&base.names)
    }

    /// The part below `base` (`None` if this path is not inside it).
    pub fn strip_prefix(&self, base: &RemotePath) -> Option<RemotePath> {
        self.names.strip_prefix(base.names.as_slice()).map(|rest| RemotePath { names: rest.to_vec() })
    }

    /// The same path below `base`, for example a remote folder mirrored into a local one.
    pub fn to_local(&self, base: &Path) -> PathBuf {
        let mut p = base.to_path_buf();
        for n in &self.names {
            // names are checked: no separators, no `..`, no drive letters, no device names
            p.push(n);
        }
        p
    }

    /// The names joined with `/`, each percent-encoded, no leading `/`: what goes after the
    /// account's root in a request URL.
    pub fn url_part(&self) -> String {
        self.names.iter().map(|n| encode_segment(n)).collect::<Vec<_>>().join("/")
    }
}

impl fmt::Display for RemotePath {
    /// `/Photos/2026`, or `/` for the root.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.names.is_empty() {
            return f.write_str("/");
        }
        for n in &self.names {
            write!(f, "/{n}")?;
        }
        Ok(())
    }
}

/// Percent-encode one path segment: everything except the unreserved characters
/// `A-Z a-z 0-9 - . _ ~` becomes `%XX` over its UTF-8 bytes.
pub fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b >> 4)]));
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b & 15)]));
        }
    }
    out
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Undo [`encode_segment`] (and any other valid percent-encoding): `%XX` bytes must form valid
/// UTF-8. A `%` not followed by two hex digits is an error, not kept as it is.
pub fn decode_segment(s: &str) -> Result<String, PathError> {
    let bad = || PathError::BadEncoding(shorten(s));
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hi = bytes.get(i + 1).copied().and_then(hex_value).ok_or_else(bad)?;
            let lo = bytes.get(i + 2).copied().and_then(hex_value).ok_or_else(bad)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalises() {
        let p = RemotePath::parse("//Photos//2026/IMG_1.CR2/").unwrap();
        assert_eq!(p.names(), ["Photos", "2026", "IMG_1.CR2"]);
        assert_eq!(p.to_string(), "/Photos/2026/IMG_1.CR2");
        assert_eq!(p.file_name(), Some("IMG_1.CR2"));
        assert_eq!(p.parent().unwrap().to_string(), "/Photos/2026");
        assert!(RemotePath::parse("").unwrap().is_root());
        assert_eq!(RemotePath::root().to_string(), "/");
        assert_eq!(RemotePath::root().parent(), None);
    }

    #[test]
    fn hostile_names_are_refused() {
        for bad in [
            "../etc/passwd",
            "a/../b",
            "a/./b",
            "a\\b",
            "C:/x",
            "a/b:stream",
            "con",
            "a/NUL.txt",
            "a/com1.log",
            "LPT9",
            "a/trailing dot.",
            "a/trailing space ",
            "a/bell\u{7}",
            "a/new\nline",
            "a/<x>",
            "a/q?",
            "a/pi|pe",
            "a/\u{85}next-line",
        ] {
            assert!(RemotePath::parse(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(RemotePath::parse(&format!("a/{}", "x".repeat(256))).is_err());
        assert!(RemotePath::parse(&format!("a/{}", "x".repeat(255))).is_ok());
    }

    #[test]
    fn ordinary_names_pass_including_unicode_and_lookalikes_of_devices() {
        for good in ["Photos/Urlaub 2026/Möwe.CR2", "写真/東京.jpg", "console", "a/com10", "a/lpt", "a/.hidden", "a/IMG (1).jpg", "a/100%"] {
            assert!(RemotePath::parse(good).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn depth_and_length_are_bounded() {
        let deep = vec!["d"; MAX_DEPTH + 1].join("/");
        assert_eq!(RemotePath::parse(&deep), Err(PathError::TooDeep));
        assert!(RemotePath::parse(&vec!["d"; MAX_DEPTH].join("/")).is_ok());
        assert_eq!(RemotePath::parse(&"x/".repeat(MAX_PATH_BYTES)), Err(PathError::TooLong));
        // a path that fits the limit name by name but not as a whole
        let mut p = RemotePath::root();
        let mut result = Ok(RemotePath::root());
        for _ in 0..40 {
            result = p.join(&"n".repeat(200));
            match &result {
                Ok(next) => p = next.clone(),
                Err(_) => break,
            }
        }
        assert!(matches!(result, Err(PathError::TooLong | PathError::TooDeep)));
    }

    #[test]
    fn relations_between_paths() {
        let base = RemotePath::parse("/Photos").unwrap();
        let file = RemotePath::parse("/Photos/2026/a.jpg").unwrap();
        assert!(file.starts_with(&base));
        assert!(!base.starts_with(&file));
        assert!(file.starts_with(&file));
        assert!(file.starts_with(&RemotePath::root()));
        assert_eq!(file.strip_prefix(&base).unwrap().to_string(), "/2026/a.jpg");
        assert_eq!(base.strip_prefix(&file), None);
        // a sibling that only shares a name prefix is not inside
        assert!(!RemotePath::parse("/Photos2/a").unwrap().starts_with(&base));
    }

    #[test]
    fn local_paths_stay_below_their_base() {
        let base = Path::new("cache").join("dima");
        let p = RemotePath::parse("/Photos/2026/a b.jpg").unwrap().to_local(&base);
        assert!(p.starts_with(&base));
        assert_eq!(p, base.join("Photos").join("2026").join("a b.jpg"));
        assert_eq!(RemotePath::root().to_local(&base), base);
    }

    #[test]
    fn percent_encoding_round_trips() {
        assert_eq!(encode_segment("a b/ü%~.-_"), "a%20b%2F%C3%BC%25~.-_");
        for s in ["a b", "ü", "写真", "100%", "a/b", "~tilde", "emoji 📷", ""] {
            assert_eq!(decode_segment(&encode_segment(s)).unwrap(), s, "{s:?}");
        }
        assert_eq!(RemotePath::parse("/a b/ü.jpg").unwrap().url_part(), "a%20b/%C3%BC.jpg");
        assert_eq!(RemotePath::root().url_part(), "");
    }

    #[test]
    fn bad_escapes_are_errors() {
        for bad in ["%", "%4", "%zz", "a%2", "%C3", "%FF%FE", "%E4%B8"] {
            assert!(matches!(decode_segment(bad), Err(PathError::BadEncoding(_))), "{bad:?}");
        }
        // a decoded NUL or `/` does not become a valid name
        assert!(RemotePath::parse(&decode_segment("a%00b").unwrap()).is_err());
        assert_eq!(RemotePath::parse(&decode_segment("a%2Fb").unwrap()).unwrap().names().len(), 2);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn parse_never_panics_and_only_yields_checked_names(s in "\\PC{0,200}") {
                if let Ok(p) = RemotePath::parse(&s) {
                    for n in p.names() {
                        prop_assert!(check_name(n).is_ok());
                    }
                    prop_assert_eq!(RemotePath::parse(&p.to_string()).unwrap(), p);
                }
            }

            #[test]
            fn encode_then_decode_is_identity(s in "\\PC{0,60}") {
                prop_assert_eq!(decode_segment(&encode_segment(&s)).unwrap(), s);
            }

            #[test]
            fn decode_never_panics(s in "[ -~%]{0,60}") {
                let _ = decode_segment(&s);
            }
        }
    }
}
