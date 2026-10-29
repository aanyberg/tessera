//! `Terminal::resize`: the terminal hands a resize to its grid, which
//! re-wraps soft-wrapped lines (`tessera-grid`'s `tests/reflow.rs` covers
//! the rules row by row); these tests check the delegation and that
//! output after a resize continues where the re-wrapped text left off.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;

use common::render_screen;
use tessera_grid::{Color, Point, ScrollRegion, Style};
use tessera_term::Terminal;

/// Fixture row A01 of `tessera-grid`'s `reflow_spec.txt`, through the
/// terminal: a soft-wrapped line is re-split on shrink and rejoined on
/// grow.
#[test]
fn resize_rewraps_a_soft_wrapped_line() {
    let mut term = Terminal::new(3, 8);
    term.advance(b"abcdefghijkl");
    assert_eq!(render_screen(&term), "abcdefgh\\\nijkl |\n\n");
    term.resize(3, 5);
    assert_eq!((term.grid().rows(), term.grid().cols()), (3, 5));
    assert_eq!(render_screen(&term), "abcde\\\nfghij\\\nkl |\n");
    assert_eq!(term.grid().cursor(), Point::new(2, 2));
    term.resize(3, 8);
    assert_eq!(render_screen(&term), "abcdefgh\\\nijkl |\n\n");
    assert_eq!(term.grid().scrollback_len(), 0);
}

/// A pending wrap re-armed by the resize wraps the next character instead
/// of overwriting the last one.
#[test]
fn output_after_a_resize_continues_after_the_text() {
    let mut term = Terminal::new(3, 8);
    term.advance(b"abcdefgh");
    term.resize(3, 4);
    assert!(term.grid().pending_wrap());
    term.advance(b"ij");
    assert_eq!(render_screen(&term), "abcd\\\nefgh\\\nij |\n");
    term.advance(b"\r\n$ ");
    term.resize(2, 12);
    assert_eq!(render_screen(&term), "abcdefghij\n$  |\n");
}

/// The scroll region set by DECSTBM is reset to the full screen, and a
/// zero size is raised to one row and column.
#[test]
fn resize_resets_the_scroll_region_and_clamps_zero() {
    let mut term = Terminal::new(6, 10);
    term.advance(b"\x1b[2;4r");
    assert_eq!(
        term.grid().scroll_region(),
        ScrollRegion { top: 1, bottom: 3 }
    );
    term.resize(8, 10);
    assert_eq!(
        term.grid().scroll_region(),
        ScrollRegion { top: 0, bottom: 7 }
    );
    term.resize(0, 0);
    assert_eq!((term.grid().rows(), term.grid().cols()), (1, 1));
}

/// Review round 1: trailing spaces drawn with SGR attributes (an inverse
/// status bar, an underlined input field) are text, not padding: a
/// shrink and a grow keep every styled cell.
#[test]
fn styled_trailing_spaces_survive_a_resize() {
    let mut term = Terminal::new(3, 20);
    term.advance(b"\x1b[7mstatus             \x1b[0m\r\n\x1b[4mname:     \x1b[0m");
    let count = |term: &Terminal, flag: u16| {
        term.grid()
            .logical_lines()
            .flat_map(|line| line.cells.iter())
            .filter(|cell| cell.style.has(flag))
            .count()
    };
    assert_eq!(count(&term, Style::INVERSE), 19);
    assert_eq!(count(&term, Style::UNDERLINE), 10);
    term.resize(3, 15);
    assert_eq!(count(&term, Style::INVERSE), 19, "at 15 columns");
    assert_eq!(count(&term, Style::UNDERLINE), 10, "at 15 columns");
    term.resize(3, 20);
    assert_eq!(count(&term, Style::INVERSE), 19, "back at 20 columns");
    assert_eq!(count(&term, Style::UNDERLINE), 10, "back at 20 columns");
    let line = |row: u16| term.grid().line(row).map(|l| l.cells[..19].to_vec());
    assert!(
        line(term.grid().cursor().row - 1)
            .is_some_and(|cells| cells.iter().all(|c| c.style.has(Style::INVERSE))),
        "the status bar is one row again"
    );
}

fn screen_text(term: &Terminal) -> Vec<String> {
    term.grid()
        .logical_lines()
        .map(tessera_grid::Line::text)
        .collect()
}

fn spaces(term: &Terminal) -> usize {
    let grid = term.grid();
    let mut count = 0;
    let lines: Vec<_> = grid.logical_lines().collect();
    let mut logical = String::new();
    for (i, line) in lines.iter().enumerate() {
        let last = line.cells.len() - 1;
        for (col, cell) in line.cells.iter().enumerate() {
            let pad = cell.is_pad() && col == last && line.wrapped && i + 1 < lines.len();
            if !cell.is_spacer() && !pad {
                logical.push(cell.ch);
            }
        }
        if !line.wrapped {
            count += logical.trim_end().matches(' ').count();
            logical.clear();
        }
    }
    count + logical.trim_end().matches(' ').count()
}

/// Verify cycle 1: a space in the last column before a wide character is
/// text; widening keeps it.
#[test]
fn a_space_before_a_wide_char_survives_widening() {
    let mut term = Terminal::new(2, 10);
    term.advance("가나 다라 마바".as_bytes());
    term.resize(2, 20);
    assert_eq!(screen_text(&term)[0], "가나 다라 마바");
}

/// Verify cycle 1: dragging the width down and back one column at a time
/// keeps every space of Korean text.
#[test]
fn a_column_by_column_drag_keeps_every_space() {
    let mut term = Terminal::new(24, 80);
    let line = "한국어 텍스트도 포함합니다 그리고 띄어쓰기가 있는 문장을 여러 번 반복합니다\r\n";
    for _ in 0..30 {
        term.advance(line.as_bytes());
    }
    let before = spaces(&term);
    assert_eq!(before, 270);
    for cols in (40..80).rev().chain(41..=80) {
        term.resize(24, cols);
    }
    assert_eq!(spaces(&term), before);
}

