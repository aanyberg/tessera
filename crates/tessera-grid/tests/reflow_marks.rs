//! Prompt marks through a re-wrap (D-MARKPOS and D-DUP, ADR 0022; #181).
//!
//! A mark stays with the text of the row it was set on: after a resize it
//! is on the row holding that source row's first cell, and no row ever
//! holds two marks of one kind (a same-kind collision keeps the later
//! mark, as [`Line::set_mark`] does).

use std::collections::HashMap;
use std::mem::discriminant;

use proptest::prelude::*;
use tessera_grid::{Grid, Line, Mark, char_width};

/// Sets `mark` on the cursor's line, as OSC 133 does (D-PENDING).
fn mark(grid: &mut Grid, mark: Mark) {
    let row = grid.cursor().row;
    grid.line_mut(row).expect("cursor row").set_mark(mark);
}

fn end(code: i32) -> Mark {
    Mark::CommandEnd {
        exit_code: Some(code),
    }
}

/// Every line of the buffer, oldest scrollback line first.
fn lines(grid: &Grid) -> Vec<&Line> {
    grid.logical_lines().collect()
}

/// Panics unless every line holds at most one mark of each kind.
fn assert_one_per_kind(grid: &Grid, context: &str) {
    for (i, line) in grid.logical_lines().enumerate() {
        for (j, a) in line.marks.iter().enumerate() {
            assert!(
                line.marks[j + 1..]
                    .iter()
                    .all(|b| discriminant(a) != discriminant(b)),
                "{context}: line {i} holds {:?}",
                line.marks
            );
        }
    }
}

/// The issue's repro (#181 item 1): the prompt's second row, which joins
/// the first at 20 columns, carries a second `PromptStart`.
#[test]
fn a_resize_never_puts_two_marks_of_one_kind_on_a_line() {
    let mut grid = Grid::new(5, 10);
    mark(&mut grid, Mark::PromptStart);
    grid.put_str("0123456789abc");
    mark(&mut grid, Mark::PromptStart);
    mark(&mut grid, end(1));
    grid.carriage_return();
    grid.line_feed();
    mark(&mut grid, end(0));

    grid.resize(5, 20);

    assert_one_per_kind(&grid, "10 -> 20");
    let rows = lines(&grid);
    assert_eq!(rows[0].text(), "0123456789abc");
    // The later row's PromptStart replaces the earlier one.
    assert_eq!(rows[0].marks, vec![Mark::PromptStart, end(1)]);
    assert_eq!(rows[1].marks, vec![end(0)]);
    grid.check_invariants();
}

/// zsh's `PROMPT_SP` flow (#181 item 2) at 80 columns: a command on row 0,
/// its output (100 x, no trailing newline) from row 1, then a prompt on a
/// soft-wrapped continuation of the output's last row (row 3).
/// `CommandEnd` is on the output's last row, where precmd's `133;D` lands,
/// and `OutputStart` on its first.
fn prompt_sp() -> Grid {
    let mut grid = Grid::with_scrollback(10, 80, 1000);
    // The command ran on row 0; its output starts on row 1.
    mark(&mut grid, Mark::PromptStart);
    grid.put_str("$ ");
    mark(&mut grid, Mark::CommandStart);
    grid.put_str("printf x");
    grid.carriage_return();
    grid.line_feed();
    mark(&mut grid, Mark::OutputStart);
    grid.put_str(&"x".repeat(100));
    // precmd: D on the cursor's row, the output's last.
    mark(&mut grid, end(0));
    // PROMPT_SP: an inverse `%`, then spaces to the edge and one more,
    // which wraps; CR goes back to the start of the continuation row.
    grid.put_char('%');
    grid.put_str(&" ".repeat(80 - 20 - 1 + 1));
    grid.carriage_return();
    mark(&mut grid, Mark::PromptStart);
    grid.put_str("% ");
    mark(&mut grid, Mark::CommandStart);
    assert!(grid.line(1).expect("row 1").wrapped, "the output wraps");
    assert!(grid.line(2).expect("row 2").wrapped, "PROMPT_SP wraps");
    assert_eq!(grid.cursor().row, 3);
    grid
}

