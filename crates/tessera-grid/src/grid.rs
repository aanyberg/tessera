//! The visible screen, its scrollback and the cursor-relative primitives
//! `tessera-term` drives it with.

use std::collections::VecDeque;

use crate::{Cell, Line, Point, Style, TabStops};

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

/// Cursor state captured by `save_cursor` (DECSC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SavedCursor {
    cursor: Point,
    pen: Style,
    pending_wrap: bool,
}

/// The visible screen plus scrollback.
///
/// Every operation is total: arguments are clamped so the grid never leaves
/// its bounds. Coordinates are absolute screen coordinates; origin mode is
/// resolved by the caller using [`Grid::scroll_region`].
#[derive(Debug, Clone)]
pub struct Grid {
    rows: u16,
    cols: u16,
    lines: Vec<Line>,
    scrollback: VecDeque<Line>,
    max_scrollback: usize,
    cursor: Point,
    pending_wrap: bool,
    pen: Style,
    region: ScrollRegion,
    saved: SavedCursor,
    tabs: TabStops,
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
            scrollback: VecDeque::new(),
            max_scrollback,
            cursor: Point::default(),
            pending_wrap: false,
            pen: Style::DEFAULT,
            region: ScrollRegion {
                top: 0,
                bottom: rows - 1,
            },
            saved: SavedCursor {
                cursor: Point::default(),
                pen: Style::DEFAULT,
                pending_wrap: false,
            },
            tabs: TabStops::new(cols),
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
    /// Callers must keep `cells.len() == cols()`; this is how shell
    /// integration attaches marks.
    pub fn line_mut(&mut self, row: u16) -> Option<&mut Line> {
        self.lines.get_mut(usize::from(row))
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
        self.max_scrollback
    }

    /// A scrollback line; index `0` is the oldest, `scrollback_len() - 1`
    /// the one most recently scrolled off the screen.
    pub fn scrollback_line(&self, index: usize) -> Option<&Line> {
        self.scrollback.get(index)
    }

