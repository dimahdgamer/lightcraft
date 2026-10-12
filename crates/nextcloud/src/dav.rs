//! WebDAV listings: the request that asks for one and the parser for the answer.
//!
//! The answer to `PROPFIND` is a `207 Multi-Status` XML document with one `response` per file or
//! folder. It comes from a server over the network, so it is parsed as hostile input: namespaces
//! are resolved by URI (a server may write `d:`, `D:` or `ns0:`), only properties the server
//! reported with a 2xx status count, and an address outside the account's files, a name that
//! fails [`check_name`](crate::path::check_name) or a duplicate is skipped and reported without
//! losing the rest of the listing. Size, nesting and entry counts are capped.

use std::collections::HashSet;
use std::fmt;

use quick_xml::NsReader;
use quick_xml::events::{BytesRef, Event};
use quick_xml::name::{Namespace, ResolveResult};

use crate::path::{PathError, RemotePath, decode_segment, encode_segment};

/// Largest answer parsed (the caller caps what it downloads too).
pub const MAX_XML_BYTES: usize = 64 << 20;
/// Most entries one listing may hold.
pub const MAX_ENTRIES: usize = 100_000;
/// Deepest element nesting accepted (a real answer is about 6 deep).
const MAX_DEPTH: usize = 24;
/// Longest text of one value (an address, an ETag).
const MAX_TEXT: usize = 8 * 1024;
/// Skipped entries kept for the report (all are counted).
const MAX_SKIPPED_KEPT: usize = 50;

const NS_DAV: &[u8] = b"DAV:";
const NS_OC: &[u8] = b"http://owncloud.org/ns";

/// The properties asked for: what [`Entry`] holds.
pub const PROPFIND_BODY: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop>"#,
    "<d:getlastmodified/><d:getcontentlength/><d:getcontenttype/><d:getetag/><d:resourcetype/>",
    "<oc:fileid/><oc:size/><oc:permissions/>",
    "</d:prop></d:propfind>"
);

/// How deep a `PROPFIND` looks. Never ask for infinity: the server may refuse it, and on a small
/// machine it would walk the whole account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// The folder or file itself.
    Zero,
    /// Also what is directly inside a folder.
    One,
}

/// The headers of a `PROPFIND` for `depth` (the body is [`PROPFIND_BODY`]).
pub fn propfind_headers(depth: Depth) -> [(&'static str, &'static str); 2] {
    [("Depth", if depth == Depth::Zero { "0" } else { "1" }), ("Content-Type", "application/xml; charset=utf-8")]
}

/// Why an answer could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DavError {
    /// Not well-formed XML, or not a multistatus answer.
    Xml(String),
    /// More than [`MAX_XML_BYTES`].
    TooLarge,
    /// More than [`MAX_ENTRIES`] entries.
    TooMany,
    /// Nested or long beyond what a listing needs.
    TooComplex(&'static str),
}

impl fmt::Display for DavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DavError::Xml(e) => write!(f, "unreadable answer from the server: {e}"),
            DavError::TooLarge => write!(f, "the server's answer is larger than {} MiB", MAX_XML_BYTES >> 20),
            DavError::TooMany => write!(f, "the folder has more than {MAX_ENTRIES} entries"),
            DavError::TooComplex(why) => write!(f, "the server's answer is too complex: {why}"),
        }
    }
}

impl std::error::Error for DavError {}

fn shorten(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// A path or user name used as a fixed part of the account's address: no control characters, no
/// separators, not `.` or `..`.
fn check_url_segment(s: &str) -> Result<(), PathError> {
    let bad = |why: &'static str| Err(PathError::BadName { name: shorten(s, 40), why });
    if s.is_empty() || s == "." || s == ".." {
        return bad("empty, `.` or `..`");
    }
    if s.len() > crate::path::MAX_NAME_BYTES || s.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | '?' | '#' | '%')) {
        return bad("too long or contains a control character, a separator, `?`, `#` or `%`");
    }
    Ok(())
}

/// Where an account's files are on the server: `/remote.php/dav/files/<user>`, below the
/// server's own path when Nextcloud is installed in a subfolder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DavRoot {
    prefix: Vec<String>,
}

impl DavRoot {
    /// `server_path` is the path of the server's address (`""` for `https://cloud.example`,
    /// `/nextcloud` when it is installed in a subfolder).
    pub fn files(server_path: &str, user: &str) -> Result<DavRoot, PathError> {
        let mut prefix = Vec::new();
        for seg in server_path.split('/').filter(|s| !s.is_empty()) {
            check_url_segment(seg)?;
            prefix.push(seg.to_string());
        }
        prefix.extend(["remote.php", "dav", "files"].map(String::from));
        check_url_segment(user)?;
        prefix.push(user.to_string());
        Ok(DavRoot { prefix })
    }

