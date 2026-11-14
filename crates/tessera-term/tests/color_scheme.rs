//! The colour-scheme query `CSI ? 996 n` and mode 2031 (#269): the reply
//! `CSI ? 997 ; 1 n` (dark) or `; 2 n` (light), DECSET/DECRST/DECRQM 2031
//! and the unsolicited report while it is set.
//!
//! Expected behaviour comes from `tests/fixtures/color_scheme_spec.txt`,
//! whose header lists the decisions (D-LUMA, D-OSC, D-DEDUP, D-RESET, ...)
//! the rows encode. The layout, read here:
//!
//! - `### <id>` starts a row; `size: RxC` gives the terminal size;
//! - `step: feed <payload>` feeds the payload (`\e` for ESC and the escapes
//!   of [`common::decode`]);
//! - `step: set theme <spec> [(N times)]` calls
//!   [`Terminal::set_theme_colors`] (N times), `<spec>` being `default`,
//!   `D`, `D2`, `L`, `bg #rrggbb` or `fg #rrggbb bg #rrggbb` (the header's
//!   themes); every row starts on [`Terminal::new`];
//! - `step: set size RxC` calls [`Terminal::resize`];
//! - `reply: <bytes>|none` checks the [`Request::Reply`] bytes queued since
//!   the previous `reply:`, concatenated, the unsolicited reports included;
//! - `screen:` checks the first screen rows against the `>` lines below it
//!   ([`common::render_screen`]) and every row after them is blank;
//! - `cite:`, `note:` and `flags:` are not checked.
//!
//! [`every_row`] runs every row whole, byte by byte and with each feed step
//! split in two at every offset, and expects the same results each time.
//!
//! The S6 tests below bound the reply path (ADR 0024, docs/security.md):
//! one reply per query under a 1 MiB flood, at most one report per scheme
//! change under a storm of theme changes and OSC 11 sets, nothing for mode
//! toggles, one reply per DECRQM.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;

use common::{decode, render_screen};
use tessera_term::{Request, Rgb, Terminal, ThemeColors};

const FIXTURE: &str = include_str!("fixtures/color_scheme_spec.txt");
const FIXTURE_NAME: &str = "fixtures/color_scheme_spec.txt";

/// The rows of the fixture.
const ROWS: usize = 65;

/// One change applied to a terminal, in the theme storm.
type Step = Box<dyn Fn(&mut Terminal)>;

/// The header's theme D (`osc_color_spec`'s TEST THEME T): dark.
fn theme_d() -> ThemeColors {
    ThemeColors {
        fg: Rgb::new(0xd0, 0xd0, 0xd0),
        bg: Rgb::new(0x10, 0x10, 0x10),
        cursor: Rgb::new(0xff, 0xcc, 0x00),
        palette: std::array::from_fn(|i| {
            let i = u8::try_from(i).expect("index below 256");
            Rgb::new(i, 255 - i, i ^ 0x55)
        }),
    }
}

/// The header's theme D2 (`osc_color_spec`'s TEST THEME T2): dark, every
/// colour different from D.
fn theme_d2() -> ThemeColors {
    ThemeColors {
        fg: Rgb::new(0xaa, 0xbb, 0xcc),
        bg: Rgb::new(0x22, 0x33, 0x44),
        cursor: Rgb::new(0x00, 0xff, 0x00),
        palette: std::array::from_fn(|i| {
            let i = u8::try_from(i).expect("index below 256");
            Rgb::new(255 - i, i, i ^ 0xaa)
        }),
    }
}

/// The header's theme L: light, palette as D.
fn theme_l() -> ThemeColors {
    ThemeColors {
        fg: Rgb::new(0x20, 0x20, 0x20),
        bg: Rgb::new(0xf0, 0xf0, 0xf0),
        cursor: Rgb::new(0, 0, 0),
        ..theme_d()
    }
}

fn parse_hex(text: &str) -> Rgb {
    let digits = text
        .strip_prefix('#')
        .filter(|d| d.len() == 6)
        .unwrap_or_else(|| panic!("colour `{text}`: expected #rrggbb"));
    let byte = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).expect("hex colour");
    Rgb::new(byte(0), byte(2), byte(4))
}

