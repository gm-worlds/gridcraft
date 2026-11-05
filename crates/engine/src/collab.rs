//! Live co-editing (Taskforce patch, branch `tf/collab`): the workbook's edits as **effects**, and
//! other people's effects applied without entering this person's undo history.
//!
//! An effect is what an edit did to one cell (or one row or column), field by field: the cell's
//! input (a typed constant or a formula's text, never text to be parsed again) and the style
//! properties that changed, as leaf paths of the `Style` JSON (`font.bold`, `fill`,
//! `borders.left`, `num_fmt`). They are found by comparing the workbook before and after each
//! top-level command ([`observe`]), so every command that only changes cells replicates without
//! command-specific code: typing, paste, fill, clear, format painter, toggles, undo and redo.
//!
//! What is not a cell or a line style (inserted rows, sheets, merges, names, conditional formats,
//! comments, charts, column widths…) is refused while co-editing: the command is put back and
//! fails with [`NOT_YET`]. Python cells and legacy array formulas are refused too.
//!
//! Conflicts are the host's business (last writer wins per effect key, ordered by the host's
//! server); this module only produces effects and applies them. Applying ([`apply`]) goes through
//! the same commit path as an edit (protection and array checks, incremental recalculation, row
//! heights) but makes no undo step and no journal entry.
//!
//! Undo stays snapshot-based. Because a snapshot predates the other people's later edits, every
//! workbook this person has seen is remembered with how many remote batches it included; when an
//! undo or redo brings an older one back, the remote effects since are applied on top of it, and
//! what then differs from before the undo is sent as ordinary effects. Undo therefore takes back
//! this person's own edits and keeps everyone else's.

use std::sync::{Arc, Weak};

use gridcraft_core::{CellRef, Value};
use gridcraft_model::{Cell, Formula, Sheet, Style, StyleId, Workbook};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::{DocState, EngineError, Result, Session};

/// The effects protocol this build speaks (the host checks it in `hello`).
pub const PROTOCOL: u32 = 1;

/// The message of a command refused while co-editing.
pub const NOT_YET: &str = "This isn't available while editing together yet";

/// The most effects one edit may make (a bigger paste is refused while co-editing).
pub const MAX_EFFECTS: usize = 20_000;

/// The longest text or formula an effect may carry (Excel's cell limit).
const MAX_TEXT: usize = 32_767;

/// Style JSON objects whose fields are separate effect keys; anything else is one value
/// (`fill`, `num_fmt`, `font.color`, each side of `borders`), because its parts only make sense
/// together.
const BRANCHES: &[&str] = &["", "font", "align", "protection", "borders"];

/// What a cell holds as authored: a constant, or a formula's text (without `=`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum Input {
    Value {
        v: Value,
    },
    Formula {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dynamic: bool,
    },
}

/// One cell's, row's or column's change. Exactly one of `cell`, `row`, `col` is set.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Effect {
    /// The sheet's name (sheets can't be renamed, added or removed while co-editing).
    pub sheet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    /// 0-based row or column index, for a whole row's or column's format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub col: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Input>,
    /// Changed style properties: leaf path → new value.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub style: Map<String, Json>,
}

/// Co-editing state of one open workbook (the session's [`Session::collab`]).
pub struct Collab {
    /// The document ([`DocState::uid`]) being co-edited.
    pub doc: u64,
    /// The workbook as last observed: effects are what changed since.
    seen: Arc<Workbook>,
    /// Effect batches made by this person and not yet taken by the host, oldest first, with the
    /// number each one is sent under.
    pub outbox: Vec<(u64, Vec<Effect>)>,
    /// The number the next batch gets (from 1).
    next_seq: u64,
    /// Batches made here that the server hasn't confirmed yet (taken by the host or not), with
    /// their effects, so a host that reconnects can send them again.
    unacked: Vec<(u64, Vec<Effect>)>,
    /// How many unconfirmed batches write each effect key ([`key_of`]): a remote write to one of
    /// these is older than this person's (the server orders theirs after it) and is skipped.
    pending: std::collections::HashMap<String, u32>,
    /// Remote batches applied so far.
    seq: u64,
    /// Every remote batch applied, with its number, for undo ([`Collab::overlay`]); trimmed to
    /// what a remembered workbook still lacks.
    remote: Vec<(u64, Vec<Effect>)>,
    /// Workbooks this person has seen, with the remote batches each one included.
    known: Vec<(Weak<Workbook>, u64)>,
    /// Why the last command was refused, for the host.
    pub refused: Option<String>,
}

impl Collab {
    /// Starts co-editing `d` as it is now.
    pub fn new(d: &DocState) -> Collab {
        let mut c = Collab {
            doc: d.uid,
            seen: d.wb.clone(),
            outbox: Vec::new(),
            next_seq: 1,
            unacked: Vec::new(),
            pending: Default::default(),
            seq: 0,
            remote: Vec::new(),
            known: Vec::new(),
            refused: None,
        };
        c.remember(&d.wb);
        c
    }

