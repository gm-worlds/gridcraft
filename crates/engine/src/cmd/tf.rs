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

pub mod rich;

pub fn specs() -> Vec<CommandSpec> {
    let mut all = rich::specs();
    all.extend(base_specs());
    all
}

fn base_specs() -> Vec<CommandSpec> {
    vec![
        cmd!("tf.kind.set", "Turn Cell Into", [], None, "{range?, kind: rating|chips|date|rich, max?, set?}", has_doc, kind_set),
        cmd!("tf.kind.clear", "Turn Cell Into Plain Cell", [], None, "{range?}", has_doc, kind_clear),
        cmd!("tf.rating.click", "Set Rating", [], None, "{cell, star}", has_doc, rating_click),
        cmd!(query "tf.kind.get", "Cell Kind", [], None, "{cell?}", has_doc, kind_get),
        cmd!("tf.chips.defineSet", "Define Chip Options", [], None, "{set, options: [{label, color?: \"rrggbb\"}]}", has_doc, chips_define_set),
        cmd!("tf.chips.toggle", "Toggle Chip", [], None, "{cell, label}", has_doc, chips_toggle),
        cmd!("tf.date.set", "Set Date", [], None, "{cell, date: \"yyyy-mm-dd\" | null}", has_doc, date_set),
        cmd!(query "tf.chips.sets", "Chip Option Sets", [], None, "{} -> {sets: {id: [{label, color}]}}", has_doc, chips_sets),
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
    if let Kind::Chips { set } = &kind {
        // `options` defines the set in the same step; without it the set must exist already.
        if p.get("options").is_some_and(|o| !o.is_null()) {
            chips_define_set(s, p)?;
        } else if chip_options(&s.doc()?.wb, set).is_none() {
            return Err(bad("tf.kind.set", format!("no chip options named `{set}` (pass `options`, or run tf.chips.defineSet first)")));
        }
    }
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

/// `yyyy-mm-dd` as `(year, month, day)`: exactly that shape, a real month, and a day the month can
/// have (its length is checked against the workbook's calendar by the caller).
pub fn parse_iso_date(text: &str) -> Option<(i32, u32, u32)> {
    let b = text.as_bytes();
    if b.len() != 10 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| text.get(r).filter(|t| t.bytes().all(|c| c.is_ascii_digit())).and_then(|t| t.parse::<u32>().ok());
    let (y, m, d) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then_some((i32::try_from(y).ok()?, m, d))
}

/// Puts a day in a date cell (what the date picker does), or empties it with `date: null`.
fn date_set(s: &mut Session, p: &Json) -> Result<Json> {
    let si = target_sheet(s, p)?;
    let c = cell_param_on(s, p, "cell", si)?.ok_or_else(|| bad("tf.date.set", "missing `cell`"))?;
    let date = match p.get("date") {
        None => return Err(bad("tf.date.set", "missing `date`")),
        Some(Json::Null) => None,
        Some(v) => Some(v.as_str().and_then(parse_iso_date).ok_or_else(|| bad("tf.date.set", "`date` must be yyyy-mm-dd"))?),
    };
    edit(s, |cx| {
        let sys = cx.wb.date_system;
        let Some(sh) = cx.wb.sheet(si) else { return Err(bad("tf.date.set", "no such sheet")) };
        if tf::classify(cx.wb.styles.get(sh.style_id(c)).num_fmt.as_str()) != Some(Kind::Date) {
            return Err(bad("tf.date.set", "not a date cell"));
        }
        let value = match date {
            None => Value::Empty,
            Some((y, m, d)) => {
                // The serial helper reads years below 1900 the way DATE() does (1800 becomes 3700):
                // a picked day is the year it says, or nothing.
                if !(1900..=9999).contains(&y) {
                    return Err(bad("tf.date.set", "that date is outside the calendar"));
                }
                if d > gridcraft_core::date::days_in_month_in(sys, y, m) {
                    return Err(bad("tf.date.set", "no such day in that month"));
                }
                let serial = gridcraft_core::date::serial_from_ymd(sys, i64::from(y), i64::from(m), i64::from(d))
                    .ok_or_else(|| bad("tf.date.set", "that date is outside the calendar"))?;
                Value::Number(serial)
            }
        };
        let sh = cx.sheet_mut(si)?;
        let mut cell = sh.cell(c).cloned().unwrap_or_default();
        // Picking a day replaces whatever was there, a formula included: the cell now holds that day.
        cell.formula = None;
        cell.py = None;
        cell.value = value;
        sh.set_cell(c, cell);
        cx.touch(si, c);
        Ok(json!({ "cell": c.a1(), "date": date.map(|(y, m, d)| format!("{y:04}-{m:02}-{d:02}")) }))
    })
}

/// The options of the set `set`, or `None` when the workbook has no such set.
pub fn chip_options(wb: &gridcraft_model::Workbook, set: &str) -> Option<Vec<tf::ChipOption>> {
    let name = tf::set_name(set);
    let defined = wb.names.iter().find(|n| n.scope.is_none() && n.name.eq_ignore_ascii_case(&name))?;
    tf::parse_set(&tf::string_constant(&defined.formula)?)
}

fn options_param(p: &Json, id: &'static str) -> Result<Vec<tf::ChipOption>> {
    let list = p.get("options").and_then(Json::as_array).ok_or_else(|| bad(id, "missing `options`"))?;
    if list.len() > tf::SET_OPTIONS_LIMIT {
        return Err(bad(id, format!("at most {} options", tf::SET_OPTIONS_LIMIT)));
    }
    let mut out: Vec<tf::ChipOption> = Vec::new();
    for (i, item) in list.iter().enumerate() {
        // An option is `{label, color?}` or just the label.
        let label = item.get("label").and_then(Json::as_str).or_else(|| item.as_str()).map(str::trim).unwrap_or_default();
        if !tf::valid_chip_label(label) {
            return Err(bad(id, format!("option {} needs a label without `,` `;` `|` or quotes", i + 1)));
        }
        if out.iter().any(|o| o.label == label) {
            return Err(bad(id, format!("`{label}` is listed twice")));
        }
        let color = match item.get("color").and_then(Json::as_str) {
            Some(hex) => tf::parse_hex(hex).ok_or_else(|| bad(id, format!("`{hex}` is not a colour (rrggbb)")))?,
            None => tf::CHIP_PALETTE.get(i % tf::CHIP_PALETTE.len()).copied().unwrap_or_default(),
        };
        out.push(tf::ChipOption { label: label.to_string(), color });
    }
    Ok(out)
}

fn chips_define_set(s: &mut Session, p: &Json) -> Result<Json> {
    let set = str_param(p, "set").ok_or_else(|| bad("tf.chips.defineSet", "missing `set`"))?.to_string();
    if !tf::valid_set_id(&set) {
        return Err(bad("tf.chips.defineSet", "`set` must be 1 to 32 letters, digits, `_` or `-`"));
    }
    let options = options_param(p, "tf.chips.defineSet")?;
    let name = tf::set_name(&set);
    let formula = format!("\"{}\"", tf::format_set(&options));
    edit(s, |cx| {
        match cx.wb.names.iter_mut().find(|n| n.scope.is_none() && n.name.eq_ignore_ascii_case(&name)) {
            Some(existing) => {
                existing.formula = formula;
                existing.hidden = true;
            }
            None => cx.wb.names.push(gridcraft_model::DefinedName { name, scope: None, formula, comment: String::new(), hidden: true }),
        }
        Ok(json!({ "set": set, "options": options.len() }))
    })
}

fn chips_sets(s: &mut Session, _p: &Json) -> Result<Json> {
    let d = s.doc()?;
    let mut sets = serde_json::Map::new();
    for n in d.wb.names.iter().filter(|n| n.scope.is_none()) {
        let Some(id) = tf::set_id_of(&n.name) else { continue };
        let Some(options) = tf::string_constant(&n.formula).and_then(|t| tf::parse_set(&t)) else { continue };
        sets.insert(id.to_string(), options_json(&options));
    }
    Ok(json!({ "sets": sets }))
}

fn options_json(options: &[tf::ChipOption]) -> Json {
    Json::Array(
        options.iter().map(|o| json!({ "label": o.label, "color": format!("{:02x}{:02x}{:02x}", o.color[0], o.color[1], o.color[2]) })).collect(),
    )
}

fn chips_toggle(s: &mut Session, p: &Json) -> Result<Json> {
    let si = target_sheet(s, p)?;
    let c = cell_param_on(s, p, "cell", si)?.ok_or_else(|| bad("tf.chips.toggle", "missing `cell`"))?;
    let label = str_param(p, "label").map(str::trim).ok_or_else(|| bad("tf.chips.toggle", "missing `label`"))?.to_string();
    edit(s, |cx| {
        let Some(sh) = cx.wb.sheet(si) else { return Err(bad("tf.chips.toggle", "no such sheet")) };
        let Some(Kind::Chips { set }) = tf::classify(cx.wb.styles.get(sh.style_id(c)).num_fmt.as_str()) else {
            return Err(bad("tf.chips.toggle", "not a chips cell"));
        };
        let options = chip_options(&cx.wb, &set).unwrap_or_default();
        let cell = sh.cell(c).cloned().unwrap_or_default();
        if cell.formula.is_some() || cell.py.is_some() {
            return Err(bad("tf.chips.toggle", "these chips come from a formula"));
        }
        let current = match &cell.value {
            Value::Text(t) => t.to_string(),
            Value::Empty => String::new(),
            _ => return Err(bad("tf.chips.toggle", "the cell holds something other than chips")),
        };
        // Switching a chip OFF always works (also one whose option was removed); ON needs the option.
        let is_on = tf::chip_labels(&current).contains(&label.as_str());
        if !is_on && !options.iter().any(|o| o.label == label) {
            return Err(bad("tf.chips.toggle", format!("`{label}` is not one of the options")));
        }
        let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
        let next = tf::chip_toggle(&current, &label, &labels);
        let sh = cx.sheet_mut(si)?;
        let mut cell = cell;
        cell.value = if next.is_empty() { Value::Empty } else { Value::Text(next.as_str().into()) };
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
    fn date_picker_sets_and_clears_a_day() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        s.execute("tf.kind.set", json!({ "range": "A1", "kind": "date" })).unwrap();
        assert_eq!(s.execute("tf.date.set", json!({ "cell": "A1", "date": "2026-11-24" })).unwrap()["date"], "2026-11-24");
        // 2026-11-24 is serial 46350 in the 1900 system.
        assert_eq!(s.execute("cell.get", json!({ "cell": "A1" })).unwrap()["value"], 46350.0);
        assert_eq!(s.execute("cell.get", json!({ "cell": "A1" })).unwrap()["text"], "2026-11-24");
        for bad in ["2026-02-30", "2026-13-01", "2026-00-10", "26-11-24", "2026/11/24", "2026-11-2x", "", "1800-01-01", "2025-02-29"] {
            assert!(s.execute("tf.date.set", json!({ "cell": "A1", "date": bad })).is_err(), "{bad}");
        }
        assert!(s.execute("tf.date.set", json!({ "cell": "A1", "date": 7 })).is_err());
        assert!(s.execute("tf.date.set", json!({ "cell": "A1" })).is_err());
        assert!(s.execute("tf.date.set", json!({ "cell": "B1", "date": "2026-11-24" })).is_err());
        assert_eq!(s.execute("tf.date.set", json!({ "cell": "A1", "date": "2024-02-29" })).unwrap()["date"], "2024-02-29");
        s.execute("tf.date.set", json!({ "cell": "A1", "date": null })).unwrap();
        assert!(s.execute("cell.get", json!({ "cell": "A1" })).unwrap()["value"].is_null());
        assert_eq!(super::parse_iso_date("2026-11-24"), Some((2026, 11, 24)));
        assert_eq!(super::parse_iso_date("+026-11-24"), None);
    }

    #[test]
    fn chips_sets_are_defined_listed_toggled_and_survive_xlsx() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        // A chips cell needs its options.
        assert!(s.execute("tf.kind.set", json!({ "range": "A1", "kind": "chips", "set": "status" })).is_err());
        s.execute(
            "tf.kind.set",
            json!({ "range": "A1:A2", "kind": "chips", "set": "status", "options": ["To do", { "label": "Doing", "color": "#F59E0B" }, "Done"] }),
        )
        .unwrap();
        let sets = s.execute("tf.chips.sets", json!({})).unwrap();
        assert_eq!(sets["sets"]["status"][1], json!({ "label": "Doing", "color": "f59e0b" }));
        assert_eq!(sets["sets"]["status"].as_array().unwrap().len(), 3);
        // Toggling keeps the options' order, whatever order they were picked in.
        assert_eq!(s.execute("tf.chips.toggle", json!({ "cell": "A1", "label": "Done" })).unwrap()["value"], "Done");
        assert_eq!(s.execute("tf.chips.toggle", json!({ "cell": "A1", "label": "To do" })).unwrap()["value"], "To do, Done");
        assert_eq!(s.execute("tf.chips.toggle", json!({ "cell": "A1", "label": "Done" })).unwrap()["value"], "To do");
        assert!(s.execute("tf.chips.toggle", json!({ "cell": "A1", "label": "Nope" })).is_err());
        assert!(s.execute("tf.chips.toggle", json!({ "cell": "B9", "label": "Done" })).is_err());
        assert_eq!(s.execute("tf.chips.toggle", json!({ "cell": "A1", "label": "To do" })).unwrap()["value"], "");
        assert!(s.execute("cell.get", json!({ "cell": "A1" })).unwrap()["value"].is_null());
        s.execute("tf.chips.toggle", json!({ "cell": "A2", "label": "Doing" })).unwrap();
        // The set is a hidden name: the Paste Name list leaves it out, a redefinition replaces it.
        s.execute("tf.chips.defineSet", json!({ "set": "status", "options": ["Doing", "Done"] })).unwrap();
        assert_eq!(s.execute("tf.chips.sets", json!({})).unwrap()["sets"]["status"].as_array().unwrap().len(), 2);
        let d = s.doc().unwrap();
        assert_eq!(d.wb.names.iter().filter(|n| n.name == "_tf_chips_status").count(), 1);
        assert!(d.wb.names.iter().all(|n| n.name != "_tf_chips_status" || n.hidden));
        let (back, _) = gridcraft_xlsx::read_xlsx(&gridcraft_xlsx::write_xlsx(&d.wb).unwrap()).unwrap();
        let options = super::chip_options(&back, "status").unwrap();
        assert_eq!(options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Doing", "Done"]);
        assert!(back.names.iter().any(|n| n.name == "_tf_chips_status" && n.hidden));
    }

    #[test]
    fn chips_bad_params_are_errors() {
        let mut s = Session::new();
        s.execute("file.new", json!({})).unwrap();
        for p in [
            json!({}),
            json!({ "set": "a b", "options": [] }),
            json!({ "set": "ok" }),
            json!({ "set": "ok", "options": ["a,b"] }),
            json!({ "set": "ok", "options": ["x", "x"] }),
            json!({ "set": "ok", "options": [{ "label": "x", "color": "red" }] }),
            json!({ "set": "ok", "options": [7] }),
        ] {
            assert!(s.execute("tf.chips.defineSet", p.clone()).is_err(), "{p}");
        }
        assert!(s.execute("tf.chips.toggle", json!({})).is_err());
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
        s.execute("tf.kind.set", json!({ "range": "C1", "kind": "chips", "set": "status", "options": ["To do", "Done"] })).unwrap();
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
