//! Nextcloud in the engine: the servers and folder pairs a library knows (saved in `prefs.json`,
//! never with a password), where the app password is kept, and the one background job at a time
//! that talks to a server.
//!
//! The password is never written to `prefs.json`, to the command journal or to a reply. It lives
//! in a [`SecretStore`]: the session's memory by default, and whatever the app installs (the
//! desktop app puts the system's credential store here) so it survives a restart. For scripts the
//! environment variable [`ENV_PASSWORD`] is the last resort.
//!
//! What the server does to the library is bounded: files are exchanged only inside the local
//! folder of a pair, a conflict moves nothing, and nothing is ever deleted on either side
//! (see `lightcraft_nextcloud::plan`).

use serde::{Deserialize, Serialize};

/// The environment variable that supplies the app password when none is stored (scripts, CI).
pub const ENV_PASSWORD: &str = "LIGHTCRAFT_NEXTCLOUD_PASSWORD";

/// A Nextcloud server and the account on it. No secret.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Server {
    /// What the person calls it (`Home`); unique in the library.
    pub name: String,
    /// `https://cloud.example.org` or `http://192.168.178.84`.
    pub url: String,
    pub user: String,
}

/// A server folder kept in step with a local folder.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Pair {
    /// A short stable name, also the name of the pair's record file: `[A-Za-z0-9_-]{1,32}`.
    pub id: String,
    /// The [`Server::name`].
    pub server: String,
    /// The folder on the server, such as `/Photos`.
    pub remote: String,
    /// The folder on this machine (absolute).
    pub local: String,
}

/// What a library remembers about Nextcloud.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct NextcloudPrefs {
    pub servers: Vec<Server>,
    pub pairs: Vec<Pair>,
    /// The number the next pair id is made from.
    pub next_pair: u32,
}

/// A name a person gave (a server): 1 to 60 characters, none of them a control character.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.chars().count() <= 60 && !s.chars().any(char::is_control)
}

/// A pair id: it becomes a file name, so it is letters, digits, `_` and `-` only.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl NextcloudPrefs {
    /// Drop what a hand-edited or damaged `prefs.json` could misuse: servers with an unusable name
    /// or a repeated one, pairs whose id is not a safe file name, repeated or pointing at no server.
    pub fn sanitized(mut self) -> NextcloudPrefs {
        let mut seen = std::collections::HashSet::new();
        self.servers.retain(|s| valid_name(&s.name) && !s.url.is_empty() && !s.user.is_empty() && seen.insert(s.name.clone()));
        let names: std::collections::HashSet<&str> = self.servers.iter().map(|s| s.name.as_str()).collect();
        let mut ids = std::collections::HashSet::new();
        self.pairs.retain(|p| {
            valid_id(&p.id) && names.contains(p.server.as_str()) && !p.remote.is_empty() && !p.local.is_empty() && ids.insert(p.id.clone())
        });
        self
    }

    pub fn server(&self, name: &str) -> Option<&Server> {
        self.servers.iter().find(|s| s.name == name)
    }

    pub fn pair(&self, id: &str) -> Option<&Pair> {
        self.pairs.iter().find(|p| p.id == id)
    }

    /// The key an app password is stored under.
    pub fn secret_key(server: &Server) -> String {
        format!("{}@{}", server.user, server.url)
    }
}

/// What the engine keeps of Nextcloud in a session.
#[derive(Default)]
pub struct Nextcloud {
    pub prefs: NextcloudPrefs,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) rt: native::Runtime,
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{MemorySecrets, SecretStore};

