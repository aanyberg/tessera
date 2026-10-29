//! The visible screen, its scrollback, the alternate screen and the
//! cursor-relative primitives `tessera-term` drives it with.

use std::cmp::Ordering;
use std::mem;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::damage::{Damage, DamageRows, DamageTracker};
use crate::grapheme::MAX_CLUSTER_SCALARS;
use crate::reflow::{self, ScreenMut, Size};
use crate::{Cell, CellFlags, Line, Point, Scrollback, Style, TabStops, char_width};

/// Default number of scrolled-off lines a grid retains.
pub const DEFAULT_MAX_SCROLLBACK: usize = 10_000;

/// Inclusive row range that scrolling, `insert_lines` and `delete_lines`
/// operate on (DECSTBM).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScrollRegion {
    /// First row of the region.
    pub top: u16,
    /// Last row of the region, inclusive.
    pub bottom: u16,
}

impl ScrollRegion {
    /// Number of rows in the region.
    pub fn height(self) -> u16 {
        self.bottom.saturating_sub(self.top).saturating_add(1)
    }

    /// True when `row` lies inside the region.
    pub fn contains(self, row: u16) -> bool {
        (self.top..=self.bottom).contains(&row)
    }
}

/// Which part of the cursor's line `erase_in_line` clears (EL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EraseLine {
    /// From the cursor to the end of the line, inclusive.
    ToEnd,
    /// From the start of the line to the cursor, inclusive.
    ToStart,
    /// The whole line.
    All,
}

/// Which part of the screen `erase_in_display` clears (ED).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EraseDisplay {
    /// From the cursor to the end of the screen, inclusive.
    Below,
    /// From the start of the screen to the cursor, inclusive.
    Above,
    /// The whole visible screen.
    All,
    /// Scrollback only; the visible screen is untouched.
    Scrollback,
}

/// How a scalar joins an existing cluster: the column of the cluster's
/// base cell and the cluster's width once the scalar is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Attach {
    col: usize,
    width: u16,
}

/// Cursor state captured by `save_cursor` (DECSC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SavedCursor {
    cursor: Point,
    pen: Style,
    pending_wrap: bool,
}

impl SavedCursor {
    /// The slot before any save: home position, default pen.
    const INITIAL: Self = Self {
        cursor: Point { row: 0, col: 0 },
        pen: Style::DEFAULT,
        pending_wrap: false,
    };

    /// Clamps the saved position into a grid of `size` after a resize. The
    /// position is not moved with the text; a saved pending wrap survives
    /// only when the width did not change.
    fn clamp(&mut self, size: Size, cols_changed: bool) {
        self.cursor.row = self.cursor.row.min(size.rows - 1);
        self.cursor.col = self.cursor.col.min(size.cols - 1);
        self.pending_wrap &= !cols_changed;
    }
}

/// The per-screen state the primary screen keeps while the alternate screen
/// is active.
#[derive(Debug, Clone)]
struct Screen {
    lines: Vec<Line>,
    cursor: Point,
    pending_wrap: bool,
    pen: Style,
    saved: SavedCursor,
    tabs: TabStops,
}

/// The visible screen plus scrollback.
///
/// Every operation is total: arguments are clamped so the grid never leaves
/// its bounds. Coordinates are absolute screen coordinates; origin mode is
/// resolved by the caller using [`Grid::scroll_region`].
///
/// # Logical buffer
///
/// History and the visible screen form one logical buffer of
/// [`Grid::total_lines`] lines, addressed by [`Grid::logical_line`]: indices
/// `0..scrollback_len()` are scrollback (oldest first) and
/// `scrollback_len()..total_lines()` are the visible rows, top to bottom.
///
/// # Alternate screen
///
/// [`Grid::enter_alt_screen`] switches to a separate blank screen of the
/// same size. While it is active every primitive operates on it, lines
/// scrolled off its top are discarded, and the primary screen's lines,
/// cursor, pending-wrap state, pen and tab stops are kept untouched until
/// [`Grid::leave_alt_screen`] restores them. Scrollback always belongs to
/// the primary screen: the scrollback accessors and the logical buffer keep
/// reporting the primary history (followed by whichever screen is active).
/// Each screen has its own [`Grid::save_cursor`] slot, as in xterm, so a
/// DECSC issued on the alternate screen leaves the primary screen's saved
/// cursor intact; the alternate slot starts at its initial state (home,
/// default pen) on every entry. The scroll region is shared by both
/// screens. Saving the cursor around DECSET 1049 is the emulator's job.
#[derive(Debug, Clone)]
pub struct Grid {
    rows: u16,
    cols: u16,
    /// Lines of the active screen.
    lines: Vec<Line>,
    scrollback: Scrollback,
    cursor: Point,
    pending_wrap: bool,
    pen: Style,
    region: ScrollRegion,
    saved: SavedCursor,
    tabs: TabStops,
    /// The stashed primary screen; `Some` exactly while the alternate screen
    /// is active. [`Grid::resize`] resizes it together with `lines`.
    primary: Option<Box<Screen>>,
    /// Which visible rows changed (ADR 0016).
    damage: DamageTracker,
    /// Whether rows of the primary screen have gone into history since the
    /// screen was last cleared or created: see
    /// [`Grid::scrolled_since_clear`].
    scrolled_since_clear: bool,
    /// When the last [`Grid::reset`] blanked the screen.
    blanked_at: BlankedAt,
}

impl Grid {
    /// Creates an empty grid of the given size with
    /// [`DEFAULT_MAX_SCROLLBACK`] lines of history.
    ///
    /// A zero dimension is raised to one: a grid always has at least one
    /// cell so the cursor has somewhere to be.
    pub fn new(rows: u16, cols: u16) -> Self {
        Self::with_scrollback(rows, cols, DEFAULT_MAX_SCROLLBACK)
    }