    fn remember(&mut self, wb: &Arc<Workbook>) {
        self.known.retain(|(w, _)| w.strong_count() > 0);
        if !self.known.iter().any(|(w, _)| w.as_ptr() == Arc::as_ptr(wb)) {
            self.known.push((Arc::downgrade(wb), self.seq));
        }
        let oldest = self.known.iter().map(|(_, s)| *s).min().unwrap_or(self.seq);
        self.remote.retain(|(s, _)| *s > oldest);
    }

    /// The remote effects a remembered workbook doesn't have yet (`None`: not a workbook seen
    /// here, or one that has them all).
    fn overlay(&self, wb: &Arc<Workbook>) -> Option<Vec<Effect>> {
        let had = self.known.iter().find(|(w, _)| w.as_ptr() == Arc::as_ptr(wb)).map(|(_, s)| *s)?;
        if had >= self.seq {
            return None;
        }
        Some(self.remote.iter().filter(|(s, _)| *s > had).flat_map(|(_, e)| e.iter().cloned()).collect())
    }

    /// The batches this person made since the last call, with their numbers.
    pub fn take_outbox(&mut self) -> Vec<(u64, Vec<Effect>)> {
        std::mem::take(&mut self.outbox)
    }

    /// Every batch the server hasn't confirmed, oldest first (to send again after a reconnect).
    pub fn unconfirmed(&self) -> &[(u64, Vec<Effect>)] {
        &self.unacked
    }

    /// Whether a batch is still waiting for the server.
    pub fn has_pending(&self) -> bool {
        !self.unacked.is_empty()
    }

    fn push_batch(&mut self, effects: Vec<Effect>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        for e in &effects {
            for k in keys_of(e) {
                *self.pending.entry(k).or_default() += 1;
            }
        }
        self.outbox.push((seq, effects.clone()));
        self.unacked.push((seq, effects));
    }

    /// The server took batch `seq` (it came back in the ordered stream): its writes are no longer
    /// newer than what arrives after.
    pub fn confirm(&mut self, seq: u64) {
        let Some(i) = self.unacked.iter().position(|(s, _)| *s == seq) else { return };
        let (_, effects) = self.unacked.remove(i);
        for e in &effects {
            for k in keys_of(e) {
                if let Some(n) = self.pending.get_mut(&k) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        self.pending.remove(&k);
                    }
                }
            }
        }
    }

    /// A remote effect without the fields this person wrote later (unconfirmed writes), or `None`
    /// when nothing is left.
    fn without_pending(&self, e: &Effect) -> Option<Effect> {
        if self.pending.is_empty() {
            return Some(e.clone());
        }
        let place = place_of(e);
        let mut out = e.clone();
        if out.input.is_some() && self.pending.contains_key(&format!("{place}#input")) {
            out.input = None;
        }
        out.style.retain(|p, _| !self.pending.contains_key(&format!("{place}#s:{p}")));
        (out.input.is_some() || !out.style.is_empty()).then_some(out)
    }
}

/// Where an effect writes, as its keys start: `Sheet!B2`, `Sheet!R3`, `Sheet!C4`.
fn place_of(e: &Effect) -> String {
    match (&e.cell, e.row, e.col) {
        (Some(c), _, _) => format!("{}!{c}", e.sheet),
        (_, Some(r), _) => format!("{}!R{r}", e.sheet),
        (_, _, Some(c)) => format!("{}!C{c}", e.sheet),
        _ => format!("{}!", e.sheet),
    }
}

/// The last-writer-wins keys of an effect, one per field it writes: `Sheet!B2#input`,
/// `Sheet!B2#s:font.bold`.
pub fn keys_of(e: &Effect) -> Vec<String> {
    let place = place_of(e);
    let mut k: Vec<String> = e.style.keys().map(|p| format!("{place}#s:{p}")).collect();
    if e.input.is_some() {
        k.push(format!("{place}#input"));
    }
    k
}

impl Session {
    /// Starts co-editing the active workbook: from now on its edits become effects and commands
    /// that change anything else are refused.
    pub fn collab_start(&mut self) -> Result<()> {
        let c = Collab::new(self.doc()?);
        self.collab = Some(c);
        Ok(())
    }

    /// Applies other people's effects to the co-edited workbook (see [`apply`]).
    pub fn collab_apply(&mut self, effects: &[Effect]) -> Result<()> {
        apply(self, effects)
    }

    /// The server confirmed this person's batch `seq` (see [`Collab::confirm`]).
    pub fn collab_confirm(&mut self, seq: u64) {
        if let Some(c) = self.collab.as_mut() {
            c.confirm(seq);
        }
    }
}

fn collab_doc(s: &mut Session) -> Option<(&mut Collab, &mut DocState)> {
    let c = s.collab.as_mut()?;
    let d = s.docs.iter_mut().find(|d| d.uid == c.doc)?;
    Some((c, d))
}

