//! Nextcloud in the app: one dialog (File ▸ Nextcloud…) for the connection, the folder pairs and what
//! the last sync did, and the per-frame poll that lets the engine sync by itself (see
//! `lightcraft_engine::nextcloud` and `docs/nextcloud.md`).
//!
//! Everything goes through the engine's `nextcloud.*` commands, so the dialog, the control channel
//! and the MCP server do the same things. A job that talks to a server runs on its own thread with a
//! row in the activity stack; this dialog only shows its progress and outcome.

use std::time::Duration;

use serde_json::{Value, json};

use crate::LightcraftApp;
use crate::state::Dialog;
use crate::text_field::TextField;
use crate::theme::Tokens;

/// How often the frame loop looks at Nextcloud while a sync could be due (an automatic sync with
/// nothing else going on).
const IDLE_POLL: Duration = Duration::from_secs(5);
/// How often the dialog and a running job refresh.
const BUSY_POLL: Duration = Duration::from_millis(250);

/// `nextcloud.status`, or `null` when it can't be read.
fn status(app: &mut LightcraftApp) -> Value {
    app.session.execute("nextcloud.status", &json!({})).unwrap_or(Value::Null)
}

/// Open the dialog.
pub fn open(app: &mut LightcraftApp) -> Result<Value, String> {
    let status = status(app);
    app.ui.dialog = Some(Dialog::Nextcloud {
        url: String::new(),
        user: String::new(),
        password: Default::default(),
        remember: true,
        remote: "/Photos".into(),
        local: String::new(),
        status,
    });
    Ok(Value::Null)
}

/// Every frame: let the engine start an automatic sync and read in what arrived, and keep the open
/// dialog's picture of the job fresh.
pub fn poll(app: &mut LightcraftApp, ctx: &egui::Context) {
    let changed = app.session.nextcloud_tick();
    let open = matches!(app.ui.dialog, Some(Dialog::Nextcloud { .. }));
    let running = if open {
        let fresh = status(app);
        let running = fresh["job"]["running"].as_bool().unwrap_or(false);
        if let Some(Dialog::Nextcloud { status, .. }) = &mut app.ui.dialog {
            *status = fresh;
        }
        running
    } else {
        app.session.nextcloud.running()
    };
    if changed {
        ctx.request_repaint();
    }
    if open || running {
        ctx.request_repaint_after(BUSY_POLL);
    } else if app.session.nextcloud.prefs.auto_sync && !app.session.nextcloud.prefs.pairs.is_empty() {
        ctx.request_repaint_after(IDLE_POLL);
    }
}

