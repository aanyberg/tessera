//! SGR (`CSI Pm m`): attributes, 16/256/24-bit colours, underline styles
//! and underline colour on the pen, and how the pen reaches cells.
//!
//! Expected behaviour comes from `tests/fixtures/sgr_spec.txt`, a fixture
//! that cites xterm, ECMA-48, kitty, `WezTerm` and Alacritty per row. Its
//! styles are written in a canonical text form that does not depend on
//! `Style`'s bit layout; [`pen_text`] prints exactly that form.
//!
//! - [`pen_rows_hold`] runs every pen row (parts A and B), fed as written
//!   and byte by byte, and compares the pen. Rows marked `free` document an
//!   accepted divergence and are parsed but not asserted.
//! - [`cell_rows_hold`] runs every cell block (part C: what printing,
//!   erasing, scrolling, REP and DECSC/DECRC do with the pen).
//! - [`color_table_matches_the_oracle`] runs `tests/scripts/sgr/color_table.txt`
//!   and checks every cell against `tests/fixtures/sgr_color_table_cells.txt`,
//!   which was generated from the colour numbers rather than from the
//!   script; [`color_table_snapshot`] pins the same screen as a compact
//!   per-run style render.
//! - Property tests feed random parameter lists.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use common::{Chunking, Script, decode};
use proptest::prelude::*;
use tessera_grid::{Cell, Color, Grid, Style, UnderlineStyle};
use tessera_term::Terminal;

const FIXTURE: &str = include_str!("fixtures/sgr_spec.txt");
const COLOR_TABLE_CELLS: &str = include_str!("fixtures/sgr_color_table_cells.txt");

// --------------------------------------------------------------- rendering

/// A colour in the fixture's notation: `-`, `iN` or `#rrggbb`.
fn color_text(color: Color) -> String {
    match color {
        Color::Default => "-".to_owned(),
        Color::Indexed(n) => format!("i{n}"),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

/// A style in the fixture's canonical notation:
/// `fg=<c> bg=<c> ul=<c> us=<style> flags=<list>`.
///
/// Flag bits the notation has no name for (none exist today) are printed
/// as `?0x...`, so a stray bit can never pass as a clean pen.
fn pen_text(style: Style) -> String {
    const NAMES: [(u16, &str); 7] = [
        (Style::BOLD, "bold"),
        (Style::DIM, "dim"),
        (Style::ITALIC, "italic"),
        (Style::BLINK, "blink"),
        (Style::INVERSE, "inverse"),
        (Style::HIDDEN, "hidden"),
        (Style::STRIKETHROUGH, "strike"),
    ];
    let us = match style.underline_style() {
        UnderlineStyle::None => "none",
        UnderlineStyle::Single => "single",
        UnderlineStyle::Double => "double",
        UnderlineStyle::Curly => "curly",
        UnderlineStyle::Dotted => "dotted",
        UnderlineStyle::Dashed => "dashed",
    };
    let mut flags: Vec<String> = NAMES
        .iter()
        .filter(|(bit, _)| style.has(*bit))
        .map(|(_, name)| (*name).to_owned())
        .collect();
    let known = NAMES
        .iter()
        .fold(Style::UNDERLINE_STYLE_MASK, |m, (b, _)| m | b);
    if style.flags & !known != 0 {
        flags.push(format!("?{:#x}", style.flags & !known));
    }
    let flags = if flags.is_empty() {
        "-".to_owned()
    } else {
        flags.join(",")
    };
    format!(
        "fg={} bg={} ul={} us={us} flags={flags}",
        color_text(style.fg),
        color_text(style.bg),
        color_text(style.underline),
    )
}

/// Feeds every payload of `feeds` into `term`, in one call each or one
/// byte per call.
fn feed(term: &mut Terminal, feeds: &[Vec<u8>], chunking: Chunking) {
    for bytes in feeds {
        match chunking {
            Chunking::Whole => term.advance(bytes),
            Chunking::Bytewise => {
                for byte in bytes {
                    term.advance(std::slice::from_ref(byte));
                }
            }
        }
    }
}

// ----------------------------------------------------------- part A and B

/// One pen row: `ID | feed | pen [| free]`.
#[derive(Debug)]
struct PenRow {
    id: String,
    /// One payload per [`Terminal::advance`] call (`++` in the fixture).
    feeds: Vec<Vec<u8>>,
    pen: String,
    free: bool,
}

/// Every pen row, in file order: the non-comment lines before part C.
fn pen_rows() -> Vec<PenRow> {
    FIXTURE
        .lines()
        .take_while(|line| !line.starts_with("### "))
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split(" | ").map(str::trim).collect();
            let (id, feed, pen, free) = match fields[..] {
                [id, feed, pen] => (id, feed, pen, false),
                [id, feed, pen, "free"] => (id, feed, pen, true),
                _ => panic!("malformed pen row: {line}"),
            };
            PenRow {
                id: id.to_owned(),
                feeds: feed.split("++").map(decode).collect(),
                pen: pen.to_owned(),
                free,
            }
        })
        .collect()
}

