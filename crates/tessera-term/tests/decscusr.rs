//! DECSCUSR (`CSI Ps SP q`, cursor style) and its interplay with DECSET /
//! DECRST 12, the resets and the configured default (#113).
//!
//! Expected behaviour comes from `tests/fixtures/decscusr_spec.txt`, which
//! cites xterm's `CASE_DECSCUSR` and `ReallyReset`, the VT510 manual,
//! kitty and Alacritty per row; its header lists the decisions (D-PS0,
//! D-BLINKFLAG, …) the rows encode. The fixture has its own layout, read
//! here:
//!
//! - `### <id>` starts a row; `size: RxC` gives the terminal size;
//! - `default: <shape> <steady|blink>` is applied with
//!   [`Terminal::set_default_cursor_style`] before the first step;
//! - `step: feed <payload>` (the payload is everything after the one
//!   space after `feed`, decoded by [`common::decode`]; a trailing
//!   whitespace-and-`#` comment is dropped), `step: set_default <shape>
//!   <steady|blink>` and `step: resize RxC` run in order;
//! - `expect: <shape> <steady|blink> [modes_blink=<bool>]` checks
//!   [`Terminal::cursor_style`] (and [`Modes::cursor_blink`]) after the
//!   steps above it;
//! - `screen:` (one `>` line per row) and `cursor: (row,col)` check the
//!   screen after the last step;
//! - `reply: <bytes>` checks the [`Request::Reply`] bytes queued since the
//!   previous `reply:` line, concatenated (the DECRQSS and DECRQM rows
//!   Q01..Q06, the #113 contract #10 fulfils). Q02's `feed (for Ps in
//!   1..=6) ...` repeats its payload with `<Ps>` replaced, and its `reply:
//!   ... for each Ps (1,2,...)` expects the replies for those values;
//! - `requests: []`, `cite:`, `note:` and `flags:` are read but checked
//!   only as described below (the `deferred` flag predates #10 and is
//!   ignored).
//!
//! [`every_row`] runs every row whole and byte by byte and reports every
//! failing row id at once. Every row also asserts that the terminal queued
//! no request other than replies; replies are checked only where a row
//! has `reply:` lines. A few rows carry a `note:` asking for one more
//! check than their keys express; [`row_extras`] adds those by id.
//!
//! [`Request::Reply`]: tessera_term::Request::Reply
//!
//! [`Modes::cursor_blink`]: tessera_term::Modes::cursor_blink

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;

use std::time::{Duration, Instant};

use common::{Chunking, decode, render_screen};
use tessera_grid::Point;
use tessera_term::{CursorShape, CursorStyle, Request, Terminal};

const FIXTURE: &str = include_str!("fixtures/decscusr_spec.txt");

/// Rows in the fixture, Q01..Q06 included.
const ROWS: usize = 60;
/// How often X08 repeats its step (its comment says so).
const FLOOD_REPEAT: usize = 2_000;

/// One `step:` or `expect:` line of a row, in fixture order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Feed(Vec<u8>),
    SetDefault(CursorStyle),
    Resize(u16, u16),
    Expect {
        style: CursorStyle,
        modes_blink: Option<bool>,
    },
    /// The replies since the previous `Reply`, concatenated.
    Reply(Vec<u8>),
}

/// One row of the fixture.
#[derive(Debug, Clone)]
struct Row {
    id: String,
    rows: u16,
    cols: u16,
    default: Option<CursorStyle>,
    steps: Vec<Step>,
    screen: Option<String>,
    cursor: Option<Point>,
}

fn parse_style(text: &str) -> CursorStyle {
    let mut words = text.split_whitespace();
    let shape = match words.next() {
        Some("block") => CursorShape::Block,
        Some("underline") => CursorShape::Underline,
        Some("beam") => CursorShape::Beam,
        other => panic!("`{text}`: unknown shape {other:?}"),
    };
    let blink = match words.next() {
        Some("steady") => false,
        Some("blink") => true,
        other => panic!("`{text}`: expected steady or blink, got {other:?}"),
    };
    assert_eq!(words.next(), None, "`{text}`: trailing words");
    CursorStyle { shape, blink }
}

