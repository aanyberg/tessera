//! A reader for `tests/fixtures/reflow_spec.txt`, the matching script
//! format under `tests/scripts/reflow/`, and a render of everything a
//! resize changes in the fixture's state-block format.
//!
//! Included by `tests/reflow.rs` only (through a `#[path]` module), on top
//! of the shared `common` module whose [`Step`] and [`decode`] it reuses.
//!
//! # State blocks
//!
//! [`render_state`] prints what the fixture's `before:` / `after:` blocks
//! state, one item per line with the indentation removed:
//!
//! ```text
//! size: 3x5  scrollback: 1
//! sb> [abc]+
//! v>  [中~e]+
//! v>  [f..]
//! cursor: (1,1)  pending_wrap: false  saved: (0,0,false)  region: 0-1
//! tabs: [0]
//! ```
//!
//! `sb>` rows are scrollback (oldest first) and `v>` rows the active
//! screen. A row is `[cells]`, then `+` when [`Line::wrapped`] is set and
//! ` marks=A,B` when it has marks. Cells are written contiguously, one
//! token each: `.` a blank, `_` padding (`CellFlags::PAD`, the column a
//! wrapped wide character left), `~` a spacer, a plain character as itself (a
//! wide one is recognisable by the `~` after it), `{e+0301}` a cluster
//! (base, then `+HHHH` per further scalar) and `{..}/W` a wide cluster; a
//! `@h` suffix gives a background of `Color::Indexed(0xh)`. `saved:` is the
//! DECSC slot, read by restoring it on a copy of the grid. While the
//! alternate screen is active a `stashed_primary:` section (`p>` rows and
//! the primary's cursor line, read by leaving the alternate screen on a
//! copy) comes before `tabs:`.

use std::fmt::Write as _;
use std::path::Path;

use tessera_grid::{Color, EraseLine, Grid, Line, Mark};

use crate::common::{Step, parse_step};

/// One operation of a fixture row or script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// A shared grid step: `put`, `goto`, `cr`, `lf`, `el`, `pen_bg`,
    /// `resize`.
    Grid(Step),
    /// Pushes a mark onto a visible row.
    Mark(u16, Mark),
    /// [`Grid::save_cursor`].
    Save,
    /// [`Grid::enter_alt_screen`].
    EnterAlt,
    /// [`Grid::leave_alt_screen`].
    LeaveAlt,
    /// [`Grid::set_scroll_region`].
    Region(u16, u16),
    /// [`Grid::clear_all_tab_stops`].
    ClearTabs,
    /// `tabs_mut().set(col)`.
    TabSet(u16),
}

impl Op {
    /// True for the operations whose outcome a row or script records: a
    /// resize, or leaving the alternate screen after one.
    fn is_checkpoint(&self, after_resize: bool) -> bool {
        matches!(self, Op::Grid(Step::Resize(..))) || (after_resize && *self == Op::LeaveAlt)
    }

    /// Parses a fixture `step:` value, a script line, or a `resize:` value
    /// passed as `resize RxC`.
    ///
    /// # Panics
    ///
    /// On anything the fixture's step syntax does not define.
    pub fn parse(id: &str, text: &str) -> Self {
        let (name, arg) = text.split_once(' ').unwrap_or((text, ""));
        let num = |s: &str| -> u16 {
            s.parse()
                .unwrap_or_else(|_| panic!("{id}: `{text}` needs a number"))
        };
        let pair = |s: &str| -> (u16, u16) {
            let (a, b) = s
                .split_once(' ')
                .unwrap_or_else(|| panic!("{id}: `{text}` needs two numbers"));
            (num(a), num(b))
        };
        match name {
            "mark_visible_row" => {
                let (row, mark) = arg
                    .split_once(' ')
                    .unwrap_or_else(|| panic!("{id}: `{text}` needs a row and a mark"));
                let mark = match mark {
                    "PromptStart" => Mark::PromptStart,
                    "CommandStart" => Mark::CommandStart,
                    "OutputStart" => Mark::OutputStart,
                    other => panic!("{id}: unknown mark `{other}`"),
                };
                Op::Mark(num(row), mark)
            }
            "save" => Op::Save,
            "enter_alt_screen" => Op::EnterAlt,
            "leave_alt_screen" => Op::LeaveAlt,
            "region" => {
                let (top, bottom) = pair(arg);
                Op::Region(top, bottom)
            }
            "clear_tabs" => Op::ClearTabs,
            "tabset" => Op::TabSet(num(arg)),
            // `put`, `goto`, `cr`, `lf`, `el`, `pen_bg`, `resize`: the
            // shared grid steps.
            _ => Op::Grid(parse_step(id, text)),
        }
    }