/// The pen after feeding a row into a fresh 24x80 terminal.
fn run_pen_row(row: &PenRow, chunking: Chunking) -> String {
    let mut term = Terminal::new(24, 80);
    feed(&mut term, &row.feeds, chunking);
    pen_text(term.grid().pen())
}

#[test]
fn pen_rows_hold() {
    let rows = pen_rows();
    assert!(rows.len() > 250, "only {} pen rows parsed", rows.len());
    let mut ids = HashSet::new();
    for row in &rows {
        assert!(ids.insert(&row.id), "duplicate id {}", row.id);
    }
    let errors: Vec<String> = rows
        .iter()
        .filter(|row| !row.free)
        .flat_map(|row| {
            [Chunking::Whole, Chunking::Bytewise]
                .into_iter()
                .filter_map(move |chunking| {
                    let got = run_pen_row(row, chunking);
                    (got != row.pen).then(|| {
                        format!(
                            "{} ({chunking:?}):\n  expected {}\n  got      {got}",
                            row.id, row.pen
                        )
                    })
                })
        })
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// The `free` rows are documentation, but they must still run cleanly and
/// give the same answer however they are fed.
#[test]
fn free_rows_run_and_ignore_chunking() {
    let free: Vec<PenRow> = pen_rows().into_iter().filter(|row| row.free).collect();
    assert_eq!(free.len(), 5, "free rows in the fixture");
    for row in &free {
        assert_eq!(
            run_pen_row(row, Chunking::Whole),
            run_pen_row(row, Chunking::Bytewise),
            "{}",
            row.id
        );
    }
}

// ----------------------------------------------------------------- part C

/// What a cell block expects of one cell.
#[derive(Debug)]
struct CellExpect {
    /// The cell's text: one scalar, or a whole cluster.
    text: String,
    wide: bool,
    cluster: bool,
    spacer: bool,
    pen: String,
}

/// One `### ID` block of part C.
#[derive(Debug, Default)]
struct CellCase {
    id: String,
    size: (u16, u16),
    feeds: Vec<Vec<u8>>,
    cells: HashMap<(u16, u16), CellExpect>,
    cursor: Option<(u16, u16)>,
    pending_wrap: Option<bool>,
    wrapped: Vec<(u16, bool)>,
    scrollback: Option<(usize, String)>,
    sb_cells: Vec<(usize, usize, CellExpect)>,
    others_blank: bool,
}

/// Parses `(r,c)`.
fn parse_point(raw: &str) -> (u16, u16) {
    let inner = raw
        .trim()
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or_else(|| panic!("`{raw}` is not `(r,c)`"));
    let (r, c) = inner.split_once(',').expect("(r,c)");
    (
        r.trim().parse().expect("row"),
        c.trim().parse().expect("col"),
    )
}

/// Parses `'<content>' <pen>`, where content is `<spacer>` or a text
/// followed by the markers `wide` and `cluster`.
fn parse_cell_expect(raw: &str) -> CellExpect {
    let raw = raw.trim();
    let close = raw.find("' fg=").unwrap_or_else(|| panic!("cell `{raw}`"));
    let mut content = &raw[1..close];
    let pen = raw[close + 2..].to_owned();
    if content == "<spacer>" {
        return CellExpect {
            text: " ".to_owned(),
            wide: false,
            cluster: false,
            spacer: true,
            pen,
        };
    }
    let (mut wide, mut cluster) = (false, false);
    while let Some((rest, marker)) = content.rsplit_once(' ') {
        match marker {
            "wide" => wide = true,
            "cluster" => cluster = true,
            _ => break,
        }
        content = rest;
    }
    CellExpect {
        text: content.to_owned(),
        wide,
        cluster,
        spacer: false,
        pen,
    }
}

/// Every `### ID` block of part C.
fn cell_cases() -> Vec<CellCase> {
    let mut cases = Vec::new();
    let mut lines = FIXTURE.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(id) = line.strip_prefix("### ") else {
            continue;
        };
        let mut case = CellCase {
            id: id.to_owned(),
            ..CellCase::default()
        };
        while let Some(line) = lines.next_if(|l| !l.starts_with("### ") && !l.starts_with('#')) {
            let Some((key, raw)) = line.split_once(": ") else {
                continue;
            };
            match key {
                "size" => {
                    let (r, c) = raw.trim().split_once('x').expect("size RxC");
                    case.size = (r.parse().expect("rows"), c.parse().expect("cols"));
                }
                "feed" => case.feeds.push(decode(raw)),
                "cell" => {
                    let (point, rest) = raw.split_once(") ").expect("cell (r,c) ...");
                    let point = parse_point(&format!("{point})"));
                    let expect = parse_cell_expect(rest);
                    assert!(
                        case.cells.insert(point, expect).is_none(),
                        "{id}: cell {point:?} listed twice"
                    );
                }
                "cursor" => case.cursor = Some(parse_point(raw)),
                "pending_wrap" => case.pending_wrap = Some(raw.trim() == "true"),
                "wrapped" => {
                    let (row, value) = raw.split_once(" = ").expect("wrapped: R = bool");
                    case.wrapped
                        .push((row.trim().parse().expect("row"), value.trim() == "true"));
                }
                "scrollback" => {
                    let rest = raw.trim().strip_prefix("len=").expect("len=");
                    let (len, first) = rest.split_once(' ').expect("len=N first=(..)");
                    let first = first
                        .strip_prefix("first=(\"")
                        .and_then(|f| f.strip_suffix("\")"))
                        .expect("first=(\"..\")");
                    case.scrollback = Some((len.parse().expect("len"), first.to_owned()));
                }
                "sb_cell" => {
                    let (index, rest) = raw.trim().split_once(' ').expect("sb_cell N (r,c) ..");
                    let (point, rest) = rest.split_once(") ").expect("(r,c)");
                    let (_, col) = parse_point(&format!("{point})"));
                    case.sb_cells.push((
                        index.parse().expect("index"),
                        usize::from(col),
                        parse_cell_expect(rest),
                    ));
                }
                "others" => case.others_blank = raw.trim() == "blank",
                "cite" | "note" => {}
                other => panic!("{id}: unknown key `{other}`"),
            }
        }
        cases.push(case);
    }
    cases
}