fn parse_size(text: &str) -> (u16, u16) {
    let parsed = text
        .trim()
        .split_once('x')
        .and_then(|(rows, cols)| Some((rows.parse().ok()?, cols.parse().ok()?)));
    parsed.unwrap_or_else(|| panic!("`{text}`: expected RxC"))
}

fn parse_point(text: &str) -> Point {
    let parsed = text
        .trim()
        .strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .and_then(|t| t.split_once(','))
        .and_then(|(row, col)| Some(Point::new(row.parse().ok()?, col.parse().ok()?)));
    parsed.unwrap_or_else(|| panic!("`{text}`: expected (row,col)"))
}

fn parse_expect(text: &str) -> Step {
    let (style, modes_blink) = match text.split_once(" modes_blink=") {
        Some((style, "true")) => (style, Some(true)),
        Some((style, "false")) => (style, Some(false)),
        Some((_, other)) => panic!("`{text}`: modes_blink={other}"),
        None => (text, None),
    };
    Step::Expect {
        style: parse_style(style),
        modes_blink,
    }
}

/// A `step:` line's argument, its trailing `<whitespace># ...` comment
/// dropped.
fn strip_comment(text: &str) -> &str {
    text.char_indices()
        .find(|&(i, c)| c.is_whitespace() && text[i..].trim_start().starts_with('#'))
        .map_or(text, |(i, _)| &text[..i])
}

/// `(for Ps in A..=B) payload`: the payload once per Ps, `<Ps>` replaced.
fn for_each_ps(text: &str) -> Option<Vec<u8>> {
    let (range, payload) = text.strip_prefix("(for Ps in ")?.split_once(") ")?;
    let (first, last) = range.split_once("..=")?;
    let (first, last): (u16, u16) = (first.parse().ok()?, last.parse().ok()?);
    Some(
        (first..=last)
            .flat_map(|ps| decode(&payload.replace("<Ps>", &ps.to_string())))
            .collect(),
    )
}

/// A `reply:` value: bytes, or `bytes for each Ps (a,b,...)` with `<Ps>`
/// replaced by each value in turn.
fn parse_reply(text: &str) -> Step {
    let Some((payload, list)) = text.split_once(" for each Ps (") else {
        return Step::Reply(decode(text.trim_end()));
    };
    let list = list.split(" in order)").next().unwrap_or(list);
    Step::Reply(
        list.split(',')
            .flat_map(|ps| decode(&payload.replace("<Ps>", ps.trim())))
            .collect(),
    )
}

fn parse_step(arg: &str) -> Step {
    let arg = strip_comment(arg);
    if let Some(bytes) = arg.strip_prefix("feed ").and_then(for_each_ps) {
        Step::Feed(bytes)
    } else if let Some(payload) = arg.strip_prefix("feed ") {
        // Everything after the single space: X07's payload starts with one.
        Step::Feed(decode(payload))
    } else if let Some(style) = arg.strip_prefix("set_default ") {
        Step::SetDefault(parse_style(style))
    } else if let Some(size) = arg.strip_prefix("resize ") {
        let (rows, cols) = parse_size(size);
        Step::Resize(rows, cols)
    } else {
        panic!("unknown step `{arg}`");
    }
}

/// The lines of each row: `(id, lines after the header)`.
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
        rows: 0,
        cols: 0,
        default: None,
        steps: Vec::new(),
        screen: None,
        cursor: None,
    };
    let mut lines = lines.iter().peekable();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("{}: unexpected line `{line}`", row.id));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match key {
            "size" => (row.rows, row.cols) = parse_size(value),
            "default" => row.default = Some(parse_style(value)),
            "step" => row.steps.push(parse_step(value)),
            "expect" => row.steps.push(parse_expect(value)),
            "screen" => {
                let mut screen = String::new();
                while let Some(text) = lines.peek().and_then(|l| l.strip_prefix('>')) {
                    screen.push_str(text);
                    screen.push('\n');
                    lines.next();
                }
                row.screen = Some(screen);
            }
            "cursor" => row.cursor = Some(parse_point(value)),
            "requests" => assert_eq!(value.trim(), "[]", "{}: requests", row.id),
            "reply" => row.steps.push(parse_reply(value)),
            "cite" | "note" | "flags" => {}
            other => panic!("{}: unknown key `{other}`", row.id),
        }
    }
    assert!(row.rows > 0 && row.cols > 0, "{}: no size", row.id);
    row
}