    /// The address for `path` as it goes in a request line: `/remote.php/dav/files/dima/Photos/`.
    /// A folder (`collection`) gets the closing `/`.
    pub fn url_path(&self, path: &RemotePath, collection: bool) -> String {
        let mut s = String::new();
        for seg in self.prefix.iter().chain(path.names()) {
            s.push('/');
            s.push_str(&encode_segment(seg));
        }
        if collection || s.is_empty() {
            s.push('/');
        }
        s
    }

    /// The path inside the account that a server's `href` names. The error says why the address
    /// can't be used; an address outside this account's files always is.
    pub fn href_to_path(&self, href: &str) -> Result<RemotePath, String> {
        let href = href.trim();
        let after_scheme = href.strip_prefix("https://").or_else(|| href.strip_prefix("http://"));
        let path = match after_scheme {
            Some(rest) => rest.find('/').and_then(|i| rest.get(i..)).unwrap_or("/"),
            None => href,
        };
        if !path.starts_with('/') {
            return Err("a relative address".into());
        }
        if path.contains(['?', '#']) {
            return Err("an address with a query or fragment".into());
        }
        let mut segments = path.split('/').filter(|s| !s.is_empty());
        for expected in &self.prefix {
            let seg = segments.next().ok_or("outside the account's files")?;
            let seg = decode_segment(seg).map_err(|e| e.to_string())?;
            if &seg != expected {
                return Err("outside the account's files".into());
            }
        }
        let mut out = RemotePath::root();
        for seg in segments {
            let name = decode_segment(seg).map_err(|e| e.to_string())?;
            out = out.join(&name).map_err(|e| e.to_string())?;
        }
        Ok(out)
    }
}

/// One file or folder of a listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: RemotePath,
    pub is_dir: bool,
    /// Bytes (for a folder, what the server reports for its contents, else 0).
    pub size: u64,
    /// Last change, seconds since 1970-01-01 UTC.
    pub modified: Option<i64>,
    /// As the server wrote it, quotes included: send it back unchanged in `If-Match`.
    pub etag: Option<String>,
    pub file_id: Option<String>,
    pub content_type: Option<String>,
    /// Nextcloud's permission letters (`RGDNVW` for a file you may change).
    pub permissions: Option<String>,
}

impl Entry {
    /// Whether the server allows changing a file here or adding to a folder. Unknown counts as
    /// allowed (the server says no with a 403 if not).
    pub fn can_write(&self) -> bool {
        match &self.permissions {
            None => true,
            Some(p) if self.is_dir => p.contains(['C', 'K']),
            Some(p) => p.contains('W'),
        }
    }
}

/// An entry that could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub href: String,
    pub why: String,
}

/// The usable entries of an answer, and a report of the others.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// The first few entries skipped.
    pub skipped: Vec<Skipped>,
    /// How many entries were skipped in all.
    pub skipped_total: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    Root,
    Multistatus,
    Response,
    Href,
    Propstat,
    Prop,
    Status,
    LastModified,
    ContentLength,
    ContentType,
    Etag,
    ResourceType,
    Collection,
    FileId,
    OcSize,
    Permissions,
    Other,
}

impl Tag {
    /// The tag of a child element called `local` in namespace `ns`, below this one.
    fn child(self, ns: &ResolveResult<'_>, local: &[u8]) -> Tag {
        let in_ns = |want: &[u8]| matches!(ns, ResolveResult::Bound(Namespace(n)) if *n == want);
        let (dav, oc) = (in_ns(NS_DAV), in_ns(NS_OC));
        match (self, dav, oc, local) {
            (Tag::Root, true, _, b"multistatus") => Tag::Multistatus,
            (Tag::Multistatus, true, _, b"response") => Tag::Response,
            (Tag::Response, true, _, b"href") => Tag::Href,
            (Tag::Response, true, _, b"propstat") => Tag::Propstat,
            (Tag::Propstat, true, _, b"prop") => Tag::Prop,
            (Tag::Propstat, true, _, b"status") => Tag::Status,
            (Tag::Prop, true, _, b"getlastmodified") => Tag::LastModified,
            (Tag::Prop, true, _, b"getcontentlength") => Tag::ContentLength,
            (Tag::Prop, true, _, b"getcontenttype") => Tag::ContentType,
            (Tag::Prop, true, _, b"getetag") => Tag::Etag,
            (Tag::Prop, true, _, b"resourcetype") => Tag::ResourceType,
            (Tag::ResourceType, true, _, b"collection") => Tag::Collection,
            (Tag::Prop, _, true, b"fileid") => Tag::FileId,
            (Tag::Prop, _, true, b"size") => Tag::OcSize,
            (Tag::Prop, _, true, b"permissions") => Tag::Permissions,
            _ => Tag::Other,
        }
    }

