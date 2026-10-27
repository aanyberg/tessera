//! Property test: no sequence of operations takes the grid out of bounds
//! or panics.

use proptest::prelude::*;
use tessera_grid::{EraseDisplay, EraseLine, Grid, Style};

#[derive(Debug, Clone)]
enum Op {
    Put(char),
    Goto(u16, u16),
    Up(u16),
    Down(u16),
    Left(u16),
    Right(u16),
    Cr,
    Lf,
    Ri,
    Save,
    Restore,
    SetTab,
    ClearTab,
    ClearAllTabs,
    TabForward(u16),
    TabBackward(u16),
    SetRegion(u16, u16),
    ResetRegion,
    ScrollUp(u16),
    ScrollDown(u16),
    InsertLines(u16),
    DeleteLines(u16),
    InsertChars(u16),
    DeleteChars(u16),
    EraseChars(u16),
    EraseLine(EraseLine),
    EraseDisplay(EraseDisplay),
    Pen(u16),
    Reset,
}

fn count() -> impl Strategy<Value = u16> {
    prop_oneof![
        4 => 0..=10u16,
        1 => 0..=u16::MAX,
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        10 => any::<char>().prop_map(Op::Put),
        3 => (count(), count()).prop_map(|(r, c)| Op::Goto(r, c)),
        2 => count().prop_map(Op::Up),
        2 => count().prop_map(Op::Down),
        2 => count().prop_map(Op::Left),
        2 => count().prop_map(Op::Right),
        3 => Just(Op::Cr),
        5 => Just(Op::Lf),
        2 => Just(Op::Ri),
        1 => Just(Op::Save),
        1 => Just(Op::Restore),
        1 => Just(Op::SetTab),
        1 => Just(Op::ClearTab),
        1 => Just(Op::ClearAllTabs),
        2 => count().prop_map(Op::TabForward),
        1 => count().prop_map(Op::TabBackward),
        2 => (count(), count()).prop_map(|(t, b)| Op::SetRegion(t, b)),
        1 => Just(Op::ResetRegion),
        2 => count().prop_map(Op::ScrollUp),
        2 => count().prop_map(Op::ScrollDown),
        2 => count().prop_map(Op::InsertLines),
        2 => count().prop_map(Op::DeleteLines),
        2 => count().prop_map(Op::InsertChars),
        2 => count().prop_map(Op::DeleteChars),
        2 => count().prop_map(Op::EraseChars),
        2 => prop_oneof![
            Just(EraseLine::ToEnd),
            Just(EraseLine::ToStart),
            Just(EraseLine::All)
        ]
        .prop_map(Op::EraseLine),
        2 => prop_oneof![
            Just(EraseDisplay::Below),
            Just(EraseDisplay::Above),
            Just(EraseDisplay::All),
            Just(EraseDisplay::Scrollback)
        ]
        .prop_map(Op::EraseDisplay),
        1 => any::<u16>().prop_map(Op::Pen),
        1 => Just(Op::Reset),
    ]
}

fn apply(grid: &mut Grid, op: &Op) {
    match *op {
        Op::Put(ch) => grid.put_char(ch),
        Op::Goto(r, c) => grid.goto(r, c),
        Op::Up(n) => grid.cursor_up(n),
        Op::Down(n) => grid.cursor_down(n),
        Op::Left(n) => grid.cursor_left(n),
        Op::Right(n) => grid.cursor_right(n),
        Op::Cr => grid.carriage_return(),
        Op::Lf => grid.line_feed(),
        Op::Ri => grid.reverse_index(),
        Op::Save => grid.save_cursor(),
        Op::Restore => grid.restore_cursor(),
        Op::SetTab => grid.set_tab_stop(),
        Op::ClearTab => grid.clear_tab_stop(),
        Op::ClearAllTabs => grid.clear_all_tab_stops(),
        Op::TabForward(n) => grid.tab_forward(n),
        Op::TabBackward(n) => grid.tab_backward(n),
        Op::SetRegion(t, b) => grid.set_scroll_region(t, b),
        Op::ResetRegion => grid.reset_scroll_region(),
        Op::ScrollUp(n) => grid.scroll_up(n),
        Op::ScrollDown(n) => grid.scroll_down(n),
        Op::InsertLines(n) => grid.insert_lines(n),
        Op::DeleteLines(n) => grid.delete_lines(n),
        Op::InsertChars(n) => grid.insert_chars(n),
        Op::DeleteChars(n) => grid.delete_chars(n),
        Op::EraseChars(n) => grid.erase_chars(n),
        Op::EraseLine(mode) => grid.erase_in_line(mode),
        Op::EraseDisplay(mode) => grid.erase_in_display(mode),
        Op::Pen(flags) => grid.set_pen(Style {
            flags,
            ..Style::DEFAULT
        }),
        Op::Reset => grid.reset(),
    }
}

fn check_invariants(grid: &Grid, rows: u16, cols: u16, max_scrollback: usize) {
    assert_eq!(grid.rows(), rows);
    assert_eq!(grid.cols(), cols);
    assert_eq!(grid.visible_lines().len(), usize::from(rows));
    for line in grid.visible_lines() {
        assert_eq!(line.cells.len(), usize::from(cols));
    }
    let cursor = grid.cursor();
    assert!(
        cursor.row < rows,
        "cursor row {} >= rows {rows}",
        cursor.row
    );
    assert!(
        cursor.col < cols,
        "cursor col {} >= cols {cols}",
        cursor.col
    );
    assert!(grid.cell(cursor).is_some());
    let region = grid.scroll_region();
    assert!(region.top <= region.bottom);
    assert!(region.bottom < rows);
    assert!(grid.scrollback_len() <= max_scrollback);
    for i in 0..grid.scrollback_len() {
        assert_eq!(
            grid.scrollback_line(i).unwrap().cells.len(),
            usize::from(cols)
        );
    }
    assert_eq!(grid.tabs().cols(), cols);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    #[test]
    fn grid_stays_in_bounds(
        rows in 0..=24u16,
        cols in 0..=40u16,
        max_scrollback in 0..=8usize,
        ops in prop::collection::vec(op(), 0..200),
    ) {
        let mut grid = Grid::with_scrollback(rows, cols, max_scrollback);
        let (rows, cols) = (grid.rows(), grid.cols());
        check_invariants(&grid, rows, cols, max_scrollback);
        for op in &ops {
            apply(&mut grid, op);
            check_invariants(&grid, rows, cols, max_scrollback);
        }
    }

    #[test]
    fn restore_returns_to_saved_position(
        rows in 1..=24u16,
        cols in 1..=40u16,
        save_at in (0..=30u16, 0..=50u16),
        ops in prop::collection::vec(op(), 0..50),
    ) {
        let mut grid = Grid::new(rows, cols);
        grid.goto(save_at.0, save_at.1);
        let saved = grid.cursor();
        grid.save_cursor();
        for op in ops.iter().filter(|op| !matches!(op, Op::Save | Op::Reset)) {
            apply(&mut grid, op);
        }
        grid.restore_cursor();
        prop_assert_eq!(grid.cursor(), saved);
    }
}
