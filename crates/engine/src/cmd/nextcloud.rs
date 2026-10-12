//! Nextcloud commands (see [`crate::nextcloud`]).
//!
//! Every one of them is declared without the command journal (`cmd!(query …)`): the journal keeps
//! each command's parameters for `query.journal`, and `nextcloud.connect` carries the app
//! password. None of them changes the catalog; the work that talks to a server runs on its own
//! thread, as a row in the activity stack, and `nextcloud.result` (or `wait: true`) gives its
//! outcome.

use std::path::{Path, PathBuf};

use lightcraft_nextcloud::{Client, Credentials, Options, RemotePath};
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, bool_or, cmd, str_param};
use crate::nextcloud::native::{PairJob, run_pairs};
use crate::nextcloud::{Pair, Server, valid_id, valid_name};
use crate::{EngineError, Result, Session};

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
    if bool_or(p, "wait", false) { s.nextcloud.rt.wait().map_err(other) } else { Ok(json!({"job": job, "running": true})) }
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

fn status(s: &mut Session, _: &Value) -> Result<Value> {
    let n = &s.nextcloud;
    let servers: Vec<Value> = n
        .prefs
        .servers
        .iter()
        .map(|sv| json!({"name": sv.name, "url": sv.url, "user": sv.user, "hasPassword": n.rt.has_secret(sv), "plainHttp": sv.url.starts_with("http://")}))
        .collect();
    let pairs: Vec<Value> = n.prefs.pairs.iter().map(|p| json!({"id": p.id, "server": p.server, "remote": p.remote, "local": p.local})).collect();
    Ok(json!({"servers": servers, "pairs": pairs, "rememberPasswords": n.rt.persistent_secrets(), "job": n.rt.status()}))
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

fn pair_add(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.pair.add";
    let server = str_param(p, "server").ok_or_else(|| bad(C, "missing `server`"))?;
    if s.nextcloud.prefs.server(server).is_none() {
        return Err(bad(C, format!("no server `{server}`: connect first")));
    }
    let remote =
        RemotePath::parse(str_param(p, "remote").ok_or_else(|| bad(C, "missing `remote` (such as /Photos)"))?).map_err(|e| bad(C, e.to_string()))?;
    let local = str_param(p, "local").map(str::trim).filter(|v| !v.is_empty()).ok_or_else(|| bad(C, "missing `local` folder"))?;
    if !Path::new(local).is_absolute() {
        return Err(bad(C, "`local` must be an absolute folder path"));
    }
    let prefs = &mut s.nextcloud.prefs;
    let (remote, local) = (remote.to_string(), local.to_string());
    if prefs.pairs.iter().any(|x| x.server == server && x.remote == remote && x.local == local) {
        return Err(bad(C, "that folder pair exists already"));
    }
    let mut id;
    loop {
        prefs.next_pair = prefs.next_pair.saturating_add(1);
        id = format!("p{}", prefs.next_pair);
        if valid_id(&id) && prefs.pair(&id).is_none() {
            break;
        }
    }
    prefs.pairs.push(Pair { id: id.clone(), server: server.to_string(), remote: remote.clone(), local: local.clone() });
    save(s);
    Ok(json!({"id": id, "remote": remote, "local": local}))
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

fn sync(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "nextcloud.sync";
    let which = str_param(p, "pair").filter(|v| *v != "all");
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
    let preview = bool_or(p, "preview", false);
    let label = if preview { "Checking Nextcloud" } else { "Syncing with Nextcloud" };
    let job = s.nextcloud.rt.start(&s.activity, "sync", label, move |cancel, progress| run_pairs(jobs, preview, cancel, progress)).map_err(other)?;
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
            "{} → null or {id, kind, running, done, total, detail, ok?, result?, error?} — the last or running job",
            always,
            |s, _| Ok(s.nextcloud.rt.status())
        ),
    ]
}