fn rows() -> Vec<Row> {
    raw_rows(FIXTURE)
        .into_iter()
        .map(|(id, lines)| parse_row(id, &lines))
        .collect()
}

fn feed(term: &mut Terminal, bytes: &[u8], chunking: Chunking) {
    match chunking {
        Chunking::Whole => term.advance(bytes),
        Chunking::Bytewise => {
            for byte in bytes {
                term.advance(std::slice::from_ref(byte));
            }
        }
    }
}

fn style_text(style: CursorStyle) -> String {
    let shape = match style.shape {
        CursorShape::Block => "block",
        CursorShape::Underline => "underline",
        CursorShape::Beam => "beam",
    };
    let blink = if style.blink { "blink" } else { "steady" };
    format!("{shape} {blink}")
}

/// Moves the terminal's requests into `replies` (the reply bytes) and
/// `others`.
fn collect(term: &mut Terminal, replies: &mut Vec<u8>, others: &mut Vec<Request>) {
    for request in term.take_requests() {
        match request {
            Request::Reply(bytes) => replies.extend(bytes),
            other => others.push(other),
        }
    }
}

/// Runs `row` and returns one message per mismatch.
fn check_row(row: &Row, chunking: Chunking) -> Vec<String> {
    let mut errors = Vec::new();
    let mut fail = |what: String| errors.push(format!("{} [{chunking:?}]: {what}", row.id));
    let mut term = Terminal::new(row.rows, row.cols);
    if let Some(style) = row.default {
        term.set_default_cursor_style(style);
    }
    // X08's comment: its step runs this many times.
    let repeat = if row.id.starts_with("X08_") {
        FLOOD_REPEAT
    } else {
        1
    };
    let mut replies = Vec::new();
    let mut others = Vec::new();
    for (index, step) in row.steps.iter().enumerate() {
        match step {
            Step::Feed(bytes) => {
                for _ in 0..repeat {
                    feed(&mut term, bytes, chunking);
                }
                collect(&mut term, &mut replies, &mut others);
            }
            Step::Reply(want) => {
                collect(&mut term, &mut replies, &mut others);
                let got = std::mem::take(&mut replies);
                if got != *want {
                    fail(format!(
                        "reply #{index}: {} != {}",
                        got.escape_ascii(),
                        want.escape_ascii()
                    ));
                }
            }
            Step::SetDefault(style) => term.set_default_cursor_style(*style),
            Step::Resize(rows, cols) => term.resize(*rows, *cols),
            Step::Expect { style, modes_blink } => {
                let got = term.cursor_style();
                if got != *style {
                    fail(format!(
                        "expect #{index}: cursor_style {} != {}",
                        style_text(got),
                        style_text(*style)
                    ));
                }
                let blink = term.modes().cursor_blink;
                if let Some(want) = modes_blink
                    && blink != *want
                {
                    fail(format!("expect #{index}: modes_blink={blink} != {want}"));
                }
            }
        }
    }
    if let Some(screen) = &row.screen {
        let got = render_screen(&term);
        if got != *screen {
            fail(format!("screen\n{got}!=\n{screen}"));
        }
    }
    if let Some(cursor) = row.cursor
        && term.grid().cursor() != cursor
    {
        fail(format!("cursor {:?} != {cursor:?}", term.grid().cursor()));
    }
    collect(&mut term, &mut replies, &mut others);
    if !others.is_empty() {
        fail(format!("requests {others:?}"));
    }
    for what in row_extras(row, &term, chunking) {
        fail(what);
    }
    errors
}

