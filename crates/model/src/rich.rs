//! Rich text: character formatting inside one cell's text (`<r><rPr>…</rPr><t>…</t></r>` runs of
//! a SpreadsheetML string item).
//!
//! A cell keeps only its text, so the formatting is kept per text in [`RichStrings`], the way
//! phonetic data is ([`crate::workbook::PhoneticString`]): it follows the text through sorting,
//! moving, copying and filling without any of those having to know about it. Two cells with the
//! same text therefore share one formatting.
//!
//! Behaviour reference: ISO/IEC 29500-1 §18.4.4 `r` and §18.4.7 `rPr` (run properties), and the
//! Microsoft Learn `Run` and `RunProperties` classes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Most formatted stretches kept for one text; more are dropped from the end.
pub const MAX_RUNS: usize = 1024;
/// Most texts a workbook keeps formatting for.
pub const MAX_STRINGS: usize = 100_000;

fn is_false(b: &bool) -> bool {
    !*b
}

/// One formatted stretch of a text: `start..end` in UTF-16 code units (the unit SpreadsheetML uses
/// for offsets into a string), with `start < end`. Text outside every run has the cell's own font.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichRun {
    pub start: u32,
    pub end: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub strike: bool,
    /// Text colour (red, green, blue); `None` is the cell's colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 3]>,
}

impl RichRun {
    /// Whether the run changes how its text looks. One that does not is not kept.
    pub fn is_styled(&self) -> bool {
        self.bold || self.italic || self.underline || self.strike || self.color.is_some()
    }

    fn same_style(&self, other: &RichRun) -> bool {
        (self.bold, self.italic, self.underline, self.strike, self.color) == (other.bold, other.italic, other.underline, other.strike, other.color)
    }
}

/// Length of a text in UTF-16 code units, saturating.
pub fn utf16_len(text: &str) -> u32 {
    u32::try_from(text.encode_utf16().count()).unwrap_or(u32::MAX)
}

/// Runs made safe for `text`: inside it, styled, in order, not overlapping (a run that starts
/// inside the one before it is cut to start where that one ends), neighbours with the same style
/// joined, and at most [`MAX_RUNS`]. Input from files, JSON and commands all passes through here.
pub fn normalize_runs(text: &str, mut runs: Vec<RichRun>) -> Vec<RichRun> {
    let len = utf16_len(text);
    runs.retain(|r| r.is_styled() && r.start < r.end && r.start < len);
    runs.sort_by_key(|r| (r.start, r.end));
    let mut out: Vec<RichRun> = Vec::with_capacity(runs.len().min(MAX_RUNS));
    for mut r in runs {
        r.end = r.end.min(len);
        if let Some(last) = out.last_mut() {
            r.start = r.start.max(last.end);
            if r.start >= r.end {
                continue;
            }
            if last.end == r.start && last.same_style(&r) {
                last.end = r.end;
                continue;
            }
        }
        if out.len() >= MAX_RUNS {
            break;
        }
        out.push(r);
    }
    out
}

/// A change to the formatting of a stretch of text: each `Some` sets that property, `None` leaves
/// it as it is. `color: Some(None)` takes the colour away.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RichPatch {
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strike: Option<bool>,
    pub color: Option<Option<[u8; 3]>>,
}

impl RichPatch {
    pub fn is_empty(&self) -> bool {
        *self == RichPatch::default()
    }

    fn apply(&self, r: &mut RichRun) {
        r.bold = self.bold.unwrap_or(r.bold);
        r.italic = self.italic.unwrap_or(r.italic);
        r.underline = self.underline.unwrap_or(r.underline);
        r.strike = self.strike.unwrap_or(r.strike);
        if let Some(color) = self.color {
            r.color = color;
        }
    }
}

