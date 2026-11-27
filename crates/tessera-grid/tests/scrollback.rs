//! Scrollback ring, logical buffer and alternate screen behaviour of
//! `Grid`, through the public API only.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use tessera_grid::{Cell, Color, Grid, Line, Point, Scrollback, Style};

/// Pushes `count` numbered lines (`L{from}`, `L{from + 1}`, …) into history:
/// each is written on the top row, which then scrolls off the screen. The
/// cursor is left on row 0.
fn write_numbered(grid: &mut Grid, from: usize, count: usize) {
    for n in from..from + count {
        grid.goto(0, 0);
        grid.put_str(&format!("L{n}"));
        grid.scroll_up(1);
    }
}

fn history(grid: &Grid) -> Vec<String> {
    (0..grid.scrollback_len())
        .map(|i| grid.scrollback_line(i).unwrap().text())
        .collect()
}

fn numbered(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|n| format!("L{n}")).collect()
}

fn line(text: &str) -> Line {
    let mut line = Line::blank(8, Style::DEFAULT);
    for (cell, ch) in line.cells.iter_mut().zip(text.chars()) {
        cell.ch = ch;
    }
    line
}

// ---------------------------------------------------------------- (1) cap

#[test]
fn pushing_past_cap_keeps_exactly_cap_newest_lines() {
    let cap = 50;
    let mut grid = Grid::with_scrollback(4, 10, cap);
    write_numbered(&mut grid, 0, cap + 17);
    assert_eq!(grid.scrollback_len(), cap);
    assert_eq!(history(&grid), numbered(17..cap + 17));
}

#[test]
fn ring_drops_oldest_when_full() {
    let mut sb = Scrollback::new(3);
    for text in ["a", "b", "c", "d", "e"] {
        sb.push(line(text));
    }
    assert_eq!(sb.len(), 3);
    assert_eq!(sb.cap(), 3);
    let texts: Vec<String> = sb.iter().map(|line| line.text()).collect();
    assert_eq!(texts, ["c", "d", "e"]);
    assert_eq!(sb.get(0).map(|line| line.text()).as_deref(), Some("c"));
    assert_eq!(sb.get(3), None);
}

/// Correctness of eviction at scale through `Grid`. The O(1) timing bound
/// lives on the ring itself (`a_push_costs_the_same_whatever_the_history_holds`): a
/// wall-clock bound here would mostly time debug-build per-line grid work
/// and flake on a loaded or efficiency core.
#[test]
fn million_line_scroll_with_10k_cap_keeps_newest_10k() {
    const LINES: usize = 1_000_000;
    const CAP: usize = 10_000;
    let mut grid = Grid::with_scrollback(2, 8, CAP);
    for n in 0..LINES {
        if n % 1_000 == 0 {
            grid.goto(1, 0);
            grid.put_str(&format!("L{n}"));
        }
        grid.line_feed();
    }
    assert_eq!(grid.scrollback_len(), CAP);
    // `L{n}` is written on row 1 and reaches history two line feeds later,
    // so the retained window holds exactly the marks from the last CAP lines.
    let texts = history(&grid);
    let marked: Vec<&str> = texts
        .iter()
        .map(String::as_str)
        .filter(|t| !t.is_empty())
        .collect();
    let expected: Vec<String> = (LINES - CAP..LINES)
        .step_by(1_000)
        .map(|n| format!("L{n}"))
        .collect();
    assert_eq!(marked, expected);
}

/// A million pushes into a full store of `cap` lines, timed.
fn million_pushes(cap: usize) -> Duration {
    let mut sb = Scrollback::new(cap);
    for _ in 0..cap {
        sb.push(Line::default());
    }
    let start = Instant::now();
    for _ in 0..1_000_000 {
        sb.push(Line::default());
    }
    let elapsed = start.elapsed();
    assert_eq!(sb.len(), cap);
    elapsed
}

