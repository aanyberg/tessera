//! The smart-selection and copy-modes fixture,
//! `tests/fixtures/smart_selection_spec.txt` (#47, ADR 0042).
//!
//! Every section 3 row (a string, a click, the span it selects) runs
//! through [`App`] on a session without a child three times: with the
//! default rules (the span), with only the rule its `by:` names (the rule
//! is the one that wins), and with no rules (a `by: word` row selects the
//! same, so word selection is unchanged). Sections 4, 5 and 7 have their
//! own tests below, written out from the fixture's rows.

use std::fmt::Write as _;

use tessera_app_core::{
    App, Config, ConfigPaths, FormattedText, Granularity, MAX_RICH_COPY_LEN, Point, PtySize,
    RichCopy, SessionId,
};
use tessera_config::{DEFAULT_SMART_RULES, Precision, SmartRule};

const FIXTURE: &str = include_str!("fixtures/smart_selection_spec.txt");
/// The section 3 rows the fixture holds.
const EXPANSION_ROWS: usize = 55;

/// The default rules a fixture `by:` names, by index in
/// [`DEFAULT_SMART_RULES`].
fn rule_indices(by: &str) -> Option<&'static [usize]> {
    Some(match by {
        "cpp_namespace" => &[0],
        "path" => &[1],
        "quoted" => &[2],
        "include_path" => &[3],
        "mailto" => &[4],
        "objc_selector" => &[5],
        "email" => &[6],
        "http_url" => &[7],
        "ssh_url" => &[8],
        "telnet_url" => &[9],
        "git_sha" => &[10],
        "ipv4" => &[11],
        "ipv6" => &[12, 13, 14],
        _ => return None,
    })
}

/// A section 3 row.
#[derive(Debug)]
struct Expansion {
    id: String,
    text: String,
    click: usize,
    span: std::ops::Range<usize>,
    expect: String,
    by: String,
}

