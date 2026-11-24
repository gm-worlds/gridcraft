//! Taskforce cell kinds on the grid (`gridcraft_tf_ext`): how a marked cell is drawn and what a
//! click on it does. The grid calls [`draws`], [`paint`] and [`click`] from one line each; all the
//! rules live here.

use std::cell::{Cell, RefCell};

use egui::{Color32, FontId, Painter, Pos2, Rect, Shape, pos2, vec2};
use gridcraft_engine::cmd::tf::chip_options;
use gridcraft_engine::core::date::{self, DateSystem};
use gridcraft_engine::core::{CellRef, Value};
use gridcraft_engine::model::Workbook;
use gridcraft_tf_ext::{self as tf, Kind};
use serde_json::json;

use crate::SheetApp;

/// Empty stars and their outline: the sheet stays light in both themes.
const STAR_EMPTY: Color32 = Color32::from_rgb(0xD5, 0xD9, 0xDE);

/// A chip whose label is not (or no longer) one of its set's options.
const CHIP_UNKNOWN: [u8; 3] = [0x8A, 0x92, 0x9C];

/// The chips cell whose option list is open, if any. One per thread: the UI runs on one.
#[derive(Clone, Copy, PartialEq)]
struct OpenChips {
    sheet: usize,
    cell: CellRef,
    /// The click that opened it is still being handled this frame: it must not close it again.
    fresh: bool,
}

/// The date cell whose calendar is open, and the month it shows.
#[derive(Clone, Copy, PartialEq)]
struct OpenDate {
    sheet: usize,
    cell: CellRef,
    year: i32,
    month: u32,
    fresh: bool,
}

thread_local! {
    static OPEN_CHIPS: RefCell<Option<OpenChips>> = const { RefCell::new(None) };
    static OPEN_DATE: RefCell<Option<OpenDate>> = const { RefCell::new(None) };
    static UTC_OFFSET_MINUTES: Cell<i32> = const { Cell::new(0) };
}

/// Tells the calendar how far the person's clock is ahead of UTC, in minutes, so that "today" is
/// their day. The shell that knows the time zone sets it (the browser does); without it, UTC.
pub fn set_utc_offset_minutes(minutes: i32) {
    UTC_OFFSET_MINUTES.with(|c| c.set(minutes.clamp(-16 * 60, 16 * 60)));
}

/// Text on the sheet, which is light in both themes.
const SHEET_INK: Color32 = Color32::from_rgb(0x1F, 0x23, 0x28);

/// Whether a cell with this number format and value is drawn by [`paint`] instead of as text.
pub fn draws(code: &str, value: &Value) -> bool {
    match tf::classify(code) {
        Some(Kind::Rating { .. }) => matches!(value, Value::Number(_) | Value::Empty),
        Some(Kind::Chips { .. }) => matches!(value, Value::Text(_) | Value::Empty),
        Some(Kind::Date) => matches!(value, Value::Number(_) | Value::Empty),
        _ => false,
    }
}

/// Draws a marked cell. Returns whether it drew (the cell's text is then left out).
pub fn paint(p: &Painter, rect: Rect, z: f32, wb: &Workbook, code: &str, value: &Value, accent: Color32) -> bool {
    match tf::classify(code) {
        Some(Kind::Date) => {
            let text = match value {
                Value::Number(n) => match ymd(wb.date_system, *n) {
                    Some((y, m, d)) => format!("{y:04}-{m:02}-{d:02}"),
                    // Not a day in the calendar: drawn as the plain number cell it is.
                    None => return false,
                },
                Value::Empty => String::new(),
                _ => return false,
            };
            paint_date(&p.with_clip_rect(rect.intersect(p.clip_rect())), rect, z, &text);
            true
        }
        Some(Kind::Chips { set }) => {
            let text = match value {
                Value::Text(t) => t.as_ref(),
                Value::Empty => "",
                _ => return false,
            };
            let options = chip_options(wb, &set).unwrap_or_default();
            paint_chips(&p.with_clip_rect(rect.intersect(p.clip_rect())), rect, z, text, &options);
            true
        }
        Some(Kind::Rating { max }) => {
            let stars = match value {
                Value::Number(n) => tf::rating_stars(*n, max),
                Value::Empty => 0,
                _ => return false,
            };
            let p = p.with_clip_rect(rect.intersect(p.clip_rect()));
            for i in 0..max {
                let r = star_rect(rect, z, i, max);
                p.add(star(r, if i < stars { accent } else { STAR_EMPTY }));
            }
            true
        }
        _ => false,
    }
}

