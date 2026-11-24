//! "Turn into…" in the cell menu, and the dialog that edits a multi-select cell's options.

use std::cell::RefCell;

use egui::vec2;
use gridcraft_engine::cmd::tf::chip_options;
use gridcraft_engine::tf_ext::{self as tf, Kind};
use serde_json::{Value as Json, json};

use crate::SheetApp;

/// The options dialog: a new list for the selected cells, or the list of an existing set.
struct Options {
    /// The set being edited; `None` while choosing for a cell that is not multi-select yet.
    editing: Option<String>,
    /// For a new multi-select cell: an existing set to use instead of a new list.
    reuse: Option<String>,
    /// One option per line.
    text: String,
    error: Option<String>,
    focus: bool,
}

thread_local! {
    static OPTIONS: RefCell<Option<Options>> = const { RefCell::new(None) };
}

/// The kind of the active cell, if it is a marked one.
fn active_kind(app: &SheetApp) -> Option<Kind> {
    let d = app.session.active()?;
    let sh = d.wb.active()?;
    tf::classify(d.wb.styles.get(sh.style_id(d.selection.active)).num_fmt.as_str())
}

/// Every chip set of the workbook: its id and its labels.
fn sets(app: &mut SheetApp) -> Vec<(String, Vec<String>)> {
    let Ok(Json::Object(reply)) = app.session.run("tf.chips.sets", json!({})) else { return Vec::new() };
    let Some(Json::Object(sets)) = reply.get("sets") else { return Vec::new() };
    sets.iter()
        .map(|(id, options)| {
            let labels = options
                .as_array()
                .map(|list| list.iter().filter_map(|o| o.get("label").and_then(Json::as_str).map(str::to_string)).collect())
                .unwrap_or_default();
            (id.clone(), labels)
        })
        .collect()
}

/// A set id no set of the workbook has yet.
fn fresh_set_id(taken: &[(String, Vec<String>)]) -> String {
    (1..=taken.len() + 1)
        .map(|n| format!("list{n}"))
        .find(|id| !taken.iter().any(|(t, _)| t.eq_ignore_ascii_case(id)))
        .unwrap_or_else(|| "list".to_string())
}

/// The cell menu's "Turn into" entries. Returns whether one was chosen (the menu then closes).
pub fn cell_items(app: &mut SheetApp, ui: &mut egui::Ui) -> bool {
    let kind = active_kind(app);
    let lang = crate::i18n::current(ui.ctx());
    let mut chosen = false;
    ui.separator();
    let item =
        |ui: &mut egui::Ui, label: &str| ui.add(egui::Button::new(lang.tr(label)).frame_when_inactive(false).min_size(vec2(190.0, 20.0))).clicked();
    if item(ui, "Turn Into Rating") {
        app.run_or_alert("tf.kind.set", json!({ "kind": "rating" }));
        chosen = true;
    }
    if item(ui, "Turn Into Multi-select…") {
        open_options(None);
        chosen = true;
    }
    if item(ui, "Turn Into Date") {
        app.run_or_alert("tf.kind.set", json!({ "kind": "date" }));
        chosen = true;
    }
    if item(ui, "Turn Into Rich Text") {
        app.run_or_alert("tf.kind.set", json!({ "kind": "rich" }));
        chosen = true;
    }
    if let Some(Kind::Chips { set }) = &kind
        && item(ui, "Edit Options…")
    {
        let labels = app.session.active().and_then(|d| chip_options(&d.wb, set)).unwrap_or_default();
        open_options(Some((set.clone(), labels.into_iter().map(|o| o.label).collect::<Vec<_>>().join("\n"))));
        chosen = true;
    }
    if kind.is_some() && item(ui, "Turn Into Plain Cell") {
        app.run_or_alert("tf.kind.clear", json!({}));
        chosen = true;
    }
    chosen
}

fn open_options(editing: Option<(String, String)>) {
    let (editing, text) = match editing {
        Some((set, text)) => (Some(set), text),
        None => (None, String::new()),
    };
    OPTIONS.with(|o| *o.borrow_mut() = Some(Options { editing, reuse: None, text, error: None, focus: true }));
}

