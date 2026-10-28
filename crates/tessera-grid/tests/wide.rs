//! Wide characters, grapheme clusters and zero-width scalars in the grid.
//!
//! Expected behaviour comes from `tests/fixtures/wide_grapheme_spec.txt`,
//! which cites its sources (UAX #11, UAX #29, UTS #51, xterm, kitty,
//! Alacritty, `WezTerm`) per row and records the decisions D1-D9. It is
//! checked in four ways:
//!
//! - [`width_rows_hold`] pins the raw `unicode-width` and
//!   `unicode-segmentation` answers the grid builds on, so a dependency
//!   bump that changes them fails here first.
//! - [`grid_rows_hold`] runs every grid-layer row and compares the
//!   cell-level render (`common::render`) with the row's result block,
//!   then checks the invariants. Term-layer rows run in `tessera-term`.
//! - Snapshot tests pin the render of the headline rows, so the cell
//!   counts are visible in review.
//! - Property tests drive random edits through wide characters and
//!   clusters and check the invariants after every step.

mod common;

use common::{Layer, Step, apply, assert_invariants, case, cases, render, run_grid};
use proptest::prelude::*;
use tessera_grid::{
    Cell, CellFlags, Color, EraseDisplay, EraseLine, Grid, Point, Style, char_width,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// ------------------------------------------------------------------ fixture

#[test]
fn fixture_has_every_row() {
    let cases = cases();
    let count = |layer| cases.iter().filter(|c| c.layer == layer).count();
    assert_eq!(cases.len(), 180, "fixture row count");
    assert_eq!(count(Layer::Width), 33, "width rows");
    assert_eq!(count(Layer::Term), 27, "term rows");
    let mut ids: Vec<&str> = cases.iter().map(|c| c.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), cases.len(), "row ids are unique");
}