/// #88: a background band that a wide character wraps through keeps its
/// colour in the padding a re-wrap leaves behind (widen, then narrow
/// back): the same cell the live write left.
#[test]
fn reflow_padding_keeps_the_band_colour() {
    let mut term = Terminal::new(3, 5);
    term.advance("\x1b[44mabcd中ef\x1b[K\x1b[0m".as_bytes());
    let pad = |term: &Terminal| term.grid().cell(Point::new(0, 4)).copied();
    let live = pad(&term);
    assert!(live.is_some_and(|c| c.is_pad() && c.style.bg == Color::Indexed(4)));
    term.resize(3, 6);
    assert_eq!(render_screen(&term), "abcd中\\\nef |\n\n");
    term.resize(3, 5);
    let cell = pad(&term).expect("row 0 has five columns");
    assert!(cell.is_pad(), "{cell:?}");
    assert_eq!(cell.style.bg, Color::Indexed(4), "{cell:?}");
    assert_eq!(Some(cell), live, "the cell a live write leaves");
    assert_eq!(
        term.grid()
            .cell(Point::new(1, 0))
            .map(|c| (c.ch, c.is_wide())),
        Some(('中', true))
    );
}

/// The padding a wrapped wide character leaves is cleared by DECALN, and
/// insert mode pads the stranded column like replace mode.
#[test]
fn padding_follows_the_terminal_paths() {
    let mut term = Terminal::new(3, 5);
    term.advance("abcd中".as_bytes());
    let pad = |term: &Terminal| {
        term.grid()
            .cell(Point::new(0, 4))
            .map(tessera_grid::Cell::is_pad)
    };
    assert_eq!(pad(&term), Some(true));
    term.advance(b"\x1b#8");
    assert_eq!(pad(&term), Some(false), "DECALN writes over it");
    let mut term = Terminal::new(3, 5);
    term.set_insert_mode(true);
    term.advance("abcd中".as_bytes());
    assert_eq!(pad(&term), Some(true), "insert mode pads too");
    term.resize(3, 8);
    assert_eq!(screen_text(&term)[0], "abcd中");
}

/// #88, hostile drag: coloured, underlined lines of mixed CJK and ASCII,
/// resized one column at a time from 40 down to 2 and back, keep every
/// glyph's style, and every padding cell has the background of the wide
/// character after it and nothing else of its style.
#[test]
fn a_column_drag_keeps_padding_in_the_band_colour() {
    let mut term = Terminal::new(6, 40);
    for n in 0..12u8 {
        let line = format!(
            "\x1b[4{};3{};{}m中a文bc日本 語x{n}\x1b[K\x1b[0m\r\n",
            n % 8,
            (n + 3) % 8,
            if n % 2 == 0 { 4 } else { 7 }
        );
        term.advance(line.as_bytes());
    }
    let glyphs = |term: &Terminal| {
        let mut out = Vec::new();
        for line in term.grid().logical_lines() {
            for cell in &line.cells {
                if !cell.is_spacer() && !cell.is_pad() && cell.ch != ' ' {
                    out.push((cell.ch, cell.style));
                }
            }
        }
        out
    };
    let before = glyphs(&term);
    for cols in (2..40).rev().chain(3..=40) {
        term.resize(6, cols);
        let lines: Vec<_> = term.grid().logical_lines().collect();
        for (i, line) in lines.iter().enumerate() {
            for (col, cell) in line.cells.iter().enumerate() {
                if cell.is_pad() {
                    let wide = lines.get(i + 1).and_then(|l| l.cells.first());
                    assert!(col + 1 == line.cells.len() && line.wrapped, "at {cols}");
                    assert_eq!(
                        wide.map(|w| (w.is_wide(), tessera_grid::Cell::blank_with(w.style).style)),
                        Some((true, cell.style)),
                        "at {cols}"
                    );
                }
            }
        }
        assert_eq!(glyphs(&term), before, "at {cols} columns");
    }
}

// ------------------------------------------------- #86: reflow after clear
//
// Rows R86-01..R86-10 of the spec researcher's fixture
// `reflow_after_clear_spec.txt` (derived from the resize code of kitty,
// ghostty, Alacritty, WezTerm and iTerm2; ADR 0009, amendment of
// 2026-10-29). Only growth of what was on the screen pushes the screen;
// lines wholly in history only lengthen history, and a height grow pulls
// history back only when the screen is anchored at the bottom.

/// The scrollback, oldest first, one row per entry, right-trimmed, with a
/// trailing `\` on a wrapped row (the notation of [`render_screen`]).
fn history(term: &Terminal) -> Vec<String> {
    let grid = term.grid();
    (0..grid.scrollback_len())
        .filter_map(|i| grid.scrollback_line(i))
        .map(|line| {
            let mut text = line.text().trim_end().to_owned();
            if line.wrapped {
                text.push('\\');
            }
            text
        })
        .collect()
}

/// Seven 6-character lines on a 4x8 screen leave `1aaaaa`..`4ddddd` in
/// history and `5eeeee`..`7ggggg` on the screen, cursor on the last row.
const SEVEN_LINES: &[u8] =
    b"1aaaaa\r\n2bbbbb\r\n3ccccc\r\n4ddddd\r\n5eeeee\r\n6fffff\r\n7ggggg\r\n";

/// The history [`SEVEN_LINES`] leaves, at 8 and at 4 columns.
const HISTORY_8: [&str; 4] = ["1aaaaa", "2bbbbb", "3ccccc", "4ddddd"];
const HISTORY_4: [&str; 8] = [
    "1aaa\\", "aa", "2bbb\\", "bb", "3ccc\\", "cc", "4ddd\\", "dd",
];

/// A 4x8 terminal fed `bytes`.
fn small(bytes: &[u8]) -> Terminal {
    let mut term = Terminal::new(4, 8);
    term.advance(bytes);
    term
}