/// Every mismatch between `cell` and `expect`.
fn cell_errors(cell: &Cell, cluster_text: Option<&str>, expect: &CellExpect) -> Vec<String> {
    let mut errors = Vec::new();
    let text = if cell.is_cluster() {
        cluster_text.unwrap_or_default().to_owned()
    } else {
        cell.ch.to_string()
    };
    if text != expect.text {
        errors.push(format!("text {text:?}, expected {:?}", expect.text));
    }
    let flags = (cell.is_wide(), cell.is_cluster(), cell.is_spacer());
    if flags != (expect.wide, expect.cluster, expect.spacer) {
        errors.push(format!("(wide, cluster, spacer) = {flags:?}"));
    }
    let pen = pen_text(cell.style);
    if pen != expect.pen {
        errors.push(format!("style {pen}, expected {}", expect.pen));
    }
    errors
}

/// Every mismatch between a cell block and what the terminal did.
fn check_cell_case(case: &CellCase, chunking: Chunking) -> Vec<String> {
    let mut term = Terminal::new(case.size.0, case.size.1);
    feed(&mut term, &case.feeds, chunking);
    let grid = term.grid();
    let mut errors = Vec::new();
    let mut fail = |what: String| errors.push(format!("{} ({chunking:?}) {what}", case.id));
    for (row, line) in (0..grid.rows()).zip(grid.visible_lines()) {
        for (col, cell) in (0..grid.cols()).zip(&line.cells) {
            match case.cells.get(&(row, col)) {
                Some(expect) => {
                    for e in cell_errors(cell, line.cluster(usize::from(col)), expect) {
                        fail(format!("cell ({row},{col}): {e}"));
                    }
                }
                None if case.others_blank && *cell != Cell::BLANK => {
                    fail(format!("cell ({row},{col}) is not blank: {cell:?}"));
                }
                None => {}
            }
        }
    }
    if let Some((row, col)) = case.cursor {
        let cursor = grid.cursor();
        if (cursor.row, cursor.col) != (row, col) {
            fail(format!("cursor {cursor:?}, expected ({row},{col})"));
        }
    }
    if let Some(pending) = case.pending_wrap
        && grid.pending_wrap() != pending
    {
        fail(format!("pending_wrap {}, expected {pending}", !pending));
    }
    for &(row, wrapped) in &case.wrapped {
        let got = grid.line(row).map(|l| l.wrapped);
        if got != Some(wrapped) {
            fail(format!("row {row} wrapped {got:?}, expected {wrapped}"));
        }
    }
    if let Some((len, first)) = &case.scrollback {
        if grid.scrollback_len() != *len {
            fail(format!(
                "scrollback len {}, expected {len}",
                grid.scrollback_len()
            ));
        }
        let got = grid.scrollback_line(0).map(tessera_grid::Line::text);
        if got.as_deref() != Some(first.as_str()) {
            fail(format!("scrollback[0] {got:?}, expected {first:?}"));
        }
    }
    for (index, col, expect) in &case.sb_cells {
        match grid.scrollback_line(*index) {
            Some(line) => {
                for e in cell_errors(&line.cells[*col], line.cluster(*col), expect) {
                    fail(format!("scrollback[{index}] col {col}: {e}"));
                }
            }
            None => fail(format!("no scrollback line {index}")),
        }
    }
    errors
}