/// Each row of [`prompt_sp`] keeps its marks through 80 -> 40 -> 80
/// columns: the prompt row keeps `PromptStart` and `CommandStart`, the
/// output's last row `CommandEnd`.
#[test]
fn prompt_sp_marks_stay_on_the_prompt_row() {
    let mut grid = prompt_sp();

    grid.resize(10, 40);
    assert_one_per_kind(&grid, "80 -> 40");
    // At 40 columns the 100 x span rows 1..=3, the `%` row's spaces
    // continue on row 4, and the prompt starts row 5 (its first cell, at
    // offset 160 of the line, starts a row at 40 columns).
    let rows = lines(&grid);
    let marks: Vec<_> = rows.iter().map(|line| line.marks.clone()).collect();
    assert_eq!(marks[0], vec![Mark::PromptStart, Mark::CommandStart]);
    assert_eq!(marks[1], vec![Mark::OutputStart]);
    assert_eq!(rows[3].text(), format!("{}%", "x".repeat(20)));
    assert_eq!(marks[3], vec![end(0)], "the output's last row");
    assert_eq!(rows[5].text(), "%", "the prompt row");
    assert_eq!(marks[5], vec![Mark::PromptStart, Mark::CommandStart]);
    for i in [2, 4] {
        assert!(marks[i].is_empty(), "row {i}: {:?}", marks[i]);
    }

    grid.resize(10, 80);
    assert_one_per_kind(&grid, "40 -> 80");
    let rows = lines(&grid);
    let marks: Vec<_> = rows.iter().map(|line| line.marks.clone()).collect();
    assert_eq!(marks[0], vec![Mark::PromptStart, Mark::CommandStart]);
    assert_eq!(marks[1], vec![Mark::OutputStart]);
    assert_eq!(marks[2], vec![end(0)]);
    assert_eq!(rows[3].text(), "%", "the prompt row");
    assert_eq!(marks[3], vec![Mark::PromptStart, Mark::CommandStart]);
    grid.check_invariants();
}

/// A line's text, wrap flag and marks.
type LineState = (String, bool, Vec<Mark>);

/// Every line's text, wrap flag and marks, and the cursor.
fn state(grid: &Grid) -> (Vec<LineState>, (u16, u16)) {
    let lines = grid
        .logical_lines()
        .map(|line| (line.text(), line.wrapped, line.marks.clone()))
        .collect();
    (lines, (grid.cursor().row, grid.cursor().col))
}

/// D-MARKPOS undoes itself (#181 round 1, E1): narrowing to a width that
/// does not divide the old one and widening back restores every row's
/// marks exactly, also through a window drag. A mark keeps its place in
/// its line's text, not just its row.
#[test]
fn narrowing_and_widening_back_restores_the_marks() {
    let drag: Vec<u16> = vec![79, 78, 77, 76, 75, 70, 65, 60, 61, 70, 75, 80];
    for path in [vec![37, 80], vec![60, 80], vec![41, 80], vec![1, 80], drag] {
        let mut grid = prompt_sp();
        let before = state(&grid);
        for &cols in &path {
            grid.resize(10, cols);
            assert_one_per_kind(&grid, &format!("{path:?} at {cols}"));
            grid.check_invariants();
        }
        if path.contains(&1) {
            // One column loses the cursor's distance past the text (it
            // stops on the last column there); the rows come back.
            assert_eq!(state(&grid).0, before.0, "{path:?}");
        } else {
            assert_eq!(state(&grid), before, "{path:?}");
        }
    }
}

/// At a width that does not divide the old one, a mark still sits on the
/// row holding its own cell: the prompt row's marks on the row the prompt
/// text starts, `CommandEnd` on the row holding the output's 81st x.
#[test]
fn at_37_columns_each_mark_is_on_the_row_of_its_cell() {
    let mut grid = prompt_sp();
    grid.resize(10, 37);
    let rows = lines(&grid);
    // The output line from row 1: offsets 0, 80 (the 81st x) and 160 (the
    // prompt) fall on rows 1 + 0, 1 + 2 and 1 + 4 at 37 columns.
    let marks: Vec<_> = rows.iter().map(|line| line.marks.clone()).collect();
    assert_eq!(marks[1], vec![Mark::OutputStart]);
    assert_eq!(marks[3], vec![end(0)]);
    assert_eq!(marks[5], vec![Mark::PromptStart, Mark::CommandStart]);
    // Row 5 starts at offset 148: the prompt's `%` is its 13th cell.
    assert_eq!(rows[5].cells[12].ch, '%', "{:?}", rows[5].text());
    for i in [2, 4] {
        assert!(marks[i].is_empty(), "row {i}: {:?}", marks[i]);
    }
}

