//! Rich text runs of a shared string item: `<si><r><rPr>…</rPr><t>…</t></r>…</si>`.
//!
//! Read into [`gridcraft_model::RichStrings`] and written back from it, so character formatting
//! inside a cell survives an open + save. Behaviour reference: ISO/IEC 29500-1 §18.4.4 `r`,
//! §18.4.7 `rPr` and §18.4.8 `si`; the Microsoft Learn `Run` and `RunProperties` classes.

use std::fmt::Write as _;

use gridcraft_model::rich::{RichRun, byte_offset, normalize_runs, utf16_len};
use gridcraft_model::workbook::PhoneticString;

use crate::xml::{self, El};

/// A boolean run property (`<b/>`, `<i val="0"/>`…): present means on unless `val` says off.
fn on(rpr: &El, name: &str) -> bool {
    rpr.child(name).is_some_and(|e| e.flag("val", true))
}

/// `rgb="AARRGGBB"` (or `RRGGBB`) of a run's `<color>`. Theme and indexed colours are not read:
/// the run then has the cell's colour.
fn color(rpr: &El) -> Option<[u8; 3]> {
    let rgb = rpr.child("color")?.attr("rgb")?.trim();
    let hex = match rgb.len() {
        8 => rgb.get(2..)?,
        6 => rgb,
        _ => return None,
    };
    let byte = |i: usize| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok());
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// The formatted stretches of a string item whose base text is `base` ([`xml::rich_text`]).
/// Empty for a plain item, and when the runs do not add up to the base text (an escape split
/// across two runs): formatting that cannot be placed is dropped, never guessed.
pub fn runs_of(si: &El, base: &str) -> Vec<RichRun> {
    if si.child("r").is_none() {
        return Vec::new();
    }
    let mut runs = Vec::new();
    let mut at = 0u32;
    for c in &si.children {
        let text = match c.name.as_str() {
            "t" => xml::decode_xstring(&c.text),
            "r" => xml::decode_xstring(&c.kids("t").map(|t| t.text.as_str()).collect::<String>()),
            _ => continue,
        };
        let end = at.saturating_add(utf16_len(&text));
        if let Some(rpr) = c.child("rPr").filter(|_| c.name == "r") {
            let underline = rpr.child("u").is_some_and(|u| u.attr("val").is_none_or(|v| v != "none"));
            runs.push(RichRun { start: at, end, bold: on(rpr, "b"), italic: on(rpr, "i"), underline, strike: on(rpr, "strike"), color: color(rpr) });
        }
        at = end;
    }
    if at != utf16_len(base) {
        return Vec::new();
    }
    normalize_runs(base, runs)
}

/// True when every character of the item sits in a run with run properties: then the runs say
/// how the whole text looks and the cell's own font says nothing about it. LibreOffice writes
/// items this way, and makes the cell's font that of the first run.
pub fn fully_formatted(si: &El) -> bool {
    let mut any = false;
    for c in &si.children {
        match c.name.as_str() {
            "t" if !c.text.is_empty() => return false,
            "r" => {
                if c.child("rPr").is_none() && c.kids("t").any(|t| !t.text.is_empty()) {
                    return false;
                }
                any = true;
            }
            _ => {}
        }
    }
    any
}

/// Cells whose text is formatted throughout by its runs (`texts`, [`fully_formatted`]) lose the
/// bold, italic, underline, strike and colour of their own font. GridCraft draws a run's
/// formatting on top of the cell's font; in the file a run's font replaces it. Without this a
/// workbook saved by LibreOffice, with part of a text bold, came back with the whole text bold.
pub fn settle_cell_fonts(wb: &mut gridcraft_model::Workbook, texts: &[String]) {
    use gridcraft_core::Value;
    use gridcraft_model::{Color, Underline};
    if texts.is_empty() {
        return;
    }
    let texts: std::collections::HashSet<&str> = texts.iter().map(String::as_str).collect();
    for si in 0..wb.sheets.len() {
        let mut changes = Vec::new();
        if let Some(sh) = wb.sheets.get(si) {
            for (at, cell) in sh.cells.iter() {
                let Value::Text(t) = &cell.value else { continue };
                if cell.formula.is_some() || !texts.contains(t.as_ref()) || wb.rich.runs(t).is_empty() {
                    continue;
                }
                let style = wb.styles.get(cell.style);
                let f = &style.font;
                if !(f.bold || f.italic || f.strike || f.underline != Underline::None || f.color != Color::Auto) {
                    continue;
                }
                let mut plain = style.clone();
                plain.font.bold = false;
                plain.font.italic = false;
                plain.font.strike = false;
                plain.font.underline = Underline::None;
                plain.font.color = Color::Auto;
                changes.push((at, plain));
            }
        }
        if changes.is_empty() {
            continue;
        }
        let ids: Vec<_> = changes.into_iter().map(|(at, style)| (at, wb.styles.intern(style))).collect();
        if let Some(sh) = wb.sheets.get_mut(si) {
            let sh = std::sync::Arc::make_mut(sh);
            for (at, id) in ids {
                sh.set_style(at, id);
            }
        }
    }
}

