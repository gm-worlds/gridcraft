//! Taskforce cell kinds (`gridcraft_tf_ext`): commands that mark cells as a rating, chips, date
//! or rich-text cell, clear the mark, and set a rating from a click on a star.
//!
//! The mark is the cell's number format, so these commands change only cell style and cell input:
//! both replicate in co-editing and travel with the cell.

use gridcraft_core::{CellRef, Value};
use gridcraft_model::NumFmt;
use gridcraft_tf_ext::{self as tf, Kind};
use serde_json::{Value as Json, json};

use super::{CommandSpec, bad, cell_param_on, cmd, edit, has_doc, str_param, target_ranges, target_sheet};
use crate::{Result, Session};

/// Why a command was refused in a workbook the host opened read-only.
pub const READ_ONLY: &str = "This workbook is open read-only";

/// Refuses a command that would change a workbook the host opened read-only. Looking around,
/// selecting, view settings, copying and opening stay available; undo and redo are refused with
/// the rest, as there is nothing of this person's to undo.
pub(crate) fn guard_read_only(s: &Session, spec: &CommandSpec) -> Result<()> {
    if s.tf_read_only && (spec.undoable || matches!(spec.id, "edit.undo" | "edit.redo" | "edit.cut" | "file.save" | "file.saveAs")) {
        return Err(crate::EngineError::Disabled(spec.id.to_string(), READ_ONLY.into()));
    }
    Ok(())
}

/// Most cells one command marks.
const MAX_CELLS: u64 = 100_000;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!("tf.kind.set", "Turn Cell Into", [], None, "{range?, kind: rating|chips|date|rich, max?, set?}", has_doc, kind_set),
        cmd!("tf.kind.clear", "Turn Cell Into Plain Cell", [], None, "{range?}", has_doc, kind_clear),
        cmd!("tf.rating.click", "Set Rating", [], None, "{cell, star}", has_doc, rating_click),
        cmd!(query "tf.kind.get", "Cell Kind", [], None, "{cell?}", has_doc, kind_get),
    ]
}

fn kind_param(p: &Json) -> Result<Kind> {
    let name = str_param(p, "kind").ok_or_else(|| bad("tf.kind.set", "missing `kind`"))?;
    match name {
        "rating" => {
            let max = match p.get("max") {
                None | Some(Json::Null) => u64::from(tf::RATING_DEFAULT_MAX),
                Some(v) => v.as_u64().ok_or_else(|| bad("tf.kind.set", "`max` must be a whole number"))?,
            };
            if !(1..=u64::from(tf::RATING_MAX_LIMIT)).contains(&max) {
                return Err(bad("tf.kind.set", format!("`max` must be 1 to {}", tf::RATING_MAX_LIMIT)));
            }
            Ok(Kind::Rating { max: u8::try_from(max).unwrap_or(tf::RATING_DEFAULT_MAX) })
        }
        "chips" => {
            let set = str_param(p, "set").ok_or_else(|| bad("tf.kind.set", "missing `set`"))?;
            if !tf::valid_set_id(set) {
                return Err(bad("tf.kind.set", "`set` must be 1 to 32 letters, digits, `_` or `-`"));
            }
            Ok(Kind::Chips { set: set.to_string() })
        }
        "date" => Ok(Kind::Date),
        "rich" => Ok(Kind::Rich),
        other => Err(bad("tf.kind.set", format!("unknown kind `{other}`"))),
    }
}

fn kind_set(s: &mut Session, p: &Json) -> Result<Json> {
    let kind = kind_param(p)?;
    set_format(s, p, "tf.kind.set", move |_| Some(kind.format_code()))
}

fn kind_clear(s: &mut Session, p: &Json) -> Result<Json> {
    set_format(s, p, "tf.kind.clear", |code| tf::is_marked(code).then(|| "General".to_string()))
}

/// Gives every target cell the number format `next` returns for its current code (`None`: leave it).
fn set_format(s: &mut Session, p: &Json, id: &'static str, next: impl Fn(&str) -> Option<String>) -> Result<Json> {
    let ranges = target_ranges(s, p)?;
    let si = target_sheet(s, p)?;
    if ranges.iter().map(|r| r.count()).fold(0u64, u64::saturating_add) > MAX_CELLS {
        return Err(bad(id, "range too large"));
    }
    edit(s, |cx| {
        let mut changed = 0u64;
        for r in &ranges {
            for c in r.iter() {
                let st = cx.wb.sheet(si).map(|sh| sh.style_id(c)).unwrap_or_default();
                let Some(code) = next(cx.wb.styles.get(st).num_fmt.as_str()) else { continue };
                let new = cx.wb.styles.derive(st, |style| style.num_fmt = NumFmt::new(&code));
                let sh = cx.sheet_mut(si)?;
                let mut cell = sh.cell(c).cloned().unwrap_or_default();
                cell.style = new;
                sh.set_cell(c, cell);
                cx.touch(si, c);
                changed += 1;
            }
        }
        Ok(json!({ "cells": changed }))
    })
}