    /// Applies the operation to `grid`.
    pub fn apply(&self, grid: &mut Grid) {
        match self {
            Op::Grid(step) => crate::common::apply(grid, step),
            Op::Mark(row, mark) => grid
                .line_mut(*row)
                .expect("mark_visible_row on a visible row")
                .marks
                .push(mark.clone()),
            Op::Save => grid.save_cursor(),
            Op::EnterAlt => grid.enter_alt_screen(),
            Op::LeaveAlt => grid.leave_alt_screen(),
            &Op::Region(top, bottom) => grid.set_scroll_region(top, bottom),
            Op::ClearTabs => grid.clear_all_tab_stops(),
            &Op::TabSet(col) => grid.tabs_mut().set(col),
        }
    }

    /// The operation in script syntax (the inverse of [`Op::parse`] for
    /// script lines; `resize` becomes the `@resize` directive).
    pub fn to_script(&self) -> String {
        match self {
            Op::Grid(Step::Put(text)) => format!("put {}", encode(text)),
            Op::Grid(Step::Goto(row, col)) => format!("goto {row} {col}"),
            Op::Grid(Step::Cr) => "cr".into(),
            Op::Grid(Step::Lf) => "lf".into(),
            Op::Grid(Step::El(mode)) => match mode {
                EraseLine::ToEnd => "el to_end".into(),
                EraseLine::ToStart => "el to_start".into(),
                EraseLine::All => "el all".into(),
            },
            Op::Grid(Step::PenBg(bg)) => match bg {
                Some(n) => format!("pen_bg {n}"),
                None => "pen_bg none".into(),
            },
            Op::Grid(Step::Resize(rows, cols)) => format!("@resize {rows}x{cols}"),
            Op::Grid(other) => panic!("{other:?} is not a reflow step"),
            Op::Mark(row, mark) => format!("mark_visible_row {row} {mark:?}"),
            Op::Save => "save".into(),
            Op::EnterAlt => "enter_alt_screen".into(),
            Op::LeaveAlt => "leave_alt_screen".into(),
            Op::Region(top, bottom) => format!("region {top} {bottom}"),
            Op::ClearTabs => "clear_tabs".into(),
            Op::TabSet(col) => format!("tabset {col}"),
        }
    }
}

/// A payload in the fixture's escape syntax: zero-width scalars and edge
/// spaces as `\u{..}`, a backslash doubled.
fn encode(text: &str) -> String {
    let last = text.chars().count().saturating_sub(1);
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        let zero_width = tessera_grid::char_width(ch).is_none_or(|w| w == 0);
        if zero_width || (ch == ' ' && (i == 0 || i == last)) {
            let _ = write!(out, "\\u{{{:X}}}", u32::from(ch));
        } else if ch == '\\' {
            out.push_str("\\\\");
        } else {
            out.push(ch);
        }
    }
    out
}

