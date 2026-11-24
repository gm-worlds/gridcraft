//! The host's look on the sheet's chrome: its accent colour and corner radius, so the app sits in
//! a Taskforce page without looking like a different product. The sheet itself stays a sheet.
//!
//! The host gives one accent colour (its primary). Every token that carries the brand colour is
//! derived from it, and each derived colour is pushed to the contrast it needs against what it is
//! drawn on, so a host colour that is too pale or too dark still gives a usable interface.

use std::cell::Cell;

use egui::Color32;

use crate::theme::{Tokens, contrast};

/// Corner radius the host may ask for, in points.
const MAX_RADIUS: u8 = 16;

thread_local! {
    static ACCENT: Cell<Option<[u8; 3]>> = const { Cell::new(None) };
    static RADIUS: Cell<Option<u8>> = const { Cell::new(None) };
}

/// Sets (or with `None` takes away) the host's accent colour. The caller re-applies the theme.
pub fn set_accent(rgb: Option<[u8; 3]>) {
    ACCENT.with(|a| a.set(rgb));
}

/// Sets (or takes away) the host's corner radius for buttons and fields, in points.
pub fn set_radius(points: Option<f32>) {
    RADIUS.with(|r| r.set(points.filter(|p| p.is_finite()).map(|p| p.round().clamp(0.0, f32::from(MAX_RADIUS)) as u8)));
}

pub fn accent() -> Option<[u8; 3]> {
    ACCENT.with(Cell::get)
}

pub fn radius() -> Option<u8> {
    RADIUS.with(Cell::get)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// `color` moved towards black or white (whichever is away from `on`) until it has at least
/// `ratio` contrast against `on`. A colour that already has it is returned as it is.
fn readable(color: Color32, on: Color32, ratio: f32) -> Color32 {
    let away = if contrast(Color32::BLACK, on) >= contrast(Color32::WHITE, on) { Color32::BLACK } else { Color32::WHITE };
    let mut out = color;
    for step in 1..=20 {
        if contrast(out, on) >= ratio {
            break;
        }
        out = mix(color, away, step as f32 / 20.0);
    }
    out
}

/// The palette with the host's accent in place of the built-in one. Without a host accent the
/// palette comes back untouched.
pub fn themed(mut t: Tokens) -> Tokens {
    let Some([r, g, b]) = accent() else { return t };
    let host = Color32::from_rgb(r, g, b);
    // Glyphs and text in the accent sit on the window and the ribbon: 4.5 against both.
    let on_chrome = readable(readable(host, t.window, 4.5), t.ribbon, 4.5);
    t.accent = on_chrome;
    t.accent_dark = mix(on_chrome, if t.dark { Color32::WHITE } else { Color32::BLACK }, 0.22);
    t.accent_soft = mix(on_chrome, t.ribbon, 0.84);
    t.on_accent =
        if contrast(Color32::WHITE, on_chrome) >= contrast(Color32::BLACK, on_chrome) { Color32::WHITE } else { Color32::from_rgb(0x12, 0x14, 0x18) };
    t.header_sel_text = readable(on_chrome, t.header_sel_bg, 4.5);
    t.header_all_bg = mix(on_chrome, t.ribbon, 0.72);
    // The sheet is white in both modes: its selection needs 3 against white.
    let on_sheet = readable(host, Color32::WHITE, 3.0);
    t.sel_border = on_sheet;
    t.sheet_fill = on_sheet;
    t.sheet_focus = on_sheet;
    t.sel_fill = Color32::from_rgba_unmultiplied(on_sheet.r(), on_sheet.g(), on_sheet.b(), t.sel_fill.a());
    t.active_row = Color32::from_rgba_unmultiplied(on_sheet.r(), on_sheet.g(), on_sheet.b(), t.active_row.a());
    t
}

/// The host's corner radius on buttons and fields (windows and menus keep their own).
pub fn style(v: &mut egui::Visuals) {
    let Some(r) = radius() else { return };
    let radius = egui::CornerRadius::same(r);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = radius;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_host_colour_the_palette_is_untouched() {
        set_accent(None);
        assert_eq!(themed(Tokens::light()), Tokens::light());
        assert_eq!(themed(Tokens::dark()), Tokens::dark());
    }

    #[test]
    fn any_host_colour_gives_readable_tokens_in_both_modes() {
        // Taskforce's own blue, then colours a host should not pick but may: pale, near black, near white, mid grey.
        for rgb in [[0x25, 0x63, 0xEB], [0xFF, 0xF2, 0x9A], [0x05, 0x05, 0x08], [0xFA, 0xFA, 0xFA], [0x80, 0x80, 0x80], [0xE1, 0x1D, 0x48]] {
            set_accent(Some(rgb));
            for dark in [false, true] {
                let t = if dark { Tokens::dark() } else { Tokens::light() };
                let what = format!("{rgb:?} dark={dark}");
                assert!(contrast(t.accent, t.window) >= 4.5, "accent on window, {what}");
                assert!(contrast(t.accent, t.ribbon) >= 4.5, "accent on ribbon, {what}");
                assert!(contrast(t.on_accent, t.accent) >= 4.5, "text on accent, {what}");
                assert!(contrast(t.header_sel_text, t.header_sel_bg) >= 4.5, "selected header, {what}");
                assert!(contrast(t.sel_border, Color32::WHITE) >= 3.0, "selection on the sheet, {what}");
                assert!(contrast(t.text, t.accent_soft) >= 4.5, "text on a soft accent row, {what}");
            }
        }
        set_accent(None);
    }

    #[test]
    fn the_radius_is_bounded() {
        set_radius(Some(6.4));
        assert_eq!(radius(), Some(6));
        set_radius(Some(400.0));
        assert_eq!(radius(), Some(MAX_RADIUS));
        set_radius(Some(-3.0));
        assert_eq!(radius(), Some(0));
        set_radius(Some(f32::NAN));
        assert_eq!(radius(), None);
        let mut v = egui::Visuals::light();
        set_radius(Some(8.0));
        style(&mut v);
        assert_eq!(v.widgets.inactive.corner_radius, egui::CornerRadius::same(8));
        set_radius(None);
    }
}
