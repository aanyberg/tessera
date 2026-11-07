//! The engine against a plain-text oracle on long logical lines of
//! multi-byte text (#46a, critic finding B1): a segment whose text is longer
//! than [`MAX_HAYSTACK_BYTES`] must still be searched to its end.
//!
//! The oracle builds each segment's text straight from the cells, with no
//! byte cap, and runs the regex on it; it shares nothing with the engine's
//! haystack but the documented row cuts ([`segment_period`]).

use std::ops::ControlFlow;

use proptest::prelude::*;
use regex::Regex;
use tessera_grid::Line;
use tessera_search::{
    Haystack, MAX_HAYSTACK_BYTES, Match, Query, QueryOptions, Rows, scan, segment_period,
};
use tessera_term::Terminal;

/// Most bytes one cell's text can take: a cluster of `MAX_CLUSTER_SCALARS`
/// four-byte scalars.
const MAX_CELL_BYTES: usize = 4 * tessera_grid::MAX_CLUSTER_SCALARS;

/// A match as `[start_row, start_col, end_row, end_col]`, `end_col`
/// exclusive on the row of the last cell.
type Found = [u64; 4];

/// `input` fed to a terminal of `rows` rows and `cols` columns.
fn terminal(rows: usize, cols: u16, input: &str) -> Terminal {
    let mut terminal = Terminal::new(u16::try_from(rows).unwrap(), cols);
    terminal.advance(input.as_bytes());
    terminal
}

/// The engine's matches of `pattern` (a regex) over the screen, as the
/// application core searches the rows it copies.
fn engine(terminal: &Terminal, pattern: &str) -> Vec<Found> {
    let query = Query::new(
        pattern,
        QueryOptions {
            regex: true,
            case_sensitive: true,
        },
    )
    .unwrap()
    .unwrap();
    let grid = terminal.grid();
    let mut found = Vec::new();
    let at = Rows {
        first: 0,
        run_above: 0,
        more_below: false,
    };
    let _ = scan(
        &query,
        grid.visible_lines(),
        at,
        segment_period(grid.cols()),
        &mut Haystack::new(),
        |m: Match| {
            found.push([
                m.start_line,
                u64::from(m.start_col),
                m.end_line,
                u64::from(m.end_col),
            ]);
            ControlFlow::Continue(())
        },
    );
    found.sort_unstable();
    found
}

/// Where one cell's text starts in a segment's text.
struct CellAt {
    byte: usize,
    row: usize,
    col: usize,
    width: usize,
}

/// The oracle: the matches of `pattern` in the uncapped text of every
/// segment of `lines`, and each segment's text length in bytes.
///
/// A logical line is cut at each row that is a multiple of `period` and at
/// least `period` rows below the line's first row (ADR 0038). A row's text
/// is its cells' text (a wide character once, a cluster whole, spacers and
/// row-end padding nothing); a row that ends its logical line loses its
/// trailing blanks, by [`Line::text`]'s rule.
fn oracle(lines: &[Line], period: usize, pattern: &Regex) -> (Vec<Found>, Vec<usize>) {
    let mut found = Vec::new();
    let mut sizes = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        let line_start = start;
        let mut end = start + 1;
        while lines[end - 1].wrapped && end < lines.len() {
            end += 1;
        }
        // Cut the logical line `line_start..end` into segments.
        let mut cuts = vec![line_start];
        for row in line_start + 1..end {
            if row % period == 0 && row - line_start >= period {
                cuts.push(row);
            }
        }
        cuts.push(end);
        for pair in cuts.windows(2) {
            let (text, cells) = segment_text(&lines[pair[0]..pair[1]], pair[0]);
            sizes.push(text.len());
            for m in pattern.find_iter(&text).filter(|m| !m.is_empty()) {
                let cell = |byte: usize| &cells[cells.partition_point(|c| c.byte <= byte) - 1];
                let (first, last) = (cell(m.start()), cell(m.end() - 1));
                found.push([
                    first.row as u64,
                    first.col as u64,
                    last.row as u64,
                    (last.col + last.width) as u64,
                ]);
            }
        }
        start = end;
    }
    found.sort_unstable();
    (found, sizes)
}

