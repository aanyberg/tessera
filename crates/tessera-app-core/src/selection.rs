//! Text selection, its text, and link hit-testing over detected URLs
//! (ADR 0025).
//!
//! A session has at most one selection. The frontend drives it through
//! [`crate::App::select_begin`] (a click, double-click or triple-click),
//! [`crate::App::select_extend`] (a drag), [`crate::App::select_adjust`]
//! (shift-click), [`crate::App::select_all`] and
//! [`crate::App::select_clear`], reads it back with
//! [`crate::App::has_selection`] and [`crate::App::selection_text`] (Copy),
//! and every frame draws it. Points are view-relative cells
//! ([`Point`]: row 0 is the top row the view shows at its scroll offset);
//! the selection itself is kept in absolute logical-line numbers, so it
//! stays on its text while output scrolls the screen and while the view is
//! scrolled back.
//!
//! # Granularity
//!
//! - [`Granularity::Char`]: whole cells. A click alone selects nothing; the
//!   first extend selects from the clicked cell to the pointer's, both
//!   included.
//! - [`Granularity::Word`]: the run of word characters (Unicode
//!   alphanumerics and [`WORD_CHARS`]) or of whitespace under the click; any
//!   other character is selected alone; past the end of the line's text
//!   nothing is selected. Words continue across soft wraps.
//! - [`Granularity::Line`]: the whole logical line, every soft-wrapped row
//!   of it at full width.
//!
//! Dragging extends by the same unit and keeps the unit first clicked
//! whole. A block selection is the rectangle between the two points.
//!
//! # When it is cleared
//!
//! On the next look at the terminal (any of the calls above, and every
//! frame) the selection is dropped when a selected row on the live screen
//! no longer holds the text it held when the selection was made (cells,
//! clusters and the soft-wrap flag are compared, so a program repainting
//! the same text keeps it), when the history evicts or clears a selected
//! line, when the grid changes size, and when the screen switches to or
//! from the alternate screen. [`crate::App::clear_scrollback`] and
//! [`crate::App::resize`] to another size clear it at once. The comparison
//! looks only at the rows the grid reports changed since the last look,
//! and only while a selection exists.

use std::ops::Range;

pub use tessera_grid::Point;
use tessera_grid::{Cell, Color, DamageRows, Grid, Line};
use tessera_render::{Overlay, OverlayKind, Selection as Highlight};

use crate::url::{self, MAX_URL_LEN};

/// The characters besides Unicode alphanumerics that a double-click
/// selects as part of a word: iTerm2's default, so a path or a file name
/// with dashes and dots is one word. `:`, `=`, `@`, `?`, `&`, `%` and `#`
/// end a word.
pub const WORD_CHARS: &str = "/-+\\~_.";

/// What a click selects and a drag extends by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Granularity {
    /// Single cells (a click and drag).
    #[default]
    Char,
    /// Words (a double-click and drag).
    Word,
    /// Whole logical lines (a triple-click, or more, and drag).
    Line,
}

/// A URL detected in the terminal's text and the cells it covers, as
/// [`crate::App::link_at`] reports it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Link {
    /// The URL as written on the screen (its scheme's case kept).
    pub url: String,
    /// The cells the URL covers, one entry per row it is on that the view
    /// shows, top to bottom: the view-relative row and its columns (end
    /// exclusive; a wide character's two columns are both included).
    /// Trailing punctuation that is not part of the URL is outside it.
    pub span: Vec<(u16, Range<u16>)>,
}

/// A cell by absolute logical-line number: line `n` is the line that was
/// the screen's top row when `n` lines had entered the history
/// ([`Grid::scrollback_pushed`]), so a line keeps its number while it
/// scrolls up the screen and into the history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Pos {
    line: i64,
    col: u16,
}

/// A grid's lines by absolute number.
#[derive(Clone, Copy)]
struct Lines<'a> {
    grid: &'a Grid,
    /// The number of logical line 0 (the oldest history line).
    base: i64,
    /// The first selectable line: the oldest history line, or the top row
    /// on the alternate screen, which shows no history.
    first: i64,
    /// The number of the screen's top row.
    top: i64,
    rows: u16,
    cols: u16,
}

impl<'a> Lines<'a> {
    fn of(grid: &'a Grid) -> Self {
        let top = i64::try_from(grid.scrollback_pushed()).unwrap_or(i64::MAX);
        let len = i64::try_from(grid.scrollback_len()).unwrap_or(i64::MAX);
        let base = top.saturating_sub(len);
        Self {
            grid,
            base,
            first: if grid.is_alt_screen() { top } else { base },
            top,
            rows: grid.rows(),
            cols: grid.cols(),
        }
    }

    /// The bottom row's number.
    fn last(&self) -> i64 {
        self.top + i64::from(self.rows) - 1
    }

