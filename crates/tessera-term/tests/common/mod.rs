//! Script-driven harness for emulator tests.
//!
//! A test feeds bytes into a [`Terminal`] and compares a plain-text render
//! of the result, usually as an `insta` snapshot. The render
//! ([`render_screen`]) has one line per grid row, top to bottom, each ending
//! in `\n`:
//!
//! 1. the cell characters, one per column (a blank cell is a space). The
//!    right half of a wide character (a [`CellFlags::SPACER`]) renders as
//!    nothing, so `中文` shows as two glyphs covering four columns in a
//!    monospace viewer, and a [`CellFlags::CLUSTER`] cell renders its whole
//!    cluster text ([`Line::cluster`]). Cell counts are therefore not
//!    visible here; `tessera-grid`'s cell-level render shows them;
//! 2. on the cursor's row, `|` immediately after the cursor's cell;
//! 3. trailing spaces stripped (after step 2, so `|` anchors the line);
//! 4. `\` appended when the line is soft-wrapped ([`Line::wrapped`]).
//!
//! # Script files
//!
//! Scripts live under `tests/scripts/<group>/*.txt`, one case per file:
//!
//! - Blank lines and lines starting with `#` are skipped.
//! - `@size <rows>x<cols>` gives the terminal size. Required, exactly once,
//!   before any other step.
//! - `@set auto_wrap true|false` and `@set insert_mode true|false` toggle
//!   modes through the [`Terminal`] API, for modes that have no escape
//!   sequence wired up yet.
//! - Every other line is one payload, fed with one [`Terminal::advance`]
//!   call after [`decode`]. The line break that ends it is not part of it.
//!
//! # Limits of the render
//!
//! The markers share characters with printable text, so a render alone
//! cannot tell them apart: a printed `|` looks like the cursor marker, and a
//! printed `\` in the last non-blank column of a row the cursor is not on
//! looks like the wrapped marker (`ab\` renders the same as a soft-wrapped
//! `ab`). Scripts that print either character should also check the state
//! [`render_state`] reports, or assert on the grid directly.
//!
//! [`Line::wrapped`]: tessera_grid::Line::wrapped
//! [`Line::cluster`]: tessera_grid::Line::cluster
//! [`CellFlags::SPACER`]: tessera_grid::CellFlags::SPACER
//! [`CellFlags::CLUSTER`]: tessera_grid::CellFlags::CLUSTER

use std::fmt::Write as _;
use std::path::Path;

use tessera_grid::{Color, Style, UnderlineStyle};
use tessera_term::{Request, Terminal};

/// One step of a [`Script`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Bytes for one [`Terminal::advance`] call.
    Feed(Vec<u8>),
    /// [`Terminal::set_auto_wrap`].
    AutoWrap(bool),
    /// [`Terminal::set_insert_mode`].
    InsertMode(bool),
}

impl Step {
    /// Parses the argument of a `set` directive: `auto_wrap true`,
    /// `insert_mode false`, ….
    ///
    /// # Panics
    ///
    /// On an unknown mode or a value other than `true` / `false`.
    pub fn parse_set(arg: &str) -> Self {
        let (mode, value) = arg
            .split_once(' ')
            .unwrap_or_else(|| panic!("`set {arg}`: expected `<mode> true|false`"));
        let on = match value.trim() {
            "true" => true,
            "false" => false,
            other => panic!("`set {arg}`: `{other}` is not true or false"),
        };
        match mode {
            "auto_wrap" => Self::AutoWrap(on),
            "insert_mode" => Self::InsertMode(on),
            other => panic!("`set {arg}`: unknown mode `{other}`"),
        }
    }
}

/// How [`Script::run`] hands payloads to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chunking {
    /// One [`Terminal::advance`] call per payload.
    Whole,
    /// One [`Terminal::advance`] call per byte, to prove the result does not
    /// depend on how PTY reads split the stream.
    Bytewise,
}

/// A terminal size plus the steps to run on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// Visible rows.
    pub rows: u16,
    /// Columns.
    pub cols: u16,
    /// Steps, in order, on one fresh terminal.
    pub steps: Vec<Step>,
}

