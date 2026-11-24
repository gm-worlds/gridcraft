//! A host's compact layout (Taskforce patch).
//!
//! On a phone the ribbon is a desktop toolbar squeezed into a third of the width, and it takes a
//! quarter of the screen with the status bar. A host that draws its own small toolbar asks for the
//! compact layout: no ribbon and no status bar; the formula bar, the sheet tabs and the grid stay.
//! Every command stays reachable (keyboard, cell menu, the host's buttons).

use std::cell::Cell;

thread_local! {
    static COMPACT: Cell<bool> = const { Cell::new(false) };
}

/// Switches the compact layout on or off. The caller asks for a repaint.
pub fn set_compact(on: bool) {
    COMPACT.with(|c| c.set(on));
}

/// True while the host asked for the compact layout.
pub fn compact() -> bool {
    COMPACT.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_is_off_until_a_host_asks() {
        assert!(!compact());
        set_compact(true);
        assert!(compact());
        set_compact(false);
        assert!(!compact());
    }
}
