//! S7C1T / S8C1T and 8-bit C1 controls (#96, ADR 0024).
//!
//! Expected behaviour comes from `tests/fixtures/c1_spec.txt`, whose header
//! cites xterm's source and ctlseqs, DEC's VT510 manual, esctest2, vttest
//! and the vte source, and lists the decisions the rows encode. The fixture
//! has its own layout, read here:
//!
//! - `### <id>` starts a row; `size: RxC` gives the terminal size;
//! - `step: feed <payload>` feeds the payload (`\e` for ESC and the escapes
//!   of [`common::decode`]);
//! - `reply: <bytes>|none` checks the [`Request::Reply`] bytes queued since
//!   the previous `reply:` line, concatenated (`<PV>` is [`DA2_PV`],
//!   `<VERSION>` this crate's version); `requests: [..]` the other requests
//!   since the previous `requests:` line (`SetTitle("..")`);
//! - `c1_8bit: true|false` checks [`Terminal::c1_8bit_replies`];
//! - `screen:` (`>` lines) and `cursor: (row,col)` check the screen;
//! - `key:`, `mouse:` or `send:` with `out7:` and `out8:` check an encoding
//!   in the keyboard and mouse modes the steps left, and what
//!   [`to_8bit_controls`] makes of it;
//! - `cite:`, `note:`, `source:` and `flags:` are not checked.
//!
//! [`every_row`] runs every row whole, byte by byte and split in two at
//! every offset of every feed step, and expects the same results each time.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;

use common::{decode, render_screen};
use tessera_grid::Point;
use tessera_term::{
    DA2_PV, Key, KeyEvent, Modifiers, MouseReport, MouseReportButton, MouseReportKind, Request,
    Terminal, encode_key, encode_mouse, to_8bit_controls,
};

const FIXTURE: &str = include_str!("fixtures/c1_spec.txt");
const FIXTURE_NAME: &str = "fixtures/c1_spec.txt";

/// `###` rows in the fixture, so a row lost from it cannot pass silently.
const ROWS: usize = 120;

/// What an output row encodes.
#[derive(Debug, Clone)]
enum Output {
    /// `key:` one key press.
    Key(KeyEvent),
    /// `mouse:` one mouse report.
    Mouse(MouseReport),
    /// `send:` bytes the host builds itself.
    Send(Vec<u8>),
}

/// One line of a row that does something, in fixture order.
#[derive(Debug, Clone)]
enum Item {
    /// `step: feed`.
    Feed(Vec<u8>),
    /// `reply:`: the expected reply bytes, `None` for `none`.
    Reply(Option<Vec<u8>>),
    /// `requests:`.
    Requests(Vec<Request>),
}

/// One row of the fixture.
#[derive(Debug, Clone, Default)]
struct Row {
    id: String,
    rows: u16,
    cols: u16,
    items: Vec<Item>,
    c1_8bit: Option<bool>,
    screen: Option<String>,
    cursor: Option<Point>,
    output: Option<Output>,
    out7: Option<Vec<u8>>,
    out8: Option<Vec<u8>>,
}

/// Replaces the fixture's `\e` with `\x1b`, leaving `\\` alone, then
/// decodes the other escapes with [`common::decode`].
fn decode_payload(text: &str) -> Vec<u8> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('e') => out.push_str("\\x1b"),
            Some(next) => {
                out.push('\\');
                out.push(next);
            }
            None => out.push('\\'),
        }
    }
    decode(&out)
}

fn parse_reply(text: &str) -> Option<Vec<u8>> {
    if text.trim() == "none" {
        return None;
    }
    let text = text
        .replace("<VERSION>", env!("CARGO_PKG_VERSION"))
        .replace("<PV>", &DA2_PV.to_string());
    Some(decode_payload(&text))
}