/// After a top-level command, or once per frame: what changed in the co-edited workbook since it
/// was last observed becomes a batch of effects in the outbox. `Err` names what changed that
/// can't be co-edited; the caller puts the workbook back.
pub fn observe(s: &mut Session) -> std::result::Result<(), String> {
    let Some((c, d)) = collab_doc(s) else { return Ok(()) };
    if Arc::ptr_eq(&c.seen, &d.wb) {
        return Ok(());
    }
    // An undo or redo brought back a workbook from before other people's later edits: put those
    // back on top of it first.
    if let Some(missing) = c.overlay(&d.wb)
        && !missing.is_empty()
        && let Err(e) = commit_effects(d, &missing)
    {
        log::warn!("collab: couldn't put other people's edits back after an undo: {e}");
    }
    let effects = diff(&c.seen, &d.wb)?;
    if effects.len() > MAX_EFFECTS {
        return Err(format!("an edit of {} cells (the limit is {MAX_EFFECTS})", effects.len()));
    }
    c.seen = d.wb.clone();
    let wb = d.wb.clone();
    c.remember(&wb);
    if !effects.is_empty() {
        c.push_batch(effects);
    }
    Ok(())
}

/// The workbook back as it was observed, after a refused change made outside a command.
pub fn revert(s: &mut Session) {
    let Some((c, d)) = collab_doc(s) else { return };
    d.wb = c.seen.clone();
    d.calc.rebuild(&d.wb);
    d.revision += 1;
}

/// Applies other people's effects, in the server's order: one commit (validated, recalculated), no
/// undo step, no journal entry, not observed as this person's. Fields this person wrote later
/// (batches the server hasn't confirmed, including ones the host hasn't taken yet) are skipped:
/// the server orders those writes after these, so they win everywhere.
pub fn apply(s: &mut Session, effects: &[Effect]) -> Result<()> {
    // This person's own changes since the last observation go out first, so they are not
    // mistaken for part of the remote batch.
    if let Err(why) = observe(s) {
        revert(s);
        if let Some(c) = s.collab.as_mut() {
            c.refused = Some(why);
        }
    }
    let Some((c, d)) = collab_doc(s) else { return Err(EngineError::NoDocument) };
    let kept: Vec<Effect> = effects.iter().filter_map(|e| c.without_pending(e)).collect();
    if kept.is_empty() {
        return Ok(());
    }
    commit_effects(d, &kept)?;
    d.revision += 1;
    c.seq += 1;
    c.remote.push((c.seq, kept));
    c.seen = d.wb.clone();
    let wb = d.wb.clone();
    c.remember(&wb);
    Ok(())
}

fn bad(msg: impl Into<String>) -> EngineError {
    EngineError::Other(format!("collab: {}", msg.into()))
}

fn commit_effects(d: &mut DocState, effects: &[Effect]) -> Result<()> {
    if effects.len() > MAX_EFFECTS {
        return Err(bad("too many effects"));
    }
    crate::cmd::commit(d, |cx| {
        for e in effects {
            let si = cx.wb.sheet_index(&e.sheet).ok_or_else(|| bad(format!("no sheet `{}`", e.sheet)))?;
            match (&e.cell, e.row, e.col) {
                (Some(a1), None, None) => {
                    let at = CellRef::parse(a1).ok_or_else(|| bad(format!("bad cell `{a1}`")))?;
                    // A table's header names its column (made unique per table): not co-edited.
                    if e.input.is_some()
                        && cx
                            .wb
                            .sheet(si)
                            .is_some_and(|sh| sh.tables.iter().any(|t| t.header_row && t.range.start.row == at.row && t.range.contains(at)))
                    {
                        return Err(bad(format!("{a1} is a table header")));
                    }
                    if let Some(input) = &e.input {
                        let style = cx.wb.sheet(si).map(|sh| sh.style_id(at)).unwrap_or_default();
                        let cell = input_cell(input, style)?;
                        let sh = cx.sheet_mut(si)?;
                        if cell.is_blank() {
                            sh.remove_cell(at);
                        } else {
                            sh.set_cell(at, cell);
                        }
                        // As typing does: a value just below an autofilter range extends it.
                        crate::cmd::data::grow_autofilter(sh, at);
                    }
                    if !e.style.is_empty() {
                        let cur = cx.wb.sheet(si).map(|sh| sh.style_id(at)).unwrap_or_default();
                        let id = patched_style(&mut cx.wb, cur, &e.style)?;
                        cx.sheet_mut(si)?.set_style(at, id);
                    }
                    cx.touch(si, at);
                }
                (None, Some(row), None) if e.input.is_none() => {
                    let cur = cx.wb.sheet(si).and_then(|sh| sh.rows.get(&row).and_then(|l| l.style)).unwrap_or_default();
                    let id = patched_style(&mut cx.wb, cur, &e.style)?;
                    let line = cx.sheet_mut(si)?.rows.entry(row).or_default();
                    line.style = (id != StyleId::DEFAULT).then_some(id);
                    cx.structural = true;
                }
                (None, None, Some(col)) if e.input.is_none() => {
                    let cur = cx.wb.sheet(si).and_then(|sh| sh.cols.get(&col).and_then(|l| l.style)).unwrap_or_default();
                    let id = patched_style(&mut cx.wb, cur, &e.style)?;
                    let line = cx.sheet_mut(si)?.cols.entry(col).or_default();
                    line.style = (id != StyleId::DEFAULT).then_some(id);
                    cx.structural = true;
                }
                _ => return Err(bad("an effect names one cell, row or column")),
            }
        }
        Ok(())
    })
}

