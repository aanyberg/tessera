//! Operating system commands (#9): OSC 0/1/2 titles, OSC 52 clipboard
//! writes, OSC 7 working directory reports, OSC 133 prompt marks and the
//! OSC 1337 shell integration handshake.
//!
//! Expected behaviour comes from `tests/fixtures/osc_spec.txt`, which cites
//! xterm, vte, Alacritty, `WezTerm`, kitty and `iTerm2` per row; its header
//! lists the decisions (D-OSC1, D-SEL, D-CAP52, ...) the rows encode. Only
//! the milestone lead edits it. The fixture has its own layout, read here:
//!
//! - `### <id>` starts a row; `size: RxC` gives the terminal size;
//! - `step: feed <payload>` is one [`Terminal::advance`] call and `step:
//!   feed_bytewise <payload>` one call per byte. The payload is everything
//!   after the one space after the step kind (never trimmed), decoded by
//!   [`common::decode`] after expanding the harness macros `@rep(C,N)` (`N`
//!   copies of the ASCII character `C`) and `@b64rep(C,N)` (standard base64,
//!   padded, of `N` copies of the byte `C`);
//! - `screen:` (one `>` line per row, the [`render_screen`] layout) or
//!   `screen: not asserted`, `cursor: (row,col)`, `pending_wrap:` and
//!   `scrollback: len=N [first=("text", wrapped=false)]` check the grid;
//! - `requests: [...]` is the whole list of requests, in order: `Bell`,
//!   `SetTitle("..")`, `SetClipboard("..")`, `SetCwd(b"..")` and
//!   `ShellIntegrationHandshake(b"..")`, with `\xNN \\ \" \u{H..} \n \r \t`
//!   escapes and `@rep(C,N)` inside the quotes;
//! - `marks: r0=[PromptStart,CommandStart] r2=[OutputStart]` (or `none`, or
//!   absent: no marks) lists the marks of the active screen's rows,
//!   `marks_scrollback: s0=[..]` those of the scrollback (absent: none),
//!   `main_marks:` those of the main screen, checked after leaving the
//!   alternate screen with `CSI ? 1049 l`;
//! - `screen_is: main|alt` checks which screen is active;
//! - `layer: parse` rows (`call:` / `expect:`) are pure-function rows for
//!   the unit tests of `tessera-grid` and `tessera-shell-integration`: they
//!   are counted and listed here, not run;
//! - `cite:`, `note:` and `flags:` are documentation.
//!
//! [`every_term_row`] runs every term row under every chunking the fixture
//! header prescribes ([`chunkings`]): whole, byte by byte and two-way splits
//! of the concatenated input. Each chunking must meet the row's
//! expectations and give the same outcome as the whole run.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;

use std::fmt::Write as _;
use std::time::Instant;

use common::{check_table, decode, render_screen, render_state};
use tessera_grid::{Line, Mark, Point};
use tessera_term::{Request, Terminal};

const FIXTURE: &str = include_str!("fixtures/osc_spec.txt");

/// Rows in the fixture.
const ROWS: usize = 194;
/// Rows of `layer: term`, run here.
const TERM_ROWS: usize = 138;
/// Rows of `layer: parse`, counted and listed.
const PARSE_ROWS: usize = 56;
/// Up to this many input bytes a row runs under every two-way split.
const EVERY_SPLIT_MAX: usize = 512;
/// Above this many input bytes a row does not run byte by byte.
const BYTEWISE_MAX: usize = 64 * 1024;
/// Split offsets taken from each end of a larger row's input.
const EDGE_SPLITS: usize = 16;
/// From this many chunkings (besides the whole run) a row runs them on
/// several threads.
const PARALLEL_RUNS: usize = 24;

/// One `step:` line.
#[derive(Debug, Clone)]
struct Feed {
    bytes: Vec<u8>,
    /// `feed_bytewise`: one call per byte when the row runs whole.
    bytewise: bool,
}

/// Marks by row (or scrollback index), rows without marks left out.
type Marks = Vec<(usize, Vec<Mark>)>;

