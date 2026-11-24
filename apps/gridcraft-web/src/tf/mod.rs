//! The Taskforce host API: what a `?host=parent` page can ask of the sheet and what it is told.
//!
//! It runs beside the host bridge (which carries the file) and the co-editing channel, over a
//! private `MessagePort`, so no other listener on the page sees it:
//!
//! - app → host, on start: `{type: "gridcraft:tf", kind: "hello", body}` with `body` the JSON
//!   text of `{protocol, build, capabilities}` ([`proto::hello`]).
//! - host → app: `{type: "gridcraft:tf", kind: "port"}` with one transferred port. A later port
//!   replaces the earlier one (the host page reloaded).
//! - on the port, host → app: `{id, method, params?}` (`params` JSON text); app → host:
//!   `{id, ok, result?}` (`result` JSON text) or `{id, ok: false, error}`.
//! - on the port, app → host: `{event, data}` (`data` JSON text): `changed`, `selection`, `view`
//!   ([`proto::Seen::events_since`]), sent after the frame in which the state changed.
//!
//! Methods: `tf.hello`, `tf.features`, `tf.goto {cell, sheet?}`, `tf.cellRect {cell}`,
//! `tf.save {}` (the save the Save button does, without a key press), `tf.readOnly {on}`,
//! `tf.theme {dark?}`; any other name is a control method (`cell.get`, `sheet.read`,
//! `engine.execute`…, see `docs/control-protocol.md`).

pub mod proto;

#[cfg(target_arch = "wasm32")]
pub use link::Host;

#[cfg(target_arch = "wasm32")]
mod link {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gridcraft_engine::core::CellRef;
    use gridcraft_ui_egui::SheetApp;
    use gridcraft_ui_egui::control::{self, Outcome};
    use serde_json::{Value as Json, json};
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::JsValue;

    use super::proto::{self, Request, Seen};

    const TYPE: &str = "gridcraft:tf";

    pub struct Host {
        parent: web_sys::Window,
        origin: String,
        port: Rc<RefCell<Option<web_sys::MessagePort>>>,
        inbox: Rc<RefCell<Vec<Request>>>,
        seen: Seen,
    }

    fn get(data: &JsValue, key: &str) -> Option<JsValue> {
        js_sys::Reflect::get(data, &key.into()).ok().filter(|v| !v.is_undefined() && !v.is_null())
    }

