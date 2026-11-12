//! The screen-clear count (#377, ADR 0039 amendment of 2026-11-11):
//! [`Terminal::screen_clears`] counts the erases of the whole primary
//! screen and [`Terminal::take_first_clear_top`] gives the screen-top line
//! id of the first one since the last take, so a reader can tell that the
//! rows its command records point at were blanked in place. Rows B01 to
//! B15 of the spec fixture
//! (`crates/tessera-app-core/tests/fixtures/clear_generation_spec.txt`);
//! the app-core half (rows E01 to E14) is `tessera-app-core`'s
//! `tests/clear_generation.rs`. B13 counts allocations, so it is in
//! `screen_clears_alloc.rs`, a binary of its own.

use proptest::prelude::*;
use tessera_grid::{Mark, Point};
use tessera_term::Terminal;

mod screen_clears_common;

use screen_clears_common::{PUSHED, expect_clears, setup};

/// Feeds `bytes` to a fresh [`setup`] and checks the count.
#[track_caller]
fn row(bytes: &[u8], bumps: u64) -> Terminal {
    let mut term = setup();
    term.advance(bytes);
    let top = (bumps > 0).then_some(PUSHED);
    expect_clears(&mut term, 1, bumps, top);
    term
}

/// Whether every visible row is blank and has no marks and no wrap flag.
fn screen_blank(term: &Terminal) -> bool {
    term.grid().visible_lines().iter().all(|line| {
        line.marks.is_empty() && !line.wrapped && line.cells.iter().all(|cell| cell.ch == ' ')
    })
}

/// Whether the rows from `row` down are blank and not wrapped.
fn screen_blank_below(term: &Terminal, row: usize) -> bool {
    term.grid().visible_lines()[row..]
        .iter()
        .all(|line| !line.wrapped && line.cells.iter().all(|cell| cell.ch == ' '))
}

#[test]
fn b01_ed_2_counts_one_clear_in_place() {
    let mut term = setup();
    term.advance(b"\x1b[2J");
    expect_clears(&mut term, 1, 1, Some(PUSHED));
    let grid = term.grid();
    assert!(screen_blank(&term));
    assert_eq!(grid.cursor(), Point::new(5, 10), "the cursor stays");
    assert_eq!(
        (grid.scrollback_len(), grid.scrollback_pushed()),
        (100, PUSHED),
        "nothing goes into the history"
    );
    // The pending wrap is disarmed.
    let mut term = setup();
    term.advance(b"\x1b[6;80Hx\x1b[2J");
    assert!(!term.grid().pending_wrap());
    expect_clears(&mut term, 1, 1, Some(PUSHED));
}

#[test]
fn b02_cup_and_ed_2_count_once() {
    let term = row(b"\x1b[H\x1b[2J", 1);
    assert!(screen_blank(&term));
    assert_eq!(term.grid().cursor(), Point::new(0, 0));
}

#[test]
fn b03_ed_0_at_home_counts() {
    let term = row(b"\x1b[H\x1b[J", 1);
    let grid = term.grid();
    assert!(grid.line(0).unwrap().cells.iter().all(|c| c.ch == ' '));
    // The cursor's row is erased as EL 0 is, from column 0: it keeps its
    // marks (as before #377); the rows below lose theirs.
    assert!((1..24).all(|row| grid.line(row).unwrap().marks.is_empty()));
    assert!(screen_blank_below(&term, 0));
    // The explicit parameter too.
    row(b"\x1b[H\x1b[0J", 1);
}

/// #86: ED 0 at home on a top row that continues the newest history line
/// is a shell redrawing a line that began above the screen, not a clear.
#[test]
fn b04_ed_0_at_home_on_a_row_continuing_the_history_does_not_count() {
    let mut term = Terminal::new(24, 80);
    // 25 rows of one logical line: the first scrolls off, wrapped.
    term.advance("y".repeat(25 * 80).as_bytes());
    assert!(term.grid().scrollback_line(0).unwrap().wrapped);
    term.advance(b"\x1b[H\x1b[J");
    assert!(
        term.grid()
            .line(1)
            .unwrap()
            .cells
            .iter()
            .all(|c| c.ch == ' '),
        "the rows below were erased"
    );
    expect_clears(&mut term, 0, 0, None);
}

