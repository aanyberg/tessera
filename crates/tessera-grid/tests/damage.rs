//! Property test for damage tracking (ADR 0016): after any sequence of
//! operations, every visible row whose cells, cluster text or wrap flag
//! differ from the previous snapshot, and the rows the cursor left and
//! entered, are inside the reported damage. False positives are allowed;
//! false negatives would leave stale cells on screen.
//!
//! Two readers are checked side by side: an owner calling
//! `Grid::take_damage` and a `&Grid` reader using `Grid::damage_since`.

use proptest::prelude::*;
use tessera_grid::{Color, DamageRows, EraseDisplay, EraseLine, Grid, Line, Point, Style};

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
    ResetSaved,
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
    Pen(u8),
    PadLastColumn,
    EnterAlt,
    LeaveAlt,
    Resize(u16, u16),
    LineMutWrap(u16),
    Reset,
    /// A checkpoint: compare with the previous snapshot.
    Check,
    /// Fill row `.0` with text, put the cursor at column `.1`, checkpoint,
    /// apply the edit alone, checkpoint: an edit whose recording call is
    /// missing cannot hide behind damage another operation recorded.
    Isolated(u16, u16, Box<Op>),
}

fn count() -> impl Strategy<Value = u16> {
    prop_oneof![4 => 0..=6u16, 1 => 0..=40u16]
}

/// Characters that exercise every write path: plain, wide, combining
/// marks, VS15/VS16, ZWJ sequences, skin tones, regional indicators and
/// controls.
fn ch() -> impl Strategy<Value = char> {
    prop_oneof![
        6 => prop::sample::select(vec!['a', 'b', 'x', ' ', '-', '>', 'é']),
        2 => prop::sample::select(vec!['中', '文', '😀', '👍', '👨', '❤', '⌚']),
        2 => prop::sample::select(vec![
            '\u{301}', '\u{308}', '\u{FE0E}', '\u{FE0F}', '\u{200D}', '\u{1F3FD}',
            '\u{1F1F8}', '\u{1F1EA}', '\u{0600}',
        ]),
        1 => any::<char>(),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => edit(),
        2 => (count(), count(), edit()).prop_map(|(r, c, e)| Op::Isolated(r, c, Box::new(e))),
        2 => Just(Op::Check),
    ]
}

/// Every operation that may change the screen.
fn edit() -> impl Strategy<Value = Op> {
    prop_oneof![
        14 => ch().prop_map(Op::Put),
        3 => (count(), count()).prop_map(|(r, c)| Op::Goto(r, c)),
        1 => count().prop_map(Op::Up),
        1 => count().prop_map(Op::Down),
        1 => count().prop_map(Op::Left),
        1 => count().prop_map(Op::Right),
        2 => Just(Op::Cr),
        4 => Just(Op::Lf),
        2 => Just(Op::Ri),
        1 => Just(Op::Save),
        1 => Just(Op::Restore),
        1 => Just(Op::ResetSaved),
        1 => count().prop_map(Op::TabForward),
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
        2 => any::<u8>().prop_map(Op::Pen),
        1 => Just(Op::PadLastColumn),
        1 => Just(Op::EnterAlt),
        1 => Just(Op::LeaveAlt),
        1 => (1..=8u16, 1..=12u16).prop_map(|(r, c)| Op::Resize(r, c)),
        1 => count().prop_map(Op::LineMutWrap),
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
        Op::ResetSaved => grid.reset_saved_cursor(),
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
        Op::Pen(i) => grid.set_pen(Style {
            bg: Color::Indexed(i % 4),
            ..Style::DEFAULT
        }),
        Op::PadLastColumn => grid.pad_last_column(),
        Op::EnterAlt => grid.enter_alt_screen(),
        Op::LeaveAlt => grid.leave_alt_screen(),
        Op::Resize(r, c) => grid.resize(r, c),
        Op::LineMutWrap(row) => {
            if let Some(line) = grid.line_mut(row) {
                line.wrapped = !line.wrapped;
            }
        }
        Op::Reset => grid.reset(),
        Op::Check | Op::Isolated(..) => {}
    }
}

/// What a frame shows: the visible lines (cells, cluster text, wrap flag)
/// and the cursor.
#[derive(Debug, Clone, PartialEq)]
struct Screen {
    lines: Vec<Line>,
    cursor: Point,
}

fn screen(grid: &Grid) -> Screen {
    Screen {
        lines: grid.visible_lines().to_vec(),
        cursor: grid.cursor(),
    }
}

/// The rows that must be reported between `before` and `after`.
fn changed_rows(before: &Screen, after: &Screen) -> Option<Vec<u16>> {
    if before.lines.len() != after.lines.len()
        || before.lines.first().map(|l| l.cells.len()) != after.lines.first().map(|l| l.cells.len())
    {
        // A resize: only `All` will do.
        return None;
    }
    let mut rows: Vec<u16> = before
        .lines
        .iter()
        .zip(&after.lines)
        .enumerate()
        .filter(|(_, (a, b))| {
            a.cells != b.cells || a.graphemes != b.graphemes || a.wrapped != b.wrapped
        })
        .map(|(i, _)| u16::try_from(i).expect("row fits"))
        .collect();
    if before.cursor != after.cursor {
        rows.push(before.cursor.row);
        rows.push(after.cursor.row);
    }
    Some(rows)
}