    /// Creates an empty grid that keeps at most `max_scrollback` scrolled-off
    /// lines. Zero disables scrollback.
    pub fn with_scrollback(rows: u16, cols: u16, max_scrollback: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            rows,
            cols,
            lines: vec![Line::blank(cols, Style::DEFAULT); usize::from(rows)],
            scrollback: Scrollback::new(max_scrollback),
            cursor: Point::default(),
            pending_wrap: false,
            pen: Style::DEFAULT,
            region: ScrollRegion {
                top: 0,
                bottom: rows - 1,
            },
            saved: SavedCursor::INITIAL,
            tabs: TabStops::new(cols),
            primary: None,
            damage: DamageTracker::new(rows),
            scrolled_since_clear: false,
            blanked_at: BlankedAt::default(),
        }
    }

    // ----------------------------------------------------------------- reads

    /// Visible row count.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Column count.
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Whether rows of the primary screen have gone into scrollback since
    /// the screen was last cleared.
    ///
    /// - *Set* when a line feed or SU with the full screen as scroll region
    ///   moves a row of the primary screen off its top (even with a
    ///   scrollback cap of zero, which keeps nothing), and when a resize
    ///   pushes rows that were on the primary screen (or the stashed one
    ///   behind the alternate screen) into history: either way the
    ///   screen's content has reached its bottom.
    /// - *Cleared* when the primary screen is erased whole: ED 2
    ///   ([`EraseDisplay::All`]), ED 0 ([`EraseDisplay::Below`]) with the
    ///   cursor at the home position (the `clear` of vt100, screen and
    ///   tmux terminfo, and Node's `console.clear`) unless the top row
    ///   continues the newest history line (then a shell is redrawing a
    ///   line that started above the screen), and RIS ([`Grid::reset`]).
    /// - *Unchanged* by everything else: ED 3 and [`Grid::clear_scrollback`]
    ///   (they empty history but leave a screen full of scrolled output),
    ///   ED 0 anywhere else or on a top row that continues history (a
    ///   shell redrawing its prompt), ED 1, erasing a
    ///   line, moving the cursor, a resize that pushes nothing, and
    ///   anything on the alternate screen.
    ///
    /// It decides where a resize puts the screen (ADR 0009, amendment of
    /// 2026-10-29 for #86): a screen that has scrolled is continuous with
    /// its history and is resized as one buffer anchored at the bottom; a
    /// cleared one keeps its own rows in place and never takes history
    /// back unless the cursor's line ends on the last row. A resize cannot
    /// tell the two apart from the cells alone: a shell redrawing its
    /// prompt with ED 0 leaves blank rows below the cursor just as a clear
    /// does.
    pub fn scrolled_since_clear(&self) -> bool {
        self.scrolled_since_clear
    }

    /// Current cursor position.
    pub fn cursor(&self) -> Point {
        self.cursor
    }

    /// True when the last write landed in the final column and the next
    /// write will wrap to a new line first (the VT "pending wrap" state).
    pub fn pending_wrap(&self) -> bool {
        self.pending_wrap
    }

    /// The style applied to written cells and whose background fills erased
    /// cells.
    pub fn pen(&self) -> Style {
        self.pen
    }

    /// Replaces the pen.
    pub fn set_pen(&mut self, pen: Style) {
        self.pen = pen;
    }

    /// Mutable access to the pen, for incremental SGR updates.
    pub fn pen_mut(&mut self) -> &mut Style {
        &mut self.pen
    }

    /// Visible lines, top to bottom.
    pub fn visible_lines(&self) -> &[Line] {
        &self.lines
    }

    /// The visible line at `row`, if in range.
    pub fn line(&self, row: u16) -> Option<&Line> {
        self.lines.get(usize::from(row))
    }

    /// Mutable access to the visible line at `row`, if in range.
    ///
    /// Callers must keep `cells.len() == cols()`, and the wide-character
    /// pairing and grapheme table consistent (see [`Line`]); this is how
    /// shell integration attaches marks.
    ///
    /// The row counts as damaged ([`Grid::damage`]) whatever the caller
    /// does with it.
    pub fn line_mut(&mut self, row: u16) -> Option<&mut Line> {
        let line = self.lines.get_mut(usize::from(row))?;
        self.damage.extend_row(row);
        Some(line)
    }

    /// The cell at `point`, if in range.
    pub fn cell(&self, point: Point) -> Option<&Cell> {
        self.line(point.row)?.cells.get(usize::from(point.col))
    }

    /// Number of lines in scrollback.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Maximum number of scrollback lines retained.
    pub fn max_scrollback(&self) -> usize {
        self.scrollback.cap()
    }

    /// A scrollback line; index `0` is the oldest, `scrollback_len() - 1`
    /// the one most recently scrolled off the screen.
    pub fn scrollback_line(&self, index: usize) -> Option<&Line> {
        self.scrollback.get(index)
    }

    /// Number of lines in the logical buffer: scrollback plus visible rows.
    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.lines.len()
    }

    /// A line of the logical buffer. Index `0` is the oldest scrollback
    /// line; `scrollback_len()..total_lines()` are the visible rows of the
    /// active screen, top to bottom.
    pub fn logical_line(&self, index: usize) -> Option<&Line> {
        match index.checked_sub(self.scrollback.len()) {
            None => self.scrollback.get(index),
            Some(row) => self.lines.get(row),
        }
    }

    /// Every line of the logical buffer, oldest scrollback line first and
    /// the bottom visible row last, without allocating.
    pub fn logical_lines(&self) -> impl DoubleEndedIterator<Item = &Line> + Clone + '_ {
        self.scrollback.iter().chain(&self.lines)
    }

    /// True while the alternate screen is active.
    pub fn is_alt_screen(&self) -> bool {
        self.primary.is_some()
    }

    /// The active scroll region.
    pub fn scroll_region(&self) -> ScrollRegion {
        self.region
    }

    /// The visible rows changed since the last [`Grid::take_damage`] (all
    /// of them for a new grid), and the current damage generation.
    ///
    /// Every primitive that changes a visible cell, a line's wrap flag or
    /// the cursor position records the rows it touched: writes and edits
    /// at the cursor its row, a cursor move the row it leaves and the row
    /// it lands on, scrolls and line insertion the rows of the region, and
    /// a full-screen scroll, an alternate-screen switch, a resize, a reset
    /// or erasing the whole display everything. [`Grid::line_mut`] counts
    /// as a change of its row. The answer may include rows that did not
    /// change, never omits one that did (ADR 0016).
    #[must_use]
    pub fn damage(&self) -> Damage {
        self.damage.current(self.cursor.row)
    }

    /// Like [`Grid::damage`], then starts over: the next call reports only
    /// what changes after this one.
    pub fn take_damage(&mut self) -> Damage {
        self.damage.take(self.cursor.row)
    }

    /// The visible rows changed after `generation`, a
    /// [`Damage::generation`] this grid reported earlier, for readers that
    /// only hold `&Grid`. A generation this grid did not hand out (another
    /// grid's, a clone's, or `0`) gives [`DamageRows::All`].
    /// [`Grid::take_damage`] does not affect the answer. The cursor's row
    /// is reported from the moment the cursor arrives on it until it leaves
    /// (or an owner takes the damage), whether or not anything on it
    /// changed and even if the reader already saw that change, so a reader
    /// that needs exact answers compares that row itself.
    #[must_use]
    pub fn damage_since(&self, generation: u64) -> DamageRows {
        self.damage.since(generation, self.cursor.row)
    }

    /// The current damage generation, what [`Grid::damage`] reports as
    /// [`Damage::generation`], without computing the damaged rows.
    #[must_use]
    pub fn damage_generation(&self) -> u64 {
        self.damage.generation()
    }

    /// The tab stops.
    pub fn tabs(&self) -> &TabStops {
        &self.tabs
    }

    /// Mutable access to the tab stops.
    pub fn tabs_mut(&mut self) -> &mut TabStops {
        &mut self.tabs
    }

    // --------------------------------------------------------------- writing

    /// Writes `ch` with the current pen at the cursor and advances it.
    ///
    /// Writing in the last column leaves the cursor there and arms pending
    /// wrap: the next write first moves to the start of the next line,
    /// scrolling if at the bottom of the region, and marks the finished
    /// line as soft-wrapped.
    ///
    /// The write is width-aware ([`char_width`]):
    ///
    /// - A scalar that joins the cluster of the character before the
    ///   cursor (see [`Grid::would_attach`]) is added to that cell's
    ///   cluster instead of taking a cell; the cell may grow from one
    ///   column to two (a flag's second regional indicator, VS16) or
    ///   shrink from two to one (VS15), moving the cursor with it.
    /// - Any other zero-width scalar, and any control character, is
    ///   dropped.
    /// - A two-column character writes a [`CellFlags::WIDE`] cell and a
    ///   [`CellFlags::SPACER`] and advances two columns. With one column
    ///   left it blanks that column with the pen background, marks it
    ///   [`CellFlags::PAD`] (padding, not text) and wraps first; on a
    ///   one-column grid it is dropped.
    ///
    /// Overwriting either half of a wide character blanks the other half.
    ///
    /// [`CellFlags::WIDE`]: crate::CellFlags::WIDE
    /// [`CellFlags::SPACER`]: crate::CellFlags::SPACER
    /// [`CellFlags::PAD`]: crate::CellFlags::PAD
    #[inline]
    pub fn put_char(&mut self, ch: char) {
        if is_simple(ch) && self.target_is_simple() {
            // Plain text: neither side can join a cluster.
            self.put_cell(Cell::new(ch, self.pen));
            return;
        }
        self.put_char_slow(ch);
    }

    /// [`Grid::put_char`] for everything but plain text.
    fn put_char_slow(&mut self, ch: char) {
        let Some(width) = char_width(ch) else {
            return;
        };
        if let Some(plan) = self.plan_attach(ch) {
            self.attach(ch, plan);
            return;
        }
        match width {
            0 => {}
            1 => self.put_cell(Cell::new(ch, self.pen)),
            _ => self.put_wide(Cell::new(ch, self.pen)),
        }
    }

    /// Writes an arbitrary cell at the cursor as a one-column character
    /// and advances it like [`Grid::put_char`]. Width and cluster markers
    /// on `cell` are ignored: it never attaches to its neighbour and never
    /// takes two columns.
    pub fn put_cell(&mut self, cell: Cell) {
        if self.pending_wrap {
            self.wrap();
        }
        self.damage.touch_cursor_row();
        let Point { row, col } = self.cursor;
        let pen = self.pen;
        let line = &mut self.lines[usize::from(row)];
        let cell = Cell {
            flags: CellFlags::EMPTY,
            ..cell
        };
        let slot = &mut line.cells[usize::from(col)];
        // A flagless cell has no cluster entry to drop and no other half.
        if slot.flags.is_empty() {
            *slot = cell;
        } else {
            line.unpair(usize::from(col), pen);
            line.set(usize::from(col), cell);
        }
        if col + 1 < self.cols {
            self.cursor.col = col + 1;
        } else {
            self.pending_wrap = true;
        }
    }

    /// Writes each character of `text` with [`Grid::put_char`].
    pub fn put_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.put_char(ch);
        }
    }

    /// True when [`Grid::put_char`] would add `ch` to an existing cell's
    /// grapheme cluster rather than write a new cell. Pure; the emulator
    /// asks before it applies insert mode, wrapping or DECAWM, none of
    /// which apply to a scalar that joins the previous character.
    ///
    /// The cell `ch` would join is the one left of the cursor, or the one
    /// under it while a wrap is pending, resolved through a spacer to the
    /// wide character's left half; never a cell on another row. `ch`
    /// attaches when all of these hold:
    ///
    /// - that cell shows something (its character is not `' '`);
    /// - its cluster has fewer than [`MAX_CLUSTER_SCALARS`] scalars;
    /// - the cluster plus `ch` is a single extended grapheme cluster
    ///   (UAX #29);
    /// - the joined cluster is at most two columns wide
    ///   (`UnicodeWidthStr::width`), unless the cluster already contains a
    ///   ZWJ (an emoji ZWJ sequence can be wider while it is incomplete);
    /// - when the cluster would grow from one column to two, a column is
    ///   free to its right on the same line.
    #[inline]
    pub fn would_attach(&self, ch: char) -> bool {
        char_width(ch).is_some() && self.plan_attach(ch).is_some()
    }

    /// Where and how `ch` would join an existing cluster, if it does (see
    /// [`Grid::would_attach`]).
    fn plan_attach(&self, ch: char) -> Option<Attach> {
        let Point { row, col } = self.cursor;
        let line = &self.lines[usize::from(row)];
        let col = usize::from(col);
        let mut base = if self.pending_wrap {
            col
        } else {
            col.checked_sub(1)?
        };
        if line.cells[base].is_spacer() {
            base = base.checked_sub(1)?;
        }
        let cell = line.cells[base];
        if cell.ch == ' ' || cannot_join(&cell, ch) {
            return None;
        }
        if cell.is_cluster() {
            if line
                .cluster(base)
                .is_some_and(|text| cluster_cannot_take(text, ch))
            {
                return None;
            }
        } else if is_simple(cell.ch) && is_combining_diacritic(ch) {
            // `e` + U+0301: one grapheme (GB9) as wide as its base.
            return Some(Attach {
                col: base,
                width: 1,
            });
        }
        self.plan_attach_slow(line, base, cell, ch)
    }

    /// The part of [`Grid::plan_attach`] that segments the joined text.
    fn plan_attach_slow(&self, line: &Line, base: usize, cell: Cell, ch: char) -> Option<Attach> {
        let mut one = [0; 4];
        let old = match line.cluster(base) {
            Some(text) if cell.is_cluster() => text,
            _ => cell.ch.encode_utf8(&mut one),
        };
        if old.chars().count() >= MAX_CLUSTER_SCALARS {
            return None;
        }
        // At most `MAX_CLUSTER_SCALARS - 1` scalars of 4 bytes plus `ch`.
        let mut buf = [0; 4 * MAX_CLUSTER_SCALARS];
        let joined = {
            let len = old.len();
            buf[..len].copy_from_slice(old.as_bytes());
            let tail = ch.encode_utf8(&mut buf[len..]).len();
            std::str::from_utf8(&buf[..len + tail]).ok()?
        };
        if joined.graphemes(true).nth(1).is_some() {
            return None;
        }
        let raw = joined.width();
        if raw > 2 && !old.contains('\u{200D}') {
            return None;
        }
        let width = if raw >= 2 { 2 } else { 1 };
        if width > cell.width() && base + 1 >= usize::from(self.cols) {
            return None;
        }
        Some(Attach { col: base, width })
    }

    /// True when the cell `ch` would join (see [`Grid::would_attach`]) is
    /// missing, a flagless cell showing an [`is_simple`] character, or the
    /// spacer of a wide character that is not a cluster: then a simple
    /// character cannot attach (see [`cannot_join`]) and is written as a
    /// plain cell.
    #[inline]
    fn target_is_simple(&self) -> bool {
        let Point { row, col } = self.cursor;
        let col = if self.pending_wrap {
            usize::from(col)
        } else if let Some(left) = usize::from(col).checked_sub(1) {
            left
        } else {
            return true;
        };
        let cells = &self.lines[usize::from(row)].cells;
        let cell = &cells[col];
        if cell.flags.is_empty() {
            return is_simple(cell.ch);
        }
        cell.is_spacer()
            && col
                .checked_sub(1)
                .is_some_and(|base| cells[base].flags == CellFlags::WIDE)
    }

    /// Adds `ch` to the cluster `plan` names and resizes the cell.
    fn attach(&mut self, ch: char, plan: Attach) {
        let Attach { col: base, width } = plan;
        self.damage.touch_cursor_row();
        let pen = self.pen;
        let line = &mut self.lines[usize::from(self.cursor.row)];
        let cell = line.cells[base];
        let mut one = [0; 4];
        let old = match line.cluster(base) {
            Some(text) if cell.is_cluster() => text,
            _ => cell.ch.encode_utf8(&mut one),
        };
        // Exact capacity, so turning it into a `Box<str>` does not
        // reallocate.
        let mut text = String::with_capacity(old.len() + ch.len_utf8());
        text.push_str(old);
        text.push(ch);
        line.graphemes.set(base, text.into_boxed_str());
        line.cells[base].flags |= CellFlags::CLUSTER;
        match width.cmp(&cell.width()) {
            Ordering::Greater => {
                line.unpair(base + 1, pen);
                line.set(base + 1, Cell::spacer(cell.style));
                line.cells[base].flags |= CellFlags::WIDE;
                self.advance_to(base + 2);
            }
            Ordering::Less => {
                line.cells[base].flags = line.cells[base].flags.difference(CellFlags::WIDE);
                line.blank_at(base + 1, pen);
                self.pending_wrap = false;
                self.cursor.col = u16::try_from(base + 1).unwrap_or(self.cols - 1);
                // The row was touched above.
            }
            Ordering::Equal => {}
        }
    }

    /// Writes a two-column character: `cell` and a spacer after it.
    fn put_wide(&mut self, cell: Cell) {
        if self.cols < 2 {
            return;
        }
        if self.pending_wrap {
            self.wrap();
        } else if self.cursor.col == self.cols - 1 {
            // One column left: pad it, then continue on the next line.
            self.pad_last_column();
            self.wrap();
        }
        self.damage.touch_cursor_row();
        let Point { row, col } = self.cursor;
        let col = usize::from(col);
        let pen = self.pen;
        let line = &mut self.lines[usize::from(row)];
        line.unpair(col, pen);
        line.unpair(col + 1, pen);
        line.set(
            col,
            Cell {
                flags: CellFlags::WIDE,
                ..cell
            },
        );
        line.set(col + 1, Cell::spacer(cell.style));
        self.advance_to(col + 2);
    }

    /// Turns the last column of the cursor's row into padding: blanks it
    /// with the pen background (a wide character it cuts is blanked whole)
    /// and marks it [`CellFlags::PAD`]. This is what a wide character that
    /// does not fit in the last column leaves behind before it wraps; the
    /// cell is not text, so a resize that joins the row with the next one
    /// drops it. The cursor and pending wrap are untouched.
    ///
    /// [`Grid::put_char`] does this itself; an emulator that wraps a wide
    /// character on its own (insert mode) calls it before wrapping.
    pub fn pad_last_column(&mut self) {
        let last = usize::from(self.cols - 1);
        let pen = self.pen;
        self.damage.touch_cursor_row();
        let line = &mut self.lines[usize::from(self.cursor.row)];
        line.fill(last..last + 1, pen);
        line.cells[last] = Cell::pad_with(pen);
    }

    /// Moves the cursor to column `col` after a write, or arms pending
    /// wrap on the last column when the write reached the end of the line.
    fn advance_to(&mut self, col: usize) {
        // Only ever called after a write, which touched the cursor row.
        match u16::try_from(col) {
            Ok(col) if col < self.cols => {
                self.cursor.col = col;
                self.pending_wrap = false;
            }
            _ => {
                self.cursor.col = self.cols - 1;
                self.pending_wrap = true;
            }
        }
    }

    /// Resolves a pending wrap: marks the cursor's line soft-wrapped and
    /// moves to the start of the next line, scrolling at the bottom of the
    /// scroll region.
    fn wrap(&mut self) {
        self.pending_wrap = false;
        self.lines[usize::from(self.cursor.row)].wrapped = true;
        self.damage.touch_cursor_row();
        self.cursor.col = 0;
        self.line_feed();
    }

    // -------------------------------------------------------- cursor motion

    /// Moves the cursor to `(row, col)`, clamped to the screen (CUP).
    pub fn goto(&mut self, row: u16, col: u16) {
        self.pending_wrap = false;
        self.move_cursor(Point {
            row: row.min(self.rows - 1),
            col: col.min(self.cols - 1),
        });
    }

    /// Moves the cursor to `to` (in range) and records the damage: the row
    /// it leaves and the row it lands on.
    #[inline]
    fn move_cursor(&mut self, to: Point) {
        if to.row == self.cursor.row {
            self.damage.touch_cursor_row();
        } else {
            self.damage.cursor_left_row(self.cursor.row);
        }
        self.cursor = to;
    }

    /// Moves the cursor to column `col` (in range) of its row.
    #[inline]
    fn move_cursor_col(&mut self, col: u16) {
        self.damage.touch_cursor_row();
        self.cursor.col = col;
    }

    /// Moves the cursor to `row` in its current column (VPA).
    pub fn goto_row(&mut self, row: u16) {
        self.goto(row, self.cursor.col);
    }

    /// Moves the cursor to `col` in its current row (CHA).
    pub fn goto_col(&mut self, col: u16) {
        self.goto(self.cursor.row, col);
    }

    /// Moves the cursor up `n` rows (CUU), stopping at the top of the scroll
    /// region unless the cursor starts above it, in which case it stops at
    /// the top of the screen (xterm semantics).
    pub fn cursor_up(&mut self, n: u16) {
        self.pending_wrap = false;
        let limit = if self.cursor.row >= self.region.top {
            self.region.top
        } else {
            0
        };
        let row = self.cursor.row.saturating_sub(n).max(limit);
        self.move_cursor(Point { row, ..self.cursor });
    }

    /// Moves the cursor down `n` rows (CUD), stopping at the bottom of the
    /// scroll region unless the cursor starts below it, in which case it
    /// stops at the bottom of the screen (xterm semantics).
    pub fn cursor_down(&mut self, n: u16) {
        self.pending_wrap = false;
        let limit = if self.cursor.row <= self.region.bottom {
            self.region.bottom
        } else {
            self.rows - 1
        };
        let row = self.cursor.row.saturating_add(n).min(limit);
        self.move_cursor(Point { row, ..self.cursor });
    }

    /// Moves the cursor left `n` columns, stopping at column zero (CUB).
    pub fn cursor_left(&mut self, n: u16) {
        self.pending_wrap = false;
        self.move_cursor_col(self.cursor.col.saturating_sub(n));
    }

    /// Moves the cursor right `n` columns, stopping at the last column (CUF).
    pub fn cursor_right(&mut self, n: u16) {
        self.pending_wrap = false;
        self.move_cursor_col(self.cursor.col.saturating_add(n).min(self.cols - 1));
    }

    /// Moves the cursor to column zero (CR).
    pub fn carriage_return(&mut self) {
        self.pending_wrap = false;
        self.move_cursor_col(0);
    }

    /// Moves the cursor down one row (LF / IND). At the bottom of the scroll
    /// region the region scrolls up instead; below the region the cursor
    /// stops at the last row.
    pub fn line_feed(&mut self) {
        self.pending_wrap = false;
        if self.cursor.row == self.region.bottom {
            self.scroll_up(1);
        } else if self.cursor.row < self.rows - 1 {
            self.move_cursor(Point {
                row: self.cursor.row + 1,
                ..self.cursor
            });
        }
    }

    /// Moves the cursor up one row (RI). At the top of the scroll region the
    /// region scrolls down instead; above the region the cursor stops at
    /// row zero.
    pub fn reverse_index(&mut self) {
        self.pending_wrap = false;
        if self.cursor.row == self.region.top {
            self.scroll_down(1);
        } else if self.cursor.row > 0 {
            self.move_cursor(Point {
                row: self.cursor.row - 1,
                ..self.cursor
            });
        }
    }

    /// Saves the cursor position, pen and pending-wrap state (DECSC).
    pub fn save_cursor(&mut self) {
        self.saved = SavedCursor {
            cursor: self.cursor,
            pen: self.pen,
            pending_wrap: self.pending_wrap,
        };
    }

    /// Restores what [`Grid::save_cursor`] saved (DECRC). Without a prior
    /// save this homes the cursor and resets the pen.
    pub fn restore_cursor(&mut self) {
        let saved = self.saved;
        self.goto(saved.cursor.row, saved.cursor.col);
        self.pen = saved.pen;
        self.pending_wrap = saved.pending_wrap;
    }

    /// Returns the active screen's [`Grid::save_cursor`] slot to its initial
    /// state: home position, default pen, no pending wrap, so a following
    /// [`Grid::restore_cursor`] behaves as if nothing had been saved. The
    /// live cursor, pen and pending wrap are untouched, and so is the other
    /// screen's slot. DECSTR uses it.
    pub fn reset_saved_cursor(&mut self) {
        self.saved = SavedCursor::INITIAL;
    }

    // ------------------------------------------------------- scrollback cap

    /// Changes how many scrollback lines are retained. Lowering the cap
    /// drops the oldest lines first; zero disables scrollback and discards
    /// what was kept.
    pub fn set_max_scrollback(&mut self, max_scrollback: usize) {
        self.scrollback.set_cap(max_scrollback);
    }

    /// Discards all scrollback, keeping the cap and the visible screen.
    /// [`Grid::scrolled_since_clear`] is unchanged: the screen still shows
    /// whatever scrolled onto it.
    pub fn clear_scrollback(&mut self) {
        self.scrollback.clear();
    }

    // ---------------------------------------------------------------- resize

    /// Resizes the grid to `rows` × `cols`, re-wrapping soft-wrapped lines
    /// to the new width (ADR 0009). A zero dimension is raised to one, as
    /// in [`Grid::new`]; the current size is a no-op that changes nothing.
    ///
    /// The primary screen and the scrollback are one buffer. The screen is
    /// *anchored at the bottom* when rows of it have gone into history
    /// since it was last cleared ([`Grid::scrolled_since_clear`]: output
    /// scrolled it, or a resize pushed its rows), or when the cursor is
    /// on the last row or its logical line ends there (every row from the
    /// cursor's to the one before the last is wrapped); only such a screen
    /// takes history back when it gets room for more rows (#86). A screen
    /// that has scrolled is resized as one buffer with its history,
    /// exactly as below without the #86 parts:
    ///
    /// - **Rows** change first, at the old width, by moving whole rows.
    ///   Growing an anchored screen pulls the newest scrollback lines back
    ///   onto the top of the screen (blank rows at the bottom once history
    ///   runs out); any other screen, such as a prompt after a clear, gets
    ///   blank rows at the bottom. Shrinking first drops blank rows below
    ///   the cursor (default blanks and [`CellFlags::PAD`] cells of any
    ///   style only, even the empty continuation of a wrapped row, whose
    ///   wrap flag is then cleared),
    ///   then pushes rows from the top into scrollback (oldest lines are
    ///   evicted past the cap). The cursor moves with the text and stops
    ///   at row zero.
    /// - **Columns:** every logical line (rows joined by
    ///   [`Line::wrapped`]) is joined and split again at the new width.
    ///   Trailing default blanks ([`Cell::BLANK`], or padding of any
    ///   style) of a logical line are dropped and rows are padded with
    ///   default blanks; every other cell, including a space with a
    ///   colour or attribute and blanks inside a line, is text and moves
    ///   with it. A wide character never straddles the edge: it starts
    ///   the next row and leaves a [`CellFlags::PAD`] cell with its
    ///   background behind, which a later join drops again (only padding
    ///   is ever dropped); on a one-column grid it is dropped. Cluster
    ///   text and marks move with their cells and lines (marks of a
    ///   joined line collect on its first row). On a screen that has
    ///   scrolled the window is the last `rows` rows of the result (blank
    ///   rows below the cursor absorb growth first). On a cleared one only
    ///   the rows that were on the screen move it: lines wholly in
    ///   scrollback just get longer or shorter there. When the screen's
    ///   rows grow, blank rows below
    ///   the cursor absorb that first and then the top rows go into
    ///   scrollback; when they shrink, an anchored screen pulls scrollback
    ///   back and any other keeps its place with blank rows at the bottom.
    ///   A line straddling scrollback and the screen splits at the row
    ///   holding the old top row's first cell.
    /// - **Cursor:** it stays on the character it was on. On a blank after
    ///   the end of its line's text it keeps its distance from the text
    ///   while that fits in the row, else it stops on the last column;
    ///   when the line's last row is exactly full it sits on the last
    ///   column with a wrap pending.
    ///
    /// The alternate screen is never re-wrapped: its rows are cut or
    /// padded, rows leaving it are discarded and pending wrap is cleared
    /// when the width changes, while the stashed primary screen is resized
    /// as above with its own cursor. Every real resize resets the scroll
    /// region to the full screen, clamps each screen's saved cursor (it is
    /// not moved with the text) and extends the tab stops with the default
    /// every-eight stops beyond the old width, keeping the ones below it.
    /// The pen is unchanged; added rows and cells use the default style.
    ///
    /// A row-only change costs O(rows changed + rows); a column change
    /// re-wraps the whole scrollback, O(total cells). Every scrollback
    /// line is as wide as the grid, so a resize to a very wide grid with a
    /// long history allocates that width for every retained line.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let size = Size {
            rows: rows.max(1),
            cols: cols.max(1),
        };
        if size.rows == self.rows && size.cols == self.cols {
            return;
        }
        self.damage.resize(size.rows);
        let old_cols = self.cols;
        let cols_changed = size.cols != old_cols;
        let active = ScreenMut {
            lines: &mut self.lines,
            cursor: &mut self.cursor,
            pending_wrap: &mut self.pending_wrap,
        };
        match self.primary.as_deref_mut() {
            Some(primary) => {
                let stashed = ScreenMut {
                    lines: &mut primary.lines,
                    cursor: &mut primary.cursor,
                    pending_wrap: &mut primary.pending_wrap,
                };
                self.scrolled_since_clear |= reflow::resize_primary(
                    stashed,
                    &mut self.scrollback,
                    old_cols,
                    size,
                    self.scrolled_since_clear,
                );
                primary.saved.clamp(size, cols_changed);
                primary.tabs.resize(size.cols);
                reflow::resize_alternate(active, old_cols, size);
            }
            None => {
                self.scrolled_since_clear |= reflow::resize_primary(
                    active,
                    &mut self.scrollback,
                    old_cols,
                    size,
                    self.scrolled_since_clear,
                );
            }
        }
        self.saved.clamp(size, cols_changed);
        self.tabs.resize(size.cols);
        self.rows = size.rows;
        self.cols = size.cols;
        self.region = ScrollRegion {
            top: 0,
            bottom: size.rows - 1,
        };
    }

    // ------------------------------------------------------ alternate screen

    /// Switches to the alternate screen: a separate blank screen of the same
    /// size, without scrollback. A no-op when already active.
    ///
    /// The primary screen's lines, cursor, pending-wrap state, pen, saved
    /// cursor and tab stops are stashed. The alternate screen starts with
    /// the cursor in the same position, the same pen and a copy of the tab
    /// stops; its lines are blank with the pen's background, like an erase.
    /// Pending wrap is cleared and its saved-cursor slot starts at the
    /// initial state (home, default pen). Unlike xterm, which keeps the
    /// alternate slot across switches, each entry starts fresh, matching
    /// the fresh alternate screen contents.
    pub fn enter_alt_screen(&mut self) {
        if self.primary.is_some() {
            return;
        }
        let blank = vec![Line::blank(self.cols, self.pen); usize::from(self.rows)];
        let primary = Screen {
            lines: mem::replace(&mut self.lines, blank),
            cursor: self.cursor,
            pending_wrap: self.pending_wrap,
            pen: self.pen,
            saved: mem::replace(&mut self.saved, SavedCursor::INITIAL),
            tabs: self.tabs.clone(),
        };
        self.primary = Some(Box::new(primary));
        self.pending_wrap = false;
        self.damage.extend_all();
    }

    /// Returns to the primary screen, discarding the alternate screen's
    /// contents and restoring the primary lines, cursor, pending-wrap state,
    /// pen, saved cursor and tab stops exactly as they were on entry. A
    /// no-op when the primary screen is already active.
    pub fn leave_alt_screen(&mut self) {
        let Some(primary) = self.primary.take() else {
            return;
        };
        let Screen {
            lines,
            cursor,
            pending_wrap,
            pen,
            saved,
            tabs,
        } = *primary;
        // `resize` keeps the stashed primary screen the size of the
        // alternate one; catch any path that does not.
        debug_assert_eq!(lines.len(), usize::from(self.rows), "stale primary rows");
        debug_assert!(
            lines
                .iter()
                .all(|l| l.cells.len() == usize::from(self.cols)),
            "stale primary cols"
        );
        self.lines = lines;
        self.cursor = cursor;
        self.pending_wrap = pending_wrap;
        self.pen = pen;
        self.saved = saved;
        self.tabs = tabs;
        self.damage.extend_all();
    }

    // ------------------------------------------------------------ tab stops

    /// Sets a tab stop at the cursor column (HTS).
    pub fn set_tab_stop(&mut self) {
        self.tabs.set(self.cursor.col);
    }

    /// Clears the tab stop at the cursor column (TBC 0).
    pub fn clear_tab_stop(&mut self) {
        self.tabs.clear(self.cursor.col);
    }

    /// Clears every tab stop (TBC 3).
    pub fn clear_all_tab_stops(&mut self) {
        self.tabs.clear_all();
    }

    /// Moves the cursor to the `n`th next tab stop, or the last column when
    /// fewer remain (HT / CHT). `n == 0` is a no-op.
    pub fn tab_forward(&mut self, n: u16) {
        self.pending_wrap = false;
        if n == 0 {
            return;
        }
        let col = self
            .tabs
            .next(self.cursor.col, n)
            .unwrap_or(self.cols - 1)
            .min(self.cols - 1);
        self.move_cursor_col(col);
    }

    /// Moves the cursor to the `n`th previous tab stop, or column zero when
    /// fewer precede it (CBT). `n == 0` is a no-op.
    pub fn tab_backward(&mut self, n: u16) {
        self.pending_wrap = false;
        if n == 0 {
            return;
        }
        let col = self.tabs.previous(self.cursor.col, n).unwrap_or(0);
        self.move_cursor_col(col);
    }

    // -------------------------------------------------------- scroll region

    /// Sets the scroll region to rows `top..=bottom` (DECSTBM).
    ///
    /// Rows are clamped to the screen. A region of fewer than two rows is
    /// invalid and resets the region to the full screen, as DECSTBM does.
    /// The cursor is left where it is; the emulator homes it.
    pub fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        let top = top.min(self.rows - 1);
        let bottom = bottom.min(self.rows - 1);
        self.region = if top < bottom {
            ScrollRegion { top, bottom }
        } else {
            ScrollRegion {
                top: 0,
                bottom: self.rows - 1,
            }
        };
    }

    /// Resets the scroll region to the full screen.
    pub fn reset_scroll_region(&mut self) {
        self.region = ScrollRegion {
            top: 0,
            bottom: self.rows - 1,
        };
    }

    /// Scrolls the region up `n` lines (SU): lines leave through the top and
    /// blank lines enter at the bottom. When the region is the whole primary
    /// screen the departing lines go to scrollback; on the alternate screen
    /// they are discarded.
    pub fn scroll_up(&mut self, n: u16) {
        let ScrollRegion { top, bottom } = self.region;
        let n = usize::from(n.min(self.region.height()));
        if n == 0 {
            return;
        }
        self.extend_region(top, bottom);
        let top = usize::from(top);
        let keep_history = self.region_is_full_screen() && !self.is_alt_screen();
        self.scrolled_since_clear |= keep_history;
        for i in top..top + n {
            let mut departed = mem::take(&mut self.lines[i]);
            departed.graphemes.compact();
            let spare = if keep_history {
                self.scrollback.push(departed)
            } else {
                Some(departed)
            };
            // Reuse the evicted (or discarded) line's allocation so a full
            // history scrolls without allocating.
            self.lines[i] = match spare {
                Some(line) => recycle(line, self.cols, self.pen),
                None => Line::blank(self.cols, self.pen),
            };
        }
        self.lines[top..=usize::from(bottom)].rotate_left(n);
    }

    /// Scrolls the region down `n` lines (SD): lines leave through the
    /// bottom and are discarded, blank lines enter at the top.
    pub fn scroll_down(&mut self, n: u16) {
        let ScrollRegion { top, bottom } = self.region;
        let n = usize::from(n.min(self.region.height()));
        if n == 0 {
            return;
        }
        self.extend_region(top, bottom);
        let (top, bottom) = (usize::from(top), usize::from(bottom));
        self.lines[top..=bottom].rotate_right(n);
        for line in &mut self.lines[top..top + n] {
            *line = Line::blank(self.cols, self.pen);
        }
    }

    fn region_is_full_screen(&self) -> bool {
        self.region.top == 0 && self.region.bottom == self.rows - 1
    }

    /// Records rows `top..=bottom` as damaged, the whole screen when that
    /// is every row.
    fn extend_region(&mut self, top: u16, bottom: u16) {
        if top == 0 && bottom >= self.rows - 1 {
            self.damage.extend_all();
        } else {
            self.damage.extend_rows(top, bottom);
        }
    }

    // ------------------------------------------------------- line editing

    /// Inserts `n` blank lines at the cursor row, pushing the lines below it
    /// down within the scroll region (IL). Lines pushed past the bottom of
    /// the region are discarded. A no-op when the cursor is outside the
    /// region. The cursor column is left unchanged.
    pub fn insert_lines(&mut self, n: u16) {
        self.pending_wrap = false;
        if !self.region.contains(self.cursor.row) {
            return;
        }
        let row = usize::from(self.cursor.row);
        let bottom = usize::from(self.region.bottom);
        let n = usize::from(n).min(bottom - row + 1);
        if n == 0 {
            return;
        }
        self.extend_region(self.cursor.row, self.region.bottom);
        self.lines[row..=bottom].rotate_right(n);
        for line in &mut self.lines[row..row + n] {
            *line = Line::blank(self.cols, self.pen);
        }
    }

    /// Deletes `n` lines starting at the cursor row, pulling the lines below
    /// up within the scroll region and filling with blanks at the bottom
    /// (DL). A no-op when the cursor is outside the region. The cursor
    /// column is left unchanged.
    pub fn delete_lines(&mut self, n: u16) {
        self.pending_wrap = false;
        if !self.region.contains(self.cursor.row) {
            return;
        }
        let row = usize::from(self.cursor.row);
        let bottom = usize::from(self.region.bottom);
        let n = usize::from(n).min(bottom - row + 1);
        if n == 0 {
            return;
        }
        self.extend_region(self.cursor.row, self.region.bottom);
        self.lines[row..=bottom].rotate_left(n);
        for line in &mut self.lines[bottom + 1 - n..=bottom] {
            *line = Line::blank(self.cols, self.pen);
        }
    }

    // ------------------------------------------------------ char editing

    /// Inserts `n` blank cells at the cursor, shifting the rest of the line
    /// right; cells pushed past the last column are lost (ICH). A wide
    /// character split by the cursor, or whose right half is pushed off the
    /// line, is blanked whole.
    pub fn insert_chars(&mut self, n: u16) {
        self.pending_wrap = false;
        let Point { row, col } = self.cursor;
        let (col, cols) = (usize::from(col), usize::from(self.cols));
        let n = usize::from(n).min(cols - col);
        if n == 0 {
            return;
        }
        self.damage.touch_cursor_row();
        self.lines[usize::from(row)].insert_blanks(col, n, self.pen);
    }

    /// Deletes `n` cells at the cursor, shifting the rest of the line left
    /// and filling with blanks at the end (DCH). A wide character split by
    /// either end of the deleted range is blanked whole.
    pub fn delete_chars(&mut self, n: u16) {
        self.pending_wrap = false;
        let Point { row, col } = self.cursor;
        let (col, cols) = (usize::from(col), usize::from(self.cols));
        let n = usize::from(n).min(cols - col);
        if n == 0 {
            return;
        }
        self.damage.touch_cursor_row();
        self.lines[usize::from(row)].delete_cells(col, n, self.pen);
    }

    /// Blanks `n` cells starting at the cursor without shifting (ECH). A
    /// wide character cut by either end of the range is blanked whole.
    pub fn erase_chars(&mut self, n: u16) {
        self.pending_wrap = false;
        let Point { row, col } = self.cursor;
        let col = usize::from(col);
        let end = col.saturating_add(usize::from(n));
        self.damage.touch_cursor_row();
        self.lines[usize::from(row)].fill(col..end, self.pen);
    }

    // --------------------------------------------------------------- erase

    /// Blanks part of the cursor's line (EL). Marks are kept; the wrap flag
    /// is cleared when the end of the line is erased. A wide character cut
    /// by the cursor column is blanked whole.
    pub fn erase_in_line(&mut self, mode: EraseLine) {
        self.pending_wrap = false;
        let Point { row, col } = self.cursor;
        let (col, cols) = (usize::from(col), usize::from(self.cols));
        self.damage.touch_cursor_row();
        let line = &mut self.lines[usize::from(row)];
        match mode {
            EraseLine::ToEnd => {
                line.fill(col..cols, self.pen);
                line.wrapped = false;
            }
            EraseLine::ToStart => line.fill(0..col + 1, self.pen),
            EraseLine::All => {
                line.fill(0..cols, self.pen);
                line.wrapped = false;
            }
        }
    }

    /// Blanks part of the screen (ED). Lines erased in full also lose their
    /// marks and wrap flag; the cursor's line, when only partially erased,
    /// behaves like [`Grid::erase_in_line`]. On the primary screen, an
    /// erase of the whole screen ([`EraseDisplay::All`], or
    /// [`EraseDisplay::Below`] from the home position when the top row
    /// does not continue the newest scrollback line) also clears
    /// [`Grid::scrolled_since_clear`] and ends the newest scrollback line
    /// (clears its wrap flag), whose continuation on the top row it erased;
    /// [`EraseDisplay::Scrollback`] is [`Grid::clear_scrollback`].
    pub fn erase_in_display(&mut self, mode: EraseDisplay) {
        self.pending_wrap = false;
        let row = usize::from(self.cursor.row);
        let whole_screen = match mode {
            EraseDisplay::All => true,
            // From home it erases the whole screen, as vt100's `clear`
            // does, unless the top row continues the newest history line:
            // then a shell is redrawing a line that started above the
            // screen (zsh after a narrow made its prompt taller than the
            // window), not clearing (#86).
            EraseDisplay::Below => {
                self.cursor == Point::default() && !self.top_row_continues_history()
            }
            EraseDisplay::Above | EraseDisplay::Scrollback => false,
        };
        match mode {
            EraseDisplay::Below => {
                self.extend_region(self.cursor.row, self.rows - 1);
                self.erase_in_line(EraseLine::ToEnd);
                for line in &mut self.lines[row + 1..] {
                    line.clear(self.pen);
                }
            }
            EraseDisplay::Above => {
                self.extend_region(0, self.cursor.row);
                for line in &mut self.lines[..row] {
                    line.clear(self.pen);
                }
                self.erase_in_line(EraseLine::ToStart);
            }
            EraseDisplay::All => {
                self.damage.extend_all();
                for line in &mut self.lines {
                    line.clear(self.pen);
                }
            }
            EraseDisplay::Scrollback => self.clear_scrollback(),
        }
        if whole_screen && !self.is_alt_screen() {
            self.scrolled_since_clear = false;
            self.end_newest_history_line();
        }
    }

    /// True when the newest scrollback line is wrapped: the top row of the
    /// primary screen continues it.
    fn top_row_continues_history(&self) -> bool {
        self.scrollback
            .len()
            .checked_sub(1)
            .and_then(|newest| self.scrollback.get(newest))
            .is_some_and(|line| line.wrapped)
    }

    /// Ends the newest scrollback line (clears its wrap flag) after the
    /// top row that may have continued it was erased, so the next text on
    /// the top row starts a line of its own (#86). O(1).
    fn end_newest_history_line(&mut self) {
        if let Some(newest) = self.scrollback.newest_mut() {
            newest.wrapped = false;
        }
    }

    /// Returns the grid to its initial state (RIS): primary screen, blank,
    /// home cursor, default pen, full scroll region, default tab stops, both
    /// saved-cursor slots at their initial state and
    /// [`Grid::scrolled_since_clear`] cleared. The alternate screen is left
    /// and the stashed primary contents are discarded. Scrollback and its
    /// cap are kept; the newest history line is ended, as ED 2 does. The
    /// whole screen counts as damaged, and the damage generations carry on
    /// from before the reset.
    ///
    /// The result equals a fresh [`Grid::with_scrollback`] of the same size
    /// and cap with the old scrollback attached, but is built in place: the
    /// rows keep their allocations and nothing is allocated. Rows the damage
    /// tracker reports unchanged since the previous reset are still the
    /// blanks that reset left and are not rewritten, so a flood of RIS costs
    /// O(rows) per reset after the first, not O(rows × cols) (#107).
    pub fn reset(&mut self) {
        let dirty = match self.primary.take() {
            // The alternate screen is active: its rows are not the ones the
            // previous reset blanked. The stashed primary rows are dropped.
            Some(_) => DamageRows::All,
            // A saturated generation no longer moves, so stamps after the
            // last reset could not be told from it.
            None if self.blanked_at.0 == u64::MAX => DamageRows::All,
            None => self.damage.since(self.blanked_at.0, self.cursor.row),
        };
        match dirty {
            DamageRows::None => {}
            DamageRows::Rows(rows) => {
                let rows = usize::from(*rows.start())..=usize::from(*rows.end());
                for line in &mut self.lines[rows] {
                    line.clear(Style::DEFAULT);
                }
            }
            DamageRows::All => {
                for line in &mut self.lines {
                    line.clear(Style::DEFAULT);
                }
            }
        }
        debug_assert!(
            self.lines.iter().all(is_initial_blank),
            "a row changed since the last reset without damage"
        );
        self.cursor = Point::default();
        self.pending_wrap = false;
        self.pen = Style::DEFAULT;
        self.reset_scroll_region();
        self.saved = SavedCursor::INITIAL;
        reset_tabs(&mut self.tabs, self.cols);
        self.scrolled_since_clear = false;
        // The blank screen ends the newest history line, as ED 2 does.
        self.end_newest_history_line();
        // The tracker is kept, so a reader of the old screen sees it all
        // change rather than a grid it does not know.
        self.damage.extend_all();
        self.blanked_at = BlankedAt(self.damage.generation());
    }

    /// Clears the screen to the cursor line ("Clear to Start", ⌘K in
    /// Terminal.app, iTerm2 and kitty): discards the scrollback and, on the
    /// primary screen, the rows above the cursor row.
    ///
    /// The cursor row becomes row 0 and keeps its contents, column and
    /// pending wrap; the rows below it move up with it and default blank
    /// rows fill the bottom. Removed rows are dropped, not pushed into the
    /// (now empty) scrollback. When rows were removed the scroll region
    /// becomes the full screen (its rows no longer hold what it was set
    /// for), the saved cursor moves up with the text (stopping at row 0)
    /// and [`Grid::scrolled_since_clear`] is cleared, since what is left on
    /// the screen never scrolled into history. With the cursor on row 0 only
    /// the scrollback goes, exactly as [`Grid::clear_scrollback`].
    ///
    /// On the alternate screen only the primary scrollback is discarded:
    /// the visible screen belongs to the full-screen application and is not
    /// touched. The whole screen counts as damaged either way.
    pub fn clear_to_cursor(&mut self) {
        self.scrollback.clear();
        self.damage.extend_all();
        let removed = self.cursor.row;
        if self.is_alt_screen() || removed == 0 {
            return;
        }
        let removed_rows = usize::from(removed);
        for line in &mut self.lines[..removed_rows] {
            line.clear(Style::DEFAULT);
        }
        self.lines.rotate_left(removed_rows);
        self.cursor.row = 0;
        self.saved.cursor.row = self.saved.cursor.row.saturating_sub(removed);
        self.reset_scroll_region();
        self.scrolled_since_clear = false;
    }
}

