//! Rich text while editing a cell: formatting a selected stretch of the draft (Bold, Italic,
//! Underline, Strikethrough), kept in step with the text as it is typed, and stored with the
//! cell when the edit is entered.
//!
//! The editor's own draft is plain text. This module keeps the formatting beside it: one draft
//! per edit, started when the editor opens, updated at the start of every frame ([`frame`]), and
//! written with `tf.rich.set` once the editor has closed with the text entered. Escape throws
//! it away ([`cancelled`]).

use std::cell::RefCell;

use egui::text::LayoutJob;
use egui::{Color32, Key, KeyboardShortcut, Modifiers};
use gridcraft_engine::core::{CellRef, Value};
use gridcraft_engine::model::rich::{RichPatch, RichRun, all_of, edit_runs, patch_runs};
use gridcraft_engine::model::{Font, Workbook};
use serde_json::json;

use crate::SheetApp;
use crate::editor::EditState;

struct Draft {
    doc: u64,
    sheet_uid: u64,
    cell: CellRef,
    /// The editor's text as of the last frame, which `runs` fit.
    text: String,
    runs: Vec<RichRun>,
    cancelled: bool,
}

thread_local! {
    static DRAFT: RefCell<Option<Draft>> = const { RefCell::new(None) };
}

/// A property a stretch of text can have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prop {
    Bold,
    Italic,
    Underline,
    Strike,
}

impl Prop {
    pub fn parse(name: &str) -> Option<Prop> {
        match name {
            "bold" => Some(Prop::Bold),
            "italic" => Some(Prop::Italic),
            "underline" => Some(Prop::Underline),
            "strike" => Some(Prop::Strike),
            _ => None,
        }
    }

    fn has(self, r: &RichRun) -> bool {
        match self {
            Prop::Bold => r.bold,
            Prop::Italic => r.italic,
            Prop::Underline => r.underline,
            Prop::Strike => r.strike,
        }
    }

    fn patch(self, on: bool) -> RichPatch {
        let mut p = RichPatch::default();
        match self {
            Prop::Bold => p.bold = Some(on),
            Prop::Italic => p.italic = Some(on),
            Prop::Underline => p.underline = Some(on),
            Prop::Strike => p.strike = Some(on),
        }
        p
    }
}

/// The UTF-16 offset of a character index (the editor counts characters).
fn utf16_at(text: &str, chars: usize) -> u32 {
    text.chars().take(chars).map(|c| c.len_utf16() as u32).sum()
}

/// The text the cell holds as a typed constant (`None` for a formula, a number, Python).
fn constant_text(wb: &Workbook, sheet: usize, cell: CellRef) -> Option<String> {
    let sh = wb.sheet(sheet)?;
    let c = sh.cell(cell);
    if c.is_some_and(|x| x.formula.is_some() || x.py.is_some()) {
        return None;
    }
    match c.map(|x| &x.value) {
        Some(Value::Text(t)) => Some(t.to_string()),
        None | Some(Value::Empty) => Some(String::new()),
        Some(_) => None,
    }
}

fn is_formula(text: &str) -> bool {
    text.starts_with(['=', '+', '-', '@'])
}

fn matches(d: &Draft, ed: &EditState) -> bool {
    d.doc == ed.doc && d.sheet_uid == ed.sheet_uid && d.cell == ed.cell && !d.cancelled
}

fn start(app: &SheetApp, ed: &EditState) -> Draft {
    // An edit that opened on the cell's own text (F2, a double click) starts with its formatting;
    // one started by typing over the cell starts plain.
    let runs = app
        .session
        .active()
        .filter(|d| d.uid == ed.doc)
        .and_then(|d| constant_text(&d.wb, ed.sheet, ed.cell).filter(|held| *held == ed.text).map(|held| d.wb.rich.runs(&held).to_vec()))
        .unwrap_or_default();
    Draft { doc: ed.doc, sheet_uid: ed.sheet_uid, cell: ed.cell, text: ed.text.clone(), runs, cancelled: false }
}

/// The editor closed with its text entered: the draft's formatting goes with it. Nothing is
/// written when the cell does not hold the draft's text (the edit was refused, completed to
/// another text, or became a formula) or when the formatting is what the text already has.
fn finish(app: &mut SheetApp, d: Draft) {
    if d.cancelled {
        return;
    }
    let Some(doc) = app.session.active().filter(|x| x.uid == d.doc) else { return };
    let Some(sheet) = doc.wb.sheets.iter().position(|sh| sh.uid.get() == d.sheet_uid) else { return };
    if constant_text(&doc.wb, sheet, d.cell).as_deref() != Some(d.text.as_str()) || d.text.is_empty() {
        return;
    }
    if doc.wb.rich.runs(&d.text) == d.runs.as_slice() {
        return;
    }
    let runs: Vec<serde_json::Value> = d
        .runs
        .iter()
        .map(|r| {
            json!({
                "start": r.start, "end": r.end, "bold": r.bold, "italic": r.italic, "underline": r.underline, "strike": r.strike,
                "color": r.color.map(|[red, green, blue]| format!("{red:02x}{green:02x}{blue:02x}")),
            })
        })
        .collect();
    app.run_or_alert("tf.rich.set", json!({ "sheet": sheet, "cell": d.cell.a1(), "runs": runs }));
}