fn rpr(r: &RichRun) -> String {
    let mut s = String::from("<rPr>");
    for (is_on, tag) in [(r.bold, "<b/>"), (r.italic, "<i/>"), (r.strike, "<strike/>"), (r.underline, "<u/>")] {
        if is_on {
            s.push_str(tag);
        }
    }
    if let Some([red, green, blue]) = r.color {
        let _ = write!(s, "<color rgb=\"FF{red:02X}{green:02X}{blue:02X}\"/>");
    }
    s.push_str("</rPr>");
    s
}

/// The contents of a shared string item: like [`xml::si_body`], with the text written as runs when
/// it has formatting. The runs are normalized again here: the model is public and restored from
/// JSON too, and what is written must be valid whatever it holds.
pub fn si_body(text: &str, runs: &[RichRun], phonetic: Option<&PhoneticString>) -> String {
    let plain = xml::si_body(text, phonetic);
    let runs = normalize_runs(text, runs.to_vec());
    if runs.is_empty() {
        return plain;
    }
    let head = xml::t_el(text);
    // What follows the text in a plain item (phonetic runs and properties) follows the runs too.
    let tail = plain.strip_prefix(head.as_str()).unwrap_or_default();
    // Runs keep their spaces: every `<t>` of a run is written space-preserving, as Excel does.
    let t = |part: &str| format!("<t xml:space=\"preserve\">{}</t>", xml::esc(&xml::encode_xstring(part)));
    let mut s = String::new();
    let mut at = 0usize;
    for r in &runs {
        let (from, to) = (byte_offset(text, r.start), byte_offset(text, r.end));
        if from < at || to <= from {
            continue;
        }
        if let Some(gap) = text.get(at..from).filter(|g| !g.is_empty()) {
            let _ = write!(s, "<r>{}</r>", t(gap));
        }
        if let Some(part) = text.get(from..to) {
            let _ = write!(s, "<r>{}{}</r>", rpr(r), t(part));
        }
        at = to;
    }
    if let Some(rest) = text.get(at..).filter(|g| !g.is_empty()) {
        let _ = write!(s, "<r>{}</r>", t(rest));
    }
    s.push_str(tail);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(si: &str) -> Result<El, Box<dyn std::error::Error>> {
        let mut got = None;
        xml::parse_streaming(format!("<sst>{si}</sst>").as_bytes(), &["si"], &mut |e| {
            got = Some(e);
            Ok(())
        })?;
        got.ok_or_else(|| "no si".into())
    }

    #[test]
    fn runs_that_format_the_whole_text_overrule_the_cells_font() -> Result<(), Box<dyn std::error::Error>> {
        use gridcraft_core::{CellRef, Value};
        use gridcraft_model::{Cell, Color, Sheet, Style, Workbook};
        // As LibreOffice writes "Ship by Friday" with "Ship" bold: every run has properties.
        let throughout = parse("<si><r><rPr><b/><sz val=\"11\"/></rPr><t>Ship</t></r><r><rPr><sz val=\"11\"/></rPr><t> by Friday</t></r></si>")?;
        assert!(fully_formatted(&throughout));
        // As GridCraft writes it: the plain part has none and takes the cell's font.
        assert!(!fully_formatted(&parse("<si><r><rPr><b/></rPr><t>Ship</t></r><r><t> by Friday</t></r></si>")?));
        assert!(!fully_formatted(&parse("<si><t>Ship by Friday</t></si>")?));

        let mut wb = Workbook::new();
        let mut bold = Style::default();
        bold.font.bold = true;
        bold.font.color = Color::Rgb(0x00C8_0000);
        bold.font.size = 14.0;
        let bold_id = wb.styles.intern(bold);
        let cell = |text: &str| Cell { value: Value::Text(text.into()), style: bold_id, ..Cell::default() };
        let (a1, a2, a3) = (CellRef::parse("A1").ok_or("a1")?, CellRef::parse("A2").ok_or("a2")?, CellRef::parse("A3").ok_or("a3")?);
        let mut sh = Sheet::new("S");
        sh.set_cell(a1, cell("Ship by Friday"));
        sh.set_cell(a2, cell("A bold heading"));
        sh.set_cell(a3, cell("No runs kept"));
        wb.sheets = vec![std::sync::Arc::new(sh)];
        std::sync::Arc::make_mut(&mut wb.rich).set("Ship by Friday", vec![RichRun { start: 0, end: 4, bold: true, ..RichRun::default() }]);
        settle_cell_fonts(&mut wb, &["Ship by Friday".to_string(), "No runs kept".to_string()]);
        let font = |at| wb.styles.get(wb.sheets[0].style_id(at)).font.clone();
        // The cell is no longer bold or red (its runs say what is), and keeps its size.
        assert!(!font(a1).bold && font(a1).color == Color::Auto && font(a1).size == 14.0);
        // A cell that is simply bold stays bold, and so does one whose text has no runs.
        assert!(font(a2).bold && font(a3).bold);
        Ok(())
    }

    #[test]
    fn runs_round_trip_through_a_string_item() -> Result<(), Box<dyn std::error::Error>> {
        let text = "Ship 😀 by Friday, not later";
        let runs = vec![
            RichRun { start: 0, end: 4, bold: true, ..RichRun::default() },
            RichRun { start: 11, end: 17, italic: true, underline: true, color: Some([0xC0, 0x39, 0x2B]), ..RichRun::default() },
            RichRun { start: 19, end: 28, strike: true, ..RichRun::default() },
        ];
        let body = si_body(text, &runs, None);
        assert!(body.starts_with("<r><rPr><b/></rPr><t xml:space=\"preserve\">Ship</t></r><r><t xml:space=\"preserve\"> 😀 by </t></r>"), "{body}");
        assert!(body.contains("<rPr><i/><u/><color rgb=\"FFC0392B\"/></rPr>"), "{body}");
        let si = parse(&format!("<si>{body}</si>"))?;
        assert_eq!(xml::rich_text(&si), text);
        assert_eq!(runs_of(&si, text), runs);
        Ok(())
    }

    #[test]
    fn plain_items_and_misplaced_runs_have_no_formatting() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(si_body("plain", &[], None), xml::t_el("plain"));
        // A run that is not styled, or lies outside the text, writes a plain item.
        assert_eq!(si_body("plain", &[RichRun { start: 9, end: 12, bold: true, ..RichRun::default() }], None), xml::t_el("plain"));
        let si = parse("<si><t>plain</t></si>")?;
        assert!(runs_of(&si, "plain").is_empty());
        // Runs that do not add up to the base text are dropped.
        let si = parse("<si><r><rPr><b/></rPr><t>ab</t></r><r><t>cd</t></r></si>")?;
        assert!(runs_of(&si, "abc").is_empty());
        assert_eq!(runs_of(&si, "abcd"), vec![RichRun { start: 0, end: 2, bold: true, ..RichRun::default() }]);
        Ok(())
    }

    #[test]
    fn run_properties_read_the_forms_files_use() -> Result<(), Box<dyn std::error::Error>> {
        let si = parse(
            "<si><r><rPr><b val=\"0\"/><i val=\"1\"/><u val=\"none\"/><color theme=\"1\"/><sz val=\"11\"/></rPr><t>ab</t></r><r><rPr><u val=\"double\"/><strike/><color rgb=\"00ff00\"/></rPr><t>cd</t></r><r><rPr><color rgb=\"zz\"/></rPr><t>e</t></r></si>",
        )?;
        assert_eq!(
            runs_of(&si, "abcde"),
            vec![
                RichRun { start: 0, end: 2, italic: true, ..RichRun::default() },
                RichRun { start: 2, end: 4, underline: true, strike: true, color: Some([0, 0xFF, 0]), ..RichRun::default() },
            ]
        );
        Ok(())
    }

    #[test]
    fn phonetic_data_follows_the_runs() {
        use gridcraft_model::workbook::PhoneticRun;
        let phonetic =
            PhoneticString { text: "東京".into(), runs: vec![PhoneticRun { start: 0, end: 2, text: "トウキョウ".into() }], props: None };
        let body = si_body("東京", &[RichRun { start: 0, end: 1, bold: true, ..RichRun::default() }], Some(&phonetic));
        assert!(body.starts_with("<r><rPr><b/></rPr>"), "{body}");
        assert!(body.ends_with("<rPh sb=\"0\" eb=\"2\"><t>トウキョウ</t></rPh>"), "{body}");
    }
}