impl Script {
    /// Parses the script format described in the module documentation.
    ///
    /// # Panics
    ///
    /// On a malformed directive, a missing or repeated `@size`, or a payload
    /// [`decode`] rejects.
    pub fn parse(text: &str) -> Self {
        let mut size = None;
        let mut steps = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(directive) = line.strip_prefix('@') {
                let (name, arg) = directive.split_once(' ').unwrap_or((directive, ""));
                match name {
                    "size" => {
                        assert!(
                            size.is_none() && steps.is_empty(),
                            "`@size` must come once, before any step"
                        );
                        size = Some(parse_size(arg));
                    }
                    "set" => steps.push(Step::parse_set(arg)),
                    other => panic!("unknown directive `@{other}`"),
                }
            } else {
                steps.push(Step::Feed(decode(line)));
            }
        }
        let (rows, cols) = size.expect("script has no `@size <rows>x<cols>` line");
        Self { rows, cols, steps }
    }

    /// Reads and parses a script file.
    ///
    /// # Panics
    ///
    /// When the file cannot be read or does not parse.
    pub fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        Self::parse(&text)
    }

    /// Runs the steps on a fresh terminal and returns it together with every
    /// request it emitted.
    pub fn run(&self, chunking: Chunking) -> (Terminal, Vec<Request>) {
        let mut term = Terminal::new(self.rows, self.cols);
        for step in &self.steps {
            match step {
                Step::Feed(bytes) => match chunking {
                    Chunking::Whole => term.advance(bytes),
                    Chunking::Bytewise => {
                        for byte in bytes {
                            term.advance(std::slice::from_ref(byte));
                        }
                    }
                },
                Step::AutoWrap(on) => term.set_auto_wrap(*on),
                Step::InsertMode(on) => term.set_insert_mode(*on),
            }
        }
        let requests = term.take_requests();
        (term, requests)
    }

    /// Runs the steps and renders the outcome with [`render_state`].
    pub fn run_rendered(&self, chunking: Chunking) -> String {
        let (term, requests) = self.run(chunking);
        render_state(&term, &requests)
    }
}

/// Parses `<rows>x<cols>`.
fn parse_size(arg: &str) -> (u16, u16) {
    let parsed = arg
        .trim()
        .split_once('x')
        .and_then(|(rows, cols)| Some((rows.parse().ok()?, cols.parse().ok()?)));
    parsed.unwrap_or_else(|| panic!("`@size {arg}`: expected `<rows>x<cols>`"))
}

/// Decodes C-style escapes in a script payload into bytes.
///
/// `\xNN` is one raw byte, `\r`, `\n`, `\t` and `\\` are what they are in C,
/// and `\u{H..}` is one Unicode scalar value, UTF-8 encoded. Everything else
/// is literal UTF-8.
///
/// # Panics
///
/// On any other escape, so a typo never silently prints a backslash.
pub fn decode(payload: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut chars = payload.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buf = [0; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                let byte = Some(&hex)
                    .filter(|hex| hex.len() == 2 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                    .unwrap_or_else(|| panic!("`\\x{hex}` in {payload:?}: need two hex digits"));
                out.push(byte);
            }
            Some('u') => {
                let rest = chars.as_str();
                let scalar = rest
                    .strip_prefix('{')
                    .and_then(|r| r.split_once('}'))
                    .map(|(hex, _)| hex)
                    .filter(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .and_then(char::from_u32)
                    .unwrap_or_else(|| panic!("bad `\\u{{..}}` escape in {payload:?}"));
                let mut buf = [0; 4];
                out.extend_from_slice(scalar.encode_utf8(&mut buf).as_bytes());
                chars.by_ref().take_while(|&c| c != '}').for_each(drop);
            }
            other => panic!("unknown escape `\\{}` in {payload:?}", other.unwrap_or(' ')),
        }
    }
    out
}

/// Feeds `bytes` into a fresh `rows × cols` terminal in one call and renders
/// its screen with [`render_screen`].
pub fn run_script(bytes: &[u8], rows: u16, cols: u16) -> String {
    let mut term = Terminal::new(rows, cols);
    term.advance(bytes);
    render_screen(&term)
}