    /// Whether the text inside the element is a value to keep.
    fn has_text(self) -> bool {
        matches!(
            self,
            Tag::Href
                | Tag::Status
                | Tag::LastModified
                | Tag::ContentLength
                | Tag::ContentType
                | Tag::Etag
                | Tag::FileId
                | Tag::OcSize
                | Tag::Permissions
        )
    }
}

/// The properties of one `propstat`.
#[derive(Clone, Debug, Default)]
struct Props {
    is_dir: Option<bool>,
    length: Option<u64>,
    oc_size: Option<u64>,
    modified: Option<i64>,
    etag: Option<String>,
    file_id: Option<String>,
    content_type: Option<String>,
    permissions: Option<String>,
}

impl Props {
    /// Take over every property `other` has.
    fn merge(&mut self, other: Props) {
        fn set<T>(into: &mut Option<T>, from: Option<T>) {
            if from.is_some() {
                *into = from;
            }
        }
        set(&mut self.is_dir, other.is_dir);
        set(&mut self.length, other.length);
        set(&mut self.oc_size, other.oc_size);
        set(&mut self.modified, other.modified);
        set(&mut self.etag, other.etag);
        set(&mut self.file_id, other.file_id);
        set(&mut self.content_type, other.content_type);
        set(&mut self.permissions, other.permissions);
    }
}

struct Parser<'a> {
    root: &'a DavRoot,
    stack: Vec<Tag>,
    text: String,
    saw_multistatus: bool,
    href: Option<String>,
    /// Properties of the open response, from its successful propstats.
    cur: Props,
    any_ok: bool,
    /// Properties and status of the open propstat.
    ps: Props,
    ps_ok: Option<bool>,
    seen: HashSet<RemotePath>,
    out: Listing,
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Whether a status line such as `HTTP/1.1 200 OK` reports success.
fn status_ok(line: &str) -> bool {
    line.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok()).is_some_and(|c| (200..300).contains(&c))
}

impl<'a> Parser<'a> {
    fn new(root: &'a DavRoot) -> Self {
        Parser {
            root,
            stack: Vec::new(),
            text: String::new(),
            saw_multistatus: false,
            href: None,
            cur: Props::default(),
            any_ok: false,
            ps: Props::default(),
            ps_ok: None,
            seen: HashSet::new(),
            out: Listing::default(),
        }
    }

    fn current(&self) -> Tag {
        self.stack.last().copied().unwrap_or(Tag::Root)
    }

    fn start(&mut self, tag: Tag) -> Result<(), DavError> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(DavError::TooComplex("elements nested too deeply"));
        }
        match tag {
            Tag::Multistatus => self.saw_multistatus = true,
            Tag::Response => {
                self.href = None;
                self.cur = Props::default();
                self.any_ok = false;
            }
            Tag::Propstat => {
                self.ps = Props::default();
                self.ps_ok = None;
            }
            Tag::Collection => self.ps.is_dir = Some(true),
            _ => {}
        }
        if tag.has_text() {
            self.text.clear();
        }
        self.stack.push(tag);
        Ok(())
    }

    fn add_text(&mut self, t: &str) -> Result<(), DavError> {
        if self.current().has_text() {
            if self.text.len().saturating_add(t.len()) > MAX_TEXT {
                return Err(DavError::TooComplex("a value is too long"));
            }
            self.text.push_str(t);
        }
        Ok(())
    }

    fn end(&mut self) -> Result<(), DavError> {
        let tag = self.stack.pop().unwrap_or(Tag::Other);
        match tag {
            Tag::Href => {
                if self.href.is_none() {
                    self.href = Some(self.text.trim().to_string());
                }
            }
            Tag::Status => self.ps_ok = Some(status_ok(&self.text)),
            Tag::LastModified => self.ps.modified = parse_http_date(&self.text),
            Tag::ContentLength => self.ps.length = self.text.trim().parse().ok(),
            Tag::OcSize => self.ps.oc_size = self.text.trim().parse().ok(),
            Tag::ContentType => self.ps.content_type = non_empty(&self.text),
            Tag::Etag => self.ps.etag = non_empty(&self.text),
            Tag::FileId => self.ps.file_id = non_empty(&self.text),
            Tag::Permissions => self.ps.permissions = non_empty(&self.text),
            // a resourcetype without `collection` is a plain file
            Tag::ResourceType => {
                self.ps.is_dir.get_or_insert(false);
            }
            Tag::Propstat => {
                if self.ps_ok == Some(true) {
                    let ps = std::mem::take(&mut self.ps);
                    self.cur.merge(ps);
                    self.any_ok = true;
                }
            }
            Tag::Response => self.finish_response()?,
            _ => {}
        }
        Ok(())
    }

    fn skip(&mut self, href: &str, why: &str) {
        self.out.skipped_total = self.out.skipped_total.saturating_add(1);
        if self.out.skipped.len() < MAX_SKIPPED_KEPT {
            self.out.skipped.push(Skipped { href: shorten(href, 200), why: why.to_string() });
        }
    }

    fn finish_response(&mut self) -> Result<(), DavError> {
        let Some(href) = self.href.take() else {
            self.skip("", "a response without an address");
            return Ok(());
        };
        if !self.any_ok {
            self.skip(&href, "the server reported no properties for it");
            return Ok(());
        }
        let path = match self.root.href_to_path(&href) {
            Ok(p) => p,
            Err(why) => {
                self.skip(&href, &why);
                return Ok(());
            }
        };
        if !self.seen.insert(path.clone()) {
            self.skip(&href, "listed twice");
            return Ok(());
        }
        if self.out.entries.len() >= MAX_ENTRIES {
            return Err(DavError::TooMany);
        }
        let p = std::mem::take(&mut self.cur);
        let is_dir = p.is_dir.unwrap_or_else(|| href.ends_with('/'));
        let size = if is_dir { p.oc_size.or(p.length) } else { p.length.or(p.oc_size) }.unwrap_or(0);
        self.out.entries.push(Entry {
            path,
            is_dir,
            size,
            modified: p.modified,
            etag: p.etag,
            file_id: p.file_id,
            content_type: p.content_type,
            permissions: p.permissions,
        });
        Ok(())
    }
}