#[test]
fn b05_ed_0_below_home_does_not_count() {
    // zsh's `CR ED 0` before every prompt.
    let term = row(b"\x1b[4;1H\r\x1b[J", 0);
    let grid = term.grid();
    assert!(grid.line(2).unwrap().marks.contains(&Mark::PromptStart));
    assert!((4..24).all(|row| grid.line(row).unwrap().marks.is_empty()));
    assert!(screen_blank_below(&term, 3));
    // From column 2 of the top row: not home either.
    row(b"\x1b[1;2H\x1b[J", 0);
}

#[test]
fn b06_partial_erases_do_not_count() {
    row(b"\x1b[1J", 0);
    row(b"\x1b[0J", 0);
    row(b"\x1b[J", 0);
    row(b"\x1b[K", 0);
    row(b"\x1b[2K", 0);
    row(b"\x1b[H\x1b[2K\x1b[1J", 0);
}

#[test]
fn b07_ed_3_alone_does_not_count() {
    let mut term = setup();
    term.advance(b"\x1b[6;80Hx");
    assert!(term.grid().pending_wrap());
    term.advance(b"\x1b[3J");
    expect_clears(&mut term, 1, 0, None);
    let grid = term.grid();
    assert_eq!(grid.scrollback_len(), 0, "the history is gone");
    assert_eq!(grid.scrollback_pushed(), PUSHED, "the count is kept");
    assert!(grid.pending_wrap(), "ED 3 leaves the pending wrap");
    assert!((0..=5).all(|row| grid.line(row).unwrap().marks.contains(&Mark::PromptStart)));
}

/// macOS `clear` sends ED 3 first, Homebrew's last: one clear either way,
/// and the same state.
#[test]
fn b08_both_orders_of_clear_count_one() {
    let macos = row(b"\x1b[3J\x1b[H\x1b[2J", 1);
    let homebrew = row(b"\x1b[H\x1b[2J\x1b[3J", 1);
    for term in [&macos, &homebrew] {
        assert_eq!(term.grid().scrollback_len(), 0);
        assert!(screen_blank(term));
    }
    assert!(
        macos
            .grid()
            .visible_lines()
            .iter()
            .eq(homebrew.grid().visible_lines())
    );
    assert_eq!(macos.grid().cursor(), homebrew.grid().cursor());
}

/// DECSED 2 counts with protection off, DEC (DECSCA) and ISO (SPA): the
/// lines lose their marks even where protected cells stay.
#[test]
fn b09_decsed_2_counts_with_and_without_protection() {
    row(b"\x1b[?2J", 1);
    // DEC protection: DECSED keeps the protected cell.
    let term = row(b"\x1b[1;1H\x1b[1\"qP\x1b[0\"q\x1b[?2J", 1);
    assert_eq!(term.grid().line(0).unwrap().cells[0].ch, 'P');
    assert_eq!(term.grid().line(0).unwrap().marks, []);
    // ISO protection: ED keeps it too.
    for erase in [&b"\x1b[2J"[..], b"\x1b[?2J"] {
        let mut bytes = b"\x1b[1;1H\x1bVP\x1bW".to_vec();
        bytes.extend_from_slice(erase);
        let term = row(&bytes, 1);
        assert_eq!(term.grid().line(0).unwrap().cells[0].ch, 'P');
        assert!(
            term.grid()
                .visible_lines()
                .iter()
                .all(|l| l.marks.is_empty())
        );
    }
}

/// Nothing on the alternate screen counts: its erases (on entry, ED 2,
/// ED 0 at home, the 1047 exit erase) and the primary screen is left as
/// it was.
#[test]
fn b10_the_alternate_screen_never_counts() {
    let before = setup();
    for bytes in [
        &b"\x1b[?1049h\x1b[H\x1b[2J\x1b[?1049l"[..],
        b"\x1b[?1049h\x1b[H\x1b[J\x1b[?2J\x1b[?1049l",
        b"\x1b[?1047h\x1b[2J\x1b[?1047l",
        b"\x1b[?47h\x1b[2J\x1b[?47l",
    ] {
        let term = row(bytes, 0);
        let (a, b) = (term.grid(), before.grid());
        assert!(a.visible_lines().iter().eq(b.visible_lines()), "{bytes:?}");
        assert_eq!(a.scrollback_len(), b.scrollback_len());
    }
}

/// RIS blanks the screen and its marks as ED 2 does, and counts once,
/// also with the alternate screen up (its stashed primary rows are
/// dropped). The top is the line id of the screen at the reset.
#[test]
fn b11_ris_counts_one_clear() {
    let term = row(b"\x1bc", 1);
    assert!(screen_blank(&term));
    row(b"\x1b[?1049h\x1bc", 1);
}

