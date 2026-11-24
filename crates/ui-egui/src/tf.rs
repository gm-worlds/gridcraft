//! Taskforce cell kinds on the grid (`gridcraft_tf_ext`): how a marked cell is drawn and what a
//! click on it does. The grid calls [`draws`], [`paint`] and [`click`] from one line each; all the
//! rules live here.

use egui::{Color32, Painter, Pos2, Rect, Shape, pos2, vec2};
use gridcraft_engine::core::{CellRef, Value};
use gridcraft_tf_ext::{self as tf, Kind};
use serde_json::json;

use crate::SheetApp;

/// Empty stars and their outline: the sheet stays light in both themes.
const STAR_EMPTY: Color32 = Color32::from_rgb(0xD5, 0xD9, 0xDE);

/// Whether a cell with this number format and value is drawn by [`paint`] instead of as text.
pub fn draws(code: &str, value: &Value) -> bool {
    match tf::classify(code) {
        Some(Kind::Rating { .. }) => matches!(value, Value::Number(_) | Value::Empty),
        _ => false,
    }
}

/// Draws a marked cell. Returns whether it drew (the cell's text is then left out).
pub fn paint(p: &Painter, rect: Rect, z: f32, code: &str, value: &Value, accent: Color32) -> bool {
    match tf::classify(code) {
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
        Some(Kind::Rating { max }) if draws(code, value) => {
            let Some(star) = (0..max).find(|i| hit_rect(rect, z, *i, max).contains(pos)) else { return false };
            let _ = app.run("tf.rating.click", json!({ "cell": c.a1(), "star": star + 1 }));
            true
        }
        _ => false,
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
