//! OSC 8 hyperlinks (#30 part 2, ADR 0026).
//!
//! Expected behaviour comes from `tests/fixtures/hyperlink_spec.txt`, which
//! cites VTE, kitty, `WezTerm`, Ghostty and the OSC 8 specification per row;
//! only the milestone lead edits it. The fixture has its own layout, read
//! here (the row layout of `osc_spec.txt` plus link keys):
//!
//! - `### <id>` starts a row; `size: RxC` gives the terminal size and
//!   `scrollback: N` its history cap;
//! - `step: feed <payload>` is one [`Terminal::advance`] call (the payload
//!   is everything after the one space, decoded by [`common::decode`] after
//!   expanding `@rep(C,N)`), `step: resize RxC` one [`Terminal::resize`];
//! - `links: (r,c)=uri[#id] (r,c0-c1)=uri ...` lists the cells of the
//!   visible screen that carry a link and its URI and `id`; every cell not
//!   listed must carry none, and `none` says no cell does;
//! - `links_scrollback: s0:(c0-c1)=uri[#id] ...` the same for the history
//!   (`s0` the oldest line; unlisted history cells carry none; a row
//!   without the key does not assert the history);
//! - `active:` the grid's open link after the last step: `none` or
//!   `uri[#id]`;
//! - `same: (r,c)(r,c)...` lists cells that must belong to one link entity
//!   and `distinct:` cells that must belong to different ones;
//! - `rejected:`, `cite:` and `note:` are documentation.
//!
//! [`every_row`] runs every row under every chunking the fixture header
//! prescribes: the steps whole (each `feed` one call), byte by byte and in
//! two halves split at every offset, with resizes applied at the offsets
//! where they stand in the input. Each run must meet the row's expectations
//! and give the same outcome (links, the entity grouping of the cells and
//! the open link) as the whole run.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;

use std::collections::BTreeSet;
use std::fmt::Write as _;

use common::{check_table, decode};
use tessera_grid::{Grid, Hyperlink, Line};
use tessera_term::Terminal;

const FIXTURE: &str = include_str!("fixtures/hyperlink_spec.txt");

/// Rows in the fixture.
const ROWS: usize = 39;

/// One `step:` line.
#[derive(Debug, Clone)]
enum Step {
    Feed(Vec<u8>),
    Resize(u16, u16),
}

/// A link as the fixture writes it: URI and optional id.
type Spec = (String, Option<String>);

/// One cell range of a `links:` entry: columns `c0..=c1` of `row`.
#[derive(Debug, Clone)]
struct Run {
    row: usize,
    cols: (usize, usize),
    link: Spec,
}

/// One row of the fixture.
#[derive(Debug, Clone, Default)]
struct Row {
    id: String,
    size: (u16, u16),
    scrollback: Option<usize>,
    steps: Vec<Step>,
    links: Vec<Run>,
    /// `None` when the row has no `links_scrollback:` line: the history is
    /// then not asserted (L44 scrolls a linked row into it without saying
    /// so).
    links_scrollback: Option<Vec<Run>>,
    active: Option<Spec>,
    same: Vec<Vec<(usize, usize)>>,
    distinct: Vec<Vec<(usize, usize)>>,
}

/// Expands `@rep(C,N)` (`N` copies of the ASCII character `C`) in `text`,
/// decoding the pieces around the macros with [`decode`].
fn expand(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("@rep(") {
        out.extend(decode(&rest[..at]));
        let (inner, after) = rest[at + "@rep(".len()..]
            .split_once(')')
            .unwrap_or_else(|| panic!("unclosed macro in {text:?}"));
        let (ch, count) = inner
            .split_once(',')
            .unwrap_or_else(|| panic!("`{inner}`: expected C,N"));
        let &[byte] = ch.as_bytes() else {
            panic!("`{inner}`: C must be one ASCII character");
        };
        out.extend(std::iter::repeat_n(
            byte,
            count.parse().expect("macro count"),
        ));
        rest = after;
    }
    out.extend(decode(rest));
    out
}

