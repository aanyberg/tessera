//! Shared by the `terminal` and `resize` targets.

use tessera_term::Terminal;

/// Most full [`tessera_grid::Grid::check_invariants`] runs per input.
///
/// A full check costs O(screen + scrollback × columns): with a 200 × 400
/// grid and 10 000 lines of history that is 4 million cells, so checking
/// after each of 4 000 one-byte chunks took a minute for one input and
/// libFuzzer reported a timeout (#14, critic round 1). Inputs of up to
/// this many steps are checked after every step; longer ones after every
/// `ceil(steps / FULL_CHECKS)`-th step and always after the last, so a run
/// costs at most `FULL_CHECKS + 1` full checks.
pub const FULL_CHECKS: usize = 64;

/// Which steps of an input get a full check.
pub struct CheckBudget {
    steps: usize,
    every: usize,
}

impl CheckBudget {
    /// A budget for an input of `steps` steps.
    pub fn new(steps: usize) -> Self {
        Self {
            steps,
            every: steps.div_ceil(FULL_CHECKS).max(1),
        }
    }

    /// Whether step `step` (from zero) gets a full check.
    pub fn due(&self, step: usize) -> bool {
        step.is_multiple_of(self.every) || step + 1 == self.steps
    }
}

/// The checks after one step: the O(1) ones every time, the grid's
/// structural invariants when `full`.
pub fn check(term: &mut Terminal, size: (u16, u16), full: bool) {
    let grid = term.grid();
    assert_eq!(
        (grid.rows(), grid.cols()),
        size,
        "the grid has the wrong size"
    );
    let cursor = grid.cursor();
    assert!(
        cursor.row < size.0 && cursor.col < size.1,
        "cursor {cursor:?} off a {size:?} grid"
    );
    assert!(
        !grid.pending_wrap() || cursor.col + 1 == size.1,
        "pending wrap off the last column at {cursor:?}"
    );
    if full {
        grid.check_invariants();
    }
    assert!((1..=5).contains(&term.conformance_level()));
    // The active screen's kitty stack is read, as a key press would.
    let _ = term.keyboard_modes();
    // Drop the requests (bells, titles) as a host would.
    drop(term.take_requests());
}