/// Acts on a click at `pos` in a marked cell. Returns whether the click was used.
pub fn click(app: &mut SheetApp, c: CellRef, code: &str, value: &Value, pos: Pos2, rect: Rect, z: f32) -> bool {
    match tf::classify(code) {
        Some(Kind::Date) if draws(code, value) && date_button(rect, z).expand(2.0).contains(pos) => {
            let Some(d) = app.session.active() else { return false };
            let (sheet, sys) = (d.wb.active_sheet(), d.wb.date_system);
            // The month of the cell's day, or of today in an empty cell.
            let shown = match value {
                Value::Number(n) => ymd(sys, *n),
                _ => None,
            };
            let Some((year, month, _)) = shown.or_else(today) else { return false };
            OPEN_DATE.with(|o| *o.borrow_mut() = Some(OpenDate { sheet, cell: c, year, month, fresh: true }));
            true
        }
        Some(Kind::Chips { .. }) if draws(code, value) => {
            let Some(sheet) = app.session.active().map(|d| d.wb.active_sheet()) else { return false };
            OPEN_CHIPS.with(|o| *o.borrow_mut() = Some(OpenChips { sheet, cell: c, fresh: true }));
            true
        }
        Some(Kind::Rating { max }) if draws(code, value) => {
            let Some(star) = (0..max).find(|i| hit_rect(rect, z, *i, max).contains(pos)) else { return false };
            let _ = app.run("tf.rating.click", json!({ "cell": c.a1(), "star": star + 1 }));
            true
        }
        _ => false,
    }
}