/// `&name;` as the character it stands for: the five predefined entities and numeric references.
/// Anything else (a name the document would have to define) is refused, so a file name is never
/// changed silently.
fn reference(r: &BytesRef<'_>) -> Result<char, DavError> {
    let unknown = || DavError::Xml("an entity reference that is not predefined".into());
    let name = r.decode().map_err(|e| DavError::Xml(e.to_string()))?;
    match name.as_ref() {
        "lt" => Ok('<'),
        "gt" => Ok('>'),
        "amp" => Ok('&'),
        "apos" => Ok('\''),
        "quot" => Ok('"'),
        other => {
            let num = other.strip_prefix('#').ok_or_else(unknown)?;
            let code = match num.strip_prefix('x') {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => num.parse().ok(),
            };
            code.and_then(char::from_u32).filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r')).ok_or_else(unknown)
        }
    }
}

/// Parse a `207 Multi-Status` answer to a `PROPFIND` for the account `root`.
pub fn parse_multistatus(xml: &[u8], root: &DavRoot) -> Result<Listing, DavError> {
    if xml.len() > MAX_XML_BYTES {
        return Err(DavError::TooLarge);
    }
    let xml_err = |e: quick_xml::Error| DavError::Xml(e.to_string());
    let mut reader = NsReader::from_reader(xml);
    let mut p = Parser::new(root);
    loop {
        let (ns, ev) = reader.read_resolved_event().map_err(xml_err)?;
        match ev {
            Event::Start(e) => {
                let tag = p.current().child(&ns, e.local_name().as_ref());
                p.start(tag)?;
            }
            Event::Empty(e) => {
                let tag = p.current().child(&ns, e.local_name().as_ref());
                p.start(tag)?;
                p.end()?;
            }
            Event::End(_) => p.end()?,
            Event::Text(t) => p.add_text(&t.decode().map_err(|e| DavError::Xml(e.to_string()))?)?,
            Event::CData(t) => p.add_text(&String::from_utf8_lossy(&t))?,
            Event::GeneralRef(r) => p.add_text(reference(&r)?.encode_utf8(&mut [0; 4]))?,
            Event::Eof => break,
            _ => {}
        }
    }
    if !p.stack.is_empty() {
        return Err(DavError::Xml("the document ends inside an element".into()));
    }
    if !p.saw_multistatus {
        return Err(DavError::Xml("not a multistatus answer".into()));
    }
    Ok(p.out)
}