/// `[]` or a comma-separated list of `SetTitle("..")`.
fn parse_requests(text: &str) -> Vec<Request> {
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .unwrap_or_else(|| panic!("requests `{text}`: expected `[..]`"));
    let mut requests = Vec::new();
    let mut rest = inner.trim();
    while !rest.is_empty() {
        let after = rest
            .strip_prefix("SetTitle(\"")
            .unwrap_or_else(|| panic!("unknown request at `{rest}` in `{text}`"));
        let end = after
            .find("\")")
            .unwrap_or_else(|| panic!("unterminated SetTitle in `{text}`"));
        let title = String::from_utf8(decode_payload(&after[..end])).expect("UTF-8 title");
        requests.push(Request::SetTitle(title));
        rest = after[end + 2..].trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
    requests
}

fn parse_point(text: &str) -> Point {
    let text = text.split_once('#').map_or(text, |(value, _)| value);
    let (row, col) = text
        .trim()
        .strip_prefix('(')
        .and_then(|t| t.strip_suffix(')'))
        .and_then(|t| t.split_once(','))
        .unwrap_or_else(|| panic!("cursor `{text}`: expected `(row,col)`"));
    Point::new(row.parse().expect("row"), col.parse().expect("col"))
}

/// `key: <name> [ctrl|shift|alt joined by +]`.
fn parse_key(text: &str) -> KeyEvent {
    let (name, mods) = text.trim().split_once(' ').unwrap_or((text.trim(), ""));
    let modifiers = mods
        .split('+')
        .filter(|m| !m.is_empty())
        .map(|m| match m {
            "ctrl" => Modifiers::CTRL,
            "shift" => Modifiers::SHIFT,
            "alt" => Modifiers::ALT,
            other => panic!("unknown modifier `{other}`"),
        })
        .fold(Modifiers::NONE, |all, one| all | one);
    let key = match name {
        "Up" => Key::Up,
        "Home" => Key::Home,
        "Delete" => Key::Delete,
        "Tab" => Key::Tab,
        "Escape" => Key::Escape,
        "Kp5" => Key::Kp5,
        _ => {
            if let Some(n) = name.strip_prefix('F') {
                Key::F(n.parse().expect("F<n>"))
            } else if let Some(c) = name.strip_prefix("Char:") {
                let c = String::from_utf8(decode_payload(c)).expect("UTF-8 key");
                let mut chars = c.chars();
                let ch = chars.next().expect("one character");
                assert!(chars.next().is_none(), "`Char:{c}`: one character");
                Key::Char(ch)
            } else {
                panic!("unknown key `{name}`")
            }
        }
    };
    KeyEvent::new(key, modifiers)
}

/// `mouse: press|release left <col>,<row>`.
fn parse_mouse(text: &str) -> MouseReport {
    let words: Vec<&str> = text.split_whitespace().collect();
    let [kind, button, cell] = words.as_slice() else {
        panic!("mouse `{text}`: expected `<kind> <button> <col>,<row>`");
    };
    let kind = match *kind {
        "press" => MouseReportKind::Press,
        "release" => MouseReportKind::Release,
        other => panic!("unknown mouse kind `{other}`"),
    };
    assert_eq!(*button, "left", "only the left button is used");
    let (col, row) = cell.split_once(',').expect("`col,row`");
    MouseReport::new(
        kind,
        MouseReportButton::Left,
        col.parse().expect("col"),
        row.parse().expect("row"),
    )
}

fn parse_row(id: String, lines: &[&str]) -> Row {
    let mut row = Row {
        id,
        ..Row::default()
    };
    let mut lines = lines.iter().peekable();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() || line.starts_with('#') || line.starts_with(' ') {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("{}: unexpected line `{line}`", row.id));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match key {
            "size" => {
                let (rows, cols) = value.trim().split_once('x').expect("size RxC");
                row.rows = rows.parse().expect("rows");
                row.cols = cols.parse().expect("cols");
            }
            "step" => {
                let payload = value
                    .strip_prefix("feed ")
                    .unwrap_or_else(|| panic!("{}: unknown step `{value}`", row.id));
                row.items.push(Item::Feed(decode_payload(payload)));
            }
            "reply" => row.items.push(Item::Reply(parse_reply(value))),
            "requests" => row.items.push(Item::Requests(parse_requests(value))),
            "c1_8bit" => row.c1_8bit = Some(value.trim() == "true"),
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
            "key" => row.output = Some(Output::Key(parse_key(value))),
            "mouse" => row.output = Some(Output::Mouse(parse_mouse(value))),
            "send" => row.output = Some(Output::Send(decode_payload(value))),
            "out7" => row.out7 = Some(decode_payload(value)),
            "out8" => row.out8 = Some(decode_payload(value)),
            "cite" | "note" | "flags" | "source" => {}
            other => panic!("{}: unknown key `{other}`", row.id),
        }
    }
    assert!(row.rows > 0 && row.cols > 0, "{}: no size", row.id);
    assert_eq!(
        row.output.is_some(),
        row.out7.is_some() && row.out8.is_some(),
        "{}: an output row has key/mouse/send, out7 and out8",
        row.id
    );
    row
}

