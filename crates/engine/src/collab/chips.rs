//! Option sets of multi-select cells while co-editing (Taskforce patch).
//!
//! A set lives in a hidden workbook-scope defined name (`gridcraft_tf_ext::set_name`). Names are
//! not co-edited, so defining a set used to be refused while two people had the workbook open,
//! and nobody could make a multi-select cell there. A set now travels as an effect of its own:
//! `chips: {set, value}` with `value` the stored text of the set, or `null` when the set is gone.
//! It belongs to the workbook; like formatting inside a text it is anchored on a cell (`A1` of the
//! first sheet), so that a build without it reads an ordinary cell effect with nothing in it.
//! Last writer wins per set.

use gridcraft_model::{DefinedName, Workbook};
use gridcraft_tf_ext as tf;
use serde::{Deserialize, Serialize};

use super::Effect;

/// The longest stored text of a set that is taken from an effect (100 options of a full label fit).
const VALUE_LIMIT: usize = 16 * 1024;

/// A set as it is after an edit: its stored text, or `None` when it was removed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChipsEffect {
    pub set: String,
    #[serde(default)]
    pub value: Option<String>,
}

/// The last-writer-wins key of a set, wherever its effect is anchored.
pub(super) fn key(set: &str) -> String {
    format!("chips#{}", set.to_ascii_lowercase())
}

fn is_set(n: &DefinedName) -> bool {
    n.scope.is_none() && tf::set_id_of(&n.name).is_some()
}

/// Takes the sets out of a workbook that is compared for changes effects cannot carry.
pub(super) fn without(wb: &mut Workbook) {
    wb.names.retain(|n| !is_set(n));
}

/// Every set of the workbook as `(id in lower case, id, stored text)`, sorted by id. A name that
/// holds something other than a set's text has `None`.
fn sets(wb: &Workbook) -> Vec<(String, &str, Option<String>)> {
    let mut out: Vec<(String, &str, Option<String>)> = wb
        .names
        .iter()
        .filter(|n| n.scope.is_none())
        .filter_map(|n| {
            let id = tf::set_id_of(&n.name)?;
            let value = tf::string_constant(&n.formula).filter(|t| tf::parse_set(t).is_some());
            Some((id.to_ascii_lowercase(), id, value))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// What the content hash takes from the sets: nothing for a workbook that has none, so such a
/// workbook hashes as it did before sets were replicated.
pub(super) fn hashed(wb: &Workbook) -> Vec<(String, String)> {
    sets(wb).into_iter().map(|(low, _, value)| (low, value.unwrap_or_default())).collect()
}

/// Effects for the sets that differ between two workbooks, or `Err` when a set's new content is
/// not a set's text (which an effect could not carry).
pub(super) fn effects(before: &Workbook, after: &Workbook) -> std::result::Result<Vec<Effect>, String> {
    let (old, new) = (sets(before), sets(after));
    if old == new {
        return Ok(Vec::new());
    }
    let Some(sheet) = after.sheets.first().map(|s| s.name.clone()) else { return Ok(Vec::new()) };
    let anchored = |set: &str, value: Option<String>| Effect {
        sheet: sheet.clone(),
        cell: Some("A1".to_string()),
        chips: Some(ChipsEffect { set: set.to_string(), value }),
        ..Default::default()
    };
    let mut out = Vec::new();
    for (low, id, value) in &new {
        if old.iter().any(|(l, _, v)| l == low && v == value) {
            continue;
        }
        let Some(value) = value else { return Err("workbook names".into()) };
        out.push(anchored(id, Some(value.clone())));
    }
    for (low, id, _) in &old {
        if !new.iter().any(|(l, _, _)| l == low) {
            out.push(anchored(id, None));
        }
    }
    Ok(out)
}

/// Writes a set from an effect into the workbook. The text is read and written again in its
/// stored form, so an effect cannot put anything but a set into the name.
pub(super) fn apply(wb: &mut Workbook, e: &ChipsEffect) -> std::result::Result<(), String> {
    if !tf::valid_set_id(&e.set) {
        return Err("bad option set id".into());
    }
    let name = tf::set_name(&e.set);
    let Some(value) = &e.value else {
        wb.names.retain(|n| !(n.scope.is_none() && n.name.eq_ignore_ascii_case(&name)));
        return Ok(());
    };
    if value.len() > VALUE_LIMIT {
        return Err("option set too long".into());
    }
    let options = tf::parse_set(value).ok_or("not an option set")?;
    let formula = format!("\"{}\"", tf::format_set(&options));
    match wb.names.iter_mut().find(|n| n.scope.is_none() && n.name.eq_ignore_ascii_case(&name)) {
        Some(existing) => {
            existing.formula = formula;
            existing.hidden = true;
        }
        None => wb.names.push(DefinedName { name, scope: None, formula, comment: String::new(), hidden: true }),
    }
    Ok(())
}
