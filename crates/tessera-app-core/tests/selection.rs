//! The selection fixture, `tests/fixtures/selection_spec.txt` (#30): every
//! `layer: core` row runs through [`App`] on a session without a child
//! (`App::spawn_test_session`, bytes fed with `App::feed_for_test`), and
//! every `layer: url` row is a pure test of [`url::url_at`].
//!
//! The fixture's `###` layout has its own parser here. A row with a key,
//! step or value it cannot interpret fails the test, and so does a file
//! with another number of rows than the 116 (79 core, 37 url) it holds.

use std::fmt::Write as _;
use std::panic::{self, AssertUnwindSafe};

use tessera_app_core::{
    App, Config, ConfigPaths, Granularity, Point, PtySize, ScrollTarget, SessionId, url,
};

const FIXTURE: &str = include_str!("fixtures/selection_spec.txt");
const CORE_ROWS: usize = 79;
const URL_ROWS: usize = 37;
/// The history cap of a row without `scrollback:`.
const DEFAULT_SCROLLBACK: usize = 100;

/// Every key a row may have.
const KEYS: [&str; 14] = [
    "layer",
    "size",
    "scrollback",
    "step",
    "sel",
    "has_selection",
    "link",
    "text",
    "cite",
    "note",
    "input",
    "at",
    "url",
    "span",
];

/// One `###` row: its id and its `key: value` lines in order.
#[derive(Debug)]
struct Row {
    id: String,
    fields: Vec<(String, String)>,
}

impl Row {
    fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn need(&self, key: &str) -> Result<&str, String> {
        self.get(key).ok_or_else(|| format!("no `{key}:`"))
    }
}

/// Splits the fixture into rows. Comment lines (`#` but not `###`) and
/// blank lines separate them; any other line must be `key: value`.
fn parse(text: &str) -> Result<Vec<Row>, String> {
    let mut rows: Vec<Row> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if let Some(id) = line.strip_prefix("### ") {
            rows.push(Row {
                id: id.trim().to_owned(),
                fields: Vec::new(),
            });
            continue;
        }
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let row = rows
            .last_mut()
            .ok_or_else(|| format!("line {}: field before the first row", number + 1))?;
        let (key, value) = line
            .split_once(": ")
            .or_else(|| line.strip_suffix(':').map(|k| (k, "")))
            .ok_or_else(|| format!("line {}: not `key: value`: {line}", number + 1))?;
        if !KEYS.contains(&key) {
            return Err(format!("row {}: unknown key `{key}`", row.id));
        }
        row.fields.push((key.to_owned(), value.to_owned()));
    }
    Ok(rows)
}

/// The scalar a `\u{H..}` escape names; `rest` starts after `\u`.
fn scalar(rest: &mut std::str::Chars<'_>) -> Result<char, String> {
    if rest.next() != Some('{') {
        return Err("`\\u` without `{`".into());
    }
    let hex: String = rest.by_ref().take_while(|&c| c != '}').collect();
    let value = u32::from_str_radix(&hex, 16).map_err(|e| format!("bad \\u{{{hex}}}: {e}"))?;
    char::from_u32(value).ok_or_else(|| format!("\\u{{{hex}}} is no scalar"))
}

/// The two hex digits of a `\xNN` escape; `rest` starts after `\x`.
fn byte(rest: &mut std::str::Chars<'_>) -> Result<u8, String> {
    let hex: String = rest.by_ref().take(2).collect();
    u8::from_str_radix(&hex, 16).map_err(|e| format!("bad \\x{hex}: {e}"))
}

/// A `feed` payload: `\xNN` raw bytes, `\r \n \t \\`, `\u{H}` scalars.
fn payload(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('x') => out.push(byte(&mut chars)?),
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some('u') => {
                let mut buf = [0; 4];
                out.extend_from_slice(scalar(&mut chars)?.encode_utf8(&mut buf).as_bytes());
            }
            other => return Err(format!("unknown feed escape \\{other:?}")),
        }
    }
    Ok(out)
}

/// A `text:` value: `none` or a quoted string with `\n \t \\ \" \xNN
/// \u{H}` escapes.
fn quoted(text: &str) -> Result<Option<String>, String> {
    if text == "none" {
        return Ok(None);
    }
    let inner = text
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .ok_or_else(|| format!("text is neither `none` nor quoted: {text}"))?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('x') => out.push(char::from(byte(&mut chars)?)),
            Some('u') => out.push(scalar(&mut chars)?),
            other => return Err(format!("unknown text escape \\{other:?}")),
        }
    }
    Ok(Some(out))
}

