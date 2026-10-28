//! `Grid::resize`: rows moving between screen and scrollback, soft-wrap
//! reflow, the alternate screen, cursor, saved cursor, marks, tab stops and
//! scroll region.
//!
//! Expected behaviour comes from `tests/fixtures/reflow_spec.txt`, which
//! cites Alacritty, `WezTerm`, kitty, xterm and iTerm2 per row and records
//! the recommendations R1-R12 and corrections C1-C7 the milestone lead
//! adopted (ADR 0009). It is checked in three ways:
//!
//! - [`fixture_rows_hold`] runs every row and compares the state after the
//!   setup steps and after every resize with the row's blocks.
//! - One snapshot per `tests/scripts/reflow/*.txt` script (one script per
//!   fixture row, see [`every_fixture_row_has_a_script`]) pins the same
//!   states in review-friendly form.
//! - Property tests check the fixture's properties P1-P7 on random content
//!   and sizes.

mod common;
#[path = "common/reflow.rs"]
mod reflow_spec;

use std::path::{Path, PathBuf};

use common::{Step, assert_invariants, invariant_errors};
use proptest::prelude::*;
use reflow_spec::{Op, Script, check_row, render_row, render_state, rows};
use tessera_grid::{
    Cell, CellFlags, Color, EraseDisplay, EraseLine, Grid, Line, Mark, Point, Style,
};

// ------------------------------------------------------------------ fixture

#[test]
fn fixture_has_every_row() {
    let rows = rows();
    assert_eq!(rows.len(), 43, "fixture row count");
    let alternatives: Vec<&str> = rows
        .iter()
        .filter(|r| r.alternative)
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(alternatives, ["B02_wide_char_stranded_blank_as_content"]);
    let mut ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), rows.len(), "row ids are unique");
}