/// The text of `rows` (absolute rows from `first`) and where each cell's
/// text starts in it.
fn segment_text(rows: &[Line], first: usize) -> (String, Vec<CellAt>) {
    let mut text = String::new();
    let mut cells = Vec::new();
    for (index, line) in rows.iter().enumerate() {
        let end = if line.wrapped {
            line.cells.len()
        } else {
            // The columns Line::text keeps.
            let kept = line.text();
            let mut chars = 0;
            let mut col = 0;
            while chars < kept.len() {
                let cell = &line.cells[col];
                if !cell.is_spacer() {
                    chars += match line.cluster(col) {
                        Some(cluster) if cell.is_cluster() => cluster.len(),
                        _ => cell.ch.len_utf8(),
                    };
                }
                col += 1;
            }
            col
        };
        for (col, cell) in line.cells[..end].iter().enumerate() {
            if cell.is_spacer() || cell.is_pad() {
                continue;
            }
            cells.push(CellAt {
                byte: text.len(),
                row: first + index,
                col,
                width: usize::from(cell.width().max(1)),
            });
            match line.cluster(col) {
                Some(cluster) if cell.is_cluster() => text.push_str(cluster),
                _ => text.push(cell.ch),
            }
        }
    }
    (text, cells)
}

/// A screen of `rows` rows at `cols` columns holding `input`, searched for
/// `pattern` by the engine and by the oracle.
fn both(rows: usize, cols: u16, input: &str, pattern: &str) -> (Vec<Found>, Vec<Found>) {
    let terminal = terminal(rows, cols, input);
    let (want, _) = oracle(
        terminal.grid().visible_lines(),
        segment_period(cols),
        &Regex::new(pattern).unwrap(),
    );
    (engine(&terminal, pattern), want)
}

/// B1 repro: box drawing (3 bytes a cell) at 2048 columns, one 15-row
/// logical line (one segment of 92 KiB), `XYZ` on its last row.
#[test]
fn b1_box_drawing_at_2048_columns() {
    let input = format!("{}XYZ", "\u{2500}".repeat(2048 * 14));
    let (got, want) = both(40, 2048, &input, "XYZ");
    assert_eq!(want, [[14, 0, 14, 3]]);
    assert_eq!(got, want);
}

/// B1 repro: Cyrillic (2 bytes a cell) at 300 columns, a 200-row logical
/// line from row 1 (one segment of 117 KiB).
#[test]
fn b1_cyrillic_at_300_columns() {
    let input = format!("x\r\n{}XYZ", "\u{434}".repeat(300 * 199));
    let (got, want) = both(220, 300, &input, "XYZ");
    assert_eq!(want, [[200, 0, 200, 3]]);
    assert_eq!(got, want);
}

/// B1 repro: a four-byte narrow character (U+1D400) at 80 columns, a
/// 250-row logical line from row 1 (one segment of 78 KiB).
#[test]
fn b1_four_byte_characters_at_80_columns() {
    let input = format!("x\r\n{}XYZ", "\u{1d400}".repeat(80 * 249));
    let (got, want) = both(260, 80, &input, "XYZ");
    assert_eq!(want, [[250, 0, 250, 3]]);
    assert_eq!(got, want);
}

/// B1 repro: cells of `e` and 30 combining marks (61 bytes a cell) at 80
/// columns, a 100-row logical line with `XYZ` at the start of rows 20, 50
/// and 99: before the fix only the first 13 rows were searched.
#[test]
fn b1_combining_marks_at_80_columns() {
    let mark = format!("e{}", "\u{301}".repeat(30));
    let mut input = String::new();
    for row in 0..100 {
        if [20, 50, 99].contains(&row) {
            input.push_str("XYZ");
            input.push_str(&mark.repeat(77));
        } else {
            input.push_str(&mark.repeat(80));
        }
    }
    let (got, want) = both(110, 80, &input, "XYZ");
    assert_eq!(want, [[20, 0, 20, 3], [50, 0, 50, 3], [99, 0, 99, 3]]);
    assert_eq!(got, want);
}

/// The pieces of a segment past the cap meet behind cut markers, as at a
/// segment cut: `^` and `$` still anchor only to the logical line's ends.
#[test]
fn b1_pieces_meet_behind_cut_markers() {
    let mark = format!("e{}", "\u{301}".repeat(30));
    for (cols, rows, cell) in [(80, 100, mark.as_str()), (300, 200, "\u{434}")] {
        let input = format!("x\r\n{}", cell.repeat(usize::from(cols) * rows));
        let terminal = terminal(rows + 3, cols, &input);
        let last = rows as u64;
        let end = u64::from(cols);
        assert_eq!(engine(&terminal, "^."), [[0, 0, 0, 1], [1, 0, 1, 1]]);
        assert_eq!(
            engine(&terminal, ".$"),
            [[0, 0, 0, 1], [last, end - 1, last, end]]
        );
    }
}