    impl Host {
        /// The host channel to a same-origin parent page, or `None` when not framed by one.
        pub fn connect(ctx: egui::Context) -> Option<Rc<RefCell<Host>>> {
            let window = web_sys::window()?;
            let parent = window.parent().ok()??;
            if js_sys::Object::is(&parent, &window) {
                return None;
            }
            let origin = window.location().origin().ok()?;
            let port: Rc<RefCell<Option<web_sys::MessagePort>>> = Rc::default();
            let inbox: Rc<RefCell<Vec<Request>>> = Rc::default();
            let host = Rc::new(RefCell::new(Host {
                parent: parent.clone(),
                origin: origin.clone(),
                port: port.clone(),
                inbox: inbox.clone(),
                seen: Seen::default(),
            }));
            let listener = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                    return;
                }
                let data = event.data();
                let text = |k: &str| get(&data, k).and_then(|v| v.as_string());
                if text("type").as_deref() != Some(TYPE) || text("kind").as_deref() != Some("port") {
                    return;
                }
                let Some(new_port) = event.ports().get(0).dyn_into::<web_sys::MessagePort>().ok() else { return };
                let (inbox, wake) = (inbox.clone(), ctx.clone());
                let on_message = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                    let data = event.data();
                    let request = Request::parse(
                        get(&data, "id").and_then(|v| v.as_f64()),
                        get(&data, "method").and_then(|v| v.as_string()),
                        get(&data, "params").and_then(|v| v.as_string()),
                    );
                    if let Some(request) = request {
                        // A page cannot queue without end: beyond this the oldest wait is long anyway.
                        if inbox.borrow().len() < 4096 {
                            inbox.borrow_mut().push(request);
                        }
                        wake.request_repaint();
                    }
                });
                new_port.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
                // One per port the host hands over; a host hands over one per page load.
                on_message.forget();
                if let Some(old) = port.borrow_mut().replace(new_port) {
                    old.set_onmessage(None);
                    old.close();
                }
                ctx.request_repaint();
            });
            window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref()).ok()?;
            // Lives as long as the page.
            listener.forget();
            Some(host)
        }

        /// Tells the host this build speaks the host API, before any file is open.
        pub fn post_hello(&self) {
            let msg = js_sys::Object::new();
            for (k, v) in [("type", TYPE.to_string()), ("kind", "hello".to_string()), ("body", proto::hello(crate::collab::build_id()).to_string())] {
                let _ = js_sys::Reflect::set(&msg, &k.into(), &v.into());
            }
            if let Err(e) = self.parent.post_message(&msg, &self.origin) {
                log::warn!("tf: couldn't post hello: {e:?}");
            }
        }

        fn send(&self, fields: &[(&str, JsValue)]) {
            let Some(port) = self.port.borrow().clone() else { return };
            let msg = js_sys::Object::new();
            for (k, v) in fields {
                let _ = js_sys::Reflect::set(&msg, &(*k).into(), v);
            }
            if let Err(e) = port.post_message(&msg) {
                log::warn!("tf: couldn't post to the host: {e:?}");
            }
        }

        /// After the app's frame: answers the host's requests, then reports what changed.
        pub fn frame(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
            let requests: Vec<Request> = std::mem::take(&mut *self.inbox.borrow_mut());
            for request in requests {
                let id = JsValue::from_f64(request.id);
                match answer(app, ctx, &request) {
                    Ok(result) => self.send(&[("id", id), ("ok", true.into()), ("result", result.to_string().into())]),
                    Err(error) => self.send(&[("id", id), ("ok", false.into()), ("error", error.into())]),
                }
            }
            if self.port.borrow().is_none() {
                return;
            }
            let now = observe(app, ctx);
            for (event, data) in now.events_since(&self.seen) {
                self.send(&[("event", event.into()), ("data", data.to_string().into())]);
            }
            self.seen = now;
        }
    }

    fn cell_param(p: &Json) -> Result<CellRef, String> {
        p.get("cell").and_then(Json::as_str).and_then(CellRef::parse).ok_or_else(|| "missing or bad `cell`".to_string())
    }

    /// A cell's rectangle on screen in CSS pixels `[x, y, w, h]`, or `None` when it is not in view.
    fn css_rect(app: &SheetApp, ctx: &egui::Context, c: CellRef) -> Option<[f32; 4]> {
        let geo = app.grid.geo.as_ref()?;
        let d = app.session.active()?;
        let sh = d.wb.active()?;
        let r = geo.screen_cell_rect(sh, c);
        let cells = app.grid.cells_rect.unwrap_or(geo.cells);
        if geo.hidden_under_frozen(sh, c) || !cells.intersects(r) {
            return None;
        }
        // egui points are CSS pixels times the interface zoom.
        let k = ctx.zoom_factor();
        Some([r.left() * k, r.top() * k, r.width() * k, r.height() * k])
    }

    fn answer(app: &mut SheetApp, ctx: &egui::Context, request: &Request) -> Result<Json, String> {
        let p = &request.params;
        match request.method.as_str() {
            "tf.hello" => Ok(proto::hello(crate::collab::build_id())),
            "tf.features" => Ok(proto::features(&app.session)),
            "tf.goto" => {
                let c = cell_param(p)?;
                if let Some(sheet) = p.get("sheet").filter(|s| !s.is_null()) {
                    app.run("sheet.activate", json!({ "sheet": sheet }))?;
                }
                app.run("selection.set", json!({ "cell": c.a1() }))?;
                app.grid.ensure_visible = true;
                Ok(json!({ "cell": c.a1() }))
            }
            "tf.cellRect" => {
                let c = cell_param(p)?;
                Ok(json!({ "cell": c.a1(), "rect": css_rect(app, ctx, c) }))
            }
            "tf.save" => {
                if app.session.tf_read_only {
                    return Err(gridcraft_engine::cmd::tf::READ_ONLY.to_string());
                }
                app.run("file.save", json!({}))?;
                Ok(Json::Null)
            }
            "tf.readOnly" => {
                let on = p.get("on").and_then(Json::as_bool).ok_or("missing `on`")?;
                app.session.tf_read_only = on;
                Ok(json!({ "readOnly": on }))
            }
            "tf.theme" => {
                if let Some(dark) = p.get("dark").and_then(Json::as_bool) {
                    app.ui.dark = dark;
                    app.ui.system_theme = false;
                }
                Ok(json!({ "dark": app.ui.dark }))
            }
            other if other.starts_with("tf.") && gridcraft_engine::find_command(other).is_none() => Err(format!("unknown method `{other}`")),
            other => match control::handle(app, ctx, other, p) {
                Outcome::Done(reply) => {
                    if reply.get("ok").and_then(Json::as_bool) == Some(true) {
                        Ok(reply.get("result").cloned().unwrap_or(Json::Null))
                    } else {
                        Err(reply.get("error").and_then(Json::as_str).unwrap_or("failed").to_string())
                    }
                }
                Outcome::Screenshot { .. } => Err("screenshots go through window.gridcraft.screenshot".to_string()),
            },
        }
    }

    fn observe(app: &SheetApp, ctx: &egui::Context) -> Seen {
        let Some(d) = app.session.active() else { return Seen::default() };
        let active = d.selection.active;
        let sheet = d.wb.active_sheet();
        let sh = d.wb.active();
        let range = d.selection.ranges.last().map(|r| r.a1()).unwrap_or_else(|| active.a1());
        let kind = sh.map(|sh| gridcraft_engine::cmd::tf::kind_json(d.wb.styles.get(sh.style_id(active)).num_fmt.as_str())).unwrap_or(Json::Null);
        // Half pixels: finer steps would send an event for every frame of a smooth scroll's tail.
        let half = |v: f32| (v * 2.0).round() as i32;
        let view = app.grid.geo.as_ref().map(|g| [half(g.scroll.x), half(g.scroll.y), (g.z * 1000.0).round() as i32]).unwrap_or_default();
        Seen {
            doc: d.uid,
            revision: d.revision,
            dirty: d.is_dirty(),
            sheet,
            sheet_name: sh.map(|s| s.name.clone()).unwrap_or_default(),
            active: active.a1(),
            range,
            kind,
            rect: css_rect(app, ctx, active).map(|r| [half(r[0]), half(r[1]), half(r[2]), half(r[3])]),
            view,
        }
    }
}