fn expand_str(text: &str) -> String {
    String::from_utf8(expand(text)).expect("fixture text is UTF-8")
}

fn parse_size(text: &str) -> (u16, u16) {
    let parsed = text
        .trim()
        .split_once('x')
        .and_then(|(rows, cols)| Some((rows.parse().ok()?, cols.parse().ok()?)));
    parsed.unwrap_or_else(|| panic!("`{text}`: expected RxC"))
}

/// `uri` or `uri#id`, macros expanded.
fn parse_spec(text: &str) -> Spec {
    match text.split_once('#') {
        Some((uri, id)) => (expand_str(uri), Some(expand_str(id))),
        None => (expand_str(text), None),
    }
}

/// `(r,c)` or `(r,c0-c1)` (and without a row, `(c0-c1)`), the text between
/// the parentheses.
fn parse_cols(text: &str) -> (usize, usize) {
    if let Some((a, b)) = text.split_once('-') {
        (a.parse().expect("column"), b.parse().expect("column"))
    } else {
        let c = text.parse().expect("column");
        (c, c)
    }
}

/// `(r,c)=uri[#id] (r,c0-c1)=uri ...`, or `none`.
fn parse_runs(text: &str) -> Vec<Run> {
    let text = text.trim();
    if text == "none" {
        return Vec::new();
    }
    text.split_whitespace()
        .map(|entry| {
            let (cell, link) = entry
                .split_once('=')
                .unwrap_or_else(|| panic!("`{entry}`: expected (r,c)=uri"));
            let inner = cell
                .strip_prefix('(')
                .and_then(|c| c.strip_suffix(')'))
                .unwrap_or_else(|| panic!("`{entry}`: expected (r,c)"));
            let (row, cols) = inner
                .split_once(',')
                .unwrap_or_else(|| panic!("`{entry}`: expected (r,c)"));
            Run {
                row: row.parse().expect("row"),
                cols: parse_cols(cols),
                link: parse_spec(link),
            }
        })
        .collect()
}

/// `s0:(c0-c1)=uri[#id] ...`, or `none`.
fn parse_history_runs(text: &str) -> Vec<Run> {
    let text = text.trim();
    if text == "none" {
        return Vec::new();
    }
    text.split_whitespace()
        .map(|entry| {
            let (line, rest) = entry
                .strip_prefix('s')
                .and_then(|e| e.split_once(':'))
                .unwrap_or_else(|| panic!("`{entry}`: expected sN:(c0-c1)=uri"));
            let (cols, uri) = rest
                .split_once('=')
                .unwrap_or_else(|| panic!("`{entry}`: expected sN:(c0-c1)=uri"));
            let cols = cols
                .strip_prefix('(')
                .and_then(|c| c.strip_suffix(')'))
                .unwrap_or_else(|| panic!("`{entry}`: expected (c0-c1)"));
            Run {
                row: line.parse().expect("history line"),
                cols: parse_cols(cols),
                link: parse_spec(uri),
            }
        })
        .collect()
}

/// `(r,c)(r,c)...` groups, one group per call.
fn parse_cells(text: &str) -> Vec<(usize, usize)> {
    text.split(')')
        .filter(|piece| !piece.trim().is_empty())
        .map(|piece| {
            let (row, col) = piece
                .trim()
                .strip_prefix('(')
                .and_then(|p| p.split_once(','))
                .unwrap_or_else(|| panic!("`{text}`: expected (r,c)"));
            (row.parse().expect("row"), col.parse().expect("col"))
        })
        .collect()
}

fn raw_rows(text: &str) -> Vec<(String, Vec<&str>)> {
    let mut rows: Vec<(String, Vec<&str>)> = Vec::new();
    for line in text.lines() {
        if let Some(id) = line.strip_prefix("### ") {
            rows.push((id.trim().to_owned(), Vec::new()));
        } else if let Some((_, lines)) = rows.last_mut() {
            lines.push(line);
        }
    }
    rows
}

