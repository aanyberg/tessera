//! Cell-level render of a [`Grid`] and a reader for
//! `tests/fixtures/wide_grapheme_spec.txt`.
//!
//! Shared by `tessera-grid`'s `tests/wide.rs` and, through a `#[path]`
//! module, `tessera-term`'s `tests/wide.rs`, so it depends on nothing but
//! `tessera-grid` (widths come from [`char_width`]).
//!
//! # Render
//!
//! [`render`] prints a grid in the fixture's result-block format, so a
//! case passes exactly when the render equals the block the fixture
//! states:
//!
//! ```text
//! cells:
//! >中/W ~ 文/W ~ . . . .
//! >. . . . . . . .
//! cursor: (0,4)
//! pending_wrap: false
//! wrapped_rows(0-based): []
//! scrollback: len=0
//! text:
//! >中文
//! >
//! ```
//!
//! `cells:` has one `>` line per visible row and one token per cell,
//! separated by one space, so every row shows its cell count:
//!
//! - `.` a blank (`' '`, no markers);
//! - `~` a spacer (the right half of a wide character);
//! - `X` a plain cell showing `X`;
//! - `X/W` the left half of a wide character, `X/C` a cluster cell and
//!   `X/WC` both; a cluster cell writes the scalars after its base as
//!   `+HHHH` (4 or more hex digits) before the `/`, for example
//!   `e+0301/C`;
//! - a base that is whitespace, zero-width, a control, a width-1 Prepend
//!   format character (U+0600..U+0604, U+06DD, U+110BD, U+110CD) or one of the marker
//!   characters `. ~ / + @ U` is written `U+HHHH` instead of `X`, so no
//!   printed character can be mistaken for a marker;
//! - a trailing `@N` gives a background of `Color::Indexed(N)`
//!   (`@#RRGGBB` for an RGB colour). Foreground and attributes are not
//!   rendered.
//!
//! `cursor:` is 0-based. `wrapped_rows` lists the rows whose
//! [`Line::wrapped`] is set. `scrollback:` gives the length and, when
//! non-empty, the oldest line's text and wrap flag. `text:` has one `>`
//! line per row with [`Line::text`], where zero-width scalars, controls,
//! whitespace other than U+0020, Prepend format characters and `\` are
//! escaped as `\u{H..}` (`\\` for a backslash).

#![allow(
    dead_code,
    reason = "shared by two test crates; each uses a different subset"
)]

use std::fmt::Write as _;

use tessera_grid::{
    Cell, CellFlags, Color, EraseDisplay, EraseLine, Grid, Line, MAX_CLUSTER_SCALARS, Style,
    char_width,
};

// ------------------------------------------------------------------ render

/// Renders `grid` in the fixture's result-block format (module docs).
pub fn render(grid: &Grid) -> String {
    let mut out = String::from("cells:\n");
    for line in grid.visible_lines() {
        let _ = writeln!(out, ">{}", render_cells(line));
    }
    let cursor = grid.cursor();
    let _ = writeln!(out, "cursor: ({},{})", cursor.row, cursor.col);
    let _ = writeln!(out, "pending_wrap: {}", grid.pending_wrap());
    let wrapped: Vec<String> = (0..grid.rows())
        .filter(|&row| grid.line(row).is_some_and(|line| line.wrapped))
        .map(|row| row.to_string())
        .collect();
    let _ = writeln!(out, "wrapped_rows(0-based): [{}]", wrapped.join(", "));
    let _ = write!(out, "scrollback: len={}", grid.scrollback_len());
    if let Some(first) = grid.scrollback_line(0) {
        let _ = write!(
            out,
            " first=(\"{}\", wrapped={})",
            escape(&first.text()),
            first.wrapped
        );
    }
    out.push_str("\ntext:\n");
    for line in grid.visible_lines() {
        let _ = writeln!(out, ">{}", escape(&line.text()));
    }
    out
}

/// One row of `cells:` tokens (module docs).
pub fn render_cells(line: &Line) -> String {
    line.cells
        .iter()
        .enumerate()
        .map(|(col, cell)| cell_token(line, col, cell))
        .collect::<Vec<_>>()
        .join(" ")
}

