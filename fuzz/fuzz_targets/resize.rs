//! Raw bytes into `tessera_term::Terminal` with resizes in between: no input
//! panics and the grid's invariants hold before and after every resize, so
//! reflow, the stashed primary screen and the saved cursors are covered.
//!
//! Input layout: `[rows] [cols] bytes...`. Sizes are `rows` modulo 101 and
//! `cols` modulo 201 (zero raised to one by the terminal). In the bytes,
//! `0xFE r c` (a byte that never occurs in UTF-8) resizes to `r` × `c`,
//! mapped the same way, up to [`MAX_RESIZES`] times; the bytes between
//! two resizes are fed with one `advance` call. Later `0xFE r c` triples
//! are dropped.

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;
use tessera_term::Terminal;

use common::CheckBudget;

/// Most resizes honoured per input. A column change re-wraps the whole
/// scrollback (up to 10 000 lines), so an input made of nothing but
/// resizes would otherwise time out.
const MAX_RESIZES: usize = 32;

/// The resize opcode.
const RESIZE: u8 = 0xFE;

enum Step<'a> {
    Feed(&'a [u8]),
    Resize(u16, u16),
}

fn size(rows: u8, cols: u8) -> (u16, u16) {
    (u16::from(rows) % 101, u16::from(cols) % 201)
}

/// Splits `bytes` into feeds and at most [`MAX_RESIZES`] resizes.
fn steps(mut bytes: &[u8]) -> Vec<Step<'_>> {
    let mut steps = Vec::new();
    let mut resizes = 0;
    while let Some(at) = bytes.iter().position(|&b| b == RESIZE) {
        if at > 0 {
            steps.push(Step::Feed(&bytes[..at]));
        }
        // A truncated opcode at the end is dropped.
        let Some(&[rows, cols]) = bytes.get(at + 1..at + 3) else {
            return steps;
        };
        if resizes < MAX_RESIZES {
            let (rows, cols) = size(rows, cols);
            steps.push(Step::Resize(rows, cols));
            resizes += 1;
        }
        bytes = &bytes[at + 3..];
    }
    if !bytes.is_empty() {
        steps.push(Step::Feed(bytes));
    }
    steps
}

fuzz_target!(|data: &[u8]| {
    let [rows, cols, rest @ ..] = data else {
        return;
    };
    let (rows, cols) = size(*rows, *cols);
    let mut term = Terminal::new(rows, cols);
    let mut current = (rows.max(1), cols.max(1));
    let steps = steps(rest);
    let budget = CheckBudget::new(steps.len());
    for (step, action) in steps.iter().enumerate() {
        match *action {
            Step::Feed(bytes) => {
                term.advance(bytes);
                common::check(&mut term, current, budget.due(step));
            }
            Step::Resize(rows, cols) => {
                term.resize(rows, cols);
                current = (rows.max(1), cols.max(1));
                // Always in full: a resize is the operation under test.
                common::check(&mut term, current, true);
            }
        }
    }
});