fn rating_click(s: &mut Session, p: &Json) -> Result<Json> {
    let si = target_sheet(s, p)?;
    let c = cell_param_on(s, p, "cell", si)?.ok_or_else(|| bad("tf.rating.click", "missing `cell`"))?;
    let star = p.get("star").and_then(Json::as_u64).ok_or_else(|| bad("tf.rating.click", "missing `star`"))?;
    let star = u8::try_from(star.min(u64::from(tf::RATING_MAX_LIMIT))).unwrap_or(tf::RATING_MAX_LIMIT);
    edit(s, |cx| {
        let Some((max, current, formula)) = cx.wb.sheet(si).and_then(|sh| {
            let Some(Kind::Rating { max }) = tf::classify(cx.wb.styles.get(sh.style_id(c)).num_fmt.as_str()) else { return None };
            let cell = sh.cell(c);
            let current = match cell.map(|x| &x.value) {
                Some(Value::Number(n)) => tf::rating_stars(*n, max),
                _ => 0,
            };
            Some((max, current, cell.is_some_and(|x| x.formula.is_some() || x.py.is_some())))
        }) else {
            return Err(bad("tf.rating.click", "not a rating cell"));
        };
        if formula {
            return Err(bad("tf.rating.click", "this rating comes from a formula"));
        }
        let next = tf::rating_after_click(current, star, max);
        let sh = cx.sheet_mut(si)?;
        let mut cell = sh.cell(c).cloned().unwrap_or_default();
        cell.value = if next == 0 { Value::Empty } else { Value::Number(f64::from(next)) };
        sh.set_cell(c, cell);
        cx.touch(si, c);
        Ok(json!({ "cell": c.a1(), "value": next }))
    })
}

fn kind_get(s: &mut Session, p: &Json) -> Result<Json> {
    let si = target_sheet(s, p)?;
    let d = s.doc()?;
    let c: CellRef = cell_param_on(s, p, "cell", si)?.unwrap_or(d.selection.active);
    let Some(sh) = d.wb.sheet(si) else { return Ok(Json::Null) };
    Ok(kind_json(d.wb.styles.get(sh.style_id(c)).num_fmt.as_str()))
}