    /// Line `n`, if it is selectable.
    fn line(&self, n: i64) -> Option<&'a Line> {
        if n < self.first || n > self.last() {
            return None;
        }
        self.grid.logical_line(usize::try_from(n - self.base).ok()?)
    }

    /// The cell at `pos`.
    fn cell(&self, pos: Pos) -> Option<&'a Cell> {
        self.line(pos.line)?.cells.get(usize::from(pos.col))
    }

    /// The number of the view's top row at scroll `offset` (clamped as the
    /// view clamps it; always the screen on the alternate screen).
    fn view_top(&self, offset: usize) -> i64 {
        if self.grid.is_alt_screen() {
            return self.top;
        }
        let offset = i64::try_from(offset.min(self.grid.scrollback_len())).unwrap_or(0);
        self.top - offset
    }

    /// View-relative `at`, clamped to the grid, as an absolute cell.
    fn clamped(&self, offset: usize, at: Point) -> Pos {
        Pos {
            line: self.view_top(offset) + i64::from(at.row.min(self.rows - 1)),
            col: at.col.min(self.cols - 1),
        }
    }

    /// View-relative `at` as an absolute cell, `None` outside the grid.
    fn exact(&self, offset: usize, at: Point) -> Option<Pos> {
        (at.row < self.rows && at.col < self.cols).then(|| Pos {
            line: self.view_top(offset) + i64::from(at.row),
            col: at.col,
        })
    }

    /// The character at `pos`: the cell itself, or the wide character a
    /// spacer belongs to; `None` on padding and outside the grid.
    fn char_at(&self, pos: Pos) -> Option<Pos> {
        let cell = self.cell(pos)?;
        if cell.is_pad() {
            return None;
        }
        if cell.is_spacer() && pos.col > 0 {
            return Some(Pos {
                col: pos.col - 1,
                ..pos
            });
        }
        Some(pos)
    }

    /// The character after the one at `pos` on its logical line (soft
    /// wraps continue it), skipping spacers and padding.
    fn next_char(&self, pos: Pos) -> Option<Pos> {
        let mut pos = Pos {
            col: pos.col + 1,
            ..pos
        };
        loop {
            if pos.col >= self.cols {
                if !self.line(pos.line)?.wrapped {
                    return None;
                }
                pos = Pos {
                    line: pos.line + 1,
                    col: 0,
                };
            }
            let cell = self.cell(pos)?;
            if !cell.is_spacer() && !cell.is_pad() {
                return Some(pos);
            }
            pos.col += 1;
        }
    }

    /// The character before the one at `pos` on its logical line.
    fn prev_char(&self, pos: Pos) -> Option<Pos> {
        let mut pos = pos;
        loop {
            if pos.col == 0 {
                let above = self.line(pos.line - 1)?;
                if !above.wrapped {
                    return None;
                }
                pos = Pos {
                    line: pos.line - 1,
                    col: self.cols,
                };
            }
            pos.col -= 1;
            let cell = self.cell(pos)?;
            if !cell.is_spacer() && !cell.is_pad() {
                return Some(pos);
            }
        }
    }

    /// The last cell of the character at `pos` (its spacer, if wide).
    fn char_end(&self, pos: Pos) -> Pos {
        match self.cell(pos) {
            Some(cell) if cell.is_wide() && pos.col + 1 < self.cols => Pos {
                col: pos.col + 1,
                ..pos
            },
            _ => pos,
        }
    }

    /// The number of the logical line's first row for the row `n` is on.
    fn logical_top(&self, n: i64) -> i64 {
        let mut top = n;
        while top > self.first && self.line(top - 1).is_some_and(|l| l.wrapped) {
            top -= 1;
        }
        top
    }

    /// The number of the logical line's last row for the row `n` is on.
    fn logical_bottom(&self, n: i64) -> i64 {
        let mut bottom = n;
        while bottom < self.last() && self.line(bottom).is_some_and(|l| l.wrapped) {
            bottom += 1;
        }
        bottom
    }

    /// The last row of row `n`'s logical line and its last cell with text
    /// at row `n` or below it (`None`: only blanks from row `n` on).
    fn text_end(&self, n: i64) -> (i64, Option<Pos>) {
        let bottom = self.logical_bottom(n);
        let last = (n..=bottom).rev().find_map(|line| {
            let cells = &self.line(line)?.cells;
            let col = cells.iter().rposition(|c| !is_blank(c))?;
            Some(Pos {
                line,
                col: u16::try_from(col).unwrap_or(u16::MAX),
            })
        });
        (bottom, last)
    }
}

/// A cell that holds no text: a space with no marker but padding (styles
/// do not count, so an erased coloured cell is blank too).
fn is_blank(cell: &Cell) -> bool {
    cell.ch == ' '
        && cell
            .flags
            .difference(tessera_grid::CellFlags::PAD)
            .is_empty()
}

/// Whether two rows hold the same cells, compared field by field.
///
/// Not with `==`: `Cell`'s and `Style`'s derived comparisons are what
/// `Snapshot::sync_row` runs on every frame, and calling them from here as
/// well made the compiler stop inlining `Style::eq` there, costing frames
/// without a selection about 15 % more cycles (ADR 0025).
fn same_cells(a: &[Cell], b: &[Cell]) -> bool {
    fn same_color(left: Color, right: Color) -> bool {
        match (left, right) {
            (Color::Default, Color::Default) => true,
            (Color::Indexed(one), Color::Indexed(other)) => one == other,
            (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => (r1, g1, b1) == (r2, g2, b2),
            _ => false,
        }
    }
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.ch == y.ch
                && x.flags.bits() == y.flags.bits()
                && x.style.flags == y.style.flags
                && same_color(x.style.fg, y.style.fg)
                && same_color(x.style.bg, y.style.bg)
                && same_color(x.style.underline, y.style.underline)
        })
}

/// What a double-click groups a character with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn class_of(cell: &Cell) -> Class {
    if is_blank(cell) {
        Class::Space
    } else if cell.ch.is_alphanumeric() || WORD_CHARS.contains(cell.ch) {
        Class::Word
    } else {
        Class::Other
    }
}

