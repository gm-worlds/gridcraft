//! Taskforce cell kinds: rating, chips (multi-select), date picker and rich text.
//!
//! A kind is marked on the cell's number format, so it travels with the cell through sort, copy,
//! fill and paste, replicates in co-editing with the style, and is an ordinary custom format to
//! any other spreadsheet program. The marker is a quoted literal `"tf-<kind>[:<arg>]"` inside a
//! valid format code. Other programs rewrite the unquoted parts of a code when they save (a `-`
//! becomes `\-`), but keep quoted literals as they are: [`classify`] therefore looks for the
//! literal anywhere in the code and never compares whole codes.
//!
//! The cell value stays a plain spreadsheet value: a number for a rating, a serial date for a
//! date, text for chips (labels joined by [`CHIP_SEPARATOR`]). A build without this crate shows
//! those values and keeps the marker.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

/// The feature name a build announces when it draws and edits these kinds.
pub const CAPABILITY: &str = "tf-kinds-1";

/// Highest star count a rating cell can have.
pub const RATING_MAX_LIMIT: u8 = 10;
/// Star count when none is given.
pub const RATING_DEFAULT_MAX: u8 = 5;
/// Between the labels of a chips cell's text.
pub const CHIP_SEPARATOR: &str = ", ";
/// Longest option-set id.
pub const SET_ID_LIMIT: usize = 32;

const PREFIX: &str = "tf-";

/// What a marked cell is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A whole number from 0 to `max`, drawn as stars.
    Rating { max: u8 },
    /// Labels from the option set `set`, drawn as chips.
    Chips { set: String },
    /// A serial date with a calendar to pick it.
    Date,
    /// Text with formatted runs.
    Rich,
}

impl Kind {
    /// The name used in commands and messages.
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Rating { .. } => "rating",
            Kind::Chips { .. } => "chips",
            Kind::Date => "date",
            Kind::Rich => "rich",
        }
    }

    /// The number-format code that marks a cell as this kind.
    pub fn format_code(&self) -> String {
        match self {
            // The marker always sits in the section for negative numbers, the one value none of
            // these kinds holds, so no other program ever shows it: a rating is a whole number
            // there, a date a date, chips and rich text their text.
            Kind::Rating { max } => format!("0;\"{PREFIX}rating:{max}\";0;@"),
            Kind::Chips { set } => format!("General;\"{PREFIX}chips:{set}\";General;@"),
            Kind::Date => format!("yyyy-mm-dd;\"{PREFIX}date\";yyyy-mm-dd;@"),
            Kind::Rich => format!("General;\"{PREFIX}rich\";General;@"),
        }
    }
}

/// The kind a number-format code marks, if any.
pub fn classify(code: &str) -> Option<Kind> {
    // Cheap exit for nearly every cell.
    if !code.contains(PREFIX) {
        return None;
    }
    quoted_literals(code).find_map(parse_marker)
}

/// Whether a code carries any Taskforce marker, known to this build or not.
pub fn is_marked(code: &str) -> bool {
    code.contains(PREFIX) && quoted_literals(code).any(|l| l.starts_with(PREFIX))
}

/// A marker this build does not know (a newer kind). Such a workbook must not be edited here.
pub fn unknown_marker(code: &str) -> Option<&str> {
    if !code.contains(PREFIX) {
        return None;
    }
    quoted_literals(code).find(|l| l.starts_with(PREFIX) && parse_marker(l).is_none())
}

fn parse_marker(literal: &str) -> Option<Kind> {
    let body = literal.strip_prefix(PREFIX)?;
    let (name, arg) = match body.split_once(':') {
        Some((n, a)) => (n, Some(a)),
        None => (body, None),
    };
    match (name, arg) {
        ("rating", None) => Some(Kind::Rating { max: RATING_DEFAULT_MAX }),
        ("rating", Some(a)) => {
            let max: u8 = a.parse().ok()?;
            (1..=RATING_MAX_LIMIT).contains(&max).then_some(Kind::Rating { max })
        }
        ("chips", Some(set)) if valid_set_id(set) => Some(Kind::Chips { set: set.to_string() }),
        ("date", None) => Some(Kind::Date),
        ("rich", None) => Some(Kind::Rich),
        _ => None,
    }
}

/// An option-set id: 1 to [`SET_ID_LIMIT`] ASCII letters, digits, `_` or `-`.
pub fn valid_set_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= SET_ID_LIMIT && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The quoted literals of a format code, without their quotes. A backslash escapes the next
/// character outside a literal, so `\"` does not open one.
fn quoted_literals(code: &str) -> impl Iterator<Item = &str> {
    let bytes = code.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            match bytes.get(i) {
                Some(b'\\') => i += 2,
                Some(b'"') => {
                    let start = i + 1;
                    let end = bytes.get(start..).and_then(|rest| rest.iter().position(|b| *b == b'"')).map(|n| start + n)?;
                    i = end + 1;
                    // Quotes are ASCII, so both ends are character boundaries.
                    return code.get(start..end);
                }
                _ => i += 1,
            }
        }
        None
    })
}

/// The stars a rating cell shows for a numeric value: rounded, within `0..=max`.
pub fn rating_stars(value: f64, max: u8) -> u8 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let rounded = value.round();
    if rounded >= f64::from(max) { max } else { rounded as u8 }
}

