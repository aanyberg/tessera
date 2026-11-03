//! "Clear to Start" keeps the whole prompt (D-CLEAR-START, ADR 0022; #132):
//! [`Grid::clear_to_cursor`] keeps from the nearest `PromptStart` at or
//! above the cursor on the screen, else from the first row of the cursor's
//! logical line, and never keeps scrollback.

use tessera_grid::{Grid, Mark, Point};

fn texts(grid: &Grid) -> Vec<String> {
    grid.visible_lines()
        .iter()
        .map(tessera_grid::Line::text)
        .collect()
}

fn prompt_start(grid: &mut Grid) {
    let row = grid.cursor().row;
    grid.line_mut(row)
        .expect("cursor row")
        .set_mark(Mark::PromptStart);
}

/// Output, then a prompt whose command wraps over three rows: the whole
/// command line is kept, without marks.
#[test]
fn a_wrapped_command_line_is_kept_whole() {
    let mut grid = Grid::new(6, 10);
    grid.put_str("out 1");
    grid.carriage_return();
    grid.line_feed();
    grid.put_str("out 2");
    grid.carriage_return();
    grid.line_feed();
    grid.put_str("$ echo 0123456789abcdefgh");
    assert_eq!(grid.cursor(), Point::new(4, 5));
    let pending = grid.pending_wrap();

    grid.clear_to_cursor();

    assert_eq!(
        texts(&grid),
        ["$ echo 012", "3456789abc", "defgh", "", "", ""]
    );
    assert_eq!(grid.cursor(), Point::new(2, 5));
    assert_eq!(grid.pending_wrap(), pending);
    assert!(grid.line(0).expect("row 0").wrapped);
    grid.check_invariants();
}

/// A two-line prompt (a status line, then `$ `) with shell integration:
/// the `PromptStart` on its first line wins over the logical-line rule.
#[test]
fn a_two_line_prompt_is_kept_from_its_prompt_start() {
    let mut grid = Grid::new(6, 20);
    grid.put_str("old output");
    grid.carriage_return();
    grid.line_feed();
    prompt_start(&mut grid);
    grid.put_str("~/src main");
    grid.carriage_return();
    grid.line_feed();
    grid.put_str("$ ls");
    assert_eq!(grid.cursor(), Point::new(2, 4));

    grid.clear_to_cursor();

    assert_eq!(texts(&grid), ["~/src main", "$ ls", "", "", "", ""]);
    assert_eq!(grid.cursor(), Point::new(1, 4));
    assert_eq!(
        grid.line(0).expect("row 0").marks,
        vec![Mark::PromptStart],
        "the mark stays with its row"
    );
    grid.check_invariants();
}

/// The nearest `PromptStart` wins: an older prompt's mark higher up does
/// not keep the output between them.
#[test]
fn the_nearest_prompt_start_wins() {
    let mut grid = Grid::new(6, 20);
    prompt_start(&mut grid);
    grid.put_str("$ make");
    grid.carriage_return();
    grid.line_feed();
    grid.put_str("built");
    grid.carriage_return();
    grid.line_feed();
    prompt_start(&mut grid);
    grid.put_str("$ ");
    grid.clear_to_cursor();
    assert_eq!(texts(&grid), ["$", "", "", "", "", ""]);
    assert_eq!(grid.cursor(), Point::new(0, 2));
}

/// While a command runs (here `ssh`, whose remote prompt has no marks),
/// its output stops the upward scan: the `OutputStart` above the cursor
/// comes before the local prompt's `PromptStart`, so the cursor's row is
/// kept, as kitty does (`screen_cursor_at_a_shell_prompt`), not the local
/// prompt and everything after it.
#[test]
fn output_start_stops_the_scan_while_a_command_runs() {
    let mut grid = Grid::new(10, 20);
    prompt_start(&mut grid);
    grid.put_str("$ ssh host");
    grid.carriage_return();
    grid.line_feed();
    let row = grid.cursor().row;
    grid.line_mut(row)
        .expect("cursor row")
        .set_mark(Mark::OutputStart);
    for i in 0..5 {
        grid.put_str(&format!("motd {i}"));
        grid.carriage_return();
        grid.line_feed();
    }
    grid.put_str("remote$ ");
    assert_eq!(grid.cursor(), Point::new(6, 8));

    grid.clear_to_cursor();

    assert_eq!(
        texts(&grid),
        ["remote$", "", "", "", "", "", "", "", "", ""]
    );
    assert_eq!(grid.cursor(), Point::new(0, 8));
}