/// [`SEVEN_LINES`], then `ESC[H ESC[2J $ `: the prompt on row 0 of an
/// otherwise empty screen, four lines in history.
fn cleared() -> Terminal {
    let mut bytes = SEVEN_LINES.to_vec();
    bytes.extend_from_slice(b"\x1b[H\x1b[2J$ ");
    small(&bytes)
}

/// R86-01: narrowing after a clear re-wraps history in place and leaves
/// the prompt on row 0 (main pulled three rows of old output back and put
/// the prompt on the last row).
#[test]
fn r86_01_narrowing_after_a_clear_keeps_the_prompt_on_row_zero() {
    let mut term = cleared();
    assert_eq!(render_screen(&term), "$  |\n\n\n\n");
    assert_eq!(history(&term), HISTORY_8);
    term.resize(4, 4);
    assert_eq!(render_screen(&term), "$  |\n\n\n\n");
    assert_eq!(term.grid().cursor(), Point::new(0, 2));
    assert!(!term.grid().pending_wrap());
    assert_eq!(history(&term), HISTORY_4);
}

/// R86-02: widening back restores the state before the narrow exactly.
#[test]
fn r86_02_widening_back_after_a_clear_restores_everything() {
    let mut term = cleared();
    term.resize(4, 4);
    term.resize(4, 8);
    assert_eq!(render_screen(&term), "$  |\n\n\n\n");
    assert_eq!(term.grid().cursor(), Point::new(0, 2));
    assert_eq!(history(&term), HISTORY_8);
}

/// 123 lines of 70 characters and a CR LF each on a 24x80 terminal, then
/// `ESC[H ESC[2J $ `: 100 lines in history (ED 2 erases the 23 still on
/// the screen in place), prompt on row 0.
fn cleared_full_scale() -> Terminal {
    let mut term = Terminal::new(24, 80);
    for i in 1..=123 {
        let line = format!("{i:03}{}\r\n", "x".repeat(67));
        term.advance(line.as_bytes());
    }
    term.advance(b"\x1b[H\x1b[2J$ ");
    term
}

/// The screen with only the prompt on row 0, at any height.
fn prompt_only(rows: usize) -> String {
    format!("$  |\n{}", "\n".repeat(rows - 1))
}

/// R86-03 (the issue's repro 1): 80 -> 40 columns re-wraps the 100
/// history lines to 200 rows and leaves the prompt alone; 40 -> 80
/// restores 100 rows, prompt still at (0,2).
#[test]
fn r86_03_issue_repro_clear_then_narrow_full_scale() {
    let mut term = cleared_full_scale();
    assert_eq!(term.grid().scrollback_len(), 100);
    assert_eq!(term.grid().cursor(), Point::new(0, 2));
    term.resize(24, 40);
    assert_eq!(render_screen(&term), prompt_only(24));
    assert_eq!(term.grid().cursor(), Point::new(0, 2));
    assert_eq!(term.grid().scrollback_len(), 200);
    term.resize(24, 80);
    assert_eq!(render_screen(&term), prompt_only(24));
    assert_eq!(term.grid().cursor(), Point::new(0, 2));
    assert_eq!(term.grid().scrollback_len(), 100);
    let first = history(&term)[0].clone();
    assert_eq!(
        first,
        format!("001{}", "x".repeat(67)),
        "oldest line intact"
    );
}

/// R86-04 (the issue's repro 2): alternating the height by one row never
/// moves the prompt and never pulls history (the cursor is not on the last
/// row), at both scales.
#[test]
fn r86_04_alternating_the_height_by_one_row_is_idempotent() {
    let mut term = cleared();
    for cycle in 0..10 {
        term.resize(3, 8);
        assert_eq!(render_screen(&term), prompt_only(3), "cycle {cycle} shrink");
        assert_eq!(history(&term), HISTORY_8, "cycle {cycle} shrink");
        term.resize(4, 8);
        assert_eq!(render_screen(&term), prompt_only(4), "cycle {cycle} grow");
        assert_eq!(history(&term), HISTORY_8, "cycle {cycle} grow");
    }
    let mut term = cleared_full_scale();
    term.resize(24, 40);
    for cycle in 0..10 {
        term.resize(23, 40);
        assert_eq!(term.grid().cursor(), Point::new(0, 2), "cycle {cycle}");
        assert_eq!(term.grid().scrollback_len(), 200, "cycle {cycle}");
        term.resize(24, 40);
        assert_eq!(render_screen(&term), prompt_only(24), "cycle {cycle}");
        assert_eq!(term.grid().scrollback_len(), 200, "cycle {cycle}");
    }
    for cycle in 0..10 {
        term.resize(23, 80);
        term.resize(24, 80);
        assert_eq!(render_screen(&term), prompt_only(24), "80 columns {cycle}");
        assert_eq!(term.grid().scrollback_len(), 100, "80 columns {cycle}");
    }
}

/// R86-05 (the issue's repro 3, unchanged from main): without a clear and
/// with the cursor on the last row the screen stays anchored at the
/// bottom both ways, and the round trip restores it.
#[test]
fn r86_05_without_a_clear_the_screen_stays_at_the_bottom() {
    let mut term = small(SEVEN_LINES);
    assert_eq!(render_screen(&term), "5eeeee\n6fffff\n7ggggg\n |\n");
    term.resize(4, 4);
    assert_eq!(render_screen(&term), "ff\n7ggg\\\ngg\n |\n");
    assert_eq!(term.grid().cursor(), Point::new(3, 0));
    let mut expected = HISTORY_4.to_vec();
    expected.extend(["5eee\\", "ee", "6fff\\"]);
    assert_eq!(history(&term), expected);
    term.resize(4, 8);
    assert_eq!(render_screen(&term), "5eeeee\n6fffff\n7ggggg\n |\n");
    assert_eq!(history(&term), HISTORY_8);
    // And by rows: the shrink pushes, the grow pulls back.
    for _ in 0..3 {
        term.resize(2, 8);
        assert_eq!(render_screen(&term), "7ggggg\n |\n");
        term.resize(4, 8);
        assert_eq!(render_screen(&term), "5eeeee\n6fffff\n7ggggg\n |\n");
        assert_eq!(history(&term), HISTORY_8);
    }
}