/// The value a click on star `star` (1-based) gives: that many stars, or none when the cell
/// already shows exactly that many (a second click on the same star clears the rating).
pub fn rating_after_click(current: u8, star: u8, max: u8) -> u8 {
    let star = star.min(max);
    if current == star { 0 } else { star }
}

/// The labels in a chips cell's text, trimmed, without empties or repeats, in order.
pub fn chip_labels(text: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for label in text.split(',').map(str::trim).filter(|l| !l.is_empty()) {
        if !out.contains(&label) {
            out.push(label);
        }
    }
    out
}

/// A chips cell's text for these labels.
pub fn chip_text<S: AsRef<str>>(labels: &[S]) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for label in labels.iter().map(|l| l.as_ref().trim()).filter(|l| !l.is_empty()) {
        if !seen.contains(&label) {
            seen.push(label);
        }
    }
    seen.join(CHIP_SEPARATOR)
}

/// A chips cell's text after switching `label` on or off, keeping the order of `options`.
pub fn chip_toggle<S: AsRef<str>>(text: &str, label: &str, options: &[S]) -> String {
    let mut labels = chip_labels(text);
    match labels.iter().position(|l| *l == label) {
        Some(i) => {
            labels.remove(i);
        }
        None => labels.push(label),
    }
    let rank = |l: &str| options.iter().position(|o| o.as_ref() == l).unwrap_or(usize::MAX);
    // Stable: labels no longer in the set keep their place after the known ones.
    labels.sort_by_key(|l| rank(l));
    chip_text(&labels)
}

/// Whether a label can be an option: not empty, no comma (the separator) and no control characters.
pub fn valid_chip_label(label: &str) -> bool {
    let t = label.trim();
    !t.is_empty() && t.len() <= 80 && !t.contains(',') && !t.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips_through_its_code() {
        for kind in [Kind::Rating { max: 5 }, Kind::Rating { max: 10 }, Kind::Chips { set: "k3_f-9".into() }, Kind::Date, Kind::Rich] {
            assert_eq!(classify(&kind.format_code()), Some(kind.clone()), "{}", kind.name());
        }
    }

    #[test]
    fn codes_rewritten_by_another_program_still_classify() {
        // What LibreOffice 25 wrote back for our codes (observed 2026-11-24): unquoted `-`
        // escaped, literals untouched.
        assert_eq!(classify("0;\"tf-rating:5\";0;@"), Some(Kind::Rating { max: 5 }));
        assert_eq!(classify("0;\\-0;0;\"tf-rating:5\""), Some(Kind::Rating { max: 5 }));
        assert_eq!(classify("yyyy\\-mm\\-dd;\"tf-date\""), Some(Kind::Date));
        assert_eq!(classify("[<0]\"tf-rating\";0"), Some(Kind::Rating { max: 5 }));
    }

    #[test]
    fn ordinary_codes_are_not_kinds() {
        for code in ["General", "0.00", "yyyy-mm-dd", "checkbox", "\"tf\"0", "0 \"items\"", "tf-rating:5", "\\\"tf-date\\\"", ""] {
            assert_eq!(classify(code), None, "{code}");
            assert!(!is_marked(code), "{code}");
        }
    }

    #[test]
    fn hostile_codes_do_not_panic() {
        for code in
            ["\"", "\\", "\"tf-", "\"tf-rating:", "\"tf-rating:999\"", "\"tf-chips:\"", "\"tf-chips:a b\"", "\"\u{e9}tf-\u{e9}\"", "\\\"\"tf-date\""]
        {
            let _ = classify(code);
            let _ = unknown_marker(code);
        }
        assert_eq!(classify("\"tf-rating:999\""), None);
        assert_eq!(classify("\"tf-rating:0\""), None);
        assert_eq!(classify("\\\"\"tf-date\""), Some(Kind::Date));
    }

    #[test]
    fn a_newer_kind_is_reported_as_unknown() {
        assert_eq!(unknown_marker("0;0;0;\"tf-slider:3\""), Some("tf-slider:3"));
        assert!(is_marked("0;0;0;\"tf-slider:3\""));
        assert_eq!(unknown_marker(&Kind::Date.format_code()), None);
        assert_eq!(unknown_marker("0.00"), None);
    }

    #[test]
    fn rating_values() {
        assert_eq!(rating_stars(3.4, 5), 3);
        assert_eq!(rating_stars(9.0, 5), 5);
        assert_eq!(rating_stars(-1.0, 5), 0);
        assert_eq!(rating_stars(f64::NAN, 5), 0);
        assert_eq!(rating_stars(f64::INFINITY, 5), 0);
        assert_eq!(rating_after_click(2, 4, 5), 4);
        assert_eq!(rating_after_click(4, 4, 5), 0);
        assert_eq!(rating_after_click(0, 9, 5), 5);
    }

    #[test]
    fn chips_text() {
        assert_eq!(chip_labels(" Red ,Blue,, Red"), vec!["Red", "Blue"]);
        assert_eq!(chip_text(&["Blue", " ", "Red", "Blue"]), "Blue, Red");
        let options = ["Red", "Green", "Blue"];
        assert_eq!(chip_toggle("Blue", "Red", &options), "Red, Blue");
        assert_eq!(chip_toggle("Red, Blue", "Red", &options), "Blue");
        assert_eq!(chip_toggle("Gone, Blue", "Green", &options), "Green, Blue, Gone");
        assert!(valid_chip_label("In review"));
        assert!(!valid_chip_label("a,b"));
        assert!(!valid_chip_label("  "));
    }
}