/// A push costs the same whatever the history holds (O(1): it packs at
/// most one row and evicts at most one, ADR 0051): a million pushes into a
/// full store of 100 000 lines take about as long as into one of 1 000.
/// A ratio, not a wall-clock budget: a debug build's absolute time mostly
/// measures the machine's load. A push linear in the history would read
/// about 100x.
#[test]
fn a_push_costs_the_same_whatever_the_history_holds() {
    const MAX_RATIO: f64 = 3.0;
    let mut ratios = Vec::new();
    for _ in 0..3 {
        let small = million_pushes(1_000);
        let large = million_pushes(100_000);
        let ratio = large.as_secs_f64() / small.as_secs_f64();
        if ratio < MAX_RATIO {
            return;
        }
        ratios.push(ratio);
    }
    panic!(
        "a push into 100 000 lines cost more than {MAX_RATIO}x one into 1 000 in every attempt: {ratios:.2?}"
    );
}

// -------------------------------------------------------- (2) set the cap

#[test]
fn lowering_the_cap_trims_oldest_first() {
    let mut grid = Grid::with_scrollback(3, 10, 100);
    write_numbered(&mut grid, 0, 30);
    assert_eq!(grid.scrollback_len(), 30);
    grid.set_max_scrollback(10);
    assert_eq!(grid.max_scrollback(), 10);
    assert_eq!(history(&grid), numbered(20..30));
    // New lines keep evicting the oldest under the new cap.
    write_numbered(&mut grid, 30, 5);
    assert_eq!(history(&grid), numbered(25..35));
}

#[test]
fn lowering_the_cap_after_the_ring_wrapped_keeps_order() {
    let mut grid = Grid::with_scrollback(2, 10, 8);
    write_numbered(&mut grid, 0, 13);
    assert_eq!(history(&grid), numbered(5..13));
    grid.set_max_scrollback(3);
    assert_eq!(history(&grid), numbered(10..13));
    grid.set_max_scrollback(6);
    write_numbered(&mut grid, 13, 4);
    assert_eq!(history(&grid), numbered(11..17));
}

#[test]
fn cap_zero_disables_history() {
    let mut grid = Grid::with_scrollback(3, 10, 100);
    write_numbered(&mut grid, 0, 10);
    grid.set_max_scrollback(0);
    assert_eq!(grid.scrollback_len(), 0);
    write_numbered(&mut grid, 10, 10);
    assert_eq!(grid.scrollback_len(), 0);
    assert_eq!(grid.total_lines(), 3);
    // Raising it again starts collecting from now on.
    grid.set_max_scrollback(5);
    write_numbered(&mut grid, 20, 2);
    assert_eq!(history(&grid).len(), 2);
}

#[test]
fn clear_scrollback_keeps_cap_and_screen() {
    let mut grid = Grid::with_scrollback(2, 10, 100);
    write_numbered(&mut grid, 0, 5);
    let screen = grid.visible_lines().to_vec();
    grid.clear_scrollback();
    assert_eq!(grid.scrollback_len(), 0);
    assert_eq!(grid.max_scrollback(), 100);
    assert_eq!(grid.visible_lines(), screen.as_slice());
    write_numbered(&mut grid, 5, 3);
    assert_eq!(grid.scrollback_len(), 3);
}

#[test]
fn constructors_keep_their_caps() {
    assert_eq!(
        Grid::new(2, 2).max_scrollback(),
        tessera_grid::DEFAULT_MAX_SCROLLBACK
    );
    assert_eq!(Grid::with_scrollback(2, 2, 7).max_scrollback(), 7);
}

// --------------------------------------------------------- (3) logical view

fn assert_logical_view(grid: &Grid) {
    let history = grid.scrollback_len();
    assert_eq!(grid.total_lines(), history + usize::from(grid.rows()));
    for i in 0..grid.total_lines() {
        let expected = if i < history {
            grid.scrollback_line(i)
        } else {
            grid.line(u16::try_from(i - history).unwrap())
                .map(tessera_grid::LineRef::borrowed)
        };
        assert!(expected.is_some(), "line {i} missing");
        assert_eq!(grid.logical_line(i), expected, "logical line {i}");
    }
    assert_eq!(grid.logical_line(grid.total_lines()), None);
    let all: Vec<Line> = grid
        .logical_lines()
        .map(tessera_grid::LineRef::into_owned)
        .collect();
    assert_eq!(all.len(), grid.total_lines());
    for (i, line) in all.iter().enumerate() {
        assert_eq!(Some(line), grid.logical_line(i).as_deref());
    }
    let reversed: Vec<Line> = grid
        .logical_lines()
        .rev()
        .map(tessera_grid::LineRef::into_owned)
        .collect();
    assert!(reversed.iter().rev().eq(all.iter()));
}