fn cell_token(line: &Line, col: usize, cell: &Cell) -> String {
    let mut token = if cell.flags == CellFlags::SPACER && cell.ch == ' ' {
        String::from("~")
    } else if cell.flags.is_empty() && cell.ch == ' ' {
        String::from(".")
    } else {
        let mut token = base_token(cell.ch);
        if cell.is_cluster() {
            match line.cluster(col) {
                Some(text) => {
                    for ch in text.chars().skip(1) {
                        let _ = write!(token, "+{:04X}", u32::from(ch));
                    }
                }
                None => token.push_str("+?"),
            }
        }
        let markers = [
            (CellFlags::SPACER, 'S'),
            (CellFlags::WIDE, 'W'),
            (CellFlags::CLUSTER, 'C'),
        ];
        let suffix: String = markers
            .iter()
            .filter(|(flag, _)| cell.flags.contains(*flag))
            .map(|&(_, letter)| letter)
            .collect();
        if !suffix.is_empty() {
            token.push('/');
            token.push_str(&suffix);
        }
        token
    };
    match cell.style.bg {
        Color::Default => {}
        Color::Indexed(n) => {
            let _ = write!(token, "@{n}");
        }
        Color::Rgb(r, g, b) => {
            let _ = write!(token, "@#{r:02X}{g:02X}{b:02X}");
        }
    }
    token
}

/// The width-1 Prepend format characters (UAX #29 `Prepend`, category
/// Cf): one column wide but invisible, so the render names them. The
/// zero-width Prepend characters are covered by the zero-width rule.
fn is_prepend_format(ch: char) -> bool {
    matches!(
        ch,
        '\u{600}'..='\u{604}' | '\u{6DD}' | '\u{110BD}' | '\u{110CD}'
    )
}

fn base_token(ch: char) -> String {
    let ambiguous = ch.is_whitespace()
        || is_prepend_format(ch)
        || char_width(ch).is_none_or(|w| w == 0)
        || matches!(ch, '.' | '~' | '/' | '+' | '@' | 'U');
    if ambiguous {
        format!("U+{:04X}", u32::from(ch))
    } else {
        String::from(ch)
    }
}

/// Escapes text for `text:` lines and the scrollback summary (module docs).
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\\' {
            out.push_str("\\\\");
        } else if char_width(ch).is_none_or(|w| w == 0)
            || (ch.is_whitespace() && ch != ' ')
            || is_prepend_format(ch)
        {
            let _ = write!(out, "\\u{{{:X}}}", u32::from(ch));
        } else {
            out.push(ch);
        }
    }
    out
}

// -------------------------------------------------------------- invariants

/// Every way `grid` breaks the wide-cell and cluster invariants (I1-I3 and
/// I5 of the fixture), checked on the visible lines and the scrollback.
pub fn invariant_errors(grid: &Grid) -> Vec<String> {
    let mut errors = Vec::new();
    for (row, line) in grid.visible_lines().iter().enumerate() {
        line_errors(&format!("row {row}"), line, &mut errors);
    }
    for i in 0..grid.scrollback_len() {
        if let Some(line) = grid.scrollback_line(i) {
            line_errors(&format!("scrollback {i}"), line, &mut errors);
        }
    }
    let cursor = grid.cursor();
    if cursor.row >= grid.rows() || cursor.col >= grid.cols() {
        errors.push(format!("cursor {cursor:?} out of bounds"));
    }
    if grid.pending_wrap() && cursor.col != grid.cols() - 1 {
        errors.push(format!("pending wrap off the last column at {cursor:?}"));
    }
    errors
}

/// Panics with every invariant violation of `grid`.
#[track_caller]
pub fn assert_invariants(context: &str, grid: &Grid) {
    let errors = invariant_errors(grid);
    assert!(
        errors.is_empty(),
        "{context}: {}\n{}",
        errors.join("; "),
        render(grid)
    );
}