/// The damage tracker's generation right after the last [`Grid::reset`]:
/// rows the tracker reports unchanged since then are still the blanks that
/// reset left. `0` (no reset yet) is below every tracker's range, so
/// [`DamageTracker::since`] answers every row.
///
/// A clone gets `0`: it also gets a fresh tracker, whose range the
/// original's generation does not belong to, so its first reset clears
/// every row.
///
/// Opaque in `Debug`, like the tracker: a reset grid and a fresh one hold
/// the same screen and must print the same.
#[derive(Default)]
struct BlankedAt(u64);

impl Clone for BlankedAt {
    fn clone(&self) -> Self {
        Self(0)
    }
}

impl std::fmt::Debug for BlankedAt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BlankedAt")
    }
}

/// True for a row exactly as [`Grid::reset`] leaves it: default blanks, no
/// marks, clusters or wrap.
fn is_initial_blank(line: &Line) -> bool {
    line.cells.iter().all(|cell| *cell == Cell::BLANK)
        && line.marks.is_empty()
        && line.graphemes.is_empty()
        && !line.wrapped
}

/// Restores the default tab stops (every [`TabStops::DEFAULT_INTERVAL`]
/// columns) over `cols` columns, in place when they already cover `cols`
/// (unlike [`TabStops::reset`], which reallocates), else by rebuilding.
fn reset_tabs(tabs: &mut TabStops, cols: u16) {
    if tabs.cols() != cols {
        *tabs = TabStops::new(cols);
        return;
    }
    tabs.clear_all();
    for col in (0..cols).step_by(usize::from(TabStops::DEFAULT_INTERVAL)) {
        tabs.set(col);
    }
}