/// zsh with an empty `PS1` after `PROMPT_SP`: the prompt's marks sit on a
/// blank continuation row, past the end of the line's text. They go to the
/// line's last row (the cursor's), not its first: for plain text and for
/// text with a wide character (the other split path).
#[test]
fn marks_past_the_text_go_to_the_lines_last_row() {
    for output in ["x".repeat(100), format!("中{}", "x".repeat(98))] {
        let mut grid = Grid::with_scrollback(10, 80, 1000);
        mark(&mut grid, Mark::OutputStart);
        grid.put_str(&output);
        mark(&mut grid, end(0));
        grid.put_char('%');
        grid.put_str(&" ".repeat(60));
        grid.carriage_return();
        mark(&mut grid, Mark::PromptStart);
        mark(&mut grid, Mark::CommandStart);
        assert_eq!(grid.cursor().row, 2);

        grid.resize(10, 40);
        let rows = lines(&grid);
        // 101 cells of text: rows 0..=2 at 40 columns; the blank prompt
        // row is trimmed and its marks join the last row.
        assert_eq!(rows[2].text(), format!("{}%", "x".repeat(20)));
        assert_eq!(rows[0].marks, vec![Mark::OutputStart]);
        assert_eq!(
            rows[2].marks,
            vec![end(0), Mark::PromptStart, Mark::CommandStart],
            "{output}"
        );
        assert!(rows[1].marks.is_empty(), "{:?}", rows[1].marks);
        assert_eq!(grid.cursor().row, 2, "the cursor's row");
        assert_one_per_kind(&grid, "80 -> 40");
    }
}

/// The old top screen row continues a line from history but holds only
/// its trailing blanks, on a screen not anchored at the bottom: a cleared
/// screen grown by a row takes back the blank continuation row readline
/// left behind, and the shell redraws its prompt from home. That row stays
/// a blank row of its own at the top and keeps the marks set on it.
#[test]
fn a_blank_top_row_continuing_history_keeps_its_marks() {
    let mut grid = Grid::with_scrollback(3, 10, 100);
    // 21 characters, then the last one erased: rows "y" x 10 (wrapped),
    // "y" x 10 (wrapped) and an empty continuation row.
    grid.put_str(&"y".repeat(21));
    grid.carriage_return();
    grid.erase_in_line(tessera_grid::EraseLine::ToEnd);
    for _ in 0..3 {
        grid.line_feed();
    }
    grid.erase_in_display(tessera_grid::EraseDisplay::All);
    assert!(!grid.scrolled_since_clear());
    grid.resize(4, 10);
    let newest = grid.scrollback_len() - 1;
    assert!(grid.scrollback_line(newest).expect("newest").wrapped);
    assert_eq!(grid.line(0).expect("row 0").text(), "");
    grid.goto(0, 0);
    mark(&mut grid, Mark::PromptStart);

    grid.resize(4, 7);
    assert_eq!(
        grid.line(0).expect("row 0").marks,
        vec![Mark::PromptStart],
        "{:?}",
        state(&grid)
    );
    for i in 0..grid.scrollback_len() {
        let line = grid.scrollback_line(i).expect("history");
        assert!(line.marks.is_empty(), "history {i}: {:?}", line.marks);
    }
    grid.check_invariants();
}

/// One column drops the wide characters that start a line: the mark's
/// column counts from the first kept cell, so 2 -> 1 -> 2 columns puts the
/// mark back on the row it was on (#181 round 2, B1).
#[test]
fn a_one_column_grid_keeps_a_mark_after_dropped_wide_characters() {
    let mut grid = Grid::with_scrollback(1, 2, 10_000);
    for _ in 0..6 {
        grid.put_char('中');
    }
    grid.put_char('a');
    mark(&mut grid, Mark::PromptStart);
    grid.put_str("bcdefghi");
    let rows = lines(&grid);
    let marked: Vec<_> = rows.iter().filter(|line| !line.marks.is_empty()).collect();
    assert_eq!(marked.len(), 1);
    assert_eq!(marked[0].text(), "ab");

    grid.resize(1, 1);
    let rows = lines(&grid);
    assert_eq!(rows[0].text(), "a", "the wide characters are dropped");
    assert_eq!(rows[0].marks, vec![Mark::PromptStart]);

    grid.resize(1, 2);
    for line in lines(&grid) {
        let want = if line.text() == "ab" {
            vec![Mark::PromptStart]
        } else {
            Vec::new()
        };
        assert_eq!(line.marks, want, "{:?}", line.text());
    }
    grid.check_invariants();
}