/// The fallback after an `OutputStart` stop is the cursor's logical line,
/// never above the output's first row: a long remote command line that
/// wraps is kept whole.
#[test]
fn after_an_output_start_stop_a_wrapped_line_is_kept_whole() {
    let mut grid = Grid::new(6, 10);
    prompt_start(&mut grid);
    grid.put_str("$ ssh h");
    grid.carriage_return();
    grid.line_feed();
    let row = grid.cursor().row;
    grid.line_mut(row)
        .expect("cursor row")
        .set_mark(Mark::OutputStart);
    grid.put_str("motd");
    grid.carriage_return();
    grid.line_feed();
    grid.put_str("r$ echo 0123456789");
    assert_eq!(grid.cursor(), Point::new(3, 8));

    grid.clear_to_cursor();

    assert_eq!(texts(&grid), ["r$ echo 01", "23456789", "", "", "", ""]);
    assert_eq!(grid.cursor(), Point::new(1, 8));
}

/// A row with both marks (a command without output: `C`, `D` and the next
/// prompt's `A` all land on it) is a prompt start.
#[test]
fn a_prompt_start_beside_an_output_start_wins() {
    let mut grid = Grid::new(4, 20);
    grid.put_str("old");
    grid.carriage_return();
    grid.line_feed();
    let row = grid.cursor().row;
    let line = grid.line_mut(row).expect("cursor row");
    line.set_mark(Mark::OutputStart);
    line.set_mark(Mark::PromptStart);
    grid.put_str("$ ");
    grid.clear_to_cursor();
    assert_eq!(texts(&grid), ["$", "", "", ""]);
}

/// A `PromptStart` below the cursor (the cursor moved up) is not a start.
#[test]
fn a_prompt_start_below_the_cursor_is_ignored() {
    let mut grid = Grid::new(4, 20);
    grid.put_str("a");
    grid.carriage_return();
    grid.line_feed();
    grid.put_str("b");
    grid.carriage_return();
    grid.line_feed();
    prompt_start(&mut grid);
    grid.goto(1, 0);
    grid.clear_to_cursor();
    assert_eq!(texts(&grid), ["b", "", "", ""]);
    assert_eq!(grid.line(1).expect("row 1").marks, vec![Mark::PromptStart]);
}

/// Without marks or wraps it is today's behaviour: the cursor row becomes
/// row 0.
#[test]
fn without_marks_the_cursor_row_becomes_the_top() {
    let mut grid = Grid::with_scrollback(3, 10, 100);
    for line in ["a", "b", "c", "d", "e"] {
        grid.put_str(line);
        grid.carriage_return();
        grid.line_feed();
    }
    grid.put_str("$ ");
    assert_eq!(grid.scrollback_len(), 3);
    grid.clear_to_cursor();
    assert_eq!(grid.scrollback_len(), 0);
    assert_eq!(texts(&grid), ["$", "", ""]);
    assert_eq!(grid.cursor(), Point::new(0, 2));
}

/// A logical line that started in the scrollback is kept from the
/// screen's top row: the scrollback always goes.
#[test]
fn a_line_from_the_scrollback_is_kept_from_the_top_row() {
    let mut grid = Grid::with_scrollback(2, 4, 100);
    prompt_start(&mut grid);
    grid.put_str("$ 0123456789");
    assert!(grid.scrollback_len() > 0);
    let before = texts(&grid);
    grid.clear_to_cursor();
    assert_eq!(grid.scrollback_len(), 0);
    assert_eq!(texts(&grid), before);
    assert_eq!(grid.cursor(), Point::new(1, 3));
    assert!(grid.pending_wrap());
}

/// The alternate screen keeps its rows, whatever marks or wraps.
#[test]
fn the_alternate_screen_is_unchanged() {
    let mut grid = Grid::with_scrollback(3, 4, 100);
    grid.put_str("a");
    grid.carriage_return();
    grid.line_feed();
    grid.enter_alt_screen();
    grid.put_str("0123456");
    let before = texts(&grid);
    let cursor = grid.cursor();
    grid.clear_to_cursor();
    assert_eq!(texts(&grid), before);
    assert_eq!(grid.cursor(), cursor);
}