/// A `"..."` value with `\"` and `\\` escapes.
fn unquote(value: &str) -> Result<String, String> {
    let inner = value
        .trim()
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .ok_or_else(|| format!("not quoted: {value}"))?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(chars.next().ok_or("a trailing backslash")?);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// The value without a trailing `# comment`.
fn uncommented(value: &str) -> &str {
    value.split_once('#').map_or(value, |(v, _)| v).trim()
}

/// The section 3 rows: `### SS-...` rows with an `expect:`.
fn expansions() -> Result<Vec<Expansion>, String> {
    let mut rows = Vec::new();
    let mut fields: Option<(String, Vec<(String, String)>)> = None;
    let mut finish = |fields: Option<(String, Vec<(String, String)>)>| -> Result<(), String> {
        let Some((id, fields)) = fields else {
            return Ok(());
        };
        let get = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let Some(expect) = get("expect") else {
            return Ok(());
        };
        let need = |key: &str| get(key).ok_or_else(|| format!("{id}: no `{key}:`"));
        let (range, quoted) = expect
            .split_once(") ")
            .ok_or_else(|| format!("{id}: bad expect {expect}"))?;
        let (start, end) = range
            .trim_start_matches('[')
            .split_once(',')
            .ok_or_else(|| format!("{id}: bad range {range}"))?;
        let num = |s: &str| {
            s.trim()
                .parse::<usize>()
                .map_err(|e| format!("{id}: bad number {s}: {e}"))
        };
        rows.push(Expansion {
            text: unquote(need("text")?)?,
            click: num(uncommented(need("click")?))?,
            span: num(start)?..num(end)?,
            expect: unquote(quoted)?,
            by: need("by")?.trim().to_owned(),
            id,
        });
        Ok(())
    };
    for line in FIXTURE.lines() {
        if let Some(rest) = line.strip_prefix("### ") {
            finish(fields.take())?;
            let id = rest.split_whitespace().next().unwrap_or_default();
            if id.starts_with("SS-") {
                fields = Some((id.to_owned(), Vec::new()));
            }
            continue;
        }
        if line.trim().is_empty() || line.starts_with('#') {
            finish(fields.take())?;
            continue;
        }
        // Lines without a key continue a `note:` (in section 4 rows).
        if let Some(((_, fields), (key, value))) = fields.as_mut().zip(line.split_once(':'))
            && !key.starts_with(' ')
        {
            fields.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    finish(fields.take())?;
    Ok(rows)
}

fn app_with(rules: Option<Vec<SmartRule>>, trim: bool) -> App {
    let mut config = Config::default();
    config.selection.smart_rules = rules;
    config.selection.trim_trailing_whitespace = trim;
    App::new(config, ConfigPaths::new("/nonexistent/tessera"))
}

fn default_rule(i: usize) -> SmartRule {
    let (pattern, precision) = DEFAULT_SMART_RULES[i];
    SmartRule::new(pattern, precision)
}

/// The selection a frame would draw, as the fixture writes it.
fn frame_selection(app: &App, id: SessionId) -> String {
    let (selection, _) = app.view_decorations_for_test(id).expect("a live session");
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

/// The first and last column of each character on row 0 of session `id`.
fn char_columns(app: &App, id: SessionId) -> Vec<(u16, u16)> {
    app.with_terminal(id, |term| {
        let line = term.grid().line(0).expect("row 0");
        let mut columns = Vec::new();
        for (col, cell) in line.cells.iter().enumerate() {
            let col = u16::try_from(col).expect("a column");
            if cell.is_spacer() || cell.is_pad() {
                if let Some(last) = columns.last_mut() {
                    let (_, end): &mut (u16, u16) = last;
                    *end = col;
                }
                continue;
            }
            columns.push((col, col));
        }
        columns
    })
    .expect("a live session")
}

/// Double-clicks character `click` of `text` on a fresh one-line session
/// of `app`; the selection drawn and copied.
fn double_click(app: &App, text: &str, click: usize) -> (String, Option<String>, Vec<(u16, u16)>) {
    let id = app
        .spawn_test_session(PtySize::new(2, 120))
        .expect("a session");
    app.feed_for_test(id, text.as_bytes()).expect("fed");
    let columns = char_columns(app, id);
    app.select_begin(
        id,
        Point::new(0, columns[click].0),
        Granularity::Word,
        false,
    )
    .expect("clicked");
    let result = (
        frame_selection(app, id),
        app.selection_text(id).expect("a live session"),
        columns,
    );
    app.close_session(id).expect("closed");
    result
}

#[test]
fn the_fixture_expansion_rows_select_their_spans() {
    let rows = expansions().expect("the fixture parses");
    assert_eq!(rows.len(), EXPANSION_ROWS, "section 3 rows");
    let defaults = app_with(None, true);
    let mut failures = String::new();
    for row in &rows {
        let chars: Vec<char> = row.text.chars().collect();
        let cut: String = chars[row.span.clone()].iter().collect();
        if cut != row.expect {
            let _ = writeln!(
                failures,
                "{}: span {:?} is {cut:?}, not {:?}",
                row.id, row.span, row.expect
            );
            continue;
        }
        let (drawn, copied, columns) = double_click(&defaults, &row.text, row.click);
        let want = format!(
            "linear (0,{})-(0,{})",
            columns[row.span.start].0,
            columns[row.span.end - 1].1
        );
        if drawn != want || copied.as_deref() != Some(row.expect.as_str()) {
            let _ = writeln!(
                failures,
                "{}: {drawn} {copied:?}, want {want} {:?}",
                row.id, row.expect
            );
        }
        // The rule `by:` names wins alone too; a `by: word` row is what
        // word selection alone gives.
        let alone = if row.by.starts_with("word") {
            app_with(Some(Vec::new()), true)
        } else {
            let Some(indices) = rule_indices(&row.by) else {
                let _ = writeln!(failures, "{}: unknown rule {}", row.id, row.by);
                continue;
            };
            app_with(
                Some(indices.iter().map(|&i| default_rule(i)).collect()),
                true,
            )
        };
        let (drawn_alone, copied_alone, _) = double_click(&alone, &row.text, row.click);
        if (drawn_alone.as_str(), copied_alone.as_deref())
            != (want.as_str(), Some(row.expect.as_str()))
        {
            let _ = writeln!(
                failures,
                "{} by {}: {drawn_alone} {copied_alone:?}, want {want}",
                row.id, row.by
            );
        }
    }
    assert!(failures.is_empty(), "{failures}");
}

/// A fresh session of `size` with `bytes` fed.
fn session(app: &App, rows: u16, cols: u16, bytes: &[u8]) -> SessionId {
    let id = app
        .spawn_test_session(PtySize::new(rows, cols))
        .expect("a session");
    app.feed_for_test(id, bytes).expect("fed");
    id
}

fn word_click(app: &App, id: SessionId, row: u16, col: u16) -> (String, Option<String>) {
    app.select_begin(id, Point::new(row, col), Granularity::Word, false)
        .expect("clicked");
    (
        frame_selection(app, id),
        app.selection_text(id).expect("live"),
    )
}

/// SS-C1 to SS-C4b: soft wraps, wide characters and NFD accents, in cells.
#[test]
fn cell_rows_select_their_cells() {
    let app = app_with(None, true);
    let url = "https://example.com/abcdef/ghijkl";
    let id = session(&app, 3, 20, b"see https://example.com/abcdef/ghijkl now");
    for (row, col) in [(1, 3), (0, 4)] {
        assert_eq!(
            word_click(&app, id, row, col),
            ("linear (0,4)-(1,16)".to_owned(), Some(url.to_owned())),
            "SS-C1 at ({row},{col})"
        );
    }
    let id = session(&app, 2, 20, "日本 /tmp/x ok".as_bytes());
    assert_eq!(
        word_click(&app, id, 0, 7),
        ("linear (0,5)-(0,10)".to_owned(), Some("/tmp/x".to_owned())),
        "SS-C2"
    );
    assert_eq!(
        word_click(&app, id, 0, 1),
        ("linear (0,0)-(0,3)".to_owned(), Some("日本".to_owned())),
        "SS-C3"
    );
    let id = session(&app, 2, 20, "/tmp/日本/x ok".as_bytes());
    assert_eq!(
        word_click(&app, id, 0, 6),
        (
            "linear (0,0)-(0,10)".to_owned(),
            Some("/tmp/日本/x".to_owned())
        ),
        "SS-C4"
    );
    let nfd = "/Users/Jose\u{301}/cafe\u{301}.txt";
    let id = session(&app, 2, 40, format!("ls a+{nfd} ok").as_bytes());
    assert_eq!(
        word_click(&app, id, 0, 19),
        ("linear (0,5)-(0,24)".to_owned(), Some(nfd.to_owned())),
        "SS-C4b"
    );
}

/// A drag after a smart double-click extends by words from the match,
/// and a click past the text selects nothing, as before.
#[test]
fn drags_extend_by_words_and_past_the_text_is_nothing() {
    let app = app_with(None, true);
    let id = session(&app, 2, 40, b"ls /usr/local/bin now then");
    app.select_begin(id, Point::new(0, 8), Granularity::Word, false)
        .unwrap();
    assert_eq!(
        app.selection_text(id).unwrap().as_deref(),
        Some("/usr/local/bin")
    );
    app.select_extend(id, Point::new(0, 19)).unwrap();
    assert_eq!(
        app.selection_text(id).unwrap().as_deref(),
        Some("/usr/local/bin now")
    );
    app.select_begin(id, Point::new(0, 30), Granularity::Word, false)
        .unwrap();
    assert!(!app.has_selection(id).unwrap());
}

/// A user rule replaces the defaults, and its precision ranks it.
#[test]
fn a_user_rule_replaces_the_defaults() {
    let app = app_with(
        Some(vec![SmartRule::new("[A-Z]+-[0-9]+", Precision::High)]),
        true,
    );
    let id = session(&app, 2, 40, b"fix ABC-123 in /a/b");
    assert_eq!(word_click(&app, id, 0, 6).1.as_deref(), Some("ABC-123"));
    // No path rule any more: the word.
    assert_eq!(word_click(&app, id, 0, 16).1.as_deref(), Some("/a/b"));
    let id = session(&app, 2, 40, b"x a/b:c");
    assert_eq!(word_click(&app, id, 0, 3).1.as_deref(), Some("a/b"));
}

/// SS-C5b to SS-C5e: where the window edge falls inside a long C++ name
/// on a character the rule cannot start or end with (`:`), the match the
/// regex finds starts or ends just inside the window; it is still dropped,
/// and the double-click selects what a word double-click selects.
#[test]
fn a_match_cut_inside_the_edge_run_is_dropped() {
    const COLS: u16 = 200;
    let smart = app_with(None, true);
    let words = app_with(Some(Vec::new()), true);
    let click = |text: &str, at: usize| {
        let row = u16::try_from(at / usize::from(COLS)).unwrap();
        let col = u16::try_from(at % usize::from(COLS)).unwrap();
        let [got, word] = [&smart, &words].map(|app| {
            let id = session(app, 70, COLS, text.as_bytes());
            let text = word_click(app, id, row, col).1;
            app.close_session(id).unwrap();
            text
        });
        (got, word)
    };
    let both = "a::".repeat(4000);
    let start_edge = format!("{}a{}", "a::".repeat(2000), " x".repeat(100));
    // (row, text, clicked character): the window's first character is the
    // second `:` (5895 - 4096 = 1799), or its last the first `:` (3001 +
    // 4095 = 7096).
    for (name, text, at) in [
        ("SS-C5b both edges", both.as_str(), 6000),
        ("SS-C5c start edge", start_edge.as_str(), 5895),
        ("SS-C5d end edge", both.as_str(), 3001),
    ] {
        let (got, word) = click(text, at);
        assert!(
            word.as_ref().is_some_and(|w| w.len() < 4),
            "{name}: {word:?}"
        );
        assert_eq!(got, word, "{name}");
    }
    // The same name wholly inside the window is found.
    let short = format!("x {}a y", "a::".repeat(1000));
    let (got, _) = click(&short, 1500);
    assert_eq!(got.map(|t| t.len()), Some(3001));
    // SS-C5e: the window ends with the line and holds no blank.
    let path = "a/".repeat(3000);
    let (got, word) = click(&path, 5000);
    assert_eq!(word.as_deref(), Some(path.as_str()), "SS-C5e word");
    assert_eq!(got, word, "SS-C5e");
}

/// Equal scores go to the earlier start, then (same start) to the earlier
/// rule; equal weight times length goes to the higher precision (D15).
#[test]
fn ties_go_to_the_earlier_start_then_the_higher_precision() {
    // `bc` and `ab` both cover the `b` with the same score: `ab` starts first.
    for rules in [["bc", "ab"], ["ab", "bc"]] {
        let rules = rules
            .iter()
            .map(|p| SmartRule::new(*p, Precision::Normal))
            .collect();
        let app = app_with(Some(rules), true);
        let id = session(&app, 2, 10, b"abc");
        assert_eq!(word_click(&app, id, 0, 1).1.as_deref(), Some("ab"));
    }
    // 1000 characters at normal (10^6) and one at high (10^9) both score
    // 10^9: the high-precision rule wins although its match is shorter.
    let app = app_with(
        Some(vec![
            SmartRule::new("[ab]{1000}", Precision::Normal),
            SmartRule::new("b", Precision::High),
        ]),
        true,
    );
    let mut line = "a".repeat(999);
    line.push('b');
    let id = session(&app, 2, 1024, line.as_bytes());
    assert_eq!(word_click(&app, id, 0, 999).1.as_deref(), Some("b"));
    // Without the high rule the long match is the selection.
    let app = app_with(
        Some(vec![SmartRule::new("[ab]{1000}", Precision::Normal)]),
        true,
    );
    let id = session(&app, 2, 1024, line.as_bytes());
    assert_eq!(word_click(&app, id, 0, 999).1.map(|s| s.len()), Some(1000));
}

/// Without trimming, a selection that runs on over blank rows ends at the
/// last row with text: no padded blank rows and no trailing line breaks.
#[test]
fn untrimmed_copy_drops_the_blank_rows_after_the_text() {
    let app = app_with(None, false);
    let id = session(&app, 6, 10, b"ab\r\n  cd");
    select(&app, id, Some(((0, 0), (4, 9))), false);
    assert_eq!(
        app.selection_text(id).unwrap().as_deref(),
        Some("ab        \n  cd      ")
    );
    // A row counts when text follows its first selected cell, so a block
    // left of `cd` still keeps that row's blanks, and none after it.
    select(&app, id, Some(((0, 0), (4, 1))), true);
    assert_eq!(app.selection_text(id).unwrap().as_deref(), Some("ab\n  "));
}

/// Selects `from..=to` (character cells) on session `id`, or all.
fn select(app: &App, id: SessionId, range: Option<((u16, u16), (u16, u16))>, block: bool) {
    match range {
        Some((from, to)) => {
            app.select_begin(id, Point::new(from.0, from.1), Granularity::Char, block)
                .unwrap();
            app.select_extend(id, Point::new(to.0, to.1)).unwrap();
        }
        None => app.select_all(id).unwrap(),
    }
}

type Range2 = Option<((u16, u16), (u16, u16))>;

/// A trim row: name, screen size, feed, range (`None`: all), block,
/// trimmed text, untrimmed text.
type TrimRow = (
    &'static str,
    (u16, u16),
    &'static [u8],
    Range2,
    bool,
    &'static str,
    &'static str,
);

/// Section 5: `trim_trailing_whitespace`, both ways, and the plain text of
/// Copy with Formatting equal to Copy's.
#[test]
#[expect(clippy::too_many_lines, reason = "one fixture table")]
fn trim_rows() {
    let rows: [TrimRow; 10] = [
        (
            "TR-1",
            (3, 10),
            b"ab\r\ncd",
            Some(((0, 0), (1, 9))),
            false,
            "ab\ncd",
            "ab        \ncd        ",
        ),
        (
            "TR-2",
            (3, 10),
            b"ab",
            Some(((0, 0), (0, 4))),
            false,
            "ab",
            "ab   ",
        ),
        // The fixture had "abc  def" without trimming; corrected there (TR-9
        // pads the last row to the selection's end the same way).
        (
            "TR-3",
            (4, 5),
            b"abc  def",
            None,
            false,
            "abc  def",
            "abc  def  ",
        ),
        (
            "TR-4",
            (4, 5),
            b"abcdefg   ",
            Some(((0, 0), (1, 4))),
            false,
            "abcdefg",
            "abcdefg   ",
        ),
        (
            "TR-5",
            (4, 5),
            "abcd日".as_bytes(),
            Some(((0, 0), (1, 4))),
            false,
            "abcd日",
            "abcd日   ",
        ),
        (
            "TR-6a",
            (2, 10),
            "a\u{a0}".as_bytes(),
            None,
            false,
            "a\u{a0}",
            "a\u{a0}        ",
        ),
        (
            "TR-6b",
            (2, 10),
            "b\u{3000}".as_bytes(),
            None,
            false,
            "b\u{3000}",
            "b\u{3000}       ",
        ),
        (
            "TR-7",
            (2, 10),
            b"ab\x1b[41m   \x1b[0m",
            Some(((0, 0), (0, 4))),
            false,
            "ab",
            "ab   ",
        ),
        (
            "TR-8",
            (3, 10),
            b"ab\r\ncdef",
            Some(((0, 0), (1, 3))),
            true,
            "ab\ncdef",
            "ab  \ncdef",
        ),
        ("TR-9", (5, 10), b"ab", None, false, "ab", "ab        "),
    ];
    let trimmed = app_with(None, true);
    let padded = app_with(None, false);
    for (name, (r, c), feed, range, block, want_trim, want_pad) in rows {
        for (app, want) in [(&trimmed, want_trim), (&padded, want_pad)] {
            let id = session(app, r, c, feed);
            select(app, id, range, block);
            let text = app.selection_text(id).unwrap();
            assert_eq!(
                text.as_deref(),
                Some(want),
                "{name} trim={}",
                app.config().selection.trim_trailing_whitespace
            );
            let formatted = app.selection_copy_with_formatting(id).unwrap();
            assert_eq!(
                formatted.map(|f| f.plain).as_deref(),
                Some(want),
                "{name}: Copy with Formatting's plain text is Copy's"
            );
            app.close_session(id).unwrap();
        }
    }
}

/// The HTML body of a formatted copy.
fn html_body(formatted: &FormattedText) -> &str {
    let RichCopy::Text { html, .. } = &formatted.rich else {
        panic!("no rich text: {formatted:?}");
    };
    let start = html.find("monospace\">\n").expect("the pre") + "monospace\">\n".len();
    &html[start..html.len() - "</pre></body></html>".len()]
}

fn rtf_of(formatted: &FormattedText) -> &str {
    let RichCopy::Text { rtf, .. } = &formatted.rich else {
        panic!("no rich text: {formatted:?}");
    };
    rtf
}

fn formatted(app: &App, rows: u16, cols: u16, feed: &[u8], range: Range2) -> Option<FormattedText> {
    let id = session(app, rows, cols, feed);
    select(app, id, range, false);
    let formatted = app.selection_copy_with_formatting(id).unwrap();
    app.close_session(id).unwrap();
    formatted
}

/// TR-7 / RT-11: trimmed coloured blanks give no run; kept ones carry
/// their background (palette 1 of the default theme).
#[test]
fn trimmed_styled_blanks() {
    let feed = b"ab\x1b[41m   \x1b[0m";
    let trimmed = formatted(&app_with(None, true), 2, 10, feed, Some(((0, 0), (0, 4)))).unwrap();
    assert_eq!(html_body(&trimmed), "ab");
    let padded = formatted(&app_with(None, false), 2, 10, feed, Some(((0, 0), (0, 4)))).unwrap();
    assert_eq!(
        html_body(&padded),
        "ab<span style=\"background-color:#cd0000\">   </span>"
    );
    assert!(
        rtf_of(&padded).ends_with("\\f0\\fs24 ab{\\cb1    }}"),
        "{}",
        rtf_of(&padded)
    );
}

/// Colours resolve under the terminal's palette and OSC overrides; links
/// are text; an empty selection gives nothing.
#[test]
fn rich_rows_through_the_terminal() {
    let app = app_with(None, true);
    // `ls --color`: bold blue directory, default text after it.
    let ls = formatted(&app, 2, 20, b"\x1b[01;34mdir\x1b[0m a.txt", None).unwrap();
    assert_eq!(
        html_body(&ls),
        "<span style=\"color:#0000ee;font-weight:bold\">dir</span> a.txt"
    );
    assert!(
        rtf_of(&ls).contains("{\\colortbl;\\red0\\green0\\blue238;}"),
        "{}",
        rtf_of(&ls)
    );
    // RT-10: an OSC 8 link is its text.
    let link = formatted(
        &app,
        2,
        20,
        b"\x1b]8;;https://x.test/\x1b\\link\x1b]8;;\x1b\\",
        None,
    )
    .unwrap();
    assert_eq!(html_body(&link), "link");
    assert!(!rtf_of(&link).contains("field"));
    // RT-12: inverse swaps the theme colours, here overridden by OSC 10/11.
    let inverse = formatted(
        &app,
        2,
        20,
        b"\x1b]10;#c0c0c0\x1b\\\x1b]11;#101010\x1b\\\x1b[7mi\x1b[0m",
        None,
    )
    .unwrap();
    assert_eq!(
        html_body(&inverse),
        "<span style=\"color:#101010;background-color:#c0c0c0\">i</span>"
    );
    // 256-colour and truecolour; faint and overline give nothing.
    let colours = formatted(
        &app,
        2,
        20,
        b"\x1b[38;5;196ma\x1b[0m\x1b[48;2;1;2;3mb\x1b[0m\x1b[2;53mc",
        None,
    )
    .unwrap();
    assert_eq!(
        html_body(&colours),
        "<span style=\"color:#ff0000\">a</span><span style=\"background-color:#010203\">b</span>c"
    );
    // RT-13: no selection, and a selection of blanks only.
    let id = session(&app, 2, 10, b"ab");
    assert_eq!(app.selection_copy_with_formatting(id).unwrap(), None);
    select(&app, id, Some(((1, 0), (1, 5))), false);
    assert!(app.has_selection(id).unwrap());
    assert_eq!(app.selection_copy_with_formatting(id).unwrap(), None);
    // Hostile cell text is escaped, and plain is the text as is.
    let hostile = formatted(&app, 2, 40, "<a href=x>&\"'{}\\é".as_bytes(), None).unwrap();
    assert_eq!(hostile.plain, "<a href=x>&\"'{}\\é");
    assert_eq!(
        html_body(&hostile),
        "&lt;a href=x&gt;&amp;&quot;&#39;&#123;&#125;&#92;&#xe9;"
    );
    assert!(rtf_of(&hostile).ends_with("<a href=x>&\"'\\{\\}\\\\\\u233?}"));
}

/// S5: a selection whose HTML passes 16 MiB has no rich text, but the
/// plain text is all there.
#[test]
fn a_rich_copy_past_the_cap_keeps_the_plain_text() {
    let app = app_with(None, true);
    let (rows, cols, lines) = (50u16, 200u16, 700usize);
    let id = app.spawn_test_session(PtySize::new(rows, cols)).unwrap();
    app.set_max_scrollback_for_test(id, 2_000).unwrap();
    // Every attribute on, the foreground alternating so no run merges:
    // about 150 bytes of HTML per cell.
    let mut feed = String::from("\x1b[1;3;4;9;48;5;4m");
    for line in 0..lines {
        for col in 0..usize::from(cols) {
            feed.push_str(if (line + col) % 2 == 0 {
                "\x1b[31mé"
            } else {
                "\x1b[32mé"
            });
        }
    }
    app.feed_for_test(id, feed.as_bytes()).unwrap();
    app.select_all(id).unwrap();
    let copy = app
        .selection_copy_with_formatting(id)
        .unwrap()
        .expect("a selection");
    assert_eq!(copy.rich, RichCopy::TooLarge);
    assert_eq!(copy.plain.chars().count(), lines * usize::from(cols));
    assert_eq!(Some(copy.plain), app.selection_text(id).unwrap());
    // Far less text than the cap: the markup is what passes it.
    assert!(lines * usize::from(cols) * 2 < MAX_RICH_COPY_LEN);
}

/// SS-C6: a double-click on a 2048-column line with 64 rules takes at
/// most 5 ms (release builds; the median of 21 clicks, so one preempted
/// click on a loaded machine does not decide it). The rules are compiled
/// at configuration load, not per click.
#[cfg(not(debug_assertions))]
#[test]
fn a_double_click_with_64_rules_on_2048_columns_takes_under_5_ms() {
    use std::time::{Duration, Instant};

    let mut rules: Vec<SmartRule> = (0..DEFAULT_SMART_RULES.len()).map(default_rule).collect();
    for i in 0..(64 - rules.len()) {
        let (pattern, precision) = DEFAULT_SMART_RULES[i % DEFAULT_SMART_RULES.len()];
        rules.push(SmartRule::new(format!("(?:{pattern})(?:#{i})?"), precision));
    }
    assert_eq!(rules.len(), 64);
    let app = app_with(Some(rules), true);
    assert_eq!(app.config().selection.compiled_rules().len(), 64);
    let cases: [(&str, String); 2] = [("ascii", "/a".repeat(1024)), ("cjk", "日/".repeat(682))];
    for (name, text) in cases {
        let id = session(&app, 1, 2048, text.as_bytes());
        let mut times = Vec::new();
        for _ in 0..21 {
            let start = Instant::now();
            app.select_begin(id, Point::new(0, 1023), Granularity::Word, false)
                .unwrap();
            times.push(start.elapsed());
        }
        times.sort();
        let median = times[times.len() / 2];
        eprintln!(
            "{name}: median {median:?}, min {:?}, max {:?}",
            times[0], times[20]
        );
        assert_eq!(
            app.selection_text(id).unwrap().map(|t| t.chars().count()),
            Some(text.chars().count()),
            "{name}: the path rule takes the whole line"
        );
        assert!(median <= Duration::from_millis(5), "{name}: {median:?}");
        app.close_session(id).unwrap();
    }
}