/// The issue's repro 3 at full scale: 100 lines of 70 characters, no
/// clear, cursor on row 23: narrowing shows the last 24 of the 200 rows.
#[test]
fn r86_05_full_scale_narrow_without_a_clear() {
    let mut term = Terminal::new(24, 80);
    for i in 1..=100 {
        term.advance(format!("{i:03}{}\r\n", "x".repeat(67)).as_bytes());
    }
    assert_eq!(term.grid().cursor(), Point::new(23, 0));
    let before = render_screen(&term);
    term.resize(24, 40);
    assert_eq!(term.grid().cursor(), Point::new(23, 0));
    // 100 lines -> 200 rows plus the cursor row; the screen is the last 24.
    assert_eq!(term.grid().scrollback_len(), 177);
    let screen = render_screen(&term);
    let rows: Vec<&str> = screen.lines().collect();
    assert_eq!(rows[0], "x".repeat(30), "second half of line 089");
    assert_eq!(rows[21], format!("100{}\\", "x".repeat(37)));
    assert_eq!(rows[23], " |");
    term.resize(24, 80);
    assert_eq!(render_screen(&term), before, "the round trip restores it");
    assert_eq!(term.grid().scrollback_len(), 77);
}

/// R86-06 (lead decision D3): growth of what is on the screen is still
/// absorbed by the blank rows below the cursor first and then pushes the
/// top rows into history, as on main.
#[test]
fn r86_06_onscreen_growth_still_pushes_the_screen() {
    let mut term = small(
        b"1aaaaa\r\n2bbbbb\r\n3ccccc\r\n4ddddd\r\n5eeeee\r\n6fffff\r\n\
          \x1b[H\x1b[2J4ddddd\r\n5eeeee\r\n$ ",
    );
    assert_eq!(render_screen(&term), "4ddddd\n5eeeee\n$  |\n\n");
    term.resize(4, 4);
    assert_eq!(render_screen(&term), "dd\n5eee\\\nee\n$  |\n");
    assert_eq!(term.grid().cursor(), Point::new(3, 2));
    assert_eq!(
        history(&term),
        ["1aaa\\", "aa", "2bbb\\", "bb", "3ccc\\", "cc", "4ddd\\"]
    );
}

/// R86-07: a prompt below row 0 of a cleared screen keeps its row: the
/// rule keeps what is above the cursor, not "prompt on row 0".
#[test]
fn r86_07_a_prompt_below_row_zero_keeps_its_row() {
    let mut bytes = SEVEN_LINES.to_vec();
    bytes.extend_from_slice(b"\x1b[H\x1b[2J\x1b[3;1H$ ");
    let mut term = small(&bytes);
    term.resize(4, 4);
    assert_eq!(render_screen(&term), "\n\n$  |\n\n");
    assert_eq!(term.grid().cursor(), Point::new(2, 2));
    assert_eq!(history(&term), HISTORY_4);
}

/// R86-08: with the scrollback erased (ED 3) there is nothing to pull.
#[test]
fn r86_08_clear_scrollback_then_narrow() {
    let mut bytes = SEVEN_LINES.to_vec();
    bytes.extend_from_slice(b"\x1b[H\x1b[2J\x1b[3J$ ");
    let mut term = small(&bytes);
    term.resize(4, 4);
    assert_eq!(render_screen(&term), "$  |\n\n\n\n");
    assert_eq!(term.grid().scrollback_len(), 0);
}

/// R86-09: the prompt line itself wraps; its extra row is paid for by a
/// blank row below the cursor, its first row stays on row 0, and widening
/// back restores the screen.
#[test]
fn r86_09_a_wrapping_prompt_line_is_absorbed_below() {
    let mut bytes = SEVEN_LINES.to_vec();
    bytes.extend_from_slice(b"\x1b[H\x1b[2J$ 12345");
    let mut term = small(&bytes);
    assert_eq!(term.grid().cursor(), Point::new(0, 7));
    term.resize(4, 4);
    assert_eq!(render_screen(&term), "$ 12\\\n345 |\n\n\n");
    assert_eq!(term.grid().cursor(), Point::new(1, 3));
    assert_eq!(history(&term), HISTORY_4);
    term.resize(4, 8);
    assert_eq!(render_screen(&term), "$ 12345 |\n\n\n\n");
    assert_eq!(history(&term), HISTORY_8);
}

/// A cleared screen with history above it whose newest rows are back on
/// the screen: `output` (ending in a line break) scrolls through a
/// one-row terminal into history, ED 2 clears the screen, a grow to
/// `1 + pull` rows with the cursor on the last row pulls the newest
/// `pull` history rows back (possibly the tail of a line, whose head stays
/// in history), the cursor goes home, a resize to `rows` adds blank rows
/// or drops them (nothing is pushed), then `after` runs. None of that
/// scrolls the screen or pushes rows into history, so it stays cleared.
fn cleared_with_history(cols: u16, output: &[u8], pull: u16, rows: u16, after: &[u8]) -> Terminal {
    let mut term = Terminal::new(1, cols);
    term.advance(output);
    term.advance(b"\x1b[2J");
    term.resize(1 + pull, cols);
    term.advance(b"\x1b[H");
    term.resize(rows, cols);
    term.advance(after);
    assert!(!term.grid().scrolled_since_clear());
    term
}

