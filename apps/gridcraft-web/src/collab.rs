//! Live co-editing with the host page (Taskforce patch, branch `tf/collab`).
//!
//! The engine's [`gridcraft_engine::collab`] turns edits into effects and applies other people's;
//! this module carries them between the engine and a `?host=parent` page. Everything travels in
//! one message type, `gridcraft:collab`, with a `kind` and a JSON `body` (a string), on the same
//! same-origin `postMessage` channel the host bridge uses:
//!
//! - app → host `hello {protocol, build}` once the app is listening (before any file).
//! - host → app `start {}` after the file is open: co-editing begins on that workbook.
//!   app → host `started {hash}`.
//! - app → host `op {seq, effects}` for every edit (at most one per command, sent after the frame).
//! - host → app `ops [{kind: "confirm", seq} | {kind: "apply", effects}]` in the server's order:
//!   this person's batch `seq` came back, or other people's effects to apply.
//! - app → host `refused {message}` when a command was put back (not co-editable yet), and
//!   `error {message}` when effects could not be applied (the host then reloads).
//! - host → app `snapshot {id}`: app → host `snapshot {id, ok, hash, name?, bytes?}`, the
//!   workbook as an .xlsx, only while nothing is waiting for the server (so it is exactly the
//!   state at the last confirmed batch).
//! - host → app `hash {id}`: app → host `hash {id, hash, pending}`.
//! - with `?collabtest=1` only: host → app `exec {id, commands: [[id, params]...]}`, app → host
//!   `exec {id, results}` (the parity check runs scripts through it).
//!
//! Remote effects are applied between frames, never into the cell being typed in: an input for
//! that cell waits until the editor closes (and is then dropped if the person's own entry is
//! newer, as the engine skips fields with unconfirmed writes).

use std::cell::RefCell;
use std::rc::Rc;

