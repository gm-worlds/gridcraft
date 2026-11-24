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