fn rows() -> Vec<Row> {
    let mut raw: Vec<(String, Vec<&str>)> = Vec::new();
    for line in FIXTURE.lines() {
        if let Some(id) = line.strip_prefix("### ") {
            raw.push((id.trim().to_owned(), Vec::new()));
        } else if let Some((_, lines)) = raw.last_mut() {
            lines.push(line);
        }
    }
    raw.into_iter()
        .map(|(id, lines)| parse_row(id, &lines))
        .collect()
}

/// How a row's feed steps reach [`Terminal::advance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunking {
    /// Each feed step in one call.
    Whole,
    /// One byte per call.
    Bytewise,
    /// Feed step `step` in two calls, split after `at` bytes; every other
    /// step whole.
    Split { step: usize, at: usize },
}

fn feed(term: &mut Terminal, bytes: &[u8], index: usize, chunking: Chunking) {
    match chunking {
        Chunking::Bytewise => {
            for byte in bytes {
                term.advance(std::slice::from_ref(byte));
            }
        }
        Chunking::Split { step, at } if step == index => {
            term.advance(&bytes[..at]);
            term.advance(&bytes[at..]);
        }
        Chunking::Whole | Chunking::Split { .. } => term.advance(bytes),
    }
}

/// Runs `row` with `chunking` and returns one message per mismatch.
fn check_row(row: &Row, chunking: Chunking) -> Vec<String> {
    let mut errors = Vec::new();
    let mut fail = |what: String| errors.push(format!("{} [{chunking:?}]: {what}", row.id));
    let mut term = Terminal::new(row.rows, row.cols);
    let mut replies: Vec<Vec<u8>> = Vec::new();
    let mut others: Vec<Request> = Vec::new();
    for (index, item) in row.items.iter().enumerate() {
        match item {
            Item::Feed(bytes) => {
                feed(&mut term, bytes, index, chunking);
                for request in term.take_requests() {
                    match request {
                        Request::Reply(bytes) => replies.push(bytes),
                        other => others.push(other),
                    }
                }
            }
            Item::Reply(want) => {
                let got = std::mem::take(&mut replies);
                let concat = got.concat();
                let ok = match want {
                    None => got.is_empty(),
                    Some(want) => concat == *want,
                };
                if !ok {
                    fail(format!(
                        "reply {:x?} != {:x?}",
                        concat,
                        want.as_deref().unwrap_or_default()
                    ));
                }
            }
            Item::Requests(want) => {
                let got = std::mem::take(&mut others);
                if got != *want {
                    fail(format!("requests {got:?} != {want:?}"));
                }
            }
        }
    }
    if let Some(want) = row.c1_8bit
        && term.c1_8bit_replies() != want
    {
        fail(format!(
            "c1_8bit_replies {} != {want}",
            term.c1_8bit_replies()
        ));
    }
    if let Some(screen) = &row.screen {
        let shown = render_screen(&term);
        if shown != *screen {
            fail(format!("screen\n{shown}!=\n{screen}"));
        }
    }
    if let Some(cursor) = row.cursor
        && term.grid().cursor() != cursor
    {
        fail(format!("cursor {:?} != {cursor:?}", term.grid().cursor()));
    }
    if let (Some(output), Some(out7), Some(out8)) = (&row.output, &row.out7, &row.out8) {
        let encoded = match output {
            Output::Key(event) => encode_key(event, &term.keyboard_modes()),
            Output::Mouse(report) => {
                let modes = term.modes();
                encode_mouse(report, modes.mouse, modes.mouse_encoding).unwrap_or_default()
            }
            Output::Send(bytes) => bytes.clone(),
        };
        if encoded != *out7 {
            fail(format!("7-bit {encoded:x?} != {out7:x?}"));
        }
        let mut eight = encoded;
        to_8bit_controls(&mut eight);
        if eight != *out8 {
            fail(format!("8-bit {eight:x?} != {out8:x?}"));
        }
    }
    errors
}