/// Printable characters one column wide whose grapheme cluster break
/// property is `Other` (UAX #29) and that are not `Extended_Pictographic`
/// apart from U+00A9 and U+00AE: ASCII, Latin-1 and Latin Extended-A/B,
/// IPA and spacing modifiers, Greek and Cyrillic letters (not U+00AD,
/// which is zero-width, nor the Cyrillic combining marks U+0483..U+0489),
/// Armenian letters U+0531..U+0556 and U+0561..U+0587, Hebrew letters
/// U+05D0..U+05EA, Arabic letters U+0621..U+064A, Georgian letters
/// U+10A0..U+10C5, U+10D0..U+10FA and U+10FC..U+10FF, Latin Extended
/// Additional U+1E00..U+1EFF (Vietnamese), dashes and quotes
/// U+2010..U+2027, box drawing and block elements U+2500..U+259F, and
/// braille U+2800..U+28FF. Two of them never form one grapheme (GB999), so
/// writing one after another needs no segmentation; a unit test checks
/// every pair.
#[inline]
fn is_simple(ch: char) -> bool {
    matches!(
        ch,
        ' '..='~'
            | '\u{A0}'..='\u{2FF}'
            | '\u{370}'..='\u{482}'
            | '\u{48A}'..='\u{52F}'
            | '\u{531}'..='\u{556}'
            | '\u{561}'..='\u{587}'
            | '\u{5D0}'..='\u{5EA}'
            | '\u{621}'..='\u{64A}'
            | '\u{10A0}'..='\u{10C5}'
            | '\u{10D0}'..='\u{10FA}'
            | '\u{10FC}'..='\u{10FF}'
            | '\u{1E00}'..='\u{1EFF}'
            | '\u{2010}'..='\u{2027}'
            | '\u{2500}'..='\u{259F}'
            | '\u{2800}'..='\u{28FF}'
    ) && ch != '\u{AD}'
}