/// Days from 1970-01-01 to the given civil date (proleptic Gregorian; Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An HTTP date such as `Sun, 11 Oct 2026 10:00:00 GMT` as seconds since 1970-01-01 UTC. The
/// weekday is optional and not checked; only GMT/UTC is accepted. `None` for anything else.
pub fn parse_http_date(s: &str) -> Option<i64> {
    let s = s.trim();
    let s = s.split_once(',').map_or(s, |(_, rest)| rest).trim();
    let mut parts = s.split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month_name = parts.next()?;
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        .iter()
        .position(|m| m.eq_ignore_ascii_case(month_name))?;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':');
    let (h, m, sec): (i64, i64, i64) = (hms.next()?.parse().ok()?, hms.next()?.parse().ok()?, hms.next()?.parse().ok()?);
    if hms.next().is_some() || !matches!(parts.next()?, "GMT" | "UTC") || parts.next().is_some() {
        return None;
    }
    let month = i64::try_from(month).ok()? + 1;
    let days_in_month = match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1970..=9999).contains(&year)
        || !(1..=days_in_month).contains(&day)
        || !(0..24).contains(&h)
        || !(0..60).contains(&m)
        || !(0..=60).contains(&sec)
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + h * 3_600 + m * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> DavRoot {
        DavRoot::files("", "dima").unwrap()
    }

    /// An answer shaped like Nextcloud's (sabre/dav): the folder itself, a sub-folder, files with
    /// a 200 and a 404 propstat, an encoded name, and several entries that must be skipped.
    const ANSWER: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/dima/Photos/</d:href>
  <d:propstat><d:prop>
    <d:getlastmodified>Sun, 11 Oct 2026 10:00:00 GMT</d:getlastmodified>
    <d:resourcetype><d:collection/></d:resourcetype>
    <d:getetag>&quot;65a1b2c3d4e5f&quot;</d:getetag>
    <oc:fileid>1234</oc:fileid><oc:size>5242880</oc:size><oc:permissions>RGDNVCK</oc:permissions>
  </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:getcontentlength/><d:getcontenttype/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/dima/Photos/2026/</d:href>
  <d:propstat><d:prop><d:getlastmodified>Fri, 10 Oct 2026 08:30:15 GMT</d:getlastmodified><d:resourcetype><d:collection/></d:resourcetype>
    <d:getetag>"dir2026"</d:getetag><oc:fileid>1300</oc:fileid><oc:size>4194304</oc:size></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/dima/Photos/M%C3%B6we%201.jpg</d:href>
  <d:propstat><d:prop><d:getlastmodified>Thu, 01 Jan 1970 00:00:01 GMT</d:getlastmodified><d:getcontentlength>1048576</d:getcontentlength>
    <d:resourcetype/><d:getetag>"abc123"</d:getetag><d:getcontenttype>image/jpeg</d:getcontenttype><oc:fileid>1301</oc:fileid>
    <oc:permissions>RGDNVW</oc:permissions></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><oc:size/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/dima/Photos/Tom%20&amp;%20Jerry.jpg</d:href>
  <d:propstat><d:prop><d:getcontentlength>10</d:getcontentlength><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>http://192.168.178.84/remote.php/dav/files/dima/Photos/absolute.cr2</d:href>
  <d:propstat><d:prop><d:getcontentlength>26214400</d:getcontentlength><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response><d:href>/remote.php/dav/files/dima/Photos/../../secret</d:href>
  <d:propstat><d:prop><d:getcontentlength>1</d:getcontentlength></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/remote.php/dav/files/other/Photos/x.jpg</d:href>
  <d:propstat><d:prop><d:getcontentlength>1</d:getcontentlength></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/remote.php/dav/files/dima/Photos/CON.txt</d:href>
  <d:propstat><d:prop><d:getcontentlength>1</d:getcontentlength></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/remote.php/dav/files/dima/Photos/2026/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/remote.php/dav/files/dima/Photos/gone.jpg</d:href>
  <d:propstat><d:prop><d:getcontentlength/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response>