fn parse_row(id: String, lines: &[&str]) -> Row {
    let mut row = Row {
        id,
        ..Row::default()
    };
    let mut size = None;
    for line in lines {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("{}: unexpected line `{line}`", row.id));
        // One space after the colon; the rest is the value, untrimmed.
        let value = value.strip_prefix(' ').unwrap_or(value);
        match key {
            "size" => size = Some(parse_size(value)),
            "scrollback" => row.scrollback = Some(value.trim().parse().expect("scrollback")),
            "step" => {
                if let Some(payload) = value.strip_prefix("feed ") {
                    row.steps.push(Step::Feed(expand(payload)));
                } else if let Some(size) = value.strip_prefix("resize ") {
                    let (rows, cols) = parse_size(size);
                    row.steps.push(Step::Resize(rows, cols));
                } else {
                    panic!("{}: unknown step `{value}`", row.id);
                }
            }
            "links" => row.links = parse_runs(value),
            "links_scrollback" => row.links_scrollback = Some(parse_history_runs(value)),
            "active" => {
                row.active = match value.trim() {
                    "none" => None,
                    spec => Some(parse_spec(spec)),
                };
            }
            "same" => row.same.push(parse_cells(value)),
            "distinct" => row.distinct.push(parse_cells(value)),
            "rejected" | "cite" | "note" => {}
            other => panic!("{}: unknown key `{other}`", row.id),
        }
    }
    row.size = size.unwrap_or_else(|| panic!("{}: no size", row.id));
    assert!(!row.steps.is_empty(), "{}: no step", row.id);
    row
}

/// How a row's input is cut into [`Terminal::advance`] calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunking {
    /// One call per `feed` step.
    Whole,
    /// One call per byte.
    Bytewise,
    /// Two calls, split before this byte offset.
    Split(usize),
}

fn chunkings(len: usize) -> Vec<Chunking> {
    let mut out = vec![Chunking::Whole, Chunking::Bytewise];
    out.extend((1..len).map(Chunking::Split));
    out
}

/// The row's input and where each step's work goes: the feed bytes joined,
/// the offsets where a `feed` ends, and the resizes with the offset they
/// follow.
/// A resize and the offset of the input it follows.
type ResizeAt = (usize, u16, u16);

fn plan(row: &Row) -> (Vec<u8>, Vec<usize>, Vec<ResizeAt>) {
    let mut input = Vec::new();
    let mut ends = Vec::new();
    let mut resizes = Vec::new();
    for step in &row.steps {
        match step {
            Step::Feed(bytes) => {
                input.extend_from_slice(bytes);
                ends.push(input.len());
            }
            Step::Resize(rows, cols) => resizes.push((input.len(), *rows, *cols)),
        }
    }
    (input, ends, resizes)
}

fn run(row: &Row, chunking: Chunking) -> Terminal {
    let (input, ends, resizes) = plan(row);
    let mut cuts: BTreeSet<usize> = BTreeSet::new();
    match chunking {
        Chunking::Whole => cuts.extend(ends),
        Chunking::Bytewise => cuts.extend(1..input.len()),
        Chunking::Split(at) => {
            cuts.insert(at);
        }
    }
    cuts.extend(resizes.iter().map(|&(at, _, _)| at));
    cuts.insert(input.len());
    let mut term = Terminal::new(row.size.0, row.size.1);
    if let Some(cap) = row.scrollback {
        term.set_max_scrollback(cap);
    }
    // A resize runs before the bytes that follow it: each cut applies the
    // ones standing at its start, and those after the last byte run last.
    let mut at = 0;
    for cut in cuts {
        for &(_, rows, cols) in resizes.iter().filter(|r| r.0 == at) {
            term.resize(rows, cols);
        }
        term.advance(&input[at..cut]);
        at = cut;
    }
    for &(_, rows, cols) in resizes.iter().filter(|r| r.0 == at) {
        term.resize(rows, cols);
    }
    term
}