/// The cells the word, whitespace run or single other character at `pos`
/// covers; `None` past the end of its logical line's text and on padding.
fn word_at(lines: &Lines<'_>, pos: Pos) -> Option<(Pos, Pos)> {
    let at = lines.char_at(pos)?;
    let (_, last) = lines.text_end(at.line);
    if last.is_none_or(|last| at > last) {
        return None;
    }
    let class = class_of(lines.cell(at)?);
    if class == Class::Other {
        return Some((at, lines.char_end(at)));
    }
    let same = |p: &Pos| lines.cell(*p).is_some_and(|c| class_of(c) == class);
    let mut start = at;
    while let Some(p) = lines.prev_char(start).filter(same) {
        start = p;
    }
    let mut end = at;
    while let Some(p) = lines.next_char(end).filter(same) {
        end = p;
    }
    Some((start, lines.char_end(end)))
}

/// The cells of the whole logical line `pos` is on, every row at full
/// width.
fn line_at(lines: &Lines<'_>, pos: Pos) -> (Pos, Pos) {
    (
        Pos {
            line: lines.logical_top(pos.line),
            col: 0,
        },
        Pos {
            line: lines.logical_bottom(pos.line),
            col: lines.cols - 1,
        },
    )
}

/// The unit `granularity` selects at `pos`; `None` for a word past the
/// end of the text.
fn unit_at(lines: &Lines<'_>, pos: Pos, granularity: Granularity) -> Option<(Pos, Pos)> {
    match granularity {
        Granularity::Char => Some((pos, pos)),
        Granularity::Word => word_at(lines, pos),
        Granularity::Line => Some(line_at(lines, pos)),
    }
}

/// What a selection was taken against, to tell when it must be cleared.
#[derive(Debug, Clone, Default)]
struct Guard {
    rows: u16,
    cols: u16,
    alt: bool,
    /// The grid's damage generation at the last look.
    generation: u64,
    /// [`Grid::scrollback_pushed`] and [`Grid::scrollback_len`] at the last
    /// look: when either moved, every guarded row is compared.
    pushed: u64,
    len: usize,
    /// The number of `lines[0]`.
    first: i64,
    /// The selected rows that were on the live screen when the selection
    /// was made, as they were then.
    lines: Vec<Line>,
    /// The view's top line at the last frame that checked the guard, if
    /// it is unchanged since: every guarded row that view showed then
    /// held the guarded text, and so did the frame's snapshot, which is an
    /// exact copy of the rows in view after its capture.
    synced: Option<i64>,
}

impl Guard {
    /// Takes the guard of a selection over lines `start..=end` of `lines`,
    /// reusing `self`'s buffers.
    fn retake(&mut self, lines: &Lines<'_>, start: i64, end: i64) {
        let grid = lines.grid;
        self.rows = lines.rows;
        self.cols = lines.cols;
        self.alt = grid.is_alt_screen();
        self.generation = grid.damage_generation();
        self.pushed = grid.scrollback_pushed();
        self.len = grid.scrollback_len();
        self.first = start.max(lines.top);
        self.synced = None;
        let last = end.min(lines.last());
        let count = usize::try_from(last - self.first + 1).unwrap_or(0);
        self.lines.truncate(count);
        for (i, n) in (self.first..=last).enumerate() {
            let Some(line) = lines.line(n) else { break };
            match self.lines.get_mut(i) {
                Some(kept) => kept.clone_from(line),
                None => self.lines.push(line.clone()),
            }
        }
    }

    /// Whether the selection over `start..` survives the grid as it is
    /// now; updates the guard's view of the grid either way.
    ///
    /// Compares the guarded rows the grid reports changed since the last
    /// look ([`Grid::damage_since`], every row when the history moved).
    /// With `frame`, the rows a view at `offset` just captured and the
    /// rows whose content that capture found changed: when the previous
    /// frame checked the guard with the view on the same lines, a guarded
    /// row in view that the capture found unchanged still holds the text,
    /// so only the changed ones are compared (a cursor moving over a
    /// selection compares nothing but each row's soft-wrap flag, which the
    /// snapshot does not hold). Rows outside the view use the damage. When
    /// the history moved or the whole screen changed, the guarded lines in
    /// the history are compared as well: a row rewritten and then scrolled
    /// off between two looks changed while it was live, and a whole-screen
    /// erase ends the newest history line.
    fn holds(
        &mut self,
        lines: &Lines<'_>,
        start: i64,
        frame: Option<(usize, &DamageRows)>,
    ) -> bool {
        let grid = lines.grid;
        if (lines.rows, lines.cols, grid.is_alt_screen()) != (self.rows, self.cols, self.alt)
            || start < lines.first
        {
            return false;
        }
        let (pushed, len) = (grid.scrollback_pushed(), grid.scrollback_len());
        let moved = (pushed, len) != (self.pushed, self.len);
        let damage = if moved {
            DamageRows::All
        } else {
            grid.damage_since(self.generation)
        };
        self.generation = grid.damage_generation();
        self.pushed = pushed;
        self.len = len;
        let view = frame.map(|(offset, changed)| (lines.view_top(offset), changed));
        let synced = view.filter(|(top, _)| self.synced == Some(*top));
        let rows = i64::from(lines.rows);
        // Guarded rows that scrolled into the history since the last look
        // may have changed on the screen first, and erasing the whole
        // screen (ED 2, RIS) ends the newest history line (clears its
        // wrap flag): when the history moved or the whole screen changed,
        // compare the guarded lines in the history too (at most a
        // screen's worth).
        let same = |line: &Line, kept: &Line| {
            line.wrapped == kept.wrapped
                && same_cells(&line.cells, &kept.cells)
                && line.graphemes == kept.graphemes
        };
        let history_holds = !matches!(damage, DamageRows::All)
            || self.lines.iter().zip(self.first..).all(|(kept, n)| {
                n >= lines.top || lines.line(n).is_some_and(|line| same(line, kept))
            });
        let holds = history_holds
            && (0..lines.rows).all(|row| {
                let n = lines.top + i64::from(row);
                let Some(kept) = usize::try_from(n - self.first)
                    .ok()
                    .and_then(|i| self.lines.get(i))
                else {
                    return true;
                };
                let check = match synced {
                    Some((top, changed)) if (top..top + rows).contains(&n) => {
                        changed.contains(u16::try_from(n - top).unwrap_or(u16::MAX))
                    }
                    _ => damage.contains(row),
                };
                let Some(line) = grid.line(row) else {
                    return false;
                };
                // The snapshot holds no soft-wrap flags, so the capture cannot
                // report a change of one: that flag is compared on every row.
                line.wrapped == kept.wrapped && (!check || same(line, kept))
            });
        if holds {
            self.synced = view.map(|(top, _)| top).or(self.synced);
        }
        holds
    }
}