#[test]
fn width_rows_hold() {
    let mut errors = Vec::new();
    for case in cases().iter().filter(|c| c.layer == Layer::Width) {
        let s = &case.input;
        let widths: Vec<Option<usize>> = s.chars().map(UnicodeWidthChar::width).collect();
        if widths != case.char_widths {
            errors.push(format!(
                "{}: char widths {widths:?}, expected {:?}",
                case.id, case.char_widths
            ));
        }
        // `char_width` is the library width clamped to a cell's two columns.
        let ours: Vec<Option<usize>> = s.chars().map(|c| char_width(c).map(usize::from)).collect();
        let clamped: Vec<Option<usize>> = widths.iter().map(|w| w.map(|w| w.min(2))).collect();
        if ours != clamped {
            errors.push(format!("{}: char_width disagrees: {ours:?}", case.id));
        }
        if s.width() != case.str_width {
            errors.push(format!(
                "{}: str width {}, expected {}",
                case.id,
                s.width(),
                case.str_width
            ));
        }
        let counts: Vec<usize> = s.graphemes(true).map(|g| g.chars().count()).collect();
        if counts != case.grapheme_scalar_counts {
            errors.push(format!(
                "{}: grapheme scalar counts {counts:?}, expected {:?}",
                case.id, case.grapheme_scalar_counts
            ));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn grid_rows_hold() {
    let mut errors = Vec::new();
    for case in cases().iter().filter(|c| c.layer == Layer::Grid) {
        let grid = run_grid(case);
        let got = render(&grid);
        if got != case.expected {
            errors.push(format!(
                "{}:\nexpected:\n{}got:\n{got}",
                case.id, case.expected
            ));
        }
        let broken = common::invariant_errors(&grid);
        if !broken.is_empty() {
            errors.push(format!("{}: {}", case.id, broken.join("; ")));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// Runs a fixture row and snapshots its render as `wide__<id>.snap`.
fn snapshot_row(id: &str) {
    let case = case(id);
    assert_eq!(case.layer, Layer::Grid, "{id} is not a grid row");
    let grid = run_grid(&case);
    assert_invariants(id, &grid);
    insta::with_settings!({ description => case.note.as_str(), omit_expression => true }, {
        insta::assert_snapshot!(id, render(&grid));
    });
}

macro_rules! snapshot_rows {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                snapshot_row(stringify!($name));
            }
        )*
    };
}

snapshot_rows! {
    b01_cjk_pair,
    b02_fullwidth_latin,
    b05_hangul_conjoining_jamo_make_one_wide_cluster,
    b08_wide_fills_line_exactly_arms_pending_wrap,
    c01_wide_with_one_column_left_wraps_and_blanks_last_col,
    c05_stranded_column_is_the_right_half_of_a_wide_char,
    c06_wide_wrap_at_bottom_scrolls_and_keeps_flags,
    c12_one_column_grid_drops_wide_chars_grid,
    d01_emoji_with_skin_tone_is_one_two_cell_cluster,
    d02_zwj_family_is_one_two_cell_cluster,
    d03_flag_pair_grows_from_one_to_two_cells,
    d10_flag_growth_at_the_last_column_is_refused,
    d11_vs16_widens_a_text_presentation_base,
    d16_vs15_narrows_a_wide_emoji,
    d21_kiss_sequence_survives_a_width_three_prefix,
    d24_emoji_modifier_after_ascii_is_its_own_cell,
    e01_combining_acute_attaches_to_the_left_cell,
    e02_stacked_marks_extend_the_same_cluster,
    e05_mark_at_column_zero_of_an_empty_line_is_dropped,
    e09_mark_with_pending_wrap_attaches_to_the_cell_under_the_cursor,
    f01_overwrite_left_half_with_narrow,
    f02_overwrite_right_half_with_narrow,
    f03_overwrite_straddling_two_wide_chars_with_a_wide_char,
    g01_ech_starting_on_a_spacer_blanks_the_base_too,
    g02_ech_ending_on_a_wide_base_blanks_its_spacer,
    g08_el_to_end_from_a_spacer,
    g10_el_to_start_through_a_wide_base,
    g13_ed_below_from_a_spacer,
    g16_dch_one_at_a_wide_base_blanks_the_orphaned_spacer,
    g17_dch_one_at_a_spacer_blanks_the_base,
    g23_ich_at_a_wide_base_shifts_the_pair,
    g24_ich_between_the_halves_destroys_the_pair,
    g26_ich_pushing_the_spacer_off_the_edge_blanks_the_base,
    i01_scrolled_off_cluster_text_is_kept_in_scrollback,
}

// ----------------------------------------------------------- direct checks

#[test]
fn cell_is_still_sixteen_bytes() {
    assert!(size_of::<Cell>() <= 16, "{} bytes", size_of::<Cell>());
}

#[test]
fn wide_cell_and_spacer_share_the_whole_style() {
    let mut grid = Grid::new(1, 6);
    let pen = Style {
        fg: Color::Indexed(2),
        bg: Color::Rgb(1, 2, 3),
        flags: Style::BOLD | Style::UNDERLINE,
    };
    grid.set_pen(pen);
    grid.put_str("中🇸🇪");
    let line = grid.line(0).expect("row 0");
    for col in [0, 2] {
        assert!(line.cells[col].is_wide(), "col {col}");
        assert!(line.cells[col + 1].is_spacer(), "col {col} + 1");
        assert_eq!(line.cells[col].style, pen, "base keeps the pen");
        assert_eq!(line.cells[col + 1].style, pen, "spacer copies the base");
    }
    assert_eq!(line.cells[1].ch, ' ');
    assert_eq!(line.cells[1].width(), 0);
    assert_eq!(line.cells[0].width(), 2);
}

#[test]
fn cluster_is_none_for_spacers_and_plain_cells() {
    let mut grid = Grid::new(1, 8);
    grid.put_str("e\u{301}👍🏽x");
    let line = grid.line(0).expect("row 0");
    assert_eq!(line.cluster(0), Some("e\u{301}"));
    assert_eq!(line.cluster(1), Some("👍🏽"));
    assert_eq!(line.cluster(2), None, "spacer");
    assert_eq!(line.cluster(3), None, "plain x");
    assert_eq!(line.cluster(99), None, "out of range");
    assert_eq!(line.graphemes.len(), 2);
    assert_eq!(
        line.cells[1].flags,
        CellFlags::WIDE | CellFlags::CLUSTER,
        "the base carries both markers"
    );
}

#[test]
fn would_attach_is_pure_and_matches_put_char() {
    let mut grid = Grid::new(2, 5);
    grid.put_str("ab中");
    let before = render(&grid);
    assert!(
        grid.would_attach('\u{301}'),
        "a mark joins 中 through its spacer"
    );
    assert!(!grid.would_attach('x'));
    assert!(!grid.would_attach('\u{200B}'), "ZWSP never joins");
    assert!(!grid.would_attach('\u{7}'), "controls never join");
    assert_eq!(render(&grid), before, "the query changes nothing");
    grid.put_char('\u{301}');
    assert_eq!(grid.line(0).unwrap().cluster(2), Some("中\u{301}"));
    assert_eq!(grid.cursor(), Point::new(0, 4), "attaching does not move");
    assert!(!grid.pending_wrap());
}

#[test]
fn controls_never_take_a_cell() {
    let mut grid = Grid::new(1, 4);
    grid.put_str("a\u{0}\u{1b}\u{7f}\u{85}b");
    assert_eq!(grid.line(0).unwrap().text(), "ab");
    assert_eq!(grid.cursor(), Point::new(0, 2));
}

#[test]
fn put_cell_ignores_width_markers() {
    let mut grid = Grid::new(1, 4);
    grid.put_str("中");
    grid.goto(0, 0);
    grid.put_cell(Cell {
        flags: CellFlags::WIDE | CellFlags::CLUSTER,
        ..Cell::new('x', Style::DEFAULT)
    });
    assert_invariants("put_cell", &grid);
    assert_eq!(grid.line(0).unwrap().cells[0].flags, CellFlags::EMPTY);
    assert_eq!(grid.line(0).unwrap().text(), "x");
}

#[test]
fn alt_screen_and_scrolling_carry_the_table_with_the_line() {
    let mut grid = Grid::with_scrollback(2, 6, 4);
    grid.put_str("e\u{301}中");
    grid.enter_alt_screen();
    grid.goto(0, 0);
    grid.put_str("a\u{302}");
    assert_eq!(grid.line(0).unwrap().text(), "a\u{302}");
    grid.leave_alt_screen();
    assert_eq!(grid.line(0).unwrap().text(), "e\u{301}中");
    grid.goto(1, 0);
    grid.put_str("x\u{303}");
    grid.scroll_up(1);
    assert_eq!(grid.line(0).unwrap().text(), "x\u{303}");
    assert_eq!(grid.scrollback_line(0).unwrap().text(), "e\u{301}中");
    grid.scroll_down(1);
    grid.insert_lines(1);
    grid.delete_lines(1);
    assert_invariants("scroll", &grid);
    // A recycled line comes back without the evicted line's clusters.
    for _ in 0..8 {
        grid.goto(1, 0);
        grid.put_str("y\u{304}");
        grid.line_feed();
    }
    assert_invariants("recycled", &grid);
    assert_eq!(grid.line(1).unwrap().text(), "");
}

#[test]
fn erase_in_display_all_and_clear_drop_every_cluster() {
    let mut grid = Grid::new(2, 6);
    grid.put_str("e\u{301}中");
    grid.erase_in_display(EraseDisplay::All);
    assert!(grid.line(0).unwrap().graphemes.is_empty());
    grid.put_str("e\u{301}");
    grid.erase_in_line(EraseLine::All);
    assert!(grid.line(0).unwrap().graphemes.is_empty());
    assert_invariants("erased", &grid);
}

// --------------------------------------------------------------- properties

/// Characters that exercise every path: plain, wide, zero-width that
/// attach or drop, modifiers, regional indicators, VS15/VS16, spacing
/// marks, prepend and controls.
fn interesting_char() -> impl Strategy<Value = char> {
    prop_oneof![
        8 => prop::sample::select(vec![
            'a', 'e', ' ', '\u{A0}', '中', '文', '😀', '👍', '👨', '👩', '⌚', '☺', '❤', '🏳',
            '🌈', '🈁', 'ᄀ', '가', 'क', '1',
        ]),
        6 => prop::sample::select(vec![
            '\u{301}', '\u{302}', '\u{200D}', '\u{200C}', '\u{200B}', '\u{FE0E}', '\u{FE0F}',
            '\u{20E3}', '\u{E0067}', '\u{1161}', '\u{11A8}', '\u{94D}', '\u{AD}',
        ]),
        4 => prop::sample::select(vec![
            '🏽', '🏻', '🇸', '🇪', '🇳', '🇴', '\u{93E}', '\u{600}', '\u{1F3F4}', '\u{7F}',
        ]),
        1 => any::<char>(),
    ]
}

#[derive(Debug, Clone)]
enum Op {
    Step(Step),
    Put(char),
    ScrollUp(u16),
    ScrollDown(u16),
    InsertLines(u16),
    DeleteLines(u16),
    Ri,
    Right(u16),
    Left(u16),
    Region(u16, u16),
}

fn op() -> impl Strategy<Value = Op> {
    let n = || 0..=4u16;
    prop_oneof![
        20 => interesting_char().prop_map(Op::Put),
        3 => (n(), 0..=12u16).prop_map(|(r, c)| Op::Step(Step::Goto(r, c))),
        1 => Just(Op::Step(Step::Cr)),
        2 => Just(Op::Step(Step::Lf)),
        2 => n().prop_map(|k| Op::Step(Step::Ich(k))),
        2 => n().prop_map(|k| Op::Step(Step::Dch(k))),
        2 => n().prop_map(|k| Op::Step(Step::Ech(k))),
        1 => prop_oneof![
            Just(EraseLine::ToEnd),
            Just(EraseLine::ToStart),
            Just(EraseLine::All)
        ]
        .prop_map(|m| Op::Step(Step::El(m))),
        1 => prop_oneof![
            Just(EraseDisplay::Below),
            Just(EraseDisplay::Above),
            Just(EraseDisplay::All)
        ]
        .prop_map(|m| Op::Step(Step::Ed(m))),
        1 => prop::option::of(0..=7u8).prop_map(|bg| Op::Step(Step::PenBg(bg))),
        1 => n().prop_map(Op::ScrollUp),
        1 => n().prop_map(Op::ScrollDown),
        1 => n().prop_map(Op::InsertLines),
        1 => n().prop_map(Op::DeleteLines),
        1 => Just(Op::Ri),
        1 => n().prop_map(Op::Right),
        1 => n().prop_map(Op::Left),
        1 => (n(), n()).prop_map(|(t, b)| Op::Region(t, b)),
    ]
}

/// The column of the cell a scalar would join, per the attach target rule
/// (D7): left of the cursor, or under it while a wrap is pending, resolved
/// through a spacer.
fn attach_target(grid: &Grid) -> Option<usize> {
    let cursor = grid.cursor();
    let col = usize::from(cursor.col);
    let base = if grid.pending_wrap() {
        col
    } else {
        col.checked_sub(1)?
    };
    let line = grid.line(cursor.row)?;
    if line.cells[base].is_spacer() {
        base.checked_sub(1)
    } else {
        Some(base)
    }
}

fn run_op(grid: &mut Grid, op: &Op) {
    match op {
        Op::Step(step) => apply(grid, step),
        &Op::Put(ch) => {
            let before = render(grid);
            let attaches = grid.would_attach(ch);
            assert_eq!(render(grid), before, "would_attach must not mutate");
            let row = grid.cursor().row;
            let target = attach_target(grid);
            grid.put_char(ch);
            if attaches {
                assert_eq!(grid.cursor().row, row, "an attach never wraps");
                let base = target.expect("an attach has a target");
                let cluster = grid.line(row).and_then(|l| l.cluster(base));
                assert!(
                    cluster.is_some_and(|c| c.ends_with(ch)),
                    "{ch:?} did not join column {base}: {cluster:?}"
                );
            }
        }
        &Op::ScrollUp(n) => grid.scroll_up(n),
        &Op::ScrollDown(n) => grid.scroll_down(n),
        &Op::InsertLines(n) => grid.insert_lines(n),
        &Op::DeleteLines(n) => grid.delete_lines(n),
        Op::Ri => grid.reverse_index(),
        &Op::Right(n) => grid.cursor_right(n),
        &Op::Left(n) => grid.cursor_left(n),
        &Op::Region(t, b) => grid.set_scroll_region(t, b),
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        ..ProptestConfig::default()
    })]

    #[test]
    fn invariants_hold_after_every_operation(
        rows in 1..=4u16,
        cols in 1..=9u16,
        ops in prop::collection::vec(op(), 0..120),
    ) {
        let mut grid = Grid::with_scrollback(rows, cols, 3);
        for (i, op) in ops.iter().enumerate() {
            run_op(&mut grid, op);
            let errors = common::invariant_errors(&grid);
            prop_assert!(errors.is_empty(), "after op {i} {op:?}: {}\n{}", errors.join("; "), render(&grid));
        }
    }

    #[test]
    fn a_lone_grapheme_reads_back_whole(
        text in prop::sample::select(vec![
            "e\u{301}", "e\u{301}\u{302}", "👍🏽", "👨\u{200D}👩\u{200D}👧", "🇸🇪", "☺\u{FE0F}",
            "⌚\u{FE0E}", "1\u{FE0F}\u{20E3}", "ᄒ\u{1161}\u{11AB}", "🏳\u{FE0F}\u{200D}🌈",
            "क\u{94D}ष", "가\u{11A8}", "🏴\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}",
        ]),
        col in 0..=5u16,
    ) {
        // I4: a single grapheme written where it fits loses no scalar.
        let mut grid = Grid::new(1, 8);
        grid.goto(0, col);
        grid.put_str(text);
        let read = grid.line(0).unwrap().text();
        prop_assert_eq!(read.trim_start(), text);
        prop_assert_eq!(grid.line(0).unwrap().graphemes.len(), 1);
    }
}
