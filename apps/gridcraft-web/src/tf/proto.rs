//! The host API's messages and the parts of it that need no browser: what a build announces, what
//! a workbook uses, and which state changes become events. Compiled natively too, for its tests.

use gridcraft_engine::Session;
use gridcraft_engine::tf_ext;
use serde_json::{Value as Json, json};

/// Version of the host API. A host that knows a lower number still works: methods and events are
/// only ever added, and an unknown method answers `ok: false` instead of closing the channel.
pub const PROTOCOL: u32 = 1;

/// What this build can do for a host, by name. The host opens a workbook read-only when the
/// workbook uses a feature that is not in the list of the build it runs in.
pub const CAPABILITIES: &[&str] =
    &["tf-host-1", "tf-save-now", "tf-read-only", "tf-goto", "tf-theme", "tf-theme-2", "tf-rich-toggle", tf_ext::CAPABILITY];

/// Most cells looked at when listing what a workbook uses.
const FEATURE_SCAN_LIMIT: usize = 2_000_000;

pub fn hello(build: &str) -> Json {
    json!({ "protocol": PROTOCOL, "build": build, "capabilities": CAPABILITIES })
}

/// A request from the host: `{id, method, params}` with `params` a JSON string (or absent).
#[derive(Debug, PartialEq)]
pub struct Request {
    pub id: f64,
    pub method: String,
    pub params: Json,
}

impl Request {
    /// `None` for anything that is not a request (the channel ignores it).
    pub fn parse(id: Option<f64>, method: Option<String>, params: Option<String>) -> Option<Request> {
        let id = id.filter(|n| n.is_finite())?;
        let method = method.filter(|m| !m.is_empty() && m.len() <= 128)?;
        let params = match params {
            None => json!({}),
            Some(text) => match serde_json::from_str::<Json>(&text) {
                Ok(Json::Null) => json!({}),
                Ok(v) => v,
                Err(_) => return None,
            },
        };
        Some(Request { id, method, params })
    }
}

/// The features the active workbook uses that a build must have to edit it safely, and markers
/// this build does not know (from a newer build: such a workbook stays read-only here).
pub fn features(session: &Session) -> Json {
    let mut uses_kinds = false;
    let mut unknown: Vec<String> = Vec::new();
    if let Some(d) = session.active() {
        // Kinds are marked on styles, so the style table is all there is to look at: a workbook
        // has few styles however many cells it has.
        for code in d.wb.styles.iter().map(|(_, s)| s.num_fmt.as_str().to_string()).take(FEATURE_SCAN_LIMIT) {
            if let Some(m) = tf_ext::unknown_marker(&code) {
                if !unknown.iter().any(|u| u == m) && unknown.len() < 16 {
                    unknown.push(m.to_string());
                }
            } else if tf_ext::is_marked(&code) {
                uses_kinds = true;
            }
        }
    }
    let mut features: Vec<&str> = Vec::new();
    if uses_kinds {
        features.push(tf_ext::CAPABILITY);
    }
    json!({ "features": features, "unknown": unknown })
}

/// What the host is told about between frames. Two equal values mean nothing to send.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen {
    pub doc: u64,
    pub revision: u64,
    pub dirty: bool,
    pub sheet: usize,
    pub sheet_name: String,
    pub active: String,
    pub range: String,
    /// The active cell's kind, as `tf.kind.get` gives it.
    pub kind: Json,
    /// The active cell on screen `[x, y, w, h]` in half CSS pixels; `None` while it is scrolled away.
    pub rect: Option<[i32; 4]>,
    /// Scroll position and zoom, rounded: a change means pins over cells must move.
    pub view: [i32; 3],
}