/// A `url` row's `input:`: `\u{H}` escapes and `@rep(C,N)` (character C
/// N times).
fn url_input(text: &str) -> Result<Vec<char>, String> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("@rep(") {
            let (args, tail) = after
                .split_once(')')
                .ok_or_else(|| format!("unclosed @rep in {text}"))?;
            let (c, n) = args
                .split_once(',')
                .ok_or_else(|| format!("bad @rep({args})"))?;
            let mut c = c.chars();
            let (Some(c), None) = (c.next(), c.next()) else {
                return Err(format!("@rep needs one character: {args}"));
            };
            let n: usize = n.parse().map_err(|e| format!("bad @rep count {n}: {e}"))?;
            out.extend(std::iter::repeat_n(c, n));
            rest = tail;
        } else if let Some(after) = rest.strip_prefix("\\u") {
            let mut chars = after.chars();
            out.push(scalar(&mut chars)?);
            rest = chars.as_str();
        } else {
            let mut chars = rest.chars();
            out.extend(chars.next());
            rest = chars.as_str();
        }
    }
    Ok(out)
}

/// `R,C` as a point.
fn point(text: &str) -> Result<Point, String> {
    let (row, col) = text
        .split_once(',')
        .ok_or_else(|| format!("not `R,C`: {text}"))?;
    let num = |s: &str| {
        s.trim()
            .parse::<u16>()
            .map_err(|e| format!("bad `{s}`: {e}"))
    };
    Ok(Point::new(num(row)?, num(col)?))
}

/// `RxC` as a size.
fn size(text: &str) -> Result<PtySize, String> {
    let (rows, cols) = text
        .split_once('x')
        .ok_or_else(|| format!("not `RxC`: {text}"))?;
    let num = |s: &str| s.parse::<u16>().map_err(|e| format!("bad `{s}`: {e}"));
    Ok(PtySize::new(num(rows)?, num(cols)?))
}

/// The selection a frame would draw, as the fixture writes it.
fn frame_selection(app: &App, id: SessionId) -> String {
    let (selection, _) = app
        .view_decorations_for_test(id)
        .expect("decorations of a live session");
    selection.map_or_else(
        || "none".to_owned(),
        |sel| {
            let kind = match sel.kind {
                tessera_render::SelectionKind::Linear => "linear",
                tessera_render::SelectionKind::Block => "block",
            };
            format!(
                "{kind} ({},{})-({},{})",
                sel.start.row, sel.start.col, sel.end.row, sel.end.col
            )
        },
    )
}

/// Runs one `layer: core` row on its own session of `app`.
fn run_core(app: &App, row: &Row) -> Result<(), String> {
    let size = size(row.need("size")?)?;
    let scrollback = row
        .get("scrollback")
        .map_or(Ok(DEFAULT_SCROLLBACK), str::parse)
        .map_err(|e| format!("bad scrollback: {e}"))?;
    let id = app.spawn_test_session(size).map_err(|e| e.to_string())?;
    let result = run_steps(app, id, row, scrollback);
    app.close_session(id).map_err(|e| e.to_string())?;
    result
}

fn run_steps(app: &App, id: SessionId, row: &Row, scrollback: usize) -> Result<(), String> {
    app.set_max_scrollback_for_test(id, scrollback)
        .map_err(|e| e.to_string())?;
    let mut link = None;
    let mut linked = false;
    let err = |e: tessera_app_core::Error| e.to_string();
    for (key, step) in &row.fields {
        if key != "step" {
            continue;
        }
        let (verb, arg) = step.split_once(' ').unwrap_or((step.as_str(), ""));
        match verb {
            "feed" => app.feed_for_test(id, &payload(arg)?).map_err(err)?,
            "begin" => {
                let mut words = arg.split_whitespace();
                let at = point(words.next().ok_or("begin without a point")?)?;
                let granularity = match words.next() {
                    Some("char") => Granularity::Char,
                    Some("word") => Granularity::Word,
                    Some("line") => Granularity::Line,
                    other => return Err(format!("bad granularity {other:?}")),
                };
                let block = match words.next() {
                    None => false,
                    Some("block") => true,
                    Some(other) => return Err(format!("bad begin flag {other}")),
                };
                app.select_begin(id, at, granularity, block).map_err(err)?;
            }
            "extend" => app.select_extend(id, point(arg)?).map_err(err)?,
            "adjust" => app.select_adjust(id, point(arg)?).map_err(err)?,
            "all" => app.select_all(id).map_err(err)?,
            "clear" => app.select_clear(id).map_err(err)?,
            "link" => {
                link = app.link_at(id, point(arg)?).map_err(err)?;
                linked = true;
            }
            "scroll" => {
                let lines = arg.parse().map_err(|e| format!("bad scroll {arg}: {e}"))?;
                app.scroll_to(id, ScrollTarget::Offset(lines))
                    .map_err(err)?;
            }
            "resize" => app.resize(id, size(arg)?).map_err(err)?,
            "clear_scrollback" => app.clear_scrollback(id).map_err(err)?,
            other => return Err(format!("unknown step `{other}`")),
        }
    }
    check(app, id, row, link, linked)
}

