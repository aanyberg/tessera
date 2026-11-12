//! The state the screen-clear tests (#377) start from, shared by
//! `screen_clears.rs` and `screen_clears_alloc.rs`.

use tessera_grid::{Mark, Point};
use tessera_term::Terminal;

/// The history lines [`setup`] scrolls off: `scrollback_pushed` and
/// `scrollback_len` after it.
pub const PUSHED: u64 = 100;

/// The fixture's state before each B row: a 24x80 primary screen with 100
/// lines of history, text and a prompt mark on rows 0 to 5 (row 2 wrapped
/// into row 3), the cursor at (5, 10), one clear counted already (`n` = 1)
/// and taken, and no requests queued.
pub fn setup() -> Terminal {
    let mut term = Terminal::new(24, 80);
    term.advance(&b"history\r\n".repeat(123));
    // n = 1: a clear before the rows were written, so the count does not
    // start at zero.
    term.advance(b"\x1b[2J");
    assert_eq!(term.take_first_clear_top(), Some(PUSHED));
    for row in 1..=6 {
        term.advance(format!("\x1b[{row};1Hrow {row}\x1b]133;A\x07").as_bytes());
    }
    // Row 2 wraps into row 3.
    term.advance(format!("\x1b[3;1H{}z", "w".repeat(80)).as_bytes());
    term.advance(b"\x1b[6;11H");
    let _ = term.take_requests();
    let grid = term.grid();
    assert_eq!(
        (grid.scrollback_pushed(), grid.scrollback_len()),
        (PUSHED, 100)
    );
    assert_eq!(grid.cursor(), Point::new(5, 10));
    assert!(grid.line(2).unwrap().wrapped);
    assert!((0..=5).all(|row| grid.line(row).unwrap().marks.contains(&Mark::PromptStart)));
    assert_eq!(term.screen_clears(), 1);
    term
}

/// What a row expects of the counter: `bumps` more clears than before, the
/// screen-top id `top` taken (none when nothing counted), and the line
/// epoch unchanged in every row.
#[track_caller]
pub fn expect_clears(term: &mut Terminal, before: u64, bumps: u64, top: Option<u64>) {
    assert_eq!(term.screen_clears(), before.wrapping_add(bumps), "clears");
    assert_eq!(term.take_first_clear_top(), top, "first clear's top");
    assert_eq!(term.take_first_clear_top(), None, "taking it clears it");
    assert_eq!(term.line_epoch(), 0, "no erase bumps the line epoch");
}
