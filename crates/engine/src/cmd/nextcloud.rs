//! Nextcloud commands (see [`crate::nextcloud`]).
//!
//! Every one of them is declared without the command journal (`cmd!(query …)`): the journal keeps
//! each command's parameters for `query.journal`, and `nextcloud.connect` carries the app
//! password. None of them changes the catalog; the work that talks to a server runs on its own
//! thread, as a row in the activity stack, and `nextcloud.result` (or `wait: true`) gives its
//! outcome.

use std::path::{Path, PathBuf};

use lightcraft_nextcloud::pair::FolderPair;
use lightcraft_nextcloud::sync::{Keep, resolve};
use lightcraft_nextcloud::{Client, Credentials, Options, RemotePath};
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, bool_or, cmd, str_param};
use crate::nextcloud::native::{OpenJob, PairJob, PublishJob, run_open, run_pack_list, run_pairs, run_publish};
use crate::nextcloud::{DEFAULT_INTERVAL_SECS, MIN_INTERVAL_SECS, Pair, Server, valid_id, valid_name};
use crate::travel::{self, Manifest};
use crate::{EngineError, Result, Session};
use lightcraft_catalog::PhotoId;

/// Most entries a folder listing gives back.
const MAX_LISTED: usize = 5000;

fn other(e: impl ToString) -> EngineError {
    EngineError::Other(e.to_string())
}

fn library_dir(s: &Session) -> Option<PathBuf> {
    s.library.as_ref().map(|l| l.dir.clone())
}

/// Give the outcome now (`wait`) or say that the job started.
fn finish(s: &mut Session, job: u64, p: &Value) -> Result<Value> {
    if !bool_or(p, "wait", false) {
        return Ok(json!({"job": job, "running": true}));
    }
    let mut result = s.nextcloud.rt.wait().map_err(other)?;
    // what the job downloaded is read into the catalog now, as the job is done
    if let Some(applied) = s.nextcloud_apply_pending()
        && result.is_object()
    {
        result["applied"] = applied;
    }
    Ok(result)
}

/// `https://cloud.example.org/` → `cloud.example.org`.
fn default_name(url: &str) -> String {
    let host = url.split("://").last().unwrap_or(url).split('/').next().unwrap_or(url);
    host.chars().take(60).collect()
}

fn save(s: &mut Session) {
    if let Err(e) = s.save_prefs() {
        log::warn!("nextcloud: {e}");
    }
}

fn effective_interval(secs: u32) -> u32 {
    if secs == 0 { DEFAULT_INTERVAL_SECS } else { secs.max(MIN_INTERVAL_SECS) }
}

fn status(s: &mut Session, _: &Value) -> Result<Value> {
    s.nextcloud_apply_pending();
    let n = &s.nextcloud;
    let servers: Vec<Value> = n
        .prefs
        .servers
        .iter()
        .map(|sv| json!({"name": sv.name, "url": sv.url, "user": sv.user, "hasPassword": n.rt.has_secret(sv), "plainHttp": sv.url.starts_with("http://")}))
        .collect();
    let pairs: Vec<Value> = n.prefs.pairs.iter().map(|p| json!({"id": p.id, "server": p.server, "remote": p.remote, "local": p.local})).collect();
    Ok(json!({
        "servers": servers,
        "pairs": pairs,
        "rememberPasswords": n.rt.persistent_secrets(),
        "autoSync": n.prefs.auto_sync,
        "intervalSecs": effective_interval(n.prefs.interval_secs),
        "job": n.rt.status(),
    }))
}

fn result(s: &mut Session, _: &Value) -> Result<Value> {
    s.nextcloud_apply_pending();
    Ok(s.nextcloud.rt.status())
}

fn settings(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.settings";
    let prefs = &mut s.nextcloud.prefs;
    if let Some(on) = p.get("auto").and_then(Value::as_bool) {
        prefs.auto_sync = on;
    }
    if let Some(v) = p.get("intervalSecs") {
        let secs = v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n == 0 || *n >= MIN_INTERVAL_SECS)
            .ok_or_else(|| bad(C, format!("`intervalSecs` must be 0 (the default) or at least {MIN_INTERVAL_SECS}")))?;
        prefs.interval_secs = secs;
    }
    let (auto, secs) = (prefs.auto_sync, effective_interval(prefs.interval_secs));
    if p.as_object().is_some_and(|o| !o.is_empty()) {
        save(s);
    }
    Ok(json!({"auto": auto, "intervalSecs": secs}))
}