/// Checks a row's `sel:`, `has_selection:`, `text:` and `link:` after its
/// steps; `link` is the last `link` step's result, `linked` whether there
/// was one.
fn check(
    app: &App,
    id: SessionId,
    row: &Row,
    link: Option<tessera_app_core::Link>,
    linked: bool,
) -> Result<(), String> {
    let err = |e: tessera_app_core::Error| e.to_string();
    let mut failures = String::new();
    if let Some(want) = row.get("sel") {
        let got = frame_selection(app, id);
        if got != want {
            let _ = write!(failures, "sel: want `{want}`, got `{got}`; ");
        }
    }
    let want_has = match row.get("has_selection") {
        Some("true") => true,
        Some("false") => false,
        Some(other) => return Err(format!("bad has_selection `{other}`")),
        None => row.get("sel").is_some_and(|sel| sel != "none"),
    };
    let has = app.has_selection(id).map_err(err)?;
    if has != want_has {
        let _ = write!(failures, "has_selection: want {want_has}, got {has}; ");
    }
    if let Some(want) = row.get("text") {
        let want = quoted(want)?;
        let got = app.selection_text(id).map_err(err)?;
        if got != want {
            let _ = write!(failures, "text: want {want:?}, got {got:?}; ");
        }
    }
    match (row.get("link"), linked) {
        (Some(want), true) => {
            let got = link.map_or_else(
                || "none".to_owned(),
                |link| {
                    let spans: Vec<String> = link
                        .span
                        .iter()
                        .map(|(r, c)| format!("({r},{})-({r},{})", c.start, c.end - 1))
                        .collect();
                    format!("{} {}", link.url, spans.join(" "))
                },
            );
            let mut words = want.split_whitespace();
            let url = words.next().unwrap_or_default();
            let url = if url == "none" {
                url.to_owned()
            } else {
                url_input(url)?.into_iter().collect()
            };
            let want: Vec<String> = std::iter::once(url)
                .chain(words.map(str::to_owned))
                .collect();
            if got.split_whitespace().ne(want.iter().map(String::as_str)) {
                let _ = write!(failures, "link: want `{}`, got `{got}`; ", want.join(" "));
            }
        }
        (None, false) => {}
        (Some(_), false) => return Err("`link:` without a link step".into()),
        (None, true) => return Err("a link step without `link:`".into()),
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures)
    }
}

/// Runs one `layer: url` row.
fn run_url(row: &Row) -> Result<(), String> {
    let line = url_input(row.need("input")?)?;
    let at: usize = row
        .need("at")?
        .parse()
        .map_err(|e| format!("bad at: {e}"))?;
    let got = url::url_at(&line, at);
    match row.need("url")? {
        "none" => match got {
            None => Ok(()),
            Some(span) => Err(format!(
                "want none, got {:?} at {span:?}",
                line[span.clone()].iter().collect::<String>()
            )),
        },
        want => {
            let want_text: String = url_input(want)?.into_iter().collect();
            let (c0, c1) = row
                .need("span")?
                .split_once('-')
                .ok_or("span is not `c0-c1`")?;
            let c0: usize = c0.parse().map_err(|e| format!("bad span start: {e}"))?;
            let c1: usize = c1.parse().map_err(|e| format!("bad span end: {e}"))?;
            let Some(span) = got else {
                return Err(format!("want {want_text:?} at {c0}-{c1}, got none"));
            };
            let text: String = line[span.clone()].iter().collect();
            if text == want_text && span == (c0..c1 + 1) {
                Ok(())
            } else {
                Err(format!(
                    "want {want_text:?} at {c0}-{c1}, got {text:?} at {}-{}",
                    span.start,
                    span.end - 1
                ))
            }
        }
    }
}

#[test]
fn every_selection_fixture_row_passes() {
    let rows = parse(FIXTURE).expect("the fixture parses");
    let app = App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"));
    let (mut core, mut urls) = (0, 0);
    let mut failures = Vec::new();
    for row in &rows {
        let result = panic::catch_unwind(AssertUnwindSafe(|| match row.get("layer") {
            Some("core") => {
                core += 1;
                run_core(&app, row)
            }
            Some("url") => {
                urls += 1;
                run_url(row)
            }
            other => Err(format!("unknown layer {other:?}")),
        }))
        .unwrap_or_else(|_| Err("panicked".into()));
        if let Err(message) = result {
            failures.push(format!("{}: {message}", row.id));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} rows failed:\n{}",
        failures.len(),
        rows.len(),
        failures.join("\n")
    );
    assert_eq!((core, urls), (CORE_ROWS, URL_ROWS), "rows run (core, url)");
}

