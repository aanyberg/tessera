//! OSC 133 marks reported to the host as [`Request::Mark`] (#45a, ADR
//! 0039): the term-layer rows (`layer: T`, and the term half of `T+N`) of
//! the prompt navigation fixture, `prompts_spec.txt` in
//! `tessera-app-core/tests/fixtures`, which the milestone lead owns.
//!
//! Each row is one test named after its id; [`every_term_row_has_a_test`]
//! fails when the fixture gains a term row without one. The rows give the
//! input as prose, so the tests spell the bytes out.
//!
//! The marks on the lines are the same under every chunking; the requests
//! are coalesced per [`Terminal::advance`] call (the last mark of each
//! kind, in the order those last ones arrived), so they differ between
//! chunkings by design (decision D14). The unit tests of `osc.rs` check
//! the coalescing against a reference under random chunkings.

use std::time::Instant;

use tessera_grid::{Line, Mark};
use tessera_term::{MAX_MARK_REQUESTS, Request, Terminal};

const FIXTURE: &str = include_str!("../../tessera-app-core/tests/fixtures/prompts_spec.txt");

/// The rows this file covers, by id.
const COVERED: &[&str] = &[
    "N10_ps2_never_stops",
    "N12_marks_set_on_alt_screen_are_dropped",
    "T01_basic_cycle_one_chunk_is_coalesced",
    "T02_basic_cycle_bytewise",
    "T03_flood_100000_sequences",
    "T04_flood_all_kinds",
    "T05_flood_with_scrolling_lines_are_per_mark",
    "T06_coalesce_keeps_arrival_order_of_last_occurrences",
    "T07_non_marks_emit_nothing",
    "T08_nothing_is_ever_replied",
    "T09_osc_633_not_handled",
    "T10_iterm2_setmark_not_handled",
    "T11_mark_on_wrap_pending_and_last_column",
    "T12_line_id_values",
    "T13_scroll_region_does_not_count",
];

const A: Mark = Mark::PromptStart;
const B: Mark = Mark::CommandStart;
const C: Mark = Mark::OutputStart;

fn d(exit_code: Option<i32>) -> Mark {
    Mark::CommandEnd { exit_code }
}

/// The bytes of T01 and T02: a whole zsh-style cycle.
const CYCLE: &[u8] = b"\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n\
\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07";

/// Every payload of the rows that must make no mark (T07, T09, T10).
const NOT_MARKS: &[&[u8]] = &[
    b"\x1b]133;L\x07\x1b]133;N\x07\x1b]133;P\x07\x1b]133;I\x07\x1b]133;a\x07\x1b]133;\x07\
\x1b]133;A;k=s\x07\x1b]133;A;k=c\x07\x1b]133;A;k=r\x07\x1b]133;Afoo\x07\x1b]133;D0\x07",
    b"\x1b]633;A\x07\x1b]633;B\x07\x1b]633;C\x07\x1b]633;D;0\x07\x1b]633;E;ls\x3b\x20-l\x07\
\x1b]633;P;Cwd=/tmp\x07",
    b"\x1b]1337;SetMark\x07",
];

/// The mark requests among `requests`, as `(mark, line)`; panics on any
/// other request.
fn marks_only(requests: Vec<Request>) -> Vec<(Mark, u64)> {
    requests
        .into_iter()
        .map(|request| match request {
            Request::Mark { mark, line } => (mark, line),
            other => panic!("only Request::Mark expected, got {other:?}"),
        })
        .collect()
}

/// A `rows` x `cols` terminal after one `advance` of `bytes`, and the
/// marks it reported.
fn fed(rows: u16, cols: u16, bytes: &[u8]) -> (Terminal, Vec<(Mark, u64)>) {
    let mut term = Terminal::new(rows, cols);
    term.advance(bytes);
    let requests = term.take_requests();
    (term, marks_only(requests))
}

/// Every line's marks, scrollback first.
fn grid_marks(term: &Terminal) -> Vec<Vec<Mark>> {
    let grid = term.grid();
    (0..grid.scrollback_len())
        .filter_map(|i| grid.scrollback_line(i))
        .chain(grid.visible_lines())
        .map(|line: &Line| line.marks.clone())
        .collect()
}

/// The marks of the screen rows that have any, as `(row, marks)`.
fn screen_marks(term: &Terminal) -> Vec<(usize, Vec<Mark>)> {
    term.grid()
        .visible_lines()
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.marks.is_empty())
        .map(|(row, line)| (row, line.marks.clone()))
        .collect()
}

#[test]
fn every_term_row_has_a_test() {
    let mut rows = Vec::new();
    let mut id = None;
    for line in FIXTURE.lines() {
        if let Some(rest) = line.strip_prefix("### ") {
            id = Some(rest.trim());
        } else if let (Some(row), Some(layer)) = (id, line.strip_prefix("layer: ")) {
            if layer.split(['+', ' ']).any(|l| l == "T") {
                rows.push(row);
            }
            id = None;
        }
    }
    rows.sort_unstable();
    let mut covered = COVERED.to_vec();
    covered.sort_unstable();
    assert_eq!(rows, covered, "term rows of the fixture and tests here");
}