#[test]
fn fixture_rows_hold() {
    let mut errors = Vec::new();
    for row in rows().iter().filter(|r| !r.alternative) {
        errors.extend(check_row(row));
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// An `ALTERNATIVE` row documents a reading the lead rejected: it must
/// *not* hold, and the recommended row it contrasts with must.
#[test]
fn alternative_rows_do_not_hold() {
    for row in rows().iter().filter(|r| r.alternative) {
        assert!(
            !check_row(row).is_empty(),
            "{}: the rejected alternative holds",
            row.id
        );
    }
}

#[test]
fn fixture_rows_keep_the_invariants() {
    for row in rows() {
        let script = row.to_script();
        script.run(|op, grid| check_invariants(&format!("{} {op}", row.id), grid));
    }
}

// ------------------------------------------------------------------ scripts

fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/reflow")
}

/// Runs `tests/scripts/reflow/<name>.txt` and snapshots every state it
/// records as `reflow__<name>.snap`.
fn snapshot_script(name: &str) {
    let path = scripts_dir().join(format!("{name}.txt"));
    let script = Script::load(&path);
    let rendered = script.run(|op, grid| check_invariants(&format!("{name} {op}"), grid));
    insta::with_settings!({ input_file => &path, omit_expression => true }, {
        insta::assert_snapshot!(name, rendered);
    });
}

macro_rules! script_tests {
    ($($name:ident),* $(,)?) => {
        /// Every script with a snapshot test, by file stem.
        const SCRIPTS: &[&str] = &[$(stringify!($name)),*];
        $(
            #[test]
            fn $name() {
                snapshot_script(stringify!($name));
            }
        )*
    };
}

script_tests! {
    a01_wrapped_line_split_and_rejoined,
    x01_readline_blank_continuation_row,
    a02_hard_lines_are_never_joined,
    a03_shrink_pushes_history_grow_pulls_back,
    b01_wide_char_lands_on_new_last_column,
    b02_wide_char_stranded_blank_as_content,
    b03_genuine_space_before_wide_char,
    b04_cluster_cell_moves_with_its_text,
    b05_wide_cluster_flag_across_split,
    b06_wide_char_cannot_fit_one_column,
    c01_hard_line_trailing_blanks_dropped_and_repadded,
    c02_wrapped_row_trailing_spaces_are_content,
    c03_blank_hard_rows_are_lines,
    d01_cursor_on_last_cell_of_wrapped_row,
    d02_cursor_on_blank_past_text,
    d03_pending_wrap_armed_on_full_row,
    d04_pending_wrap_with_text_continuing_to_cleared,
    d05_cursor_below_text_rows_follows_blank_rows,
    d06_cursor_on_row_zero_rows_shrink,
    d07_cursor_at_bottom_rows_shrink_pushes_top,
    d08_grow_rows_pulls_fewer_than_requested,
    d09_scrollback_cap_full_rows_shrink_evicts_oldest,
    d10_scrollback_cap_full_columns_shrink_evicts_oldest,
    e01_alt_screen_truncates_primary_reflows,
    e02_alt_screen_shrink_rows_keeps_cursor_row,
    e03_alt_screen_wide_char_cut_by_new_edge,
    e04_leave_alt_after_resize_restores_consistent_primary,
    f01_marks_stay_on_first_row_of_logical_line,
    f02_marks_on_hard_lines_stay_with_their_line,
    b07_wide_char_strands_a_column_on_grow,
    b08_cursor_on_wide_char_spacer_follows_base,
    g01_saved_cursor_clamped,
    g02_saved_cursor_pending_flag_row_only_resize,
    h01_tab_stops_after_resize,
    h02_scroll_region_reset_by_any_real_resize,
    i01_one_column_grid,
    i02_one_row_grid,
    i03_one_by_one_and_zero_clamp,
    j01_same_size_is_a_noop,
    j02_row_only_change_does_not_rewrap,
    k01_both_dimensions_at_once,
    k02_logical_line_spanning_scrollback_and_screen,
    l01_pad_rows_use_default_pen_not_current_pen,
    l02_scrollback_cap_zero,
}

/// Each fixture row has a script with exactly its size, cap and steps, and
/// every script on disk has a snapshot test.
#[test]
fn every_fixture_row_has_a_script() {
    let mut errors = Vec::new();
    for row in rows() {
        let name = row.id.to_ascii_lowercase();
        let path = scripts_dir().join(format!("{name}.txt"));
        if !path.exists() {
            errors.push(format!(
                "{name}: no script; expected:\n{}",
                row.to_script().text(&row)
            ));
            continue;
        }
        if Script::load(&path) != row.to_script() {
            errors.push(format!("{name}: script differs from the fixture row"));
        }
        if !SCRIPTS.contains(&name.as_str()) {
            errors.push(format!("{name}: add it to `script_tests!`"));
        }
    }
    let on_disk = std::fs::read_dir(scripts_dir())
        .expect("tests/scripts/reflow exists")
        .count();
    if on_disk != SCRIPTS.len() {
        errors.push(format!(
            "{on_disk} scripts on disk, {} tested",
            SCRIPTS.len()
        ));
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

// ------------------------------------------------------------- invariants

/// The fixture's P2 and C7: every line of scrollback, screen and stashed
/// primary has `cols` cells and consistent wide pairs and clusters, the
/// cursor is in bounds (on the last column when a wrap is pending), the
/// history respects its cap, both screens have one size and the tab stops
/// cover the width.
fn invariant_messages(grid: &Grid) -> Vec<String> {
    let mut errors = invariant_errors(grid);
    let (rows, cols) = (usize::from(grid.rows()), usize::from(grid.cols()));
    if grid.visible_lines().len() != rows {
        errors.push(format!(
            "{} visible lines, {rows} rows",
            grid.visible_lines().len()
        ));
    }
    for (i, line) in grid.logical_lines().enumerate() {
        if line.cells.len() != cols {
            errors.push(format!(
                "line {i}: {} cells, {cols} columns",
                line.cells.len()
            ));
        }
    }
    if grid.scrollback_len() > grid.max_scrollback() {
        errors.push(format!(
            "scrollback {} over its cap {}",
            grid.scrollback_len(),
            grid.max_scrollback()
        ));
    }
    for (i, line) in grid.logical_lines().enumerate() {
        for (col, cell) in line.cells.iter().enumerate() {
            if cell.is_pad()
                && (col + 1 != line.cells.len() || cell.ch != ' ' || cell.flags != CellFlags::PAD)
            {
                errors.push(format!(
                    "line {i} col {col}: padding that is not a bare last-column blank"
                ));
            }
        }
    }
    if usize::from(grid.tabs().cols()) != cols {
        errors.push(format!("tab stops cover {} columns", grid.tabs().cols()));
    }
    if grid.is_alt_screen() {
        // Leaving debug-asserts equal dimensions; the lines are checked too.
        let mut primary = grid.clone();
        primary.leave_alt_screen();
        for message in invariant_messages(&primary) {
            errors.push(format!("stashed primary: {message}"));
        }
    }
    errors
}

#[track_caller]
fn check_invariants(context: &str, grid: &Grid) {
    let errors = invariant_messages(grid);
    assert!(
        errors.is_empty(),
        "{context}: {}\n{}",
        errors.join("; "),
        render_state(grid)
    );
}

/// Everything observable about a grid, for equality checks: the full
/// state block plus the derived `Debug` (lines with styles, pen, …).
fn full_state(grid: &Grid) -> String {
    format!("{}{grid:?}", render_state(grid))
}

// ------------------------------------------------------- logical content

/// The text of every logical line of scrollback and the primary screen,
/// as a re-wrap sees it: rows joined by `wrapped`, the stranded column a
/// wide character left at the end of a wrapped row skipped, trailing
/// default blanks (`Cell::BLANK`) trimmed; a space with any style is text.
/// Trailing empty lines are dropped (a resize adds and removes blank rows
/// at the bottom).
fn logical_texts(grid: &Grid) -> Vec<String> {
    let mut texts: Vec<String> = logical_units(grid)
        .into_iter()
        .map(|mut units| {
            while units.last().is_some_and(|(_, blank)| *blank) {
                units.pop();
            }
            units.into_iter().map(|(text, _)| text).collect()
        })
        .collect();
    while texts.last().is_some_and(String::is_empty) {
        texts.pop();
    }
    texts
}

/// Each logical line as its glyphs (one per non-spacer cell), each with
/// whether it is a default blank.
fn logical_units(grid: &Grid) -> Vec<Vec<(String, bool)>> {
    let lines: Vec<&Line> = grid.logical_lines().collect();
    let mut out = Vec::new();
    let mut current = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        current.extend(row_units(line, lines.get(i + 1).copied()));
        if !line.wrapped || i + 1 == lines.len() {
            out.push(std::mem::take(&mut current));
        }
    }
    out
}

fn row_units(line: &Line, next: Option<&Line>) -> Vec<(String, bool)> {
    let stranded = line.wrapped && next.is_some() && line.cells.last().is_some_and(Cell::is_pad);
    let len = line.cells.len() - usize::from(stranded);
    line.cells[..len]
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.is_spacer())
        .map(|(col, c)| {
            let text = match line.cluster(col) {
                Some(text) if c.is_cluster() => text.to_owned(),
                _ => c.ch.to_string(),
            };
            (text, *c == Cell::BLANK)
        })
        .collect()
}

/// The cursor as (logical line index, glyph offset in that line): the
/// index of the glyph under it (a spacer counts as its wide character),
/// plus one while a wrap is pending (the place after that glyph).
fn cursor_position(grid: &Grid) -> (usize, usize) {
    let lines: Vec<&Line> = grid.logical_lines().collect();
    let target = grid.scrollback_len() + usize::from(grid.cursor().row);
    let (mut index, mut offset) = (0, 0);
    for (i, line) in lines.iter().enumerate() {
        if i == target {
            let col = usize::from(grid.cursor().col);
            let glyphs_before = line.cells[..col].iter().filter(|c| !c.is_spacer()).count();
            let glyph = glyphs_before - usize::from(line.cells[col].is_spacer());
            return (index, offset + glyph + usize::from(grid.pending_wrap()));
        }
        offset += row_units(line, lines.get(i + 1).copied()).len();
        if !line.wrapped {
            index += 1;
            offset = 0;
        }
    }
    unreachable!("the cursor is on a visible row")
}

/// Every row of scrollback and the primary screen, without the blank rows
/// at the bottom, as rendered rows (cells, wrapped flag, marks). Which rows
/// count as blank depends on content and marks only: a row change drops an
/// empty row below the cursor even when it continues a wrapped row, and
/// then clears the wrap flag of the row above (lead decision after round
/// 1, spec step 1), so the last row kept is shown without its flag.
fn rows_without_trailing_blanks(grid: &Grid) -> Vec<String> {
    let lines: Vec<&Line> = grid.logical_lines().collect();
    let keep = lines
        .iter()
        .rposition(|line| !line.marks.is_empty() || line.cells.iter().any(|c| *c != Cell::BLANK))
        .map_or(0, |i| i + 1);
    let mut rows: Vec<String> = lines[..keep].iter().map(|l| render_row(l)).collect();
    if let Some(last) = rows.last_mut() {
        *last = last.replacen("]+", "]", 1);
    }
    rows
}

// --------------------------------------------------------------- strategies

/// Text pieces: ASCII, wide characters, combining clusters, a wide flag
/// cluster, an emoji with a skin tone, spaces.
fn piece(wide: bool) -> BoxedStrategy<&'static str> {
    let narrow = prop_oneof![
        6 => prop::sample::select(vec!["a", "b", "c", "x", "y", "z", "0", "9", "-", "~"]),
        2 => Just(" "),
        1 => Just("e\u{301}"),
        1 => Just("o\u{308}\u{301}"),
    ];
    if wide {
        prop_oneof![
            8 => narrow,
            2 => prop::sample::select(vec!["中", "文", "日", "本"]),
            1 => Just("🇯🇵"),
            1 => Just("👍🏽"),
        ]
        .boxed()
    } else {
        narrow.boxed()
    }
}