/// R86-10 (lead decision D4): a logical line straddling history and the
/// screen of a cleared screen splits where the old top screen row
/// starts: the row holding that cell and everything after it are the
/// screen, everything before it is history, and the round trip restores
/// both.
#[test]
fn r86_10_a_straddling_line_splits_at_the_old_top_row() {
    let mut term = cleared_with_history(8, b"0\r\nabcdefghijkl\r\n", 1, 3, b"\x1b[2;1H$ ");
    assert_eq!(render_screen(&term), "ijkl\n$  |\n\n");
    assert_eq!(history(&term), ["0", "abcdefgh\\"]);
    term.resize(3, 4);
    assert_eq!(render_screen(&term), "ijkl\n$  |\n\n");
    assert_eq!(history(&term), ["0", "abcd\\", "efgh\\"]);
    term.resize(3, 8);
    assert_eq!(render_screen(&term), "ijkl\n$  |\n\n");
    assert_eq!(history(&term), ["0", "abcdefgh\\"]);
    // At three columns the old top row's first cell (`i`) shares its row
    // with `gh`: that row is the screen's first, and the blank row below
    // the cursor absorbs the growth.
    term.resize(3, 3);
    assert_eq!(render_screen(&term), "ghi\\\njkl\n$  |\n");
    assert_eq!(history(&term), ["0", "abc\\", "def\\"]);
    // Widening back takes the rest of that shared row: the top row moves
    // back one row of the straddling line (no rule without memory can
    // tell which cells of `ghi` were on the screen), and never past the
    // line's start: from there the round trip is exact.
    term.resize(3, 8);
    assert_eq!(render_screen(&term), "abcdefgh\\\nijkl\n$  |\n");
    assert_eq!(history(&term), ["0"]);
    for _ in 0..3 {
        term.resize(3, 3);
        term.resize(3, 8);
        assert_eq!(render_screen(&term), "abcdefgh\\\nijkl\n$  |\n");
        assert_eq!(history(&term), ["0"]);
    }
}

/// #86 (lead decision 2026-10-29, A''): a screen whose cursor sits inside
/// a long command line that wraps onto the last row (Ctrl-A on a long
/// command) is anchored at the bottom like a cursor on the last row: a
/// height or width change and back gives the same screen, as on main.
/// ghostty's literal rule (cursor on the last row) would leave `6fffff` in
/// history after either round trip.
#[test]
fn a_cursor_inside_a_wrapped_last_line_stays_anchored() {
    let mut bytes = SEVEN_LINES.to_vec();
    bytes.extend_from_slice(b"$ abcdefghij\x1b[A\x1b[3G");
    let screen = "6fffff\n7ggggg\n$ a|bcdef\\\nghij\n";
    assert_eq!(render_screen(&small(&bytes)), screen);
    let mut rows = small(&bytes);
    rows.resize(3, 8);
    assert_eq!(render_screen(&rows), "7ggggg\n$ a|bcdef\\\nghij\n");
    rows.resize(4, 8);
    assert_eq!(render_screen(&rows), screen, "the grow pulls 6fffff back");
    let mut cols = small(&bytes);
    cols.resize(4, 5);
    assert_eq!(render_screen(&cols), "g\n$ a|bc\\\ndefgh\\\nij\n");
    cols.resize(4, 8);
    assert_eq!(render_screen(&cols), screen, "the widen pulls 6fffff back");
}

/// A cleared 3x4 screen whose top row is an empty continuation of the
/// history line `abcd`: the line's tail is pulled back from history (see
/// [`cleared_with_history`]), then EL 2 erases it on the top two rows (ED
/// 2 would end the history line too).
fn erased_continuation(after: &[u8]) -> Terminal {
    let mut after_erase = b"\x1b[1;1H\x1b[2K\x1b[2;1H\x1b[2K".to_vec();
    after_erase.extend_from_slice(after);
    cleared_with_history(4, b"abcdefghij\r\n", 2, 3, &after_erase)
}

/// #86: the top row of a cleared screen is an empty continuation of a
/// line in history (EL 2 erased it). That row has no text, so the screen
/// keeps none of the line: narrowing leaves the line's rows in history,
/// the empty row stays a row of its own and the prompt keeps its row.
#[test]
fn an_erased_continuation_row_stays_out_of_the_screen() {
    let mut term = erased_continuation(b"\x1b[2;1H$ ");
    assert_eq!(history(&term), ["abcd\\"]);
    assert_eq!(render_screen(&term), "\n$  |\n\n");
    term.resize(3, 2);
    assert_eq!(render_screen(&term), "\n$ |\n\n");
    assert_eq!(history(&term), ["ab\\", "cd"]);
    term.resize(3, 4);
    // The cursor clamped onto the last column at two columns stays there
    // (fixture D02).
    assert_eq!(render_screen(&term), "\n$ |\n\n");
    assert_eq!(history(&term), ["abcd"], "the dead wrap link is gone");
}

/// R86-10 with wide characters (the cell-by-cell split): the straddling
/// line's top row starts with a wide character; the block starts on the
/// row holding it at every width, and the round trip restores both.
#[test]
fn r86_10_a_straddling_line_of_wide_chars() {
    let mut term = cleared_with_history(8, "0\r\nabcdef中文\r\n".as_bytes(), 1, 3, b"\x1b[2;1H$ ");
    assert_eq!(render_screen(&term), "文\n$  |\n\n");
    assert_eq!(history(&term), ["0", "abcdef中\\"]);
    term.resize(3, 3);
    assert_eq!(render_screen(&term), "文\n$  |\n\n");
    assert_eq!(history(&term), ["0", "abc\\", "def\\", "中\\"]);
    term.resize(3, 8);
    assert_eq!(render_screen(&term), "文\n$  |\n\n");
    assert_eq!(history(&term), ["0", "abcdef中\\"]);
    // At five columns `文` shares its row with `f中`.
    term.resize(3, 5);
    assert_eq!(render_screen(&term), "f中文\n$  |\n\n");
    assert_eq!(history(&term), ["0", "abcde\\"]);
}

