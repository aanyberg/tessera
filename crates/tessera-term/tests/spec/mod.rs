//! Reader for `tests/fixtures/csi_spec.txt`, shared by `tests/csi.rs` and
//! `tests/vttest.rs`, and for the fixtures in the same layout
//! (`decstr_spec.txt`, `esctest_spec.txt`).
//!
//! The fixture uses the layout of `print_c0_spec.txt`: a case starts with
//! `### <id>` and lists `size:`, one or more `step:` lines, `screen:` with
//! one `>`-prefixed line per row, `cursor:`, `pending_wrap:`, `requests:`
//! and `scrollback:`. Other keys (`cite:`, `note:`, `flags:`, `needs:`,
//! `source:`) are documentation. A line starting `# --` ends a case;
//! deferred vttest screens are `#>` comments and are never cases.

use tessera_grid::Point;
use tessera_term::Request;

use crate::common::{Chunking, Script, Step, decode, render_screen};

/// What one fixture case expects.
#[derive(Debug)]
pub struct Case {
    /// The `### <id>` heading.
    pub id: String,
    /// Size and steps.
    pub script: Script,
    /// Expected [`render_screen`] output.
    pub screen: String,
    /// Expected cursor, 0-based.
    pub cursor: Point,
    /// Expected pending-wrap flag.
    pub pending_wrap: bool,
    /// Expected requests, in order: `Bell` and `Resize(rows,cols)` entries.
    pub requests: Vec<Request>,
    /// `(len, first line as (text, wrapped))`; `None` when not asserted.
    pub scrollback: Option<(usize, Option<(String, bool)>)>,
}

/// The fixture text.
pub const FIXTURE: &str = include_str!("../fixtures/csi_spec.txt");

/// True for vttest case ids (`V<menu>S<screen>_...`).
pub fn is_vttest(id: &str) -> bool {
    vttest_screen(id).is_some()
}

/// `(menu, screen)` of a vttest case id such as `V8S2_accordion_il_dl`.
pub fn vttest_screen(id: &str) -> Option<(u32, u32)> {
    let rest = id.strip_prefix('V')?;
    let (menu, rest) = rest.split_at_checked(1)?;
    let rest = rest.strip_prefix('S')?;
    let (screen, _) = rest.split_once('_')?;
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(menu) || !all_digits(screen) {
        return None;
    }
    Some((menu.parse().ok()?, screen.parse().ok()?))
}

/// Drops a trailing `# comment` and surrounding blanks from a field value.
fn value(raw: &str) -> &str {
    raw.split_once('#').map_or(raw, |(v, _)| v).trim()
}

fn parse_cursor(raw: &str) -> Point {
    let inner = value(raw)
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .expect("cursor is `(row,col)`");
    let (row, col) = inner.split_once(',').expect("cursor is `(row,col)`");
    Point::new(
        row.trim().parse().expect("cursor row"),
        col.trim().parse().expect("cursor col"),
    )
}

/// A `requests:` row: `[]`, or `Bell` and `Resize(rows,cols)` entries
/// separated by commas.
fn parse_requests(raw: &str) -> Vec<Request> {
    let inner = value(raw)
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .expect("requests is `[..]`");
    let mut requests = Vec::new();
    let mut rest = inner.trim();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("Bell") {
            requests.push(Request::Bell);
            rest = after;
        } else if let Some(after) = rest.strip_prefix("Resize(") {
            let (size, after) = after.split_once(')').expect("Resize(rows,cols)");
            let (rows, cols) = size.split_once(',').expect("Resize(rows,cols)");
            requests.push(Request::Resize {
                rows: rows.trim().parse().expect("Resize rows"),
                cols: cols.trim().parse().expect("Resize cols"),
            });
            rest = after;
        } else {
            panic!("unexpected request `{rest}` in fixture");
        }
        rest = rest.trim_start().trim_start_matches(',').trim_start();
    }
    requests
}

fn parse_scrollback(raw: &str) -> Option<(usize, Option<(String, bool)>)> {
    let raw = raw.trim();
    if raw.starts_with("not asserted") {
        return None;
    }
    let rest = raw.strip_prefix("len=").expect("scrollback is `len=N ...`");
    let (len, first) = rest.split_once(' ').unwrap_or((rest, ""));
    let first = first.trim().strip_prefix("first=(\"").map(|first| {
        let (text, flags) = first
            .split_once("\", wrapped=")
            .expect("first=(\"..\", wrapped=..)");
        let wrapped = flags.trim_end_matches(')') == "true";
        (text.to_owned(), wrapped)
    });
    Some((len.parse().expect("scrollback len"), first))
}