/// `color` mixed towards `to` by `t` (0 = the colour, 1 = `to`).
fn mix(color: [u8; 3], to: u8, t: f32) -> Color32 {
    let m = |c: u8| (f32::from(c) + (f32::from(to) - f32::from(c)) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(m(color[0]), m(color[1]), m(color[2]))
}

/// A chip's fill and text colours: a light tint of the option's colour with dark text of the same
/// hue, readable on the sheet (which is light in both themes).
fn chip_colors(color: [u8; 3]) -> (Color32, Color32) {
    (mix(color, 255, 0.82), mix(color, 0, 0.55))
}

fn paint_chips(p: &Painter, rect: Rect, z: f32, text: &str, options: &[tf::ChipOption]) {
    let labels = tf::chip_labels(text);
    let font = FontId::proportional((11.0 * z * 1.25).clamp(8.0, 22.0));
    let pad = 5.0 * z;
    let h = (rect.height() - 4.0).min(font.size + 6.0 * z).max(6.0);
    let mut x = rect.left() + 3.0 * z;
    for (i, label) in labels.iter().enumerate() {
        let color = options.iter().find(|o| o.label == *label).map_or(CHIP_UNKNOWN, |o| o.color);
        let (fill, ink) = chip_colors(color);
        let galley = p.layout_no_wrap((*label).to_string(), font.clone(), ink);
        let w = galley.size().x + 2.0 * pad;
        // What does not fit is counted instead of cut in half.
        if i > 0 && x + w > rect.right() - 2.0 {
            let more = p.layout_no_wrap(format!("+{}", labels.len() - i), font.clone(), mix(CHIP_UNKNOWN, 0, 0.4));
            p.galley(pos2(x, rect.center().y - more.size().y / 2.0), more, Color32::BLACK);
            return;
        }
        let pill = Rect::from_min_size(pos2(x, rect.center().y - h / 2.0), vec2(w, h));
        p.rect_filled(pill, h / 2.0, fill);
        p.galley(pos2(pill.left() + pad, pill.center().y - galley.size().y / 2.0), galley, ink);
        x += w + 3.0 * z;
    }
}

/// The option list of the chips cell that was clicked: every option with a tick when it is on.
/// Stays open while options are switched; closes on a click elsewhere, Escape, or when the cell
/// is no longer the active one. The grid calls this once per frame after its own popups.
pub fn popup(app: &mut SheetApp, ui: &mut egui::Ui, geo: &crate::grid::Geo) {
    chips_popup(app, ui, geo);
    date_popup(app, ui, geo);
}

/// The calendar day of a date serial, or `None` when it is not a day in the calendar.
fn ymd(sys: DateSystem, serial: f64) -> Option<(i32, u32, u32)> {
    // Serial 0 is the 1900 system's "day 0 of January": no day to show in a calendar.
    date::datetime_from_serial(sys, serial.floor()).filter(|d| d.day >= 1).map(|d| (d.year, d.month, d.day))
}

/// Today on the person's clock (see [`set_utc_offset_minutes`]), as a calendar day.
fn today() -> Option<(i32, u32, u32)> {
    let offset = f64::from(UTC_OFFSET_MINUTES.with(Cell::get)) / 1440.0;
    ymd(DateSystem::D1900, gridcraft_engine::calc::now_serial() + offset)
}

/// The month after (`step` 1) or before (`step` -1) a month, kept inside the calendar's years.
fn step_month(year: i32, month: u32, step: i32) -> (i32, u32) {
    let index = i64::from(year) * 12 + i64::from(month) - 1 + i64::from(step);
    let (y, m) = (index.div_euclid(12), index.rem_euclid(12) + 1);
    if (1900..=9999).contains(&y) { (y as i32, m as u32) } else { (year, month) }
}

/// Column (Monday = 0) of the first day of a month, and the month's length.
fn month_shape(sys: DateSystem, year: i32, month: u32) -> Option<(u32, u32)> {
    // The serial helper reads years below 1900 the way DATE() does (1800 becomes 3700).
    if !(1900..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    let first = date::serial_from_ymd(sys, i64::from(year), i64::from(month), 1)?;
    let weekday = date::datetime_from_serial(sys, first)?.weekday;
    Some(((weekday + 6) % 7, date::days_in_month_in(sys, year, month)))
}

/// Where the calendar button sits in a date cell.
fn date_button(rect: Rect, z: f32) -> Rect {
    let side = (13.0 * z).min(rect.height() - 4.0).max(4.0);
    Rect::from_center_size(pos2(rect.right() - 3.0 * z - side / 2.0, rect.center().y), vec2(side, side))
}

fn paint_date(p: &Painter, rect: Rect, z: f32, text: &str) {
    let button = date_button(rect, z);
    let muted = mix(CHIP_UNKNOWN, 0, 0.25);
    // A small calendar: a sheet with a header band and two days.
    let r = (button.width() * 0.16).max(1.0);
    p.rect_filled(button, r, Color32::WHITE);
    p.rect_stroke(button, r, egui::Stroke::new(1.0, muted), egui::StrokeKind::Inside);
    p.rect_filled(Rect::from_min_max(button.min, pos2(button.right(), button.top() + button.height() * 0.3)), r, muted);
    let dot = (button.width() * 0.09).max(0.8);
    for dx in [-0.2, 0.2] {
        p.circle_filled(pos2(button.center().x + button.width() * dx, button.top() + button.height() * 0.64), dot, muted);
    }
    if text.is_empty() {
        return;
    }
    // A day always fits: the text shrinks before it would be cut (never `####`).
    let avail = (button.left() - rect.left() - 5.0 * z).max(1.0);
    let mut size = (11.0 * z * 1.25).clamp(6.0, 26.0);
    let mut galley = p.layout_no_wrap(text.to_string(), FontId::proportional(size), SHEET_INK);
    if galley.size().x > avail {
        size = (size * avail / galley.size().x).max(5.0);
        galley = p.layout_no_wrap(text.to_string(), FontId::proportional(size), SHEET_INK);
    }
    p.galley(pos2(rect.left() + 3.0 * z, rect.center().y - galley.size().y / 2.0), galley, SHEET_INK);
}

/// The calendar of the date cell whose button was clicked: a month to pick a day from, with
/// Today and Clear. Picking closes it; so do a click elsewhere, Escape and leaving the cell.
fn date_popup(app: &mut SheetApp, ui: &mut egui::Ui, geo: &crate::grid::Geo) {
    let Some(open) = OPEN_DATE.with(|o| *o.borrow()) else { return };
    let close = || OPEN_DATE.with(|o| *o.borrow_mut() = None);
    let Some(d) = app.session.active() else { return close() };
    let wb = d.wb.clone();
    let Some(sh) = wb.sheet(open.sheet).filter(|_| wb.active_sheet() == open.sheet && d.selection.active == open.cell) else { return close() };
    if tf::classify(wb.styles.get(sh.style_id(open.cell)).num_fmt.as_str()) != Some(Kind::Date) {
        return close();
    }
    let sys = wb.date_system;
    let Some((lead, days)) = month_shape(sys, open.year, open.month) else { return close() };
    let chosen = match sh.value(open.cell) {
        Value::Number(n) => ymd(sys, n),
        _ => None,
    };
    let now = today();
    let r = geo.screen_cell_rect(sh, open.cell);
    let screen = ui.ctx().content_rect();
    let (cell_w, cell_h) = (30.0, 26.0);
    let height = 8.0 * cell_h + 24.0;
    let above = r.bottom() + height > screen.bottom() && r.top() - height >= screen.top();
    let (at, pivot) = if above { (r.left_top(), egui::Align2::LEFT_BOTTOM) } else { (r.left_bottom(), egui::Align2::LEFT_TOP) };
    // What the person did this frame: a day to set, the cell to empty, or another month to show.
    let mut pick: Option<Option<(i32, u32, u32)>> = None;
    let mut shown = (open.year, open.month);
    let response =
        egui::Area::new(egui::Id::unique("tf_date_calendar")).fixed_pos(at).pivot(pivot).order(egui::Order::Foreground).show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).inner_margin(8.0).show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                ui.set_width(7.0 * cell_w);
                let flat = |text: &str| egui::Button::new(text).frame_when_inactive(false);
                ui.horizontal(|ui| {
                    if ui.add_sized([cell_w, cell_h], flat("‹")).on_hover_text("Previous month").clicked() {
                        shown = step_month(open.year, open.month, -1);
                    }
                    let title = format!("{} {}", date::MONTHS.get(open.month as usize - 1).copied().unwrap_or_default(), open.year);
                    ui.add_sized([5.0 * cell_w, cell_h], egui::Label::new(egui::RichText::new(title).strong()));
                    if ui.add_sized([cell_w, cell_h], flat("›")).on_hover_text("Next month").clicked() {
                        shown = step_month(open.year, open.month, 1);
                    }
                });
                ui.horizontal(|ui| {
                    for name in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"] {
                        ui.add_sized([cell_w, cell_h * 0.8], egui::Label::new(egui::RichText::new(name).small().weak()));
                    }
                });
                let mut day = 1u32;
                for week in 0..6u32 {
                    if day > days {
                        break;
                    }
                    ui.horizontal(|ui| {
                        for column in 0..7u32 {
                            if (week == 0 && column < lead) || day > days {
                                ui.allocate_exact_size(vec2(cell_w, cell_h), egui::Sense::hover());
                                continue;
                            }
                            let this = (open.year, open.month, day);
                            let mut button = flat(&day.to_string()).selected(chosen == Some(this));
                            if now == Some(this) {
                                button = button.stroke(egui::Stroke::new(1.0, ui.visuals().selection.bg_fill));
                            }
                            if ui.add_sized([cell_w, cell_h], button).clicked() {
                                pick = Some(Some(this));
                            }
                            day += 1;
                        }
                    });
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if let Some(t) = now.filter(|_| ui.add_sized([3.5 * cell_w, cell_h], flat("Today")).clicked()) {
                        pick = Some(Some(t));
                    }
                    if ui.add_sized([3.5 * cell_w, cell_h], flat("Clear")).clicked() {
                        pick = Some(None);
                    }
                });
            });
        });
    if let Some(choice) = pick {
        let date = choice.map(|(y, m, d)| format!("{y:04}-{m:02}-{d:02}"));
        app.run_or_alert("tf.date.set", json!({ "cell": open.cell.a1(), "date": date }));
        return close();
    }
    let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
    if escape || (!open.fresh && response.response.clicked_elsewhere()) {
        return close();
    }
    OPEN_DATE.with(|o| {
        if let Some(p) = o.borrow_mut().as_mut() {
            p.fresh = false;
            (p.year, p.month) = shown;
        }
    });
}