/// #86, critic round 1: a cursor on an erased continuation row keeps its
/// column when that row becomes a blank row of its own (and is clamped
/// to the last column, like any cursor on blanks, when that is
/// narrower).
#[test]
fn a_cursor_on_an_erased_continuation_row_keeps_its_column() {
    let mut term = erased_continuation(b"\x1b[1;3H");
    assert_eq!(history(&term), ["abcd\\"]);
    assert_eq!(term.grid().cursor(), Point::new(0, 2));
    term.resize(3, 3);
    assert_eq!(term.grid().cursor(), Point::new(0, 2));
    assert_eq!(render_screen(&term), "   |\n\n\n");
    assert_eq!(history(&term), ["abc\\", "d"]);
    term.resize(3, 4);
    assert_eq!(term.grid().cursor(), Point::new(0, 2));
    assert_eq!(render_screen(&term), "   |\n\n\n");
    assert_eq!(history(&term), ["abcd"]);
    term.resize(3, 2);
    assert_eq!(term.grid().cursor(), Point::new(0, 1), "clamped");
    assert_eq!(history(&term), ["ab\\", "cd"]);
}

// ------------------------- #86 verify cycle 1: a prompt redrawn by its shell

/// 123 lines of 70 characters on a 24x80 terminal, then zsh's prompt
/// `% ` with a right prompt `[rprompt]` (bytes from zsh 5.9).
fn rprompt_session() -> Terminal {
    let mut term = Terminal::new(24, 80);
    for i in 1..=123 {
        term.advance(format!("L{i:03}-{}\r\n", "x".repeat(66)).as_bytes());
    }
    term.advance(rprompt(80).as_bytes());
    term
}

/// zsh's prompt redraw on `SIGWINCH` at `cols` columns: back to the
/// prompt's start, ED 0, the prompt, the right prompt one column from the
/// edge, back after the prompt.
fn rprompt_redraw(cols: u16) -> String {
    format!("\r\x1b[J{}", rprompt(cols))
}

/// zsh's `% ` prompt with `[rprompt]` drawn at the right edge.
fn rprompt(cols: u16) -> String {
    format!("% \x1b[K\x1b[{}C[rprompt]\x1b[{}D", cols - 12, cols - 3)
}

/// Adversarial verify, cycle 1 (the major): a shell scrolled to the
/// bottom whose right prompt wraps at the narrow redraws its prompt with
/// ED 0, which leaves a blank row below the cursor. That screen has
/// scrolled, so the widen still pulls history back exactly as before #86:
/// the prompt stays on row 22, not row 11 with twelve blank rows below.
#[test]
fn a_redrawn_right_prompt_stays_at_the_bottom() {
    let mut term = rprompt_session();
    assert!(term.grid().scrolled_since_clear());
    assert_eq!(term.grid().cursor(), Point::new(23, 2));
    term.resize(24, 40);
    assert_eq!(term.grid().cursor(), Point::new(22, 2));
    term.advance(rprompt_redraw(40).as_bytes());
    assert_eq!(term.grid().cursor(), Point::new(22, 2));
    term.resize(24, 80);
    assert_eq!(term.grid().cursor(), Point::new(22, 2));
    assert_eq!(term.grid().scrollback_len(), 101);
    let screen = render_screen(&term);
    let rows: Vec<&str> = screen.lines().collect();
    assert_eq!(rows[0], format!("L102-{}", "x".repeat(66)));
    assert_eq!(rows[21], format!("L123-{}", "x".repeat(66)));
    assert!(rows[22].starts_with("%  |"), "{}", rows[22]);
    assert_eq!(rows[23], "", "one blank row, as before #86");
}

/// The same with a window drag: every width from 80 down to 50 and back,
/// each followed by the shell's redraw (as powerlevel10k's right prompt
/// does). The prompt never leaves the bottom two rows, the row above it
/// is always the newest output, and no output is lost or doubled.
#[test]
fn a_redrawn_right_prompt_survives_a_window_drag() {
    let mut term = rprompt_session();
    for cols in (50..80).rev().chain(51..=80) {
        term.resize(24, cols);
        term.advance(rprompt_redraw(cols).as_bytes());
        let row = term.grid().cursor().row;
        assert!(row >= 22, "at {cols} columns the prompt is on row {row}");
        let above = term.grid().line(row - 1).map(tessera_grid::Line::text);
        assert!(
            above.as_deref().is_some_and(|t| t.ends_with('x')),
            "at {cols} columns the row above the prompt is {above:?}"
        );
    }
    let lines: Vec<String> = term
        .grid()
        .logical_lines()
        .map(tessera_grid::Line::text)
        .filter(|t| t.starts_with('L'))
        .collect();
    // Back at 80 columns every line is one row again.
    let expected: Vec<String> = (1..=123)
        .map(|i| format!("L{i:03}-{}", "x".repeat(66)))
        .collect();
    assert_eq!(lines, expected);
}

/// The cleared counterpart: Ctrl-L (zsh: `ESC[H ESC[2J`) before the same
/// prompt. The screen has not scrolled since the clear, so narrowing,
/// the redraw and widening keep the prompt on row 0 (#86).
#[test]
fn a_redrawn_right_prompt_after_a_clear_stays_on_row_zero() {
    let mut term = Terminal::new(24, 80);
    for i in 1..=123 {
        term.advance(format!("L{i:03}-{}\r\n", "x".repeat(66)).as_bytes());
    }
    term.advance(b"\x1b[H\x1b[2J");
    term.advance(rprompt(80).as_bytes());
    assert!(!term.grid().scrolled_since_clear());
    for cols in [40, 80, 60, 80] {
        term.resize(24, cols);
        term.advance(rprompt_redraw(cols).as_bytes());
        assert_eq!(term.grid().cursor(), Point::new(0, 2), "at {cols}");
    }
    assert_eq!(term.grid().scrollback_len(), 100);
}