fn line_errors(at: &str, line: &Line, errors: &mut Vec<String>) {
    let cells = &line.cells;
    for (col, cell) in cells.iter().enumerate() {
        if cell.is_wide() {
            match cells.get(col + 1) {
                Some(next) if next.is_spacer() => {
                    if next.style != cell.style {
                        errors.push(format!("{at} col {col}: spacer style differs"));
                    }
                }
                _ => errors.push(format!("{at} col {col}: WIDE without a spacer")),
            }
        }
        if cell.is_spacer() {
            if col == 0 || !cells[col - 1].is_wide() {
                errors.push(format!("{at} col {col}: SPACER without a wide cell"));
            }
            if cell.ch != ' ' || cell.is_wide() || cell.is_cluster() {
                errors.push(format!("{at} col {col}: spacer is not a bare ' '"));
            }
        }
        match (cell.is_cluster(), line.cluster(col)) {
            (true, None) => errors.push(format!("{at} col {col}: CLUSTER without text")),
            (false, Some(_)) => errors.push(format!("{at} col {col}: text without CLUSTER")),
            (true, Some(text)) => {
                let scalars = text.chars().count();
                if !(2..=MAX_CLUSTER_SCALARS).contains(&scalars) {
                    errors.push(format!("{at} col {col}: cluster of {scalars} scalars"));
                }
                if !text.starts_with(cell.ch) || cell.ch == ' ' {
                    errors.push(format!("{at} col {col}: cluster base is not ch"));
                }
            }
            (false, None) => {}
        }
    }
    for (col, _) in line.graphemes.iter() {
        if col >= cells.len() {
            errors.push(format!("{at}: cluster at column {col} past the end"));
        }
    }
}

// ----------------------------------------------------------------- fixture

/// Which API a fixture case drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// Raw `unicode-width` / `unicode-segmentation` answers.
    Width,
    /// [`Grid`] primitives.
    Grid,
    /// `tessera_term::Terminal`.
    Term,
}

/// One step of a grid- or term-layer case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// [`Grid::put_str`].
    Put(String),
    /// [`Grid::goto`].
    Goto(u16, u16),
    /// [`Grid::carriage_return`].
    Cr,
    /// [`Grid::line_feed`].
    Lf,
    /// [`Grid::insert_chars`].
    Ich(u16),
    /// [`Grid::delete_chars`].
    Dch(u16),
    /// [`Grid::erase_chars`].
    Ech(u16),
    /// [`Grid::erase_in_line`].
    El(EraseLine),
    /// [`Grid::erase_in_display`].
    Ed(EraseDisplay),
    /// Pen background `Color::Indexed(N)`, or the default pen for `None`.
    PenBg(Option<u8>),
    /// [`Grid::reset`].
    Reset,
    /// One `Terminal::advance` call with the payload's UTF-8 bytes; the raw
    /// payload (escapes intact) is kept for script files.
    Feed { text: String, raw: String },
    /// One `Terminal::advance` call per byte of the payload.
    FeedBytewise { text: String, raw: String },
    /// `Terminal::set_auto_wrap`.
    AutoWrap(bool),
    /// `Terminal::set_insert_mode`.
    InsertMode(bool),
}

/// One `### ` row of the fixture.
#[derive(Debug, Clone)]
pub struct Case {
    /// The row id, `B01_cjk_pair`.
    pub id: String,
    /// Which API it drives.
    pub layer: Layer,
    /// `(rows, cols)`; `(0, 0)` for width rows.
    pub size: (u16, u16),
    /// Steps in order (grid and term rows).
    pub steps: Vec<Step>,
    /// The `note:` line, if any.
    pub note: String,
    /// The decoded `input:` (width rows).
    pub input: String,
    /// `char_widths:` as `Some(width)` / `None` per scalar (width rows).
    pub char_widths: Vec<Option<usize>>,
    /// `str_width:` (width rows).
    pub str_width: usize,
    /// `grapheme_scalar_counts:` (width rows).
    pub grapheme_scalar_counts: Vec<usize>,
    /// The result block, from `cells:` to the end of the row, one `\n`
    /// after each line (grid and term rows).
    pub expected: String,
}