/// A `set theme` spec.
fn parse_theme(spec: &str) -> ThemeColors {
    let words: Vec<&str> = spec.split_whitespace().collect();
    match words.as_slice() {
        ["default"] => ThemeColors::default(),
        ["D"] => theme_d(),
        ["D2"] => theme_d2(),
        ["L"] => theme_l(),
        ["bg", bg] => ThemeColors {
            bg: parse_hex(bg),
            ..theme_d()
        },
        ["fg", fg, "bg", bg] => ThemeColors {
            fg: parse_hex(fg),
            bg: parse_hex(bg),
            ..theme_d()
        },
        _ => panic!("unknown theme `{spec}`"),
    }
}

/// One line of a row that does something, in fixture order.
#[derive(Debug, Clone)]
enum Item {
    /// `step: feed`.
    Feed(Vec<u8>),
    /// `step: set size`.
    Size(u16, u16),
    /// `step: set theme`: the theme and how often it is set.
    Theme(Box<ThemeColors>, usize),
    /// `reply:`: the expected reply bytes, `None` for `none`.
    Reply(Option<Vec<u8>>),
}

/// One row of the fixture.
#[derive(Debug, Clone)]
struct Row {
    id: String,
    rows: u16,
    cols: u16,
    items: Vec<Item>,
    /// The first screen rows, one `\n`-terminated line each.
    screen: Option<Vec<String>>,
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

/// `step:` lines.
fn parse_step(text: &str) -> Item {
    if let Some(payload) = text.strip_prefix("feed ") {
        Item::Feed(decode_payload(payload))
    } else if let Some(size) = text.strip_prefix("set size ") {
        let (rows, cols) = size.trim().split_once('x').expect("size RxC");
        Item::Size(rows.parse().expect("rows"), cols.parse().expect("cols"))
    } else if let Some(spec) = text.strip_prefix("set theme ") {
        let (spec, times) = match spec.trim().strip_suffix(" times)") {
            Some(rest) => {
                let (spec, count) = rest.rsplit_once(" (").expect("`<spec> (N times)`");
                (spec, count.replace(',', "").parse().expect("repeat count"))
            }
            None => (spec.trim(), 1),
        };
        Item::Theme(Box::new(parse_theme(spec)), times)
    } else {
        panic!("unknown step `{text}`");
    }
}

fn parse_row(id: String, lines: &[&str]) -> Row {
    let mut row = Row {
        id,
        rows: 0,
        cols: 0,
        items: Vec::new(),
        screen: None,
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
            "size" => {
                let (rows, cols) = value.trim().split_once('x').expect("size RxC");
                row.rows = rows.parse().expect("rows");
                row.cols = cols.parse().expect("cols");
            }
            "step" => row.items.push(parse_step(value)),
            "reply" => {
                let reply = (value.trim() != "none").then(|| decode_payload(value.trim()));
                row.items.push(Item::Reply(reply));
            }
            "screen" => {
                let mut screen = Vec::new();
                while let Some(text) = lines.peek().and_then(|l| l.strip_prefix('>')) {
                    screen.push(format!("{text}\n"));
                    lines.next();
                }
                row.screen = Some(screen);
            }
            "cite" | "note" | "flags" => {}
            other => panic!("{}: unknown key `{other}`", row.id),
        }
    }
    assert!(row.rows > 0 && row.cols > 0, "{}: no size", row.id);
    row
}

