# lightcraft-nextcloud

Nextcloud for LightCraft, in pure Rust (L1). This crate holds what can be done without a network
or a disk, so it builds for wasm32 and is tested without a server:

- `path`: [`RemotePath`], a path inside an account's files built only from names that are safe on
  Windows, macOS and Linux (no `..`, separators, drive letters, device names, control characters,
  trailing dots or spaces; bounded length and depth), plus percent-encoding of address segments.
- `dav`: the `PROPFIND` request pieces and `parse_multistatus`, which reads the `207 Multi-Status`
  answer. Namespaces are resolved by URI, only properties with a 2xx status count, and an entry
  with an address outside the account, an unsafe name, a duplicate or no properties is skipped and
  reported, never allowed to hide the others. Size, nesting, text length and entry count are capped;
  a truncated or foreign document is an error, not a short listing.
- `auth`: `Credentials` (user name and **app password**, never the account password) and the Basic
  `Authorization` header. The secret is hidden from `Debug`.

- `client` (native only, on `lightcraft-fetch`): `Client` with `check_login`, `list`, `stat`, `download`,
  `upload`, `mkcol`, `mkcol_all` and `delete`.
  - The app password goes only to the address that was given. A redirect is an error, never followed.
  - Plain `http://` is refused for anything but the local network (`is_plain_http()` lets a caller warn
    about the local case).
  - A download goes to `<name>.part-<hash of the ETag>` and takes its real name only at the announced
    size; it continues only with bytes of the *same* version, and without an ETag it never continues.
  - An upload carries `If-Match` / `If-None-Match`, so it cannot overwrite a change it has not seen, and
    it fails with `SourceChanged` if the file changed while it was sent.
  - Tested against a small in-memory WebDAV server on 127.0.0.1, never the internet.

- `plan`: pure functions that decide what to transfer. For each file the local state, the server's state and
  the state recorded after the last transfer are compared: changed here only is an upload that must find the
  version it saw (`If-Match`), changed on the server only is a download, changed on both sides or
  deleted-on-one-side-and-changed-on-the-other is a **conflict** and moves nothing, and a deletion is only
  reported, never carried out. Property tests check that following a plan converges and that a conflict
  never changes either side.
- `pair`: `FolderPair` (a server folder and a local folder, mapping paths both ways without ever leaving
  either), the record of what both sides looked like (`State`, JSON, read back as untrusted input), and
  `scan_local` (bounded, symlinks not followed).

- `sync` (native only): `prepare` (list the server, scan the local folder, plan; nothing is changed),
  `execute` (carry the plan out with the client and record every transfer) and `sync_pair` (both). Listing a
  big server folder is slow on a small machine (six seconds for 2,400 files on a Raspberry Pi), so it is
  skipped when the top folder's ETag is the one recorded by the last complete run, and unchanged
  sub-folders are skipped when it is not. A problem with one file is reported for that file; a problem
  with the connection ends the run; the record is up to date either way.

Everything a server sends is hostile input: tests include damaged copies of a real-shaped answer,
random bytes, hostile addresses and names, and checks that no accepted path escapes its folder.