/// The fixture text, embedded at compile time.
pub const FIXTURE: &str = include_str!("../fixtures/wide_grapheme_spec.txt");

/// Parses every row of [`FIXTURE`].
///
/// # Panics
///
/// On any key, step or escape the row syntax does not define.
pub fn cases() -> Vec<Case> {
    parse(FIXTURE)
}

/// The row whose id is `id`, ignoring ASCII case.
///
/// # Panics
///
/// When no row has that id.
pub fn case(id: &str) -> Case {
    cases()
        .into_iter()
        .find(|c| c.id.eq_ignore_ascii_case(id))
        .unwrap_or_else(|| panic!("no fixture row `{id}`"))
}

/// Parses fixture text (see the ROW SYNTAX section of the fixture).
///
/// # Panics
///
/// On any key, step or escape the row syntax does not define.
pub fn parse(text: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(id) = line.strip_prefix("### ") else {
            continue;
        };
        let mut body = Vec::new();
        while let Some(line) = lines.next_if(|l| !l.starts_with("### ")) {
            if !line.starts_with('#') {
                body.push(line);
            }
        }
        cases.push(parse_case(id.trim(), &body));
    }
    cases
}

fn parse_case(id: &str, body: &[&str]) -> Case {
    let mut case = Case {
        id: id.to_owned(),
        layer: Layer::Grid,
        size: (0, 0),
        steps: Vec::new(),
        note: String::new(),
        input: String::new(),
        char_widths: Vec::new(),
        str_width: 0,
        grapheme_scalar_counts: Vec::new(),
        expected: String::new(),
    };
    let mut layer = None;
    let mut rest = body.iter();
    while let Some(line) = rest.next() {
        if line.trim().is_empty() {
            continue;
        }
        if *line == "cells:" {
            let mut block: Vec<&str> = std::iter::once(*line).chain(rest.copied()).collect();
            while block.last().is_some_and(|l| l.trim().is_empty()) {
                block.pop();
            }
            case.expected = block.iter().flat_map(|l| [*l, "\n"]).collect();
            break;
        }
        let (key, value) = line
            .split_once(": ")
            .unwrap_or_else(|| panic!("{id}: malformed line `{line}`"));
        match key {
            "layer" => {
                layer = Some(match value {
                    "width" => Layer::Width,
                    "grid" => Layer::Grid,
                    "term" => Layer::Term,
                    other => panic!("{id}: unknown layer `{other}`"),
                });
            }
            "size" => {
                let (rows, cols) = value.split_once('x').expect("size is RxC");
                case.size = (rows.parse().expect("rows"), cols.parse().expect("cols"));
            }
            "step" => case.steps.push(parse_step(id, value)),
            "input" => case.input = decode(value),
            "note" => value.clone_into(&mut case.note),
            "char_widths" => {
                case.char_widths = list(value)
                    .map(|w| (w != "none").then(|| w.parse().expect("width")))
                    .collect();
            }
            "str_width" => case.str_width = value.parse().expect("str_width"),
            "grapheme_scalar_counts" => {
                case.grapheme_scalar_counts =
                    list(value).map(|n| n.parse().expect("count")).collect();
            }
            "cite" | "flags" => {}
            other => panic!("{id}: unknown key `{other}`"),
        }
    }
    case.layer = layer.unwrap_or_else(|| panic!("{id}: no layer"));
    if case.layer != Layer::Width {
        assert!(case.size.0 > 0, "{id}: no size");
        assert!(!case.expected.is_empty(), "{id}: no result block");
    }
    case
}