/// Filler text of one logical line, and the columns one unit of it takes.
const FILLERS: &[(&str, usize)] = &[
    ("a", 1),
    ("\u{434}", 1),
    ("\u{2500}", 1),
    ("\u{1d400}", 1),
    ("\u{4e2d}", 2),
    ("e\u{301}", 1),
    (
        "e\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}",
        1,
    ),
    ("a\u{4e2d}e\u{301}\u{1d400}", 5),
];

/// One logical line: a filler, its length in rows and the needles (`Q`
/// or `XYZ`) placed at fractions of its length.
#[derive(Debug, Clone)]
struct Spec {
    filler: usize,
    rows: usize,
    needles: Vec<(u16, bool)>,
}

/// Most bytes of filler text in one generated line, so a debug run stays
/// quick while multi-byte lines still pass the cap several times over.
const MAX_LINE_BYTES: usize = 400 << 10;

/// The input of one logical line at `cols` columns, and the rows it takes
/// at most.
fn line_input(spec: &Spec, cols: u16) -> (String, usize) {
    let (unit, width) = FILLERS[spec.filler];
    let units = (spec.rows * usize::from(cols) / width)
        .min(MAX_LINE_BYTES / unit.len())
        .max(1);
    let mut at: Vec<(usize, bool)> = spec
        .needles
        .iter()
        .map(|&(fraction, long)| (units * usize::from(fraction) / 65_536, long))
        .collect();
    at.sort_unstable();
    let mut input = String::new();
    let mut next = at.iter().peekable();
    for index in 0..units {
        while let Some(&(_, long)) = next.next_if(|&&(place, _)| place == index) {
            input.push_str(if long { "XYZ" } else { "Q" });
        }
        input.push_str(unit);
    }
    // Every unit and needle cell, plus a padding cell a row for wide text.
    let cells = units * width + 3 * at.len();
    (
        input,
        cells / usize::from(cols) + cells / usize::from(cols) / 2 + 2,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Every engine match is an oracle match; every single-cell match
    /// (`Q`) is found; and a three-cell match (`XYZ`) goes missing only
    /// where a segment's text is cut into pieces, at most once a piece.
    #[test]
    fn the_engine_agrees_with_a_plain_text_oracle(
        cols in prop::sample::select(vec![7u16, 80, 300, 2047, 2048]),
        specs in prop::collection::vec(
            (
                0..FILLERS.len(),
                1usize..=384,
                prop::collection::vec((any::<u16>(), any::<bool>()), 0..8),
            )
                .prop_map(|(filler, rows, needles)| Spec { filler, rows, needles }),
            1..3,
        ),
    ) {
        let period = segment_period(cols);
        let mut input = String::new();
        let mut rows = 1;
        for spec in &specs {
            let spec = Spec { rows: spec.rows.min(3 * period), ..spec.clone() };
            let (line, height) = line_input(&spec, cols);
            input.push_str(&line);
            input.push_str("\r\n");
            rows += height;
        }
        let terminal = terminal(rows, cols, &input);
        let pattern = "Q|XYZ";
        let (want, sizes) =
            oracle(terminal.grid().visible_lines(), period, &Regex::new(pattern).unwrap());
        let got = engine(&terminal, pattern);
        prop_assert!(got.windows(2).all(|w| w[0] != w[1]), "a match found twice");
        for m in &got {
            prop_assert!(want.binary_search(m).is_ok(), "{m:?} is not an oracle match");
        }
        let missed: Vec<&Found> = want.iter().filter(|m| got.binary_search(m).is_err()).collect();
        for m in &missed {
            prop_assert!(!(m[0] == m[2] && m[3] == m[1] + 1), "single-cell {m:?} missed");
        }
        // A piece holds at least the cap less one cell.
        let cuts: usize = sizes
            .iter()
            .map(|&size| size / (MAX_HAYSTACK_BYTES - MAX_CELL_BYTES))
            .sum();
        prop_assert!(
            missed.len() <= cuts,
            "{} missed, {cuts} piece cuts: {missed:?}",
            missed.len()
        );
    }
}