/// A wide character that does not fit leaves padding and starts the next
/// row; a mark on the row it starts at the old width follows it.
#[test]
fn a_mark_follows_its_row_across_wide_characters() {
    let mut grid = Grid::with_scrollback(6, 6, 100);
    grid.put_str("ab中文字");
    mark(&mut grid, Mark::PromptStart);
    grid.put_str("xy");
    // Rows at 6 columns: "ab中文" | "字xy".
    assert_eq!(lines(&grid)[1].text(), "字xy");

    grid.resize(6, 5);
    // "ab中" + pad | "文字x" | "y": the second source row starts at 字.
    let rows = lines(&grid);
    let at = rows
        .iter()
        .position(|line| line.text().contains('字'))
        .expect("字");
    for (i, line) in rows.iter().enumerate() {
        let want = if i == at {
            vec![Mark::PromptStart]
        } else {
            Vec::new()
        };
        assert_eq!(line.marks, want, "row {i} {:?}", line.text());
    }
    grid.check_invariants();
}

/// S5: marks per line are capped by construction at one per kind. A flood
/// of 100,000 marks on one wrapped line, then 50 resizes, leaves at most
/// four marks on any line.
#[test]
fn a_mark_flood_then_resizes_keeps_one_mark_per_kind_per_line() {
    let mut grid = Grid::with_scrollback(8, 10, 200);
    let kinds = |i: i32| match i % 4 {
        0 => Mark::PromptStart,
        1 => Mark::CommandStart,
        2 => Mark::OutputStart,
        _ => end(i),
    };
    for i in 0..100_000 {
        if i % 1000 == 0 {
            // A new row of the same logical line every thousand marks.
            grid.put_str(&"z".repeat(10));
        }
        mark(&mut grid, kinds(i));
    }
    let total: usize = grid.logical_lines().map(|line| line.marks.len()).sum();
    assert!(total <= 4 * grid.total_lines(), "{total}");
    let mut cols = 10u16;
    for step in 0..50u16 {
        cols = 1 + (cols * 7 + step * 3) % 37;
        grid.resize(8, cols);
        for (i, line) in grid.logical_lines().enumerate() {
            assert!(line.marks.len() <= 4, "step {step} line {i}");
        }
        assert_one_per_kind(&grid, &format!("step {step}"));
    }
    grid.check_invariants();
}

/// One step of a random feed.
#[derive(Debug, Clone)]
enum Op {
    /// `n` new characters, every `wide_every`-th one wide.
    Text {
        n: u8,
        wide_every: u8,
    },
    Cr,
    Lf,
    /// Moves the cursor (clamped to the screen).
    Goto {
        row: u16,
        col: u16,
    },
    /// An OSC 133 mark: 0 A, 1 B, 2 C, 3 D.
    Mark(u8),
    Resize {
        rows: u16,
        cols: u16,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (1u8..30, 0u8..5).prop_map(|(n, wide_every)| Op::Text { n, wide_every }),
        2 => Just(Op::Cr),
        2 => Just(Op::Lf),
        1 => (0u16..10, 0u16..30).prop_map(|(row, col)| Op::Goto { row, col }),
        4 => (0u8..4).prop_map(Op::Mark),
        2 => (1u16..9, 1u16..25).prop_map(|(rows, cols)| Op::Resize { rows, cols }),
    ]
}

/// Characters used once each, so a cell's character names it: narrow ones
/// from the Private Use Area, wide ones from CJK.
struct Fresh {
    narrow: u32,
    wide: u32,
}

impl Fresh {
    fn next(&mut self, wide: bool) -> char {
        let (counter, base) = if wide {
            (&mut self.wide, 0x4E00)
        } else {
            (&mut self.narrow, 0xE000)
        };
        let ch = char::from_u32(base + *counter).expect("scalar");
        *counter += 1;
        assert_eq!(char_width(ch), Some(if wide { 2 } else { 1 }), "{ch:?}");
        ch
    }
}

/// The first cell's character of a row, when it is one the feed wrote.
fn first_char(line: &Line) -> Option<char> {
    let cell = line.cells.first()?;
    (cell.ch != ' ' && !cell.is_spacer()).then_some(cell.ch)
}