fn input_cell(input: &Input, style: StyleId) -> Result<Cell> {
    match input {
        Input::Value { v } => {
            match v {
                Value::Array(_) => return Err(bad("an array is not a cell input")),
                Value::Text(t) if t.len() > MAX_TEXT => return Err(bad("text too long")),
                Value::Number(n) if !n.is_finite() => return Err(bad("not a finite number")),
                _ => {}
            }
            Ok(Cell { value: v.clone(), formula: None, style, py: None })
        }
        Input::Formula { text, dynamic } => {
            if text.len() > MAX_TEXT {
                return Err(bad("formula too long"));
            }
            let mut f = Formula::new(text);
            f.dynamic = *dynamic;
            let mut cell = Cell::formula(f);
            cell.style = style;
            Ok(cell)
        }
    }
}

fn patched_style(wb: &mut Workbook, base: StyleId, patch: &Map<String, Json>) -> Result<StyleId> {
    let mut json = serde_json::to_value(wb.styles.get(base)).map_err(|e| bad(e.to_string()))?;
    for (path, v) in patch {
        let mut at = &mut json;
        let parts: Vec<&str> = path.split('.').collect();
        if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
            return Err(bad(format!("bad style path `{path}`")));
        }
        for (i, part) in parts.iter().enumerate() {
            let Json::Object(m) = at else { return Err(bad(format!("bad style path `{path}`"))) };
            if i + 1 == parts.len() {
                m.insert((*part).to_string(), v.clone());
                break;
            }
            at = m.entry((*part).to_string()).or_insert_with(|| Json::Object(Map::new()));
        }
    }
    let style: Style = serde_json::from_value(json).map_err(|e| bad(format!("style: {e}")))?;
    Ok(wb.styles.intern(style))
}

// ---------------------------------------------------------------- diff

/// A cell's input, or why it can't be co-edited.
fn input_of(cell: Option<&Cell>) -> std::result::Result<Input, String> {
    let Some(cell) = cell else { return Ok(Input::Value { v: Value::Empty }) };
    if cell.py.is_some() {
        return Err("Python cells".into());
    }
    if let Some(f) = &cell.formula {
        if f.array.is_some() {
            return Err("array formulas".into());
        }
        return Ok(Input::Formula { text: f.text.clone(), dynamic: f.dynamic });
    }
    if matches!(cell.value, Value::Array(_)) {
        return Err("array values".into());
    }
    Ok(Input::Value { v: cell.value.clone() })
}

fn style_json(wb: &Workbook, id: StyleId) -> Json {
    serde_json::to_value(wb.styles.get(id)).unwrap_or(Json::Null)
}

/// The style properties that differ, as leaf paths (see [`BRANCHES`]).
fn leaf_diff(path: &str, a: &Json, b: &Json, out: &mut Map<String, Json>) {
    if a == b {
        return;
    }
    if BRANCHES.contains(&path)
        && let (Json::Object(x), Json::Object(y)) = (a, b)
    {
        let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
        keys.sort();
        keys.dedup();
        for k in keys {
            let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
            leaf_diff(&p, x.get(k).unwrap_or(&Json::Null), y.get(k).unwrap_or(&Json::Null), out);
        }
        return;
    }
    out.insert(path.to_string(), b.clone());
}

/// A sheet without what effects carry or what is derived, for the "anything else changed?" check.
fn sheet_rest(sh: &Sheet) -> Sheet {
    let mut s = sh.clone();
    s.cells = Default::default();
    s.spill.clear();
    s.spill_ranges.clear();
    // Where this person is looking is theirs.
    s.view_active = CellRef::default();
    s.view_top_left = CellRef::default();
    s.zoom = 0;
    for lines in [&mut s.rows, &mut s.cols] {
        for l in lines.values_mut() {
            l.style = None;
        }
    }
    // Automatic row heights follow the content (each side computes its own).
    for l in s.rows.values_mut() {
        if !l.custom {
            l.size = None;
        }
    }
    s.rows.retain(|_, l| *l != Default::default());
    s.cols.retain(|_, l| *l != Default::default());
    s
}

/// The autofilter range `after` has if typing the changed cells grew it (`grow_autofilter`, which
/// applying them repeats), so that growth isn't taken for a filter change.
fn grown_autofilter(sa: &Sheet, sb: &Sheet, cells: &[CellRef]) -> Option<gridcraft_model::AutoFilter> {
    let mut sim = sb.clone();
    sim.autofilter = sa.autofilter.clone();
    for at in cells {
        crate::cmd::data::grow_autofilter(&mut sim, *at);
    }
    sim.autofilter
}

fn workbook_rest(wb: &Workbook) -> Workbook {
    let mut w = wb.clone();
    w.sheets.clear();
    w.styles = Default::default();
    w.active_sheet = Default::default();
    w.view = Default::default();
    w.props = Default::default();
    w
}