fn chips_popup(app: &mut SheetApp, ui: &mut egui::Ui, geo: &crate::grid::Geo) {
    let Some(open) = OPEN_CHIPS.with(|o| *o.borrow()) else { return };
    let close = || OPEN_CHIPS.with(|o| *o.borrow_mut() = None);
    let Some(d) = app.session.active() else { return close() };
    let wb = d.wb.clone();
    let Some(sh) = wb.sheet(open.sheet).filter(|_| wb.active_sheet() == open.sheet && d.selection.active == open.cell) else { return close() };
    let Some(Kind::Chips { set }) = tf::classify(wb.styles.get(sh.style_id(open.cell)).num_fmt.as_str()) else { return close() };
    let options = chip_options(&wb, &set).unwrap_or_default();
    let on: Vec<String> = match sh.value(open.cell) {
        Value::Text(t) => tf::chip_labels(&t).into_iter().map(str::to_string).collect(),
        _ => Vec::new(),
    };
    let r = geo.screen_cell_rect(sh, open.cell);
    let screen = ui.ctx().content_rect();
    let row_h = 26.0;
    let height = (options.len().clamp(1, 10) as f32) * row_h + 8.0;
    let above = r.bottom() + height > screen.bottom() && r.top() - height >= screen.top();
    let (at, pivot) = if above { (r.left_top(), egui::Align2::LEFT_BOTTOM) } else { (r.left_bottom(), egui::Align2::LEFT_TOP) };
    let mut toggled: Option<String> = None;
    let response =
        egui::Area::new(egui::Id::unique("tf_chips_options")).fixed_pos(at).pivot(pivot).order(egui::Order::Foreground).show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).inner_margin(4.0).show(ui, |ui| {
                ui.set_min_width(r.width().max(140.0));
                if options.is_empty() {
                    ui.label("No options yet");
                }
                egui::ScrollArea::vertical().max_height(10.0 * row_h).show(ui, |ui| {
                    for option in &options {
                        let is_on = on.contains(&option.label);
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(140.0), row_h), egui::Sense::click());
                        if resp.hovered() {
                            ui.painter().rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
                        }
                        let (fill, ink) = chip_colors(option.color);
                        let galley = ui.painter().layout_no_wrap(option.label.clone(), FontId::proportional(13.0), ink);
                        let pill = Rect::from_min_size(pos2(rect.left() + 24.0, rect.center().y - 10.0), vec2(galley.size().x + 14.0, 20.0));
                        ui.painter().rect_filled(pill, 10.0, fill);
                        ui.painter().galley(pos2(pill.left() + 7.0, pill.center().y - galley.size().y / 2.0), galley, ink);
                        if is_on {
                            let tick = Rect::from_center_size(pos2(rect.left() + 12.0, rect.center().y), vec2(12.0, 12.0));
                            crate::icons::paint_on_sheet(ui.painter(), tick, crate::icons::Icon::Check, ui.visuals().text_color());
                        }
                        resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, is_on, option.label.as_str()));
                        if resp.clicked() {
                            toggled = Some(option.label.clone());
                        }
                    }
                });
            });
        });
    if let Some(label) = toggled {
        app.run_or_alert("tf.chips.toggle", json!({ "cell": open.cell.a1(), "label": label }));
    }
    let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
    if escape || (!open.fresh && response.response.clicked_elsewhere()) {
        close();
    } else if open.fresh {
        OPEN_CHIPS.with(|o| {
            if let Some(p) = o.borrow_mut().as_mut() {
                p.fresh = false;
            }
        });
    }
}