fn list(value: &str) -> impl Iterator<Item = &str> {
    value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .expect("a `[..]` list")
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn parse_step(id: &str, step: &str) -> Step {
    let (name, arg) = step.split_once(' ').unwrap_or((step, ""));
    let num = |arg: &str| -> u16 {
        arg.parse()
            .unwrap_or_else(|_| panic!("{id}: `{step}` needs a number"))
    };
    match name {
        "put" => {
            let text = decode(arg);
            assert!(
                !text.contains(['\r', '\n']),
                "{id}: `\\r` / `\\n` are term-layer escapes"
            );
            Step::Put(text)
        }
        "goto" => {
            let (row, col) = arg.split_once(' ').expect("goto R C");
            Step::Goto(num(row), num(col))
        }
        "cr" => Step::Cr,
        "lf" => Step::Lf,
        "ich" => Step::Ich(num(arg)),
        "dch" => Step::Dch(num(arg)),
        "ech" => Step::Ech(num(arg)),
        "el" => Step::El(match arg {
            "to_end" => EraseLine::ToEnd,
            "to_start" => EraseLine::ToStart,
            "all" => EraseLine::All,
            other => panic!("{id}: unknown el `{other}`"),
        }),
        "ed" => Step::Ed(match arg {
            "below" => EraseDisplay::Below,
            "above" => EraseDisplay::Above,
            "all" => EraseDisplay::All,
            other => panic!("{id}: unknown ed `{other}`"),
        }),
        "pen_bg" => Step::PenBg(match arg {
            "none" => None,
            n => Some(n.parse().expect("pen_bg index")),
        }),
        "reset" => Step::Reset,
        "feed" => Step::Feed {
            text: decode(arg),
            raw: arg.to_owned(),
        },
        "feed_bytewise" => Step::FeedBytewise {
            text: decode(arg),
            raw: arg.to_owned(),
        },
        "set" => {
            let on = match arg.split_once(' ').map(|(_, v)| v) {
                Some("true") => true,
                Some("false") => false,
                _ => panic!("{id}: `{step}` needs true|false"),
            };
            match arg.split_once(' ').map(|(m, _)| m) {
                Some("auto_wrap") => Step::AutoWrap(on),
                Some("insert_mode") => Step::InsertMode(on),
                _ => panic!("{id}: unknown mode in `{step}`"),
            }
        }
        other => panic!("{id}: unknown step `{other}`"),
    }
}

/// Decodes a fixture payload: `\u{H..}` is one scalar, `\\` a backslash,
/// `\r` and `\n` CR and LF; everything else is literal.
///
/// # Panics
///
/// On any other escape or a malformed `\u{..}`, so a typo never hides.
pub fn decode(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len());
    let mut chars = payload.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('u') => {
                let rest = chars.as_str();
                let (hex, after) = rest
                    .strip_prefix('{')
                    .and_then(|r| r.split_once('}'))
                    .unwrap_or_else(|| panic!("bad `\\u{{..}}` in {payload:?}"));
                let scalar = Some(hex)
                    .filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .and_then(char::from_u32)
                    .unwrap_or_else(|| panic!("bad `\\u{{{hex}}}` in {payload:?}"));
                out.push(scalar);
                chars = after.chars();
            }
            other => panic!("unknown escape `\\{}` in {payload:?}", other.unwrap_or(' ')),
        }
    }
    out
}

/// Runs a grid-layer case's steps on a fresh grid.
///
/// # Panics
///
/// On a term-only step.
pub fn run_grid(case: &Case) -> Grid {
    let mut grid = Grid::new(case.size.0, case.size.1);
    for step in &case.steps {
        apply(&mut grid, step);
    }
    grid
}

/// Applies one grid step.
///
/// # Panics
///
/// On a term-only step.
pub fn apply(grid: &mut Grid, step: &Step) {
    match step {
        Step::Put(text) => grid.put_str(text),
        &Step::Goto(row, col) => grid.goto(row, col),
        Step::Cr => grid.carriage_return(),
        Step::Lf => grid.line_feed(),
        &Step::Ich(n) => grid.insert_chars(n),
        &Step::Dch(n) => grid.delete_chars(n),
        &Step::Ech(n) => grid.erase_chars(n),
        &Step::El(mode) => grid.erase_in_line(mode),
        &Step::Ed(mode) => grid.erase_in_display(mode),
        &Step::PenBg(bg) => grid.set_pen(Style {
            bg: bg.map_or(Color::Default, Color::Indexed),
            ..Style::DEFAULT
        }),
        Step::Reset => grid.reset(),
        other => panic!("{other:?} is a term-layer step"),
    }
}