/// `runs` of `text` after `patch` is applied to `start..end` (UTF-16 units, clamped to the text).
pub fn patch_runs(text: &str, runs: &[RichRun], start: u32, end: u32, patch: &RichPatch) -> Vec<RichRun> {
    let runs = normalize_runs(text, runs.to_vec());
    let len = utf16_len(text);
    let (start, end) = (start.min(len), end.min(len));
    if start >= end || patch.is_empty() {
        return runs;
    }
    let mut cuts: Vec<u32> = runs.iter().flat_map(|r| [r.start, r.end]).chain([0, len, start, end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::with_capacity(cuts.len());
    for pair in cuts.windows(2) {
        let (Some(&a), Some(&b)) = (pair.first(), pair.get(1)) else { continue };
        let mut piece = runs.iter().find(|r| r.start <= a && b <= r.end).cloned().unwrap_or_default();
        (piece.start, piece.end) = (a, b);
        if start <= a && b <= end {
            patch.apply(&mut piece);
        }
        out.push(piece);
    }
    normalize_runs(text, out)
}

/// The runs of a text after it was edited from `old` to `new` (typing, deleting, pasting over a
/// selection): what lies before the change stays, what lies after moves with it, and what was
/// typed takes the formatting of the character before it (of the character after it at the very
/// start of the text), as a word processor does.
pub fn edit_runs(old: &str, new: &str, runs: &[RichRun]) -> Vec<RichRun> {
    if old == new {
        return normalize_runs(new, runs.to_vec());
    }
    let units = |n: usize, text: &str, from_end: bool| -> u32 {
        let chars: Box<dyn Iterator<Item = char>> = if from_end { Box::new(text.chars().rev()) } else { Box::new(text.chars()) };
        chars.take(n).map(|c| c.len_utf16() as u32).sum()
    };
    let prefix_chars = old.chars().zip(new.chars()).take_while(|(a, b)| a == b).count();
    let room = old.chars().count().min(new.chars().count()) - prefix_chars;
    let suffix_chars = old.chars().rev().zip(new.chars().rev()).take(room).take_while(|(a, b)| a == b).count();
    // In UTF-16 units: the unchanged start `p`, the end of what was replaced in `old`, and the
    // length of what replaced it.
    let p = units(prefix_chars, old, false);
    let gone_end = utf16_len(old).saturating_sub(units(suffix_chars, old, true));
    let inserted = utf16_len(new).saturating_sub(units(suffix_chars, new, true)).saturating_sub(p);
    let typed_end = p.saturating_add(inserted);
    let after = |x: u32| x.saturating_sub(gone_end).saturating_add(typed_end);
    let pure_insert = gone_end == p;
    let moved = runs
        .iter()
        .map(|r| {
            let start = if r.start < p || (r.start == p && (p == 0 || !pure_insert)) {
                r.start.min(p)
            } else if r.start >= gone_end {
                after(r.start)
            } else {
                typed_end
            };
            let end = if r.end < p {
                r.end
            } else if r.end > gone_end || (r.end == gone_end && !pure_insert && r.start >= p) {
                after(r.end)
            } else if r.end == p && r.start >= p {
                // An empty run at the change (its text was replaced): nothing left of it.
                start
            } else {
                // The change is inside the run or right at its end: what was typed joins it.
                typed_end.max(if r.end >= gone_end { after(r.end) } else { 0 })
            };
            RichRun { start, end, ..r.clone() }
        })
        .collect();
    normalize_runs(new, moved)
}

/// Whether every character of `start..end` has the property `has` picks (what a toggle button
/// shows as on). An empty stretch has nothing.
pub fn all_of(text: &str, runs: &[RichRun], start: u32, end: u32, has: impl Fn(&RichRun) -> bool) -> bool {
    let len = utf16_len(text);
    let (start, end) = (start.min(len), end.min(len));
    if start >= end {
        return false;
    }
    let mut at = start;
    for r in runs.iter().filter(|r| has(r)) {
        if r.start > at {
            break;
        }
        at = at.max(r.end);
        if at >= end {
            return true;
        }
    }
    false
}

/// The byte offset in `text` of a UTF-16 offset, moved back to a character boundary when it falls
/// inside a surrogate pair; the text's length when it is past the end.
pub fn byte_offset(text: &str, utf16: u32) -> usize {
    let mut units = 0u32;
    for (i, ch) in text.char_indices() {
        let next = units.saturating_add(ch.len_utf16() as u32);
        if next > utf16 {
            return i;
        }
        units = next;
    }
    text.len()
}

/// The formatting of a workbook's texts, by text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RichStrings(BTreeMap<String, Vec<RichRun>>);

impl RichStrings {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The formatted stretches of `text`; empty when it has none.
    pub fn runs(&self, text: &str) -> &[RichRun] {
        if self.0.is_empty() {
            return &[];
        }
        self.0.get(text).map_or(&[], Vec::as_slice)
    }

    /// Sets the formatting of `text` (normalized); no styled run left means no formatting. Returns
    /// whether it was stored: a full table takes no new text.
    pub fn set(&mut self, text: &str, runs: Vec<RichRun>) -> bool {
        let runs = normalize_runs(text, runs);
        if runs.is_empty() {
            self.0.remove(text);
            return true;
        }
        if self.0.len() >= MAX_STRINGS && !self.0.contains_key(text) {
            return false;
        }
        self.0.insert(text.to_string(), runs);
        true
    }

    /// Like [`RichStrings::set`], but a text that already has formatting keeps it (a file whose
    /// string table holds one text twice: the first wins, as for phonetic data).
    pub fn set_if_absent(&mut self, text: &str, runs: Vec<RichRun>) -> bool {
        if self.0.contains_key(text) {
            return false;
        }
        self.set(text, runs) && self.0.contains_key(text)
    }

    pub fn remove(&mut self, text: &str) {
        self.0.remove(text);
    }

    /// Drops the formatting of every text `used` says no cell holds any more.
    pub fn retain_texts(&mut self, mut used: impl FnMut(&str) -> bool) {
        self.0.retain(|text, _| used(text));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[RichRun])> {
        self.0.iter().map(|(t, r)| (t.as_str(), r.as_slice()))
    }

    /// What was read from untrusted JSON, made safe.
    pub fn sanitized(self) -> RichStrings {
        let mut out = RichStrings::default();
        for (text, runs) in self.0.into_iter().take(MAX_STRINGS) {
            out.set(&text, runs);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(start: u32, end: u32) -> RichRun {
        RichRun { start, end, bold: true, ..RichRun::default() }
    }

    #[test]
    fn runs_are_clamped_ordered_and_joined() {
        let text = "Hello world";
        let got = normalize_runs(
            text,
            vec![run(6, 99), run(0, 3), run(3, 5), run(2, 4), RichRun { start: 0, end: 5, ..RichRun::default() }, run(50, 60), run(7, 7)],
        );
        assert_eq!(got, vec![run(0, 5), run(6, 11)]);
        let italic = RichRun { start: 3, end: 8, italic: true, ..RichRun::default() };
        assert_eq!(normalize_runs(text, vec![run(0, 5), italic.clone()]), vec![run(0, 5), RichRun { start: 5, ..italic }]);
        assert!(normalize_runs("", vec![run(0, 1)]).is_empty());
        assert_eq!(normalize_runs(&"x".repeat(5000), (0..3000).map(|i| run(i, i + 1)).collect()), vec![run(0, 3000)]);
        let many: Vec<RichRun> =
            (0..3000u32).map(|i| RichRun { start: i, end: i + 1, bold: i % 2 == 0, italic: i % 2 == 1, ..RichRun::default() }).collect();
        assert_eq!(normalize_runs(&"x".repeat(5000), many).len(), MAX_RUNS);
    }

    #[test]
    fn a_patch_formats_only_its_stretch() {
        let text = "Hello world";
        let bold = RichPatch { bold: Some(true), ..RichPatch::default() };
        let a = patch_runs(text, &[], 0, 5, &bold);
        assert_eq!(a, vec![run(0, 5)]);
        // Italic over "lo wo" splits the bold run and leaves the rest alone.
        let b = patch_runs(text, &a, 3, 8, &RichPatch { italic: Some(true), ..RichPatch::default() });
        assert_eq!(
            b,
            vec![
                run(0, 3),
                RichRun { start: 3, end: 5, bold: true, italic: true, ..RichRun::default() },
                RichRun { start: 5, end: 8, italic: true, ..RichRun::default() }
            ]
        );
        // Taking bold off everywhere leaves the italic stretch as one run.
        let c = patch_runs(text, &b, 0, 99, &RichPatch { bold: Some(false), ..RichPatch::default() });
        assert_eq!(c, vec![RichRun { start: 3, end: 8, italic: true, ..RichRun::default() }]);
        let red = RichPatch { color: Some(Some([200, 0, 0])), ..RichPatch::default() };
        let d = patch_runs(text, &c, 6, 11, &red);
        assert_eq!(d.len(), 3);
        assert_eq!(patch_runs(text, &d, 0, 11, &RichPatch { color: Some(None), ..RichPatch::default() }), c);
        // Nothing to do: an empty stretch, an empty patch.
        assert_eq!(patch_runs(text, &c, 4, 4, &bold), c);
        assert_eq!(patch_runs(text, &c, 0, 5, &RichPatch::default()), c);
        assert!(all_of(text, &b, 0, 5, |r| r.bold));
        assert!(!all_of(text, &b, 0, 6, |r| r.bold));
        assert!(all_of(text, &b, 3, 8, |r| r.italic));
        assert!(!all_of(text, &b, 2, 8, |r| r.italic));
        assert!(!all_of(text, &b, 4, 4, |r| r.bold));
    }

    #[test]
    fn runs_follow_edits_of_the_text() {
        let bold = |start, end| vec![run(start, end)];
        // "Hello world", "world" bold.
        let old = "Hello world";
        let runs = bold(6, 11);
        // Typing before the run moves it; typing right before it does not join it.
        assert_eq!(edit_runs(old, "Oh Hello world", &runs), bold(9, 14));
        assert_eq!(edit_runs(old, "Hello big world", &runs), bold(10, 15));
        // Typing inside or at the end of the run joins it.
        assert_eq!(edit_runs(old, "Hello wooorld", &runs), bold(6, 13));
        assert_eq!(edit_runs(old, "Hello worlds!", &runs), bold(6, 13));
        // Deleting inside shrinks it; deleting all of it removes it.
        assert_eq!(edit_runs(old, "Hello wld", &runs), bold(6, 9));
        assert_eq!(edit_runs(old, "Hello ", &runs), vec![]);
        assert_eq!(edit_runs(old, "Hello", &runs), vec![]);
        // Deleting before it moves it back.
        assert_eq!(edit_runs(old, "Ho world", &runs), bold(3, 8));
        // Replacing a selection across its start: the typed text takes the left side's (plain) look.
        assert_eq!(edit_runs(old, "HelXXrld", &runs), bold(5, 8));
        // A run at the very start: typing at position 0 joins it (nothing to its left).
        assert_eq!(edit_runs("Bold tail", "A Bold tail", &bold(0, 4)), bold(0, 6));
        // Replacing everything keeps nothing; the same text keeps everything.
        assert_eq!(edit_runs(old, "Other", &runs), vec![]);
        assert_eq!(edit_runs(old, old, &runs), runs);
        // Characters outside the BMP count two units.
        assert_eq!(edit_runs("a😀b", "a😀😀b", &bold(3, 4)), bold(5, 6));
        assert_eq!(edit_runs("", "new", &[]), vec![]);
        // Never out of range, whatever the edit.
        for new in ["", "x", "Hello world and much more text", "world", "Hello"] {
            for r in edit_runs(old, new, &[run(0, 5), run(6, 11)]) {
                assert!(r.start < r.end && r.end <= utf16_len(new), "{new}: {r:?}");
            }
        }
    }

    #[test]
    fn offsets_are_utf16_and_never_split_a_character() {
        let text = "a😀é";
        assert_eq!(utf16_len(text), 4);
        assert_eq!(byte_offset(text, 0), 0);
        assert_eq!(byte_offset(text, 1), 1);
        // Inside the emoji's surrogate pair: back to its start.
        assert_eq!(byte_offset(text, 2), 1);
        assert_eq!(byte_offset(text, 3), 5);
        assert_eq!(byte_offset(text, 4), text.len());
        assert_eq!(byte_offset(text, 99), text.len());
    }

    #[test]
    fn table_sets_replaces_and_forgets() -> Result<(), Box<dyn std::error::Error>> {
        let mut t = RichStrings::default();
        assert!(t.runs("Hello").is_empty());
        assert!(t.set("Hello", vec![run(0, 2)]));
        assert_eq!(t.runs("Hello"), &[run(0, 2)]);
        assert!(!t.set_if_absent("Hello", vec![run(1, 3)]));
        assert_eq!(t.runs("Hello"), &[run(0, 2)]);
        assert!(t.set("Hello", vec![]));
        assert!(t.is_empty());
        t.set("a", vec![run(0, 1)]);
        t.set("b", vec![run(0, 1)]);
        t.retain_texts(|text| text == "b");
        assert_eq!(t.iter().map(|(text, _)| text).collect::<Vec<_>>(), vec!["b"]);
        // JSON is the map itself, and damaged JSON is made safe.
        let json = serde_json::to_string(&t)?;
        assert_eq!(json, r#"{"b":[{"start":0,"end":1,"bold":true}]}"#);
        let hostile: RichStrings = serde_json::from_str(r#"{"ab":[{"start":9,"end":2},{"start":0,"end":70,"italic":true}]}"#)?;
        assert_eq!(hostile.sanitized().runs("ab"), &[RichRun { start: 0, end: 2, italic: true, ..RichRun::default() }]);
        Ok(())
    }
}