/// Whether a character is one of [`Fresh`]'s wide ones.
fn is_wide(ch: char) -> bool {
    ('\u{4E00}'..'\u{E000}').contains(&ch)
}

/// Where each character is: its line in the buffer and its column.
fn places(grid: &Grid) -> HashMap<char, (usize, usize)> {
    let mut places = HashMap::new();
    for (i, line) in grid.logical_lines().enumerate() {
        for (col, cell) in line.cells.iter().enumerate() {
            if !cell.is_spacer() {
                places.insert(cell.ch, (i, col));
            }
        }
    }
    places
}

/// Line `line` of the buffer counted from the first line the grid ever
/// had: stable while text scrolls into history and out at the cap, until
/// the next resize (which moves rows between the screen and history
/// without counting them).
fn absolute(grid: &Grid, line: usize) -> i64 {
    let pushed = i64::try_from(grid.scrollback_pushed()).expect("count");
    let history = i64::try_from(grid.scrollback_len()).expect("len");
    pushed - history + i64::try_from(line).expect("line")
}

/// A mark the model follows: the mark, and where it sits in the text, as
/// the character there and its place (absolute line and column) when last
/// seen. `right` is set when the place is the right half (spacer) of the
/// wide character `ch`: a mark keeps its column when a wide character
/// overwrites its place, so it can sit on a spacer.
#[derive(Debug, Clone)]
struct Tracked {
    mark: Mark,
    ch: Option<char>,
    right: bool,
    at: (i64, usize),
}

/// The model of D-MARKPOS: a mark set on a row sits at the row's first
/// cell, and every re-wrap keeps it at the character there, so after a
/// resize it is on the row now holding that character. An edit that
/// overwrites the character keeps the place (the new character names it);
/// a mark whose place holds a blank at a resize cannot be followed and
/// makes that resize's check lenient.
#[derive(Default)]
struct Model {
    tracked: Vec<Tracked>,
}

impl Model {
    /// The character now at `t`'s place, its line in the buffer and
    /// whether the place is that character's right half, if it is a
    /// character the feed wrote (a spacer names its wide character).
    fn resolve(
        grid: &Grid,
        places: &HashMap<char, (usize, usize)>,
        t: &Tracked,
    ) -> Option<(char, usize, bool)> {
        if let Some(&(line, _)) = t.ch.and_then(|ch| places.get(&ch)) {
            return Some((t.ch?, line, t.right));
        }
        let line = usize::try_from(t.at.0 - absolute(grid, 0)).ok()?;
        let row = grid.logical_line(line)?;
        let right = row.cells.get(t.at.1)?.is_spacer();
        let col = if right {
            t.at.1.checked_sub(1)?
        } else {
            t.at.1
        };
        let ch = row.cells.get(col)?.ch;
        (ch != ' ').then_some((ch, line, right))
    }

    /// The line `t` is on: its character's, else its place's.
    fn line_of(grid: &Grid, places: &HashMap<char, (usize, usize)>, t: &Tracked) -> Option<usize> {
        match t.ch.and_then(|ch| places.get(&ch)) {
            Some(&(line, _)) => Some(line),
            None => usize::try_from(t.at.0 - absolute(grid, 0)).ok(),
        }
    }

    /// [`mark`] on the cursor's row, followed from the row's first cell.
    fn set(&mut self, grid: &mut Grid, m: Mark) {
        let row = grid.scrollback_len() + usize::from(grid.cursor().row);
        let places = places(grid);
        // The row's mark of that kind is replaced.
        self.tracked.retain(|t| {
            discriminant(&t.mark) != discriminant(&m)
                || Self::line_of(grid, &places, t) != Some(row)
        });
        let ch = first_char(grid.logical_line(row).expect("cursor row"));
        self.tracked.push(Tracked {
            mark: m.clone(),
            ch,
            right: false,
            at: (absolute(grid, row), 0),
        });
        mark(grid, m);
    }