#[test]
fn logical_buffer_is_history_then_screen() {
    let mut grid = Grid::with_scrollback(3, 10, 4);
    assert_logical_view(&grid);
    write_numbered(&mut grid, 0, 2);
    assert_logical_view(&grid);
    // Wrap the ring so its storage no longer starts at the oldest line.
    write_numbered(&mut grid, 2, 7);
    assert_eq!(history(&grid), numbered(5..9));
    assert_logical_view(&grid);
    let texts: Vec<String> = grid.logical_lines().map(|line| line.text()).collect();
    assert_eq!(texts, ["L5", "L6", "L7", "L8", "", "", ""]);
}

#[test]
fn logical_buffer_on_alt_screen_shows_primary_history_then_alt_screen() {
    let mut grid = Grid::with_scrollback(2, 10, 10);
    write_numbered(&mut grid, 0, 3);
    grid.enter_alt_screen();
    grid.goto(0, 0);
    grid.put_str("alt");
    assert_logical_view(&grid);
    let texts: Vec<String> = grid.logical_lines().map(|line| line.text()).collect();
    assert_eq!(texts, ["L0", "L1", "L2", "alt", ""]);
}

// --------------------------------------------------------- (4) alt screen

fn red() -> Style {
    Style {
        fg: Color::Indexed(1),
        bg: Color::Rgb(9, 9, 9),
        flags: Style::BOLD,
        ..Style::DEFAULT
    }
}

#[test]
fn alt_screen_round_trip_restores_primary_exactly() {
    let mut grid = Grid::with_scrollback(4, 6, 10);
    write_numbered(&mut grid, 0, 5);
    grid.goto(1, 0);
    grid.put_str("prompt");
    assert!(grid.pending_wrap());
    grid.set_pen(red());
    grid.goto(0, 3);
    grid.set_tab_stop();
    grid.goto(1, 5);
    grid.put_char('$');

    let lines = grid.visible_lines().to_vec();
    let cursor = grid.cursor();
    let pending_wrap = grid.pending_wrap();
    let pen = grid.pen();
    let tabs = grid.tabs().clone();
    let saved_history = history(&grid);

    assert!(!grid.is_alt_screen());
    grid.enter_alt_screen();
    assert!(grid.is_alt_screen());
    // A fresh blank screen of the same size; the cursor stays put.
    assert_eq!(grid.visible_lines().len(), 4);
    assert!(grid.visible_lines().iter().all(|l| l.text().is_empty()));
    assert_eq!(grid.cursor(), cursor);
    assert!(!grid.pending_wrap());

    grid.set_pen(Style::DEFAULT);
    grid.clear_all_tab_stops();
    grid.goto(0, 0);
    grid.put_str("vim vim vim vim vim");
    grid.scroll_up(2);
    grid.goto(3, 2);
    grid.line_feed();
    grid.scroll_down(1);
    assert_eq!(
        history(&grid),
        saved_history,
        "alt screen must not touch history"
    );

    grid.leave_alt_screen();
    assert!(!grid.is_alt_screen());
    assert_eq!(grid.visible_lines(), lines.as_slice());
    assert_eq!(grid.cursor(), cursor);
    assert_eq!(grid.pending_wrap(), pending_wrap);
    assert_eq!(grid.pen(), pen);
    assert_eq!(grid.tabs(), &tabs);
    assert_eq!(history(&grid), saved_history);
}