/// Lines of text written with `put`, CR LF between them. Spaces may sit
/// right before wide characters: only padding (`CellFlags::PAD`) is ever
/// dropped by a re-wrap.
fn text_lines(wide: bool) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(prop::collection::vec(piece(wide), 0..24), 1..6)
        .prop_map(|lines| lines.into_iter().map(|pieces| pieces.concat()).collect())
}

fn write_lines(grid: &mut Grid, lines: &[String]) {
    write_styled_lines(grid, lines, &[]);
}

/// Pens for [`write_styled_lines`]: default, a background, an underline,
/// inverse video, a foreground.
fn pen(index: usize) -> Style {
    match index % 5 {
        0 => Style::DEFAULT,
        1 => Style {
            bg: Color::Indexed(4),
            ..Style::DEFAULT
        },
        2 => Style::DEFAULT.with(Style::UNDERLINE),
        3 => Style::DEFAULT.with(Style::INVERSE),
        _ => Style {
            fg: Color::Indexed(2),
            ..Style::DEFAULT
        },
    }
}

/// Writes `lines` like [`write_lines`], line `i` with pen `pens[i]`
/// (default when missing), so trailing spaces carry that style.
fn write_styled_lines(grid: &mut Grid, lines: &[String], pens: &[usize]) {
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            grid.carriage_return();
            grid.line_feed();
        }
        grid.set_pen(pen(pens.get(i).copied().unwrap_or(0)));
        grid.put_str(line);
    }
    grid.set_pen(Style::DEFAULT);
}

/// A height: mostly small, sometimes zero, large or `u16::MAX`.
fn height() -> impl Strategy<Value = u16> {
    prop_oneof![
        16 => 0..14u16,
        1 => 200..400u16,
        1 => Just(u16::MAX),
    ]
}

/// A size for [`resize_is_total_and_keeps_the_invariants`]. Every line of
/// history is as wide as the grid, so with a 10 000-line history the width
/// stays moderate (`u16::MAX` columns would need ten gigabytes);
/// [`extreme_widths_are_total`] covers the widest grids with a short
/// history. The area of the screen itself stays testable too.
fn size() -> impl Strategy<Value = (u16, u16)> {
    (height(), prop_oneof![8 => 0..14u16, 1 => 100..300u16]).prop_map(|(rows, cols)| {
        if u32::from(rows) * u32::from(cols) > 100_000 {
            (rows, cols.min(3))
        } else {
            (rows, cols)
        }
    })
}

/// Sizes up to `u16::MAX` columns on few rows.
fn wide_size() -> impl Strategy<Value = (u16, u16)> {
    (
        0..4u16,
        prop_oneof![2 => 0..14u16, 1 => 1000..1100u16, 1 => Just(u16::MAX)],
    )
}

/// Any operation, including the alternate screen, marks, regions, erases
/// and resizes to any size.
fn any_op(
    max_row: u16,
    sizes: impl Strategy<Value = (u16, u16)> + 'static,
) -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => text_lines(true).prop_map(|lines| Op::Grid(Step::Put(lines.concat()))),
        2 => Just(Op::Grid(Step::Cr)),
        2 => Just(Op::Grid(Step::Lf)),
        2 => (0..max_row, 0..20u16).prop_map(|(r, c)| Op::Grid(Step::Goto(r, c))),
        1 => Just(Op::EnterAlt),
        1 => Just(Op::LeaveAlt),
        1 => Just(Op::Save),
        1 => (0..max_row).prop_map(|r| Op::Mark(r, Mark::PromptStart)),
        1 => (0..6u16, 0..12u16).prop_map(|(t, b)| Op::Region(t, b)),
        1 => Just(Op::Grid(Step::Ed(EraseDisplay::Below))),
        1 => Just(Op::Grid(Step::El(EraseLine::All))),
        1 => (0..3u16).prop_map(|n| Op::Grid(Step::Ich(n))),
        1 => (0..3u16).prop_map(|n| Op::Grid(Step::Dch(n))),
        1 => (0..3u16).prop_map(|n| Op::Grid(Step::Ech(n))),
        1 => prop::option::of(0..8u8).prop_map(|bg| Op::Grid(Step::PenBg(bg))),
        4 => sizes.prop_map(|(r, c)| Op::Grid(Step::Resize(r, c))),
    ]
}