#[test]
fn cell_rows_hold() {
    let cases = cell_cases();
    assert_eq!(cases.len(), 33, "cell blocks in the fixture");
    for case in &cases {
        assert!(
            !case.feeds.is_empty() && !case.cells.is_empty(),
            "{}",
            case.id
        );
    }
    let errors: Vec<String> = cases
        .iter()
        .flat_map(|case| {
            [Chunking::Whole, Chunking::Bytewise]
                .into_iter()
                .flat_map(|chunking| check_cell_case(case, chunking))
        })
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

// ----------------------------------------------------------------- part D

fn color_table_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/sgr/color_table.txt")
}

fn color_table_script() -> Script {
    Script::load(&color_table_path())
}

/// `(row, col)` -> (label character, pen) for every labelled cell.
fn color_table_oracle() -> HashMap<(u16, u16), (char, String)> {
    let mut cells = HashMap::new();
    let mut groups = 0;
    for line in COLOR_TABLE_CELLS.lines().filter(|l| !l.starts_with('#')) {
        let (head, pen) = line.split_once(" | ").expect("`row col \"label\" | pen`");
        let mut parts = head.splitn(3, ' ');
        let row: u16 = parts.next().and_then(|r| r.parse().ok()).expect("row");
        let col: u16 = parts.next().and_then(|c| c.parse().ok()).expect("col");
        let label = parts
            .next()
            .and_then(|l| l.strip_prefix('"'))
            .and_then(|l| l.strip_suffix('"'))
            .expect("quoted label");
        for (offset, ch) in (0..).zip(label.chars()) {
            cells.insert((row, col + offset), (ch, pen.trim().to_owned()));
        }
        groups += 1;
    }
    assert_eq!(groups, 433, "groups in the colour-table oracle");
    cells
}