#[test]
fn scrolling_on_alt_screen_does_not_grow_scrollback() {
    let mut grid = Grid::with_scrollback(3, 4, 100);
    write_numbered(&mut grid, 0, 4);
    let before = grid.scrollback_len();
    grid.enter_alt_screen();
    grid.goto(2, 0);
    for _ in 0..20 {
        grid.put_str("abcd");
        grid.line_feed();
    }
    grid.scroll_up(u16::MAX);
    assert_eq!(grid.scrollback_len(), before);
    grid.leave_alt_screen();
    assert_eq!(grid.scrollback_len(), before);
    // Back on the primary screen, scrolling records history again.
    grid.goto(2, 0);
    grid.line_feed();
    assert_eq!(grid.scrollback_len(), before + 1);
}

#[test]
fn alt_screen_is_blank_with_pen_background() {
    let mut grid = Grid::new(2, 3);
    grid.put_str("abc");
    grid.set_pen(red());
    grid.enter_alt_screen();
    for line in grid.visible_lines() {
        assert_eq!(line.cells.len(), 3);
        assert!(line.cells.iter().all(Cell::is_blank));
        assert!(line.cells.iter().all(|c| c.style.bg == red().bg));
    }
}

#[test]
fn entering_twice_and_leaving_twice_are_no_ops() {
    let mut grid = Grid::new(2, 4);
    grid.put_str("main");
    grid.leave_alt_screen();
    assert!(!grid.is_alt_screen());
    assert_eq!(grid.line(0).unwrap().text(), "main");

    grid.enter_alt_screen();
    grid.goto(0, 0);
    grid.put_str("alt");
    grid.enter_alt_screen();
    assert_eq!(
        grid.line(0).unwrap().text(),
        "alt",
        "second enter keeps alt"
    );
    grid.leave_alt_screen();
    assert_eq!(grid.line(0).unwrap().text(), "main");
    grid.leave_alt_screen();
    assert_eq!(grid.line(0).unwrap().text(), "main");
}

#[test]
fn each_entry_starts_a_fresh_alt_screen() {
    let mut grid = Grid::new(2, 4);
    grid.enter_alt_screen();
    grid.put_str("old");
    grid.leave_alt_screen();
    grid.enter_alt_screen();
    assert!(grid.visible_lines().iter().all(|l| l.text().is_empty()));
}

#[test]
fn reset_leaves_alt_screen_and_keeps_history() {
    let mut grid = Grid::with_scrollback(2, 4, 10);
    write_numbered(&mut grid, 0, 3);
    grid.enter_alt_screen();
    grid.put_str("alt");
    grid.reset();
    assert!(!grid.is_alt_screen());
    assert_eq!(history(&grid), numbered(0..3));
    assert_eq!(grid.max_scrollback(), 10);
    assert!(grid.visible_lines().iter().all(|l| l.text().is_empty()));
    assert_eq!(grid.cursor(), Point::new(0, 0));
}

#[test]
fn clearing_scrollback_on_alt_screen_clears_primary_history() {
    let mut grid = Grid::with_scrollback(2, 4, 10);
    write_numbered(&mut grid, 0, 3);
    grid.enter_alt_screen();
    grid.clear_scrollback();
    grid.leave_alt_screen();
    assert_eq!(grid.scrollback_len(), 0);
}

#[test]
fn each_screen_has_its_own_saved_cursor() {
    // DECSET 1049 is DECSC + enter, then leave + DECRC. An application that
    // saves the cursor on the alternate screen (ESC 7) must not clobber the
    // primary screen's saved cursor, as in xterm, VTE and Alacritty.
    let mut grid = Grid::new(10, 10);
    grid.set_pen(red());
    grid.goto(5, 5);
    grid.save_cursor();
    grid.enter_alt_screen();

    grid.set_pen(Style::DEFAULT);
    grid.goto(0, 0);
    grid.save_cursor();
    grid.goto(7, 2);
    grid.restore_cursor();
    assert_eq!(grid.cursor(), Point::new(0, 0), "alt restores its own save");

    grid.leave_alt_screen();
    grid.goto(9, 9);
    grid.set_pen(Style::DEFAULT);
    grid.restore_cursor();
    assert_eq!(grid.cursor(), Point::new(5, 5));
    assert_eq!(grid.pen(), red());

    // The alternate screen's slot starts fresh on every entry.
    grid.enter_alt_screen();
    grid.goto(3, 3);
    grid.restore_cursor();
    assert_eq!(grid.cursor(), Point::new(0, 0));
    assert_eq!(grid.pen(), Style::DEFAULT);
}