/// The selection itself.
#[derive(Debug, Clone)]
struct Sel {
    /// The unit first clicked (or, after an adjust, the end that stayed):
    /// a drag selects from it to the unit under the pointer.
    anchor: (Pos, Pos),
    /// The selected cells, both included: the first and last in reading
    /// order, or the top-left and bottom-right corners of a block.
    start: Pos,
    end: Pos,
    granularity: Granularity,
    block: bool,
    /// False after a character click with no drag yet, and after a
    /// double-click past the text: nothing is selected.
    active: bool,
    guard: Guard,
}

impl Sel {
    /// Selects from the anchor to `unit`.
    fn span_to(&mut self, unit: (Pos, Pos)) {
        let points = [self.anchor.0, self.anchor.1, unit.0, unit.1];
        if self.block {
            let line = |f: fn(i64, i64) -> i64| points.iter().map(|p| p.line).reduce(f);
            let col = |f: fn(u16, u16) -> u16| points.iter().map(|p| p.col).reduce(f);
            self.start = Pos {
                line: line(i64::min).unwrap_or_default(),
                col: col(u16::min).unwrap_or_default(),
            };
            self.end = Pos {
                line: line(i64::max).unwrap_or_default(),
                col: col(u16::max).unwrap_or_default(),
            };
        } else {
            self.start = points.iter().copied().min().unwrap_or(unit.0);
            self.end = points.iter().copied().max().unwrap_or(unit.1);
        }
        self.active = true;
    }
}

/// What a frame draws besides the cells: the selection, clipped to the
/// rows the view shows, and the hovered link's underlines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Decorations {
    pub(crate) selection: Option<Highlight>,
    pub(crate) overlays: Vec<Overlay>,
}

/// A session's selection and link hover. Lives under its own lock in the
/// session, taken before the terminal lock (and after the view table's).
#[derive(Debug, Default)]
pub(crate) struct SelectionState {
    sel: Option<Sel>,
    /// The cell the pointer hovers with the link modifier held,
    /// view-relative.
    hover: Option<Point>,
}

impl SelectionState {
    /// Drops the selection if the grid no longer holds what it selected;
    /// `frame` as for [`Guard::holds`].
    fn validate(&mut self, grid: &Grid, frame: Option<(usize, &DamageRows)>) {
        if let Some(sel) = &mut self.sel {
            let lines = Lines::of(grid);
            let start = sel.start.min(sel.anchor.0).line;
            if !sel.guard.holds(&lines, start, frame) {
                self.sel = None;
            }
        }
    }

    /// A click at `at` (view-relative at scroll `offset`): starts a new
    /// selection of `granularity`, a block one if `block`. A character
    /// click selects nothing until [`Self::extend`]; a word click past the
    /// end of the text selects nothing either.
    pub(crate) fn begin(
        &mut self,
        grid: &Grid,
        offset: usize,
        at: Point,
        granularity: Granularity,
        block: bool,
    ) {
        let lines = Lines::of(grid);
        let pos = lines.clamped(offset, at);
        let unit = match granularity {
            Granularity::Char => None,
            _ => unit_at(&lines, pos, granularity),
        };
        let mut guard = self.sel.take().map(|sel| sel.guard).unwrap_or_default();
        let (start, end) = unit.unwrap_or((pos, pos));
        guard.retake(&lines, start.line, end.line);
        self.sel = Some(Sel {
            anchor: (start, end),
            start,
            end,
            granularity,
            block,
            active: unit.is_some(),
            guard,
        });
    }

    /// A drag to `at`: selects from the anchor to the unit under `at`.
    /// Nothing without a selection begun.
    pub(crate) fn extend(&mut self, grid: &Grid, offset: usize, at: Point) {
        self.validate(grid, None);
        let Some(sel) = &mut self.sel else { return };
        let lines = Lines::of(grid);
        let pos = lines.clamped(offset, at);
        let unit = unit_at(&lines, pos, sel.granularity).unwrap_or((pos, pos));
        sel.span_to(unit);
        sel.guard.retake(&lines, sel.start.line, sel.end.line);
    }