/// A cell's link as the row's expectations write it.
fn spec_of(link: &Hyperlink) -> Spec {
    (link.uri().to_owned(), link.id().map(str::to_owned))
}

/// The links of `line` as `(first column, last column, link)` runs of
/// adjacent cells with equal URI and id.
fn runs_of(line: &Line) -> Vec<((usize, usize), Spec)> {
    let mut runs: Vec<((usize, usize), Spec)> = Vec::new();
    for (col, link) in line.graphemes.links() {
        let spec = spec_of(link);
        match runs.last_mut() {
            Some(((_, last), s)) if *last + 1 == col && *s == spec => *last = col,
            _ => runs.push(((col, col), spec)),
        }
    }
    runs
}

/// The cells `runs` list, as the per-cell expectation of a line.
fn expected_cells(runs: &[Run], line: usize) -> Vec<(usize, Spec)> {
    let mut cells: Vec<(usize, Spec)> = runs
        .iter()
        .filter(|run| run.row == line)
        .flat_map(|run| (run.cols.0..=run.cols.1).map(|c| (c, run.link.clone())))
        .collect();
    cells.sort_by_key(|(col, _)| *col);
    cells
}

fn cells_of(line: &Line) -> Vec<(usize, Spec)> {
    line.graphemes
        .links()
        .map(|(col, link)| (col, spec_of(link)))
        .collect()
}

/// What a run produced, comparable across chunkings: the links of every
/// cell, which cells belong to the same entity, and the open link.
fn outcome(grid: &Grid) -> String {
    let mut entities: Vec<Hyperlink> = Vec::new();
    let mut class = |link: &Hyperlink| -> usize {
        if let Some(i) = entities.iter().position(|e| e.same_link(link)) {
            i
        } else {
            entities.push(link.clone());
            entities.len() - 1
        }
    };
    let mut out = String::new();
    for (index, line) in grid.logical_lines().enumerate() {
        for (col, link) in line.graphemes.links() {
            let _ = writeln!(out, "{index}:{col} {link:?} #{}", class(link));
        }
    }
    let _ = writeln!(out, "active {:?}", grid.hyperlink());
    out
}

fn check_run(row: &Row, chunking: Chunking) -> (String, Vec<String>) {
    let term = run(row, chunking);
    let grid = term.grid();
    grid.check_invariants();
    let mut errors = Vec::new();
    let mut fail = |what: String| errors.push(format!("{} [{chunking:?}]: {what}", row.id));
    for (r, line) in grid.visible_lines().iter().enumerate() {
        let want = expected_cells(&row.links, r);
        let got = cells_of(line);
        if got != want {
            fail(format!(
                "row {r} links {:?} != expected {want:?}",
                runs_of(line)
            ));
        }
        for (col, cell) in line.cells.iter().enumerate() {
            if cell.is_link() != line.graphemes.link(col).is_some() {
                fail(format!("row {r} col {col}: LINK flag and table disagree"));
            }
        }
    }
    if let Some(history) = &row.links_scrollback {
        for s in 0..grid.scrollback_len() {
            let line = grid.scrollback_line(s).expect("history line");
            let want = expected_cells(history, s);
            let got = cells_of(line);
            if got != want {
                fail(format!(
                    "history {s} links {:?} != expected {want:?}",
                    runs_of(line)
                ));
            }
        }
        for run in history {
            assert!(
                run.row < grid.scrollback_len(),
                "{}: no history line",
                row.id
            );
        }
    }
    let active = grid.hyperlink().map(spec_of);
    if active != row.active {
        fail(format!("active {active:?} != {:?}", row.active));
    }
    let link_at = |&(r, c): &(usize, usize)| -> Option<&Hyperlink> {
        grid.line(u16::try_from(r).ok()?)?.hyperlink(c)
    };
    for group in &row.same {
        let links: Vec<_> = group.iter().map(link_at).collect();
        let first = links.first().copied().flatten();
        let ok = first.is_some()
            && links
                .iter()
                .all(|l| l.zip(first).is_some_and(|(l, f)| l.same_link(f)));
        if !ok {
            fail(format!("{group:?} are not one link: {links:?}"));
        }
    }
    for group in &row.distinct {
        let links: Vec<_> = group.iter().map(link_at).collect();
        let pairs_differ = links.iter().enumerate().all(|(i, a)| {
            a.is_some()
                && links[i + 1..]
                    .iter()
                    .all(|b| b.zip(*a).is_some_and(|(b, a)| !a.same_link(b)))
        });
        if !pairs_differ {
            fail(format!("{group:?} are not distinct links: {links:?}"));
        }
    }
    (outcome(grid), errors)
}