// -------------------------------------------------------- (5) properties

#[derive(Debug, Clone)]
enum Op {
    Put(char),
    LineFeed,
    ScrollUp(u16),
    ScrollDown(u16),
    SetRegion(u16, u16),
    ResetRegion,
    SetCap(usize),
    ClearScrollback,
    EnterAlt,
    LeaveAlt,
    SaveCursor,
    RestoreCursor,
    Reset,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => any::<char>().prop_map(Op::Put),
        8 => Just(Op::LineFeed),
        4 => prop_oneof![3 => 0..=6u16, 1 => any::<u16>()].prop_map(Op::ScrollUp),
        1 => (0..=6u16).prop_map(Op::ScrollDown),
        1 => (0..=8u16, 0..=8u16).prop_map(|(t, b)| Op::SetRegion(t, b)),
        1 => Just(Op::ResetRegion),
        2 => (0..=40usize).prop_map(Op::SetCap),
        1 => Just(Op::ClearScrollback),
        1 => Just(Op::EnterAlt),
        1 => Just(Op::LeaveAlt),
        1 => Just(Op::SaveCursor),
        1 => Just(Op::RestoreCursor),
        1 => Just(Op::Reset),
    ]
}

fn apply(grid: &mut Grid, op: &Op) {
    match *op {
        Op::Put(ch) => grid.put_char(ch),
        Op::LineFeed => grid.line_feed(),
        Op::ScrollUp(n) => grid.scroll_up(n),
        Op::ScrollDown(n) => grid.scroll_down(n),
        Op::SetRegion(t, b) => grid.set_scroll_region(t, b),
        Op::ResetRegion => grid.reset_scroll_region(),
        Op::SetCap(cap) => grid.set_max_scrollback(cap),
        Op::ClearScrollback => grid.clear_scrollback(),
        Op::EnterAlt => grid.enter_alt_screen(),
        Op::LeaveAlt => grid.leave_alt_screen(),
        Op::SaveCursor => grid.save_cursor(),
        Op::RestoreCursor => grid.restore_cursor(),
        Op::Reset => grid.reset(),
    }
}

/// Reference model of the ring: a `VecDeque` trimmed from the front.
#[derive(Debug, Clone)]
enum RingOp {
    Push(u8),
    SetCap(usize),
    Clear,
}