/// Cheap proofs that `ch` cannot join the cluster of `base` (a cell that is
/// not a blank or a spacer), so [`Grid::plan_attach`] can skip
/// segmentation.
///
/// - Two [`is_simple`] characters never form one grapheme (a unit test
///   checks every pair).
/// - A simple character after a lone wide character never forms one
///   grapheme: no two-column scalar is `Prepend`, a UAX #29 conjunct
///   linker or a Hangul or regional-indicator base a simple character
///   could extend, and no simple character is `Extend`, `SpacingMark` or
///   an `InCB=Consonant` (GB999). A unit test checks every two-column
///   scalar of planes 0 and 1 against a sample of simple characters and
///   every simple character against a sample of two-column bases.
/// - A two-column scalar after a lone scalar of nonzero width makes a
///   cluster at least three columns wide (the gate refuses it), unless it
///   is an emoji modifier (U+1F3FB..U+1F3FF), whose modifier sequence is
///   two columns. A zero-width base, which only [`Grid::put_cell`] can
///   write, is excluded. A unit test checks this against the full rule for
///   every two-column scalar of planes 0 and 1 (and a sample of planes 2
///   and 3) after a sample of 16 bases, and a property test after any
///   base.
#[inline]
fn cannot_join(base: &Cell, ch: char) -> bool {
    if base.is_cluster() {
        return false;
    }
    (is_simple(ch) && (is_simple(base.ch) || base.is_wide()))
        || (char_width(ch) == Some(2)
            && !('\u{1F3FB}'..='\u{1F3FF}').contains(&ch)
            && char_width(base.ch) != Some(0))
}

/// Combining Diacritical Marks (U+0300..U+036F): zero-width, grapheme
/// cluster break `Extend`.
#[inline]
fn is_combining_diacritic(ch: char) -> bool {
    ('\u{300}'..='\u{36F}').contains(&ch)
}

/// A cheap proof that `ch` cannot extend the cluster `text`: `ch` is
/// [`is_simple`] but not `Extended_Pictographic` (U+00A9, U+00AE, which may
/// follow a ZWJ), and the cluster ends in an [`is_simple`] character or a
/// combining diacritic, neither of which is `Prepend`, so UAX #29 breaks
/// before `ch` (GB999). A unit test checks it against the full rule.
#[inline]
fn cluster_cannot_take(text: &str, ch: char) -> bool {
    is_simple(ch)
        && !matches!(ch, '\u{A9}' | '\u{AE}')
        && text
            .chars()
            .next_back()
            .is_some_and(|last| is_simple(last) || is_combining_diacritic(last))
}