fn check_row(row: &Row) {
    let (input, ..) = plan(row);
    let (whole, mut errors) = check_run(row, Chunking::Whole);
    for chunking in chunkings(input.len()).into_iter().skip(1) {
        let (got, run_errors) = check_run(row, chunking);
        errors.extend(run_errors);
        if got != whole {
            errors.push(format!(
                "{} [{chunking:?}]: differs from the whole run\n{got}!= whole\n{whole}",
                row.id
            ));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn every_row() {
    let rows: Vec<Row> = raw_rows(FIXTURE)
        .into_iter()
        .map(|(id, lines)| parse_row(id, &lines))
        .collect();
    check_table(
        "hyperlink_spec.txt",
        ROWS,
        rows.iter().map(|row| (row.id.clone(), row)),
        check_row,
    );
}

#[test]
fn chunkings_cover_every_split() {
    assert_eq!(chunkings(3).len(), 2 + 2);
    assert_eq!(expand("a@rep(b,3)\\x1b"), b"abbb\x1b");
    assert_eq!(
        parse_spec("https://a.bc/#k"),
        ("https://a.bc/".into(), Some("k".into()))
    );
}

fn open(uri: &str) -> Vec<u8> {
    format!("\x1b]8;;{uri}\x1b\\").into_bytes()
}

#[test]
fn the_last_non_empty_id_wins_and_other_params_are_skipped() {
    let mut term = Terminal::new(2, 10);
    term.advance(b"\x1b]8;id=a:x:id=b:id=:y=z;https://a.bc/\x1b\\q");
    let link = term.grid().hyperlink().expect("open");
    assert_eq!(link.id(), Some("b"));
    assert_eq!(term.grid().line(0).unwrap().hyperlink(0), Some(link));
}

#[test]
fn a_payload_that_may_have_been_cut_closes_the_link() {
    let mut term = Terminal::new(2, 10);
    term.advance(&open("https://a.bc/"));
    // 16 pieces (8, params, the URI and 13 more) fill the parser's storage: the URI may be cut.
    let mut cut = b"\x1b]8;;https://a.bc/".to_vec();
    cut.extend(b";x".repeat(13));
    cut.extend(b"\x1b\\");
    term.advance(&cut);
    assert!(term.grid().hyperlink().is_none());
    // One piece fewer is a URI with its semicolons.
    let mut fits = b"\x1b]8;;https://a.bc/".to_vec();
    fits.extend(b";x".repeat(12));
    fits.extend(b"\x1b\\");
    term.advance(&fits);
    let uri = term.grid().hyperlink().expect("open").uri().to_owned();
    assert_eq!(uri, format!("https://a.bc/{}", ";x".repeat(12)));
}

#[test]
fn a_flood_of_links_holds_a_bounded_number_of_them() {
    let mut term = Terminal::new(2, 10);
    for i in 0..200_000 {
        term.advance(&open(&format!("https://a.bc/{i}")));
        term.advance(b"\rx");
    }
    assert!(term.grid().live_hyperlinks() <= 2);
    term.grid().check_invariants();
}

#[test]
fn past_the_cap_new_links_are_ignored_and_close_the_open_one() {
    let mut term = Terminal::new(10, 200);
    term.set_max_scrollback(1000);
    for i in 0..tessera_grid::MAX_LIVE_LINKS {
        term.advance(&open(&format!("https://a.bc/{i}")));
        term.advance(b"x");
    }
    assert_eq!(term.grid().live_hyperlinks(), tessera_grid::MAX_LIVE_LINKS);
    term.advance(&open("https://a.bc/one-too-many"));
    assert!(term.grid().hyperlink().is_none());
    term.advance(b"y");
    let cursor = term.grid().cursor();
    let line = term.grid().line(cursor.row).unwrap();
    assert!(line.hyperlink(usize::from(cursor.col) - 1).is_none());
    // A link that exists can still be reopened by id once it is alive.
    assert_eq!(term.grid().live_hyperlinks(), tessera_grid::MAX_LIVE_LINKS);
}

#[test]
fn decaln_keeps_the_open_link_for_the_text_after_it() {
    let mut term = Terminal::new(2, 4);
    term.advance(&open("https://a.bc/"));
    term.advance(b"\x1b#8y");
    let grid = term.grid();
    assert_eq!(grid.hyperlink().map(Hyperlink::uri), Some("https://a.bc/"));
    let row = grid.line(0).unwrap();
    assert_eq!(row.text(), "yEEE");
    assert_eq!(row.hyperlink(0).map(Hyperlink::uri), Some("https://a.bc/"));
    assert!((1..4).all(|col| row.hyperlink(col).is_none()), "the fill");
    assert!(grid.line(1).unwrap().hyperlink(0).is_none());
}

#[test]
fn a_repeated_alternate_screen_set_closes_the_open_link() {
    for set in ["\x1b[?1049h", "\x1b[?47h", "\x1b[?1047h"] {
        let mut term = Terminal::new(2, 6);
        term.advance(set.as_bytes());
        term.advance(&open("https://a.bc/"));
        term.advance(b"a");
        term.advance(set.as_bytes());
        assert!(term.grid().hyperlink().is_none(), "{set:?}");
        term.advance(b"\x1b[1;2Hb");
        let row = term.grid().line(0).unwrap();
        assert!(row.hyperlink(1).is_none(), "{set:?}");
    }
}

#[test]
fn controls_and_bidi_formatting_in_a_uri_refuse_the_link_and_the_text_prints() {
    for ch in [
        '\u{7f}', '\u{80}', '\u{85}', '\u{9b}', '\u{9f}', '\u{200e}', '\u{200f}', '\u{202a}',
        '\u{202e}', '\u{2066}', '\u{2069}',
    ] {
        let mut term = Terminal::new(2, 10);
        term.advance(&open("https://a.bc/"));
        term.advance(b"a");
        term.advance(&open(&format!("https://a.bc/{ch}x")));
        assert!(term.grid().hyperlink().is_none(), "{ch:?}");
        term.advance(b"b");
        let row = term.grid().line(0).unwrap();
        assert_eq!(row.text(), "ab", "{ch:?}");
        assert!(row.hyperlink(0).is_some() && row.hyperlink(1).is_none());
        assert_eq!(term.grid().live_hyperlinks(), 1, "{ch:?}");
    }
    // NUL is dropped by the parser, as before: the link opens without it.
    let mut term = Terminal::new(2, 10);
    term.advance(b"\x1b]8;;https://a.bc/a\0b\x1b\\");
    let link = term.grid().hyperlink().map(|l| l.uri().to_owned());
    assert_eq!(link.as_deref(), Some("https://a.bc/ab"));
}