fn rows(fixture: &str) -> Vec<Row> {
    let mut raw: Vec<(String, Vec<&str>)> = Vec::new();
    for line in fixture.lines() {
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

/// The replies queued since the last call; panics on any other request
/// (no row queues one).
fn take_replies(term: &mut Terminal, id: &str) -> Vec<u8> {
    let mut replies = Vec::new();
    for request in term.take_requests() {
        match request {
            Request::Reply(bytes) => replies.extend_from_slice(&bytes),
            other => panic!("{id}: unexpected request {other:?}"),
        }
    }
    replies
}

/// Runs `row` with `chunking` and returns one message per mismatch.
fn check_row(row: &Row, chunking: Chunking) -> Vec<String> {
    let mut errors = Vec::new();
    let mut fail = |what: String| errors.push(format!("{} [{chunking:?}]: {what}", row.id));
    let mut term = Terminal::new(row.rows, row.cols);
    let mut got = Vec::new();
    for (index, item) in row.items.iter().enumerate() {
        match item {
            Item::Feed(bytes) => feed(&mut term, bytes, index, chunking),
            Item::Size(rows, cols) => term.resize(*rows, *cols),
            Item::Theme(theme, times) => {
                for _ in 0..*times {
                    term.set_theme_colors((**theme).clone());
                }
            }
            Item::Reply(want) => {
                got.extend(take_replies(&mut term, &row.id));
                let shown = std::mem::take(&mut got);
                let ok = match want {
                    None => shown.is_empty(),
                    Some(want) => shown == *want,
                };
                if !ok {
                    fail(format!(
                        "reply {:?} != {:?}",
                        String::from_utf8_lossy(&shown),
                        want.as_deref().map(String::from_utf8_lossy)
                    ));
                }
            }
        }
        // Collected after every step, so a request queued by one step is
        // never mistaken for the next one's.
        got.extend(take_replies(&mut term, &row.id));
    }
    if let Some(screen) = &row.screen {
        let shown = render_screen(&term);
        let shown: Vec<&str> = shown.split_inclusive('\n').collect();
        let blank = shown.iter().skip(screen.len()).all(|line| *line == "\n");
        if shown.len() < screen.len() || shown[..screen.len()] != screen[..] || !blank {
            fail(format!("screen\n{}!=\n{}", shown.concat(), screen.concat()));
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
    let rows = rows(FIXTURE);
    let headings = FIXTURE.lines().filter(|l| l.starts_with("### ")).count();
    assert_eq!(headings, ROWS, "rows in {FIXTURE_NAME}");
    common::check_table(
        FIXTURE_NAME,
        headings,
        rows.iter().map(|row| (row.id.clone(), row)),
        |row| {
            let errors: Vec<String> = chunkings(row)
                .into_iter()
                .flat_map(|chunking| check_row(row, chunking))
                .collect();
            // The first few say why; the count says how often.
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

/// The 7-bit reply for `light`.
fn report(light: bool) -> &'static [u8] {
    if light {
        b"\x1b[?997;2n"
    } else {
        b"\x1b[?997;1n"
    }
}

/// D-LUMA, computed here independently of the crate.
fn is_light(bg: Rgb) -> bool {
    299 * u32::from(bg.r) + 587 * u32::from(bg.g) + 114 * u32::from(bg.b) > 127_500
}

/// [`Terminal::advance`] slice the app-core reader feeds under one lock
/// hold (`ADVANCE_SLICE`), after which it drains the requests.
const SLICE: usize = 4096;

/// S6: a 1 MiB flood of `CSI ? 996 n` answers each query exactly once
/// with the 9-byte reply, the replies of one reader slice are bounded by
/// its queries (at most 9 bytes per 7 input bytes, plus one query cut
/// across slices), nothing else is queued, and the screen and cursor do
/// not change. The outbox bound past this is app-core's
/// (`a_colour_scheme_report_into_a_full_outbox_is_dropped_and_counted`).
#[test]
fn a_flood_of_scheme_queries_is_one_reply_each() {
    const QUERY: &[u8] = b"\x1b[?996n";
    let queries = (1 << 20) / QUERY.len();
    let flood = QUERY.repeat(queries);
    let mut term = Terminal::new(24, 80);
    term.set_theme_colors(theme_l());
    let before = render_screen(&term);
    let mut replies = 0;
    let mut worst = 0;
    for slice in flood.chunks(SLICE) {
        term.advance(slice);
        let bytes = take_replies(&mut term, "flood");
        worst = worst.max(bytes.len());
        assert!(
            bytes.len() <= (slice.len() / QUERY.len() + 1) * report(true).len(),
            "{} reply bytes for a slice of {}",
            bytes.len(),
            slice.len()
        );
        assert!(
            bytes.chunks(report(true).len()).all(|r| r == report(true)),
            "every reply is the light report"
        );
        replies += bytes.len() / report(true).len();
    }
    assert_eq!(replies, queries, "one reply per query");
    assert!(worst <= (SLICE / QUERY.len() + 1) * report(true).len());
    assert_eq!(render_screen(&term), before, "the screen is untouched");
}

/// S6 (D-DEDUP, D-FLOOD): 10 000 rounds of theme changes and OSC 11 sets
/// and resets under 2031, undrained, queue exactly one report per change
/// of the scheme (computed here) and none per other step; drained every
/// 100 steps as a reader would, every report arrives, in order, each the
/// new scheme, so the reports alternate.
#[test]
fn a_theme_storm_queues_one_report_per_scheme_change() {
    const ROUNDS: usize = 10_000;
    let bgs = [b"#ffffff".as_slice(), b"#000000", b"#202020", b"#f0f0f0"];
    for drain_every in [usize::MAX, 100] {
        let mut term = Terminal::new(24, 80);
        term.set_theme_colors(theme_d());
        term.advance(b"\x1b[?2031h");
        let mut light = false;
        let mut want = Vec::new();
        let mut got = Vec::new();
        let mut changes = 0;
        for round in 0..ROUNDS {
            // A theme, then an OSC 11 set and, every other round, its reset.
            let theme = match round % 3 {
                0 => theme_l(),
                1 => theme_d(),
                _ => theme_d2(),
            };
            let osc = [b"\x1b]11;".as_slice(), bgs[round % bgs.len()], b"\x07"].concat();
            let mut steps: Vec<Step> = vec![
                Box::new(move |t: &mut Terminal| t.set_theme_colors(theme.clone())),
                Box::new(move |t: &mut Terminal| t.advance(&osc)),
            ];
            if round % 2 == 1 {
                steps.push(Box::new(|t: &mut Terminal| t.advance(b"\x1b]111\x07")));
            }
            for step in steps {
                step(&mut term);
                let now = is_light(term.colors().bg);
                if now != light {
                    light = now;
                    changes += 1;
                    want.extend_from_slice(report(now));
                }
            }
            if round % drain_every.min(ROUNDS) == 0 {
                got.extend(take_replies(&mut term, "storm"));
            }
        }
        got.extend(take_replies(&mut term, "storm"));
        assert!(changes > ROUNDS, "the storm changes the scheme: {changes}");
        assert_eq!(
            got.len(),
            changes * report(true).len(),
            "one report per change"
        );
        assert!(got == want, "every report, in order, the new scheme");
        let alternates = got
            .chunks(report(true).len())
            .zip(got.chunks(report(true).len()).skip(1))
            .all(|(a, b)| a != b);
        assert!(alternates, "consecutive reports differ");
    }
}

/// S6: 100 000 DECSET/DECRST 2031 toggles, interleaved with re-applying
/// the same theme, queue nothing and leave nothing behind; the mode ends
/// as the last toggle left it.
#[test]
fn toggling_2031_sends_nothing() {
    let mut term = Terminal::new(24, 80);
    term.set_theme_colors(theme_l());
    let toggles = b"\x1b[?2031h\x1b[?2031l".repeat(50_000);
    for slice in toggles.chunks(SLICE) {
        term.advance(slice);
        term.set_theme_colors(theme_l());
        assert_eq!(take_replies(&mut term, "toggles"), b"");
    }
    assert!(!term.modes().color_scheme_updates);
    term.advance(b"\x1b[?2031h");
    assert!(term.modes().color_scheme_updates);
    assert_eq!(take_replies(&mut term, "toggles"), b"");
}

/// S6: a flood of `CSI ? 2031 $ p` answers each request exactly once.
#[test]
fn a_flood_of_2031_mode_requests_is_one_reply_each() {
    const REQUEST: &[u8] = b"\x1b[?2031$p";
    const REPLY: &[u8] = b"\x1b[?2031;1$y";
    let requests = (1 << 20) / REQUEST.len();
    let flood = REQUEST.repeat(requests);
    let mut term = Terminal::new(24, 80);
    term.advance(b"\x1b[?2031h");
    let mut replies = 0;
    for slice in flood.chunks(SLICE) {
        term.advance(slice);
        let bytes = take_replies(&mut term, "decrqm");
        assert!(bytes.chunks(REPLY.len()).all(|r| r == REPLY));
        assert!(bytes.len() <= (slice.len() / REQUEST.len() + 1) * REPLY.len());
        replies += bytes.len() / REPLY.len();
    }
    assert_eq!(replies, requests);
}

/// [`Terminal::take_replies`] drains the replies, joined in order, and
/// leaves every other request queued in its order.
#[test]
fn take_replies_leaves_the_other_requests() {
    let mut term = Terminal::new(24, 80);
    term.advance(b"\x1b[5n\x07\x1b]2;title\x07\x1b[?996n\x07");
    assert_eq!(term.take_replies(), b"\x1b[0n\x1b[?997;1n");
    assert_eq!(term.take_replies(), b"");
    assert_eq!(
        term.take_requests(),
        [
            Request::Bell,
            Request::SetTitle("title".into()),
            Request::Bell
        ]
    );
}