#[test]
fn t01_basic_cycle_one_chunk_is_coalesced() {
    let (term, marks) = fed(5, 20, CYCLE);
    assert_eq!(marks, [(C, 1), (d(Some(0)), 2), (A, 2), (B, 2)]);
    assert_eq!(
        screen_marks(&term),
        [(0, vec![A, B]), (1, vec![C]), (2, vec![d(Some(0)), A, B])]
    );
}

#[test]
fn t02_basic_cycle_bytewise() {
    let mut term = Terminal::new(5, 20);
    let mut marks = Vec::new();
    for byte in CYCLE {
        term.advance(std::slice::from_ref(byte));
        let call = marks_only(term.take_requests());
        assert!(call.len() <= 1, "one sequence ends per byte");
        marks.extend(call);
    }
    assert_eq!(
        marks,
        [(A, 0), (B, 0), (C, 1), (d(Some(0)), 2), (A, 2), (B, 2)]
    );
    let (whole, _) = fed(5, 20, CYCLE);
    assert_eq!(grid_marks(&term), grid_marks(&whole));
}

/// T01's bytes split in two anywhere: the grid marks are the whole run's,
/// and each call reports at most one mark per kind.
#[test]
fn t02_every_split_gives_the_grid_marks_of_the_whole_run() {
    let (whole, _) = fed(5, 20, CYCLE);
    for at in 0..=CYCLE.len() {
        let mut term = Terminal::new(5, 20);
        let (head, tail) = CYCLE.split_at(at);
        let mut reported = Vec::new();
        for part in [head, tail] {
            term.advance(part);
            let call = marks_only(term.take_requests());
            assert!(call.len() <= MAX_MARK_REQUESTS, "split at {at}");
            reported.extend(call);
        }
        assert_eq!(grid_marks(&term), grid_marks(&whole), "split at {at}");
        // The last mark reported is always the last one parsed.
        assert_eq!(reported.last(), Some(&(B, 2)), "split at {at}");
    }
}

#[test]
fn t03_flood_100000_sequences() {
    let bytes = b"\x1b]133;A\x07".repeat(100_000);
    let start = Instant::now();
    let (term, marks) = fed(24, 80, &bytes);
    eprintln!("100 000 x OSC 133;A in one advance: {:?}", start.elapsed());
    assert_eq!(marks, [(A, 0)], "exactly one request (S6)");
    assert_eq!(
        screen_marks(&term),
        [(0, vec![A])],
        "one mark per kind per line"
    );
}

#[test]
fn t04_flood_all_kinds() {
    let unit = b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;1\x07";
    let (term, marks) = fed(24, 80, &unit.repeat(25_000));
    assert_eq!(marks, [(A, 0), (B, 0), (C, 0), (d(Some(1)), 0)]);
    assert!(marks.len() <= MAX_MARK_REQUESTS);
    assert_eq!(screen_marks(&term), [(0, vec![A, B, C, d(Some(1))])]);
}

/// A flood that scrolls: the line is read when each sequence is parsed,
/// so the request names the last mark's line, not the first one's.
#[test]
fn t05_flood_with_scrolling_lines_are_per_mark() {
    let mut bytes = b"\x1b]133;A\x07".to_vec();
    bytes.extend(b"\n".repeat(50));
    bytes.extend(b"\x1b]133;A\x07");
    let (term, marks) = fed(5, 20, &bytes);
    let pushed = term.grid().scrollback_pushed();
    assert_eq!(pushed, 46);
    assert_eq!(marks, [(A, pushed + 4)]);
    // Many lines, one mark per line, many lines per call.
    let mut flood = Vec::new();
    for _ in 0..10_000 {
        flood.extend(b"\x1b]133;A\x07\x1b]133;C\x07\r\n");
    }
    let (term, marks) = fed(5, 20, &flood);
    let last = term.grid().scrollback_pushed() + 3;
    assert_eq!(marks, [(A, last), (C, last)]);
}

#[test]
fn t06_coalesce_keeps_arrival_order_of_last_occurrences() {
    let bytes = b"\x1b]133;A\x07\x1b]133;B\x07\r\n\x1b]133;A\x07\r\n\x1b]133;C\x07";
    let (_, marks) = fed(5, 20, bytes);
    assert_eq!(marks, [(B, 0), (A, 1), (C, 2)]);
}

#[test]
fn t07_non_marks_emit_nothing() {
    let (term, marks) = fed(5, 20, NOT_MARKS[0]);
    assert_eq!(marks, []);
    assert!(grid_marks(&term).iter().all(Vec::is_empty));
}