#[test]
fn the_parser_refuses_what_it_cannot_interpret() {
    assert!(parse("### X\nlayer: core\nbogus: 1\n").is_err());
    assert!(parse("layer: core\n").is_err());
    assert!(parse("### X\nno colon here\n").is_err());
    assert!(payload("\\q").is_err());
    assert!(quoted("unquoted").is_err());
    assert_eq!(
        payload("a\\x1b\\u{4e16}\\r\\n").unwrap(),
        "a\x1b\u{4e16}\r\n".as_bytes()
    );
    assert_eq!(
        url_input("x@rep(a,3)\\u{4e16}").unwrap(),
        vec!['x', 'a', 'a', 'a', '\u{4e16}']
    );
    let app = App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"));
    let unknown_step = parse("### X\nlayer: core\nsize: 2x5\nstep: jump 1,1\n").unwrap();
    assert!(run_core(&app, &unknown_step[0]).is_err());
}

fn app() -> App {
    App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"))
}

/// Selects the top row of `id` and returns whether it is selected.
fn select_top_row(app: &App, id: SessionId) -> bool {
    app.select_begin(id, Point::new(0, 0), Granularity::Line, false)
        .unwrap();
    app.has_selection(id).unwrap()
}

#[test]
fn a_resize_there_and_back_clears_the_selection() {
    // The rows hold the same text after the round trip, so only the
    // resize itself can clear it.
    let app = app();
    let id = app.spawn_test_session(PtySize::new(3, 10)).unwrap();
    app.feed_for_test(id, b"hello").unwrap();
    assert!(select_top_row(&app, id));
    app.resize(id, PtySize::new(3, 10)).unwrap();
    assert!(app.has_selection(id).unwrap(), "the same size keeps it");
    app.resize(id, PtySize::new(3, 8)).unwrap();
    app.resize(id, PtySize::new(3, 10)).unwrap();
    assert!(!app.has_selection(id).unwrap());
    assert_eq!(app.selection_text(id).unwrap(), None);
}

#[test]
fn clear_scrollback_clears_a_selection_whose_rows_look_the_same() {
    // ⌘K moves the cursor's row to the top; with identical rows the
    // content comparison alone would keep the selection.
    let app = app();
    let id = app.spawn_test_session(PtySize::new(3, 10)).unwrap();
    app.feed_for_test(id, b"x\r\nx\r\nx").unwrap();
    assert!(select_top_row(&app, id));
    app.clear_scrollback(id).unwrap();
    assert!(!app.has_selection(id).unwrap());
}

#[test]
fn a_selection_outlives_a_flood_and_frames_check_it_cheaply() {
    // Output under a selection made while scrolled back: the selected
    // history line survives 5 000 lines (below the 10 000-line cap) and the frame's
    // decorations still find it in view.
    let app = app();
    let id = app.spawn_test_session(PtySize::new(24, 80)).unwrap();
    app.feed_for_test(id, b"first\r\n").unwrap();
    for _ in 0..30 {
        app.feed_for_test(id, b"filler line\r\n").unwrap();
    }
    app.scroll_to(id, ScrollTarget::Top).unwrap();
    app.select_begin(id, Point::new(0, 0), Granularity::Word, false)
        .unwrap();
    let line = b"0123456789 abcdefghij 0123456789 abcdefghij\r\n".repeat(1000);
    for _ in 0..5 {
        app.feed_for_test(id, &line).unwrap();
        let (selection, _) = app.view_decorations_for_test(id).unwrap();
        assert!(selection.is_some(), "the view stays on the selected line");
    }
    assert_eq!(app.selection_text(id).unwrap().as_deref(), Some("first"));
}

#[test]
fn a_selected_row_rewritten_then_scrolled_into_history_clears() {
    // The row changes while live and leaves the screen before the next
    // look: the selection must not copy the new text.
    let app = app();
    let id = app.spawn_test_session(PtySize::new(3, 10)).unwrap();
    app.feed_for_test(id, b"hello\r\nb\r\nc").unwrap();
    app.select_begin(id, Point::new(0, 0), Granularity::Word, false)
        .unwrap();
    assert_eq!(app.selection_text(id).unwrap().as_deref(), Some("hello"));
    app.feed_for_test(id, b"\x1b[1;1HXXXXX\x1b[3;1H\r\n\r\n")
        .unwrap();
    assert!(!app.has_selection(id).unwrap());
    assert_eq!(app.selection_text(id).unwrap(), None);
}