#[test]
fn color_table_matches_the_oracle() {
    let script = color_table_script();
    let oracle = color_table_oracle();
    for chunking in [Chunking::Whole, Chunking::Bytewise] {
        let (term, _) = script.run(chunking);
        let grid = term.grid();
        let mut errors = Vec::new();
        for (row, line) in (0..grid.rows()).zip(grid.visible_lines()) {
            for (col, cell) in (0..grid.cols()).zip(&line.cells) {
                match oracle.get(&(row, col)) {
                    Some((ch, pen)) => {
                        let got = pen_text(cell.style);
                        if cell.ch != *ch || got != *pen || !cell.flags.is_empty() {
                            errors.push(format!(
                                "({row},{col}): {:?} {got}, expected {ch:?} {pen}",
                                cell.ch
                            ));
                        }
                    }
                    None if *cell != Cell::BLANK => {
                        errors.push(format!("({row},{col}) not blank: {cell:?}"));
                    }
                    None => {}
                }
            }
        }
        assert!(errors.is_empty(), "{chunking:?}\n{}", errors.join("\n"));
    }
}

/// Renders every run of equally styled, non-blank cells of the screen as
/// `row:col "text" <pen>`, one run per line.
fn render_runs(grid: &Grid) -> String {
    let mut out = String::new();
    for (row, line) in (0..grid.rows()).zip(grid.visible_lines()) {
        let mut col = 0;
        while col < line.cells.len() {
            let cell = line.cells[col];
            if cell == Cell::BLANK {
                col += 1;
                continue;
            }
            let start = col;
            let mut text = String::new();
            while col < line.cells.len()
                && line.cells[col].style == cell.style
                && line.cells[col] != Cell::BLANK
            {
                text.push(line.cells[col].ch);
                col += 1;
            }
            // Writing to a `String` cannot fail.
            let _ = writeln!(out, "{row}:{start} {text:?} {}", pen_text(cell.style));
        }
    }
    out
}

#[test]
fn color_table_snapshot() {
    let script = color_table_script();
    let whole = render_runs(script.run(Chunking::Whole).0.grid());
    let bytewise = render_runs(script.run(Chunking::Bytewise).0.grid());
    assert_eq!(whole, bytewise, "feeding byte by byte changed the result");
    insta::with_settings!({ input_file => color_table_path(), omit_expression => true }, {
        insta::assert_snapshot!("color_table", whole);
    });
}

// ------------------------------------------------------------ direct tests

/// DECSC saves the whole pen, underline colour and style included, and
/// DECRC brings it back for the next printed character (lead decision 5).
#[test]
fn decsc_decrc_round_trip_the_whole_pen() {
    let mut term = Terminal::new(2, 10);
    term.advance(b"\x1b[1;3;4:4;38;2;1;2;3;48;5;200;58:2::9:8:7m\x1b7");
    let saved = term.grid().pen();
    term.advance(b"\x1b[0;7mzz\x1b8X");
    assert_eq!(term.grid().pen(), saved);
    let cell = term.grid().line(0).expect("row 0").cells[0];
    assert_eq!(cell.ch, 'X');
    assert_eq!(cell.style, saved);
    assert_eq!(
        pen_text(saved),
        "fg=#010203 bg=i200 ul=#090807 us=dotted flags=bold,italic"
    );
}