#[cfg(not(target_arch = "wasm32"))]
impl Nextcloud {
    /// Where app passwords are kept beyond this session (the desktop app installs the system's
    /// credential store). Until then they live in memory only.
    pub fn set_secret_store(&mut self, store: std::sync::Arc<dyn SecretStore>) {
        self.rt.persistent = Some(store);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod native {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    use lightcraft_catalog::safe_file;
    use lightcraft_nextcloud::pair::{FolderPair, State, is_sidecar};
    use lightcraft_nextcloud::sync::{Event, Report, prepare, sync_pair};
    use lightcraft_nextcloud::{Client, Credentials, Options, RemotePath};
    use serde_json::{Value, json};

    use super::{ENV_PASSWORD, NextcloudPrefs, Pair, Server};
    use crate::activity::{Activity, Cancel, TaskHandle};

    /// Where app passwords are kept.
    pub trait SecretStore: Send + Sync {
        fn get(&self, key: &str) -> Option<String>;
        fn set(&self, key: &str, secret: &str) -> Result<(), String>;
        fn delete(&self, key: &str) -> Result<(), String>;
    }

    /// Secrets in memory, gone when the session ends.
    #[derive(Default)]
    pub struct MemorySecrets(Mutex<HashMap<String, String>>);

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    impl SecretStore for MemorySecrets {
        fn get(&self, key: &str) -> Option<String> {
            lock(&self.0).get(key).cloned()
        }

        fn set(&self, key: &str, secret: &str) -> Result<(), String> {
            lock(&self.0).insert(key.to_string(), secret.to_string());
            Ok(())
        }

        fn delete(&self, key: &str) -> Result<(), String> {
            lock(&self.0).remove(key);
            Ok(())
        }
    }

    /// A job's numbers and outcome, shared between the worker and whoever asks.
    #[derive(Default)]
    struct Shared {
        running: bool,
        done: u64,
        total: u64,
        detail: String,
        outcome: Option<Result<Value, String>>,
    }

    struct Job {
        id: u64,
        kind: &'static str,
        shared: Arc<Mutex<Shared>>,
        cancel: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    /// Reports a job's progress to the activity stack and to [`Runtime::status`].
    pub struct Progress {
        shared: Arc<Mutex<Shared>>,
        task: TaskHandle,
    }

    impl Progress {
        pub fn set(&self, done: u64, total: u64) {
            self.task.progress(done, total);
            let mut s = lock(&self.shared);
            s.done = done;
            s.total = total;
        }

        pub fn detail(&self, text: &str) {
            self.task.detail(text);
            lock(&self.shared).detail = text.chars().take(200).collect();
        }
    }

    /// The pairs' records, in the library folder or, without one, in memory.
    #[derive(Clone)]
    pub struct StateStore {
        dir: Option<PathBuf>,
        memory: Arc<Mutex<HashMap<String, State>>>,
    }

    impl StateStore {
        fn path(&self, id: &str) -> Option<PathBuf> {
            self.dir.as_ref().map(|d| d.join(format!("{id}.json")))
        }

        /// The record of a pair, and anything worth telling about reading it. A record that can't
        /// be read is kept as `<name>.corrupt-<time>` and the pair starts afresh (a full listing, and
        /// every file that differs is a conflict, never an overwrite).
        pub fn load(&self, id: &str) -> (State, Vec<String>) {
            let Some(path) = self.path(id) else { return (lock(&self.memory).get(id).cloned().unwrap_or_default(), vec![]) };
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (State::default(), vec![]),
                Err(e) => return (State::default(), vec![format!("the Nextcloud record {} can't be read ({e}); starting afresh", path.display())]),
            };
            match State::from_json(&text) {
                Ok((state, 0)) => (state, vec![]),
                Ok((state, dropped)) => (state, vec![format!("{dropped} unusable entries in the Nextcloud record were ignored")]),
                Err(e) => {
                    let secs = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    let keep = path.with_extension(format!("json.corrupt-{secs}"));
                    let _ = std::fs::rename(&path, &keep);
                    (State::default(), vec![format!("the Nextcloud record was damaged ({e}); kept as {}", keep.display())])
                }
            }
        }

        pub fn save(&self, id: &str, state: &State) -> Result<(), String> {
            let Some(path) = self.path(id) else {
                lock(&self.memory).insert(id.to_string(), state.clone());
                return Ok(());
            };
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
            }
            safe_file::write_atomic(&path, state.to_json().as_bytes()).map_err(|e| format!("can't save {}: {e}", path.display()))
        }

        pub fn remove(&self, id: &str) {
            lock(&self.memory).remove(id);
            if let Some(p) = self.path(id) {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// The state of Nextcloud in a session that is not part of the preferences.
    #[derive(Default)]
    pub struct Runtime {
        /// Passwords given this session, whether or not they are also remembered.
        session: MemorySecrets,
        /// Where passwords are remembered beyond the session, if the app installed a store.
        pub(super) persistent: Option<Arc<dyn SecretStore>>,
        job: Option<Job>,
        next_job: u64,
        memory_states: Arc<Mutex<HashMap<String, State>>>,
    }

    impl Runtime {
        pub fn persistent_secrets(&self) -> bool {
            self.persistent.is_some()
        }

        /// The app password of `server`: this session's, else the remembered one, else the
        /// environment's.
        pub fn secret(&self, server: &Server) -> Option<String> {
            let key = NextcloudPrefs::secret_key(server);
            self.session
                .get(&key)
                .or_else(|| self.persistent.as_ref().and_then(|p| p.get(&key)))
                .or_else(|| std::env::var(ENV_PASSWORD).ok().filter(|s| !s.is_empty()))
        }

        pub fn has_secret(&self, server: &Server) -> bool {
            self.secret(server).is_some()
        }

        /// Keep `secret` for this session and, with `remember` and a store, beyond it.
        pub fn set_secret(&self, server: &Server, secret: &str, remember: bool) -> Result<(), String> {
            let key = NextcloudPrefs::secret_key(server);
            self.session.set(&key, secret)?;
            match (&self.persistent, remember) {
                (Some(store), true) => store.set(&key, secret).map_err(|e| format!("the app password could not be remembered: {e}")),
                _ => Ok(()),
            }
        }

        pub fn forget_secret(&self, server: &Server) {
            let key = NextcloudPrefs::secret_key(server);
            let _ = self.session.delete(&key);
            if let Some(store) = &self.persistent {
                let _ = store.delete(&key);
            }
        }

        /// A client for `server`, with the larger upload limit that suits a server on the local network.
        pub fn client(&self, server: &Server) -> Result<Client, String> {
            let secret = self.secret(server).ok_or_else(|| format!("no app password for {}: connect again", server.name))?;
            let credentials = Credentials::new(&server.user, &secret).map_err(|e| e.to_string())?;
            let client = Client::new(&server.url, credentials.clone(), Options::default()).map_err(|e| e.to_string())?;
            if client.is_plain_http() {
                // on the local network there is no Cloudflare in the way; nginx allows 512 MB
                let options = Options { max_upload_bytes: 480 << 20, ..Options::default() };
                return Client::new(&server.url, credentials, options).map_err(|e| e.to_string());
            }
            Ok(client)
        }

        pub fn states(&self, library_dir: Option<&Path>) -> StateStore {
            StateStore { dir: library_dir.map(|d| d.join("Nextcloud")), memory: self.memory_states.clone() }
        }

        pub fn running(&self) -> bool {
            self.job.as_ref().is_some_and(|j| lock(&j.shared).running)
        }

        /// Run `work` on its own thread, as a row in the activity stack. One job at a time.
        pub fn start(
            &mut self,
            activity: &Activity,
            kind: &'static str,
            label: &str,
            work: impl FnOnce(&AtomicBool, &Progress) -> Result<Value, String> + Send + 'static,
        ) -> Result<u64, String> {
            if self.running() {
                return Err("a Nextcloud job is already running".into());
            }
            self.reap();
            self.next_job += 1;
            let id = self.next_job;
            let cancel = Arc::new(AtomicBool::new(false));
            let task = activity.start("nextcloud", label, Cancel::Flag(cancel.clone()));
            let shared = Arc::new(Mutex::new(Shared { running: true, ..Shared::default() }));
            let progress = Progress { shared: shared.clone(), task: task.handle() };
            let (flag, out) = (cancel.clone(), shared.clone());
            let handle = std::thread::Builder::new()
                .name("nextcloud".into())
                .spawn(move || {
                    let result = crate::guard::catch("the Nextcloud job", || work(&flag, &progress)).unwrap_or_else(Err);
                    let mut s = lock(&out);
                    s.outcome = Some(result);
                    s.running = false;
                    drop(s);
                    // the row goes away when the guard drops, after the outcome is visible
                    drop(task);
                })
                .map_err(|e| format!("can't start the Nextcloud job: {e}"))?;
            self.job = Some(Job { id, kind, shared, cancel, handle: Some(handle) });
            Ok(id)
        }

        /// Join a finished worker thread.
        fn reap(&mut self) {
            if let Some(job) = &mut self.job
                && !lock(&job.shared).running
                && let Some(h) = job.handle.take()
            {
                let _ = h.join();
            }
        }

        /// Wait for the job to end and give its outcome.
        pub fn wait(&mut self) -> Result<Value, String> {
            let Some(job) = &mut self.job else { return Err("no Nextcloud job".into()) };
            if let Some(h) = job.handle.take() {
                let _ = h.join();
            }
            lock(&job.shared).outcome.clone().unwrap_or_else(|| Err("the Nextcloud job ended without a result".into()))
        }

        pub fn cancel(&self) -> bool {
            self.job.as_ref().filter(|j| lock(&j.shared).running).is_some_and(|j| {
                j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                true
            })
        }

        /// The last or running job: `{id, kind, running, done, total, detail, ok?, result?, error?}`.
        pub fn status(&self) -> Value {
            let Some(j) = &self.job else { return Value::Null };
            let s = lock(&j.shared);
            let mut v = json!({"id": j.id, "kind": j.kind, "running": s.running, "done": s.done, "total": s.total, "detail": s.detail});
            match &s.outcome {
                Some(Ok(r)) => {
                    v["ok"] = json!(true);
                    v["result"] = r.clone();
                }
                Some(Err(e)) => {
                    v["ok"] = json!(false);
                    v["error"] = json!(e);
                }
                None => {}
            }
            v
        }
    }

    /// The paths of a list for a reply: the first `LIMIT`, and how many there were.
    pub fn paths_json(paths: &[RemotePath]) -> Value {
        const LIMIT: usize = 200;
        json!({"count": paths.len(), "paths": paths.iter().take(LIMIT).map(ToString::to_string).collect::<Vec<_>>()})
    }

    /// What a run did, for a reply.
    pub fn report_json(id: &str, r: &Report) -> Value {
        json!({
            "pair": id,
            "listed": r.listed,
            "uploaded": paths_json(&r.uploaded),
            "downloaded": paths_json(&r.downloaded),
            "adopted": r.adopted.len(),
            "forgotten": r.forgotten.len(),
            "conflicts": r.conflicts.iter().take(200).map(|(p, c)| json!({"path": p.to_string(), "why": c.to_string()})).collect::<Vec<_>>(),
            "conflictCount": r.conflicts.len(),
            "remoteDeleted": paths_json(&r.remote_deleted),
            "localDeleted": paths_json(&r.local_deleted),
            "failed": r.failed.iter().take(200).map(|(p, e)| json!({"path": p.to_string(), "error": e.to_string()})).collect::<Vec<_>>(),
            "failedCount": r.failed.len(),
            "skippedLocal": r.skipped_local,
            "clean": r.is_clean(),
        })
    }

    /// A pair ready to run on a worker thread.
    pub struct PairJob {
        pub pair: Pair,
        pub client: Client,
        pub states: StateStore,
    }

    /// Sync (or, with `preview`, only plan) each pair; the record of a pair is saved after its run,
    /// also when the run failed part-way, since the record is always up to date with what was done.
    pub fn run_pairs(jobs: Vec<PairJob>, preview: bool, cancel: &AtomicBool, progress: &Progress) -> Result<Value, String> {
        let mut results = Vec::new();
        let mut failed_run = None;
        for job in jobs {
            let id = job.pair.id.clone();
            let remote = RemotePath::parse(&job.pair.remote).map_err(|e| format!("{}: {e}", job.pair.remote))?;
            let pair = FolderPair::new(remote, PathBuf::from(&job.pair.local));
            let (mut state, warnings) = job.states.load(&id);
            let (mut planned, mut finished) = (0u64, 0u64);
            let mut on_event = |e: &Event<'_>| match e {
                Event::Listing(dir) => progress.detail(&dir.to_string()),
                Event::Planned { uploads, downloads, .. } => {
                    planned = (*uploads + *downloads) as u64;
                    progress.set(0, planned);
                }
                Event::Transfer { path, .. } => progress.detail(path.file_name().unwrap_or("")),
                Event::Finished { .. } => {
                    finished += 1;
                    progress.set(finished, planned);
                }
            };
            if preview {
                let prepared = prepare(&job.client, &pair, &state, is_sidecar, cancel, &mut on_event).map_err(|e| e.to_string())?;
                let p = &prepared.plan;
                results.push(json!({
                    "pair": id,
                    "preview": true,
                    "uploads": paths_json(&p.uploads().cloned().collect::<Vec<_>>()),
                    "downloads": paths_json(&p.downloads().cloned().collect::<Vec<_>>()),
                    "conflicts": p.conflicts().map(|(path, c)| json!({"path": path.to_string(), "why": c.to_string()})).take(200).collect::<Vec<_>>(),
                }));
                continue;
            }
            let outcome = sync_pair(&job.client, &pair, &mut state, is_sidecar, cancel, &mut on_event);
            let saved = job.states.save(&id, &state);
            match outcome {
                Ok(report) => {
                    let mut v = report_json(&id, &report);
                    if !warnings.is_empty() {
                        v["warnings"] = json!(warnings);
                    }
                    if let Err(e) = saved {
                        v["warnings"] = json!([e]);
                    }
                    v["localFolder"] = json!(job.pair.local);
                    results.push(v);
                }
                Err(e) => {
                    failed_run = Some(format!("{id}: {e}"));
                    break;
                }
            }
        }
        match failed_run {
            Some(e) => Err(e),
            None => Ok(json!({"pairs": results})),
        }
    }
}
