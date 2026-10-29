//! Raw bytes into `tessera_term::Terminal::advance`: no input panics and the
//! grid's invariants (`Grid::check_invariants`) hold after every chunk.
//!
//! Input layout: `[rows] [cols] [chunk] bytes...`.
//!
//! - `rows`: `0..=200` (the byte modulo 201); zero is passed through, so
//!   `Terminal::new` raising it to one is covered.
//! - `cols`: `0..=200` exactly for bytes up to 200, then 55 coarser steps up
//!   to 400.
//! - `chunk`: the rest is fed in calls of `chunk + 1` bytes (1 to 256), so
//!   sequences and UTF-8 characters straddle calls.
//!
//! The cheap checks (size, cursor, pending wrap) run after every chunk, the
//! full ones within [`common::FULL_CHECKS`] (see there for why).

#![no_main]

mod common;

use libfuzzer_sys::fuzz_target;
use tessera_term::Terminal;

use common::CheckBudget;

/// Columns for a size byte: exact below 201, 200..=400 above.
fn cols(byte: u8) -> u16 {
    let byte = u16::from(byte);
    if byte <= 200 {
        byte
    } else {
        200 + (byte - 200) * 200 / 55
    }
}

fuzz_target!(|data: &[u8]| {
    let [rows, cols_byte, chunk, rest @ ..] = data else {
        return;
    };
    let rows = u16::from(*rows) % 201;
    let cols = cols(*cols_byte);
    let mut term = Terminal::new(rows, cols);
    // DECCOLM never resizes (ADR 0020), so the size is fixed for the run.
    let size = (rows.max(1), cols.max(1));
    let chunk = usize::from(*chunk) + 1;
    let budget = CheckBudget::new(rest.len().div_ceil(chunk));
    for (step, piece) in rest.chunks(chunk).enumerate() {
        term.advance(piece);
        common::check(&mut term, size, budget.due(step));
    }
});