fn gap(z: f32) -> f32 {
    (2.0 * z).max(1.0)
}

/// Star width: the zoomed size, or smaller so that all `max` stars fit the cell's width and height.
fn star_size(cell: Rect, z: f32, max: u8) -> f32 {
    let n = f32::from(max.max(1));
    let fit = (cell.width() - 8.0 * z - (n - 1.0) * gap(z)) / n;
    (13.0 * z * 1.2).clamp(9.0, 26.0).min(fit).min(cell.height() - 2.0).max(2.0)
}

fn star_rect(cell: Rect, z: f32, index: u8, max: u8) -> Rect {
    let s = star_size(cell, z, max);
    let gap = gap(z);
    let left = cell.left() + 4.0 * z + f32::from(index) * (s + gap);
    Rect::from_min_size(pos2(left, cell.center().y - s / 2.0), vec2(s, s))
}

/// The area that counts as star `index`: its square plus half the gap on each side and the cell's
/// full height, so a finger does not have to land on the drawing.
fn hit_rect(cell: Rect, z: f32, index: u8, max: u8) -> Rect {
    let r = star_rect(cell, z, index, max);
    let half_gap = gap(z) / 2.0;
    Rect::from_min_max(pos2(r.left() - half_gap, cell.top()), pos2(r.right() + half_gap, cell.bottom()))
}