fn connect(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.connect";
    let text = |k: &str| str_param(p, k).map(str::trim).filter(|v| !v.is_empty());
    let url = text("url").ok_or_else(|| bad(C, "missing `url`"))?.trim_end_matches('/').to_string();
    let user = text("user").ok_or_else(|| bad(C, "missing `user`"))?.to_string();
    let name = text("name").map_or_else(|| default_name(&url), str::to_string);
    if !valid_name(&name) {
        return Err(bad(C, "`name` must be 1 to 60 characters without control characters"));
    }
    if s.nextcloud.rt.running() {
        return Err(bad(C, "a Nextcloud job is already running"));
    }
    let server = Server { name: name.clone(), url, user };
    let password = str_param(p, "password")
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| s.nextcloud.rt.secret(&server))
        .ok_or_else(|| bad(C, "missing `password`: an app password (Nextcloud ▸ Personal settings ▸ Security ▸ Devices & sessions)"))?;
    // checked before anything is stored or sent: the address, the user name, the password's form
    let credentials = Credentials::new(&server.user, &password).map_err(|e| bad(C, e.to_string()))?;
    Client::new(&server.url, credentials, Options::default()).map_err(|e| bad(C, e.to_string()))?;

    let prefs = &mut s.nextcloud.prefs;
    match prefs.servers.iter().position(|x| x.name == name) {
        Some(i) => {
            let old = std::mem::replace(&mut prefs.servers[i], server.clone());
            if old.user != server.user || old.url != server.url {
                s.nextcloud.rt.forget_secret(&old);
            }
        }
        None => prefs.servers.push(server.clone()),
    }
    s.nextcloud.rt.set_secret(&server, &password, bool_or(p, "remember", true)).map_err(other)?;
    save(s);

    let client = s.nextcloud.rt.client(&server).map_err(other)?;
    let (shown, user) = (name, server.user.clone());
    let job = s
        .nextcloud
        .rt
        .start(&s.activity, "connect", "Signing in to Nextcloud", move |cancel, progress| {
            progress.detail(&shown);
            client.check_login(cancel).map_err(|e| e.to_string())?;
            Ok(json!({"server": shown, "user": user}))
        })
        .map_err(other)?;
    finish(s, job, p)
}

fn disconnect(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.disconnect";
    let name = str_param(p, "name").ok_or_else(|| bad(C, "missing `name`"))?;
    let server = s.nextcloud.prefs.server(name).cloned().ok_or_else(|| bad(C, format!("no server `{name}`")))?;
    if s.nextcloud.rt.running() {
        return Err(bad(C, "a Nextcloud job is running: cancel it first"));
    }
    let states = s.nextcloud.rt.states(library_dir(s).as_deref());
    let prefs = &mut s.nextcloud.prefs;
    let removed: Vec<String> = prefs.pairs.iter().filter(|x| x.server == name).map(|x| x.id.clone()).collect();
    prefs.pairs.retain(|x| x.server != name);
    prefs.servers.retain(|x| x.name != name);
    for id in &removed {
        states.remove(id);
    }
    s.nextcloud.rt.forget_secret(&server);
    save(s);
    Ok(json!({"removedPairs": removed}))
}