/// Turns a line that left the screen into a blank line of `cols` cells
/// carrying the background of `pen`, keeping its allocations.
fn recycle(mut line: Line, cols: u16, pen: Style) -> Line {
    line.cells.clear();
    line.cells.resize(usize::from(cols), Cell::blank_with(pen));
    line.marks.clear();
    line.graphemes.clear();
    line.wrapped = false;
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Color, Mark};

    fn text(grid: &Grid) -> Vec<String> {
        grid.visible_lines().iter().map(Line::text).collect()
    }

    fn filled(rows: u16, cols: u16) -> Grid {
        // Row r is filled with the digit r repeated, so rows are
        // distinguishable after scrolling.
        let mut grid = Grid::new(rows, cols);
        for r in 0..rows {
            grid.goto(r, 0);
            let ch = char::from_digit(u32::from(r) % 10, 10).unwrap();
            for _ in 0..cols {
                grid.put_char(ch);
            }
        }
        grid.goto(0, 0);
        grid
    }

    fn red_bg() -> Style {
        Style {
            bg: Color::Indexed(1),
            ..Style::DEFAULT
        }
    }

    #[test]
    fn new_clamps_zero_dimensions() {
        let grid = Grid::new(0, 0);
        assert_eq!((grid.rows(), grid.cols()), (1, 1));
        assert_eq!(grid.visible_lines().len(), 1);
        assert_eq!(grid.visible_lines()[0].cells.len(), 1);
        assert_eq!(grid.scroll_region(), ScrollRegion { top: 0, bottom: 0 });
    }

    #[test]
    fn put_char_advances_and_uses_pen() {
        let mut grid = Grid::new(2, 5);
        grid.set_pen(Style::DEFAULT.with(Style::BOLD));
        grid.put_str("ab");
        assert_eq!(grid.cursor(), Point::new(0, 2));
        assert_eq!(text(&grid), vec!["ab", ""]);
        let cell = grid.cell(Point::new(0, 0)).unwrap();
        assert_eq!(cell.ch, 'a');
        assert!(cell.style.has(Style::BOLD));
        assert_eq!(grid.cell(Point::new(5, 5)), None);
    }

    #[test]
    fn put_char_pending_wrap_then_wraps() {
        let mut grid = Grid::new(2, 3);
        grid.put_str("abc");
        assert_eq!(grid.cursor(), Point::new(0, 2));
        assert!(grid.pending_wrap());
        assert!(!grid.line(0).unwrap().wrapped);
        grid.put_char('d');
        assert_eq!(grid.cursor(), Point::new(1, 1));
        assert!(!grid.pending_wrap());
        assert!(grid.line(0).unwrap().wrapped);
        assert_eq!(text(&grid), vec!["abc", "d"]);
    }

    #[test]
    fn put_char_wrap_at_bottom_scrolls() {
        let mut grid = Grid::new(2, 2);
        grid.put_str("abcde");
        assert_eq!(text(&grid), vec!["cd", "e"]);
        assert_eq!(grid.cursor(), Point::new(1, 1));
        assert_eq!(grid.scrollback_len(), 1);
        assert_eq!(grid.scrollback_line(0).unwrap().text(), "ab");
        assert!(grid.scrollback_line(0).unwrap().wrapped);
    }

    #[test]
    fn cursor_motion_clears_pending_wrap() {
        let mut grid = Grid::new(2, 2);
        grid.put_str("ab");
        assert!(grid.pending_wrap());
        grid.cursor_left(1);
        assert!(!grid.pending_wrap());
        grid.put_str("xy");
        assert!(grid.pending_wrap());
        grid.carriage_return();
        assert!(!grid.pending_wrap());
        assert_eq!(text(&grid), vec!["xy", ""]);
    }

    #[test]
    fn goto_clamps() {
        let mut grid = Grid::new(3, 4);
        grid.goto(10, 10);
        assert_eq!(grid.cursor(), Point::new(2, 3));
        grid.goto(1, 2);
        assert_eq!(grid.cursor(), Point::new(1, 2));
        grid.goto_row(99);
        assert_eq!(grid.cursor(), Point::new(2, 2));
        grid.goto_col(99);
        assert_eq!(grid.cursor(), Point::new(2, 3));
        grid.goto_col(0);
        assert_eq!(grid.cursor(), Point::new(2, 0));
    }

    #[test]
    fn relative_moves_clamp_to_screen() {
        let mut grid = Grid::new(3, 4);
        grid.cursor_up(5);
        assert_eq!(grid.cursor(), Point::new(0, 0));
        grid.cursor_left(5);
        assert_eq!(grid.cursor(), Point::new(0, 0));
        grid.cursor_down(1);
        grid.cursor_right(2);
        assert_eq!(grid.cursor(), Point::new(1, 2));
        grid.cursor_down(u16::MAX);
        grid.cursor_right(u16::MAX);
        assert_eq!(grid.cursor(), Point::new(2, 3));
        grid.cursor_up(1);
        grid.cursor_left(1);
        assert_eq!(grid.cursor(), Point::new(1, 2));
    }

    #[test]
    fn relative_moves_respect_region_when_inside() {
        let mut grid = Grid::new(6, 2);
        grid.set_scroll_region(2, 3);
        grid.goto(2, 0);
        grid.cursor_up(5);
        assert_eq!(grid.cursor().row, 2);
        grid.cursor_down(5);
        assert_eq!(grid.cursor().row, 3);
        // Below the region: up stops at the region top, down at the screen.
        grid.goto(5, 0);
        grid.cursor_up(10);
        assert_eq!(grid.cursor().row, 2);
        grid.goto(5, 0);
        grid.cursor_down(10);
        assert_eq!(grid.cursor().row, 5);
        // Above the region: up stops at the screen, down at the region bottom.
        grid.goto(1, 0);
        grid.cursor_up(10);
        assert_eq!(grid.cursor().row, 0);
        grid.goto(1, 0);
        grid.cursor_down(10);
        assert_eq!(grid.cursor().row, 3);
    }

    #[test]
    fn line_feed_and_reverse_index_scroll_at_region_edges() {
        let mut grid = filled(4, 1);
        grid.set_scroll_region(1, 2);
        grid.goto(2, 0);
        grid.line_feed();
        assert_eq!(grid.cursor().row, 2);
        assert_eq!(text(&grid), vec!["0", "2", "", "3"]);
        assert_eq!(grid.scrollback_len(), 0);
        grid.goto(1, 0);
        grid.reverse_index();
        assert_eq!(grid.cursor().row, 1);
        assert_eq!(text(&grid), vec!["0", "", "2", "3"]);
        // Outside the region: plain motion, clamped to the screen.
        grid.goto(3, 0);
        grid.line_feed();
        assert_eq!(grid.cursor().row, 3);
        grid.goto(0, 0);
        grid.reverse_index();
        assert_eq!(grid.cursor().row, 0);
        grid.goto(0, 0);
        grid.line_feed();
        assert_eq!(grid.cursor().row, 1);
        assert_eq!(text(&grid), vec!["0", "", "2", "3"]);
    }

    #[test]
    fn full_screen_scroll_fills_scrollback_up_to_limit() {
        let mut grid = Grid::with_scrollback(2, 1, 2);
        grid.goto(1, 0);
        for ch in ['a', 'b', 'c', 'd'] {
            grid.put_char(ch);
            grid.carriage_return();
            grid.line_feed();
        }
        assert_eq!(grid.scrollback_len(), 2);
        assert_eq!(grid.scrollback_line(0).unwrap().text(), "b");
        assert_eq!(grid.scrollback_line(1).unwrap().text(), "c");
        assert_eq!(text(&grid), vec!["d", ""]);
        let mut none = Grid::with_scrollback(2, 1, 0);
        none.goto(1, 0);
        none.line_feed();
        assert_eq!(none.scrollback_len(), 0);
    }

    #[test]
    fn scroll_up_and_down_within_region() {
        let mut grid = filled(5, 1);
        grid.set_scroll_region(1, 3);
        grid.scroll_up(1);
        assert_eq!(text(&grid), vec!["0", "2", "3", "", "4"]);
        grid.scroll_down(2);
        assert_eq!(text(&grid), vec!["0", "", "", "2", "4"]);
        grid.scroll_up(100);
        assert_eq!(text(&grid), vec!["0", "", "", "", "4"]);
        grid.scroll_down(0);
        assert_eq!(text(&grid), vec!["0", "", "", "", "4"]);
        assert_eq!(grid.scrollback_len(), 0);
    }

    #[test]
    fn scroll_uses_pen_background() {
        let mut grid = filled(2, 1);
        grid.set_pen(red_bg());
        grid.scroll_up(1);
        assert_eq!(
            grid.cell(Point::new(1, 0)).unwrap().style.bg,
            Color::Indexed(1)
        );
        assert_eq!(grid.cell(Point::new(1, 0)).unwrap().ch, ' ');
    }

    #[test]
    fn set_scroll_region_clamps_and_rejects_degenerate() {
        let mut grid = Grid::new(5, 1);
        grid.set_scroll_region(1, 99);
        assert_eq!(grid.scroll_region(), ScrollRegion { top: 1, bottom: 4 });
        grid.set_scroll_region(3, 3);
        assert_eq!(grid.scroll_region(), ScrollRegion { top: 0, bottom: 4 });
        grid.set_scroll_region(2, 3);
        grid.set_scroll_region(4, 1);
        assert_eq!(grid.scroll_region(), ScrollRegion { top: 0, bottom: 4 });
        grid.set_scroll_region(2, 3);
        grid.reset_scroll_region();
        assert_eq!(grid.scroll_region(), ScrollRegion { top: 0, bottom: 4 });
        assert_eq!(grid.scroll_region().height(), 5);
    }

    #[test]
    fn insert_lines_within_region() {
        let mut grid = filled(5, 1);
        grid.set_scroll_region(1, 3);
        grid.goto(2, 0);
        grid.insert_lines(1);
        assert_eq!(text(&grid), vec!["0", "1", "", "2", "4"]);
        assert_eq!(grid.cursor(), Point::new(2, 0));
        grid.insert_lines(99);
        assert_eq!(text(&grid), vec!["0", "1", "", "", "4"]);
        grid.goto(4, 0);
        grid.insert_lines(1);
        assert_eq!(text(&grid), vec!["0", "1", "", "", "4"]);
        grid.goto(0, 0);
        grid.insert_lines(0);
        assert_eq!(text(&grid), vec!["0", "1", "", "", "4"]);
    }

    #[test]
    fn delete_lines_within_region() {
        let mut grid = filled(5, 1);
        grid.set_scroll_region(1, 3);
        grid.goto(1, 0);
        grid.delete_lines(1);
        assert_eq!(text(&grid), vec!["0", "2", "3", "", "4"]);
        grid.delete_lines(99);
        assert_eq!(text(&grid), vec!["0", "", "", "", "4"]);
        grid.goto(0, 0);
        grid.delete_lines(1);
        assert_eq!(text(&grid), vec!["0", "", "", "", "4"]);
    }

    #[test]
    fn insert_delete_erase_chars() {
        let mut grid = Grid::new(1, 6);
        grid.put_str("abcdef");
        grid.goto(0, 2);
        grid.insert_chars(2);
        assert_eq!(text(&grid), vec!["ab  cd"]);
        grid.delete_chars(1);
        assert_eq!(text(&grid), vec!["ab cd"]);
        grid.goto(0, 0);
        grid.put_str("abcdef");
        grid.goto(0, 1);
        grid.erase_chars(2);
        assert_eq!(text(&grid), vec!["a  def"]);
        grid.erase_chars(u16::MAX);
        assert_eq!(text(&grid), vec!["a"]);
        grid.goto(0, 0);
        grid.put_str("abcdef");
        grid.goto(0, 4);
        grid.insert_chars(u16::MAX);
        assert_eq!(text(&grid), vec!["abcd"]);
        grid.goto(0, 0);
        grid.delete_chars(u16::MAX);
        assert_eq!(text(&grid), vec![""]);
        grid.insert_chars(0);
        grid.delete_chars(0);
        assert_eq!(grid.cursor(), Point::new(0, 0));
    }

    #[test]
    fn erase_in_line_modes() {
        let mut grid = Grid::new(1, 5);
        grid.put_str("abcde");
        assert!(grid.pending_wrap());
        grid.goto(0, 2);
        grid.erase_in_line(EraseLine::ToEnd);
        assert_eq!(text(&grid), vec!["ab"]);
        grid.goto(0, 0);
        grid.put_str("abcde");
        grid.goto(0, 2);
        grid.erase_in_line(EraseLine::ToStart);
        assert_eq!(text(&grid), vec!["   de"]);
        grid.set_pen(red_bg());
        grid.erase_in_line(EraseLine::All);
        assert_eq!(text(&grid), vec![""]);
        assert!(!grid.pending_wrap());
        assert!(
            grid.visible_lines()[0]
                .cells
                .iter()
                .all(|c| c.style.bg == Color::Indexed(1))
        );
    }

    #[test]
    fn erase_in_line_keeps_marks_clears_wrap_on_end() {
        let mut grid = Grid::new(2, 2);
        grid.put_str("abc");
        grid.line_mut(0).unwrap().marks.push(Mark::PromptStart);
        assert!(grid.line(0).unwrap().wrapped);
        grid.goto(0, 1);
        grid.erase_in_line(EraseLine::ToStart);
        assert!(grid.line(0).unwrap().wrapped);
        grid.erase_in_line(EraseLine::ToEnd);
        assert!(!grid.line(0).unwrap().wrapped);
        assert_eq!(grid.line(0).unwrap().marks, vec![Mark::PromptStart]);
    }

    #[test]
    fn erase_in_display_modes() {
        let mut grid = filled(3, 3);
        grid.goto(1, 1);
        grid.erase_in_display(EraseDisplay::Below);
        assert_eq!(text(&grid), vec!["000", "1", ""]);
        let mut grid = filled(3, 3);
        grid.goto(1, 1);
        grid.erase_in_display(EraseDisplay::Above);
        assert_eq!(text(&grid), vec!["", "  1", "222"]);
        let mut grid = filled(3, 3);
        grid.line_mut(2).unwrap().marks.push(Mark::OutputStart);
        grid.goto(1, 1);
        grid.erase_in_display(EraseDisplay::All);
        assert_eq!(text(&grid), vec!["", "", ""]);
        assert_eq!(grid.line(2).unwrap().marks, Vec::<Mark>::new());
        assert_eq!(grid.cursor(), Point::new(1, 1));
    }

    #[test]
    fn erase_in_display_at_edges() {
        let mut grid = filled(2, 2);
        grid.goto(1, 1);
        grid.erase_in_display(EraseDisplay::Below);
        assert_eq!(text(&grid), vec!["00", "1"]);
        grid.goto(0, 0);
        grid.erase_in_display(EraseDisplay::Above);
        assert_eq!(text(&grid), vec![" 0", "1"]);
    }

    #[test]
    fn erase_scrollback_only() {
        let mut grid = Grid::new(1, 1);
        grid.put_char('a');
        grid.line_feed();
        assert_eq!(grid.scrollback_len(), 1);
        grid.put_char('b');
        grid.erase_in_display(EraseDisplay::Scrollback);
        assert_eq!(grid.scrollback_len(), 0);
        assert_eq!(text(&grid), vec!["b"]);
    }

    #[test]
    fn save_restore_cursor() {
        let mut grid = Grid::new(3, 3);
        grid.restore_cursor();
        assert_eq!(grid.cursor(), Point::new(0, 0));
        grid.set_pen(red_bg());
        grid.put_str("abc");
        assert!(grid.pending_wrap());
        grid.save_cursor();
        grid.goto(2, 0);
        grid.set_pen(Style::DEFAULT);
        grid.restore_cursor();
        assert_eq!(grid.cursor(), Point::new(0, 2));
        assert_eq!(grid.pen(), red_bg());
        assert!(grid.pending_wrap());
        grid.put_char('d');
        assert_eq!(text(&grid), vec!["abc", "d", ""]);
    }

    #[test]
    fn reset_saved_cursor_keeps_the_live_cursor() {
        let mut grid = Grid::new(3, 3);
        grid.set_pen(red_bg());
        grid.goto(1, 1);
        grid.save_cursor();
        grid.put_str("ab");
        assert!(grid.pending_wrap());
        grid.reset_saved_cursor();
        assert_eq!(grid.cursor(), Point::new(1, 2), "live cursor kept");
        assert!(grid.pending_wrap(), "live pending wrap kept");
        assert_eq!(grid.pen(), red_bg(), "live pen kept");
        grid.restore_cursor();
        assert_eq!(grid.cursor(), Point::new(0, 0));
        assert_eq!(grid.pen(), Style::DEFAULT);
        assert!(!grid.pending_wrap());
    }

    #[test]
    fn reset_saved_cursor_leaves_the_other_screens_slot() {
        let mut grid = Grid::new(3, 3);
        grid.goto(2, 2);
        grid.save_cursor();
        grid.enter_alt_screen();
        grid.goto(1, 1);
        grid.save_cursor();
        grid.reset_saved_cursor();
        grid.restore_cursor();
        assert_eq!(grid.cursor(), Point::new(0, 0), "alternate slot reset");
        grid.leave_alt_screen();
        grid.restore_cursor();
        assert_eq!(grid.cursor(), Point::new(2, 2), "primary slot kept");
    }

    #[test]
    fn tab_stops_move_cursor() {
        let mut grid = Grid::new(1, 20);
        grid.tab_forward(1);
        assert_eq!(grid.cursor().col, 8);
        grid.tab_forward(5);
        assert_eq!(grid.cursor().col, 19);
        grid.tab_backward(1);
        assert_eq!(grid.cursor().col, 16);
        grid.tab_backward(9);
        assert_eq!(grid.cursor().col, 0);
        grid.goto(0, 3);
        grid.set_tab_stop();
        assert!(grid.tabs().is_set(3));
        grid.goto(0, 0);
        grid.tab_forward(1);
        assert_eq!(grid.cursor().col, 3);
        grid.clear_tab_stop();
        grid.goto(0, 0);
        grid.tab_forward(1);
        assert_eq!(grid.cursor().col, 8);
        grid.clear_all_tab_stops();
        grid.goto(0, 0);
        grid.tab_forward(1);
        assert_eq!(grid.cursor().col, 19);
        grid.tab_forward(0);
        grid.tab_backward(0);
        assert_eq!(grid.cursor().col, 19);
        grid.tabs_mut().reset();
        grid.goto(0, 0);
        grid.tab_forward(2);
        assert_eq!(grid.cursor().col, 16);
    }

    #[test]
    fn tab_clears_pending_wrap() {
        let mut grid = Grid::new(1, 2);
        grid.put_str("ab");
        assert!(grid.pending_wrap());
        grid.tab_forward(1);
        assert!(!grid.pending_wrap());
        assert_eq!(grid.cursor().col, 1);
    }

    #[test]
    fn reset_keeps_size_and_scrollback() {
        let mut grid = filled(3, 2);
        grid.line_feed();
        grid.goto(2, 1);
        grid.line_feed();
        assert_eq!(grid.scrollback_len(), 1);
        grid.set_pen(red_bg());
        grid.set_scroll_region(0, 1);
        grid.set_tab_stop();
        grid.save_cursor();
        grid.reset();
        assert_eq!((grid.rows(), grid.cols()), (3, 2));
        assert_eq!(grid.scrollback_len(), 1);
        assert_eq!(text(&grid), vec!["", "", ""]);
        assert_eq!(grid.cursor(), Point::new(0, 0));
        assert_eq!(grid.pen(), Style::DEFAULT);
        assert_eq!(grid.scroll_region(), ScrollRegion { top: 0, bottom: 2 });
        assert_eq!(grid.tabs(), &TabStops::new(2));
        grid.restore_cursor();
        assert_eq!(grid.cursor(), Point::new(0, 0));
    }

    /// Every [`is_simple`] character lies below this code point; the pair
    /// tests below enumerate it all.
    const SIMPLE_DOMAIN: u32 = 0x3000;

    /// Every [`is_simple`] character is one column wide and no two of them
    /// form one grapheme, so the plain-text fast path of `put_char` and the
    /// first rule of [`cannot_join`] never skip an attach.
    #[test]
    fn simple_characters_are_narrow_and_never_join() {
        use unicode_segmentation::UnicodeSegmentation;
        assert!(
            (SIMPLE_DOMAIN..=0x10_FFFF)
                .filter_map(char::from_u32)
                .all(|c| !is_simple(c)),
            "widen SIMPLE_DOMAIN to keep the pair checks exhaustive"
        );
        let simple: Vec<char> = (0..SIMPLE_DOMAIN)
            .filter_map(char::from_u32)
            .filter(|&c| is_simple(c))
            .collect();
        assert!(simple.len() > 1300, "{} simple characters", simple.len());
        let mut pair = String::new();
        for &a in &simple {
            assert_eq!(char_width(a), Some(1), "{a:?}");
            for &b in &simple {
                pair.clear();
                pair.push(a);
                pair.push(b);
                assert_eq!(
                    pair.graphemes(true).nth(1),
                    Some(&*b.to_string()),
                    "{a:?} {b:?}"
                );
            }
        }
    }

    /// Bases for the [`cannot_join`] checks: narrow and wide, emoji and
    /// not, modifier bases, Hangul jamo, Prepend (one wide and one zero
    /// width), a regional indicator.
    const BASES: &[char] = &[
        'a', 'é', '中', '👍', '👨', '☺', '⌚', '🇸', 'ᄀ', '가', 'क', '\u{600}', '\u{70F}',
        '\u{A0}', '🏳', '1',
    ];

    /// The plan the full rule makes for `ch` after a lone `base`. A base
    /// that is not two columns wide is written with `put_cell`, which is
    /// how a zero-width scalar can end up in a cell.
    fn slow_plan(base: char, ch: char) -> Option<Attach> {
        let mut grid = Grid::new(1, 6);
        if char_width(base) == Some(2) {
            grid.put_char(base);
        } else {
            grid.put_cell(Cell::new(base, Style::DEFAULT));
        }
        let line = &grid.lines[0];
        grid.plan_attach_slow(line, 0, line.cells[0], ch)
    }

    #[test]
    fn cannot_join_never_contradicts_the_full_rule() {
        // Every two-column scalar outside the ideograph planes, plus a
        // sample of those.
        let wide = (0..0x2_0000)
            .chain((0x2_0000..0x4_0000).step_by(97))
            .filter_map(char::from_u32)
            .filter(|&c| char_width(c) == Some(2));
        let mut checked = 0;
        for ch in wide {
            for &base in BASES {
                let cell = Cell::new(base, Style::DEFAULT);
                if cannot_join(&cell, ch) {
                    checked += 1;
                    assert_eq!(slow_plan(base, ch), None, "{base:?} + {ch:?}");
                }
            }
        }
        assert!(checked > 100_000, "{checked} rejections checked");
        assert!(!cannot_join(&Cell::new('👍', Style::DEFAULT), '🏽'));
    }

    #[test]
    fn diacritic_fast_accept_matches_the_full_rule() {
        let simple = (0..SIMPLE_DOMAIN)
            .filter_map(char::from_u32)
            .filter(|&c| is_simple(c) && c != ' ');
        for base in simple {
            for mark in ('\u{300}'..='\u{36F}').filter(|&c| is_combining_diacritic(c)) {
                assert_eq!(
                    slow_plan(base, mark),
                    Some(Attach { col: 0, width: 1 }),
                    "{base:?} + {mark:?}"
                );
            }
        }
    }

    /// The plan the full rule makes for `ch` after a lone wide `base`,
    /// with the base cell as the grid wrote it (`WIDE`).
    fn wide_plan(base: char, ch: char) -> (Cell, Option<Attach>) {
        let mut grid = Grid::new(1, 6);
        grid.put_char(base);
        let line = &grid.lines[0];
        let cell = line.cells[0];
        (cell, grid.plan_attach_slow(line, 0, cell, ch))
    }

    #[test]
    fn simple_after_a_wide_base_matches_the_full_rule() {
        let simple: Vec<char> = (0..SIMPLE_DOMAIN)
            .filter_map(char::from_u32)
            .filter(|&c| is_simple(c))
            .collect();
        let wide: Vec<char> = (0..0x2_0000)
            .chain((0x2_0000..0x4_0000).step_by(97))
            .filter_map(char::from_u32)
            .filter(|&c| char_width(c) == Some(2))
            .collect();
        let few_simple = [
            'a', ' ', '\u{A9}', '\u{AE}', '\u{E9}', '\u{5D0}', '\u{627}', '\u{1EA1}', '\u{201C}',
            '\u{2500}',
        ];
        let few_wide = [
            '\u{4E2D}',
            '\u{1F600}',
            '\u{1F44D}',
            '\u{1100}',
            '\u{AC00}',
            '\u{231A}',
            '\u{1F201}',
            '\u{FF21}',
            '\u{1F3FD}',
            '\u{3000}',
            '\u{115F}',
        ];
        let pairs = wide
            .iter()
            .flat_map(|&b| few_simple.map(|c| (b, c)))
            .chain(simple.iter().flat_map(|&c| few_wide.map(|b| (b, c))));
        let mut checked = 0;
        for (base, ch) in pairs {
            let (cell, plan) = wide_plan(base, ch);
            assert!(cell.is_wide(), "{base:?} is written wide");
            assert!(cannot_join(&cell, ch), "{base:?} + {ch:?} takes the rule");
            assert_eq!(plan, None, "{base:?} + {ch:?}");
            checked += 1;
        }
        assert!(checked > 500_000, "{checked} pairs checked");
    }

    #[test]
    fn cluster_cannot_take_matches_the_full_rule() {
        use unicode_segmentation::UnicodeSegmentation;
        let candidates = || {
            (0..SIMPLE_DOMAIN)
                .filter_map(char::from_u32)
                .filter(|&c| is_simple(c) || is_combining_diacritic(c))
        };
        // The rule only looks at the classes of the cluster's last scalar
        // and of `ch`: try every tail against a few followers and every
        // follower against a few tails, after clusters of several shapes.
        let few = [
            'a', ' ', '\u{E9}', '\u{A9}', '\u{301}', '\u{36F}', '\u{3B1}', '\u{44F}',
        ];
        let pairs = candidates()
            .flat_map(|tail| few.map(|ch| (tail, ch)))
            .chain(candidates().flat_map(|ch| few.map(|tail| (tail, ch))));
        let mut text = String::new();
        let mut checked = 0;
        for (tail, ch) in pairs {
            for head in ["a\u{301}", "\u{1F468}\u{200D}", "\u{1F1F8}", "\u{600}"] {
                text.clear();
                text.push_str(head);
                text.push(tail);
                if cluster_cannot_take(&text, ch) {
                    checked += 1;
                    let joined = format!("{text}{ch}");
                    let last = joined.graphemes(true).next_back();
                    assert_eq!(last, Some(&*ch.to_string()), "{text:?} + {ch:?}");
                }
            }
        }
        assert!(checked > 10_000, "{checked} rejections checked");
        assert!(!cluster_cannot_take("\u{1F468}\u{200D}", '\u{A9}'));
    }

    proptest::proptest! {
        #[test]
        fn cannot_join_implies_the_full_rule_refuses(
            base in proptest::prelude::any::<char>(),
            ch in proptest::prelude::any::<char>(),
        ) {
            let cell = Cell::new(base, Style::DEFAULT);
            let placeable = char_width(base).is_some() && base != ' ';
            if placeable && cannot_join(&cell, ch) {
                proptest::prop_assert_eq!(slow_plan(base, ch), None);
            }
        }
    }

    /// What `reset` did before #107: a fresh grid of the same size and
    /// cap with the old scrollback attached and its newest line ended.
    fn rebuilt(old: &Grid) -> Grid {
        let mut fresh = Grid::with_scrollback(old.rows, old.cols, old.scrollback.cap());
        fresh.scrollback = old.scrollback.clone();
        fresh.end_newest_history_line();
        fresh
    }

    /// Operations that leave state `reset` must undo or keep.
    #[derive(Debug, Clone)]
    enum ResetOp {
        Put(char),
        Goto(u16, u16),
        Lf,
        Ri,
        Cr,
        Pen(u8),
        Save,
        Restore,
        SetTab,
        ClearTabs,
        Region(u16, u16),
        ScrollUp(u16),
        ScrollDown(u16),
        InsertLines(u16),
        DeleteLines(u16),
        InsertChars(u16),
        DeleteChars(u16),
        EraseLine(u8),
        EraseDisplay(u8),
        PadLastColumn,
        EnterAlt,
        LeaveAlt,
        Resize(u16, u16),
        Cap(usize),
        Mark(u16),
        Wrap(u16),
        TakeDamage,
        Clone,
        ClearToCursor,
        Reset,
    }

    fn reset_op() -> impl proptest::strategy::Strategy<Value = ResetOp> {
        use proptest::prelude::*;
        let n = || prop_oneof![4 => 0..=4u16, 1 => 0..=12u16];
        prop_oneof![
            12 => prop::sample::select(vec![
                'a', 'b', ' ', 'é', '中', '😀', '\u{301}', '\u{FE0F}', '\u{200D}', '\u{1F1F8}',
            ])
            .prop_map(ResetOp::Put),
            3 => (n(), n()).prop_map(|(r, c)| ResetOp::Goto(r, c)),
            4 => Just(ResetOp::Lf),
            1 => Just(ResetOp::Ri),
            1 => Just(ResetOp::Cr),
            2 => any::<u8>().prop_map(ResetOp::Pen),
            1 => Just(ResetOp::Save),
            1 => Just(ResetOp::Restore),
            1 => Just(ResetOp::SetTab),
            1 => Just(ResetOp::ClearTabs),
            2 => (n(), n()).prop_map(|(t, b)| ResetOp::Region(t, b)),
            1 => n().prop_map(ResetOp::ScrollUp),
            1 => n().prop_map(ResetOp::ScrollDown),
            1 => n().prop_map(ResetOp::InsertLines),
            1 => n().prop_map(ResetOp::DeleteLines),
            1 => n().prop_map(ResetOp::InsertChars),
            1 => n().prop_map(ResetOp::DeleteChars),
            1 => (0..3u8).prop_map(ResetOp::EraseLine),
            1 => (0..4u8).prop_map(ResetOp::EraseDisplay),
            1 => Just(ResetOp::PadLastColumn),
            2 => Just(ResetOp::EnterAlt),
            1 => Just(ResetOp::LeaveAlt),
            1 => (1..=7u16, 1..=9u16).prop_map(|(r, c)| ResetOp::Resize(r, c)),
            1 => prop_oneof![Just(0usize), Just(1), Just(3), Just(100)].prop_map(ResetOp::Cap),
            1 => n().prop_map(ResetOp::Mark),
            1 => n().prop_map(ResetOp::Wrap),
            2 => Just(ResetOp::TakeDamage),
            1 => Just(ResetOp::Clone),
            1 => Just(ResetOp::ClearToCursor),
            4 => Just(ResetOp::Reset),
        ]
    }

    /// Applies `op`; for [`ResetOp::Reset`], checks the result against
    /// [`rebuilt`].
    fn apply_reset_op(
        grid: &mut Grid,
        op: &ResetOp,
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        use proptest::{prop_assert, prop_assert_eq};
        let erase_line = [EraseLine::ToEnd, EraseLine::ToStart, EraseLine::All];
        let erase_display = [
            EraseDisplay::Below,
            EraseDisplay::Above,
            EraseDisplay::All,
            EraseDisplay::Scrollback,
        ];
        match *op {
            ResetOp::Put(ch) => grid.put_char(ch),
            ResetOp::Goto(r, c) => grid.goto(r, c),
            ResetOp::Lf => grid.line_feed(),
            ResetOp::Ri => grid.reverse_index(),
            ResetOp::Cr => grid.carriage_return(),
            ResetOp::Pen(v) => {
                grid.set_pen(Style {
                    fg: Color::Indexed(v),
                    bg: Color::Indexed(v.wrapping_mul(7)),
                    flags: u16::from(v),
                    ..Style::DEFAULT
                });
            }
            ResetOp::Save => grid.save_cursor(),
            ResetOp::Restore => grid.restore_cursor(),
            ResetOp::SetTab => grid.set_tab_stop(),
            ResetOp::ClearTabs => grid.clear_all_tab_stops(),
            ResetOp::Region(t, b) => grid.set_scroll_region(t, b),
            ResetOp::ScrollUp(n) => grid.scroll_up(n),
            ResetOp::ScrollDown(n) => grid.scroll_down(n),
            ResetOp::InsertLines(n) => grid.insert_lines(n),
            ResetOp::DeleteLines(n) => grid.delete_lines(n),
            ResetOp::InsertChars(n) => grid.insert_chars(n),
            ResetOp::DeleteChars(n) => grid.delete_chars(n),
            ResetOp::EraseLine(m) => grid.erase_in_line(erase_line[usize::from(m)]),
            ResetOp::EraseDisplay(m) => grid.erase_in_display(erase_display[usize::from(m)]),
            ResetOp::PadLastColumn => grid.pad_last_column(),
            ResetOp::EnterAlt => grid.enter_alt_screen(),
            ResetOp::LeaveAlt => grid.leave_alt_screen(),
            ResetOp::Resize(r, c) => grid.resize(r, c),
            ResetOp::Cap(cap) => grid.set_max_scrollback(cap),
            ResetOp::Mark(row) => {
                if let Some(line) = grid.line_mut(row) {
                    line.marks.push(Mark::PromptStart);
                }
            }
            ResetOp::Wrap(row) => {
                if let Some(line) = grid.line_mut(row) {
                    line.wrapped = true;
                }
            }
            ResetOp::TakeDamage => drop(grid.take_damage()),
            ResetOp::Clone => *grid = grid.clone(),
            ResetOp::ClearToCursor => grid.clear_to_cursor(),
            ResetOp::Reset => {
                let expected = rebuilt(grid);
                let generation = grid.damage_generation();
                grid.reset();
                prop_assert_eq!(format!("{grid:?}"), format!("{expected:?}"));
                // `Debug` shows the scrollback's length only.
                prop_assert!(grid.logical_lines().eq(expected.logical_lines()));
                prop_assert!(grid.damage_generation() > generation);
                prop_assert_eq!(grid.damage().rows, DamageRows::All);
            }
        }
        Ok(())
    }

    proptest::proptest! {
        /// `reset` in place equals the rebuild it replaced (#107), after
        /// any history: alternate screen active, regions, saved cursors,
        /// tab stops, marks, pens, caps, clones, taken damage, and resets
        /// in a row with edits in between (the rows it skips).
        #[test]
        fn reset_in_place_equals_a_rebuilt_grid(
            rows in 1..=7u16,
            cols in 1..=9u16,
            cap in proptest::prop_oneof![proptest::strategy::Just(0usize), 0..=6usize],
            ops in proptest::collection::vec(reset_op(), 0..80),
        ) {
            let mut grid = Grid::with_scrollback(rows, cols, cap);
            for op in &ops {
                apply_reset_op(&mut grid, op)?;
            }
            apply_reset_op(&mut grid, &ResetOp::Reset)?;
        }
    }

    #[test]
    fn reset_rewrites_only_rows_changed_since_the_last_reset() {
        let mut grid = Grid::new(4, 3);
        grid.put_str("abc");
        grid.reset();
        let since = grid.damage_generation();
        grid.goto(2, 0);
        grid.put_char('x');
        // Rows 0 (the cursor left it) and 2 changed; 1 and 3 did not.
        assert_eq!(grid.damage_since(since), DamageRows::Rows(0..=2));
        grid.reset();
        assert_eq!(text(&grid), vec!["", "", "", ""]);
    }

    /// Rows unchanged since the last reset are not rewritten (#107): a
    /// change that bypasses the damage bookkeeping (no primitive makes one)
    /// survives the next reset, and debug builds catch it.
    #[test]
    fn reset_skips_rows_unchanged_since_the_last_reset() {
        let mut grid = Grid::new(4, 3);
        grid.reset();
        grid.lines[2].cells[1] = Cell::new('x', Style::DEFAULT);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            grid.reset();
            grid
        }));
        if cfg!(debug_assertions) {
            assert!(outcome.is_err(), "the debug check reports the stale row");
        } else {
            let grid = outcome.expect("no check in release");
            assert_eq!(grid.lines[2].cells[1].ch, 'x', "the row was skipped");
        }
    }

    /// A clone has its own tracker, so its first reset clears every row
    /// instead of trusting the original's record (#107).
    #[test]
    fn a_clones_first_reset_clears_every_row() {
        let mut grid = Grid::new(4, 3);
        grid.reset();
        let mut clone = grid.clone();
        assert_eq!(clone.blanked_at.0, 0);
        clone.lines[2].cells[1] = Cell::new('x', Style::DEFAULT);
        clone.reset();
        assert!(clone.lines.iter().all(is_initial_blank));
        assert_ne!(clone.blanked_at.0, 0, "the clone's own reset is recorded");
    }

    #[test]
    fn reset_restores_tab_stops_of_the_grid_width() {
        let mut grid = Grid::new(2, 20);
        grid.clear_all_tab_stops();
        grid.reset();
        assert_eq!(grid.tabs(), &TabStops::new(20));
        // A tab-stop table of another width (only possible through
        // `tabs_mut`) is rebuilt at the grid's width.
        *grid.tabs_mut() = TabStops::new(5);
        grid.reset();
        assert_eq!(grid.tabs(), &TabStops::new(20));
    }

    #[test]
    fn reset_on_the_alternate_screen_drops_both_screens() {
        let mut grid = filled(3, 2);
        grid.reset();
        grid.enter_alt_screen();
        grid.put_str("zz");
        grid.reset();
        assert!(!grid.is_alt_screen());
        assert_eq!(text(&grid), vec!["", "", ""]);
    }

    /// Writes `lines` one per row from the cursor, with CR LF between them
    /// (and none after the last).
    fn write_lines(grid: &mut Grid, lines: &[&str]) {
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                grid.carriage_return();
                grid.line_feed();
            }
            grid.put_str(line);
        }
    }

    #[test]
    fn clear_to_cursor_moves_the_cursor_line_to_the_top() {
        let mut grid = Grid::new(4, 3);
        write_lines(&mut grid, &["a", "b", "c", "d", "e", "f"]);
        assert_eq!(grid.scrollback_len(), 2);
        assert_eq!(text(&grid), vec!["c", "d", "e", "f"]);
        grid.set_scroll_region(1, 2);
        grid.goto(2, 1);
        grid.save_cursor();
        grid.goto(1, 1);
        grid.put_char('y');
        let since = grid.damage_generation();
        grid.clear_to_cursor();
        assert_eq!(grid.scrollback_len(), 0);
        assert_eq!(text(&grid), vec!["dy", "e", "f", ""]);
        assert_eq!(grid.cursor(), Point::new(0, 2));
        assert_eq!(grid.scroll_region(), ScrollRegion { top: 0, bottom: 3 });
        assert!(!grid.scrolled_since_clear());
        assert_eq!(grid.damage_since(since), DamageRows::All);
        assert!(is_initial_blank(&grid.visible_lines()[3]));
        grid.restore_cursor();
        assert_eq!(
            grid.cursor(),
            Point::new(1, 1),
            "saved row moved up with the text"
        );
    }

    #[test]
    fn clear_to_cursor_keeps_a_pending_wrap() {
        let mut grid = Grid::new(4, 2);
        write_lines(&mut grid, &["p", "qR", "s"]);
        grid.goto(1, 1);
        grid.put_char('R');
        assert!(grid.pending_wrap());
        grid.clear_to_cursor();
        assert_eq!(text(&grid), vec!["qR", "s", "", ""]);
        assert!(grid.pending_wrap());
        assert_eq!(grid.cursor(), Point::new(0, 1));
    }

    #[test]
    fn clear_to_cursor_clamps_a_saved_cursor_above_it_to_row_zero() {
        let mut grid = Grid::new(4, 2);
        grid.save_cursor();
        grid.goto(3, 0);
        grid.clear_to_cursor();
        grid.goto(2, 1);
        grid.restore_cursor();
        assert_eq!(grid.cursor(), Point::new(0, 0));
    }

    #[test]
    fn clear_to_cursor_on_row_zero_only_drops_the_scrollback() {
        let mut grid = Grid::new(3, 2);
        write_lines(&mut grid, &["a", "b", "c", "d"]);
        grid.set_scroll_region(1, 2);
        grid.goto(0, 1);
        let before = format!("{:?}", grid.visible_lines());
        assert!(grid.scrolled_since_clear());
        grid.clear_to_cursor();
        assert_eq!(grid.scrollback_len(), 0);
        assert_eq!(format!("{:?}", grid.visible_lines()), before);
        assert_eq!(grid.scroll_region(), ScrollRegion { top: 1, bottom: 2 });
        assert!(
            grid.scrolled_since_clear(),
            "the screen still shows scrolled output"
        );
    }

    #[test]
    fn clear_to_cursor_on_the_alternate_screen_keeps_the_screen() {
        let mut grid = Grid::new(3, 2);
        write_lines(&mut grid, &["a", "b", "c", "d"]);
        grid.enter_alt_screen();
        write_lines(&mut grid, &["x", "y"]);
        let before = format!("{grid:?}");
        assert!(before.contains("len: 1"));
        grid.clear_to_cursor();
        assert_eq!(grid.scrollback_len(), 0);
        assert_eq!(format!("{grid:?}"), before.replace("len: 1", "len: 0"));
        grid.leave_alt_screen();
        assert_eq!(text(&grid), vec!["b", "c", "d"], "the primary screen too");
    }

    #[test]
    fn scroll_region_helpers() {
        let region = ScrollRegion { top: 2, bottom: 4 };
        assert_eq!(region.height(), 3);
        assert!(region.contains(2));
        assert!(region.contains(4));
        assert!(!region.contains(1));
        assert!(!region.contains(5));
    }
}