fn ring_op() -> impl Strategy<Value = RingOp> {
    prop_oneof![
        8 => any::<u8>().prop_map(RingOp::Push),
        2 => (0..=12usize).prop_map(RingOp::SetCap),
        1 => Just(RingOp::Clear),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    #[test]
    fn scrollback_never_exceeds_cap(
        rows in 1..=8u16,
        cols in 1..=12u16,
        cap in 0..=40usize,
        ops in prop::collection::vec(op(), 0..300),
    ) {
        let mut grid = Grid::with_scrollback(rows, cols, cap);
        for op in &ops {
            let before = grid.scrollback_len();
            let was_alt = grid.is_alt_screen();
            apply(&mut grid, op);
            prop_assert!(
                grid.scrollback_len() <= grid.max_scrollback(),
                "{} > {} after {:?}", grid.scrollback_len(), grid.max_scrollback(), op
            );
            if was_alt && matches!(op, Op::Put(_) | Op::LineFeed | Op::ScrollUp(_) | Op::ScrollDown(_)) {
                prop_assert_eq!(grid.scrollback_len(), before, "alt screen grew history");
            }
            prop_assert_eq!(grid.total_lines(), grid.scrollback_len() + usize::from(rows));
            prop_assert_eq!(grid.visible_lines().len(), usize::from(rows));
            for line in grid.logical_lines() {
                prop_assert_eq!(line.cells.len(), usize::from(cols));
            }
        }
        assert_logical_view(&grid);
    }

    #[test]
    fn ring_matches_deque_model(ops in prop::collection::vec(ring_op(), 0..200), cap in 0..=12usize) {
        let mut ring = Scrollback::new(cap);
        let mut model: VecDeque<u8> = VecDeque::new();
        let mut model_cap = cap;
        for op in &ops {
            match *op {
                RingOp::Push(n) => {
                    let mut l = Line::blank(1, Style::DEFAULT);
                    l.cells[0].style.flags = u16::from(n);
                    let spilled = ring.push(l).map(|l| l.cells[0].style.flags);
                    model.push_back(n);
                    let expected = (model.len() > model_cap)
                        .then(|| model.pop_front().map(u16::from))
                        .flatten();
                    prop_assert_eq!(spilled, expected);
                }
                RingOp::SetCap(c) => {
                    ring.set_cap(c);
                    model_cap = c;
                    while model.len() > c {
                        model.pop_front();
                    }
                }
                RingOp::Clear => {
                    ring.clear();
                    model.clear();
                }
            }
            prop_assert_eq!(ring.len(), model.len());
            prop_assert_eq!(ring.cap(), model_cap);
            prop_assert_eq!(ring.is_empty(), model.is_empty());
            let got: Vec<u16> = ring.iter().map(|l| l.cells[0].style.flags).collect();
            let want: Vec<u16> = model.iter().copied().map(u16::from).collect();
            prop_assert_eq!(&got, &want);
            for (i, flags) in want.iter().enumerate() {
                prop_assert_eq!(ring.get(i).map(|l| l.cells[0].style.flags), Some(*flags));
            }
            prop_assert!(ring.get(model.len()).is_none());
        }
    }
}

// ------------------------------------- (n) packed rows release their links

/// Writes one row of `links` one-cell OSC 8 links tagged `tag`, then CR LF.
fn row_of_links(grid: &mut Grid, tag: usize, links: usize) {
    for c in 0..links {
        grid.begin_hyperlink(&format!("https://x.test/{tag}/{c}"), None)
            .expect("a short link opens");
        grid.put_char('a');
    }
    grid.end_hyperlink();
    grid.carriage_return();
    grid.line_feed();
}

/// Writes `n` plain rows.
fn plain_rows(grid: &mut Grid, n: usize) {
    for _ in 0..n {
        grid.put_str("plain");
        grid.carriage_return();
        grid.line_feed();
    }
}

/// Evicting the front of a packed block releases the links of the
/// evicted rows at once, though the block itself stays (ADR 0051 D6).
#[test]
fn evicting_part_of_a_packed_block_releases_its_links() {
    let mut grid = Grid::with_scrollback(24, 80, 600);
    for t in 0..700 {
        row_of_links(&mut grid, t, 10);
    }
    assert_eq!(grid.scrollback_len(), 600);
    let before = grid.live_hyperlinks();
    plain_rows(&mut grid, 100);
    let after = grid.live_hyperlinks();
    assert_eq!(before - after, 1_000, "before {before}, after {after}");
}

/// Rows read at random (and so cached decoded, ADR 0051 D4) hold no
/// hyperlink entity once evicted, nor once the history is switched off.
#[test]
fn evicted_rows_read_before_hold_no_links() {
    let filled = || {
        let mut grid = Grid::with_scrollback(24, 80, 2_000);
        for t in 0..800 {
            row_of_links(&mut grid, t, 80);
        }
        for i in 0..grid.scrollback_len() {
            drop(grid.scrollback_line(i));
        }
        grid
    };
    let mut grid = filled();
    assert_eq!(grid.live_hyperlinks(), 800 * 80);
    plain_rows(&mut grid, 2_100);
    assert_eq!(grid.live_hyperlinks(), 0, "evicted rows keep links");
    let mut grid = filled();
    let screen = grid.live_hyperlinks() - grid.scrollback_len() * 80;
    assert!(grid.scrollback_len() > 700 && screen > 0);
    grid.set_max_scrollback(0);
    assert_eq!(grid.scrollback_len(), 0);
    assert_eq!(grid.live_hyperlinks(), screen, "a cap of 0 keeps links");
}