    /// A shift-click at `at`: moves the selection's end nearer to `at`
    /// (measured in cells along the lines; a tie moves the start) to the
    /// unit there, keeping the granularity and block-ness. Nothing without
    /// a selection.
    pub(crate) fn adjust(&mut self, grid: &Grid, offset: usize, at: Point) {
        self.validate(grid, None);
        let Some(sel) = self.sel.as_mut().filter(|sel| sel.active) else {
            return;
        };
        let lines = Lines::of(grid);
        let pos = lines.clamped(offset, at);
        let unit = unit_at(&lines, pos, sel.granularity).unwrap_or((pos, pos));
        let cols = i128::from(lines.cols);
        let distance = |a: Pos, b: Pos| {
            ((i128::from(a.line) - i128::from(b.line)) * cols + i128::from(a.col)
                - i128::from(b.col))
            .abs()
        };
        let move_end = if pos > sel.end {
            true
        } else if pos < sel.start {
            false
        } else {
            distance(pos, sel.end) < distance(pos, sel.start)
        };
        let unit_of = |p: Pos| unit_at(&lines, p, sel.granularity).unwrap_or((p, p));
        let (start, end) = (sel.start, sel.end);
        // The end that stays becomes the anchor a drag continues from.
        if move_end {
            sel.end = unit.1;
            sel.anchor = (start, unit_of(start).1.min(sel.end));
        } else {
            sel.start = unit.0;
            sel.anchor = (unit_of(end).0.max(sel.start), end);
        }
        if sel.block {
            let (a, b) = (sel.start, sel.end);
            sel.start = Pos {
                line: a.line.min(b.line),
                col: a.col.min(b.col),
            };
            sel.end = Pos {
                line: a.line.max(b.line),
                col: a.col.max(b.col),
            };
        }
        sel.guard.retake(&lines, sel.start.line, sel.end.line);
    }

    /// Selects every line: the history and the screen, or only the screen
    /// on the alternate screen.
    pub(crate) fn select_all(&mut self, grid: &Grid) {
        let lines = Lines::of(grid);
        let start = Pos {
            line: lines.first,
            col: 0,
        };
        let end = Pos {
            line: lines.last(),
            col: lines.cols - 1,
        };
        let mut guard = self.sel.take().map(|sel| sel.guard).unwrap_or_default();
        guard.retake(&lines, start.line, end.line);
        self.sel = Some(Sel {
            anchor: (start, end),
            start,
            end,
            granularity: Granularity::Char,
            block: false,
            active: true,
            guard,
        });
    }

    /// Drops the selection.
    pub(crate) fn clear(&mut self) {
        self.sel = None;
    }

    /// Whether something is selected (it may be blank, or outside the
    /// view).
    pub(crate) fn has_selection(&mut self, grid: &Grid) -> bool {
        self.validate(grid, None);
        self.sel.as_ref().is_some_and(|sel| sel.active)
    }

    /// The selected text, or `None` without a selection; see
    /// [`crate::App::selection_text`] for the rules.
    pub(crate) fn text(&mut self, grid: &Grid) -> Option<String> {
        self.validate(grid, None);
        let sel = self.sel.as_ref().filter(|sel| sel.active)?;
        Some(extract(&Lines::of(grid), sel))
    }

    /// Stores the hovered cell (view-relative), or none.
    pub(crate) fn set_hover(&mut self, at: Option<Point>) {
        self.hover = at;
    }

    /// What the next frame of the view at scroll `offset` draws: the
    /// selection clipped to its rows, and one underline per row of the
    /// hovered link. Frames use [`Self::frame_decorations`]; this is for
    /// looks outside a frame (tests).
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn decorations(&mut self, grid: &Grid, offset: usize) -> Decorations {
        self.validate(grid, None);
        self.draw(grid, offset)
    }

    /// [`Self::decorations`] for a frame whose snapshot just captured the
    /// rows of the view at `offset`, finding the rows in `changed` changed
    /// ([`crate::view::View::capture`]): the selection is checked only
    /// against those, and against the damage outside the view.
    pub(crate) fn frame_decorations(
        &mut self,
        grid: &Grid,
        offset: usize,
        changed: &DamageRows,
    ) -> Decorations {
        self.validate(grid, Some((offset, changed)));
        self.draw(grid, offset)
    }

    /// The decorations of the selection as it is, unchecked.
    fn draw(&self, grid: &Grid, offset: usize) -> Decorations {
        let lines = Lines::of(grid);
        let selection = self
            .sel
            .as_ref()
            .filter(|sel| sel.active)
            .and_then(|sel| clip(&lines, offset, sel));
        let overlays = self
            .hover
            .and_then(|at| link_at(grid, offset, at))
            .map(|link| {
                link.span
                    .into_iter()
                    .map(|(row, cols)| Overlay::new(OverlayKind::Underline, row, cols))
                    .collect()
            })
            .unwrap_or_default();
        Decorations {
            selection,
            overlays,
        }
    }
}

/// `sel` in the rows of the view at scroll `offset`, clipped to them;
/// `None` when it is wholly outside.
fn clip(lines: &Lines<'_>, offset: usize, sel: &Sel) -> Option<Highlight> {
    let top = lines.view_top(offset);
    let bottom = top + i64::from(lines.rows) - 1;
    if sel.end.line < top || sel.start.line > bottom {
        return None;
    }
    let row = |line: i64| u16::try_from(line - top).unwrap_or(0);
    if sel.block {
        let start = Point::new(row(sel.start.line.max(top)), sel.start.col);
        let end = Point::new(row(sel.end.line.min(bottom)), sel.end.col);
        return Some(Highlight::block(start, end));
    }
    let start = if sel.start.line < top {
        Point::new(0, 0)
    } else {
        Point::new(row(sel.start.line), sel.start.col)
    };
    let end = if sel.end.line > bottom {
        Point::new(lines.rows - 1, lines.cols - 1)
    } else {
        Point::new(row(sel.end.line), sel.end.col)
    };
    Some(Highlight::linear(start, end))
}

