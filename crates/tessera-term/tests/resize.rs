//! `Terminal::resize`: the terminal hands a resize to its grid, which
//! re-wraps soft-wrapped lines (`tessera-grid`'s `tests/reflow.rs` covers
//! the rules row by row); these tests check the delegation and that
//! output after a resize continues where the re-wrapped text left off.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;

use common::render_screen;
use tessera_grid::{Point, ScrollRegion, Style};
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