/// One `layer: term` row of the fixture.
#[derive(Debug, Clone)]
struct Row {
    id: String,
    rows: u16,
    cols: u16,
    steps: Vec<Feed>,
    screen: Option<String>,
    cursor: Option<Point>,
    pending_wrap: Option<bool>,
    requests: Vec<Request>,
    marks: Marks,
    main_marks: Option<Marks>,
    marks_scrollback: Marks,
    scrollback: Option<(usize, Option<(String, bool)>)>,
    alt_screen: Option<bool>,
}

impl Row {
    /// Every step's bytes, in order: the input the chunkings split.
    fn input(&self) -> Vec<u8> {
        self.steps.iter().flat_map(|f| f.bytes.clone()).collect()
    }
}

/// How [`run`] hands a row's input to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunking {
    /// One call per `feed` step; `feed_bytewise` steps one call per byte.
    Whole,
    /// One call per byte of the whole input.
    Bytewise,
    /// The whole input in two calls, split before this byte offset.
    Split(usize),
}

/// The chunkings the fixture header prescribes for `len` input bytes.
fn chunkings(len: usize) -> Vec<Chunking> {
    let mut out = vec![Chunking::Whole];
    if len <= BYTEWISE_MAX {
        out.push(Chunking::Bytewise);
    }
    if len <= EVERY_SPLIT_MAX {
        out.extend((1..len).map(Chunking::Split));
    } else {
        out.extend((1..=EDGE_SPLITS).map(Chunking::Split));
        out.extend((len - EDGE_SPLITS..len).map(Chunking::Split));
    }
    out
}

/// Decodes `text` with `literal`, except that `@rep(C,N)` and
/// `@b64rep(C,N)` expand to their bytes directly (they never contain an
/// escape, and the 1 MiB rows stay cheap to read).
fn expand(text: &str, literal: impl Fn(&str) -> Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut rest = text;
    loop {
        let next = [rest.find("@rep("), rest.find("@b64rep(")]
            .into_iter()
            .flatten()
            .min();
        let Some(at) = next else {
            out.extend(literal(rest));
            return out;
        };
        out.extend(literal(&rest[..at]));
        let tail = &rest[at..];
        let (base64, args) = match tail.strip_prefix("@b64rep(") {
            Some(args) => (true, args),
            None => (false, &tail["@rep(".len()..]),
        };
        let (inner, after) = args
            .split_once(')')
            .unwrap_or_else(|| panic!("unclosed macro in {text:?}"));
        let (ch, count) = inner
            .split_once(',')
            .unwrap_or_else(|| panic!("`{inner}`: expected C,N"));
        let &[byte] = ch.as_bytes() else {
            panic!("`{inner}`: C must be one ASCII character");
        };
        assert!(byte.is_ascii(), "`{inner}`: C must be ASCII");
        let count: usize = count.parse().expect("macro count");
        if base64 {
            // Base64 of whole 3-byte groups concatenates: encode one group.
            let group = base64_encode(&[byte; 3]).repeat(count / 3);
            out.extend_from_slice(group.as_bytes());
            out.extend_from_slice(base64_encode(&vec![byte; count % 3]).as_bytes());
        } else {
            out.extend(std::iter::repeat_n(byte, count));
        }
        rest = after;
    }
}

/// Standard base64 with padding (RFC 4648 section 4), written out here so
/// the expectation does not come from the decoder under test.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes the inside of a `".."` or `b".."` literal of a `requests:` line.
fn unescape(literal: &str, bytes: bool) -> Vec<u8> {
    expand(literal, |piece| unescape_piece(piece, bytes))
}

/// [`unescape`] of a piece without macros.
fn unescape_piece(literal: &str, bytes: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(literal.len());
    let mut chars = literal.chars();
    let push_char = |out: &mut Vec<u8>, ch: char| {
        let mut buf = [0; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    };
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            push_char(&mut out, ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some('"') => out.push(b'"'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                let byte = u8::from_str_radix(&hex, 16)
                    .unwrap_or_else(|_| panic!("`\\x{hex}` in {literal:?}"));
                if bytes {
                    out.push(byte);
                } else {
                    assert!(byte.is_ascii(), "`\\x{hex}` in a string: use \\u{{..}}");
                    out.push(byte);
                }
            }
            Some('u') => {
                let rest = chars.as_str();
                let (hex, _) = rest
                    .strip_prefix('{')
                    .and_then(|r| r.split_once('}'))
                    .unwrap_or_else(|| panic!("bad \\u escape in {literal:?}"));
                let scalar = u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .unwrap_or_else(|| panic!("bad \\u escape in {literal:?}"));
                push_char(&mut out, scalar);
                chars.by_ref().take_while(|&c| c != '}').for_each(drop);
            }
            other => panic!("unknown escape `\\{other:?}` in {literal:?}"),
        }
    }
    out
}

