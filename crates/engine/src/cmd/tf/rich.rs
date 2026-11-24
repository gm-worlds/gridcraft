//! Rich text commands: character formatting inside one cell's text.
//!
//! The formatting lives in the workbook's [`gridcraft_model::RichStrings`], by text. Offsets are
//! UTF-16 code units, the unit of the file format (and of a JavaScript host).

use std::collections::HashSet;
use std::sync::Arc;

use gridcraft_core::{CellRef, Value};
use gridcraft_model::Workbook;
use gridcraft_model::rich::{RichPatch, RichRun, all_of, patch_runs, utf16_len};
use gridcraft_tf_ext as tf;
use serde_json::{Value as Json, json};

use super::super::{CommandSpec, bad, cell_param_on, cmd, edit, has_doc, target_sheet};
use crate::{Result, Session};

/// Excel's limit on a cell's text, in characters.
const MAX_TEXT: usize = 32_767;
/// Above this many formatted texts, the ones no cell holds any more are forgotten after an edit.
const PRUNE_ABOVE: usize = 256;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "tf.rich.set",
            "Set Rich Text",
            [],
            None,
            "{cell, text?, runs: [{start, end, bold?, italic?, underline?, strike?, color?: \"rrggbb\"}]} (offsets in UTF-16 units)",
            has_doc,
            rich_set
        ),
        cmd!(
            "tf.rich.format",
            "Format Part Of A Cell's Text",
            [],
            None,
            "{cell, start, end, toggle?: bold|italic|underline|strike, bold?, italic?, underline?, strike?, color?: \"rrggbb\"|null}",
            has_doc,
            rich_format
        ),
        cmd!(query "tf.rich.get", "Rich Text Of A Cell", [], None, "{cell?} -> {cell, text, runs}", has_doc, rich_get),
    ]
}

fn run_json(r: &RichRun) -> Json {
    let mut o = serde_json::Map::new();
    o.insert("start".into(), json!(r.start));
    o.insert("end".into(), json!(r.end));
    for (name, on) in [("bold", r.bold), ("italic", r.italic), ("underline", r.underline), ("strike", r.strike)] {
        if on {
            o.insert(name.into(), Json::Bool(true));
        }
    }
    if let Some([red, green, blue]) = r.color {
        o.insert("color".into(), json!(format!("{red:02x}{green:02x}{blue:02x}")));
    }
    Json::Object(o)
}

pub fn runs_json(runs: &[RichRun]) -> Json {
    Json::Array(runs.iter().map(run_json).collect())
}

fn offset(v: &Json, key: &str, id: &str) -> Result<u32> {
    let n = v.get(key).and_then(Json::as_u64).ok_or_else(|| bad(id, format!("missing or bad `{key}`")))?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

fn flag(v: &Json, key: &str, id: &str) -> Result<Option<bool>> {
    match v.get(key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad(id, format!("`{key}` must be true or false"))),
    }
}

fn color(v: &Json, id: &str) -> Result<Option<Option<[u8; 3]>>> {
    match v.get("color") {
        None => Ok(None),
        Some(Json::Null) => Ok(Some(None)),
        Some(c) => c.as_str().and_then(tf::parse_hex).map(|rgb| Some(Some(rgb))).ok_or_else(|| bad(id, "`color` must be \"rrggbb\"")),
    }
}

/// Runs from untrusted JSON. The model normalizes them (order, overlap, range) when they are set.
pub fn runs_param(v: Option<&Json>, id: &str) -> Result<Vec<RichRun>> {
    let list = v.and_then(Json::as_array).ok_or_else(|| bad(id, "missing `runs`"))?;
    if list.len() > gridcraft_model::rich::MAX_RUNS {
        return Err(bad(id, "too many runs"));
    }
    list.iter()
        .map(|item| {
            Ok(RichRun {
                start: offset(item, "start", id)?,
                end: offset(item, "end", id)?,
                bold: flag(item, "bold", id)?.unwrap_or(false),
                italic: flag(item, "italic", id)?.unwrap_or(false),
                underline: flag(item, "underline", id)?.unwrap_or(false),
                strike: flag(item, "strike", id)?.unwrap_or(false),
                color: color(item, id)?.flatten(),
            })
        })
        .collect()
}

/// The text a cell holds as a typed constant; an error for a formula or anything that is not text.
fn constant_text(wb: &Workbook, si: usize, c: CellRef, id: &str) -> Result<String> {
    let sh = wb.sheet(si).ok_or_else(|| bad(id, "no such sheet"))?;
    let cell = sh.cell(c);
    if cell.is_some_and(|x| x.formula.is_some() || x.py.is_some()) {
        return Err(bad(id, "this text comes from a formula"));
    }
    match cell.map(|x| &x.value) {
        Some(Value::Text(t)) => Ok(t.to_string()),
        None | Some(Value::Empty) => Ok(String::new()),
        Some(_) => Err(bad(id, "the cell does not hold text")),
    }
}