/// One `### ` row of the fixture.
#[derive(Debug, Clone)]
pub struct Row {
    /// The row id, `A01_wrapped_line_split_and_rejoined`.
    pub id: String,
    /// The `desc:` line.
    pub desc: String,
    /// Size before the operations.
    pub size: (u16, u16),
    /// `scrollback_cap:`, if given.
    pub cap: Option<usize>,
    /// Every operation in order, setup steps and checkpoints alike.
    pub ops: Vec<Op>,
    /// The `before:` block (state after the setup steps), normalised.
    pub before: String,
    /// One normalised `after:` block per checkpoint operation, in order.
    pub after: Vec<String>,
    /// `flags: ALTERNATIVE`: documents a rejected reading; not asserted.
    pub alternative: bool,
}

impl Row {
    /// A fresh grid of the row's size and scrollback cap.
    pub fn grid(&self) -> Grid {
        Grid::with_scrollback(
            self.size.0,
            self.size.1,
            self.cap.unwrap_or(tessera_grid::DEFAULT_MAX_SCROLLBACK),
        )
    }

    /// The row as a script (see [`Script`]).
    pub fn to_script(&self) -> Script {
        Script {
            size: self.size,
            cap: self.cap,
            ops: self.ops.clone(),
        }
    }
}

/// The fixture text, embedded at compile time.
pub const FIXTURE: &str = include_str!("../fixtures/reflow_spec.txt");

/// Parses every row of [`FIXTURE`].
///
/// # Panics
///
/// On any key, step or block the row syntax does not define.
pub fn rows() -> Vec<Row> {
    let mut rows = Vec::new();
    let mut lines = FIXTURE.lines().peekable();
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
        rows.push(parse_row(id.trim(), &body));
    }
    rows
}

fn parse_row(id: &str, body: &[&str]) -> Row {
    let mut row = Row {
        id: id.to_owned(),
        desc: String::new(),
        size: (0, 0),
        cap: None,
        ops: Vec::new(),
        before: String::new(),
        after: Vec::new(),
        alternative: false,
    };
    let mut checkpoints = 0;
    let mut resized = false;
    // `None` while outside a block, else whether it is the `before:` one.
    let mut block: Option<bool> = None;
    for line in body {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(content) = line.strip_prefix("    ") {
            let target = match block {
                Some(true) => &mut row.before,
                Some(false) => row.after.last_mut().expect("an after block"),
                None => panic!("{id}: block line outside a block: `{line}`"),
            };
            target.push_str(content.trim());
            target.push('\n');
            continue;
        }
        match line.trim() {
            "before:" => {
                block = Some(true);
                continue;
            }
            "after:" => {
                assert_eq!(
                    row.after.len() + 1,
                    checkpoints,
                    "{id}: one after per checkpoint"
                );
                row.after.push(String::new());
                block = Some(false);
                continue;
            }
            _ => block = None,
        }
        let (key, value) = line
            .split_once(": ")
            .unwrap_or_else(|| panic!("{id}: malformed line `{line}`"));
        match key {
            "desc" => value.clone_into(&mut row.desc),
            "layer" => assert_eq!(value, "grid", "{id}: only grid rows exist"),
            "size" => row.size = parse_size(value),
            "scrollback_cap" => row.cap = Some(value.parse().expect("scrollback_cap")),
            "step" | "resize" => {
                let op = if key == "resize" {
                    Op::parse(id, &format!("resize {value}"))
                } else {
                    Op::parse(id, value)
                };
                if op.is_checkpoint(resized) {
                    checkpoints += 1;
                }
                resized |= key == "resize";
                row.ops.push(op);
            }
            "flags" => row.alternative = value.contains("ALTERNATIVE"),
            "cite" | "note" => {}
            other => panic!("{id}: unknown key `{other}`"),
        }
    }
    assert!(row.size.0 > 0, "{id}: no size");
    assert!(!row.before.is_empty(), "{id}: no before block");
    assert_eq!(
        row.after.len(),
        checkpoints,
        "{id}: one after block per checkpoint"
    );
    row
}

fn parse_size(value: &str) -> (u16, u16) {
    let (rows, cols) = value.trim().split_once('x').expect("size is RxC");
    (rows.parse().expect("rows"), cols.parse().expect("cols"))
}