/// A kind as the host sees it: `{kind, max?, set?}`, or `null` for a plain cell.
pub fn kind_json(code: &str) -> Json {
    match tf::classify(code) {
        Some(Kind::Rating { max }) => json!({ "kind": "rating", "max": max }),
        Some(Kind::Chips { set }) => json!({ "kind": "chips", "set": set }),
        Some(kind) => json!({ "kind": kind.name() }),
        None => Json::Null,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    fn fmt(s: &mut Session, cell: &str) -> String {
        s.execute("cell.get", json!({ "cell": cell })).unwrap()["style"]["num_fmt"].as_str().unwrap().to_string()
    }

    #[test]
    fn rating_cell_is_marked_clicked_and_cleared() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        s.execute("tf.kind.set", json!({ "range": "A1:A3", "kind": "rating" })).unwrap();
        assert_eq!(s.execute("tf.kind.get", json!({ "cell": "A2" })).unwrap(), json!({ "kind": "rating", "max": 5 }));
        assert_eq!(s.execute("tf.rating.click", json!({ "cell": "A1", "star": 4 })).unwrap()["value"], 4);
        assert_eq!(s.execute("cell.get", json!({ "cell": "A1" })).unwrap()["value"], 4.0);
        // The same star again clears it.
        assert_eq!(s.execute("tf.rating.click", json!({ "cell": "A1", "star": 4 })).unwrap()["value"], 0);
        assert!(s.execute("cell.get", json!({ "cell": "A1" })).unwrap()["value"].is_null());
        // A star beyond the maximum is the maximum.
        assert_eq!(s.execute("tf.rating.click", json!({ "cell": "A1", "star": 99 })).unwrap()["value"], 5);
        s.execute("tf.kind.clear", json!({ "range": "A1:B9" })).unwrap();
        assert_eq!(fmt(&mut s, "A1"), "General");
        assert_eq!(s.execute("cell.get", json!({ "cell": "A1" })).unwrap()["value"], 5.0);
    }

    #[test]
    fn read_only_refuses_changes_and_keeps_looking_around() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        s.execute("cell.set", json!({ "cell": "A1", "input": "kept" })).unwrap();
        s.tf_read_only = true;
        for (id, p) in [
            ("cell.set", json!({ "cell": "A1", "input": "x" })),
            ("home.bold", json!({ "range": "A1" })),
            ("tf.kind.set", json!({ "range": "A1", "kind": "rating" })),
            ("edit.undo", json!({})),
            ("edit.clearContents", json!({ "range": "A1" })),
            ("file.save", json!({})),
        ] {
            let e = s.execute(id, p).unwrap_err().to_string();
            assert!(e.contains(super::READ_ONLY), "{id}: {e}");
        }
        assert_eq!(s.execute("cell.get", json!({ "cell": "A1" })).unwrap()["value"], "kept");
        s.execute("selection.set", json!({ "cell": "C3" })).unwrap();
        s.execute("edit.copy", json!({ "range": "A1" })).unwrap();
        s.tf_read_only = false;
        s.execute("cell.set", json!({ "cell": "A1", "input": "x" })).unwrap();
    }

    #[test]
    fn clear_leaves_other_formats_alone() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        s.execute("home.formatCells", json!({ "range": "B1", "style": { "num_fmt": "0.00" } })).unwrap();
        s.execute("tf.kind.clear", json!({ "range": "A1:C3" })).unwrap();
        assert_eq!(fmt(&mut s, "B1"), "0.00");
    }

    #[test]
    fn bad_params_are_errors_not_panics() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        for p in [
            json!({}),
            json!({ "kind": "slider" }),
            json!({ "kind": "rating", "max": 0 }),
            json!({ "kind": "rating", "max": 99 }),
            json!({ "kind": "rating", "max": "x" }),
            json!({ "kind": "chips" }),
            json!({ "kind": "chips", "set": "a b" }),
            json!({ "kind": "rating", "range": "A:XFD" }),
        ] {
            assert!(s.execute("tf.kind.set", p.clone()).is_err(), "{p}");
        }
        assert!(s.execute("tf.rating.click", json!({ "cell": "A1", "star": 1 })).is_err());
        assert!(s.execute("tf.rating.click", json!({ "cell": "A1" })).is_err());
    }

    #[test]
    fn a_formula_rating_is_not_overwritten() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        s.execute("cell.set", json!({ "cell": "A1", "input": "=1+2" })).unwrap();
        s.execute("tf.kind.set", json!({ "range": "A1", "kind": "rating" })).unwrap();
        assert!(s.execute("tf.rating.click", json!({ "cell": "A1", "star": 5 })).is_err());
        assert_eq!(s.execute("cell.get", json!({ "cell": "A1" })).unwrap()["formula"], "=1+2");
    }

    #[test]
    fn marked_cells_survive_sort_and_xlsx() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        s.execute("range.setValues", json!({ "range": "A1", "values": [["b", 2], ["a", 4]] })).unwrap();
        s.execute("tf.kind.set", json!({ "range": "B1:B2", "kind": "rating", "max": 7 })).unwrap();
        s.execute("tf.kind.set", json!({ "range": "C1", "kind": "chips", "set": "status" })).unwrap();
        s.execute("tf.kind.set", json!({ "range": "D1", "kind": "date" })).unwrap();
        s.execute("data.sortAscending", json!({ "range": "A1:B2" })).unwrap();
        assert_eq!(s.execute("cell.get", json!({ "cell": "B1" })).unwrap()["value"], 4.0);
        assert_eq!(s.execute("tf.kind.get", json!({ "cell": "B1" })).unwrap(), json!({ "kind": "rating", "max": 7 }));
        let d = s.doc().unwrap();
        let bytes = gridcraft_xlsx::write_xlsx(&d.wb).unwrap();
        let (back, _) = gridcraft_xlsx::read_xlsx(&bytes).unwrap();
        let sh = back.sheet(0).unwrap();
        let code = |a1: &str| back.styles.get(sh.style_id(gridcraft_core::CellRef::parse(a1).unwrap())).num_fmt.as_str().to_string();
        assert_eq!(super::kind_json(&code("B2")), json!({ "kind": "rating", "max": 7 }));
        assert_eq!(super::kind_json(&code("C1")), json!({ "kind": "chips", "set": "status" }));
        assert_eq!(super::kind_json(&code("D1")), json!({ "kind": "date" }));
    }
}