/// Adversarial verify, cycle 1 (minor 1): ED 2 erases the continuation of
/// a wrapped history line on the top row, so it ends that line: the
/// prompt written there afterwards is a line of its own, not glued to
/// the old output by a later re-wrap.
#[test]
fn ed2_ends_the_history_line_it_erased_the_continuation_of() {
    let mut term = Terminal::new(3, 4);
    term.advance(b"abcdefghijklm");
    assert_eq!(history(&term), ["abcd\\"]);
    term.advance(b"\x1b[2J\x1b[H$ ");
    assert_eq!(history(&term), ["abcd"]);
    term.resize(3, 2);
    assert_eq!(render_screen(&term), "$ |\n\n\n");
    assert_eq!(history(&term), ["ab\\", "cd"]);
    term.resize(3, 8);
    assert_eq!(render_screen(&term), "$ |\n\n\n");
    assert_eq!(history(&term), ["abcd"]);
}

/// #86 verify cycles 1 and 2: the escape sequences behind
/// `scrolled_since_clear`: output that scrolls sets it; ED 0 away from
/// home (a shell's prompt redraw), EL, DECSTR, cursor motion, ED 1 and
/// ED 3 keep it; ED 2, ED 0 from home (vt100/screen/tmux `clear`, Node's
/// `console.clear`) and RIS clear it.
#[test]
fn scrolled_since_clear_through_the_terminal() {
    let mut term = Terminal::new(3, 8);
    term.advance(b"a\r\nb\r\nc");
    assert!(!term.grid().scrolled_since_clear());
    term.advance(b"\r\nd");
    assert!(term.grid().scrolled_since_clear());
    term.advance(b"\r\x1b[J\x1b[2K\x1b[!p\x1b[H\x1b[1J\x1b[3J");
    assert!(
        term.grid().scrolled_since_clear(),
        "ED 0 off home, EL, DECSTR, CUP, ED 1, ED 3"
    );
    for clear in [
        &b"\x1b[2J"[..],
        b"\x1b[H\x1b[J",
        b"\x1b[1;1H\x1b[0J",
        b"\x1bc",
    ] {
        term.advance(b"\x1b[3;1H\r\n\r\n");
        assert!(term.grid().scrolled_since_clear());
        term.advance(clear);
        assert!(!term.grid().scrolled_since_clear(), "{clear:?}");
    }
}

/// #86 verify cycle 1: on a screen that has scrolled, a narrow still lets
/// the blank row a prompt redraw left below the cursor absorb the growth
/// of history too, exactly as before #86 (one buffer anchored at the
/// bottom), even when the screen's own lines do not grow.
#[test]
fn a_scrolled_screen_absorbs_history_growth_as_before() {
    let mut term = small(b"1aaaaaa\r\n2aaaaaa\r\nb\r\nc\r\n$ \r\n\x1b[A\r\x1b[J$ ");
    assert!(term.grid().scrolled_since_clear());
    assert_eq!(render_screen(&term), "b\nc\n$  |\n\n");
    term.resize(4, 4);
    assert_eq!(render_screen(&term), "aaa\nb\nc\n$  |\n");
    assert_eq!(history(&term), ["1aaa\\", "aaa", "2aaa\\"]);
}

/// #86 critic round 1, revisited after verify cycle 1: content below the
/// cursor on a screen that has scrolled (eight 7-character lines on 4x8,
/// cursor moved to row 0) round-trips through a narrow as before #86.
#[test]
fn content_below_the_cursor_of_a_scrolled_screen_round_trips() {
    let mut term = small(
        b"1aaaaaa\r\n2bbbbbb\r\n3cccccc\r\n4dddddd\r\n5eeeeee\r\n6ffffff\r\n7gggggg\r\n8hhhhhh\x1b[H",
    );
    let screen = "5|eeeeee\n6ffffff\n7gggggg\n8hhhhhh\n";
    assert_eq!(render_screen(&term), screen);
    term.resize(4, 4);
    term.resize(4, 8);
    assert_eq!(
        render_screen(&term).replace('|', ""),
        screen.replace('|', "")
    );
    assert_eq!(term.grid().scrollback_len(), 4);
}

/// #86 (A''): Ctrl-A on a long wrapped command on a screen that has not
/// scrolled since it was cleared: the cursor's line ends on the last row,
/// so the screen counts as anchored at the bottom and both round trips
/// pull back what the shrink or narrow pushed. ghostty's literal rule
/// (cursor on the last row) would leave `b` in history.
#[test]
fn ctrl_a_on_a_cleared_screen_stays_anchored() {
    let make = || cleared_with_history(8, b"a\r\nb\r\n$ abcdefghij\r\n", 3, 3, b"\x1b[2;3H");
    let screen = "b\n$ a|bcdef\\\nghij\n";
    let mut rows = make();
    assert_eq!(render_screen(&rows), screen);
    assert_eq!(history(&rows), ["a"]);
    rows.resize(2, 8);
    assert_eq!(render_screen(&rows), "$ a|bcdef\\\nghij\n");
    rows.resize(3, 8);
    assert_eq!(render_screen(&rows), screen);
    let mut cols = make();
    cols.resize(3, 5);
    assert_eq!(render_screen(&cols), "$ a|bc\\\ndefgh\\\nij\n");
    cols.resize(3, 8);
    assert_eq!(render_screen(&cols), screen);
    assert_eq!(history(&cols), ["a"]);
}

// ------------------------- #86 verify cycle 2: what else counts as a clear

/// Drags `term` through `widths` (24 rows), redrawing zsh's right prompt
/// after each resize, and checks the prompt stays at the bottom with the
/// newest output right above it.
fn drag_with_redraws(term: &mut Terminal, widths: &[u16]) {
    for &cols in widths {
        term.resize(24, cols);
        term.advance(rprompt_redraw(cols).as_bytes());
        let row = term.grid().cursor().row;
        assert!(row >= 22, "at {cols} columns the prompt is on row {row}");
        let above = term.grid().line(row - 1).map(tessera_grid::Line::text);
        assert!(
            above.as_deref().is_some_and(|t| t.ends_with('x')),
            "at {cols} columns the row above the prompt is {above:?}"
        );
    }
}