fn apply_checked(grid: &mut Grid, op: &Op) -> Result<(), TestCaseError> {
    if let Op::Mark(row, _) = op
        && *row >= grid.rows()
    {
        return Ok(());
    }
    let last_row_wrapped_before =
        !grid.is_alt_screen() && grid.visible_lines().last().is_some_and(|line| line.wrapped);
    let size_before = (grid.rows(), grid.cols());
    op.apply(grid);
    if let Op::Grid(Step::Resize(..)) = op
        && (grid.rows(), grid.cols()) != size_before
    {
        let errors = invariant_messages(grid);
        prop_assert!(errors.is_empty(), "{op:?}: {}", errors.join("; "));
        let region = grid.scroll_region();
        prop_assert_eq!((region.top, region.bottom), (0, grid.rows() - 1));
        if !last_row_wrapped_before && !grid.is_alt_screen() {
            prop_assert!(
                !grid.visible_lines().last().is_some_and(|line| line.wrapped),
                "{op:?} left the last row wrapped"
            );
        }
    }
    Ok(())
}

/// Applies `ops`, checking the invariants after every resize and that
/// repeating it, or resizing to the current size, changes nothing.
fn run_total(mut grid: Grid, ops: &[Op]) -> Result<(), TestCaseError> {
    for op in ops {
        apply_checked(&mut grid, op)?;
        if let &Op::Grid(Step::Resize(rows, cols)) = op {
            let before = full_state(&grid);
            grid.resize(rows, cols);
            prop_assert_eq!(&full_state(&grid), &before, "repeating {:?}", op);
            grid.resize(grid.rows(), grid.cols());
            prop_assert_eq!(&full_state(&grid), &before, "same-size resize");
        }
    }
    Ok(())
}

// ------------------------------------------------------------- properties

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// P1, P2, P3, P7: any content on either screen, any cap, any sizes up
    /// to `u16::MAX`: no panic, every invariant holds after every resize,
    /// a repeated resize and a same-size resize change nothing.
    #[test]
    fn resize_is_total_and_keeps_the_invariants(
        start in (1..12u16, 1..16u16),
        cap in prop::sample::select(vec![0usize, 1, 3, 50, 10_000]),
        ops in prop::collection::vec(any_op(12, size()), 1..24),
    ) {
        run_total(Grid::with_scrollback(start.0, start.1, cap), &ops)?;
    }

    /// P4: a column change keeps the text of every logical line (history
    /// large enough, no wide text on a one-column grid), styled trailing
    /// spaces and background-filled rows included.
    #[test]
    fn column_change_conserves_logical_text(
        rows in 1..9u16,
        c0 in 2..14u16,
        c1 in 1..30u16,
        wide_lines in text_lines(true),
        narrow_lines in text_lines(false),
        pens in prop::collection::vec(0..5usize, 0..6),
    ) {
        let lines = if c1 == 1 { narrow_lines } else { wide_lines };
        let mut grid = Grid::new(rows, c0);
        write_styled_lines(&mut grid, &lines, &pens);
        let before = logical_texts(&grid);
        grid.resize(rows, c1);
        assert_invariants("after the column change", &grid);
        prop_assert_eq!(logical_texts(&grid), before);
    }

    /// P5: shrink then grow back, and grow then shrink back, restore the
    /// text of every logical line and the cursor's place in its line (the
    /// cursor at the end of the text; no wide text at one column).
    #[test]
    fn round_trips_restore_text_and_cursor(
        rows in 1..9u16,
        c0 in 2..14u16,
        narrower in 1..13u16,
        wider in 1..20u16,
        wide_lines in text_lines(true),
        narrow_lines in text_lines(false),
    ) {
        let c1 = narrower.min(c0 - 1);
        let c2 = c0 + wider;
        let mut lines = if c1 == 1 { narrow_lines } else { wide_lines };
        // The cursor ends right after the text: trailing blanks of the last
        // line would park it on blanks past the text, whose offset is
        // clamped rather than kept (fixture D02).
        if let Some(last) = lines.last_mut() {
            let trimmed = last.trim_end_matches(' ').len();
            last.truncate(trimmed);
        }
        let mut grid = Grid::new(rows, c0);
        write_lines(&mut grid, &lines);
        let texts = logical_texts(&grid);
        let cursor = cursor_position(&grid);
        for via in [c1, c2] {
            let mut copy = grid.clone();
            copy.resize(rows, via);
            assert_invariants("there", &copy);
            prop_assert_eq!(logical_texts(&copy), texts.clone(), "at {} columns", via);
            prop_assert_eq!(cursor_position(&copy), cursor, "at {} columns", via);
            copy.resize(rows, c0);
            assert_invariants("and back", &copy);
            prop_assert_eq!(logical_texts(&copy), texts.clone(), "back from {}", via);
            prop_assert_eq!(cursor_position(&copy), cursor, "back from {}", via);
        }
    }

    /// P6: a row-only change moves rows between screen and history without
    /// rewriting any (blank rows at the bottom aside), and the cursor stays
    /// on its text unless it had to be clamped to the top row.
    #[test]
    fn row_change_moves_rows_without_rewriting_them(
        rows in 1..12u16,
        cols in 1..14u16,
        new_rows in 1..20u16,
        lines in text_lines(true),
        up in 0..12u16,
    ) {
        let mut grid = Grid::new(rows, cols);
        write_lines(&mut grid, &lines);
        grid.cursor_up(up);
        let before = rows_without_trailing_blanks(&grid);
        let cursor_line = grid.scrollback_len() + usize::from(grid.cursor().row);
        let (col, pending) = (grid.cursor().col, grid.pending_wrap());
        grid.resize(new_rows, cols);
        assert_invariants("after the row change", &grid);
        prop_assert_eq!(rows_without_trailing_blanks(&grid), before);
        prop_assert_eq!((grid.cursor().col, grid.pending_wrap()), (col, pending));
        let now = grid.scrollback_len() + usize::from(grid.cursor().row);
        if grid.cursor().row > 0 {
            prop_assert_eq!(now, cursor_line, "the cursor stays on its row");
        }
    }
}