/// DECCOLM erases the primary screen through ED 2: one clear, with the
/// column change allowed (DECSET 40) and only then.
#[test]
fn b12_deccolm_counts_when_allowed() {
    let term = row(b"\x1b[?40h\x1b[?3h", 1);
    assert!(screen_blank(&term));
    row(b"\x1b[?40h\x1b[?3h\x1b[?3l", 2);
    row(b"\x1b[?3h", 0);
    // On the alternate screen it erases that screen: no count.
    row(b"\x1b[?40h\x1b[?1049h\x1b[?3h\x1b[?1049l", 0);
}

/// Everything that is not a whole-screen erase of the primary screen
/// leaves the count alone; a resize bumps the line epoch only.
#[test]
fn b14_nothing_else_counts() {
    for bytes in [
        &b"hello"[..],
        b"\x1b[1;31mred\x1b[m",
        b"\x1b[H\x1b[10;10H",
        b"\x1b[2K\x1b[1K\x1b[K",
        b"\x1b[1J",
        b"\x1b[5;10r\x1b[r",
        b"\x1b[3S\x1b[2T\x1bD\x1bM\x1bE",
        b"\x1b[2L\x1b[2M\x1b[3P\x1b[3@\x1b[5X",
        b"\x1b#8",
        b"\x1b[!p",
        &b"x\r\n".repeat(40),
    ] {
        row(bytes, 0);
    }
    let mut term = setup();
    term.resize(30, 100);
    assert_eq!(term.screen_clears(), 1);
    assert_eq!(term.take_first_clear_top(), None);
    assert_eq!(term.line_epoch(), 1, "a resize bumps the epoch only");
}

/// A CSI split across `advance` calls counts once, when its final byte
/// arrives.
#[test]
fn b15_a_split_csi_counts_once() {
    let mut term = setup();
    for piece in [&b"\x1b"[..], b"[", b"H", b"\x1b[2"] {
        term.advance(piece);
        assert_eq!(term.screen_clears(), 1, "not before the final byte");
    }
    term.advance(b"J");
    expect_clears(&mut term, 1, 1, Some(PUSHED));
}

/// The top is the line id of the screen's top row at the first clear
/// since the last take, the lowest: later clears after output scrolled do
/// not move it.
#[test]
fn the_first_clear_since_the_take_gives_the_top() {
    let mut term = setup();
    term.advance(b"\x1b[2J");
    term.advance(&b"\r\n".repeat(30));
    term.advance(b"\x1b[2J");
    assert_eq!(term.screen_clears(), 3);
    assert_eq!(term.take_first_clear_top(), Some(PUSHED));
    term.advance(b"\x1b[2J");
    assert_eq!(term.take_first_clear_top(), Some(PUSHED + 30 - 18));
}

/// Bytes that clear, partly clear, scroll and switch screens, in pieces.
fn clear_bytes() -> impl Strategy<Value = Vec<u8>> {
    let piece = prop::sample::select(vec![
        &b"\x1b[2J"[..],
        b"\x1b[H",
        b"\x1b[J",
        b"\x1b[?2J",
        b"\x1b[3J",
        b"\x1b[1J",
        b"\x1b[5;3H",
        b"\x1bc",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[?40h\x1b[?3h",
        b"abc",
        b"\r\n",
        &[b'y'; 81],
        b"\x1b]133;A\x07",
    ]);
    prop::collection::vec(piece, 0..40).prop_map(|pieces| pieces.concat())
}

proptest! {
    /// The count and the top are the same however the output is split
    /// (fixture D3 for the terminal half): one call, or one per piece of
    /// 1 to 7 bytes.
    #[test]
    fn the_count_and_the_top_do_not_depend_on_chunking(
        bytes in clear_bytes(),
        lens in prop::collection::vec(1usize..8, 1..64),
    ) {
        let mut whole = Terminal::new(6, 20);
        whole.advance(&bytes);
        let mut chunked = Terminal::new(6, 20);
        let mut rest = &bytes[..];
        for len in lens.iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (now, later) = rest.split_at((*len).min(rest.len()));
            chunked.advance(now);
            rest = later;
        }
        prop_assert_eq!(whole.screen_clears(), chunked.screen_clears());
        prop_assert_eq!(whole.take_first_clear_top(), chunked.take_first_clear_top());
        prop_assert_eq!(whole.line_epoch(), 0);
    }
}