/// The text of `sel`: soft-wrapped rows joined, a newline at every hard
/// line end (every row of a block), the blanks after the last text of a
/// logical line dropped, spacers and padding skipped, clusters whole, and
/// the newlines of trailing blank rows dropped.
fn extract(lines: &Lines<'_>, sel: &Sel) -> String {
    let mut out = String::new();
    // The current logical line's last row and last cell with text.
    let mut logical: Option<(i64, Option<Pos>)> = None;
    for n in sel.start.line..=sel.end.line {
        let Some(line) = lines.line(n) else { break };
        let (first, last) = if sel.block {
            (sel.start.col, sel.end.col)
        } else {
            (
                if n == sel.start.line {
                    sel.start.col
                } else {
                    0
                },
                if n == sel.end.line {
                    sel.end.col
                } else {
                    lines.cols - 1
                },
            )
        };
        if logical.is_none_or(|(bottom, _)| n > bottom) {
            logical = Some(lines.text_end(n));
        }
        let text_end = logical.and_then(|(_, end)| end);
        let mut first = usize::from(first);
        if first > 0 && line.cells.get(first).is_some_and(Cell::is_spacer) {
            first -= 1;
        }
        let last = usize::from(last).min(line.cells.len().saturating_sub(1));
        for col in first..=last {
            let pos = Pos {
                line: n,
                col: u16::try_from(col).unwrap_or(u16::MAX),
            };
            if text_end.is_none_or(|end| pos > end) {
                break;
            }
            let cell = &line.cells[col];
            if cell.is_spacer() || cell.is_pad() {
                continue;
            }
            match line.cluster(col) {
                Some(text) if cell.is_cluster() => out.push_str(text),
                _ => out.push(cell.ch),
            }
        }
        if n < sel.end.line && (sel.block || !line.wrapped) {
            out.push('\n');
        }
    }
    let kept = out.trim_end_matches('\n').len();
    out.truncate(kept);
    out
}

/// The characters of the logical line around `at`, one per character
/// cell (a cluster's base scalar), with the cell each starts at: at most
/// `reach` characters to either side of `at`'s, and `at`'s index in
/// them. `None` on padding and outside the grid.
fn window(lines: &Lines<'_>, at: Pos, reach: usize) -> Option<(Vec<char>, Vec<Pos>, usize)> {
    let at = lines.char_at(at)?;
    let mut before = Vec::new();
    let mut pos = at;
    while before.len() < reach {
        let Some(prev) = lines.prev_char(pos) else {
            break;
        };
        before.push(prev);
        pos = prev;
    }
    before.reverse();
    let index = before.len();
    let mut cells = before;
    cells.push(at);
    let mut pos = at;
    for _ in 0..reach {
        let Some(next) = lines.next_char(pos) else {
            break;
        };
        cells.push(next);
        pos = next;
    }
    let chars = cells
        .iter()
        .map(|&p| lines.cell(p).map_or(' ', |c| c.ch))
        .collect();
    Some((chars, cells, index))
}

/// The detected URL under view-relative `at` at scroll `offset` (see
/// [`crate::App::link_at`]).
pub(crate) fn link_at(grid: &Grid, offset: usize, at: Point) -> Option<Link> {
    link_with_reach(grid, offset, at, MAX_URL_LEN + 1)
}

