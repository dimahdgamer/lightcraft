# Nextcloud

> **Last updated:** 2026-10-12 · **Status:** engine commands, applying downloads, automatic sync, conflict decisions, the dialog in the desktop app, (Windows) remembering the app password, and travel packs, with tests.

Keeps XMP sidecars in step between a folder of a library and a folder on a Nextcloud server, so edits made on
one machine arrive on the other. Originals are never uploaded or changed. A **travel pack** adds the smart
previews of some photos, so a laptop without the originals can edit them too.

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

## In the app

**File ▸ Nextcloud…** opens one dialog: the connection (address, user name, app password), the folder pairs
(*Server folder* and *On this computer*, with *Choose…*), *Sync by itself*, *Sync now*, the travel packs
(below), and what the last job did. A conflict is listed with **Keep this computer's** and **Keep the server's**. Buttons wait while a job
runs; the same job shows as a row in the activity stack. Everything the dialog does is a `nextcloud.*`
command. The app calls `Session::nextcloud_tick` every frame (see below), so the automatic sync works with
the dialog closed. The password field shows dots, and the password is cleared from the dialog as soon as
the engine has it; it is not in the dialog's debug text, nor in what the control channel can read.

On **Windows** the app password is remembered in the Credential Manager (a generic credential named
`<user>@<address>.LightCraft Nextcloud`, readable by your Windows user only) when *Remember the app password
on this computer* is ticked. Elsewhere it is kept until LightCraft is closed, and the dialog says so. The
web build has no Nextcloud.

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
| `nextcloud.resolve` | decide a conflict: `keep: "local"` or `"server"` |
| `nextcloud.pack.publish` / `.list` / `.open` | travel packs (below) |
| `nextcloud.settings` | `auto` (sync by itself) and `intervalSecs` |
| `nextcloud.cancel`, `nextcloud.result` | stop / read the running or last job |

Jobs that talk to a server run on their own thread, as a row (`kind: nextcloud`) in the activity stack.
They return `{job}` at once; `wait: true` gives the outcome instead. One job runs at a time.

## Downloads reach the catalog

A downloaded sidecar is read into the photo it belongs to (`<stem>.xmp` or `<file name>.xmp` in the photo's
folder) as one undo step, the sidecar winning for what it states, as Read Metadata from File does. This
happens when a job is waited for (`wait: true`), or when `nextcloud.status` / `nextcloud.result` is asked after
the job is done (`applied` in the reply), or from `Session::nextcloud_tick`. A sidecar of a photo that is not in
the library is left for the import, which reads sidecars as it always does.

**The sidecar is not written back after being read**, even with *Automatically Write Changes into XMP* on:
rewriting it would change its time, the next sync would send it up again, and a second machine would do the
same with it, for ever.

## Syncing by itself

`nextcloud.settings {auto: true}` makes `Session::nextcloud_tick` (which a frontend calls often; it is cheap)
start a sync of all pairs ten seconds after the last edit, and every `intervalSecs` (300 by default, at
least 30). Without a pair or with a job running it does nothing.

## Deciding a conflict

A conflict is reported by `nextcloud.sync` and stays one, on every run, until it is decided with
`nextcloud.resolve {pair, path, keep}`: `local` replaces the server's file with this machine's (or restores it
on the server if it was deleted there), `server` replaces this machine's file and reads it into the catalog
(or restores it here). A side that no longer has the file cannot be kept. Keeping *both* (the other side's
edit as a named version of the photo) is not built yet.

## Travel packs

For editing away from home without carrying the originals (a Canon 80D raw is 25 MB; a smart preview is a
fraction of that). A pack is a folder on the server, `/LightCraft/<name>/` (`base` changes the first part):

```text
manifest.json              what each photo is: path, size, camera data, content hash …
previews/<xx>/<hash>.lcsp  the smart previews, named by the hash of the original's bytes
photos/<path>/IMG.xmp      the sidecars, in folders laid out like the originals' folders
```

**On the computer that has the originals** select the photos and use *Publish selected photos* in the dialog
(`nextcloud.pack.publish {name}`; `ids` or `all` instead of the selection). The folder the photos share
(or `root`) becomes a folder pair with `photos/`. Photos that have no smart preview yet get one first, in the
background; the command says so (`needsSmartPreviews`) and sends nothing, so press it again when they are done.
Publishing writes the sidecars of the pack's photos next to the originals, as *Write Changes into XMP* would.

**On the laptop** *Look for packs*, then *Open* (`nextcloud.pack.open {name}`). The previews and sidecars are
fetched, and every photo of the pack is added to the library as a record whose original is not there (like a
photo on an unplugged drive; the library shows it as missing) and which is edited from its smart preview.
`local` says where the pack lives on the laptop (`Travel Packs/<name>` in the library by default). A photo
the library already has, by path or by content, is not added twice.

From then on the PC's photo folder and the laptop's `photos/` folder are two ends of an ordinary folder pair
on the server's `photos/`: with *Sync by itself* on (or with *Sync now*) an edit made on the laptop arrives as
a sidecar on the PC, where *Synchronize Folder* applies it as for any sidecar, and the other way round.
Conflicts are decided as always. Nothing bounces back: opening a pack does not rewrite the sidecars it just
fetched.

Everything read from a pack is untrusted: a manifest entry that is invalid (a path leaving the pack, an
absurd size, a duplicate) is skipped and counted (`skippedEntries`), a manifest from a newer format version is
refused, and sizes and counts are capped. The manifest is uploaded last, so a pack that is listed is complete
as far as its previews go; a second publish sends only the previews that are not on the server yet.

Limits: previews are sent one after the other; a photo outside the pack's folder is skipped (and listed);
*Remove* on a missing original in the library is not what you want for pack photos; the web build has no packs.

## What is saved, and where

- `prefs.json` of the library: server name, address and user name, and the folder pairs. **No password.**
- `Nextcloud/pack-<name>.json` in the library folder: which previews of a pack were already sent.
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

## A bad network

A connection that drops (Wi-Fi gone, laptop closed) ends the run with an error and leaves the record as it
was; the next run starts again where things stand. A download that is cut off stays as its `.part-…` file and
the next run asks only for the rest, as long as the server's version is the same. If the server stored an
upload but its answer never arrived, or the app was closed after a download but before the record was
saved, both sides now hold the same file (same size and time) and the next run recognises that and records
it instead of reporting a conflict. Two edits that merely have the same size are still a conflict. Such a
download is then not read into the catalog by the sync; *Synchronize Folder* does that, as for any sidecar
changed from outside.

## Speed

Listing a folder of 2,400 photos took about six seconds on a Raspberry Pi 4, so a folder is listed only when
its version changed: if the top folder's ETag is the one recorded by the last complete run, nothing is listed
(one small request); otherwise only sub-folders whose ETag changed are listed. A run is complete when it left
no conflict, failure or reported deletion.

## Not yet

Remembering the password in the macOS Keychain and the Linux secret service (the Windows store is done),
keeping both sides of a conflict, translations of the dialog beyond German, downloading whole folders of
originals, and exporting finished photos to the Nextcloud.