/// Lead decision E1: ED 3 empties history but leaves a screen full of
/// scrolled output, so it does not count as a clear: a drag with the
/// right prompt redrawn keeps the prompt at the bottom, as before #86.
#[test]
fn ed3_keeps_a_scrolled_screen_at_the_bottom() {
    let mut term = rprompt_session();
    term.advance(b"\x1b[3J");
    assert_eq!(term.grid().scrollback_len(), 0);
    assert!(term.grid().scrolled_since_clear());
    drag_with_redraws(&mut term, &[60, 40, 60, 80]);
    assert_eq!(term.grid().cursor(), Point::new(22, 2));
}

/// Lead decision E2: after Ctrl-L, a screenful of output that does not
/// scroll fills the screen; the narrow pushes its top rows into history,
/// which makes the screen count as scrolled, so the widen pulls them back
/// and the prompt stays at the bottom, as before #86.
#[test]
fn a_cleared_screen_filled_to_the_bottom_stays_at_the_bottom() {
    let mut term = Terminal::new(24, 80);
    for i in 1..=50 {
        term.advance(format!("O{i:03}-{}\r\n", "x".repeat(66)).as_bytes());
    }
    term.advance(b"\x1b[H\x1b[2J");
    for i in 1..=23 {
        term.advance(format!("L{i:03}-{}\r\n", "x".repeat(66)).as_bytes());
    }
    term.advance(rprompt(80).as_bytes());
    assert_eq!(term.grid().cursor(), Point::new(23, 2));
    assert!(!term.grid().scrolled_since_clear(), "nothing scrolled yet");
    drag_with_redraws(&mut term, &[60, 40, 60, 80]);
    assert!(term.grid().scrolled_since_clear(), "the narrow pushed rows");
    assert_eq!(term.grid().cursor(), Point::new(22, 2));
}

/// Lead decision m1: ED 0 from the home position erases the whole screen
/// (the `clear` of vt100, screen and tmux terminfo; Node's
/// `console.clear`), so on a top row of its own it is a clear: narrowing
/// keeps the prompt on row 0 (R86-01 with those sequences).
#[test]
fn ed0_from_home_is_a_clear() {
    for clear in [&b"\x1b[H\x1b[J"[..], b"\x1b[1;1H\x1b[0J"] {
        let mut bytes = SEVEN_LINES.to_vec();
        bytes.extend_from_slice(clear);
        bytes.extend_from_slice(b"$ ");
        let mut term = small(&bytes);
        assert!(!term.grid().scrolled_since_clear(), "{clear:?}");
        term.resize(4, 4);
        assert_eq!(render_screen(&term), "$  |\n\n\n\n", "{clear:?}");
        assert_eq!(history(&term), HISTORY_4, "{clear:?}");
    }
}

/// #86 (A''), no rows pushed: on a cleared screen whose last two rows are
/// a long command with the cursor at its start (Ctrl-A), a grow and a
/// widen take history back as for a cursor on the last row (here `h2`,
/// from before the clear). ghostty's literal rule would add blank rows at
/// the bottom instead.
#[test]
fn ctrl_a_anchors_a_cleared_screen_without_a_push() {
    let make = || {
        cleared_with_history(
            8,
            b"h1\r\nh2\r\nh3\r\n",
            1,
            3,
            b"\x1b[2;1H$ abcdefghij\x1b[2;3H",
        )
    };
    let mut rows = make();
    assert_eq!(render_screen(&rows), "h3\n$ a|bcdef\\\nghij\n");
    assert_eq!(history(&rows), ["h1", "h2"]);
    rows.resize(4, 8);
    assert_eq!(render_screen(&rows), "h2\nh3\n$ a|bcdef\\\nghij\n");
    assert!(!rows.grid().scrolled_since_clear());
    let mut cols = make();
    cols.resize(3, 12);
    assert_eq!(render_screen(&cols), "h2\nh3\n$ a|bcdefghij\n");
    assert_eq!(history(&cols), ["h1"]);
}

/// #86, lead decision after round 2: zsh's `CR ED 0` redraw lands at the
/// home position when a narrow made the prompt line taller than the
/// window and pushed its first row into history. The top row continues
/// that history line, so it is a redraw, not a clear: the screen still
/// counts as scrolled and the widen pulls history back as before #86.
#[test]
fn a_prompt_redraw_from_home_is_not_a_clear() {
    let mut term = Terminal::new(2, 40);
    for i in 1..=10 {
        term.advance(format!("L{i:03}-{}\r\n", "x".repeat(30)).as_bytes());
    }
    term.advance(rprompt(40).as_bytes());
    term.resize(2, 13);
    assert_eq!(term.grid().cursor().row, 0, "the prompt's first row left");
    term.advance(rprompt_redraw(13).as_bytes());
    assert!(term.grid().scrolled_since_clear(), "a redraw, not a clear");
    let newest = term.grid().scrollback_len() - 1;
    assert!(
        term.grid()
            .scrollback_line(newest)
            .is_some_and(|l| l.wrapped),
        "the history line still continues on the top row"
    );
}

/// The cost of that rule: a vt100 `clear` (`ESC[H ESC[J`) issued while the
/// top row continues a wrapped history line looks like that redraw and
/// does not count as a clear (`ESC[2J` does).
#[test]
fn a_vt100_clear_inside_a_wrapped_line_is_not_a_clear() {
    let mut term = Terminal::new(3, 4);
    term.advance(b"abcdefghijklm\x1b[H\x1b[J");
    assert!(term.grid().scrolled_since_clear());
    assert_eq!(history(&term), ["abcd\\"]);
    term.advance(b"\x1b[2J");
    assert!(!term.grid().scrolled_since_clear());
    assert_eq!(history(&term), ["abcd"]);
}