/// A `;` colour that reads fewer values than its parameter has
/// sub-parameters leaves the rest of that group unread; they are skipped,
/// never applied as SGR codes of their own.
#[test]
fn sub_parameters_left_after_a_semicolon_colour_are_skipped() {
    let pen = |bytes: &[u8]| {
        let mut term = Terminal::new(1, 1);
        term.advance(bytes);
        pen_text(term.grid().pen())
    };
    // `58;48` is an unknown colour kind: `58` and `48` are eaten, then the
    // `:40` sub-parameter must not set a background.
    assert_eq!(
        pen(b"\x1b[58;48:40;1m"),
        "fg=- bg=- ul=- us=none flags=bold"
    );
    // `38;5;1` reads the index; the `:2` after it must not turn on dim.
    assert_eq!(pen(b"\x1b[38;5;1:2m"), "fg=i1 bg=- ul=- us=none flags=-");
}

/// The ANSI colour codes map onto indexes 0-15 in order.
#[test]
fn ansi_codes_map_to_indexes() {
    let mut term = Terminal::new(1, 1);
    for n in 0..8u8 {
        for (code, base, bg) in [
            (30, 0, false),
            (40, 0, true),
            (90, 8, false),
            (100, 8, true),
        ] {
            term.advance(format!("\x1b[0;{}m", code + u16::from(n)).as_bytes());
            let pen = term.grid().pen();
            let (set, other) = if bg {
                (pen.bg, pen.fg)
            } else {
                (pen.fg, pen.bg)
            };
            assert_eq!(set, Color::Indexed(base + n), "SGR {}", code + u16::from(n));
            assert_eq!(other, Color::Default);
        }
    }
}

// ---------------------------------------------------------------- property

/// One SGR parameter: a value and whether `:` (rather than `;`) precedes
/// it. Values cluster around the codes that consume further parameters.
fn sgr_param() -> impl Strategy<Value = (u16, bool)> {
    let value = prop_oneof![
        4 => 0u16..=110,
        2 => prop::sample::select(vec![0u16, 2, 4, 5, 38, 48, 58, 255, 256]),
        1 => any::<u16>(),
    ];
    (value, any::<bool>())
}

/// `CSI <params> m` with the given separators.
fn sgr_bytes(params: &[(u16, bool)]) -> Vec<u8> {
    let mut out = b"\x1b[".to_vec();
    for (i, (value, colon)) in params.iter().enumerate() {
        if i > 0 {
            out.push(if *colon { b':' } else { b';' });
        }
        out.extend_from_slice(value.to_string().as_bytes());
    }
    out.push(b'm');
    out
}

/// At most one underline shape bit, and only together with `UNDERLINE`.
fn underline_bits_are_consistent(style: Style) -> bool {
    let shapes = style.flags & Style::UNDERLINE_STYLE_MASK & !Style::UNDERLINE;
    shapes.count_ones() <= 1 && (shapes == 0 || style.has(Style::UNDERLINE))
}

proptest! {
    /// Random parameter lists never panic, keep the underline invariant on
    /// the pen and on printed cells, and do not depend on chunking.
    #[test]
    fn random_sgr_keeps_the_underline_invariant(
        lists in prop::collection::vec(prop::collection::vec(sgr_param(), 0..40), 1..4),
    ) {
        let mut whole = Terminal::new(2, 8);
        let mut bytewise = Terminal::new(2, 8);
        for list in &lists {
            let bytes = sgr_bytes(list);
            whole.advance(&bytes);
            for byte in &bytes {
                bytewise.advance(std::slice::from_ref(byte));
            }
            whole.advance(b"x");
            bytewise.advance(b"x");
            let pen = whole.grid().pen();
            prop_assert!(underline_bits_are_consistent(pen), "{}", pen_text(pen));
            prop_assert_eq!(pen, bytewise.grid().pen());
        }
        for line in whole.grid().visible_lines() {
            for cell in &line.cells {
                prop_assert!(underline_bits_are_consistent(cell.style));
            }
        }
    }
}