    /// The active scroll region.
    pub fn scroll_region(&self) -> ScrollRegion {
        self.region
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
    pub fn put_char(&mut self, ch: char) {
        let cell = Cell::new(ch, self.pen);
        self.put_cell(cell);
    }

    /// Writes an arbitrary cell at the cursor and advances it like
    /// [`Grid::put_char`].
    pub fn put_cell(&mut self, cell: Cell) {
        if self.pending_wrap {
            self.pending_wrap = false;
            self.lines[usize::from(self.cursor.row)].wrapped = true;
            self.cursor.col = 0;
            self.line_feed();
        }
        let Point { row, col } = self.cursor;
        self.lines[usize::from(row)].cells[usize::from(col)] = cell;
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

    // -------------------------------------------------------- cursor motion

    /// Moves the cursor to `(row, col)`, clamped to the screen (CUP).
    pub fn goto(&mut self, row: u16, col: u16) {
        self.pending_wrap = false;
        self.cursor = Point {
            row: row.min(self.rows - 1),
            col: col.min(self.cols - 1),
        };
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
        self.cursor.row = self.cursor.row.saturating_sub(n).max(limit);
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
        self.cursor.row = self.cursor.row.saturating_add(n).min(limit);
    }

    /// Moves the cursor left `n` columns, stopping at column zero (CUB).
    pub fn cursor_left(&mut self, n: u16) {
        self.pending_wrap = false;
        self.cursor.col = self.cursor.col.saturating_sub(n);
    }

    /// Moves the cursor right `n` columns, stopping at the last column (CUF).
    pub fn cursor_right(&mut self, n: u16) {
        self.pending_wrap = false;
        self.cursor.col = self.cursor.col.saturating_add(n).min(self.cols - 1);
    }

    /// Moves the cursor to column zero (CR).
    pub fn carriage_return(&mut self) {
        self.pending_wrap = false;
        self.cursor.col = 0;
    }

    /// Moves the cursor down one row (LF / IND). At the bottom of the scroll
    /// region the region scrolls up instead; below the region the cursor
    /// stops at the last row.
    pub fn line_feed(&mut self) {
        self.pending_wrap = false;
        if self.cursor.row == self.region.bottom {
            self.scroll_up(1);
        } else if self.cursor.row < self.rows - 1 {
            self.cursor.row += 1;
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
            self.cursor.row -= 1;
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
        self.cursor.col = self
            .tabs
            .next(self.cursor.col, n)
            .unwrap_or(self.cols - 1)
            .min(self.cols - 1);
    }

    /// Moves the cursor to the `n`th previous tab stop, or column zero when
    /// fewer precede it (CBT). `n == 0` is a no-op.
    pub fn tab_backward(&mut self, n: u16) {
        self.pending_wrap = false;
        if n == 0 {
            return;
        }
        self.cursor.col = self.tabs.previous(self.cursor.col, n).unwrap_or(0);
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
    /// blank lines enter at the bottom. When the region is the whole screen
    /// the departing lines go to scrollback.
    pub fn scroll_up(&mut self, n: u16) {
        let ScrollRegion { top, bottom } = self.region;
        let n = usize::from(n.min(self.region.height()));
        if n == 0 {
            return;
        }
        let top = usize::from(top);
        let keep_history = self.region_is_full_screen() && self.max_scrollback > 0;
        for i in top..top + n {
            let line = std::mem::replace(&mut self.lines[i], Line::blank(self.cols, self.pen));
            if keep_history {
                self.scrollback.push_back(line);
            }
        }
        while self.scrollback.len() > self.max_scrollback {
            self.scrollback.pop_front();
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
        let (top, bottom) = (usize::from(top), usize::from(bottom));
        self.lines[top..=bottom].rotate_right(n);
        for line in &mut self.lines[top..top + n] {
            *line = Line::blank(self.cols, self.pen);
        }
    }

    fn region_is_full_screen(&self) -> bool {
        self.region.top == 0 && self.region.bottom == self.rows - 1
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
        self.lines[row..=bottom].rotate_left(n);
        for line in &mut self.lines[bottom + 1 - n..=bottom] {
            *line = Line::blank(self.cols, self.pen);
        }
    }

    // ------------------------------------------------------ char editing

    /// Inserts `n` blank cells at the cursor, shifting the rest of the line
    /// right; cells pushed past the last column are lost (ICH).
    pub fn insert_chars(&mut self, n: u16) {
        self.pending_wrap = false;
        let Point { row, col } = self.cursor;
        let (col, cols) = (usize::from(col), usize::from(self.cols));
        let n = usize::from(n).min(cols - col);
        if n == 0 {
            return;
        }
        let line = &mut self.lines[usize::from(row)];
        line.cells[col..].rotate_right(n);
        line.fill(col..col + n, self.pen);
    }

    /// Deletes `n` cells at the cursor, shifting the rest of the line left
    /// and filling with blanks at the end (DCH).
    pub fn delete_chars(&mut self, n: u16) {
        self.pending_wrap = false;
        let Point { row, col } = self.cursor;
        let (col, cols) = (usize::from(col), usize::from(self.cols));
        let n = usize::from(n).min(cols - col);
        if n == 0 {
            return;
        }
        let line = &mut self.lines[usize::from(row)];
        line.cells[col..].rotate_left(n);
        line.fill(cols - n..cols, self.pen);
    }

    /// Blanks `n` cells starting at the cursor without shifting (ECH).
    pub fn erase_chars(&mut self, n: u16) {
        self.pending_wrap = false;
        let Point { row, col } = self.cursor;
        let col = usize::from(col);
        let end = col.saturating_add(usize::from(n));
        self.lines[usize::from(row)].fill(col..end, self.pen);
    }

    // --------------------------------------------------------------- erase

    /// Blanks part of the cursor's line (EL). Marks are kept; the wrap flag
    /// is cleared when the end of the line is erased.
    pub fn erase_in_line(&mut self, mode: EraseLine) {
        self.pending_wrap = false;
        let Point { row, col } = self.cursor;
        let (col, cols) = (usize::from(col), usize::from(self.cols));
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
    /// behaves like [`Grid::erase_in_line`].
    pub fn erase_in_display(&mut self, mode: EraseDisplay) {
        self.pending_wrap = false;
        let row = usize::from(self.cursor.row);
        match mode {
            EraseDisplay::Below => {
                self.erase_in_line(EraseLine::ToEnd);
                for line in &mut self.lines[row + 1..] {
                    line.clear(self.pen);
                }
            }
            EraseDisplay::Above => {
                for line in &mut self.lines[..row] {
                    line.clear(self.pen);
                }
                self.erase_in_line(EraseLine::ToStart);
            }
            EraseDisplay::All => {
                for line in &mut self.lines {
                    line.clear(self.pen);
                }
            }
            EraseDisplay::Scrollback => self.scrollback.clear(),
        }
    }

    /// Returns the grid to its initial state (RIS): blank screen, home
    /// cursor, default pen, full scroll region, default tab stops. Scrollback
    /// is kept.
    pub fn reset(&mut self) {
        let scrollback = std::mem::take(&mut self.scrollback);
        *self = Self::with_scrollback(self.rows, self.cols, self.max_scrollback);
        self.scrollback = scrollback;
    }
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