// ------------------------------------------------------------------ scripts

/// A script under `tests/scripts/reflow/`: one fixture row as operations.
///
/// - Blank lines and lines starting with `#` are skipped.
/// - `@size <rows>x<cols>` gives the grid size, once, first.
/// - `@scrollback <n>` sets the scrollback cap (optional, after `@size`).
/// - `@resize <rows>x<cols>` calls [`Grid::resize`].
/// - Every other line is a fixture step (`put abc`, `goto 0 7`, `save`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// Size before the operations.
    pub size: (u16, u16),
    /// Scrollback cap, if not the default.
    pub cap: Option<usize>,
    /// Operations in order.
    pub ops: Vec<Op>,
}

impl Script {
    /// Parses the script format.
    ///
    /// # Panics
    ///
    /// On a malformed line or a missing `@size`.
    pub fn parse(name: &str, text: &str) -> Self {
        let mut size = None;
        let mut cap = None;
        let mut ops = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(directive) = line.strip_prefix('@') {
                let (key, value) = directive
                    .split_once(' ')
                    .unwrap_or_else(|| panic!("{name}: malformed `{line}`"));
                match key {
                    "size" => {
                        assert!(
                            size.is_none() && ops.is_empty(),
                            "{name}: `@size` once, first"
                        );
                        size = Some(parse_size(value));
                    }
                    "scrollback" => cap = Some(value.parse().expect("scrollback cap")),
                    "resize" => ops.push(Op::parse(name, &format!("resize {value}"))),
                    other => panic!("{name}: unknown directive `@{other}`"),
                }
            } else {
                ops.push(Op::parse(name, line));
            }
        }
        Self {
            size: size.unwrap_or_else(|| panic!("{name}: no `@size`")),
            cap,
            ops,
        }
    }

    /// Reads and parses a script file.
    ///
    /// # Panics
    ///
    /// When the file cannot be read or does not parse.
    pub fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        Self::parse(&path.display().to_string(), &text)
    }

    /// The script text for a fixture row.
    pub fn text(&self, row: &Row) -> String {
        let mut out = format!(
            "# Fixture row {} (crates/tessera-grid/tests/fixtures/reflow_spec.txt).\n# {}\n",
            row.id, row.desc
        );
        let _ = writeln!(out, "@size {}x{}", self.size.0, self.size.1);
        if let Some(cap) = self.cap {
            let _ = writeln!(out, "@scrollback {cap}");
        }
        for op in &self.ops {
            out.push_str(&op.to_script());
            out.push('\n');
        }
        out
    }

    /// Runs the operations on a fresh grid and renders the state before
    /// the first resize and after every checkpoint, each under a heading.
    /// Calls `check` with the grid after every operation.
    pub fn run(&self, mut check: impl FnMut(&str, &Grid)) -> String {
        let mut grid = Grid::with_scrollback(
            self.size.0,
            self.size.1,
            self.cap.unwrap_or(tessera_grid::DEFAULT_MAX_SCROLLBACK),
        );
        let mut out = String::new();
        let mut resized = false;
        for op in &self.ops {
            let checkpoint = op.is_checkpoint(resized);
            if checkpoint && !resized {
                out.push_str("before:\n");
                out.push_str(&render_state(&grid));
            }
            op.apply(&mut grid);
            check(&op.to_script(), &grid);
            if checkpoint {
                let _ = writeln!(out, "{}:", op.to_script().trim_start_matches('@'));
                out.push_str(&render_state(&grid));
            }
            resized |= matches!(op, Op::Grid(Step::Resize(..)));
        }
        out
    }
}

