//! Differential tests for packed scrollback (ADR 0051): a grid whose
//! history is packed after a few hot rows must read and re-wrap exactly
//! like one whose history stays unpacked (the cell path).

use proptest::prelude::*;

use crate::scrollback::Scrollback;
use crate::{Color, Grid, Line, LineRef, Mark, Style};

/// One step of a generated session.
#[derive(Debug, Clone)]
enum Op {
    /// Text, written cell by cell (wide characters, clusters, emoji).
    Text(String),
    /// A pen: bold and a colour from a small set.
    Pen(u8),
    /// Opens hyperlink `n`, or closes the open one.
    Link(Option<u8>),
    /// A mark on the cursor's row, with an anchor column.
    Mark(u8, u16),
    /// CR LF.
    Newline,
    /// A resize to rows × cols: the re-wrap under test.
    Resize(u16, u16),
}

fn text() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        6 => "[a-z ]{1,12}",
        1 => Just("\u{4e2d}".to_owned()),
        1 => Just("e\u{301}".to_owned()),
        1 => Just("\u{1F468}\u{200D}\u{1F469}".to_owned()),
        1 => Just("   ".to_owned()),
    ];
    proptest::collection::vec(piece, 1..6).prop_map(|parts| parts.concat())
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => text().prop_map(Op::Text),
        2 => any::<u8>().prop_map(Op::Pen),
        1 => proptest::option::of(0u8..3).prop_map(Op::Link),
        1 => (0u8..4, 0u16..40).prop_map(|(m, a)| Op::Mark(m, a)),
        6 => Just(Op::Newline),
        1 => (2u16..7, 1u16..13).prop_map(|(r, c)| Op::Resize(r, c)),
    ]
}

fn mark(n: u8) -> Mark {
    match n {
        0 => Mark::PromptStart,
        1 => Mark::CommandStart,
        2 => Mark::OutputStart,
        _ => Mark::CommandEnd {
            exit_code: Some(i32::from(n)),
        },
    }
}

fn apply(grid: &mut Grid, op: &Op) {
    match op {
        Op::Text(text) => grid.put_str(text),
        Op::Pen(n) => {
            let mut style = Style {
                fg: Color::Indexed(n % 4),
                ..Style::DEFAULT
            };
            if n & 4 != 0 {
                style = style.with(Style::BOLD);
            }
            grid.set_pen(style);
        }
        Op::Link(Some(n)) => {
            let uri = format!("https://example.test/{n}");
            grid.begin_hyperlink(&uri, None)
                .expect("a short link opens");
        }
        Op::Link(None) => grid.end_hyperlink(),
        Op::Mark(m, anchor) => {
            let row = grid.cursor().row;
            if let Some(line) = grid.line_mut(row) {
                line.set_mark_at(mark(*m), *anchor);
            }
        }
        Op::Newline => {
            grid.carriage_return();
            grid.line_feed();
        }
        Op::Resize(rows, cols) => grid.resize(*rows, *cols),
    }
}

/// A grid of 4 × 10 whose history keeps `hot` rows unpacked.
fn grid_with_hot_rows(cap: usize, hot: usize) -> Grid {
    let mut grid = Grid::with_scrollback(4, 10, cap);
    *grid.scrollback_mut() = Scrollback::with_hot_rows(cap, hot);
    grid
}

fn lines(grid: &Grid) -> Vec<Line> {
    grid.logical_lines().map(LineRef::into_owned).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Packed history reads, evicts and re-wraps as unpacked history
    /// does: every line (cells, clusters, links, marks and anchors, PAD
    /// cells, wrap flags), the cursor and the history length match after
    /// every step; a random history row read after every step (through the
    /// decode cache) matches too, and both grids keep the same hyperlink
    /// entities alive (a cached evicted row must not pin its links).
    #[test]
    fn packed_history_matches_the_cell_path(
        ops in proptest::collection::vec((op(), any::<u16>()), 1..400),
        // Small caps evict within one block; large ones span several
        // 256-row blocks (`BLOCK_ROWS`) and drop whole blocks.
        cap in prop_oneof![8usize..120, 256usize..700],
        hot in 0usize..6,
    ) {
        let mut packed = grid_with_hot_rows(cap, hot);
        let mut cells = grid_with_hot_rows(cap, usize::MAX);
        for (i, (op, probe)) in ops.iter().enumerate() {
            apply(&mut packed, op);
            apply(&mut cells, op);
            if packed.scrollback_len() > 0 {
                let row = usize::from(*probe) % packed.scrollback_len();
                prop_assert_eq!(packed.scrollback_line(row), cells.scrollback_line(row), "step {} row {}", i, row);
            }
            prop_assert_eq!(packed.live_hyperlinks(), cells.live_hyperlinks(), "step {}", i);
            prop_assert_eq!(cells.scrollback_mut().packed_len(), 0);
            prop_assert_eq!(packed.scrollback_len(), cells.scrollback_len(), "step {}", i);
            prop_assert_eq!(packed.cursor(), cells.cursor(), "step {}", i);
            prop_assert_eq!(packed.pending_wrap(), cells.pending_wrap(), "step {}", i);
            if matches!(op, Op::Resize(..)) || i + 1 == ops.len() {
                prop_assert_eq!(lines(&packed), lines(&cells), "step {} {:?}", i, op);
                packed.check_invariants();
            }
        }
        // Random access through the decode cache agrees too.
        for i in (0..packed.scrollback_len()).rev() {
            prop_assert_eq!(packed.scrollback_line(i), cells.scrollback_line(i), "row {}", i);
        }
    }
}

/// A fixed session that packs, evicts and re-wraps (the proptest's sure
/// case): at least one block is packed and every line still matches.
#[test]
fn a_long_session_packs_and_rewraps_like_the_cell_path() {
    let mut packed = grid_with_hot_rows(300, 3);
    let mut cells = grid_with_hot_rows(300, usize::MAX);
    let script = [
        Op::Mark(0, 0),
        Op::Text("$ ls".into()),
        Op::Mark(2, 3),
        Op::Newline,
        Op::Link(Some(1)),
        Op::Text("link \u{4e2d}e\u{301}".into()),
        Op::Link(None),
        Op::Pen(5),
        Op::Text("bold \u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}".into()),
        Op::Pen(0),
        Op::Newline,
    ];
    for n in 0..200 {
        for op in &script {
            apply(&mut packed, op);
            apply(&mut cells, op);
        }
        if n % 50 == 49 {
            let size = [(3, 7), (5, 11), (2, 3), (4, 10)][n / 50];
            apply(&mut packed, &Op::Resize(size.0, size.1));
            apply(&mut cells, &Op::Resize(size.0, size.1));
            assert!(packed.scrollback_mut().packed_len() > 0, "rows are packed");
            assert_eq!(lines(&packed), lines(&cells), "after resize {n}");
        }
    }
    assert_eq!(lines(&packed), lines(&cells));
}