    /// Resizes `grid` and checks D-DUP and D-MARKPOS: every followed mark
    /// whose character is still in the buffer is on that character's row
    /// (a same-kind collision keeps the mark from the later row), and,
    /// when every mark on the grid is followed, nothing else is there.
    fn resize(&mut self, grid: &mut Grid, rows: u16, cols: u16, context: &str) -> bool {
        let before = places(grid);
        let lines = self::lines(grid);
        let mut anonymous = false;
        let mut sources = Vec::new();
        for t in self.tracked.drain(..) {
            if let Some((ch, line, right)) = Self::resolve(grid, &before, &t) {
                // The previous checks put it there, or it was set there
                // since.
                assert!(
                    lines[line].marks.contains(&t.mark),
                    "{context}: line {line} lost {:?} before the resize: {:?}",
                    t.mark,
                    lines[line].marks
                );
                sources.push((before[&ch], ch, right, t.mark));
            } else {
                // Its row left the buffer, or its place holds a blank.
                let line = usize::try_from(t.at.0 - absolute(grid, 0)).ok();
                anonymous |= line.is_some_and(|line| line < lines.len());
            }
        }
        // Source order: a later mark of a kind replaces an earlier one on
        // the same row.
        sources.sort_by_key(|&(place, _, _, _)| place);
        let on_grid: usize = lines.iter().map(|line| line.marks.len()).sum();
        anonymous |= on_grid != sources.len();

        grid.resize(rows, cols);
        grid.check_invariants();
        assert_one_per_kind(grid, context);
        let after = places(grid);
        let lines = self::lines(grid);
        let mut expected: HashMap<(usize, std::mem::Discriminant<Mark>), (char, bool, Mark)> =
            HashMap::new();
        for (_, ch, right, m) in sources {
            match after.get(&ch) {
                Some(&(row, _)) => {
                    expected.insert((row, discriminant(&m)), (ch, right, m));
                }
                // A wide character a one-column grid dropped gives its
                // marks to whichever row the next kept cell starts.
                None => anonymous |= is_wide(ch) && cols == 1,
            }
        }
        for (&(row, _), (_, _, want)) in &expected {
            let have = &lines[row].marks;
            match have.iter().find(|m| discriminant(*m) == discriminant(want)) {
                None => panic!("{context}: row {row} lost {want:?}, has {have:?}"),
                Some(found) if !anonymous => assert_eq!(found, want, "{context}: row {row}"),
                Some(_) => {}
            }
        }
        if !anonymous {
            // Nothing appears that no followed mark brought.
            let total: usize = lines.iter().map(|line| line.marks.len()).sum();
            assert_eq!(total, expected.len(), "{context}: {lines:?}");
            self.tracked = expected
                .into_values()
                .map(|(ch, right, mark)| Tracked {
                    at: (
                        absolute(grid, after[&ch].0),
                        after[&ch].1 + usize::from(right),
                    ),
                    ch: Some(ch),
                    right,
                    mark,
                })
                .collect();
        }
        // After a lenient check the marks' identities are unknown: follow
        // only marks set from now on (and none while unfollowed marks are
        // on the grid, see `on_grid`).
        !anonymous
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_048))]

    /// D-DUP and D-MARKPOS over random feeds and resizes.
    #[test]
    fn marks_stay_with_their_rows_through_random_resizes(
        start in (1u16..9, 1u16..25),
        cap in prop_oneof![Just(0usize), 1usize..6, Just(10_000)],
        ops in proptest::collection::vec(op(), 1..60),
    ) {
        let mut grid = Grid::with_scrollback(start.0, start.1, cap);
        let mut fresh = Fresh { narrow: 0, wide: 0 };
        let mut model = Model::default();
        let mut code = 0;
        let mut strict = 0u32;
        for (step, op) in ops.iter().enumerate() {
            match *op {
                Op::Text { n, wide_every } => {
                    for i in 0..n {
                        let wide = wide_every != 0 && i % wide_every == 0;
                        grid.put_char(fresh.next(wide));
                    }
                }
                Op::Cr => grid.carriage_return(),
                Op::Lf => grid.line_feed(),
                Op::Goto { row, col } => grid.goto(row, col),
                Op::Mark(kind) => {
                    let m = match kind {
                        0 => Mark::PromptStart,
                        1 => Mark::CommandStart,
                        2 => Mark::OutputStart,
                        _ => {
                            code += 1;
                            end(code)
                        }
                    };
                    model.set(&mut grid, m);
                }
                Op::Resize { rows, cols } => {
                    let context = format!("step {step}: {op:?}");
                    strict += u32::from(model.resize(&mut grid, rows, cols, &context));
                }
            }
        }
        STRICT.fetch_add(strict, std::sync::atomic::Ordering::Relaxed);
        RESIZES.fetch_add(
            u32::try_from(ops.iter().filter(|op| matches!(op, Op::Resize { .. })).count())
                .expect("count"),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

static STRICT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static RESIZES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