/// A five-pointed star filling `r`, as triangles around its centre (a concave outline cannot go
/// through the convex-polygon painter).
fn star(r: Rect, color: Color32) -> Shape {
    let c = r.center();
    let outer = r.width() / 2.0;
    let inner = outer * 0.42;
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(c, color);
    for k in 0..10u32 {
        let radius = if k % 2 == 0 { outer } else { inner };
        let angle = -std::f32::consts::FRAC_PI_2 + k as f32 * std::f32::consts::PI / 5.0;
        mesh.colored_vertex(pos2(c.x + radius * angle.cos(), c.y + radius * angle.sin()), color);
    }
    for k in 1..=10u32 {
        mesh.add_triangle(0, k, if k == 10 { 1 } else { k + 1 });
    }
    Shape::mesh(mesh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> String {
        Kind::Rating { max: 5 }.format_code()
    }

    #[test]
    fn calendar_months_have_the_right_shape() {
        use gridcraft_engine::core::date::DateSystem::D1900;
        // November 2026 starts on a Sunday (last column) and has 30 days; February 2024 has 29.
        assert_eq!(month_shape(D1900, 2026, 11), Some((6, 30)));
        assert_eq!(month_shape(D1900, 2024, 2), Some((3, 29)));
        assert_eq!(month_shape(D1900, 2026, 6), Some((0, 30)));
        assert_eq!(month_shape(D1900, 1800, 1), None);
        assert_eq!(step_month(2026, 12, 1), (2027, 1));
        assert_eq!(step_month(2026, 1, -1), (2025, 12));
        assert_eq!(step_month(9999, 12, 1), (9999, 12));
        assert_eq!(step_month(1900, 1, -1), (1900, 1));
        assert_eq!(ymd(D1900, 46350.75), Some((2026, 11, 24)));
        assert_eq!(ymd(D1900, 0.0), None);
        assert_eq!(ymd(D1900, -3.0), None);
        assert_eq!(ymd(D1900, 1e12), None);
    }

    #[test]
    fn date_cells_draw_for_numbers_and_empty_only() {
        let code = Kind::Date.format_code();
        assert!(draws(&code, &Value::Number(46350.0)));
        assert!(draws(&code, &Value::Empty));
        assert!(!draws(&code, &Value::Text("soon".into())));
        let cell = Rect::from_min_size(pos2(0.0, 0.0), vec2(80.0, 20.0));
        assert!(cell.contains_rect(date_button(cell, 1.0)));
        // A cell squeezed to nothing still gives a finite button.
        assert!(date_button(Rect::from_min_size(pos2(0.0, 0.0), vec2(2.0, 1.0)), 1.0).is_finite());
    }

    #[test]
    fn chips_draw_for_text_and_empty_only() {
        let code = Kind::Chips { set: "status".into() }.format_code();
        assert!(draws(&code, &Value::Text("Done".into())));
        assert!(draws(&code, &Value::Empty));
        assert!(!draws(&code, &Value::Number(2024.0)));
    }

    #[test]
    fn chip_text_stays_readable_on_its_fill() {
        // WCAG contrast of the ink on the tint, for every palette colour and the fallback: at least 4.5.
        let lum = |c: Color32| {
            let f = |v: u8| {
                let x = f32::from(v) / 255.0;
                if x <= 0.03928 { x / 12.92 } else { ((x + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        };
        for color in tf::CHIP_PALETTE.iter().copied().chain([CHIP_UNKNOWN]) {
            let (fill, ink) = chip_colors(color);
            let ratio = (lum(fill) + 0.05) / (lum(ink) + 0.05);
            assert!(ratio >= 4.5, "{color:?}: {ratio:.2}");
        }
    }

    #[test]
    fn rating_draws_for_numbers_and_empty_only() {
        assert!(draws(&code(), &Value::Number(3.0)));
        assert!(draws(&code(), &Value::Empty));
        assert!(!draws(&code(), &Value::Text("n/a".into())));
        assert!(!draws("General", &Value::Number(3.0)));
        assert!(!draws("checkbox", &Value::Bool(true)));
    }

    #[test]
    fn all_stars_fit_a_narrow_cell() {
        let cell = Rect::from_min_size(pos2(0.0, 0.0), vec2(51.0, 16.0));
        for max in [5u8, 7, 10] {
            assert!(star_rect(cell, 0.8, max - 1, max).right() <= cell.right(), "{max} stars");
        }
    }

    #[test]
    fn every_star_has_its_own_hit_area_inside_the_row() {
        let cell = Rect::from_min_size(pos2(100.0, 50.0), vec2(120.0, 20.0));
        for z in [0.4, 0.8, 1.6] {
            let mut last_right = cell.left();
            for i in 0..5u8 {
                let h = hit_rect(cell, z, i, 5);
                assert!(h.left() >= last_right - 0.01, "star {i} overlaps the one before at zoom {z}");
                assert!(h.contains(star_rect(cell, z, i, 5).center()));
                assert_eq!((h.top(), h.bottom()), (cell.top(), cell.bottom()));
                last_right = h.right();
            }
        }
    }
}