/// Renders the visible grid as described in the module documentation.
pub fn render_screen(term: &Terminal) -> String {
    let grid = term.grid();
    let cursor = grid.cursor();
    let mut out = String::new();
    for (row, line) in (0..grid.rows()).zip(grid.visible_lines()) {
        // One piece of text per cell, so the cursor marker lands after the
        // cursor's cell even when cells render as zero or several chars.
        let mut text: Vec<String> = line
            .cells
            .iter()
            .enumerate()
            .map(|(col, cell)| match line.cluster(col) {
                _ if cell.is_spacer() => String::new(),
                Some(cluster) if cell.is_cluster() => cluster.to_owned(),
                _ => String::from(cell.ch),
            })
            .collect();
        if row == cursor.row {
            text.insert(usize::from(cursor.col) + 1, String::from('|'));
        }
        let end = text.iter().rposition(|t| t != " ").map_or(0, |i| i + 1);
        out.extend(text.drain(..end));
        if line.wrapped {
            out.push('\\');
        }
        out.push('\n');
    }
    out
}

/// Renders the screen plus the state the screen cannot show: the cursor
/// position (0-based), the pending-wrap flag, the modes, the requests and
/// the scrollback (oldest first, with the same `\` wrap marker).
pub fn render_state(term: &Terminal, requests: &[Request]) -> String {
    let grid = term.grid();
    let cursor = grid.cursor();
    let mut out = render_screen(term);
    out.push_str("--\n");
    // Writing to a `String` cannot fail.
    let _ = writeln!(out, "cursor: ({},{})", cursor.row, cursor.col);
    let _ = writeln!(out, "pending_wrap: {}", grid.pending_wrap());
    let _ = writeln!(
        out,
        "auto_wrap: {} insert_mode: {}",
        term.auto_wrap(),
        term.insert_mode()
    );
    let _ = writeln!(out, "requests: {requests:?}");
    let _ = writeln!(out, "scrollback: {}", grid.scrollback_len());
    for line in (0..grid.scrollback_len()).filter_map(|i| grid.scrollback_line(i)) {
        let mark = if line.wrapped { "\\" } else { "" };
        let _ = writeln!(out, "  {:?}{mark}", line.text());
    }
    out
}

/// A colour in the canonical pen notation: `-`, `iN` or `#rrggbb`.
#[allow(dead_code)] // Used by esctest.rs; print.rs and wide.rs include this module without the allow.
pub fn color_text(color: Color) -> String {
    match color {
        Color::Default => "-".to_owned(),
        Color::Indexed(n) => format!("i{n}"),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

/// A style in the canonical pen notation of `tests/fixtures/sgr_spec.txt`:
/// `fg=<c> bg=<c> ul=<c> us=<style> flags=<list>`, flags in a fixed order
/// (`bold,dim,italic,blink,inverse,hidden,strike`) or `-` for none.
///
/// Flag bits the notation has no name for (none exist today) are printed
/// as `?0x...`, so a stray bit can never pass as a clean pen. `tests/sgr.rs`
/// has its own copy (it predates this one).
#[allow(dead_code)] // Used by esctest.rs; print.rs and wide.rs include this module without the allow.
pub fn pen_text(style: Style) -> String {
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

/// Renders the cell styles of the visible grid that differ from
/// [`Style::default`], one line per run of equal styles within a row:
/// `(row,col0)-(row,col1) <pen>` (0-based, inclusive, pen as [`pen_text`]),
/// top to bottom, left to right. A grid with only default cells renders as
/// `pens: default`.
#[allow(dead_code)] // Used by esctest.rs; print.rs and wide.rs include this module without the allow.
pub fn render_pens(term: &Terminal) -> String {
    let mut out = String::new();
    for (row, line) in (0u16..).zip(term.grid().visible_lines()) {
        let mut col = 0;
        while col < line.cells.len() {
            let style = line.cells[col].style;
            let run = line.cells[col..]
                .iter()
                .take_while(|cell| cell.style == style)
                .count();
            if style != Style::default() {
                // Writing to a `String` cannot fail.
                let _ = writeln!(
                    out,
                    "({row},{col})-({row},{}) {}",
                    col + run - 1,
                    pen_text(style)
                );
            }
            col += run;
        }
    }
    if out.is_empty() {
        out.push_str("pens: default\n");
    }
    out
}