/// Runs a fixture row and compares every block with the row's. Returns one
/// message per mismatch.
pub fn check_row(row: &Row) -> Vec<String> {
    let mut grid = row.grid();
    let mut errors = Vec::new();
    let mut resized = false;
    let mut afters = row.after.iter();
    for op in &row.ops {
        let checkpoint = op.is_checkpoint(resized);
        if checkpoint && !resized {
            let got = render_state(&grid);
            if got != row.before {
                errors.push(format!(
                    "{} before:\nexpected:\n{}got:\n{got}",
                    row.id, row.before
                ));
            }
        }
        op.apply(&mut grid);
        if checkpoint {
            let expected = afters.next().expect("an after block per checkpoint");
            let got = render_state(&grid);
            if &got != expected {
                errors.push(format!(
                    "{} after `{}`:\nexpected:\n{expected}got:\n{got}",
                    row.id,
                    op.to_script()
                ));
            }
        }
        resized |= matches!(op, Op::Grid(Step::Resize(..)));
    }
    errors
}

// ------------------------------------------------------------------ render

/// Renders the state a resize changes (module docs).
pub fn render_state(grid: &Grid) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "size: {}x{}  scrollback: {}",
        grid.rows(),
        grid.cols(),
        grid.scrollback_len()
    );
    for i in 0..grid.scrollback_len() {
        let line = grid.scrollback_line(i).expect("scrollback line");
        let _ = writeln!(out, "sb> {}", render_row(line));
    }
    for line in grid.visible_lines() {
        let _ = writeln!(out, "v>  {}", render_row(line));
    }
    let region = grid.scroll_region();
    let _ = writeln!(
        out,
        "{}  region: {}-{}",
        cursor_line(grid),
        region.top,
        region.bottom
    );
    if grid.is_alt_screen() {
        let mut primary = grid.clone();
        primary.leave_alt_screen();
        out.push_str("stashed_primary:\n");
        for line in primary.visible_lines() {
            let _ = writeln!(out, "p>  {}", render_row(line));
        }
        let _ = writeln!(out, "{}", cursor_line(&primary));
    }
    let tabs: Vec<String> = (0..grid.cols())
        .filter(|&col| grid.tabs().is_set(col))
        .map(|col| col.to_string())
        .collect();
    let _ = writeln!(out, "tabs: [{}]", tabs.join(", "));
    out
}

/// `cursor: (r,c)  pending_wrap: b  saved: (r,c,b)`.
fn cursor_line(grid: &Grid) -> String {
    let cursor = grid.cursor();
    let mut restored = grid.clone();
    restored.restore_cursor();
    let saved = restored.cursor();
    format!(
        "cursor: ({},{})  pending_wrap: {}  saved: ({},{},{})",
        cursor.row,
        cursor.col,
        grid.pending_wrap(),
        saved.row,
        saved.col,
        restored.pending_wrap()
    )
}

/// One row in the fixture's row syntax.
pub fn render_row(line: &Line) -> String {
    let mut out = String::from("[");
    for (col, cell) in line.cells.iter().enumerate() {
        if cell.is_spacer() {
            out.push('~');
        } else if cell.is_cluster() {
            let text = line.cluster(col).unwrap_or("?");
            let mut chars = text.chars();
            out.push('{');
            out.extend(chars.next());
            for ch in chars {
                let _ = write!(out, "+{:04X}", u32::from(ch));
            }
            out.push('}');
            if cell.is_wide() {
                out.push_str("/W");
            }
        } else if cell.ch == ' ' && cell.flags.is_empty() {
            out.push('.');
        } else if cell.ch == ' ' && cell.is_pad() {
            out.push('_');
        } else {
            out.push(cell.ch);
        }
        match cell.style.bg {
            Color::Default => {}
            Color::Indexed(n) => {
                let _ = write!(out, "@{n:x}");
            }
            Color::Rgb(r, g, b) => {
                let _ = write!(out, "@#{r:02X}{g:02X}{b:02X}");
            }
        }
    }
    out.push(']');
    if line.wrapped {
        out.push('+');
    }
    if !line.marks.is_empty() {
        let marks: Vec<String> = line.marks.iter().map(|m| format!("{m:?}")).collect();
        let _ = write!(out, " marks={}", marks.join(","));
    }
    out
}