/// Every chunking `row` runs with.
fn chunkings(row: &Row) -> Vec<Chunking> {
    let mut all = vec![Chunking::Whole, Chunking::Bytewise];
    for (step, item) in row.items.iter().enumerate() {
        if let Item::Feed(bytes) = item {
            all.extend((1..bytes.len()).map(|at| Chunking::Split { step, at }));
        }
    }
    all
}

/// Every row of the fixture, whole, byte by byte and at every two-way
/// split, through [`common::check_table`].
#[test]
fn every_row() {
    let rows = rows();
    let headings = FIXTURE.lines().filter(|l| l.starts_with("### ")).count();
    assert_eq!(headings, ROWS, "rows in the fixture");
    common::check_table(
        FIXTURE_NAME,
        headings,
        rows.iter().map(|row| (row.id.clone(), row)),
        |row| {
            let errors: Vec<String> = chunkings(row)
                .into_iter()
                .flat_map(|chunking| check_row(row, chunking))
                .collect();
            let shown: Vec<&str> = errors.iter().take(4).map(String::as_str).collect();
            assert!(
                errors.is_empty(),
                "{} mismatches, the first:\n{}",
                errors.len(),
                shown.join("\n")
            );
        },
    );
}

/// Streams that switch 8-bit mode on and off around C1 codes (raw and UTF-8
/// encoded), characters with bytes in `0x80..=0x9F`, strings and queries.
fn c1_stream() -> impl proptest::strategy::Strategy<Value = Vec<u8>> {
    use proptest::prelude::*;
    const TOKENS: &[&[u8]] = &[
        b"\x1b G",
        b"\x1b F",
        b"\x1bc",
        b"\x1b[64;1\"p",
        b"\x1b[64\"p",
        b"\x1b[61\"p",
        b"\x84",
        b"\x85",
        b"\x88",
        b"\x8d",
        b"\x90",
        b"\x9a",
        b"\x9b",
        b"\x9c",
        b"\x9d",
        b"\x9f",
        b"\x80",
        b"\xc2\x84",
        b"\xc2\x9b",
        b"\xc2\x9c",
        b"\xc2\x9d",
        b"\xc2\x90",
        b"\xc3\x9c",
        b"\xc9\x9c",
        b"\xe4\xb8\x9c",
        b"\xe2\x85\x9b",
        b"\xe2\x9b",
        b"\xc3",
        b"\xa0",
        b"a",
        b"b",
        b" ",
        b"1",
        b";",
        b"2",
        b"H",
        b"m",
        b"q",
        b"$",
        b"\x1b[2;3H",
        b"\x1b]0;x",
        b"\x1bP$qm",
        b"\x1bPq",
        b"\x1b\\",
        b"\x07",
        b"\x18",
        b"\r\n",
        b"\x1b[c",
        b"\x1b[6n",
        b"\x1bZ",
        b"\x1b_x",
    ];
    proptest::collection::vec(proptest::sample::select(TOKENS), 0..80).prop_map(|t| t.concat())
}