/// Add a folder pair (or, with `existing_ok`, take the one that is there already).
fn make_pair(s: &mut Session, c: &str, server: &str, remote: &RemotePath, local: &str, existing_ok: bool) -> Result<Pair> {
    if s.nextcloud.prefs.server(server).is_none() {
        return Err(bad(c, format!("no server `{server}`: connect first")));
    }
    let local = local.trim();
    if local.is_empty() || !Path::new(local).is_absolute() {
        return Err(bad(c, "`local` must be an absolute folder path"));
    }
    let prefs = &mut s.nextcloud.prefs;
    let remote = remote.to_string();
    if let Some(found) = prefs.pairs.iter().find(|x| x.server == server && x.remote == remote && x.local == local) {
        return if existing_ok { Ok(found.clone()) } else { Err(bad(c, "that folder pair exists already")) };
    }
    let mut id;
    loop {
        prefs.next_pair = prefs.next_pair.saturating_add(1);
        id = format!("p{}", prefs.next_pair);
        if valid_id(&id) && prefs.pair(&id).is_none() {
            break;
        }
    }
    let pair = Pair { id, server: server.to_string(), remote, local: local.to_string() };
    prefs.pairs.push(pair.clone());
    save(s);
    Ok(pair)
}

fn pair_add(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.pair.add";
    let server = str_param(p, "server").ok_or_else(|| bad(C, "missing `server`"))?;
    let remote =
        RemotePath::parse(str_param(p, "remote").ok_or_else(|| bad(C, "missing `remote` (such as /Photos)"))?).map_err(|e| bad(C, e.to_string()))?;
    let local = str_param(p, "local").ok_or_else(|| bad(C, "missing `local` folder"))?;
    let pair = make_pair(s, C, server, &remote, local, false)?;
    Ok(json!({"id": pair.id, "remote": pair.remote, "local": pair.local}))
}

fn pair_remove(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.pair.remove";
    let id = str_param(p, "id").ok_or_else(|| bad(C, "missing `id`"))?;
    if s.nextcloud.prefs.pair(id).is_none() {
        return Err(bad(C, format!("no folder pair `{id}`")));
    }
    if s.nextcloud.rt.running() {
        return Err(bad(C, "a Nextcloud job is running: cancel it first"));
    }
    s.nextcloud.prefs.pairs.retain(|x| x.id != id);
    s.nextcloud.rt.states(library_dir(s).as_deref()).remove(id);
    save(s);
    Ok(Value::Null)
}

fn list(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.list";
    let name = str_param(p, "server").ok_or_else(|| bad(C, "missing `server`"))?;
    let server = s.nextcloud.prefs.server(name).cloned().ok_or_else(|| bad(C, format!("no server `{name}`")))?;
    let dir = RemotePath::parse(str_param(p, "path").unwrap_or("/")).map_err(|e| bad(C, e.to_string()))?;
    let client = s.nextcloud.rt.client(&server).map_err(other)?;
    let job = s
        .nextcloud
        .rt
        .start(&s.activity, "list", "Reading a Nextcloud folder", move |cancel, progress| {
            progress.detail(&dir.to_string());
            let mut entries = client.list(&dir, cancel).map_err(|e| e.to_string())?.entries;
            // folders first, then by name without regard to case
            entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.path.file_name().unwrap_or("").to_lowercase().cmp(&b.path.file_name().unwrap_or("").to_lowercase())));
            let total = entries.len();
            let items: Vec<Value> = entries
                .iter()
                .take(MAX_LISTED)
                .map(|e| json!({"name": e.path.file_name().unwrap_or(""), "path": e.path.to_string(), "dir": e.is_dir, "size": e.size, "modified": e.modified, "etag": e.etag}))
                .collect();
            Ok(json!({"path": dir.to_string(), "entries": items, "total": total, "truncated": total > MAX_LISTED}))
        })
        .map_err(other)?;
    finish(s, job, p)
}

/// The server a command is about: `server`, or the only one the library knows.
fn server_of(s: &Session, p: &Value, c: &str) -> Result<Server> {
    match str_param(p, "server") {
        Some(name) => s.nextcloud.prefs.server(name).cloned().ok_or_else(|| bad(c, format!("no server `{name}`"))),
        None => match s.nextcloud.prefs.servers.as_slice() {
            [only] => Ok(only.clone()),
            [] => Err(bad(c, "no server: connect first")),
            _ => Err(bad(c, "more than one server: say which with `server`")),
        },
    }
}