</d:multistatus>"#;

    fn by_name<'a>(l: &'a Listing, path: &str) -> &'a Entry {
        l.entries.iter().find(|e| e.path.to_string() == path).unwrap_or_else(|| panic!("{path} missing in {:?}", l.entries))
    }

    #[test]
    fn parses_a_nextcloud_listing() {
        let l = parse_multistatus(ANSWER.as_bytes(), &root()).unwrap();
        let paths: Vec<String> = l.entries.iter().map(|e| e.path.to_string()).collect();
        assert_eq!(paths, ["/Photos", "/Photos/2026", "/Photos/Möwe 1.jpg", "/Photos/Tom & Jerry.jpg", "/Photos/absolute.cr2"]);

        let top = by_name(&l, "/Photos");
        assert!(top.is_dir);
        assert_eq!(top.size, 5_242_880, "a folder's size comes from oc:size");
        assert_eq!(top.etag.as_deref(), Some("\"65a1b2c3d4e5f\""), "the ETag keeps its quotes");
        assert_eq!(top.modified, Some(1_791_712_800));
        assert_eq!(top.file_id.as_deref(), Some("1234"));
        assert!(top.can_write());

        let file = by_name(&l, "/Photos/Möwe 1.jpg");
        assert!(!file.is_dir);
        assert_eq!((file.size, file.modified), (1_048_576, Some(1)));
        assert_eq!(file.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!(file.permissions.as_deref(), Some("RGDNVW"));
        assert!(file.can_write());

        assert_eq!(by_name(&l, "/Photos/absolute.cr2").size, 26_214_400, "an absolute address is read by its path");
    }

    /// Eight entries taken from a real Nextcloud 34 answer (names replaced by neutral ones, all
    /// else as the server wrote it: element order, `&quot;` in ETags, `%20`, the 404 propstat of
    /// the folder, an `.ARW`, a script, `.JPG` and `.jpeg`).
    const REAL: &str = include_str!("../tests/fixtures/photos-propfind-excerpt.xml");

    #[test]
    fn parses_an_answer_captured_from_a_real_nextcloud() {
        let r = root();
        let l = parse_multistatus(REAL.as_bytes(), &r).unwrap();
        assert_eq!((l.entries.len(), l.skipped_total), (8, 0));
        let top = by_name(&l, "/Photos");
        assert!(top.is_dir && top.can_write());
        assert_eq!((top.size, top.file_id.as_deref(), top.permissions.as_deref()), (22_622_440_273, Some("5"), Some("RGDNVCK")));
        assert_eq!((top.etag.as_deref(), top.modified), (Some("\"6ac665e9070dd\""), Some(1_791_387_113)));
        assert_eq!(by_name(&l, "/Photos/Folder 1").size, 10_065_200_687);
        let raw = by_name(&l, "/Photos/IMG_0005.ARW");
        assert_eq!(
            (raw.is_dir, raw.size, raw.content_type.as_deref(), raw.modified),
            (false, 20_915_200, Some("image/x-dcraw"), Some(1_753_883_354))
        );
        assert_eq!(by_name(&l, "/Photos/Sample 3.jpg").size, 6_901_504, "%20 in the address is a space in the name");
        assert!(l.entries.iter().filter(|e| !e.is_dir).all(|e| e.etag.is_some() && e.modified.is_some() && e.file_id.is_some() && e.can_write()));
        // every address round-trips: what the client would request for an entry is what the server wrote
        for e in &l.entries {
            let url = r.url_path(&e.path, e.is_dir);
            assert!(REAL.contains(&format!("<d:href>{url}</d:href>")), "{url}");
        }
    }

    #[test]
    fn write_permission_follows_the_letters() {
        let entry = |is_dir: bool, permissions: Option<&str>| Entry {
            path: RemotePath::root(),
            is_dir,
            size: 0,
            modified: None,
            etag: None,
            file_id: None,
            content_type: None,
            permissions: permissions.map(str::to_string),
        };
        assert!(entry(false, Some("RGDNVW")).can_write());
        assert!(!entry(false, Some("RGDNV")).can_write(), "no W: a file you may not change");
        assert!(entry(true, Some("RGDNVCK")).can_write());
        assert!(!entry(true, Some("RGDNV")).can_write(), "no C or K: nothing may be added to it");
        assert!(entry(false, None).can_write() && entry(true, None).can_write(), "unknown counts as allowed");
    }

    #[test]
    fn skips_what_cannot_be_used_and_says_why() {
        let l = parse_multistatus(ANSWER.as_bytes(), &root()).unwrap();
        assert_eq!(l.skipped_total, 5);
        let why = |needle: &str| l.skipped.iter().find(|s| s.href.contains(needle)).map(|s| s.why.clone()).unwrap_or_default();
        assert!(why("secret").contains(".."), "{}", why("secret"));
        assert!(why("other").contains("outside"), "{}", why("other"));
        assert!(why("CON.txt").contains("device"), "{}", why("CON.txt"));
        assert!(why("2026").contains("twice"), "{}", why("2026"));
        assert!(why("gone").contains("no properties"), "{}", why("gone"));
    }

    #[test]
    fn namespaces_are_resolved_by_uri_not_by_prefix() {
        let xml = r#"<D:multistatus xmlns:D="DAV:" xmlns:o="http://owncloud.org/ns" xmlns:x="urn:other">
          <D:response><D:href>/remote.php/dav/files/dima/a.jpg</D:href>
            <D:propstat><D:prop><D:getcontentlength>7</D:getcontentlength><o:fileid>9</o:fileid><x:getcontentlength>999</x:getcontentlength></D:prop>
            <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        let l = parse_multistatus(xml.as_bytes(), &root()).unwrap();
        let e = l.entries.first().unwrap();
        assert_eq!((e.size, e.file_id.as_deref()), (7, Some("9")), "a same-named element of another namespace is ignored");
        // a default namespace works too
        let xml = r#"<multistatus xmlns="DAV:"><response><href>/remote.php/dav/files/dima/b</href>
            <propstat><prop><getcontentlength>3</getcontentlength></prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#;
        assert_eq!(parse_multistatus(xml.as_bytes(), &root()).unwrap().entries.first().map(|e| e.size), Some(3));
    }

    #[test]
    fn only_successful_propstats_count() {
        let xml = r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/remote.php/dav/files/dima/a</d:href>
          <d:propstat><d:prop><d:getcontentlength>5</d:getcontentlength></d:prop><d:status>HTTP/1.1 403 Forbidden</d:status></d:propstat>
          <d:propstat><d:prop><d:getetag>"e"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
        let e = parse_multistatus(xml.as_bytes(), &root()).unwrap().entries.remove(0);
        assert_eq!((e.size, e.etag.as_deref()), (0, Some("\"e\"")));
    }

    #[test]
    fn a_subfolder_install_has_its_own_prefix() {
        let sub = DavRoot::files("/nextcloud/", "dima").unwrap();
        assert_eq!(sub.url_path(&RemotePath::parse("/Photos/a b").unwrap(), false), "/nextcloud/remote.php/dav/files/dima/Photos/a%20b");
        assert_eq!(sub.href_to_path("/nextcloud/remote.php/dav/files/dima/Photos/").unwrap().to_string(), "/Photos");
        assert!(sub.href_to_path("/remote.php/dav/files/dima/Photos/").is_err());
    }

    #[test]
    fn urls_round_trip_through_hrefs() {
        let r = root();
        let p = RemotePath::parse("/Photos/Möwe 1 & 2/100%.jpg").unwrap();
        let url = r.url_path(&p, false);
        assert_eq!(url, "/remote.php/dav/files/dima/Photos/M%C3%B6we%201%20%26%202/100%25.jpg");
        assert_eq!(r.href_to_path(&url).unwrap(), p);
        assert_eq!(r.url_path(&RemotePath::root(), true), "/remote.php/dav/files/dima/");
        assert_eq!(r.href_to_path("/remote.php/dav/files/dima/").unwrap(), RemotePath::root());
        // a user name with characters that need encoding
        let odd = DavRoot::files("", "max mustermann@example.org").unwrap();
        assert_eq!(odd.url_path(&RemotePath::root(), true), "/remote.php/dav/files/max%20mustermann%40example.org/");
    }

    #[test]
    fn bad_hrefs_are_refused() {
        let r = root();
        for bad in [
            "relative/a.jpg",
            "/remote.php/dav/files/dima/a.jpg?x=1",
            "/remote.php/dav/files/dima/a%2Fb",
            "/remote.php/dav/files/dima/%2e%2e/x",
            "/remote.php/dav/files/dima/%00",
            "/remote.php/dav/files/dima/%zz",
            "/remote.php/dav/files",
            "/remote.php/dav/files/DIMA/a",
            "/",
            "",
        ] {
            assert!(r.href_to_path(bad).is_err(), "{bad:?}");
        }
        assert!(DavRoot::files("", "").is_err());
        assert!(DavRoot::files("", "a/b").is_err());
        assert!(DavRoot::files("/x/../y", "dima").is_err());
    }

    #[test]
    fn entities_and_cdata_are_decoded_and_unknown_ones_refused() {
        let wrap = |name: &str| {
            format!(
                r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>{name}</d:href><d:propstat><d:prop><d:getcontentlength>1</d:getcontentlength></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
            )
        };
        let one = |name: &str| parse_multistatus(wrap(name).as_bytes(), &root()).map(|l| l.entries.into_iter().next().map(|e| e.path.to_string()));
        assert_eq!(one("/remote.php/dav/files/dima/a&#x20;b&#65;").unwrap().as_deref(), Some("/a bA"));
        assert_eq!(one("<![CDATA[/remote.php/dav/files/dima/c]]>").unwrap().as_deref(), Some("/c"));
        assert!(one("/remote.php/dav/files/dima/&evil;").is_err(), "an undefined entity is an error, not dropped");
        assert!(one("/remote.php/dav/files/dima/&#0;").is_err(), "NUL is refused");
        assert!(one("/remote.php/dav/files/dima/&#xD800;").is_err(), "a surrogate is refused");
    }

    #[test]
    fn a_damaged_or_foreign_document_is_an_error() {
        let r = root();
        assert!(matches!(parse_multistatus(b"", &r), Err(DavError::Xml(_))));
        assert!(matches!(parse_multistatus(b"not xml at all", &r), Err(DavError::Xml(_))));
        let cut = &ANSWER.as_bytes()[..ANSWER.len() / 2];
        assert!(matches!(parse_multistatus(cut, &r), Err(DavError::Xml(_))), "a truncated answer must not look like a short listing");
        let err = r#"<?xml version="1.0"?><d:error xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns"><s:exception>Sabre\DAV\Exception\NotAuthenticated</s:exception></d:error>"#;
        assert!(matches!(parse_multistatus(err.as_bytes(), &r), Err(DavError::Xml(m)) if m.contains("multistatus")));
        let mismatched = br#"<d:multistatus xmlns:d="DAV:"><d:response></d:multistatus>"#;
        assert!(parse_multistatus(mismatched, &r).is_err());
    }

    #[test]
    fn size_depth_and_count_are_capped() {
        let r = root();
        assert_eq!(parse_multistatus(&vec![b' '; MAX_XML_BYTES + 1], &r), Err(DavError::TooLarge));
        let deep = format!(r#"<d:multistatus xmlns:d="DAV:">{}{}</d:multistatus>"#, "<a>".repeat(60), "</a>".repeat(60));
        assert!(matches!(parse_multistatus(deep.as_bytes(), &r), Err(DavError::TooComplex(_))));
        let long =
            format!(r#"<d:multistatus xmlns:d="DAV:"><d:response><d:href>/{}</d:href></d:response></d:multistatus>"#, "a".repeat(MAX_TEXT + 1));
        assert!(matches!(parse_multistatus(long.as_bytes(), &r), Err(DavError::TooComplex(_))));
        // very many skipped entries are counted but only a few are kept
        let one = r#"<d:response><d:href>/elsewhere</d:href><d:propstat><d:prop><d:getcontentlength>1</d:getcontentlength></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"#;
        let many = format!(r#"<d:multistatus xmlns:d="DAV:">{}</d:multistatus>"#, one.repeat(500));
        let l = parse_multistatus(many.as_bytes(), &r).unwrap();
        assert_eq!((l.entries.len(), l.skipped_total, l.skipped.len()), (0, 500, MAX_SKIPPED_KEPT));
    }

    #[test]
    fn http_dates() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("Sun, 11 Oct 2026 10:00:00 GMT"), Some(1_791_712_800));
        assert_eq!(parse_http_date("11 oct 2026 10:00:00 UTC"), Some(1_791_712_800), "weekday optional, month case-insensitive");
        assert_eq!(parse_http_date("Tue, 29 Feb 2028 23:59:59 GMT"), Some(1_835_481_599));
        assert_eq!(parse_http_date("Fri, 31 Dec 9999 23:59:59 GMT"), Some(253_402_300_799));
        for bad in [
            "",
            "yesterday",
            "Sun, 11 Oct 2026 10:00:00 PST",
            "Sun, 11 Oct 2026 10:00:00",
            "Sat, 31 Apr 2026 10:00:00 GMT",
            "Sat, 29 Feb 2027 10:00:00 GMT",
            "Sat, 11 Oct 1969 10:00:00 GMT",
            "Sun, 11 Oct 2026 24:00:00 GMT",
            "Sun, 11 Oct 2026 10:61:00 GMT",
            "Sat, 00 Oct 2026 10:00:00 GMT",
            "Sat, 11 Foo 2026 10:00:00 GMT",
            "Sun, 11 Oct 2026 10:00:00:00 GMT",
            "Sun, 11 Oct 2026 10:00:00 GMT extra",
        ] {
            assert_eq!(parse_http_date(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_request_pieces_are_well_formed() {
        assert_eq!(propfind_headers(Depth::One)[0], ("Depth", "1"));
        assert_eq!(propfind_headers(Depth::Zero)[0], ("Depth", "0"));
        // the request body is itself well-formed XML naming the properties the parser reads
        let mut reader = quick_xml::Reader::from_str(PROPFIND_BODY);
        let mut names = Vec::new();
        loop {
            match reader.read_event().unwrap() {
                Event::Empty(e) | Event::Start(e) => names.push(String::from_utf8_lossy(e.name().as_ref()).into_owned()),
                Event::Eof => break,
                _ => {}
            }
        }
        for want in ["d:getlastmodified", "d:getcontentlength", "d:getetag", "d:resourcetype", "oc:fileid", "oc:size", "oc:permissions"] {
            assert!(names.iter().any(|n| n == want), "{want} missing from {names:?}");
        }
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
                let _ = parse_multistatus(&bytes, &root());
            }

            #[test]
            fn a_damaged_real_answer_never_panics_and_never_yields_an_unchecked_path(start in 0usize..2000, len in 0usize..80, fill in any::<u8>(), cut in any::<bool>()) {
                let mut bytes = ANSWER.as_bytes().to_vec();
                let start = start.min(bytes.len());
                let end = (start + len).min(bytes.len());
                if cut { bytes.drain(start..end); } else { bytes[start..end].fill(fill); }
                if let Ok(l) = parse_multistatus(&bytes, &root()) {
                    for e in &l.entries {
                        for n in e.path.names() { prop_assert!(crate::path::check_name(n).is_ok()); }
                    }
                }
            }

            #[test]
            fn http_date_never_panics(s in "\\PC{0,40}") {
                let _ = parse_http_date(&s);
            }
        }
    }
}