/// The first JSON key at which two values differ, to name what a refused command changed.
fn first_difference<T: Serialize>(a: &T, b: &T) -> String {
    match (serde_json::to_value(a), serde_json::to_value(b)) {
        (Ok(Json::Object(x)), Ok(Json::Object(y))) => {
            x.keys().chain(y.keys()).find(|k| x.get(*k) != y.get(*k)).cloned().unwrap_or_else(|| "settings".into())
        }
        _ => "settings".into(),
    }
}

/// The effects that take `before` to `after`, or what changed that effects can't carry.
pub fn diff(before: &Workbook, after: &Workbook) -> std::result::Result<Vec<Effect>, String> {
    if before.sheets.len() != after.sheets.len() || before.sheets.iter().zip(&after.sheets).any(|(a, b)| a.name != b.name) {
        return Err("sheets".into());
    }
    let (wa, wb) = (workbook_rest(before), workbook_rest(after));
    if wa != wb {
        return Err(format!("workbook {}", first_difference(&wa, &wb)));
    }
    let mut out = Vec::new();
    for (sa, sb) in before.sheets.iter().zip(&after.sheets) {
        if Arc::ptr_eq(sa, sb) {
            continue;
        }
        let mut cells = sa.cells.diff(&sb.cells);
        cells.sort_unstable_by_key(|c| (c.row, c.col));
        let (mut ra, rb) = (sheet_rest(sa), sheet_rest(sb));
        if ra.autofilter != rb.autofilter && sa.autofilter.is_some() && grown_autofilter(sa, sb, &cells) == sb.autofilter {
            ra.autofilter = rb.autofilter.clone();
        }
        if ra != rb {
            return Err(first_difference(&ra, &rb).replace('_', " "));
        }
        for at in cells {
            let (ia, ib) = (input_of(sa.cells.get(at))?, input_of(sb.cells.get(at))?);
            let mut style = Map::new();
            leaf_diff("", &style_json(before, sa.style_id(at)), &style_json(after, sb.style_id(at)), &mut style);
            if ia == ib && style.is_empty() {
                continue;
            }
            out.push(Effect { sheet: sb.name.clone(), cell: Some(at.a1()), input: (ia != ib).then_some(ib), style, ..Default::default() });
        }
        for (is_row, (la, lb)) in [(true, (&sa.rows, &sb.rows)), (false, (&sa.cols, &sb.cols))] {
            let mut keys: Vec<u32> = la.keys().chain(lb.keys()).copied().collect();
            keys.sort_unstable();
            keys.dedup();
            for k in keys {
                let ya = la.get(&k).and_then(|l| l.style).unwrap_or_default();
                let yb = lb.get(&k).and_then(|l| l.style).unwrap_or_default();
                let mut style = Map::new();
                leaf_diff("", &style_json(before, ya), &style_json(after, yb), &mut style);
                if style.is_empty() {
                    continue;
                }
                let (row, col) = if is_row { (Some(k), None) } else { (None, Some(k)) };
                out.push(Effect { sheet: sb.name.clone(), row, col, style, ..Default::default() });
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- hash

/// FNV-1a: stable across builds and platforms (std's hasher is not promised to be).
struct Fnv(u64);
impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
        // A separator, so ("ab","c") and ("a","bc") differ.
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(0x100_0000_01b3);
    }
}

/// A hash of what co-editing replicates (sheet names, cell inputs, cell and line styles), equal
/// on every client that has applied the same effects. Cached results, style ids and the view are
/// left out.
pub fn content_hash(wb: &Workbook) -> u64 {
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    let mut styles: std::collections::HashMap<StyleId, String> = std::collections::HashMap::new();
    let mut style_text = |id: StyleId| styles.entry(id).or_insert_with(|| style_json(wb, id).to_string()).clone();
    let default = style_text(StyleId::DEFAULT);
    for sh in &wb.sheets {
        h.write(sh.name.as_bytes());
        for (at, cell) in sh.cells.iter() {
            let input = input_of(Some(cell)).map(|i| serde_json::to_string(&i).unwrap_or_default()).unwrap_or_else(|e| e);
            let style = style_text(cell.style);
            if !cell.has_content() && style == default {
                continue;
            }
            h.write(&at.row.to_le_bytes());
            h.write(&at.col.to_le_bytes());
            h.write(input.as_bytes());
            h.write(style.as_bytes());
        }
        for t in &sh.tables {
            h.write(t.name.as_bytes());
            for c in &t.columns {
                h.write(c.name.as_bytes());
            }
        }
        if let Some(af) = &sh.autofilter {
            h.write(af.range.a1().as_bytes());
        }
        for (tag, lines) in [(b"r", &sh.rows), (b"c", &sh.cols)] {
            for (k, l) in lines {
                let Some(id) = l.style else { continue };
                let style = style_text(id);
                if style == default {
                    continue;
                }
                h.write(tag);
                h.write(&k.to_le_bytes());
                h.write(style.as_bytes());
            }
        }
    }
    h.0
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session() -> Session {
        let mut s = Session::new();
        s.execute("file.new", json!({"sample": "sales"})).unwrap();
        s.collab_start().unwrap();
        s
    }

    /// Everything `from` made since the last call, applied to `to` (as the server would relay it).
    fn relay(from: &mut Session, to: &mut Session) -> usize {
        let batches = from.collab.as_mut().unwrap().take_outbox();
        for (seq, b) in &batches {
            // Over the wire and back; the sender sees its own batch come back as the confirmation.
            let wire: Vec<Effect> = serde_json::from_str(&serde_json::to_string(b).unwrap()).unwrap();
            to.collab_apply(&wire).unwrap();
            from.collab_confirm(*seq);
        }
        batches.len()
    }

    fn hash(s: &Session) -> u64 {
        content_hash(&s.doc().unwrap().wb)
    }

    fn input(s: &Session, a1: &str) -> String {
        s.doc().unwrap().wb.sheet(0).unwrap().input_text(CellRef::parse(a1).unwrap())
    }

    fn style(s: &Session, a1: &str) -> Style {
        let d = s.doc().unwrap();
        d.wb.styles.get(d.wb.sheet(0).unwrap().style_id(CellRef::parse(a1).unwrap())).clone()
    }

    #[test]
    fn typing_and_formatting_converge() {
        let (mut a, mut b) = (session(), session());
        assert_eq!(hash(&a), hash(&b));
        a.execute("cell.set", json!({"cell": "F1", "input": "Notes"})).unwrap();
        a.execute("cell.set", json!({"cell": "F2", "input": "=B2*1.21"})).unwrap();
        a.execute("cell.set", json!({"cell": "F3", "input": "5/4/2026"})).unwrap();
        a.execute("range.setValues", json!({"range": "F3", "values": [["=not a formula"]], "parse": false})).ok();
        a.execute("range.setValues", json!({"range": "G1", "values": [["text"]], "parse": false})).unwrap();
        a.execute("home.bold", json!({"range": "A1:D1"})).unwrap();
        a.execute("home.fillColor", json!({"range": "A2:B3", "color": "#FFF2CC"})).unwrap();
        a.execute("home.percent", json!({"range": "C2"})).unwrap();
        assert!(relay(&mut a, &mut b) >= 7);
        assert_eq!(hash(&a), hash(&b));
        assert_eq!(input(&b, "F2"), "=B2*1.21");
        assert_eq!(
            b.doc().unwrap().wb.sheet(0).unwrap().value(CellRef::parse("F2").unwrap()),
            a.doc().unwrap().wb.sheet(0).unwrap().value(CellRef::parse("F2").unwrap())
        );
        assert!(style(&b, "A1").font.bold);
        // B's history holds none of A's edits.
        assert!(b.doc().unwrap().undo.is_empty());
    }

    #[test]
    fn table_headers_are_refused_but_table_data_is_not() {
        let (mut a, mut b) = (session(), session());
        // F4 is a header of the sample's table (B4:I11).
        let err = a.execute("cell.set", json!({"cell": "F4", "input": "Renamed"})).unwrap_err();
        assert!(err.to_string().contains("tables"), "{err}");
        a.execute("cell.set", json!({"cell": "F6", "input": "123"})).unwrap();
        relay(&mut a, &mut b);
        assert_eq!(hash(&a), hash(&b));
        assert_eq!(input(&b, "F6"), "123");
        let header: Vec<Effect> =
            serde_json::from_value(json!([{"sheet": "Sales", "cell": "F4", "input": {"t": "value", "v": {"t": "Text", "v": "x"}}}])).unwrap();
        assert!(b.collab_apply(&header).is_err());
    }

    #[test]
    fn text_starting_with_an_equals_sign_stays_text() {
        let (mut a, mut b) = (session(), session());
        a.execute("range.setValues", json!({"range": "H1", "values": [["=1+1"]], "parse": false})).unwrap();
        a.execute("cell.set", json!({"cell": "H2", "input": "'=1+1"})).unwrap();
        relay(&mut a, &mut b);
        assert_eq!(hash(&a), hash(&b));
        let v = b.doc().unwrap().wb.sheet(0).unwrap().value(CellRef::parse("H2").unwrap());
        assert_eq!(v, Value::Text("=1+1".into()));
    }

    #[test]
    fn fill_paste_and_clear_replicate_as_effects() {
        let (mut a, mut b) = (session(), session());
        a.execute("cell.set", json!({"cell": "J1", "input": "=A1&\"!\""})).unwrap();
        a.execute("selection.set", json!({"range": "J1:J6"})).unwrap();
        a.execute("edit.fillDown", json!({})).unwrap();
        a.execute("edit.paste", json!({"at": "L2", "text": "1\t2\n3\t4"})).unwrap();
        a.execute("selection.set", json!({"range": "A2:A3"})).unwrap();
        a.execute("edit.clear", json!({})).ok();
        a.execute("edit.clearContents", json!({})).ok();
        relay(&mut a, &mut b);
        assert_eq!(hash(&a), hash(&b));
        assert_eq!(input(&b, "J4"), "=A4&\"!\"");
    }

    #[test]
    fn structural_commands_are_refused_and_put_back() {
        let mut a = session();
        let h = hash(&a);
        let undo = a.doc().unwrap().undo.len();
        let err = a.execute("home.insertRows", json!({"rows": "2:2"})).unwrap_err();
        assert!(err.to_string().contains(NOT_YET), "{err}");
        assert_eq!(hash(&a), h);
        assert_eq!(a.doc().unwrap().undo.len(), undo);
        assert!(a.collab.as_mut().unwrap().take_outbox().is_empty());
        assert!(a.execute("home.insertSheet", json!({})).is_err());
        assert_eq!(a.doc().unwrap().wb.sheets.len(), 3.min(a.doc().unwrap().wb.sheets.len()));
        // Ordinary edits still work afterwards.
        a.execute("cell.set", json!({"cell": "Z1", "input": "ok"})).unwrap();
        assert_eq!(a.collab.as_mut().unwrap().take_outbox().len(), 1);
    }

    #[test]
    fn undo_takes_back_only_my_own_edit() {
        let (mut a, mut b) = (session(), session());
        a.execute("cell.set", json!({"cell": "P1", "input": "mine"})).unwrap();
        relay(&mut a, &mut b);
        b.execute("cell.set", json!({"cell": "P2", "input": "theirs"})).unwrap();
        relay(&mut b, &mut a);
        assert_eq!(input(&a, "P2"), "theirs");
        a.execute("edit.undo", json!({})).unwrap();
        assert_eq!(input(&a, "P1"), "");
        assert_eq!(input(&a, "P2"), "theirs", "undo must not revert the other person's edit");
        relay(&mut a, &mut b);
        assert_eq!(input(&b, "P1"), "");
        assert_eq!(input(&b, "P2"), "theirs");
        assert_eq!(hash(&a), hash(&b));
        // Redo brings mine back and still keeps theirs.
        a.execute("edit.redo", json!({})).unwrap();
        assert_eq!(input(&a, "P1"), "mine");
        assert_eq!(input(&a, "P2"), "theirs");
        relay(&mut a, &mut b);
        assert_eq!(hash(&a), hash(&b));
    }

    #[test]
    fn undo_keeps_a_later_remote_write_to_the_same_cell() {
        let (mut a, mut b) = (session(), session());
        a.execute("cell.set", json!({"cell": "Q1", "input": "first"})).unwrap();
        relay(&mut a, &mut b);
        b.execute("cell.set", json!({"cell": "Q1", "input": "later"})).unwrap();
        relay(&mut b, &mut a);
        a.execute("edit.undo", json!({})).unwrap();
        assert_eq!(input(&a, "Q1"), "later");
        relay(&mut a, &mut b);
        assert_eq!(hash(&a), hash(&b));
    }

    #[test]
    fn concurrent_bold_and_fill_on_one_cell_both_survive() {
        let (mut a, mut b) = (session(), session());
        a.execute("home.bold", json!({"range": "B2", "on": true})).unwrap();
        b.execute("home.fillColor", json!({"range": "B2", "color": "#00FF00"})).unwrap();
        // Server order: a's batch, then b's; each side applies the other's.
        let ea = a.collab.as_mut().unwrap().take_outbox();
        let eb = b.collab.as_mut().unwrap().take_outbox();
        // Server order: a's batch, then b's. b sees a's first while its own is unconfirmed, so
        // a's write to b's fields is skipped there; a gets b's after its own confirmation.
        for (seq, e) in &ea {
            b.collab_apply(e).unwrap();
            a.collab_confirm(*seq);
        }
        for (seq, e) in &eb {
            a.collab_apply(e).unwrap();
            b.collab_confirm(*seq);
        }
        assert!(style(&a, "B2").font.bold && style(&b, "B2").font.bold);
        assert_eq!(style(&a, "B2").fill, style(&b, "B2").fill);
        assert_eq!(hash(&a), hash(&b));
    }

    #[test]
    fn whole_column_formatting_replicates() {
        let (mut a, mut b) = (session(), session());
        a.execute("selection.set", json!({"range": "E:E"})).unwrap();
        a.execute("home.italic", json!({})).unwrap();
        relay(&mut a, &mut b);
        assert_eq!(hash(&a), hash(&b));
        assert!(style(&b, "E40").font.italic);
    }

    #[test]
    fn hostile_effects_are_refused_without_changes() {
        let mut b = session();
        let h = hash(&b);
        for e in [
            json!([{"sheet": "Nope", "cell": "A1", "input": {"t": "value", "v": {"t": "Number", "v": 1}}}]),
            json!([{"sheet": b.doc().unwrap().wb.sheets[0].name, "cell": "ZZZZZ999999999", "input": {"t": "value", "v": {"t": "Empty"}}}]),
            json!([{"sheet": b.doc().unwrap().wb.sheets[0].name, "cell": "A1", "style": {"font.bold.x.y": true}}]),
            json!([{"sheet": b.doc().unwrap().wb.sheets[0].name, "cell": "A1", "style": {"font": 5}}]),
            json!([{"sheet": b.doc().unwrap().wb.sheets[0].name, "row": 1, "input": {"t": "value", "v": {"t": "Empty"}}}]),
        ] {
            let effects: Vec<Effect> = serde_json::from_value(e).unwrap();
            assert!(b.collab_apply(&effects).is_err());
            assert_eq!(hash(&b), h);
        }
    }

    // ------------------------------------------------------------ convergence fuzz

    struct Client {
        s: Session,
        /// How far into the server log this client has applied.
        applied: usize,
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    fn local_edit(c: &mut Client, r: &mut Rng) {
        let cell = format!("{}{}", ["B", "C", "D", "M", "N"][r.pick(5) as usize], 1 + r.pick(6));
        let range = format!("{cell}:{}", ["N", "O"][r.pick(2) as usize].to_string() + &(1 + r.pick(6)).to_string());
        let what = r.pick(10);
        let res = match what {
            0..=2 => c.s.execute("cell.set", {
                let input = ["x", "=B2+1", "7", "", "=SUM(B5:C6)", "yes"][r.pick(6) as usize];
                json!({"cell": cell, "input": input})
            }),
            3 => c.s.execute("home.bold", json!({"range": range})),
            4 => c.s.execute("home.fillColor", {
                let color = ["#FF0000", "#00FF00", "none"][r.pick(3) as usize];
                json!({"range": cell, "color": color})
            }),
            5 => c.s.execute("home.italic", json!({"range": cell})),
            6 => c.s.execute("edit.undo", json!({})),
            7 => c.s.execute("edit.redo", json!({})),
            8 => c.s.execute("edit.paste", json!({"at": cell, "text": "1\t2\n3\t4"})),
            _ => c.s.execute("home.percent", json!({"range": range})),
        };
        if std::env::var_os("GRIDCRAFT_COLLAB_TRACE").is_some() {
            eprintln!(
                "   edit {what} {cell} {range}: {:?} -> outbox {}",
                res.as_ref().map(|_| ()).map_err(|e| e.to_string()),
                serde_json::to_string(&c.s.collab.as_ref().unwrap().outbox).unwrap_or_default()
            );
        }
    }

    fn send(c: &mut Client, id: usize, log: &mut Vec<(usize, u64, Vec<Effect>)>) {
        for (seq, batch) in c.s.collab.as_mut().unwrap().take_outbox() {
            // The server appends in arrival order.
            log.push((id, seq, batch));
        }
    }

    fn deliver(c: &mut Client, id: usize, log: &[(usize, u64, Vec<Effect>)], upto: usize) {
        while c.applied < upto.min(log.len()) {
            let (from, seq, batch) = &log[c.applied];
            c.applied += 1;
            if *from == id {
                c.s.collab_confirm(*seq);
            } else {
                if std::env::var_os("GRIDCRAFT_COLLAB_TRACE").is_some() {
                    eprintln!("   apply from {from}: {}", serde_json::to_string(batch).unwrap_or_default());
                }
                c.s.collab_apply(batch).unwrap();
            }
        }
    }

    #[test]
    fn two_clients_converge_under_random_interleaving() {
        let seeds: u64 = std::env::var("GRIDCRAFT_COLLAB_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
        let only: Option<u64> = std::env::var("GRIDCRAFT_COLLAB_SEED").ok().and_then(|v| v.parse().ok());
        for seed in only.map_or(1..=seeds, |s| s..=s) {
            let mut r = Rng(0x9e37_79b9_7f4a_7c15 ^ seed.wrapping_mul(0x1000_0000_01b3));
            let mut cs: Vec<Client> = (0..2).map(|_| Client { s: session(), applied: 0 }).collect();
            let mut log: Vec<(usize, u64, Vec<Effect>)> = Vec::new();
            let steps: usize = std::env::var("GRIDCRAFT_COLLAB_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
            let trace = std::env::var_os("GRIDCRAFT_COLLAB_TRACE").is_some();
            for _ in 0..steps {
                let id = r.pick(2) as usize;
                if trace {
                    eprintln!("-- client {id} (applied {})", cs[id].applied);
                }
                match r.pick(4) {
                    0 | 1 => local_edit(&mut cs[id], &mut r),
                    2 => send(&mut cs[id], id, &mut log),
                    _ => {
                        let upto = cs[id].applied + 1 + r.pick(3) as usize;
                        deliver(&mut cs[id], id, &log, upto);
                    }
                }
            }
            for (id, c) in cs.iter_mut().enumerate() {
                send(c, id, &mut log);
            }
            for (id, c) in cs.iter_mut().enumerate() {
                deliver(c, id, &log, usize::MAX);
            }
            assert!(cs.iter().all(|c| !c.s.collab.as_ref().unwrap().has_pending()));
            if hash(&cs[0].s) != hash(&cs[1].s) {
                let (a, b) = (&cs[0].s.doc().unwrap().wb, &cs[1].s.doc().unwrap().wb);
                let d = diff(a, b);
                panic!("seed {seed} diverged: {}", serde_json::to_string(&d).unwrap_or_default());
            }
        }
    }
}
