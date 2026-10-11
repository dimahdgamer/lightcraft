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

Everything a server sends is hostile input: tests include damaged copies of a real-shaped answer,
random bytes, hostile addresses and names, and checks that no accepted path escapes its folder.