/// What a click asked for, done after the dialog is drawn.
enum Act {
    Connect,
    Disconnect(String),
    SyncAll,
    SyncPair(String),
    Cancel,
    AddPair,
    RemovePair(String),
    ChooseFolder,
    Auto(bool),
    Resolve { pair: String, path: String, keep: &'static str },
}

fn count(v: &Value, key: &str) -> u64 {
    v[key]["count"].as_u64().unwrap_or(0)
}

/// The dialog's body.
pub fn body(app: &mut LightcraftApp, ui: &mut egui::Ui, dlg: &mut Dialog) {
    let Dialog::Nextcloud { url, user, password, remember, remote, local, status } = dlg else { return };
    let t = Tokens::get(ui.ctx());
    ui.set_width(520.0);
    let dim = |ui: &mut egui::Ui, s: &str| ui.label(egui::RichText::new(s).color(t.text_dim));
    let mut act = None;

    let servers = status["servers"].as_array().cloned().unwrap_or_default();
    let pairs = status["pairs"].as_array().cloned().unwrap_or_default();
    let job = status["job"].clone();
    let running = job["running"].as_bool().unwrap_or(false);

    dim(
        ui,
        crate::i18n::tr(
            "Keeps your edits (the XMP sidecars) in step with a folder on a Nextcloud, so a photo edited on one computer arrives on the others. Your originals are never uploaded or changed.",
        ),
    );
    ui.add_space(4.0);

    // ---- the server
    ui.label(egui::RichText::new(crate::i18n::tr("Server")).font(t.semibold(12.5)).color(t.text));
    if let Some(sv) = servers.first() {
        let name = sv["name"].as_str().unwrap_or("");
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(name).strong());
            dim(ui, &format!("{} · {}", sv["user"].as_str().unwrap_or(""), sv["url"].as_str().unwrap_or("")));
            let r = ui.add_enabled(!running, egui::Button::new(crate::i18n::tr("Disconnect")));
            crate::widgets::register(ui.ctx(), "button:ncDisconnect", r.rect);
            if r.clicked() {
                act = Some(Act::Disconnect(name.to_string()));
            }
        });
        if sv["plainHttp"].as_bool().unwrap_or(false) {
            ui.label(
                egui::RichText::new(crate::i18n::tr("This connection is not encrypted (http://). Use it only on your own network.")).color(t.caution),
            );
        }
        if !sv["hasPassword"].as_bool().unwrap_or(true) {
            ui.label(egui::RichText::new(crate::i18n::tr("No app password is stored for this server: connect again below.")).color(t.caution));
        }
    }
    let field = |ui: &mut egui::Ui, label: &str, widget: &str, text: &mut String, hint: &str, secret: bool| {
        ui.horizontal(|ui| {
            ui.add_sized([120.0, 20.0], egui::Label::new(egui::RichText::new(crate::i18n::tr(label)).color(t.text_label)));
            TextField::singleline(widget, text).hint(crate::i18n::tr(hint)).width(340.0).password(secret).show(ui);
        });
    };
    if servers.is_empty() {
        field(ui, "Address", "ncUrl", url, "https://cloud.example.org", false);
        field(ui, "User name", "ncUser", user, "your Nextcloud user name", false);
        field(ui, "App password", "ncPassword", &mut password.0, "Settings ▸ Security ▸ Create new app password", true);
        let persistent = status["rememberPasswords"].as_bool().unwrap_or(false);
        ui.horizontal(|ui| {
            if persistent {
                let r = ui.checkbox(remember, crate::i18n::tr("Remember the app password on this computer"));
                crate::widgets::register(ui.ctx(), "check:ncRemember", r.rect);
            } else {
                dim(ui, crate::i18n::tr("The app password is kept until LightCraft is closed."));
            }
        });
        let ready = !url.trim().is_empty() && !user.trim().is_empty() && !password.0.is_empty() && !running;
        let r = ui.add_enabled(ready, egui::Button::new(crate::i18n::tr("Connect")));
        crate::widgets::register(ui.ctx(), "button:ncConnect", r.rect);
        if r.clicked() {
            act = Some(Act::Connect);
        }
    }

    // ---- folder pairs
    ui.add_space(6.0);
    ui.label(egui::RichText::new(crate::i18n::tr("Folders")).font(t.semibold(12.5)).color(t.text));
    if pairs.is_empty() {
        dim(ui, crate::i18n::tr("No folder is kept in step yet."));
    }
    for p in &pairs {
        let id = p["id"].as_str().unwrap_or("").to_string();
        ui.horizontal(|ui| {
            ui.label(format!("{}  ↔  {}", p["remote"].as_str().unwrap_or(""), p["local"].as_str().unwrap_or("")));
            let r = ui.add_enabled(!running, egui::Button::new(crate::i18n::tr("Sync")));
            crate::widgets::register(ui.ctx(), format!("button:ncSync:{id}"), r.rect);
            if r.clicked() {
                act = Some(Act::SyncPair(id.clone()));
            }
            let r = ui.add_enabled(!running, egui::Button::new(crate::i18n::tr("Remove")));
            crate::widgets::register(ui.ctx(), format!("button:ncRemove:{id}"), r.rect);
            if r.clicked() {
                act = Some(Act::RemovePair(id.clone()));
            }
        });
    }
    if !servers.is_empty() {
        field(ui, "Server folder", "ncRemote", remote, "/Photos", false);
        ui.horizontal(|ui| {
            ui.add_sized([120.0, 20.0], egui::Label::new(egui::RichText::new(crate::i18n::tr("On this computer")).color(t.text_label)));
            TextField::singleline("ncLocal", local).hint(crate::i18n::tr("the same folder here")).width(250.0).show(ui);
            let r = ui.add(egui::Button::new(crate::i18n::tr("Choose…")));
            crate::widgets::register(ui.ctx(), "button:ncChoose", r.rect);
            if r.clicked() {
                act = Some(Act::ChooseFolder);
            }
        });
        let ready = !remote.trim().is_empty() && !local.trim().is_empty();
        let r = ui.add_enabled(ready, egui::Button::new(crate::i18n::tr("Add folder")));
        crate::widgets::register(ui.ctx(), "button:ncAdd", r.rect);
        if r.clicked() {
            act = Some(Act::AddPair);
        }
    }

    // ---- automatic sync
    if !pairs.is_empty() {
        ui.add_space(6.0);
        let mut auto = status["autoSync"].as_bool().unwrap_or(false);
        let r = ui.checkbox(&mut auto, crate::i18n::tr("Sync by itself (a few seconds after an edit, and every few minutes)"));
        crate::widgets::register(ui.ctx(), "check:ncAuto", r.rect);
        if r.changed() {
            act = Some(Act::Auto(auto));
        }
    }

    // ---- the job
    ui.add_space(6.0);
    if running {
        ui.horizontal(|ui| {
            ui.spinner();
            let (done, total) = (job["done"].as_u64().unwrap_or(0), job["total"].as_u64().unwrap_or(0));
            let what = job["detail"].as_str().unwrap_or("");
            if total > 0 {
                ui.label(format!("{what}  {done} / {total}"));
            } else {
                ui.label(what);
            }
            let r = ui.button(crate::i18n::tr("Cancel"));
            crate::widgets::register(ui.ctx(), "button:ncCancel", r.rect);
            if r.clicked() {
                act = Some(Act::Cancel);
            }
        });
    } else if !job.is_null() {
        match job["ok"].as_bool() {
            Some(false) => {
                ui.label(egui::RichText::new(job["error"].as_str().unwrap_or("")).color(t.reject));
            }
            Some(true) => {
                let result = &job["result"];
                match job["kind"].as_str().unwrap_or("") {
                    "connect" => {
                        ui.label(crate::i18n::tr_format!("Signed in as {user}.", user = result["user"].as_str().unwrap_or("")));
                    }
                    "sync" => {
                        let pairs_done = result["pairs"].as_array().cloned().unwrap_or_default();
                        let sent: u64 = pairs_done.iter().map(|p| count(p, "uploaded")).sum();
                        let got: u64 = pairs_done.iter().map(|p| count(p, "downloaded")).sum();
                        ui.label(crate::i18n::tr_format!("Sent {sent}, received {got}.", sent = sent, got = got));
                        if let Some(read) = result["applied"]["read"].as_u64().or_else(|| job["applied"]["read"].as_u64()).filter(|n| *n > 0) {
                            dim(ui, &crate::i18n::tr_format!("{n} photos updated from what arrived.", n = read));
                        }
                        for p in &pairs_done {
                            let id = p["pair"].as_str().unwrap_or("").to_string();
                            for f in p["failed"].as_array().into_iter().flatten().take(5) {
                                ui.label(
                                    egui::RichText::new(format!("{}: {}", f["path"].as_str().unwrap_or(""), f["error"].as_str().unwrap_or("")))
                                        .color(t.reject),
                                );
                            }
                            for (i, c) in p["conflicts"].as_array().into_iter().flatten().enumerate().take(20) {
                                let path = c["path"].as_str().unwrap_or("").to_string();
                                ui.label(egui::RichText::new(format!("{path} — {}", c["why"].as_str().unwrap_or(""))).color(t.caution));
                                ui.horizontal(|ui| {
                                    let r = ui.add(egui::Button::new(crate::i18n::tr("Keep this computer's")));
                                    crate::widgets::register(ui.ctx(), format!("button:ncKeepLocal:{i}"), r.rect);
                                    if r.clicked() {
                                        act = Some(Act::Resolve { pair: id.clone(), path: path.clone(), keep: "local" });
                                    }
                                    let r = ui.add(egui::Button::new(crate::i18n::tr("Keep the server's")));
                                    crate::widgets::register(ui.ctx(), format!("button:ncKeepServer:{i}"), r.rect);
                                    if r.clicked() {
                                        act = Some(Act::Resolve { pair: id.clone(), path, keep: "server" });
                                    }
                                });
                            }
                            if p["remoteDeleted"]["count"].as_u64().unwrap_or(0) + p["localDeleted"]["count"].as_u64().unwrap_or(0) > 0 {
                                dim(ui, crate::i18n::tr("Some files were deleted on one side. Nothing is deleted for you."));
                            }
                        }
                    }
                    _ => {}
                }
            }
            None => {}
        }
    }
    if !pairs.is_empty() {
        let r = ui.add_enabled(!running, egui::Button::new(crate::i18n::tr("Sync now")));
        crate::widgets::register(ui.ctx(), "button:ncSyncAll", r.rect);
        if r.clicked() {
            act = Some(Act::SyncAll);
        }
    }

    // ---- what was clicked
    let Some(act) = act else { return };
    let ctx = ui.ctx().clone();
    let result = match act {
        Act::Connect => {
            let r = app
                .session
                .execute("nextcloud.connect", &json!({"url": url.trim(), "user": user.trim(), "password": password.0, "remember": *remember}));
            if r.is_ok() {
                // the password is in the engine now; it does not stay in the dialog
                password.0.clear();
            }
            r
        }
        Act::Disconnect(name) => app.session.execute("nextcloud.disconnect", &json!({"name": name})),
        Act::SyncAll => app.session.execute("nextcloud.sync", &json!({})),
        Act::SyncPair(id) => app.session.execute("nextcloud.sync", &json!({"pair": id})),
        Act::Cancel => app.session.execute("nextcloud.cancel", &json!({})),
        Act::AddPair => {
            let name = servers.first().and_then(|s| s["name"].as_str()).unwrap_or("").to_string();
            let r = app.session.execute("nextcloud.pair.add", &json!({"server": name, "remote": remote.trim(), "local": local.trim()}));
            if r.is_ok() {
                local.clear();
            }
            r
        }
        Act::RemovePair(id) => app.session.execute("nextcloud.pair.remove", &json!({"id": id})),
        Act::ChooseFolder => {
            if let Some(f) = app.services.pick_folder.as_mut().and_then(|f| f()) {
                *local = f;
            }
            Ok(Value::Null)
        }
        Act::Auto(on) => app.session.execute("nextcloud.settings", &json!({"auto": on})),
        Act::Resolve { pair, path, keep } => app.session.execute("nextcloud.resolve", &json!({"pair": pair, "path": path, "keep": keep})),
    };
    if let Err(e) = result {
        app.toast(&ctx, e.to_string());
    }
    ctx.request_repaint();
}