use gridcraft_engine::collab::{self, Effect};
use gridcraft_ui_egui::SheetApp;
use serde::Deserialize;
use serde_json::{Value as Json, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;

const TYPE: &str = "gridcraft:collab";

/// The build these effects come from (the host shows it and pins sessions to it).
pub fn build_id() -> &'static str {
    option_env!("GRIDCRAFT_BUILD_ID").unwrap_or(concat!(env!("CARGO_PKG_VERSION"), "+tf"))
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum Item {
    Confirm { seq: u64 },
    Apply { effects: Vec<Effect> },
}

enum Incoming {
    Start,
    Ops(Vec<Item>),
    Snapshot(String),
    Hash(String),
    Exec(String, Vec<(String, Json)>),
}

pub struct Link {
    parent: web_sys::Window,
    origin: String,
    inbox: Rc<RefCell<Vec<Incoming>>>,
    /// Inputs for the cell being edited, held until the editor closes, latest per cell.
    deferred: Vec<Effect>,
    test_commands: bool,
}

impl Link {
    /// The co-editing channel to a same-origin parent page, or `None` when not framed by one.
    pub fn connect(ctx: egui::Context) -> Option<Rc<RefCell<Link>>> {
        let window = web_sys::window()?;
        let parent = window.parent().ok()??;
        // Not framed: the parent is the window itself.
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        let query = window.location().search().unwrap_or_default();
        let test_commands = web_sys::UrlSearchParams::new_with_str(&query).ok().and_then(|q| q.get("collabtest")).as_deref() == Some("1");
        let inbox: Rc<RefCell<Vec<Incoming>>> = Rc::default();
        let link =
            Rc::new(RefCell::new(Link { parent: parent.clone(), origin: origin.clone(), inbox: inbox.clone(), deferred: Vec::new(), test_commands }));
        let listener = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin {
                return;
            }
            let from_parent = event.source().is_some_and(|s| js_sys::Object::is(&s, &parent));
            if !from_parent {
                return;
            }
            let data = event.data();
            let field = |k: &str| js_sys::Reflect::get(&data, &k.into()).ok().and_then(|v| v.as_string());
            if field("type").as_deref() != Some(TYPE) {
                return;
            }
            let body: Json = field("body").and_then(|b| serde_json::from_str(&b).ok()).unwrap_or(Json::Null);
            let id = || body.get("id").and_then(Json::as_str).unwrap_or_default().chars().take(64).collect::<String>();
            let incoming = match field("kind").as_deref() {
                Some("start") => Incoming::Start,
                Some("ops") => match serde_json::from_value::<Vec<Item>>(body.clone()) {
                    Ok(items) => Incoming::Ops(items),
                    Err(e) => {
                        log::warn!("collab: unreadable ops: {e}");
                        return;
                    }
                },
                Some("snapshot") => Incoming::Snapshot(id()),
                Some("hash") => Incoming::Hash(id()),
                Some("exec") => {
                    let cmds = body
                        .get("commands")
                        .and_then(Json::as_array)
                        .map(|a| a.iter().filter_map(|c| Some((c.get(0)?.as_str()?.to_string(), c.get(1).cloned().unwrap_or(Json::Null)))).collect())
                        .unwrap_or_default();
                    Incoming::Exec(id(), cmds)
                }
                _ => return,
            };
            inbox.borrow_mut().push(incoming);
            ctx.request_repaint();
        });
        window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref()).ok()?;
        // Lives as long as the page.
        listener.forget();
        Some(link)
    }

    fn post(&self, kind: &str, body: &Json, bytes: Option<&[u8]>) {
        let msg = js_sys::Object::new();
        let set = |k: &str, v: JsValue| {
            let _ = js_sys::Reflect::set(&msg, &k.into(), &v);
        };
        set("type", TYPE.into());
        set("kind", kind.into());
        set("body", body.to_string().into());
        let result = match bytes {
            Some(b) => {
                let buffer = js_sys::Uint8Array::from(b).buffer();
                set("bytes", buffer.clone().into());
                self.parent.post_message_with_transfer(&msg, &self.origin, &js_sys::Array::of1(&buffer))
            }
            None => self.parent.post_message(&msg, &self.origin),
        };
        if let Err(e) = result {
            log::warn!("collab: couldn't post {kind}: {e:?}");
        }
    }

    pub fn post_hello(&self) {
        self.post("hello", &json!({"protocol": collab::PROTOCOL, "build": build_id(), "capabilities": ["effects", "snapshot", "hash"]}), None);
    }

    /// Between frames: start, apply, confirm, send this person's edits, answer requests.
    pub fn frame(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
        let incoming = std::mem::take(&mut *self.inbox.borrow_mut());
        let mut changed = false;
        for msg in incoming {
            match msg {
                Incoming::Start => {
                    if app.session.collab.is_none() {
                        match app.session.collab_start() {
                            Ok(()) => self.post("started", &json!({"hash": hash_text(app)}), None),
                            Err(e) => self.post("error", &json!({"message": e.to_string()}), None),
                        }
                    }
                }
                Incoming::Ops(items) => {
                    if app.session.collab.is_none() {
                        continue;
                    }
                    // This person's edits since the last frame go out first, so they are not
                    // mistaken for part of what arrives.
                    self.observe(app);
                    for item in items {
                        match item {
                            Item::Confirm { seq } => app.session.collab_confirm(seq),
                            Item::Apply { effects } => {
                                let effects = self.hold_edited(app, effects);
                                if effects.is_empty() {
                                    continue;
                                }
                                if let Err(e) = app.session.collab_apply(&effects) {
                                    log::error!("collab: couldn't apply effects: {e}");
                                    self.post("error", &json!({"message": e.to_string()}), None);
                                }
                                changed = true;
                            }
                        }
                    }
                }
                Incoming::Snapshot(id) => self.snapshot(app, &id),
                Incoming::Hash(id) => {
                    let pending = app.session.collab.as_ref().is_some_and(|c| c.has_pending() || !c.outbox.is_empty());
                    self.post("hash", &json!({"id": id, "hash": hash_text(app), "pending": pending}), None);
                }
                Incoming::Exec(id, cmds) => {
                    if !self.test_commands {
                        continue;
                    }
                    let results: Vec<Json> = cmds
                        .into_iter()
                        .map(|(cmd, params)| match app.session.execute(&cmd, params) {
                            Ok(v) => json!({"ok": v}),
                            Err(e) => json!({"error": e.to_string()}),
                        })
                        .collect();
                    self.post("exec", &json!({"id": id, "results": results, "hash": hash_text(app)}), None);
                    changed = true;
                }
            }
        }
        if app.session.collab.is_none() {
            return;
        }
        // The editor closed (or moved): what waited for it goes in now.
        if !self.deferred.is_empty() && !self.is_editing_any(app, &self.deferred) {
            let held = std::mem::take(&mut self.deferred);
            if let Err(e) = app.session.collab_apply(&held) {
                self.post("error", &json!({"message": e.to_string()}), None);
            }
            changed = true;
        }
        self.observe(app);
        let batches = app.session.collab.as_mut().map(collab::Collab::take_outbox).unwrap_or_default();
        for (seq, effects) in batches {
            self.post("op", &json!({"seq": seq, "effects": effects}), None);
        }
        if let Some(why) = app.session.collab.as_mut().and_then(|c| c.refused.take()) {
            self.post("refused", &json!({"message": why}), None);
            // The host says so in its own words (and language); the engine's message would be a
            // second, technical one for the same thing.
            if app.toast.as_ref().is_some_and(|(t, _)| t.contains(collab::NOT_YET)) {
                app.toast = None;
            }
            if app.message.as_ref().is_some_and(|(a, b)| a.contains(collab::NOT_YET) || b.contains(collab::NOT_YET)) {
                app.message = None;
            }
        }
        if changed {
            ctx.request_repaint();
        }
    }

    /// What changed outside a command (the engine observes after each command already); a change
    /// effects can't carry is put back.
    fn observe(&self, app: &mut SheetApp) {
        if let Err(what) = collab::observe(&mut app.session) {
            collab::revert(&mut app.session);
            let why = format!("{} ({what})", collab::NOT_YET);
            self.post("refused", &json!({"message": why}), None);
        }
    }

    /// The (sheet name, cell) being typed in, in the co-edited workbook.
    fn edited_cell(app: &SheetApp) -> Option<(String, String)> {
        let ed = app.editor.as_ref()?;
        let c = app.session.collab.as_ref()?;
        if ed.doc != c.doc {
            return None;
        }
        let d = app.session.documents().iter().find(|d| d.uid == c.doc)?;
        Some((d.wb.sheet(ed.sheet)?.name.clone(), ed.cell.a1()))
    }

    fn is_editing_any(&self, app: &SheetApp, effects: &[Effect]) -> bool {
        Self::edited_cell(app).is_some_and(|(sheet, cell)| effects.iter().any(|e| e.sheet == sheet && e.cell.as_deref() == Some(cell.as_str())))
    }

    /// `effects` without the input for the cell being typed in, which waits (replacing an older
    /// one that waited for the same cell).
    fn hold_edited(&mut self, app: &SheetApp, mut effects: Vec<Effect>) -> Vec<Effect> {
        let Some((sheet, cell)) = Self::edited_cell(app) else { return effects };
        for e in &mut effects {
            if e.sheet == sheet && e.cell.as_deref() == Some(cell.as_str()) && e.input.is_some() {
                self.deferred.retain(|d| !(d.sheet == e.sheet && d.cell == e.cell));
                self.deferred.push(Effect { input: e.input.take(), style: Default::default(), ..e.clone() });
            }
        }
        effects.retain(|e| e.input.is_some() || !e.style.is_empty());
        effects
    }

    fn snapshot(&self, app: &mut SheetApp, id: &str) {
        self.observe(app);
        let Some(c) = app.session.collab.as_ref() else {
            self.post("snapshot", &json!({"id": id, "ok": false, "reason": "not_started"}), None);
            return;
        };
        if c.has_pending() || !c.outbox.is_empty() || !self.deferred.is_empty() {
            self.post("snapshot", &json!({"id": id, "ok": false, "reason": "pending"}), None);
            return;
        }
        let Some(d) = app.session.documents().iter().find(|d| d.uid == c.doc) else { return };
        let name = if d.title.to_ascii_lowercase().ends_with(".xlsx") {
            d.title.clone()
        } else {
            format!("{}.xlsx", d.title.rsplit_once('.').map_or(d.title.as_str(), |(b, _)| b))
        };
        match gridcraft_engine::io::save_bytes(&d.wb, &name) {
            Ok(bytes) => self.post("snapshot", &json!({"id": id, "ok": true, "hash": hash_text(app), "name": name}), Some(&bytes)),
            Err(e) => self.post("snapshot", &json!({"id": id, "ok": false, "reason": e.to_string()}), None),
        }
    }
}

/// The content hash as 16 hex digits (a u64 does not fit a JS number).
fn hash_text(app: &SheetApp) -> String {
    let doc = app.session.collab.as_ref().map(|c| c.doc);
    let wb = app.session.documents().iter().find(|d| Some(d.uid) == doc).or_else(|| app.session.active()).map(|d| d.wb.clone());
    wb.map(|wb| format!("{:016x}", collab::content_hash(&wb))).unwrap_or_default()
}