/// Forgets the formatting of texts no cell holds any more, once there are many.
fn prune(wb: &mut Workbook) {
    if wb.rich.len() <= PRUNE_ABOVE {
        return;
    }
    let used: HashSet<Arc<str>> = wb
        .sheets
        .iter()
        .flat_map(|sh| sh.cells.iter())
        .filter_map(|(_, cell)| match &cell.value {
            Value::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    Arc::make_mut(&mut wb.rich).retain_texts(|text| used.contains(text));
}

fn store(wb: &mut Workbook, text: &str, runs: Vec<RichRun>, id: &str) -> Result<()> {
    if !Arc::make_mut(&mut wb.rich).set(text, runs) {
        return Err(bad(id, "this workbook holds too many formatted texts"));
    }
    prune(wb);
    Ok(())
}

/// Sets a cell's text formatting, and its text when `text` is given.
fn rich_set(s: &mut Session, p: &Json) -> Result<Json> {
    const ID: &str = "tf.rich.set";
    let si = target_sheet(s, p)?;
    let c = cell_param_on(s, p, "cell", si)?.ok_or_else(|| bad(ID, "missing `cell`"))?;
    let runs = runs_param(p.get("runs"), ID)?;
    let new_text = match p.get("text") {
        None | Some(Json::Null) => None,
        Some(t) => Some(t.as_str().ok_or_else(|| bad(ID, "`text` must be text"))?.to_string()),
    };
    if new_text.as_ref().is_some_and(|t| t.chars().count() > MAX_TEXT) {
        return Err(bad(ID, "the text is too long for a cell"));
    }
    edit(s, |cx| {
        let held = constant_text(&cx.wb, si, c, ID)?;
        let text = match new_text {
            Some(text) if text != held => {
                let sh = cx.sheet_mut(si)?;
                let mut cell = sh.cell(c).cloned().unwrap_or_default();
                cell.value = if text.is_empty() { Value::Empty } else { Value::Text(text.as_str().into()) };
                sh.set_cell(c, cell);
                text
            }
            _ => held,
        };
        store(&mut cx.wb, &text, runs, ID)?;
        cx.touch(si, c);
        Ok(json!({ "cell": c.a1(), "text": text, "runs": runs_json(cx.wb.rich.runs(&text)) }))
    })
}

/// Formats a stretch of a cell's text: what Bold does while part of the text is selected.
fn rich_format(s: &mut Session, p: &Json) -> Result<Json> {
    const ID: &str = "tf.rich.format";
    let si = target_sheet(s, p)?;
    let c = cell_param_on(s, p, "cell", si)?.ok_or_else(|| bad(ID, "missing `cell`"))?;
    let (start, end) = (offset(p, "start", ID)?, offset(p, "end", ID)?);
    let mut patch = RichPatch {
        bold: flag(p, "bold", ID)?,
        italic: flag(p, "italic", ID)?,
        underline: flag(p, "underline", ID)?,
        strike: flag(p, "strike", ID)?,
        color: color(p, ID)?,
    };
    let toggle = match p.get("toggle") {
        None | Some(Json::Null) => None,
        Some(t) => Some(
            t.as_str()
                .filter(|t| ["bold", "italic", "underline", "strike"].contains(t))
                .ok_or_else(|| bad(ID, "`toggle` must be bold, italic, underline or strike"))?,
        ),
    };
    if toggle.is_none() && patch.is_empty() {
        return Err(bad(ID, "nothing to change"));
    }
    edit(s, |cx| {
        let text = constant_text(&cx.wb, si, c, ID)?;
        if start >= end || start >= utf16_len(&text) {
            return Err(bad(ID, "`start` and `end` must mark a stretch of the text"));
        }
        let current = cx.wb.rich.runs(&text).to_vec();
        // A toggle turns the property on unless the whole stretch has it already.
        match toggle {
            Some("bold") => patch.bold = Some(!all_of(&text, &current, start, end, |r| r.bold)),
            Some("italic") => patch.italic = Some(!all_of(&text, &current, start, end, |r| r.italic)),
            Some("underline") => patch.underline = Some(!all_of(&text, &current, start, end, |r| r.underline)),
            Some("strike") => patch.strike = Some(!all_of(&text, &current, start, end, |r| r.strike)),
            _ => {}
        }
        let runs = patch_runs(&text, &current, start, end, &patch);
        store(&mut cx.wb, &text, runs, ID)?;
        cx.touch(si, c);
        Ok(json!({ "cell": c.a1(), "text": text, "runs": runs_json(cx.wb.rich.runs(&text)) }))
    })
}

fn rich_get(s: &mut Session, p: &Json) -> Result<Json> {
    let si = target_sheet(s, p)?;
    let d = s.doc()?;
    let c: CellRef = cell_param_on(s, p, "cell", si)?.unwrap_or(d.selection.active);
    let text = match d.wb.sheet(si).map(|sh| sh.value(c)) {
        Some(Value::Text(t)) => t.to_string(),
        _ => String::new(),
    };
    Ok(json!({ "cell": c.a1(), "text": text, "runs": runs_json(d.wb.rich.runs(&text)) }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use crate::Session;

    #[test]
    fn part_of_a_text_is_formatted_and_survives_xlsx() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        s.execute("cell.set", json!({ "cell": "A1", "input": "Ship by Friday" })).unwrap();
        let r = s.execute("tf.rich.format", json!({ "cell": "A1", "start": 0, "end": 4, "toggle": "bold" })).unwrap();
        assert_eq!(r["runs"], json!([{ "start": 0, "end": 4, "bold": true }]));
        s.execute("tf.rich.format", json!({ "cell": "A1", "start": 8, "end": 14, "italic": true, "color": "#C0392B" })).unwrap();
        // Toggling bold over a stretch that is only partly bold turns it on for all of it; again turns it off.
        let r = s.execute("tf.rich.format", json!({ "cell": "A1", "start": 0, "end": 7, "toggle": "bold" })).unwrap();
        assert_eq!(r["runs"][0], json!({ "start": 0, "end": 7, "bold": true }));
        let r = s.execute("tf.rich.format", json!({ "cell": "A1", "start": 0, "end": 7, "toggle": "bold" })).unwrap();
        assert_eq!(r["runs"], json!([{ "start": 8, "end": 14, "italic": true, "color": "c0392b" }]));
        // Undo brings the bold back; redo takes it away again.
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(s.execute("tf.rich.get", json!({ "cell": "A1" })).unwrap()["runs"][0], json!({ "start": 0, "end": 7, "bold": true }));
        // A cell with the same text shows the same formatting; another text has none.
        s.execute("cell.set", json!({ "cell": "B1", "input": "Ship by Friday" })).unwrap();
        s.execute("cell.set", json!({ "cell": "B2", "input": "Other" })).unwrap();
        assert_eq!(s.execute("tf.rich.get", json!({ "cell": "B1" })).unwrap()["runs"].as_array().unwrap().len(), 2);
        assert_eq!(s.execute("tf.rich.get", json!({ "cell": "B2" })).unwrap()["runs"], json!([]));
        let bytes = gridcraft_xlsx::write_xlsx(&s.doc().unwrap().wb).unwrap();
        let (back, _) = gridcraft_xlsx::read_xlsx(&bytes).unwrap();
        assert_eq!(back.rich, s.doc().unwrap().wb.rich);
    }

    #[test]
    fn set_replaces_text_and_runs_together() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        let r = s
            .execute("tf.rich.set", json!({ "cell": "A1", "text": "a😀b", "runs": [{ "start": 3, "end": 4, "strike": true }, { "start": 0, "end": 1, "underline": true }, { "start": 9, "end": 12, "bold": true }] }))
            .unwrap();
        assert_eq!(r["runs"], json!([{ "start": 0, "end": 1, "underline": true }, { "start": 3, "end": 4, "strike": true }]));
        assert_eq!(s.execute("cell.get", json!({ "cell": "A1" })).unwrap()["value"], "a😀b");
        // No runs: plain text again.
        assert_eq!(s.execute("tf.rich.set", json!({ "cell": "A1", "runs": [] })).unwrap()["runs"], json!([]));
        assert!(s.doc().unwrap().wb.rich.is_empty());
    }

    #[test]
    fn bad_params_and_wrong_cells_are_errors() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        s.execute("cell.set", json!({ "cell": "A1", "input": "text" })).unwrap();
        s.execute("cell.set", json!({ "cell": "A2", "input": "=1+1" })).unwrap();
        s.execute("cell.set", json!({ "cell": "A3", "input": "12" })).unwrap();
        for p in [
            json!({}),
            json!({ "cell": "A1" }),
            json!({ "cell": "A1", "start": 0, "end": 2 }),
            json!({ "cell": "A1", "start": 2, "end": 2, "bold": true }),
            json!({ "cell": "A1", "start": 9, "end": 12, "bold": true }),
            json!({ "cell": "A1", "start": 0, "end": 2, "toggle": "blink" }),
            json!({ "cell": "A1", "start": 0, "end": 2, "bold": "yes" }),
            json!({ "cell": "A1", "start": 0, "end": 2, "color": "red" }),
            json!({ "cell": "A1", "start": -1, "end": 2, "bold": true }),
            json!({ "cell": "A2", "start": 0, "end": 1, "bold": true }),
            json!({ "cell": "A3", "start": 0, "end": 1, "bold": true }),
            json!({ "cell": "B9", "start": 0, "end": 1, "bold": true }),
        ] {
            assert!(s.execute("tf.rich.format", p.clone()).is_err(), "{p}");
        }
        for p in [
            json!({ "cell": "A1" }),
            json!({ "cell": "A1", "runs": 3 }),
            json!({ "cell": "A1", "runs": [{ "start": 0 }] }),
            json!({ "cell": "A1", "text": 4, "runs": [] }),
            json!({ "cell": "A2", "runs": [] }),
        ] {
            assert!(s.execute("tf.rich.set", p.clone()).is_err(), "{p}");
        }
        assert!(s.doc().unwrap().wb.rich.is_empty());
    }
}