/// The checks some rows' notes ask for beyond their keys, by row id.
fn row_extras(row: &Row, term: &Terminal, chunking: Chunking) -> Vec<String> {
    let mut errors = Vec::new();
    let id = row.id.split('_').next().unwrap_or_default();
    match id {
        // "modes().cursor_visible is true at the end of this row."
        "P14" | "R08" if !term.modes().cursor_visible => {
            errors.push("cursor_visible is false".to_owned());
        }
        // "feed `\x1b[2qX` leaves X at (0,0)": DECLL is consumed whole.
        "X03" => {
            let mut fresh = Terminal::new(row.rows, row.cols);
            feed(&mut fresh, b"\x1b[2qX", chunking);
            let first = fresh.grid().visible_lines()[0].cells[0].ch;
            if first != 'X' || fresh.grid().cursor() != Point::new(0, 1) {
                errors.push(format!("`CSI 2 q X`: {first:?} at (0,0)"));
            }
        }
        // "Style stays at its previous value (default)".
        "X06" if term.cursor_style() != CursorStyle::default() => {
            errors.push(format!("style {}", style_text(term.cursor_style())));
        }
        // ?25 travels with ?12 in both sequences: shown after the set,
        // hidden after the reset.
        "B08" => {
            let mut fresh = Terminal::new(row.rows, row.cols);
            feed(&mut fresh, b"\x1b[?12;25h", chunking);
            let shown = fresh.modes().cursor_visible;
            feed(&mut fresh, b"\x1b[?25;12l", chunking);
            let hidden = !fresh.modes().cursor_visible;
            if !(shown && hidden) {
                errors.push(format!("cursor_visible: shown {shown}, hidden {hidden}"));
            }
        }
        _ => {}
    }
    errors
}

#[test]
fn every_row() {
    let rows = rows();
    assert_eq!(rows.len(), ROWS, "rows in the fixture");
    let runnable: Vec<&Row> = rows.iter().collect();
    let errors: Vec<String> = runnable
        .iter()
        .flat_map(|row| {
            [Chunking::Whole, Chunking::Bytewise]
                .into_iter()
                .flat_map(|chunking| check_row(row, chunking))
        })
        .collect();
    let mut failing: Vec<&str> = errors.iter().filter_map(|e| e.split(' ').next()).collect();
    failing.dedup();
    assert!(
        errors.is_empty(),
        "{} of {} rows fail: {}\n{}",
        failing.len(),
        runnable.len(),
        failing.join(", "),
        errors.join("\n")
    );
    eprintln!("{} rows passed whole and byte by byte", runnable.len());
}

/// P12's premise, checked against the parser rather than assumed: an
/// oversized parameter saturates at `u16::MAX` (so it is a DECSCUSR Ps
/// above 7, ignored), it does not wrap to a small value.
#[test]
fn oversized_parameters_saturate_in_the_parser() {
    let mut parser = tessera_vt::Parser::new();
    let mut seen = Vec::new();
    parser.advance(b"\x1b[4294967296 q\x1b[65536 q\x1b[65535 q", |action| {
        if let tessera_vt::Action::Csi { params, .. } = action {
            seen.push(params);
        }
    });
    assert_eq!(seen, vec![vec![u16::MAX]; 3]);
}

/// A flood of DECSCUSR and DECSET 12 queues nothing and keeps the cost
/// per sequence flat: 200 000 sequences (2.4 MB) take about as long per
/// sequence as 20 000.
#[test]
fn a_flood_of_cursor_styles_requests_nothing_and_stays_linear() {
    let unit = b"\x1b[5 q\x1b[?12l\x1b[0 q\x1b[3 q";
    let time = |n: usize| {
        let bytes = unit.repeat(n);
        let mut term = Terminal::new(3, 10);
        let start = Instant::now();
        term.advance(&bytes);
        let took = start.elapsed();
        assert!(term.take_requests().is_empty(), "{n}: requests");
        assert_eq!(term.cursor_style().shape, CursorShape::Underline);
        took
    };
    let best = |n: usize| (0..3).map(|_| time(n)).min().unwrap_or(Duration::MAX);
    let small = best(5_000);
    let large = best(50_000);
    eprintln!("5 000 units: {small:?}, 50 000 units: {large:?}");
    // Linear is 10x; allow for noise, catch quadratic growth (100x).
    assert!(
        large < small * 30 + Duration::from_millis(20),
        "{large:?} vs {small:?}"
    );
}