proptest! {
    // Each case at `u16::MAX` columns renders and compares megabytes of
    // state; a few dozen cases keep the debug test run short.
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// P1, P2, P3, P7 for grids up to `u16::MAX` columns wide, with a
    /// history short enough to fit in memory at that width.
    #[test]
    fn extreme_widths_are_total(
        start in (1..4u16, 1..16u16),
        cap in prop::sample::select(vec![0usize, 1, 3, 20]),
        ops in prop::collection::vec(any_op(4, wide_size()), 1..16),
    ) {
        run_total(Grid::with_scrollback(start.0, start.1, cap), &ops)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// P5 with the cursor on a glyph anywhere in the text (not only at its
    /// end): with rows enough for the text at every width visited nothing
    /// reaches history, so the cursor's place in its line is restored.
    #[test]
    fn round_trips_restore_a_cursor_on_a_glyph(
        c0 in 2..14u16,
        narrower in 2..13u16,
        wider in 1..20u16,
        lines in text_lines(true),
        pick in any::<prop::sample::Index>(),
    ) {
        let rows = 160;
        let c1 = narrower.min(c0 - 1).max(2);
        let c2 = c0 + wider;
        let mut grid = Grid::new(rows, c0);
        write_lines(&mut grid, &lines);
        let glyphs: Vec<(u16, u16)> = (0..rows)
            .flat_map(|r| (0..c0).map(move |c| (r, c)))
            .filter(|&(r, c)| grid.cell(Point::new(r, c)).is_some_and(|cell| *cell != Cell::BLANK))
            .collect();
        prop_assume!(!glyphs.is_empty());
        let (row, col) = glyphs[pick.index(glyphs.len())];
        grid.goto(row, col);
        let texts = logical_texts(&grid);
        let cursor = cursor_position(&grid);
        for via in [c1, c2] {
            if via == c0 {
                continue;
            }
            let mut copy = grid.clone();
            copy.resize(rows, via);
            prop_assert_eq!(copy.scrollback_len(), 0);
            prop_assert_eq!(cursor_position(&copy), cursor, "at {} columns", via);
            copy.resize(rows, c0);
            prop_assert_eq!(logical_texts(&copy), texts.clone(), "back from {}", via);
            prop_assert_eq!(cursor_position(&copy), cursor, "back from {}", via);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Verify cycle 1: text with spaces right before wide characters keeps
    /// every space (and every other glyph) through 80 -> W -> 80 at every
    /// width W from 2 to 79.
    #[test]
    fn every_width_round_trip_keeps_spaces_before_wide_chars(
        lines in prop::collection::vec(
            prop::collection::vec(
                prop_oneof![
                    3 => Just(" "),
                    3 => prop::sample::select(vec!["가", "나", "中", "文", "🇯🇵", "👍🏽"]),
                    2 => prop::sample::select(vec!["a", "b", "e\u{301}"]),
                ],
                1..60,
            ),
            1..6,
        ),
    ) {
        let lines: Vec<String> = lines.into_iter().map(|p| p.concat()).collect();
        let mut grid = Grid::new(24, 80);
        write_lines(&mut grid, &lines);
        let texts = logical_texts(&grid);
        for width in 2..80 {
            let mut copy = grid.clone();
            copy.resize(24, width);
            prop_assert_eq!(&logical_texts(&copy), &texts, "at {} columns", width);
            copy.resize(24, 80);
            prop_assert_eq!(&logical_texts(&copy), &texts, "back from {}", width);
        }
    }
}

// --------------------------------------------------------- direct tests

#[test]
fn same_size_resize_keeps_everything() {
    let mut grid = Grid::new(3, 6);
    grid.put_str("abcdefgh");
    grid.set_scroll_region(1, 2);
    grid.tabs_mut().set(3);
    let before = full_state(&grid);
    grid.resize(3, 6);
    assert_eq!(full_state(&grid), before);
}

#[test]
fn zero_size_clamps_like_new() {
    let mut grid = Grid::new(2, 2);
    grid.resize(0, 0);
    assert_eq!((grid.rows(), grid.cols()), (1, 1));
    grid.resize(0, 5);
    assert_eq!((grid.rows(), grid.cols()), (1, 5));
    check_invariants("0x5", &grid);
}

#[test]
fn extreme_sizes_do_not_panic() {
    let mut grid = Grid::new(3, 4);
    grid.put_str("ab中cd e\u{301}🇯🇵");
    for (rows, cols) in [(u16::MAX, 2), (1, u16::MAX), (2, 1), (u16::MAX, 1), (0, 0)] {
        grid.resize(rows, cols);
        check_invariants(&format!("{rows}x{cols}"), &grid);
    }
}

#[test]
fn grid_reads_stay_consistent_after_resize() {
    let mut grid = Grid::new(2, 4);
    grid.put_str("abcdefghij");
    grid.resize(2, 6);
    assert_eq!(grid.total_lines(), grid.scrollback_len() + 2);
    assert_eq!(grid.logical_lines().count(), grid.total_lines());
    assert_eq!(grid.cell(Point::new(1, 3)).map(|c| c.ch), Some('j'));
    assert_eq!(grid.cursor(), Point::new(1, 4));
    // Writing continues after the text.
    grid.put_str("kl");
    assert_eq!(grid.line(1).map(Line::text).as_deref(), Some("ghijkl"));
    assert!(grid.pending_wrap());
}

#[test]
fn a_full_history_reflows_and_keeps_its_newest_lines() {
    let mut grid = Grid::with_scrollback(4, 10, 20);
    for n in 0..40 {
        grid.put_str(&format!("line {n:02} xx"));
        grid.carriage_return();
        grid.line_feed();
    }
    assert_eq!(grid.scrollback_len(), 20);
    grid.resize(4, 5);
    check_invariants("10 -> 5", &grid);
    assert_eq!(grid.scrollback_len(), 20, "the cap still holds");
    let screen: Vec<String> = grid.visible_lines().iter().map(Line::text).collect();
    assert_eq!(
        screen,
        ["38 xx", "line", "39 xx", ""],
        "bottom-anchored window"
    );
    grid.resize(4, 10);
    check_invariants("5 -> 10", &grid);
    let texts: Vec<String> = grid.logical_lines().map(Line::text).collect();
    assert!(texts.contains(&"line 39 xx".to_owned()), "{texts:?}");
}

#[test]
fn spacer_cells_never_appear_without_their_wide_cell() {
    let mut grid = Grid::new(3, 7);
    grid.put_str("中文日本語");
    for cols in [1, 2, 3, 4, 5, 9, 2, 7] {
        grid.resize(3, cols);
        check_invariants(&format!("{cols} columns"), &grid);
        for line in grid.logical_lines() {
            if let Some(first) = line.cells.first() {
                assert!(!first.is_spacer(), "{}", render_row(line));
            }
        }
    }
}

/// Review round 1: readline erased a soft-wrapped line's continuation; the
/// empty continuation row is dropped first when rows shrink, so the
/// prompt the cursor is on stays visible (script `x01`).
#[test]
fn an_empty_continuation_row_goes_before_the_prompt() {
    let mut grid = Grid::new(2, 10);
    grid.put_str("$ echo hello");
    grid.erase_in_line(EraseLine::All);
    grid.goto(0, 2);
    assert!(grid.line(0).is_some_and(|l| l.wrapped));
    grid.resize(1, 10);
    assert_eq!(grid.scrollback_len(), 0);
    assert_eq!(grid.line(0).map(Line::text).as_deref(), Some("$ echo hel"));
    assert!(
        !grid.line(0).is_some_and(|l| l.wrapped),
        "nothing continues it"
    );
    assert_eq!(grid.cursor(), Point::new(0, 2));
}

/// Review round 1: a cursor past the text of a row that ends up exactly
/// full waits on the last column with a wrap pending, so the next
/// character starts the next row.
#[test]
fn a_cursor_past_a_full_last_row_arms_pending_wrap() {
    let mut grid = Grid::new(3, 8);
    grid.put_str("abcd");
    grid.goto(0, 6);
    grid.resize(3, 4);
    assert_eq!(grid.cursor(), Point::new(0, 3));
    assert!(grid.pending_wrap());
    grid.put_char('x');
    assert_eq!(grid.line(0).map(Line::text).as_deref(), Some("abcd"));
    assert_eq!(grid.line(1).map(Line::text).as_deref(), Some("x"));
    // A wide character filling the row: the cursor waits on its spacer
    // column, as after `put_wide`, with the wrap pending.
    let mut grid = Grid::new(3, 8);
    grid.put_str("ab中");
    grid.goto(0, 7);
    grid.resize(3, 4);
    assert_eq!(
        (grid.cursor(), grid.pending_wrap()),
        (Point::new(0, 3), true)
    );
}

/// Pins the one place a column shrink cannot keep the cursor on its
/// character: when the rest of its line below it fills the screen, its
/// row goes into history and the cursor stays on row zero, same column,
/// no wrap pending (fixture D06 for rows; the round trip cannot restore
/// it).
#[test]
fn a_cursor_row_pushed_into_history_stays_on_row_zero() {
    let mut grid = Grid::new(2, 4);
    grid.put_str("abcdefgh");
    grid.goto(0, 1);
    grid.resize(2, 2);
    assert_eq!(grid.cursor(), Point::new(0, 1));
    assert!(!grid.pending_wrap());
    let screen: Vec<String> = grid.visible_lines().iter().map(Line::text).collect();
    assert_eq!(screen, ["ef", "gh"]);
    assert_eq!(grid.scrollback_len(), 2);
}

/// Review round 1: trailing spaces written with any style (inverse,
/// underline, a foreground, a background) are text: they survive a shrink
/// and a grow; only default blanks are trimmed.
#[test]
fn styled_trailing_spaces_survive_a_reflow() {
    for index in 1..5 {
        let style = pen(index);
        let mut grid = Grid::new(3, 12);
        grid.put_str("ab");
        grid.set_pen(style);
        grid.put_str("      ");
        grid.set_pen(Style::DEFAULT);
        grid.carriage_return();
        grid.line_feed();
        let styled = |grid: &Grid| {
            grid.logical_lines()
                .flat_map(|line| line.cells.iter())
                .filter(|cell| cell.ch == ' ' && cell.style == style)
                .count()
        };
        assert_eq!(styled(&grid), 6, "pen {index}");
        grid.resize(3, 5);
        check_invariants(&format!("pen {index} at 5"), &grid);
        assert_eq!(styled(&grid), 6, "pen {index} at 5 columns");
        grid.resize(3, 12);
        assert_eq!(styled(&grid), 6, "pen {index} back at 12 columns");
        assert_eq!(
            grid.line(0)
                .map(|l| l.cells[2..8].iter().all(|c| c.style == style)),
            Some(true),
            "pen {index}: the spaces are back in place"
        );
    }
}

/// Review round 2: a one-column re-wrap that drops the wide characters
/// after a blank leaves an empty last row, which the prune removes; the
/// row before it must not stay wrapped, or a later re-wrap would join it
/// with whatever comes below.
#[test]
fn a_pruned_empty_row_does_not_leave_its_line_wrapped() {
    let mut grid = Grid::new(1, 4);
    grid.put_str("a 中");
    grid.goto(0, 0);
    grid.resize(1, 1);
    check_invariants("1x1", &grid);
    assert_eq!(grid.line(0).map(Line::text).as_deref(), Some("a"));
    assert!(
        !grid.line(0).is_some_and(|l| l.wrapped),
        "nothing continues it"
    );
    grid.resize(3, 1);
    check_invariants("3x1", &grid);
    let rows: Vec<(String, bool)> = grid
        .visible_lines()
        .iter()
        .map(|l| (l.text(), l.wrapped))
        .collect();
    assert_eq!(rows[0], ("a".to_owned(), false));
}

/// Review round 2 pins where the blank rows that absorb a narrower width
/// are counted: after the re-wrap, not in the source rows. An empty row
/// that continued the wrapped command line is joined into that line by
/// the re-wrap; counting it in the source would drop a row of real text
/// ("el") instead.
#[test]
fn the_prune_counts_blank_rows_after_the_rewrap() {
    let mut grid = Grid::new(2, 10);
    grid.put_str("$ echo hello");
    grid.erase_in_line(EraseLine::All);
    grid.goto(0, 2);
    grid.resize(2, 4);
    check_invariants("2x4", &grid);
    let texts: Vec<String> = grid.logical_lines().map(Line::text).collect();
    assert_eq!(texts, ["$ ec", "ho h", "el"]);
}

/// A grid whose first row ends in the padding `put_wide` leaves when a
/// wide character wraps: `[abcd_]+ [中~...]`.
fn padded() -> Grid {
    let mut grid = Grid::new(3, 5);
    grid.put_str("abcd中");
    let pad = grid.cell(Point::new(0, 4)).copied();
    assert_eq!(pad.map(|c| (c.ch, c.is_pad())), Some((' ', true)));
    grid
}

fn pads(grid: &Grid) -> usize {
    grid.logical_lines()
        .flat_map(|line| line.cells.iter())
        .filter(|cell| cell.is_pad())
        .count()
}

/// One named edit of a grid.
type GridEdit = fn(&mut Grid);

/// Verify cycle 1: every write or erase of a padding cell clears the
/// marker, so padding never survives as a hidden drop marker on text.
#[test]
fn writing_or_erasing_padding_clears_it() {
    let paths: Vec<(&str, GridEdit)> = vec![
        ("put_char", |g| {
            g.goto(0, 4);
            g.put_char('x');
        }),
        ("put_str space", |g| {
            g.goto(0, 4);
            g.put_str(" ");
        }),
        ("put_cell", |g| {
            g.goto(0, 4);
            g.put_cell(Cell::new(' ', Style::DEFAULT));
        }),
        ("ECH", |g| {
            g.goto(0, 4);
            g.erase_chars(1);
        }),
        ("EL to end", |g| {
            g.goto(0, 2);
            g.erase_in_line(EraseLine::ToEnd);
        }),
        ("EL all", |g| {
            g.goto(0, 0);
            g.erase_in_line(EraseLine::All);
        }),
        ("ED below", |g| {
            g.goto(0, 0);
            g.erase_in_display(EraseDisplay::Below);
        }),
        ("ED all", |g| g.erase_in_display(EraseDisplay::All)),
        ("ICH", |g| {
            g.goto(0, 1);
            g.insert_chars(1);
        }),
        ("DCH", |g| {
            g.goto(0, 1);
            g.delete_chars(1);
        }),
        ("reset", Grid::reset),
    ];
    for (name, path) in paths {
        let mut grid = padded();
        path(&mut grid);
        assert_eq!(pads(&grid), 0, "{name} left padding behind");
        check_invariants(name, &grid);
    }
    // Scrolling moves the padded row whole (it is still padding in
    // history); the rows that enter are blank without it.
    let mut grid = padded();
    grid.scroll_up(3);
    let visible = grid
        .visible_lines()
        .iter()
        .flat_map(|line| line.cells.iter())
        .filter(|cell| cell.is_pad())
        .count();
    assert_eq!((visible, pads(&grid)), (0, 1));
    // A wide character written into the last column again pads it again.
    let mut grid = padded();
    grid.goto(1, 4);
    grid.put_char('文');
    assert_eq!(
        grid.cell(Point::new(1, 4)).map(Cell::is_pad),
        Some(true),
        "a new wrap of a wide character pads its row"
    );
}

/// The padding carries the pen's background like any erased cell, and a
/// reflow drops it whatever its colour.
#[test]
fn padding_takes_the_pen_background_and_is_dropped_by_a_join() {
    let mut grid = Grid::new(3, 5);
    grid.set_pen(pen(1));
    grid.put_str("abcd中");
    let cell = grid.cell(Point::new(0, 4)).copied().unwrap();
    assert!(cell.is_pad());
    assert_eq!(cell.style.bg, Color::Indexed(4));
    assert!(cell.is_blank(), "padding is blank to everyone else");
    grid.resize(3, 8);
    assert_eq!(grid.line(0).map(Line::text).as_deref(), Some("abcd中"));
    assert_eq!(pads(&grid), 0);
}

/// Verify cycle 1: narrowing to one column keeps only what history and
/// the screen can hold while it re-wraps (here a 50-line history), and the
/// result is the same as without the bound.
#[test]
fn narrowing_to_one_column_keeps_history_to_its_cap() {
    let mut grid = Grid::with_scrollback(24, 80, 50);
    for n in 0..200 {
        grid.put_str(&format!("{n:03} {}", "x".repeat(70)));
        grid.carriage_return();
        grid.line_feed();
    }
    grid.resize(24, 1);
    check_invariants("80 -> 1", &grid);
    assert_eq!(grid.scrollback_len(), 50);
    let tail: String = grid.logical_lines().map(|l| l.cells[0].ch).collect();
    assert!(tail.trim_end().ends_with("xxxxx"), "{tail}");
    grid.resize(24, 80);
    check_invariants("1 -> 80", &grid);
}

/// Review round 3 (P6 oracle): the only text is two spaces that wrapped
/// onto a second row of a one-column grid; with the cursor moved up, a
/// shrink to one row drops that empty continuation row.
#[test]
fn a_row_change_may_drop_an_empty_continuation_row() {
    let mut grid = Grid::new(2, 1);
    write_lines(&mut grid, &["  ".to_owned()]);
    grid.cursor_up(1);
    let before = rows_without_trailing_blanks(&grid);
    grid.resize(1, 1);
    check_invariants("2 -> 1 rows", &grid);
    assert_eq!(rows_without_trailing_blanks(&grid), before);
    assert_eq!(grid.scrollback_len(), 0);
    assert!(!grid.line(0).is_some_and(|l| l.wrapped));
}

/// Review round 3: widening the alternate screen turns the padding of its
/// old last column into a plain blank.
#[test]
fn widening_the_alternate_screen_unpads_its_old_last_column() {
    let mut grid = Grid::new(3, 5);
    grid.enter_alt_screen();
    grid.put_str("abcd中");
    assert_eq!(pads(&grid), 1);
    grid.resize(3, 8);
    check_invariants("alt 5 -> 8", &grid);
    assert_eq!(pads(&grid), 0);
    assert_eq!(grid.cell(Point::new(0, 4)), Some(&Cell::BLANK));
}

/// Review round 3: the column a re-wrap strands is padding in the default
/// style, not the pen's or the text's.
#[test]
fn a_rewrap_pads_with_the_default_style() {
    let mut grid = Grid::new(3, 6);
    grid.set_pen(pen(1));
    grid.put_str("abcd中");
    grid.set_pen(pen(3));
    grid.resize(3, 5);
    let cell = grid.cell(Point::new(0, 4)).copied().unwrap();
    assert!(cell.is_pad());
    assert_eq!(
        Cell {
            flags: CellFlags::EMPTY,
            ..cell
        },
        Cell::BLANK
    );
    assert_eq!(cell.flags, CellFlags::PAD);
}

/// Review round 3: narrowing to one column with a small history keeps only
/// a bounded part of the output while it re-wraps; the result must be what
/// an unbounded re-wrap gives, cut to the cap: same screen, cursor and
/// pending wrap, and the history is the newest `cap` lines. Long lines mix
/// plain rows and rows with wide characters (both split paths); short
/// lines at the end hold the cursor (so it stays on screen at one column),
/// blank rows sit below it (so the prune runs), and the line count varies
/// so the last rows dropped while re-wrapping land at every distance from
/// the end.
#[test]
fn a_bounded_rewrap_matches_an_unbounded_one() {
    for lines in 150..210 {
        let blanks = 3 + lines % 5;
        let up = blanks + 1 + lines % 3;
        let mut small = Grid::with_scrollback(24, 80, 50);
        let mut large = Grid::with_scrollback(24, 80, 1_000_000);
        for grid in [&mut small, &mut large] {
            for n in 0..lines {
                if n % 2 == 0 {
                    grid.put_str(&format!("{n:04} plain text of a line"));
                } else {
                    grid.put_str(&format!("{n:04} 中文 wide 日本 text"));
                }
                grid.carriage_return();
                grid.line_feed();
            }
            for tail in ["ab中c", "xyz", "q中"] {
                grid.put_str(tail);
                grid.carriage_return();
                grid.line_feed();
            }
            for _ in 0..blanks {
                grid.line_feed();
            }
            grid.cursor_up(up);
            grid.cursor_right(1);
        }
        let case = format!("{lines} lines, {blanks} blanks, up {up}");
        small.resize(24, 1);
        large.resize(24, 1);
        check_invariants(&case, &small);
        assert_ne!(large.cursor().row, 0, "{case}: the cursor stays on screen");
        let screen = |g: &Grid| g.visible_lines().iter().map(render_row).collect::<Vec<_>>();
        assert_eq!(screen(&small), screen(&large), "{case}: screen");
        assert_eq!(small.cursor(), large.cursor(), "{case}: cursor");
        assert_eq!(
            small.pending_wrap(),
            large.pending_wrap(),
            "{case}: pending wrap"
        );
        assert_eq!(small.scrollback_len(), 50, "{case}");
        let offset = large.scrollback_len() - 50;
        for i in 0..50 {
            assert_eq!(
                small.scrollback_line(i).map(render_row),
                large.scrollback_line(offset + i).map(render_row),
                "{case}: history line {i}"
            );
        }
    }
}

/// Review round 3: the re-wrap must keep the cap plus *twice* the screen
/// while it drops rows, because the prune may still remove up to a screen
/// of blank rows afterwards. Here the 1024th output row (where the first
/// drop happens) is the very last one: 50 history lines and 21 screen
/// lines of 14 characters (980 rows at one column), a 41-character line
/// with the cursor and 3 blank rows below it. The history must still hold
/// its full cap of 50 lines, the newest 50 of an unbounded re-wrap.
#[test]
fn a_drop_on_the_last_row_still_leaves_a_full_history() {
    let mut small = Grid::with_scrollback(24, 80, 50);
    let mut large = Grid::with_scrollback(24, 80, 1_000_000);
    for grid in [&mut small, &mut large] {
        for n in 0..70 {
            grid.put_str(&format!("line {n:04} abcd"));
            grid.carriage_return();
            grid.line_feed();
        }
        grid.put_str(&"x".repeat(41));
        grid.carriage_return();
        grid.line_feed();
        grid.line_feed();
        grid.line_feed();
        grid.cursor_up(3);
        assert_eq!(grid.scrollback_len(), 50);
    }
    small.resize(24, 1);
    large.resize(24, 1);
    check_invariants("small", &small);
    assert_eq!(
        large.scrollback_len(),
        1024 - 3 - 24,
        "no drop without a cap"
    );
    assert_eq!(small.scrollback_len(), 50);
    let offset = large.scrollback_len() - 50;
    for i in 0..50 {
        assert_eq!(
            small.scrollback_line(i).map(render_row),
            large.scrollback_line(offset + i).map(render_row),
            "history line {i}"
        );
    }
    let screen = |g: &Grid| g.visible_lines().iter().map(render_row).collect::<Vec<_>>();
    assert_eq!(screen(&small), screen(&large));
    assert_eq!(small.cursor(), large.cursor());
}

/// Review round 4: marks and a cursor past the text under the bound. Every
/// line carries a prompt mark and the cursor ends after `$ `; narrowing to
/// one column with a small history must give what an unbounded re-wrap
/// gives, marks included, and the kept history still carries marks.
#[test]
fn a_bounded_rewrap_keeps_marks_and_a_cursor_past_the_text() {
    for lines in [150, 171, 190] {
        let mut small = Grid::with_scrollback(24, 80, 50);
        let mut large = Grid::with_scrollback(24, 80, 1_000_000);
        for grid in [&mut small, &mut large] {
            for n in 0..lines {
                let row = grid.cursor().row;
                grid.line_mut(row)
                    .expect("cursor row")
                    .marks
                    .push(Mark::PromptStart);
                grid.put_str(&format!("{n:04} plain text of a line"));
                grid.carriage_return();
                grid.line_feed();
            }
            grid.put_str("$ ");
        }
        small.resize(24, 1);
        large.resize(24, 1);
        check_invariants(&format!("{lines} lines"), &small);
        let screen = |g: &Grid| g.visible_lines().iter().map(render_row).collect::<Vec<_>>();
        assert_eq!(screen(&small), screen(&large), "{lines}: screen");
        assert_eq!(small.cursor(), large.cursor(), "{lines}: cursor");
        assert_eq!(
            small.pending_wrap(),
            large.pending_wrap(),
            "{lines}: pending wrap"
        );
        assert_eq!(small.scrollback_len(), 50, "{lines}");
        let offset = large.scrollback_len() - 50;
        let mut marked = 0;
        for i in 0..50 {
            let kept = small.scrollback_line(i);
            assert_eq!(
                kept.map(render_row),
                large.scrollback_line(offset + i).map(render_row),
                "{lines}: history line {i}"
            );
            marked += usize::from(kept.is_some_and(|l| !l.marks.is_empty()));
        }
        assert!(marked > 0, "{lines}: the kept history has marks");
    }
}