/// Toggles a property over the editor's selected text. `false` when there is nothing to format
/// (no edit, no selection, a formula): the caller then does what it did before.
pub fn toggle(app: &SheetApp, prop: Prop) -> bool {
    let Some(ed) = app.editor.as_ref().filter(|e| !e.python && e.anchor != e.caret && !is_formula(&e.text)) else { return false };
    DRAFT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(d) = slot.as_mut().filter(|d| matches(d, ed)) else { return false };
        // The selection is in the editor's current text: bring the runs up to it first.
        if d.text != ed.text {
            d.runs = edit_runs(&d.text, &ed.text, &d.runs);
            d.text.clone_from(&ed.text);
        }
        let (from, to) = (ed.anchor.min(ed.caret), ed.anchor.max(ed.caret));
        let (start, end) = (utf16_at(&d.text, from), utf16_at(&d.text, to));
        let on = !all_of(&d.text, &d.runs, start, end, |r| prop.has(r));
        d.runs = patch_runs(&d.text, &d.runs, start, end, &prop.patch(on));
        true
    })
}

/// Start of every frame: keeps the draft in step with the editor, takes the formatting keys while
/// text is selected in it, and stores the formatting once the editor has closed.
pub fn frame(app: &mut SheetApp, ctx: &egui::Context) {
    let open = app.editor.as_ref().filter(|e| !e.python);
    let stale = DRAFT.with(|slot| {
        let mut slot = slot.borrow_mut();
        match (slot.as_mut(), open) {
            (Some(d), Some(ed)) if matches(d, ed) => {
                if d.text != ed.text {
                    d.runs = edit_runs(&d.text, &ed.text, &d.runs);
                    d.text.clone_from(&ed.text);
                }
                None
            }
            // Another edit (or none) is open: the draft's edit has ended.
            (Some(_), _) => slot.take(),
            (None, _) => None,
        }
    });
    if let Some(d) = stale {
        finish(app, d);
    }
    let Some(ed) = app.editor.as_ref().filter(|e| !e.python) else { return };
    if DRAFT.with(|slot| slot.borrow().is_none()) {
        let d = start(app, ed);
        DRAFT.with(|slot| *slot.borrow_mut() = Some(d));
    }
    if ed.anchor == ed.caret || is_formula(&ed.text) {
        return;
    }
    let keys = [
        (KeyboardShortcut::new(Modifiers::COMMAND, Key::B), Prop::Bold),
        (KeyboardShortcut::new(Modifiers::COMMAND, Key::I), Prop::Italic),
        (KeyboardShortcut::new(Modifiers::COMMAND, Key::U), Prop::Underline),
        (KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::X), Prop::Strike),
    ];
    for (shortcut, prop) in keys {
        if ctx.input_mut(|i| i.consume_shortcut(&shortcut)) && toggle(app, prop) {
            ctx.request_repaint();
        }
    }
}

/// Escape (or the formula bar's Cancel): the draft's formatting is thrown away with its text.
pub fn cancelled() {
    DRAFT.with(|slot| {
        if let Some(d) = slot.borrow_mut().as_mut() {
            d.cancelled = true;
        }
    });
}

/// What the editor's text widget needs to draw the draft's formatting.
pub struct EditRuns {
    text: String,
    runs: Vec<RichRun>,
    font: Font,
}

/// The formatting to draw in the editor for this edit; `None` when the draft has none.
pub fn for_layout(app: &SheetApp, ed: &EditState) -> Option<EditRuns> {
    let (text, runs) =
        DRAFT.with(|slot| slot.borrow().as_ref().filter(|d| matches(d, ed) && !d.runs.is_empty()).map(|d| (d.text.clone(), d.runs.clone())))?;
    let doc = app.session.active().filter(|d| d.uid == ed.doc)?;
    let sh = doc.wb.sheet(ed.sheet)?;
    Some(EditRuns { text, runs, font: doc.wb.styles.get(sh.style_id(ed.cell)).font.clone() })
}