/// What a run left behind: the screen, the cursor, the mode and every
/// request.
fn outcome(bytes: &[u8], cuts: &[usize]) -> (String, Point, bool, Vec<Request>) {
    let mut term = Terminal::new(5, 12);
    let mut requests = Vec::new();
    let mut at = 0;
    for &cut in cuts.iter().chain(std::iter::once(&bytes.len())) {
        let cut = cut.clamp(at, bytes.len());
        term.advance(&bytes[at..cut]);
        requests.extend(term.take_requests());
        at = cut;
    }
    (
        render_screen(&term),
        term.grid().cursor(),
        term.c1_8bit_replies(),
        requests,
    )
}

proptest::proptest! {
    /// However the output is split into reads, 8-bit mode switches and C1
    /// codes give the same screen, cursor, mode and requests as one read.
    #[test]
    fn c1_handling_does_not_depend_on_read_boundaries(
        bytes in c1_stream(),
        mut cuts in proptest::collection::vec(0usize..400, 0..12),
    ) {
        cuts.sort_unstable();
        proptest::prop_assert_eq!(outcome(&bytes, &cuts), outcome(&bytes, &[]));
    }
}

/// Seconds to parse `bytes` in one [`Terminal::advance`] call.
fn parse_seconds(bytes: &[u8]) -> f64 {
    let mut term = Terminal::new(24, 80);
    let start = std::time::Instant::now();
    term.advance(bytes);
    let seconds = start.elapsed().as_secs_f64();
    drop(term.take_requests());
    seconds
}

/// A flood: its name, a prefix, the unit repeated and the tail.
type Flood = (&'static str, &'static [u8], &'static [u8], &'static [u8]);

/// One read of about `len` bytes: `unit` repeated, then `tail`.
fn flood(prefix: &[u8], unit: &[u8], tail: &[u8], len: usize) -> Vec<u8> {
    let mut bytes = prefix.to_vec();
    bytes.extend(unit.repeat(len / unit.len()));
    bytes.extend_from_slice(tail);
    bytes
}

/// Parsing stays linear in the size of a read whatever it holds: a read
/// with one byte in `0x80..=0x9F` takes the C1-aware path, and every DCS
/// start and end and every sequence that can switch 8-bit mode must not
/// make it look at the rest of the read again (#96 review B1). Sixteen
/// times the input must take about sixteen times as long, not 256: best of
/// three interleaved runs, up to five attempts, bar 64 (the geometric
/// midpoint, so load noise has a factor of four either way).
#[test]
fn c1_aware_parsing_is_linear_in_the_read() {
    const N: usize = 4 << 10;
    const SCALE: usize = 16;
    const BAR: f64 = 64.0;
    let cases: [Flood; 4] = [
        ("dcs 7-bit", b"", b"\x1bPqx\x1b\\", "日".as_bytes()),
        ("s7c1t", b"", b"\x1b F", "日".as_bytes()),
        ("decscl", b"", b"\x1b[64;1\"p", "日".as_bytes()),
        ("dcs 8-bit", b"\x1b G", b"\x1bPqx\x1b\\", b"\x84"),
    ];
    let mut slow = Vec::new();
    for (name, prefix, unit, tail) in cases {
        let small = flood(prefix, unit, tail, N);
        let large = flood(prefix, unit, tail, SCALE * N);
        let mut ratio = f64::INFINITY;
        for _ in 0..5 {
            let (mut s, mut l) = (f64::INFINITY, f64::INFINITY);
            for _ in 0..3 {
                s = s.min(parse_seconds(&small));
                l = l.min(parse_seconds(&large));
            }
            ratio = l / s;
            if ratio < BAR {
                break;
            }
        }
        if ratio >= BAR {
            slow.push(format!(
                "{name}: {SCALE}x the input took {ratio:.1}x as long"
            ));
        }
    }
    assert!(slow.is_empty(), "not linear:\n{}", slow.join("\n"));
}
