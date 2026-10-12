# Nextcloud

> **Last updated:** 2026-10-12 · **Status:** engine commands and tests; the desktop dialogs are not built yet.

Keeps XMP sidecars (and nothing else) in step between a folder of a library and a folder on a Nextcloud
server, so edits made on one machine arrive on the other. Originals are never uploaded or changed.

## Idea

A **folder pair** joins a server folder (`/Photos`) with a local folder. Per file, three states are compared: what
is here, what the server has, and what both looked like after the last transfer. Whichever side differs from
that record has changed:

| here | server | what happens |
|---|---|---|
| changed | unchanged | upload, only if the server still has the version that was seen (`If-Match`) |
| unchanged | changed | download |
| changed | changed | **conflict**: reported, nothing is moved |
| deleted | unchanged, or the reverse | reported; **nothing is ever deleted** on either side |

Two sidecars named alike on both sides that differ and have no record yet are also a conflict. The decision
logic is `lightcraft_nextcloud::plan`, pure functions with property tests.

## Setting up

1. In Nextcloud: *Personal settings ▸ Security ▸ Devices & sessions ▸ Create new app password*. LightCraft
   asks for this app password, **never** the account password. It can be revoked on its own and does not need
   the second factor.
2. `nextcloud.connect {url, user, password}` (`url` such as `https://cloud.example.org`, or
   `http://192.168.178.84` on the local network). The login is checked in the background.
3. `nextcloud.pair.add {server, remote: "/Photos", local: "D:\\Photos"}`.
4. `nextcloud.sync {wait: true}`; `preview: true` only plans.

## Commands

All of them are declared without the command journal, which would otherwise keep their parameters (and so
the password) for `query.journal`.

| id | what |
|---|---|
| `nextcloud.status` | servers and pairs (never a password), whether passwords survive a restart, the last job |
| `nextcloud.connect` | add a server and check the login |
| `nextcloud.disconnect` | forget a server, its pairs, their records and its stored password |
| `nextcloud.pair.add` / `.remove` | folder pairs |
| `nextcloud.list` | what is directly inside a server folder |
| `nextcloud.sync` | bring pairs in step (`pair`, `preview`) |
| `nextcloud.cancel`, `nextcloud.result` | stop / read the running or last job |

Jobs that talk to a server run on their own thread, as a row (`kind: nextcloud`) in the activity stack.
They return `{job}` at once; `wait: true` gives the outcome instead. One job runs at a time.

## What is saved, and where

- `prefs.json` of the library: server name, address and user name, and the folder pairs. **No password.**
- `Nextcloud/<pair id>.json` in the library folder: the record of the last transfer of each file and the
  versions of the server's folders. It is read back as untrusted input; a damaged record is kept as
  `.corrupt-<time>` and the pair starts afresh (every file that differs is then a conflict, never an
  overwrite).
- The app password: in memory for the session. An app can install a `SecretStore` (the system's credential
  store) with `Session::nextcloud.set_secret_store` to remember it. For scripts,
  `LIGHTCRAFT_NEXTCLOUD_PASSWORD` is used when nothing else is stored.

## Safety

- The password goes only to the server address given, over `https://`; plain `http://` is accepted only for
  addresses on the local network (private, loopback, link-local, `*.local`, `*.lan`, single-label names) and
  the status says `plainHttp: true` so a UI can warn. A redirect is an error and is never followed.
- Names the server sends are checked before anything is written: no `..`, separators, drive letters, Windows
  device names (`CON`, `NUL`…), control characters, or trailing dots and spaces. A downloaded file cannot
  land outside its pair's local folder.
- A download goes to `<name>.part-<hash of the ETag>` and takes its name only at the announced size; it
  continues only with bytes of the same version.
- Like every command, these are open to anything that can use the control channel or the MCP server (both
  are loopback-only and have no authentication, see [control-protocol.md](control-protocol.md)): an agent
  with that access can use a server the library is connected to, but cannot read the password back.

## Speed

Listing a folder of 2,400 photos took about six seconds on a Raspberry Pi 4, so a folder is listed only when
its version changed: if the top folder's ETag is the one recorded by the last complete run, nothing is listed
(one small request); otherwise only sub-folders whose ETag changed are listed. A run is complete when it left
no conflict, failure or reported deletion.

## Not yet

Applying downloaded sidecars to the catalog, upload after an edit without a manual sync, the dialogs of the
desktop app, remembering the password in the system's credential store, and the travel pack (smart previews
and a manifest, to edit without the originals).