impl Seen {
    /// The events to send after `before`, in order: `changed`, `selection`, `view`.
    pub fn events_since(&self, before: &Seen) -> Vec<(&'static str, Json)> {
        let mut out = Vec::new();
        let new_doc = self.doc != before.doc;
        if new_doc || self.revision != before.revision || self.dirty != before.dirty {
            out.push(("changed", json!({ "revision": self.revision, "dirty": self.dirty, "opened": new_doc })));
        }
        let moved = new_doc
            || self.sheet != before.sheet
            || self.sheet_name != before.sheet_name
            || self.active != before.active
            || self.range != before.range;
        if moved || self.kind != before.kind {
            out.push((
                "selection",
                json!({ "sheet": self.sheet_name, "sheetIndex": self.sheet, "cell": self.active, "range": self.range, "kind": self.kind, "rect": self.rect }),
            ));
        }
        if moved || self.view != before.view || self.rect != before.rect {
            out.push(("view", json!({ "scroll": [self.view[0], self.view[1]], "zoom": self.view[2], "rect": self.rect })));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_or_are_ignored() {
        assert_eq!(
            Request::parse(Some(1.0), Some("tf.goto".into()), Some("{\"cell\":\"B2\"}".into())),
            Some(Request { id: 1.0, method: "tf.goto".into(), params: json!({"cell": "B2"}) })
        );
        assert_eq!(Request::parse(Some(2.0), Some("tf.hello".into()), None).map(|r| r.params), Some(json!({})));
        assert_eq!(Request::parse(Some(2.0), Some("tf.hello".into()), Some("null".into())).map(|r| r.params), Some(json!({})));
        assert_eq!(Request::parse(None, Some("tf.hello".into()), None), None);
        assert_eq!(Request::parse(Some(f64::NAN), Some("tf.hello".into()), None), None);
        assert_eq!(Request::parse(Some(1.0), None, None), None);
        assert_eq!(Request::parse(Some(1.0), Some(String::new()), None), None);
        assert_eq!(Request::parse(Some(1.0), Some("x".repeat(200)), None), None);
        assert_eq!(Request::parse(Some(1.0), Some("tf.goto".into()), Some("{not json".into())), None);
    }

    #[test]
    fn hello_names_every_capability_once() {
        let h = hello("0.19.1+tf.9");
        let caps: Vec<&str> = h["capabilities"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
        assert!(caps.contains(&"tf-kinds-1") && caps.contains(&"tf-read-only") && caps.contains(&"tf-save-now"));
        let mut sorted = caps.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), caps.len());
        assert_eq!(h["protocol"], 1);
    }

    #[test]
    fn features_list_what_the_workbook_uses() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        assert_eq!(features(&s), json!({ "features": [], "unknown": [] }));
        s.execute("tf.kind.set", json!({ "range": "A1", "kind": "rating" })).unwrap();
        assert_eq!(features(&s), json!({ "features": ["tf-kinds-1"], "unknown": [] }));
        s.execute("home.formatCells", json!({ "range": "B1", "style": { "num_fmt": "0;\"tf-slider:3\";0;@" } })).unwrap();
        assert_eq!(features(&s)["unknown"], json!(["tf-slider:3"]));
    }

    #[test]
    fn only_changes_become_events() {
        let a = Seen { doc: 1, revision: 4, active: "A1".into(), range: "A1".into(), sheet_name: "Sheet1".into(), ..Seen::default() };
        assert!(a.events_since(&a).is_empty());
        let typed = Seen { revision: 5, dirty: true, ..a.clone() };
        assert_eq!(typed.events_since(&a).iter().map(|e| e.0).collect::<Vec<_>>(), ["changed"]);
        let moved = Seen { active: "B2".into(), range: "B2".into(), rect: Some([10, 20, 60, 20]), ..a.clone() };
        assert_eq!(moved.events_since(&a).iter().map(|e| e.0).collect::<Vec<_>>(), ["selection", "view"]);
        let scrolled = Seen { view: [0, 40, 100], ..a.clone() };
        assert_eq!(scrolled.events_since(&a).iter().map(|e| e.0).collect::<Vec<_>>(), ["view"]);
        let opened = Seen { doc: 2, ..a.clone() };
        let events = opened.events_since(&a);
        assert_eq!(events.iter().map(|e| e.0).collect::<Vec<_>>(), ["changed", "selection", "view"]);
        assert_eq!(events[0].1["opened"], true);
    }
}