/// Splits the editor's layout job (`shown` is its text) into the draft's formatted stretches.
pub fn restyle(rich: Option<&EditRuns>, job: &mut LayoutJob, shown: &str, size: f32, color: Color32) {
    let Some(rich) = rich.filter(|_| !is_formula(shown) && job.text == shown) else { return };
    // The widget may hold text typed this frame, which the draft sees at the next one.
    let runs = if rich.text == shown { rich.runs.clone() } else { edit_runs(&rich.text, shown, &rich.runs) };
    if !runs.is_empty() {
        job.sections = crate::grid::rich::sections(shown, &runs, &rich.font, size, color);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use gridcraft_engine::Session;

    use super::*;

    fn app_with(text: &str) -> SheetApp {
        let mut s = Session::new();
        s.new_workbook();
        let mut app = SheetApp::new(s, Default::default());
        app.run("cell.set", json!({"cell": "A1", "input": text})).unwrap();
        app.run("selection.set", json!({"cell": "A1"})).unwrap();
        app
    }

    fn select(app: &mut SheetApp, from: usize, to: usize) {
        let ed = app.editor.as_mut().unwrap();
        (ed.anchor, ed.caret) = (from, to);
    }

    fn runs(app: &SheetApp, text: &str) -> Vec<RichRun> {
        app.session.active().unwrap().wb.rich.runs(text).to_vec()
    }

    fn bold(start: u32, end: u32) -> RichRun {
        RichRun { start, end, bold: true, ..RichRun::default() }
    }

    #[test]
    fn bold_on_a_selection_is_stored_when_the_edit_is_entered() {
        let ctx = egui::Context::default();
        let mut app = app_with("Ship by Friday");
        app.begin_edit(None, false);
        frame(&mut app, &ctx);
        // Nothing selected: nothing to format.
        assert!(!toggle(&app, Prop::Bold));
        select(&mut app, 0, 4);
        assert!(toggle(&app, Prop::Bold));
        // The editor draws it at once.
        let ed = app.editor.take().unwrap();
        assert_eq!(for_layout(&app, &ed).map(|r| r.runs), Some(vec![bold(0, 4)]));
        app.editor = Some(ed);
        // Typing after the bold word keeps the formatting in place.
        app.editor.as_mut().unwrap().text = "Ship it by Friday".into();
        frame(&mut app, &ctx);
        // Entering an edit whose text did not change still stores formatting: do both.
        assert!(app.commit_edit(0, 0, false, false));
        assert!(runs(&app, "Ship it by Friday").is_empty(), "stored at the next frame, not before");
        frame(&mut app, &ctx);
        assert_eq!(runs(&app, "Ship it by Friday"), vec![bold(0, 4)]);
        // Formatting only (the text untouched), then Enter.
        app.begin_edit(None, false);
        frame(&mut app, &ctx);
        select(&mut app, 8, 10);
        assert!(toggle(&app, Prop::Italic));
        assert!(app.commit_edit(0, 0, false, false));
        frame(&mut app, &ctx);
        assert_eq!(runs(&app, "Ship it by Friday").len(), 2);
        // Toggling bold off over the bold word, then Escape: nothing changes.
        app.begin_edit(None, false);
        frame(&mut app, &ctx);
        select(&mut app, 4, 0);
        assert!(toggle(&app, Prop::Bold));
        app.cancel_edit();
        frame(&mut app, &ctx);
        assert_eq!(runs(&app, "Ship it by Friday").len(), 2);
        // The same, entered: the bold is gone, the italic stays.
        app.begin_edit(None, false);
        frame(&mut app, &ctx);
        select(&mut app, 0, 4);
        assert!(toggle(&app, Prop::Bold));
        assert!(app.commit_edit(0, 0, false, false));
        frame(&mut app, &ctx);
        assert_eq!(runs(&app, "Ship it by Friday"), vec![RichRun { start: 8, end: 10, italic: true, ..RichRun::default() }]);
    }

    #[test]
    fn typing_over_a_cell_starts_plain_and_formulas_are_left_alone() {
        let ctx = egui::Context::default();
        let mut app = app_with("Old text");
        app.run("tf.rich.format", json!({"cell": "A1", "start": 0, "end": 3, "bold": true})).unwrap();
        // Started by typing: the new text has no formatting, and gets none from the old.
        app.begin_edit(Some("N".into()), false);
        frame(&mut app, &ctx);
        app.editor.as_mut().unwrap().text = "New".into();
        frame(&mut app, &ctx);
        assert!(app.commit_edit(0, 0, false, false));
        frame(&mut app, &ctx);
        assert!(runs(&app, "New").is_empty());
        // A formula draft takes no formatting.
        app.begin_edit(Some("=1+1".into()), false);
        frame(&mut app, &ctx);
        select(&mut app, 0, 3);
        assert!(!toggle(&app, Prop::Bold));
        app.cancel_edit();
        frame(&mut app, &ctx);
    }

    #[test]
    fn character_indexes_become_utf16_offsets() {
        assert_eq!(utf16_at("a😀b", 0), 0);
        assert_eq!(utf16_at("a😀b", 2), 3);
        assert_eq!(utf16_at("a😀b", 9), 4);
        assert!(is_formula("=A1") && is_formula("+1") && !is_formula("plain = text") && !is_formula(""));
        assert_eq!(Prop::parse("strike"), Some(Prop::Strike));
        assert_eq!(Prop::parse("blink"), None);
    }
}