/// The folder on the server that holds travel packs (`base`, `/LightCraft` by default).
fn pack_base(p: &Value, c: &str) -> Result<RemotePath> {
    RemotePath::parse(str_param(p, "base").unwrap_or("/LightCraft")).map_err(|e| bad(c, e.to_string()))
}

fn pack_name(p: &Value, c: &str) -> Result<String> {
    let name = str_param(p, "name").map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad(c, "missing `name`"))?;
    if !travel::valid_pack_name(name) {
        return Err(bad(c, "`name` must be one usable folder name (none of / \\ : * ? \" < > |, no trailing dot or space)"));
    }
    Ok(name.to_string())
}

fn pack_publish(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.pack.publish";
    let server = server_of(s, p, C)?;
    let name = pack_name(p, C)?;
    let base = pack_base(p, C)?;
    let lib = library_dir(s).ok_or_else(|| bad(C, "a travel pack needs a library on disk"))?;
    let ids: Vec<PhotoId> = if let Some(list) = p.get("ids").and_then(Value::as_array) {
        list.iter().filter_map(Value::as_u64).map(PhotoId).collect()
    } else if bool_or(p, "all", false) {
        s.catalog.photos().filter(|ph| ph.in_library()).map(|ph| ph.id).collect()
    } else {
        s.targets(p)
    };
    if ids.is_empty() {
        return Err(bad(C, "no photos: select some, or pass `ids` or `all`"));
    }
    let root = match str_param(p, "root") {
        Some(r) => PathBuf::from(r),
        None => {
            let paths: Vec<PathBuf> = ids
                .iter()
                .filter_map(|id| s.catalog.photo(*id))
                .filter_map(|ph| if let lightcraft_catalog::Source::File { path } = &ph.source { Some(PathBuf::from(path)) } else { None })
                .collect();
            travel::common_root(&paths).ok_or_else(|| bad(C, "the photos share no folder: give `root`, the folder the pack is made from"))?
        }
    };
    if !root.is_absolute() {
        return Err(bad(C, "`root` must be an absolute folder path"));
    }
    let collected = travel::collect(s, &ids, &root).map_err(|e| bad(C, e))?;
    if collected.photos.is_empty() {
        let why: Vec<String> = collected.skipped.iter().take(5).map(|(n, w)| format!("{n}: {w}")).collect();
        return Err(bad(C, format!("nothing to publish ({})", why.join("; "))));
    }
    // the smart previews are what a laptop edits from: make the missing ones first, in the background
    if !collected.missing_previews.is_empty() {
        let missing: Vec<u64> = collected.missing_previews.iter().map(|id| id.0).collect();
        s.execute("library.smartPreviews", &json!({"ids": missing, "background": true}))?;
        return Ok(json!({"needsSmartPreviews": missing.len(), "building": true}));
    }
    // the edits go in the sidecars: write them for the photos of the pack
    let owners = crate::sidecar::StemOwners::of(&s.catalog);
    let sidecar_failures: Vec<Value> =
        collected.ids.iter().filter_map(|id| s.save_sidecar_with(*id, &owners).err().map(|e| json!({"id": id.0, "error": e.to_string()}))).collect();
    let pack = base.join(&name).map_err(|e| bad(C, e.to_string()))?;
    let remote_photos = pack.join("photos").map_err(|e| bad(C, e.to_string()))?;
    let pair = make_pair(s, C, &server.name, &remote_photos, &root.to_string_lossy(), true)?;
    let client = s.nextcloud.rt.client(&server).map_err(other)?;
    let pair_client = s.nextcloud.rt.client(&server).map_err(other)?;
    let states = s.nextcloud.rt.states(Some(&lib));
    let record_dir = lib.join("Nextcloud");
    std::fs::create_dir_all(&record_dir).map_err(|e| other(format!("can't create {}: {e}", record_dir.display())))?;
    let manifest = Manifest { version: travel::VERSION, name: name.clone(), created: (s.clock)(), photos: collected.photos.clone() };
    let skipped: Vec<Value> = collected.skipped.iter().take(50).map(|(n, w)| json!({"name": n, "why": w})).collect();
    let job = PublishJob {
        client,
        name,
        pack,
        manifest: manifest.to_json(),
        photos: collected.photos.len(),
        previews: collected.previews,
        record: record_dir.join(format!("pack-{}.json", manifest.name)),
        pair: PairJob { pair, client: pair_client, states },
        extra: json!({"skipped": skipped, "skippedCount": collected.skipped.len(), "sidecarFailures": sidecar_failures, "root": root.to_string_lossy()}),
    };
    let started = s
        .nextcloud
        .rt
        .start(&s.activity, "pack", "Publishing a travel pack", move |cancel, progress| run_publish(job, cancel, progress))
        .map_err(other)?;
    finish(s, started, p)
}