fn covers(damage: &DamageRows, rows: Option<&[u16]>) -> Result<(), String> {
    match rows {
        None if *damage == DamageRows::All => Ok(()),
        None => Err(format!("the size changed but damage is {damage:?}")),
        Some(rows) => match rows.iter().find(|&&row| !damage.contains(row)) {
            Some(row) => Err(format!("row {row} changed but damage is {damage:?}")),
            None => Ok(()),
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 1024,
        ..ProptestConfig::default()
    })]

    #[test]
    fn every_changed_row_is_damaged(
        rows in 1..=8u16,
        cols in 1..=12u16,
        ops in prop::collection::vec(op(), 0..120),
    ) {
        let mut grid = Grid::with_scrollback(rows, cols, 4);
        let first = grid.take_damage();
        prop_assert_eq!(first.rows, DamageRows::All, "a new grid is all damaged");
        let mut readers = Readers::new(&grid);
        for op in &ops {
            match op {
                Op::Check => readers.check(&mut grid)?,
                Op::Isolated(row, col, edit) => {
                    let filler: String = ('a'..='z').cycle().take(usize::from(grid.cols())).collect();
                    grid.set_pen(Style::DEFAULT);
                    grid.goto(*row, 0);
                    grid.put_str(&filler);
                    grid.goto(*row, *col);
                    readers.check(&mut grid)?;
                    apply(&mut grid, edit);
                    readers.check(&mut grid)
                        .map_err(|e| TestCaseError::fail(format!("after {edit:?}: {e}")))?;
                }
                _ => apply(&mut grid, op),
            }
        }
    }
}

/// The two kinds of reader: a `&Grid` reader with `damage_since` and an
/// owner that takes the damage at every checkpoint.
struct Readers {
    generation: u64,
    seen: Screen,
}

impl Readers {
    fn new(grid: &Grid) -> Self {
        Self {
            generation: grid.damage_generation(),
            seen: screen(grid),
        }
    }

    /// Checks both readers against the change since the last checkpoint.
    fn check(&mut self, grid: &mut Grid) -> Result<(), TestCaseError> {
        let now = screen(grid);
        let expected = changed_rows(&self.seen, &now);
        let since = grid.damage_since(self.generation);
        covers(&since, expected.as_deref())
            .map_err(|e| TestCaseError::fail(format!("damage_since: {e}")))?;
        // Taking also folds the sticky cursor-row flag, so the next
        // checkpoint sees only what happened after this one.
        let taken = grid.take_damage();
        covers(&taken.rows, expected.as_deref())
            .map_err(|e| TestCaseError::fail(format!("take_damage: {e}")))?;
        self.generation = taken.generation;
        self.seen = now;
        Ok(())
    }
}

#[test]
fn an_idle_grid_reports_nothing() {
    let mut grid = Grid::new(4, 10);
    grid.put_str("hello");
    grid.take_damage();
    assert_eq!(grid.damage().rows, DamageRows::None);
    let generation = grid.damage().generation;
    grid.put_str("!");
    assert_eq!(grid.damage().rows, DamageRows::Rows(0..=0));
    // The `&Grid` reader sees the cursor row until the cursor leaves it.
    assert_eq!(grid.damage_since(generation), DamageRows::Rows(0..=0));
    grid.carriage_return();
    grid.line_feed();
    let generation = grid.damage().generation;
    grid.take_damage();
    assert_eq!(grid.damage_since(generation), DamageRows::Rows(1..=1));
}

#[test]
fn a_full_screen_scroll_is_all_and_a_region_scroll_is_its_rows() {
    let mut grid = Grid::new(6, 4);
    grid.take_damage();
    grid.scroll_up(1);
    assert_eq!(grid.take_damage().rows, DamageRows::All);
    grid.set_scroll_region(1, 3);
    grid.scroll_up(1);
    assert_eq!(grid.take_damage().rows, DamageRows::Rows(1..=3));
    grid.enter_alt_screen();
    assert_eq!(grid.take_damage().rows, DamageRows::All);
    grid.resize(3, 4);
    assert_eq!(grid.take_damage().rows, DamageRows::All);
    grid.resize(3, 4);
    assert_eq!(grid.take_damage().rows, DamageRows::None, "a no-op resize");
}

#[test]
fn a_reset_keeps_the_generations_of_the_old_screen() {
    let mut grid = Grid::new(2, 4);
    grid.put_str("ab");
    let generation = grid.damage().generation;
    grid.reset();
    assert_eq!(grid.damage_since(generation), DamageRows::All);
    assert!(grid.damage().generation > generation);
}

/// DECSTR's saved-cursor reset changes no visible row and no cursor, so
/// it records nothing; the restore after it moves the cursor and does.
#[test]
fn resetting_the_saved_cursor_is_not_damage() {
    let mut grid = Grid::new(4, 10);
    grid.goto(2, 3);
    grid.save_cursor();
    grid.take_damage();
    let generation = grid.damage().generation;
    grid.reset_saved_cursor();
    assert_eq!(grid.take_damage().rows, DamageRows::None);
    assert_eq!(grid.damage_since(generation), DamageRows::None);
    grid.restore_cursor();
    assert_eq!(grid.take_damage().rows, DamageRows::Rows(0..=2));
}