/// Parses every case in the fixture, in file order.
///
/// # Panics
///
/// On a case with a missing or malformed field.
pub fn parse_fixture(text: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(id) = line.strip_prefix("### ") else {
            continue;
        };
        let mut size = None;
        let mut steps = Vec::new();
        let mut screen = String::new();
        let (mut cursor, mut pending_wrap, mut requests, mut scrollback) = (None, None, None, None);
        while let Some(line) = lines.next_if(|l| !l.starts_with("### ") && !l.starts_with("# --")) {
            let (key, raw) = line
                .split_once(": ")
                .unwrap_or((line.trim_end_matches(':'), ""));
            match key {
                "size" => {
                    let (rows, cols) = raw.split_once('x').expect("size is `RxC`");
                    size = Some((
                        rows.trim().parse().expect("rows"),
                        cols.trim().parse().expect("cols"),
                    ));
                }
                "step" => steps.push(if let Some(p) = raw.strip_prefix("feed ") {
                    Step::Feed(decode(p))
                } else if let Some(arg) = raw.strip_prefix("set ") {
                    Step::parse_set(arg)
                } else {
                    panic!("{id}: unknown step `{raw}`")
                }),
                "screen" => {
                    while let Some(row) = lines.next_if(|l| l.starts_with('>')) {
                        screen.push_str(row[1..].trim_end());
                        screen.push('\n');
                    }
                }
                "cursor" => cursor = Some(parse_cursor(raw)),
                "pending_wrap" => pending_wrap = Some(value(raw) == "true"),
                "requests" => requests = Some(parse_requests(raw)),
                "scrollback" => scrollback = Some(parse_scrollback(raw)),
                _ => {}
            }
        }
        let (rows, cols) = size.unwrap_or_else(|| panic!("{id}: no size"));
        cases.push(Case {
            id: id.to_owned(),
            script: Script { rows, cols, steps },
            screen,
            cursor: cursor.unwrap_or_else(|| panic!("{id}: no cursor")),
            pending_wrap: pending_wrap.unwrap_or_else(|| panic!("{id}: no pending_wrap")),
            requests: requests.unwrap_or_else(|| panic!("{id}: no requests")),
            scrollback: scrollback.unwrap_or_else(|| panic!("{id}: no scrollback")),
        });
    }
    cases
}

/// Every mismatch between a case and what the terminal did.
pub fn check_case(case: &Case, chunking: Chunking) -> Vec<String> {
    let (term, requests) = case.script.run(chunking);
    // `requests:` rows list the other requests; replies have their own
    // fixture (reports_spec.txt).
    let (_, requests) = crate::common::split_replies(&requests);
    let grid = term.grid();
    let mut errors = Vec::new();
    let mut expect = |what: &str, ok: bool, detail: String| {
        if !ok {
            errors.push(format!("{} ({chunking:?}) {what}: {detail}", case.id));
        }
    };
    let screen = render_screen(&term);
    expect(
        "screen",
        screen == case.screen,
        format!("\nexpected:\n{}got:\n{screen}", case.screen),
    );
    expect(
        "cursor",
        grid.cursor() == case.cursor,
        format!("expected {:?}, got {:?}", case.cursor, grid.cursor()),
    );
    expect(
        "pending_wrap",
        grid.pending_wrap() == case.pending_wrap,
        format!("expected {}", case.pending_wrap),
    );
    expect(
        "requests",
        requests == case.requests,
        format!("expected {:?}, got {requests:?}", case.requests),
    );
    if let Some((len, first)) = &case.scrollback {
        expect(
            "scrollback_len",
            grid.scrollback_len() == *len,
            format!("expected {len}, got {}", grid.scrollback_len()),
        );
        if let Some((text, wrapped)) = first {
            let got = grid.scrollback_line(0).map(|l| (l.text(), l.wrapped));
            expect(
                "scrollback[0]",
                got.as_ref() == Some(&(text.clone(), *wrapped)),
                format!("expected {:?}, got {got:?}", (text, wrapped)),
            );
        }
    }
    errors
}

/// Checks every case `select` keeps, whole and byte by byte, and panics
/// with all mismatches at once. Returns how many cases were checked.
pub fn check_all(select: impl Fn(&Case) -> bool) -> usize {
    check_all_in(FIXTURE, select)
}

/// [`check_all`] for another fixture in the same layout, such as
/// `tests/fixtures/esctest_spec.txt`.
pub fn check_all_in(fixture: &str, select: impl Fn(&Case) -> bool) -> usize {
    let cases: Vec<Case> = parse_fixture(fixture).into_iter().filter(select).collect();
    let errors: Vec<String> = cases
        .iter()
        .flat_map(|case| {
            [Chunking::Whole, Chunking::Bytewise]
                .into_iter()
                .flat_map(|chunking| check_case(case, chunking))
        })
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
    cases.len()
}