fn pack_open(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.pack.open";
    let server = server_of(s, p, C)?;
    let name = pack_name(p, C)?;
    let base = pack_base(p, C)?;
    let lib = library_dir(s).ok_or_else(|| bad(C, "opening a travel pack needs a library on disk"))?;
    let smart_dir = s.media.smart_dir.clone().ok_or_else(|| bad(C, "opening a travel pack needs a library on disk (it keeps the smart previews)"))?;
    let local_root = match str_param(p, "local") {
        Some(l) => PathBuf::from(l),
        None => lib.join("Travel Packs").join(&name),
    };
    if !local_root.is_absolute() {
        return Err(bad(C, "`local` must be an absolute folder path"));
    }
    let pack = base.join(&name).map_err(|e| bad(C, e.to_string()))?;
    let remote_photos = pack.join("photos").map_err(|e| bad(C, e.to_string()))?;
    let pair = make_pair(s, C, &server.name, &remote_photos, &local_root.join("photos").to_string_lossy(), true)?;
    let client = s.nextcloud.rt.client(&server).map_err(other)?;
    let pair_client = s.nextcloud.rt.client(&server).map_err(other)?;
    let states = s.nextcloud.rt.states(Some(&lib));
    let job = OpenJob { client, name, pack, local_root, smart_dir, pair: PairJob { pair, client: pair_client, states } };
    let started =
        s.nextcloud.rt.start(&s.activity, "pack", "Opening a travel pack", move |cancel, progress| run_open(job, cancel, progress)).map_err(other)?;
    finish(s, started, p)
}

fn pack_list(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.pack.list";
    let server = server_of(s, p, C)?;
    let base = pack_base(p, C)?;
    let client = s.nextcloud.rt.client(&server).map_err(other)?;
    let started = s
        .nextcloud
        .rt
        .start(&s.activity, "packList", "Looking for travel packs", move |cancel, progress| run_pack_list(client, base, cancel, progress))
        .map_err(other)?;
    finish(s, started, p)
}

/// Start a sync of one pair (or all) as a job. Used by `nextcloud.sync` and the automatic sync.
pub(crate) fn start_sync(s: &mut Session, which: Option<&str>, preview: bool) -> Result<u64> {
    const C: &str = "nextcloud.sync";
    let pairs: Vec<Pair> = match which {
        Some(id) => vec![s.nextcloud.prefs.pair(id).cloned().ok_or_else(|| bad(C, format!("no folder pair `{id}`")))?],
        None => s.nextcloud.prefs.pairs.clone(),
    };
    if pairs.is_empty() {
        return Err(bad(C, "no folder pairs: add one with nextcloud.pair.add"));
    }
    let states = s.nextcloud.rt.states(library_dir(s).as_deref());
    let mut jobs = Vec::new();
    for pair in pairs {
        let server = s.nextcloud.prefs.server(&pair.server).cloned().ok_or_else(|| bad(C, format!("no server `{}`", pair.server)))?;
        let client = s.nextcloud.rt.client(&server).map_err(other)?;
        jobs.push(PairJob { pair, client, states: states.clone() });
    }
    let label = if preview { "Checking Nextcloud" } else { "Syncing with Nextcloud" };
    let job = s.nextcloud.rt.start(&s.activity, "sync", label, move |cancel, progress| run_pairs(jobs, preview, cancel, progress)).map_err(other)?;
    // any sync counts for the automatic one's interval, so it does not start right after a manual one
    s.nextcloud.rt.last_start = Some(web_time::Instant::now());
    Ok(job)
}