/// Parses a `requests: [...]` value.
fn parse_requests(text: &str) -> Vec<Request> {
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .unwrap_or_else(|| panic!("`{text}`: expected [..]"));
    let mut requests = Vec::new();
    let mut rest = inner.trim_start();
    while !rest.is_empty() {
        let name_end = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let name = &rest[..name_end];
        rest = &rest[name_end..];
        let request = if name == "Bell" {
            Request::Bell
        } else {
            let (bytes, after) = if let Some(after) = rest.strip_prefix("(b\"") {
                (true, after)
            } else if let Some(after) = rest.strip_prefix("(\"") {
                (false, after)
            } else {
                panic!("`{text}`: `{name}` needs a quoted argument");
            };
            // The literal ends at the first `"` not escaped by a `\`.
            let mut end = None;
            let mut escaped = false;
            for (i, c) in after.char_indices() {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => {
                        end = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            let end = end.unwrap_or_else(|| panic!("`{text}`: unclosed literal"));
            let value = unescape(&after[..end], bytes);
            rest = after[end + 1..]
                .strip_prefix(')')
                .unwrap_or_else(|| panic!("`{text}`: expected `)`"));
            let string = || String::from_utf8(value.clone()).expect("string literal is UTF-8");
            match (name, bytes) {
                ("SetTitle", false) => Request::SetTitle(string()),
                ("SetClipboard", false) => Request::SetClipboard(string()),
                ("SetCwd", true) => Request::SetCwd(value),
                ("ShellIntegrationHandshake", true) => Request::ShellIntegrationHandshake(value),
                _ => panic!("`{text}`: unknown request `{name}`"),
            }
        };
        requests.push(request);
        rest = rest.trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
    requests
}

fn parse_mark(name: &str) -> Mark {
    match name {
        "PromptStart" => Mark::PromptStart,
        "CommandStart" => Mark::CommandStart,
        "OutputStart" => Mark::OutputStart,
        "CommandEnd(None)" => Mark::CommandEnd { exit_code: None },
        other => {
            let code = other
                .strip_prefix("CommandEnd(")
                .and_then(|c| c.strip_suffix(')'))
                .and_then(|c| c.parse().ok())
                .unwrap_or_else(|| panic!("unknown mark `{other}`"));
            Mark::CommandEnd {
                exit_code: Some(code),
            }
        }
    }
}

/// Parses `r0=[A,B] r2=[C]` (`prefix` `r`) or `s0=[..]` (`prefix` `s`), or
/// `none`.
fn parse_marks(text: &str, prefix: char) -> Marks {
    let text = text.trim();
    if text == "none" {
        return Vec::new();
    }
    text.split_whitespace()
        .map(|entry| {
            let (index, list) = entry
                .strip_prefix(prefix)
                .and_then(|e| e.split_once('='))
                .unwrap_or_else(|| panic!("`{entry}`: expected {prefix}N=[..]"));
            let list = list
                .strip_prefix('[')
                .and_then(|l| l.strip_suffix(']'))
                .unwrap_or_else(|| panic!("`{entry}`: expected [..]"));
            let marks = list.split(',').map(|m| parse_mark(m.trim())).collect();
            (index.parse().expect("mark row"), marks)
        })
        .collect()
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

fn parse_bool(text: &str) -> bool {
    match text.trim() {
        "true" => true,
        "false" => false,
        other => panic!("`{other}`: expected true or false"),
    }
}

/// `len=N` or `len=N first=("text", wrapped=false)`.
fn parse_scrollback(text: &str) -> (usize, Option<(String, bool)>) {
    let rest = text
        .trim()
        .strip_prefix("len=")
        .unwrap_or_else(|| panic!("`{text}`: expected len=N"));
    let (len, first) = rest.split_once(' ').unwrap_or((rest, ""));
    let first = first.trim().strip_prefix("first=(\"").map(|first| {
        let (line, flags) = first
            .split_once("\", wrapped=")
            .expect("first=(\"..\", wrapped=..)");
        (line.to_owned(), parse_bool(flags.trim_end_matches(')')))
    });
    (len.parse().expect("scrollback len"), first)
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

/// A parsed row: a term row to run, or the id of a parse-layer row.
enum Parsed {
    Term(Box<Row>),
    Parse(String),
}

fn parse_row(id: String, lines: &[&str]) -> Parsed {
    let is_parse = lines.iter().any(|l| l.trim() == "layer: parse");
    if is_parse {
        return Parsed::Parse(id);
    }
    let mut size = None;
    let mut row = Row {
        id,
        rows: 0,
        cols: 0,
        steps: Vec::new(),
        screen: None,
        cursor: None,
        pending_wrap: None,
        requests: Vec::new(),
        marks: Vec::new(),
        main_marks: None,
        marks_scrollback: Vec::new(),
        scrollback: None,
        alt_screen: None,
    };
    let mut saw_requests = false;
    let mut lines = lines.iter().peekable();
    while let Some(line) = lines.next() {
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
            "layer" => assert_eq!(value.trim(), "term", "{}: layer", row.id),
            "step" => {
                let (bytewise, payload) = if let Some(p) = value.strip_prefix("feed_bytewise ") {
                    (true, p)
                } else if let Some(p) = value.strip_prefix("feed ") {
                    (false, p)
                } else {
                    panic!("{}: unknown step `{value}`", row.id);
                };
                row.steps.push(Feed {
                    bytes: expand(payload, decode),
                    bytewise,
                });
            }
            "screen" => {
                if value.trim() == "not asserted" {
                    continue;
                }
                assert!(value.trim().is_empty(), "{}: screen `{value}`", row.id);
                let mut screen = String::new();
                while let Some(text) = lines.peek().and_then(|l| l.strip_prefix('>')) {
                    screen.push_str(text);
                    screen.push('\n');
                    lines.next();
                }
                row.screen = Some(screen);
            }
            "cursor" => row.cursor = Some(parse_point(value)),
            "pending_wrap" => row.pending_wrap = Some(parse_bool(value)),
            "requests" => {
                row.requests = parse_requests(value);
                saw_requests = true;
            }
            "marks" => row.marks = parse_marks(value, 'r'),
            "main_marks" => row.main_marks = Some(parse_marks(value, 'r')),
            "marks_scrollback" => row.marks_scrollback = parse_marks(value, 's'),
            "scrollback" => row.scrollback = Some(parse_scrollback(value)),
            "screen_is" => {
                row.alt_screen = Some(match value.trim() {
                    "main" => false,
                    "alt" => true,
                    other => panic!("{}: screen_is `{other}`", row.id),
                });
            }
            "cite" | "note" | "flags" => {}
            other => panic!("{}: unknown key `{other}`", row.id),
        }
    }
    (row.rows, row.cols) = size.unwrap_or_else(|| panic!("{}: no size", row.id));
    assert!(saw_requests, "{}: no requests line", row.id);
    assert!(!row.steps.is_empty(), "{}: no step", row.id);
    Parsed::Term(Box::new(row))
}

/// Everything a row can check, read from a terminal after its input.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    /// [`render_state`] without the requests: screen, cursor, pending wrap,
    /// modes and scrollback text.
    state: String,
    requests: Vec<Request>,
    alt_screen: bool,
    marks: Marks,
    marks_scrollback: Marks,
    /// The main screen's marks after `CSI ? 1049 l`, when the row asks.
    main_marks: Option<Marks>,
}

fn marks_of<'a>(lines: impl IntoIterator<Item = &'a Line>) -> Marks {
    lines
        .into_iter()
        .enumerate()
        .filter(|(_, line)| !line.marks.is_empty())
        .map(|(i, line)| (i, line.marks.clone()))
        .collect()
}

/// Feeds `row`'s input to a fresh terminal as `chunking` says.
fn run(row: &Row, input: &[u8], chunking: Chunking) -> Terminal {
    let mut term = Terminal::new(row.rows, row.cols);
    match chunking {
        Chunking::Whole => {
            for step in &row.steps {
                if step.bytewise {
                    for byte in &step.bytes {
                        term.advance(std::slice::from_ref(byte));
                    }
                } else {
                    term.advance(&step.bytes);
                }
            }
        }
        Chunking::Bytewise => {
            for byte in input {
                term.advance(std::slice::from_ref(byte));
            }
        }
        Chunking::Split(at) => {
            let (head, tail) = input.split_at(at);
            term.advance(head);
            term.advance(tail);
        }
    }
    term
}

fn outcome(row: &Row, mut term: Terminal) -> Outcome {
    let requests = term.take_requests();
    let grid = term.grid();
    let marks = marks_of(grid.visible_lines());
    let marks_scrollback =
        marks_of((0..grid.scrollback_len()).filter_map(|i| grid.scrollback_line(i)));
    let alt_screen = term.modes().alt_screen;
    let state = render_state(&term, &[]);
    let main_marks = row.main_marks.as_ref().map(|_| {
        term.advance(b"\x1b[?1049l");
        marks_of(term.grid().visible_lines())
    });
    Outcome {
        state,
        requests,
        alt_screen,
        marks,
        marks_scrollback,
        main_marks,
    }
}

/// Requests for a message, long strings shortened.
fn show(requests: &[Request]) -> String {
    let text = format!("{requests:?}");
    if text.len() > 300 {
        let cut = (0..=300)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        format!("{}... ({} bytes)", &text[..cut], text.len())
    } else {
        text
    }
}

/// Runs `row` under `chunking`: its outcome and every mismatch with the
/// row's expectations.
fn check_run(row: &Row, input: &[u8], chunking: Chunking) -> (Outcome, Vec<String>) {
    let term = run(row, input, chunking);
    let mut errors = Vec::new();
    let mut fail = |what: String| errors.push(format!("{} [{chunking:?}]: {what}", row.id));
    if let Some(screen) = &row.screen {
        let got = render_screen(&term);
        if got != *screen {
            fail(format!("screen\n{got}!= expected\n{screen}"));
        }
    }
    let grid = term.grid();
    if let Some(cursor) = row.cursor
        && grid.cursor() != cursor
    {
        fail(format!("cursor {:?} != {cursor:?}", grid.cursor()));
    }
    if let Some(pending) = row.pending_wrap
        && grid.pending_wrap() != pending
    {
        fail(format!("pending_wrap {} != {pending}", grid.pending_wrap()));
    }
    if let Some((len, first)) = &row.scrollback {
        if grid.scrollback_len() != *len {
            fail(format!("scrollback len {} != {len}", grid.scrollback_len()));
        }
        if let Some((text, wrapped)) = first {
            let got = grid.scrollback_line(0).map(|l| (l.text(), l.wrapped));
            if got.as_ref() != Some(&(text.clone(), *wrapped)) {
                fail(format!("scrollback[0] {got:?} != {:?}", (text, wrapped)));
            }
        }
    }
    let got = outcome(row, term);
    if got.requests != row.requests {
        fail(format!(
            "requests {} != {}",
            show(&got.requests),
            show(&row.requests)
        ));
    }
    if let Some(alt) = row.alt_screen
        && got.alt_screen != alt
    {
        fail(format!("alt screen {} != {alt}", got.alt_screen));
    }
    if got.marks != row.marks {
        fail(format!("marks {:?} != {:?}", got.marks, row.marks));
    }
    if got.marks_scrollback != row.marks_scrollback {
        fail(format!(
            "scrollback marks {:?} != {:?}",
            got.marks_scrollback, row.marks_scrollback
        ));
    }
    if got.main_marks != row.main_marks {
        fail(format!(
            "main screen marks {:?} != {:?}",
            got.main_marks, row.main_marks
        ));
    }
    (got, errors)
}

/// Runs `row` under every chunking; panics with every mismatch, including
/// any chunking whose outcome differs from the whole run's.
///
/// A row with [`PARALLEL_RUNS`] chunkings or more runs them on several
/// threads, to keep the table within its time budget (0.5 s): the 1.4 MB
/// clipboard rows above all, whose every run parses the whole input.
fn check_row(row: &Row) {
    let input = row.input();
    let (whole, mut errors) = check_run(row, &input, Chunking::Whole);
    let others: Vec<Chunking> = chunkings(input.len())
        .into_iter()
        .filter(|&c| c != Chunking::Whole)
        .collect();
    let check = |chunking: Chunking| {
        let (got, mut errors) = check_run(row, &input, chunking);
        if got != whole {
            let mut detail = String::new();
            if got.state != whole.state {
                let _ = write!(detail, "\n{}!= whole\n{}", got.state, whole.state);
            }
            errors.push(format!(
                "{} [{chunking:?}]: differs from the whole run{detail}",
                row.id
            ));
        }
        errors
    };
    if others.len() < PARALLEL_RUNS {
        errors.extend(others.into_iter().flat_map(check));
    } else {
        let threads = std::thread::available_parallelism().map_or(4, usize::from);
        let per_thread = others.len().div_ceil(threads).max(1);
        std::thread::scope(|scope| {
            let handles: Vec<_> = others
                .chunks(per_thread)
                .map(|group| {
                    scope.spawn(|| group.iter().flat_map(|&c| check(c)).collect::<Vec<_>>())
                })
                .collect();
            for handle in handles {
                errors.extend(handle.join().expect("a chunking run panicked"));
            }
        });
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn every_term_row() {
    let start = Instant::now();
    let parsed: Vec<Parsed> = raw_rows(FIXTURE)
        .into_iter()
        .map(|(id, lines)| parse_row(id, &lines))
        .collect();
    assert_eq!(parsed.len(), ROWS, "rows in the fixture");
    let mut term_rows = Vec::new();
    let mut parse_rows = Vec::new();
    for row in parsed {
        match row {
            Parsed::Term(row) => term_rows.push(*row),
            Parsed::Parse(id) => parse_rows.push(id),
        }
    }
    eprintln!(
        "{} parse-layer rows not run here (unit tests in tessera-grid and \
         tessera-shell-integration): {}",
        parse_rows.len(),
        parse_rows.join(", ")
    );
    assert_eq!(parse_rows.len(), PARSE_ROWS, "parse-layer rows");
    assert_eq!(term_rows.len(), TERM_ROWS, "term rows");
    check_table(
        "osc_spec.txt",
        TERM_ROWS,
        term_rows.iter().map(|row| (row.id.clone(), row)),
        check_row,
    );
    eprintln!(
        "{TERM_ROWS} term rows passed under every chunking in {:?}",
        start.elapsed()
    );
}

#[test]
fn harness_base64_encoder_matches_rfc_4648_vectors() {
    // RFC 4648 section 10.
    for (plain, encoded) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64_encode(plain.as_bytes()), encoded, "{plain:?}");
    }
    let expanded = |text: &str| expand(text, |piece| piece.as_bytes().to_vec());
    assert_eq!(expanded("x@rep(a,3)y@b64rep(f,1)z@x"), b"xaaayZg==z@x");
    for n in 0..8 {
        assert_eq!(
            expanded(&format!("@b64rep(a,{n})")),
            base64_encode(&vec![b'a'; n]).as_bytes(),
            "@b64rep(a,{n})"
        );
    }
}

#[test]
fn chunkings_follow_the_fixture_header() {
    assert_eq!(chunkings(3).len(), 1 + 1 + 2);
    assert_eq!(chunkings(512).len(), 1 + 1 + 511);
    let large = chunkings(513);
    assert_eq!(large.len(), 1 + 1 + 32);
    assert!(large.contains(&Chunking::Split(16)) && large.contains(&Chunking::Split(497)));
    assert!(!large.contains(&Chunking::Split(17)) && large.contains(&Chunking::Split(512)));
    let huge = chunkings(1_398_110);
    assert_eq!(huge.len(), 1 + 32);
    assert!(!huge.contains(&Chunking::Bytewise));
}