/// Nothing an OSC 133 payload holds is ever answered (S1, S2): every
/// payload above, the options Tessera ignores and a 5000-byte option
/// string produce mark requests only.
#[test]
fn t08_nothing_is_ever_replied() {
    let long = format!("\x1b]133;A;{}\x07", "x".repeat(5000));
    let mut payloads: Vec<&[u8]> = vec![
        CYCLE,
        b"\x1b]133;A;cl=line;aid=1;redraw=0;err=1\x07",
        b"\x1b]133;D;99999999999\x07\x1b]133;D;abc\x07\x1b]133;D;;aid=1\x07",
        long.as_bytes(),
    ];
    payloads.extend(NOT_MARKS);
    for payload in payloads {
        for bytewise in [false, true] {
            let mut term = Terminal::new(5, 20);
            if bytewise {
                for byte in payload {
                    term.advance(std::slice::from_ref(byte));
                }
            } else {
                term.advance(payload);
            }
            let requests = term.take_requests();
            assert!(
                requests
                    .iter()
                    .all(|request| matches!(request, Request::Mark { .. })),
                "{:?}: {requests:?}",
                String::from_utf8_lossy(payload)
            );
        }
    }
    // The options do not stop the mark.
    let (_, marks) = fed(5, 20, b"\x1b]133;A;cl=line;aid=1;redraw=0;err=1\x07");
    assert_eq!(marks, [(A, 0)]);
}

#[test]
fn t09_osc_633_not_handled() {
    let (term, marks) = fed(5, 20, NOT_MARKS[1]);
    assert_eq!(marks, []);
    assert!(grid_marks(&term).iter().all(Vec::is_empty));
}

#[test]
fn t10_iterm2_setmark_not_handled() {
    let (term, marks) = fed(5, 20, NOT_MARKS[2]);
    assert_eq!(marks, []);
    assert!(grid_marks(&term).iter().all(Vec::is_empty));
}

#[test]
fn t11_mark_on_wrap_pending_and_last_column() {
    let (term, marks) = fed(3, 4, b"abcd\x1b]133;A\x07");
    assert_eq!(marks, [(A, 0)]);
    assert_eq!(screen_marks(&term), [(0, vec![A])]);
    assert!(term.grid().pending_wrap(), "the pending wrap is left alone");
    assert_eq!(term.grid().cursor().row, 0);
}

#[test]
fn t12_line_id_values() {
    let mut term = Terminal::new(3, 10);
    term.set_max_scrollback(100);
    term.advance(&b"x\r\n".repeat(10));
    term.advance(b"\x1b]133;A\x07");
    assert_eq!(term.grid().scrollback_pushed(), 8);
    assert_eq!(marks_only(term.take_requests()), [(A, 10)]);
}

/// Lines leaving a partial scroll region never enter history, so they do
/// not move the ids.
#[test]
fn t13_scroll_region_does_not_count() {
    let mut bytes = b"\x1b[2;4r\x1b[4;1H".to_vec();
    bytes.extend(b"\n".repeat(20));
    bytes.extend(b"\x1b]133;A\x07");
    let (term, marks) = fed(5, 10, &bytes);
    assert_eq!(term.grid().scrollback_pushed(), 0);
    assert_eq!(marks, [(A, 3)]);
}

/// The term half of N10: a secondary prompt (`A;k=s`) makes no mark and
/// no request, and the `B` after it still does.
#[test]
fn n10_ps2_never_stops() {
    let bytes = b"\x1b]133;A\x07$ \x1b]133;B\x07for x in 1; do\r\n\x1b]133;A;k=s\x07> \
\x1b]133;B\x07echo $x; done\r\n\x1b]133;C\x07";
    let (term, marks) = fed(5, 40, bytes);
    assert_eq!(
        screen_marks(&term),
        [(0, vec![A, B]), (1, vec![B]), (2, vec![C])]
    );
    assert_eq!(marks, [(A, 0), (B, 1), (C, 2)]);
}

/// Marks on the alternate screen are dropped, and so are their requests.
#[test]
fn n12_marks_set_on_alt_screen_are_dropped() {
    let (term, marks) = fed(
        5,
        20,
        b"\x1b[?1049h\x1b]133;A\x07\x1b]133;C\x07\x1b]133;D;0\x07\x1b[?1049l",
    );
    assert_eq!(marks, []);
    assert!(grid_marks(&term).iter().all(Vec::is_empty));
}

/// The ids are only meaningful within one [`Terminal::line_epoch`]: a
/// resize to another size bumps it, and so does a clear to the cursor that moved rows up;
/// output never does.
#[test]
fn the_line_epoch_counts_what_moves_rows_uncounted() {
    let mut term = Terminal::new(5, 20);
    term.advance(&b"x\r\n".repeat(50));
    assert_eq!(term.line_epoch(), 0, "output does not bump it");
    term.resize(5, 20);
    assert_eq!(term.line_epoch(), 0, "the same size moves nothing");
    term.resize(6, 20);
    assert_eq!(term.line_epoch(), 1, "any resize, the height alone too");
    term.resize(6, 30);
    assert_eq!(term.line_epoch(), 2);
    // The cursor is on row 0 after this: nothing moves.
    term.advance(b"\x1b[H");
    term.clear_to_cursor();
    assert_eq!(term.line_epoch(), 2, "a clear that kept row 0");
    term.advance(b"\x1b[4;1H\x1b]133;A\x07");
    term.clear_to_cursor();
    assert_eq!(term.grid().cursor().row, 0);
    assert_eq!(term.line_epoch(), 3, "a clear that moved the prompt row up");
}