fn sync(s: &mut Session, p: &Value) -> Result<Value> {
    let which = str_param(p, "pair").filter(|v| *v != "all");
    let job = start_sync(s, which, bool_or(p, "preview", false))?;
    finish(s, job, p)
}

fn resolve_conflict(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.resolve";
    let id = str_param(p, "pair").ok_or_else(|| bad(C, "missing `pair`"))?;
    let pair = s.nextcloud.prefs.pair(id).cloned().ok_or_else(|| bad(C, format!("no folder pair `{id}`")))?;
    let path = RemotePath::parse(str_param(p, "path").ok_or_else(|| bad(C, "missing `path` (a conflict from nextcloud.sync)"))?)
        .map_err(|e| bad(C, e.to_string()))?;
    let base = RemotePath::parse(&pair.remote).map_err(|e| bad(C, e.to_string()))?;
    if path == base || !path.starts_with(&base) {
        return Err(bad(C, format!("{path} is not a file inside {}", pair.remote)));
    }
    let keep = match str_param(p, "keep") {
        Some("local") => Keep::Local,
        Some("server") => Keep::Server,
        _ => return Err(bad(C, "`keep` must be \"local\" (this machine's file replaces the server's) or \"server\"")),
    };
    let server = s.nextcloud.prefs.server(&pair.server).cloned().ok_or_else(|| bad(C, format!("no server `{}`", pair.server)))?;
    let client = s.nextcloud.rt.client(&server).map_err(other)?;
    let states = s.nextcloud.rt.states(library_dir(s).as_deref());
    let job = s
        .nextcloud
        .rt
        .start(&s.activity, "resolve", "Resolving a Nextcloud conflict", move |cancel, progress| {
            progress.detail(path.file_name().unwrap_or(""));
            let folder = FolderPair::new(base, PathBuf::from(&pair.local));
            let (mut state, _) = states.load(&pair.id);
            let outcome = resolve(&client, &folder, &mut state, &path, keep, cancel, &mut |_| {});
            // the record is saved either way: it is always up to date with what was done
            states.save(&pair.id, &state)?;
            outcome.map_err(|e| e.to_string())?;
            if keep == Keep::Server {
                progress.add_downloaded(folder.local_for(&path));
            }
            Ok(json!({"pair": pair.id, "path": path.to_string(), "kept": if keep == Keep::Local { "local" } else { "server" }}))
        })
        .map_err(other)?;
    finish(s, job, p)
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "nextcloud.status",
            "Nextcloud Status",
            [],
            None,
            "{} → {servers: [{name, url, user, hasPassword, plainHttp}], pairs: [{id, server, remote, local}], rememberPasswords: bool, job} — the servers and folder pairs this library knows (never a password), whether passwords survive a restart, and the last or running job",
            always,
            status
        ),
        cmd!(
            query "nextcloud.connect",
            "Connect to Nextcloud",
            [],
            None,
            "{url, user, password?, name?, remember?: true, wait?: false} — add a server (an app password from Nextcloud ▸ Personal settings ▸ Security, never the account password; `password` may be left out to use the stored one or $LIGHTCRAFT_NEXTCLOUD_PASSWORD) and check the login in the background → {job} or, with `wait`, {server, user}. Plain http:// is accepted only for addresses on the local network. The password is not saved in the library",
            always,
            connect
        ),
        cmd!(
            query "nextcloud.disconnect",
            "Disconnect from Nextcloud",
            [],
            None,
            "{name} — forget a server, its folder pairs and their records, and its stored password; no file on either side is touched → {removedPairs}",
            always,
            disconnect
        ),
        cmd!(
            query "nextcloud.pair.add",
            "Add Nextcloud Folder Pair",
            [],
            None,
            "{server, remote, local} — keep the server folder `remote` (such as /Photos) in step with the absolute local folder `local`: XMP sidecars go both ways, nothing else is transferred and nothing is deleted → {id, remote, local}",
            always,
            pair_add
        ),
        cmd!(
            query "nextcloud.pair.remove",
            "Remove Nextcloud Folder Pair",
            [],
            None,
            "{id} — stop keeping a pair in step; its files are not touched",
            always,
            pair_remove
        ),
        cmd!(
            query "nextcloud.list",
            "List a Nextcloud Folder",
            [],
            None,
            "{server, path?: \"/\", wait?: false} — what is directly inside a server folder, in the background → {job} or, with `wait`, {path, entries: [{name, path, dir, size, modified, etag}], total, truncated}",
            always,
            list
        ),
        cmd!(
            query "nextcloud.sync",
            "Sync with Nextcloud",
            [],
            None,
            "{pair?: id|\"all\", preview?: false, wait?: false} — bring folder pairs in step (sidecars both ways; a conflict, or a file deleted on one side, is reported and moves nothing), in the background → {job} or, with `wait`, {pairs: [{pair, uploaded, downloaded, conflicts, remoteDeleted, localDeleted, failed, clean, …}]}; `preview` only plans",
            always,
            sync
        ),
        cmd!(
            query "nextcloud.pack.publish",
            "Publish a Travel Pack",
            [],
            None,
            "{name, ids?, all?: bool, root?, server?, base?: \"/LightCraft\", wait?: false} — put photos on the Nextcloud so they can be edited on a laptop that has no originals: the smart previews, a manifest and the sidecars (written for the pack), in `<base>/<name>/`; the photo folder becomes a folder pair on its photos/. Photos without a smart preview are made first in the background → {needsSmartPreviews, building}; run it again when they are done. `root` is the folder the pack is made from (the deepest folder the photos share by default). → {job} or, with `wait`, {pack, photos, previewsSent, previewsFailed, sync, skipped, sidecarFailures}",
            always,
            pack_publish
        ),
        cmd!(
            query "nextcloud.pack.list",
            "List Travel Packs",
            [],
            None,
            "{server?, base?: \"/LightCraft\", wait?: false} — the packs on the server → {job} or, with `wait`, {base, packs: [{name, manifestSize, modified}]}",
            always,
            pack_list
        ),
        cmd!(
            query "nextcloud.pack.open",
            "Open a Travel Pack",
            [],
            None,
            "{name, server?, base?: \"/LightCraft\", local?, wait?: false} — fetch a pack's smart previews and sidecars and make offline records of its photos (no original here; they are edited from their smart previews, and what is edited goes back as sidecars) → {job} or, with `wait`, {pack, photos, skippedEntries, previewsFetched, previewsHad, previewsFailed, sync, applied.pack: {added, existing}}. `local` is where the pack lives on this machine (`Travel Packs/<name>` in the library by default)",
            always,
            pack_open
        ),
        cmd!(
            query "nextcloud.resolve",
            "Resolve a Nextcloud Conflict",
            [],
            None,
            "{pair, path, keep: \"local\"|\"server\", wait?: false} — decide a conflict that nextcloud.sync reported: `local` replaces the server's file with this machine's (or restores it there), `server` replaces this machine's (or restores it) and reads it into the catalog; the file is then in step → {job} or, with `wait`, {pair, path, kept}. Nothing is decided without this call",
            always,
            resolve_conflict
        ),
        cmd!(
            query "nextcloud.settings",
            "Nextcloud Settings",
            [],
            None,
            "{auto?: bool, intervalSecs?: 0|>=30} → {auto, intervalSecs} — whether the app syncs by itself (a few seconds after an edit, and every `intervalSecs`, 300 by default)",
            always,
            settings
        ),
        cmd!(
            query "nextcloud.cancel",
            "Cancel Nextcloud Job",
            [],
            None,
            "{} → {cancelled: bool} — stop the running job at its next check; what is finished stays recorded",
            always,
            |s, _| Ok(json!({"cancelled": s.nextcloud.rt.cancel()}))
        ),
        cmd!(
            query "nextcloud.result",
            "Nextcloud Job Result",
            [],
            None,
            "{} → null or {id, kind, running, done, total, detail, applied?, ok?, result?, error?} — the last or running job; sidecars it downloaded are read into the catalog when this is asked after it is done (`applied`)",
            always,
            result
        ),
    ]
}
