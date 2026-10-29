//! RIS (`ESC c`) after any history leaves the terminal exactly like a new
//! one of the same size, however many arrive in a row (#107: the grid
//! resets in place instead of rebuilding), and ⌘K's
//! `Terminal::clear_to_cursor`.

use std::time::{Duration, Instant};

use proptest::prelude::*;
use tessera_term::Terminal;

/// Everything observable about a terminal's screen and modes. `Grid`'s
/// `Debug` covers the screen, cursor, pen, region, saved cursors, tab
/// stops and the alternate screen, but shows only the scrollback length,
/// so the history is compared line by line as well.
fn state(term: &Terminal) -> (String, Vec<tessera_grid::Line>, bool, bool) {
    let grid = term.grid();
    (
        format!("{grid:?}"),
        grid.logical_lines().cloned().collect(),
        term.auto_wrap(),
        term.insert_mode(),
    )
}

#[derive(Debug, Clone)]
enum Step {
    Bytes(Vec<u8>),
    Resize(u16, u16),
    AutoWrap(bool),
    InsertMode(bool),
    ClearToCursor,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        8 => prop::sample::select(vec![
            b"hello world ".to_vec(),
            "中文😀e\u{301}".as_bytes().to_vec(),
            b"\r\n".to_vec(),
            b"\n\n\n\n".to_vec(),
            b"\x1b[2;4r".to_vec(),
            b"\x1b[r".to_vec(),
            b"\x1b[41;1m".to_vec(),
            b"\x1b[3S".to_vec(),
            b"\x1b[2T".to_vec(),
            b"\x1b[2L".to_vec(),
            b"\x1b7".to_vec(),
            b"\x1b[5;3H".to_vec(),
            b"\x1b[3g".to_vec(),
            b"\x1bH".to_vec(),
            b"\x1b[2J".to_vec(),
            b"\x1b[3J".to_vec(),
            b"\x1b#8".to_vec(),
            // Fills the line to its end: RIS lands on a pending wrap.
            b"0123456789abcdefghij".to_vec(),
            // A sequence cut by RIS: ESC c aborts it.
            b"\x1b[38;2;1".to_vec(),
        ])
        .prop_map(Step::Bytes),
        3 => Just(Step::Bytes(b"\x1bc".to_vec())),
        1 => (1..=12u16, 1..=16u16).prop_map(|(r, c)| Step::Resize(r, c)),
        1 => any::<bool>().prop_map(Step::AutoWrap),
        1 => any::<bool>().prop_map(Step::InsertMode),
        1 => Just(Step::ClearToCursor),
    ]
}

fn run(term: &mut Terminal, step: &Step) {
    match step {
        Step::Bytes(bytes) => term.advance(bytes),
        Step::Resize(r, c) => term.resize(*r, *c),
        Step::AutoWrap(on) => term.set_auto_wrap(*on),
        Step::InsertMode(on) => term.set_insert_mode(*on),
        Step::ClearToCursor => term.clear_to_cursor(),
    }
}

proptest! {
    /// After any history and an RIS, the terminal equals a new one of its
    /// size, and stays equal to it under the same input afterwards.
    #[test]
    fn ris_leaves_a_new_terminal(
        before in prop::collection::vec(step(), 0..40),
        after in prop::collection::vec(step(), 0..20),
    ) {
        let mut term = Terminal::new(6, 10);
        for step in &before {
            run(&mut term, step);
        }
        term.advance(b"\x1bc");
        let (rows, cols) = (term.grid().rows(), term.grid().cols());
        let mut fresh = Terminal::new(rows, cols);
        prop_assert_eq!(state(&term), state(&fresh));
        for step in &after {
            run(&mut term, step);
            run(&mut fresh, step);
            prop_assert_eq!(state(&term), state(&fresh));
        }
    }
}

#[test]
fn ris_flood_after_output_leaves_a_new_terminal() {
    let mut term = Terminal::new(24, 80);
    for i in 0..500 {
        term.advance(format!("line {i} \x1b[1;31mred\x1b[m\r\n").as_bytes());
    }
    term.advance(b"\x1b[3;9r\x1b7\x1b[10;70Htail");
    term.advance(&b"\x1bc".repeat(4096));
    let fresh = Terminal::new(24, 80);
    assert_eq!(state(&term), state(&fresh));
}

#[test]
fn clear_to_cursor_keeps_the_prompt_line_on_top() {
    let mut term = Terminal::new(4, 10);
    term.advance(b"a\r\nb\r\nc\r\nd\r\ne\r\n$ ls");
    assert_eq!(term.grid().scrollback_len(), 2);
    term.clear_to_cursor();
    let grid = term.grid();
    assert_eq!(grid.scrollback_len(), 0);
    assert_eq!(grid.visible_lines()[0].text(), "$ ls");
    assert!(
        grid.visible_lines()[1..]
            .iter()
            .all(|l| l.text().is_empty())
    );
    assert_eq!((grid.cursor().row, grid.cursor().col), (0, 4));
}

#[test]
fn rep_after_clear_to_cursor_has_nothing_to_repeat() {
    let mut term = Terminal::new(3, 10);
    term.advance(b"\r\n\r\nx");
    term.clear_to_cursor();
    term.advance(b"\x1b[3b");
    assert_eq!(term.grid().visible_lines()[0].text(), "x");
}

/// The best of five timings of one 64 KiB read
/// of 32768 RIS on a `rows` x `cols` terminal (10 000-line cap) after
/// `history` lines of output.
fn flood_time(rows: u16, cols: u16, history: usize) -> Duration {
    let flood = b"\x1bc".repeat(32_768);
    let mut best = Duration::MAX;
    for _ in 0..5 {
        let mut term = Terminal::new(rows, cols);
        for i in 0..history {
            term.advance(format!("output line {i}\r\n").as_bytes());
        }
        let start = Instant::now();
        term.advance(&flood);
        best = best.min(start.elapsed());
        assert_eq!(term.grid().cursor().row, 0);
        assert_eq!(term.grid().scrollback_len(), 0);
    }
    best
}

/// 32768 RIS in one read (a 64 KiB chunk): on 96x430 after 2000 lines,
/// on 24x80, and on 96x430 with the 10 000-line scrollback full (the
/// first RIS drops it). Before #107 each RIS rebuilt the grid and the
/// 96x430 read took about 568 ms; the bar is 60 ms for each. Only
/// meaningful in release: `cargo test --release -p tessera-term --test ris
/// -- --ignored --nocapture`. A debug build checks every reset cell by
/// cell and only prints the times.
#[test]
#[ignore = "timing: run in release with --ignored"]
fn ris_flood_on_a_large_terminal_is_fast() {
    for (rows, cols, history) in [(96, 430, 2000), (24, 80, 2000), (96, 430, 10_200)] {
        let best = flood_time(rows, cols, history);
        println!("32768 x RIS on {rows}x{cols} after {history} lines: best of 5 {best:?}");
        if !cfg!(debug_assertions) {
            assert!(best < Duration::from_millis(60), "{rows}x{cols}: {best:?}");
        }
    }
}
