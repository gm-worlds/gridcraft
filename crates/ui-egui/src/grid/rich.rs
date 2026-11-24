//! Rich text in the grid: a cell whose text has character formatting
//! ([`gridcraft_engine::model::RichStrings`]) is laid out as one section per formatted stretch.

use egui::text::{ByteIndex, LayoutJob, LayoutSection, TextFormat};
use egui::{Color32, FontId, Stroke};
use gridcraft_engine::core::Value;
use gridcraft_engine::model::rich::{RichRun, byte_offset};
use gridcraft_engine::model::{Font, Workbook};

use crate::theme;

/// The formatting of one cell's text, ready to restyle the layout jobs made for it.
pub(super) struct Runs<'a> {
    text: &'a str,
    runs: &'a [RichRun],
    font: &'a Font,
    size: f32,
}

impl<'a> Runs<'a> {
    /// `None` for a cell without formatting inside its text (nearly every cell: one map lookup,
    /// and none at all in a workbook without rich text).
    pub(super) fn of(wb: &'a Workbook, value: &'a Value, font: &'a Font, size: f32) -> Option<Runs<'a>> {
        let Value::Text(text) = value else { return None };
        let runs = wb.rich.runs(text);
        (!runs.is_empty()).then_some(Runs { text, runs, font, size })
    }

    /// Splits `job` into sections when it lays out exactly the cell's text. A job for something
    /// else (the text with line breaks removed, `####`, a repeated fill character) is left alone:
    /// the offsets would not fit it.
    fn restyle(&self, job: &mut LayoutJob, color: Color32) {
        if job.text != self.text {
            return;
        }
        job.sections = sections(self.text, self.runs, self.font, self.size, color);
    }
}

/// [`Runs::restyle`] for a cell that may have no formatting (the call sites stay one line).
pub(super) fn restyle(rich: Option<&Runs<'_>>, job: &mut LayoutJob, color: Color32) {
    if let Some(rich) = rich {
        rich.restyle(job, color);
    }
}

fn format(font: &Font, size: f32, color: Color32, run: Option<&RichRun>) -> TextFormat {
    let (bold, italic) = (font.bold || run.is_some_and(|r| r.bold), font.italic || run.is_some_and(|r| r.italic));
    let ink = run.and_then(|r| r.color).map_or(color, |[r, g, b]| Color32::from_rgb(r, g, b));
    let mut f = TextFormat::simple(FontId::new(size, theme::cell_family(&font.name, bold, italic)), ink);
    // The cell's own underline and strike are drawn over the whole text by the cell painter.
    let line = Stroke::new((size / 14.0).max(1.0), ink);
    if run.is_some_and(|r| r.underline) {
        f.underline = line;
    }
    if run.is_some_and(|r| r.strike) {
        f.strikethrough = line;
    }
    f
}

/// One section per stretch of `text`: the formatted runs, and the plain text between them.
pub(crate) fn sections(text: &str, runs: &[RichRun], font: &Font, size: f32, color: Color32) -> Vec<LayoutSection> {
    let mut out = Vec::with_capacity(runs.len() * 2 + 1);
    let mut push = |from: usize, to: usize, run: Option<&RichRun>| {
        if from < to {
            out.push(LayoutSection { leading_space: 0.0, byte_range: ByteIndex(from)..ByteIndex(to), format: format(font, size, color, run) });
        }
    };
    let mut at = 0usize;
    for r in runs {
        // Runs are normalized when stored; offsets are checked again because the model is public.
        let (from, to) = (byte_offset(text, r.start).max(at), byte_offset(text, r.end));
        if to <= from {
            continue;
        }
        push(at, from, None);
        push(from, to, Some(r));
        at = to;
    }
    push(at, text.len(), None);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_cover_the_text_once_in_order() {
        let text = "Ship 😀 by Friday";
        let runs = [
            RichRun { start: 0, end: 4, bold: true, ..RichRun::default() },
            RichRun { start: 11, end: 17, italic: true, underline: true, strike: true, color: Some([200, 0, 0]), ..RichRun::default() },
        ];
        let font = Font::default();
        let got = sections(text, &runs, &font, 14.0, Color32::BLACK);
        let ranges: Vec<_> = got.iter().map(|s| s.byte_range.start.0..s.byte_range.end.0).collect();
        assert_eq!(ranges, vec![0..4, 4..13, 13..text.len()]);
        assert_eq!(&text[13..], "Friday");
        assert_eq!(got[2].format.color, Color32::from_rgb(200, 0, 0));
        assert!(got[2].format.underline.width > 0.0 && got[2].format.strikethrough.width > 0.0);
        assert_eq!(got[1].format.underline, Stroke::NONE);
        // Hostile runs (out of order, past the end, inside a character) never give a bad range.
        let wild = [
            RichRun { start: 6, end: 3, bold: true, ..RichRun::default() },
            RichRun { start: 5, end: 6, bold: true, ..RichRun::default() },
            RichRun { start: 90, end: 99, bold: true, ..RichRun::default() },
        ];
        let got = sections(text, &wild, &font, 14.0, Color32::BLACK);
        let mut end = 0;
        for s in &got {
            assert_eq!(s.byte_range.start.0, end);
            assert!(text.is_char_boundary(s.byte_range.start.0) && text.is_char_boundary(s.byte_range.end.0));
            end = s.byte_range.end.0;
        }
        assert_eq!(end, text.len());
    }
}