/// The lines of the dialog's text as options: trimmed, empty lines left out.
fn labels_of(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

/// Draws the options dialog while it is open. Called once per frame with the grid's popups.
pub fn options_dialog(app: &mut SheetApp, ctx: &egui::Context) {
    let Some(mut state) = OPTIONS.with(|o| o.borrow_mut().take()) else { return };
    let existing = if state.editing.is_none() { sets(app) } else { Vec::new() };
    let mut done = false;
    let mut confirm = false;
    let title = if state.editing.is_some() { "Edit Options" } else { "Multi-select Options" };
    egui::Window::new(title)
        .id(egui::Id::unique("tf_chip_options"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.set_width(280.0);
            if !existing.is_empty() {
                let shown = match &state.reuse {
                    Some(id) => existing.iter().find(|(s, _)| s == id).map(|(_, labels)| labels.join(", ")).unwrap_or_default(),
                    None => "A new list".to_string(),
                };
                ui.label("Options");
                craft_ui::ComboBox::from_id_salt("tf_chip_options_reuse").width(260.0).selected_text(shown).show_ui(ui, |ui| {
                    ui.selectable_value(&mut state.reuse, None, "A new list");
                    for (id, labels) in &existing {
                        ui.selectable_value(&mut state.reuse, Some(id.clone()), labels.join(", "));
                    }
                });
                ui.add_space(6.0);
            }
            if state.reuse.is_none() {
                ui.label("One option per line");
                let field = ui.add(egui::TextEdit::multiline(&mut state.text).desired_rows(6).desired_width(f32::INFINITY));
                if state.focus {
                    field.request_focus();
                    state.focus = false;
                }
            }
            if let Some(error) = &state.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                confirm = ui.button("OK").clicked();
                done = ui.button("Cancel").clicked();
            });
        });
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        done = true;
    }
    if confirm {
        let result = match (&state.editing, &state.reuse) {
            (Some(set), _) => {
                // Options that stay keep their colour; new ones get the next of the palette.
                let old = app.session.active().and_then(|d| chip_options(&d.wb, set)).unwrap_or_default();
                let options: Vec<Json> = labels_of(&state.text)
                    .into_iter()
                    .map(|label| match old.iter().find(|o| o.label == label) {
                        Some(o) => json!({ "label": label, "color": format!("{:02x}{:02x}{:02x}", o.color[0], o.color[1], o.color[2]) }),
                        None => json!(label),
                    })
                    .collect();
                app.run("tf.chips.defineSet", json!({ "set": set, "options": options }))
            }
            (None, Some(set)) => app.run("tf.kind.set", json!({ "kind": "chips", "set": set })),
            (None, None) => app.run("tf.kind.set", json!({ "kind": "chips", "set": fresh_set_id(&existing), "options": labels_of(&state.text) })),
        };
        match result {
            Ok(_) => done = true,
            Err(e) => state.error = Some(crate::clean_error(&e)),
        }
    }
    if !done {
        OPTIONS.with(|o| *o.borrow_mut() = Some(state));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use gridcraft_engine::Session;

    use super::*;

    #[test]
    fn option_lines_and_set_ids() {
        assert_eq!(labels_of(" To do \n\nDoing\r\n  \nDone"), vec!["To do", "Doing", "Done"]);
        assert!(labels_of("\n \n").is_empty());
        assert_eq!(fresh_set_id(&[]), "list1");
        assert_eq!(fresh_set_id(&[("list1".into(), vec![]), ("LIST2".into(), vec![])]), "list3");
        assert_eq!(fresh_set_id(&[("list2".into(), vec![])]), "list1");
    }

    #[test]
    fn the_workbooks_sets_are_listed_and_the_active_kind_is_read() {
        let mut s = Session::new();
        s.new_workbook();
        let mut app = SheetApp::new(s, Default::default());
        assert!(sets(&mut app).is_empty());
        assert_eq!(active_kind(&app), None);
        app.run("tf.kind.set", json!({ "range": "A1", "kind": "chips", "set": "list1", "options": ["To do", "Done"] })).unwrap();
        app.run("selection.set", json!({ "cell": "A1" })).unwrap();
        assert_eq!(sets(&mut app), vec![("list1".to_string(), vec!["To do".to_string(), "Done".to_string()])]);
        assert_eq!(active_kind(&app), Some(Kind::Chips { set: "list1".into() }));
    }
}