/// [`link_at`], looking `reach` characters to either side: the cap
/// (U9) makes [`MAX_URL_LEN`] + 1 enough to give what the whole logical
/// line would.
fn link_with_reach(grid: &Grid, offset: usize, at: Point, reach: usize) -> Option<Link> {
    let lines = Lines::of(grid);
    let pos = lines.exact(offset, at)?;
    let (chars, cells, index) = window(&lines, pos, reach)?;
    let span = url::url_at(&chars, index)?;
    let mut text = String::new();
    let top = lines.view_top(offset);
    let mut rows: Vec<(u16, Range<u16>)> = Vec::new();
    for &p in &cells[span] {
        let line = lines.line(p.line)?;
        match line.cluster(usize::from(p.col)) {
            Some(cluster) if lines.cell(p).is_some_and(Cell::is_cluster) => text.push_str(cluster),
            _ => text.push(lines.cell(p)?.ch),
        }
        let row = p.line - top;
        let Ok(row) = u16::try_from(row) else {
            continue;
        };
        if row >= lines.rows {
            continue;
        }
        let end = lines.char_end(p).col + 1;
        match rows.last_mut() {
            Some((last, cols)) if *last == row => cols.end = end,
            _ => rows.push((row, p.col..end)),
        }
    }
    Some(Link {
        url: text,
        span: rows,
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use tessera_term::Terminal;

    use super::*;

    fn term(rows: u16, cols: u16, bytes: &[u8]) -> Terminal {
        let mut term = Terminal::new(rows, cols);
        term.advance(bytes);
        term
    }

    fn point(row: u16, col: u16) -> Point {
        Point::new(row, col)
    }

    #[test]
    fn a_selection_survives_identical_repaints_and_clears_on_change() {
        let mut term = term(3, 10, b"hello\r\nworld");
        let mut state = SelectionState::default();
        state.begin(term.grid(), 0, point(0, 0), Granularity::Char, false);
        state.extend(term.grid(), 0, point(0, 4));
        // A full repaint of the same text (ED 2 + rewrite) keeps it.
        term.advance(b"\x1b[H\x1b[2Jhello\r\nworld");
        assert!(state.has_selection(term.grid()));
        // A change on an unselected row keeps it too.
        term.advance(b"\x1b[2;1HWORLD");
        assert!(state.has_selection(term.grid()));
        // A change on the selected row, outside the selected cells, clears.
        term.advance(b"\x1b[1;9Hx");
        assert!(!state.has_selection(term.grid()));
    }

    #[test]
    fn eviction_at_the_default_cap_clears() {
        let cap = tessera_grid::DEFAULT_MAX_SCROLLBACK;
        let mut term = term(2, 5, b"");
        for i in 0..=cap {
            term.advance(format!("{}\r\n", i % 10).as_bytes());
        }
        let mut state = SelectionState::default();
        // The oldest history line, selected while scrolled to the top.
        let offset = term.grid().scrollback_len();
        state.begin(term.grid(), offset, point(0, 0), Granularity::Line, false);
        assert!(state.has_selection(term.grid()));
        term.advance(b"x\r\n");
        assert!(!state.has_selection(term.grid()));
    }

    #[test]
    fn validation_compares_only_damaged_rows_until_the_history_moves() {
        let mut term = term(4, 10, b"a\r\nb\r\nc\r\nd");
        let mut state = SelectionState::default();
        state.select_all(term.grid());
        // Cursor motion on the bottom row: no change, kept.
        term.advance(b"\x1b[4;5H");
        assert!(state.has_selection(term.grid()));
        // A line feed at the bottom pushes `a` into history: every row is
        // compared by its line number, and none changed.
        term.advance(b"\r\n");
        assert!(state.has_selection(term.grid()));
        assert_eq!(state.text(term.grid()).as_deref(), Some("a\nb\nc\nd"));
    }

    #[test]
    fn switching_to_the_alternate_screen_clears() {
        let mut term = term(3, 10, b"hello");
        let mut state = SelectionState::default();
        state.begin(term.grid(), 0, point(0, 0), Granularity::Word, false);
        term.advance(b"\x1b[?1049h");
        assert!(!state.has_selection(term.grid()));
    }

    /// The reference link lookup: the whole logical line, no reach limit.
    fn reference_link(grid: &Grid, at: Point) -> Option<Link> {
        link_with_reach(grid, 0, at, usize::MAX)
    }

    #[test]
    fn a_hover_lookup_reads_a_bounded_window_of_a_huge_line() {
        // One logical line of 2 000 rows of URL characters (no newline):
        // the lookup walks at most the cap to either side.
        let mut term = Terminal::new(2_000, 200);
        term.advance(&vec![b'a'; 2_000 * 200 - 1]);
        let lines = Lines::of(term.grid());
        let at = Pos {
            line: lines.top + 1_000,
            col: 100,
        };
        let (chars, _, index) = window(&lines, at, MAX_URL_LEN + 1).expect("a character");
        assert_eq!(chars.len(), 2 * (MAX_URL_LEN + 1) + 1);
        assert_eq!(index, MAX_URL_LEN + 1);
        assert_eq!(link_at(term.grid(), 0, point(1_000, 100)), None);
    }

    #[test]
    fn the_reach_window_agrees_with_the_whole_line_on_a_long_wrapped_line() {
        let mut bytes = Vec::new();
        bytes.extend(std::iter::repeat_n(b'/', 3000));
        bytes.extend(b"https://a.bc/x ");
        bytes.extend(std::iter::repeat_n(b'a', 2000));
        bytes.extend(b" https://d.ef/");
        let term = term(120, 50, &bytes);
        for row in 0..110 {
            for col in [0, 7, 25, 49] {
                assert_eq!(
                    link_at(term.grid(), 0, point(row, col)),
                    reference_link(term.grid(), point(row, col)),
                    "row {row} col {col}"
                );
            }
        }
    }

    /// An App with a fake session of `rows` x `cols` and an offscreen
    /// view attached the way `App::attach_view` attaches one; `None` when
    /// GPU tests are skipped.
    fn app_with_view(rows: u16, cols: u16) -> Option<(crate::App, crate::SessionId)> {
        if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
            return None;
        }
        let app = crate::App::new(
            crate::Config::default(),
            crate::ConfigPaths::new("/nonexistent/tessera"),
        );
        let id = app
            .spawn_test_session(crate::PtySize::new(rows, cols))
            .expect("session");
        let theme = app.view_theme();
        let gpu = tessera_render::Gpu::headless().expect("a Metal adapter");
        let fonts = app.fonts_for(&theme).expect("the configured font");
        let view = crate::view::View::offscreen(
            gpu,
            fonts,
            theme,
            tessera_render::PhysicalSize::new(400, 300),
            1.0,
        )
        .expect("view");
        crate::session::lock(&app.views).insert(id, view);
        // The view would resize the session; keep the fixture's size.
        app.resize(id, crate::PtySize::new(rows, cols))
            .expect("resize");
        Some((app, id))
    }

    fn shown(app: &crate::App, id: crate::SessionId) -> Decorations {
        crate::session::lock(&app.views)[&id]
            .snapshot()
            .decorations()
            .clone()
    }

    #[test]
    fn frames_carry_the_selection_and_hover_and_skip_when_unchanged() {
        use crate::view::{FrameStatus, MIN_FRAME_INTERVAL};
        let Some((app, id)) = app_with_view(4, 30) else {
            return;
        };
        let next = || std::thread::sleep(MIN_FRAME_INTERVAL + std::time::Duration::from_millis(1));
        app.feed_for_test(id, b"see https://a.bc/x ok").unwrap();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Skipped);
        // A selection is drawn, and an unchanged one is skipped.
        app.select_begin(id, point(0, 0), Granularity::Word, false)
            .unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        let selection = Highlight::linear(point(0, 0), point(0, 2));
        assert_eq!(shown(&app, id).selection, Some(selection));
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Skipped);
        // A hover over the URL underlines it (trailing text outside).
        app.set_hover(id, Some(point(0, 8))).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        assert_eq!(
            shown(&app, id).overlays,
            vec![Overlay::new(OverlayKind::Underline, 0, 4..18)]
        );
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Skipped);
        // Moving the hover within the same link draws nothing new.
        app.set_hover(id, Some(point(0, 10))).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Skipped);
        // Off the link, and clearing the selection, each draw a frame.
        app.set_hover(id, Some(point(0, 1))).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        assert_eq!(shown(&app, id).overlays, Vec::new());
        app.select_clear(id).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        assert_eq!(shown(&app, id), Decorations::default());
        // The frame clips a selection to the rows in view: scrolled back
        // over history, the selection on the live screen is outside it.
        for _ in 0..6 {
            app.feed_for_test(id, b"\r\nline").unwrap();
        }
        app.select_begin(id, point(1, 0), Granularity::Line, false)
            .unwrap();
        app.scroll_to(id, crate::ScrollTarget::Offset(4)).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        assert_eq!(shown(&app, id).selection, None);
        assert!(app.has_selection(id).unwrap());
        app.scroll_to(id, crate::ScrollTarget::Offset(2)).unwrap();
        next();
        assert_eq!(app.render_view(id).unwrap(), FrameStatus::Presented);
        assert_eq!(
            shown(&app, id).selection,
            Some(Highlight::linear(point(3, 0), point(3, 29)))
        );
    }

    /// One step of the differential test below.
    #[derive(Debug, Clone)]
    enum Op {
        /// Output.
        Feed(String),
        /// A frame of the view at this scroll offset.
        Frame(usize),
        /// A look outside a frame (`has_selection`).
        Look,
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => prop_oneof![
                "[a-c ]{1,3}",
                Just("\r\n".to_owned()),
                (1u16..6, 1u16..9).prop_map(|(r, c)| format!("\x1b[{r};{c}H")),
                Just("\x1b[1T".to_owned()),
                Just("\x1b[2;4r\x1b[4H\n\x1b[r".to_owned()),
                Just("\x1b[H\x1b[2J".to_owned()),
                Just("\x1b[3J".to_owned()),
                // A rewrite of the top row, then lines that scroll it off.
                Just("\x1b[1;1Hzz\x1b[5;1H\n\n".to_owned()),
            ]
            .prop_map(Op::Feed),
            2 => (0usize..4).prop_map(Op::Frame),
            1 => Just(Op::Look),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        /// The frame path (only rows the capture found changed, damage
        /// outside the view) and the look path (damage) clear the selection
        /// exactly when a full comparison of every guarded row would.
        #[test]
        fn validation_agrees_with_a_full_comparison(
            setup in "[a-c \r\n]{0,40}",
            from in (0u16..5, 0u16..8),
            to in (0u16..5, 0u16..8),
            offset in 0usize..3,
            ops in proptest::collection::vec(op(), 1..30),
        ) {
            let mut term = Terminal::new(5, 8);
            term.advance(setup.as_bytes());
            let mut state = SelectionState::default();
            let grid = term.grid();
            state.begin(grid, offset, point(from.0, from.1), Granularity::Char, false);
            state.extend(grid, offset, point(to.0, to.1));
            let sel = state.sel.clone().expect("selected");
            // The reference: the selected live rows as they were, by line.
            let reference: Vec<(i64, Line)> = {
                let lines = Lines::of(grid);
                (sel.start.line..=sel.end.line)
                    .filter(|&n| n >= lines.top)
                    .filter_map(|n| Some((n, lines.line(n)?.clone())))
                    .collect()
            };
            let start = sel.start.line;
            let mut snapshot = crate::view::Snapshot::default();
            let mut alive = true;
            for op in ops {
                let checked = match op {
                    Op::Feed(bytes) => {
                        term.advance(bytes.as_bytes());
                        false
                    }
                    Op::Frame(offset) => {
                        let changed = snapshot.capture(&term, offset);
                        state.frame_decorations(term.grid(), offset, &changed);
                        true
                    }
                    Op::Look => {
                        state.has_selection(term.grid());
                        true
                    }
                };
                if !checked {
                    continue;
                }
                let lines = Lines::of(term.grid());
                alive = alive
                    && start >= lines.first
                    && reference.iter().all(|(n, line)| {
                        lines.line(*n).is_some_and(|now| {
                                now.cells == line.cells
                                    && now.graphemes == line.graphemes
                                    && now.wrapped == line.wrapped
                            })
                    });
                prop_assert_eq!(state.sel.is_some(), alive);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(300))]

        /// The windowed lookup agrees with the whole logical line, and
        /// every span is within the grid and the link text starts with a
        /// scheme.
        #[test]
        fn windowed_links_agree_with_the_reference(
            parts in proptest::collection::vec(
                prop_oneof![
                    Just("https://".to_owned()),
                    Just("mailto:".to_owned()),
                    "[a-c@./()\\[\\] \u{4e16}\u{301}]{0,5}",
                    Just("\r\n".to_owned()),
                ],
                0..10,
            ),
            cols in 3u16..9,
        ) {
            let term = term(6, cols, parts.concat().as_bytes());
            for row in 0..6 {
                for col in 0..cols {
                    let at = point(row, col);
                    let full = link_at(term.grid(), 0, at);
                    prop_assert_eq!(&full, &reference_link(term.grid(), at));
                    if let Some(link) = full {
                        prop_assert!(link.url.to_ascii_lowercase().starts_with("https://")
                            || link.url.to_ascii_lowercase().starts_with("mailto:"));
                        for (r, c) in &link.span {
                            prop_assert!(*r < 6 && c.start < c.end && c.end <= cols);
                        }
                    }
                }
            }
        }
    }
}
