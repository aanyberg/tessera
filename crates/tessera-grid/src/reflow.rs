//! Resizing a screen: moving rows between the screen and scrollback, and
//! re-wrapping soft-wrapped lines to a new width (ADR 0009).
//!
//! [`Grid::resize`](crate::Grid::resize) drives the two functions here:
//!
//! - [`resize_primary`] for the primary screen (the live one, or the one
//!   stashed while the alternate screen is active). Scrollback and the
//!   screen are one buffer: a row change moves whole rows between them at
//!   the old width, then a column change joins every logical line (rows
//!   linked by [`Line::wrapped`]) and splits it again at the new width.
//! - [`resize_alternate`] for the alternate screen, which is never
//!   re-wrapped: its rows follow the same row rule without history, and
//!   each row is cut or padded to the new width.
//!
//! Everything here is total: any size of at least one row and one column,
//! any content.

use std::cmp::Ordering;

use crate::{Cell, GraphemeTable, Line, Mark, Point, Scrollback, Style};

/// The parts of one screen a resize rewrites.
pub(crate) struct ScreenMut<'a> {
    /// The screen's rows, top to bottom.
    pub(crate) lines: &'a mut Vec<Line>,
    /// The screen's cursor.
    pub(crate) cursor: &'a mut Point,
    /// The screen's pending-wrap flag.
    pub(crate) pending_wrap: &'a mut bool,
}

/// A grid size, both dimensions at least one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Size {
    /// Visible rows.
    pub(crate) rows: u16,
    /// Columns.
    pub(crate) cols: u16,
}

/// Resizes the primary screen whose rows are `old_cols` wide to `size`,
/// sharing `history` with it.
///
/// Rows first, at the old width (only whole rows move); then, when the
/// width changes, every line of history and screen is re-wrapped and the
/// rows that were on the screen are placed again (see [`rewrap`]).
///
/// `scrolled` is
/// [`Grid::scrolled_since_clear`](crate::Grid::scrolled_since_clear): a
/// screen rows of which have gone into history since it was last cleared
/// is continuous with its history and is resized as one buffer anchored at
/// the bottom, exactly as before #86; a cleared one keeps its own rows in
/// place (ADR 0009, amendment of 2026-10-29).
///
/// Returns whether rows that were on the screen went into history (a
/// shrink that pushed rows, or a re-wrap whose screen block outgrew the
/// screen): the screen's content reached its bottom, as when output
/// scrolls it, so the caller sets `scrolled` from then on.
pub(crate) fn resize_primary(
    mut screen: ScreenMut<'_>,
    history: &mut Scrollback,
    old_cols: u16,
    size: Size,
    scrolled: bool,
) -> bool {
    let mut pushed = change_rows(&mut screen, Some(history), size.rows, old_cols, scrolled);
    if size.cols != old_cols {
        pushed |= rewrap(&mut screen, history, size, scrolled);
    }
    pushed
}

/// Resizes the alternate screen whose rows are `old_cols` wide to `size`
/// without re-wrapping: rows follow the primary row rule but rows leaving
/// the screen are discarded, and every row is cut or padded to the new
/// width. A wide character cut in half by the new edge is blanked. Pending
/// wrap is cleared when the width changes.
pub(crate) fn resize_alternate(mut screen: ScreenMut<'_>, old_cols: u16, size: Size) {
    // The alternate screen has no history: nothing it drops is pushed.
    let _ = change_rows(&mut screen, None, size.rows, old_cols, false);
    if size.cols != old_cols {
        for line in screen.lines.iter_mut() {
            cut_or_pad(line, size.cols);
        }
        screen.cursor.col = screen.cursor.col.min(size.cols - 1);
        *screen.pending_wrap = false;
    }
}

/// Grows or shrinks the screen to `rows` rows of `cols` cells without
/// touching any row's cells.
///
/// - **Grow:** when the screen has `scrolled` since it was last cleared
///   or is anchored at the bottom ([`anchored_at_bottom`]), the newest
///   lines of `history` (as many as exist, up to the growth) move to the
///   top of the screen and the cursor moves down with the text; the rest,
///   or all of the growth on any other screen (a prompt after a clear),
///   is blank default rows at the bottom. So shrinking and growing back a
///   cleared screen never pulls in history that was not on it (#86).
/// - **Shrink:** blank rows below the cursor go first (see
///   [`removable_blank_rows`]), then rows leave through the top into
///   `history` (or are discarded without one) and the cursor moves up with
///   the text, stopping at row zero.
///
/// The cursor column and pending wrap are untouched. Returns whether rows
/// left through the top into `history`.
fn change_rows(
    screen: &mut ScreenMut<'_>,
    history: Option<&mut Scrollback>,
    rows: u16,
    cols: u16,
    scrolled: bool,
) -> bool {
    let mut pushed_any = false;
    let lines = &mut *screen.lines;
    let old = lines.len();
    let new = usize::from(rows);
    match new.cmp(&old) {
        Ordering::Equal => {}
        Ordering::Greater => {
            let mut pulled = Vec::new();
            if let Some(history) =
                history.filter(|_| scrolled || anchored_at_bottom(lines, screen.cursor.row))
            {
                while pulled.len() < new - old {
                    let Some(line) = history.pop_newest() else {
                        break;
                    };
                    pulled.push(line);
                }
            }
            pulled.reverse();
            let moved = pulled.len();
            lines.splice(0..0, pulled);
            lines.resize_with(new, || Line::blank(cols, Style::DEFAULT));
            screen.cursor.row = clamp_row(usize::from(screen.cursor.row) + moved, rows);
        }
        Ordering::Less => {
            let excess = old - new;
            let dropped = removable_blank_rows(lines, screen.cursor.row, excess);
            lines.truncate(old - dropped);
            if dropped > 0
                && let Some(last) = lines.last_mut()
            {
                // A dropped row may have continued this one; nothing does
                // now.
                last.wrapped = false;
            }
            let pushed = excess - dropped;
            pushed_any = pushed > 0 && history.is_some();
            match history {
                Some(history) => {
                    for mut line in lines.drain(..pushed) {
                        line.graphemes.compact();
                        // An evicted line has nowhere to go.
                        drop(history.push(line));
                    }
                }
                None => drop(lines.drain(..pushed)),
            }
            screen.cursor.row =
                clamp_row(usize::from(screen.cursor.row).saturating_sub(pushed), rows);
        }
    }
    pushed_any
}

/// True when the screen is anchored at the bottom by its cursor: the
/// cursor is on the last row, or its row and every row below it but the
/// last are wrapped (the cursor's logical line ends on the last row). On
/// a screen that has not scrolled since it was last cleared, only such a
/// screen takes history back when it grows (a height grow, or a re-wrap
/// that leaves fewer rows than the screen has); any other, such as a
/// prompt on a cleared screen, gets blank rows at the bottom instead
/// (#86; ghostty's rule for rows, extended to the cursor's line).
fn anchored_at_bottom(lines: &[Line], cursor_row: u16) -> bool {
    let last = lines.len().saturating_sub(1);
    lines
        .get(usize::from(cursor_row)..last)
        .is_none_or(|rows| rows.iter().all(|line| line.wrapped))
}

/// How many rows at the bottom of `lines`, at most `limit`, a resize may
/// drop instead of pushing text into history: rows strictly below
/// `cursor_row` that are [`blank_row`]s. It stops at `limit` rows, so a
/// prompt on top of a cleared screen does not scan every row of it on
/// each one-row shrink (#86). A dropped row may be the empty continuation of a
/// wrapped row (readline leaves one behind after erasing); the caller
/// clears that row's wrap flag.
fn removable_blank_rows(lines: &[Line], cursor_row: u16, limit: usize) -> usize {
    let below = lines
        .len()
        .saturating_sub(usize::from(cursor_row).saturating_add(1));
    lines
        .iter()
        .rev()
        .take(below)
        .take_while(|line| blank_row(line))
        .take(limit)
        .count()
}

/// A row with nothing in it: every cell a [`plain_blank`], no marks, not
/// wrapped.
fn blank_row(line: &Line) -> bool {
    !line.wrapped && line.marks.is_empty() && line.cells.iter().all(plain_blank)
}

/// Cuts `line` to `cols` cells or pads it with default blanks. A wide
/// character whose spacer falls off the new edge is blanked, clusters past
/// the edge are dropped, and padding that is no longer in the last column
/// becomes a blank.
fn cut_or_pad(line: &mut Line, cols: u16) {
    let cols = usize::from(cols);
    if line.cells.len() > cols {
        line.cells.truncate(cols);
        line.graphemes.remove_range(cols..usize::MAX);
        if line.cells.last().is_some_and(Cell::is_wide) {
            line.blank_at(cols - 1, Style::DEFAULT);
        }
    } else {
        // Padding is only ever the last column; the old last column is not
        // the last any more.
        if let Some(last) = line.cells.last_mut() {
            last.flags = last.flags.difference(crate::CellFlags::PAD);
        }
        line.cells.resize(cols, Cell::BLANK);
    }
}

/// A blank as far as reflow is concerned: exactly [`Cell::BLANK`], a
/// space with the default style, or padding ([`Cell::is_pad`]) of any
/// background (the pen's from `put_wide`, the wide character's from a
/// re-wrap). Trailing ones end a logical line's text and rows made
/// of them may be dropped; any other space with a colour or attribute (a
/// background fill, an underline, inverse video) is content.
fn plain_blank(cell: &Cell) -> bool {
    *cell == Cell::BLANK || (cell.is_pad() && cell.ch == ' ')
}

/// Converts a row index known to be below `rows` (or clamps it there).
fn clamp_row(row: usize, rows: u16) -> u16 {
    u16::try_from(row).map_or(rows - 1, |row| row.min(rows - 1))
}

/// Re-wraps scrollback and screen to `size.cols` columns and places the
/// screen again. The cursor follows its character.
///
/// A screen that has `scrolled` since it was last cleared is one buffer
/// with its history, anchored at the bottom (as before #86): blank rows
/// below the cursor absorb the rows the buffer grew by, wherever they
/// come from, and the window is the last `size.rows` rows.
///
/// On a cleared screen only rows that were on the screen move the screen
/// (#86, ADR 0009 as amended 2026-10-29): the *screen block* is the
/// re-wrapped rows from the one holding the old top row's first cell (a
/// logical line that straddles history and screen splits there) to the
/// end. History before it only lengthens or shortens history, so
/// narrowing after a clear never pulls old output back onto the screen.
///
/// - A block longer than the screen first loses blank rows below the
///   cursor (as many as it grew by), then its top rows go into history:
///   the window is its last `size.rows` rows.
/// - A block shorter than the screen takes rows back from history above it
///   only when the cursor anchors the screen at the bottom
///   ([`anchored_at_bottom`]); otherwise the block keeps its place and
///   blank rows fill the bottom.
///
/// Returns whether rows of the screen block went into history (the block
/// was still longer than the screen after the blank rows absorbed what
/// they could).
fn rewrap(
    screen: &mut ScreenMut<'_>,
    history: &mut Scrollback,
    size: Size,
    scrolled: bool,
) -> bool {
    let rows = usize::from(size.rows);
    let anchored = scrolled || anchored_at_bottom(screen.lines, screen.cursor.row);
    let mut source = history.take_lines();
    let top = source.len();
    let cursor = SourceCursor {
        index: top + usize::from(screen.cursor.row),
        col: usize::from(screen.cursor.col),
        pending_wrap: *screen.pending_wrap,
    };
    source.append(screen.lines);
    let source_len = source.len();

    let keep = history.cap().saturating_add(rows.saturating_mul(2));
    let mut rewrapper = Rewrapper::new(size.cols, cursor, top, anchored, keep);
    let mut group = Vec::new();
    for line in source {
        let wrapped = line.wrapped;
        group.push(line);
        if !wrapped {
            rewrapper.logical_line(&mut group);
            group.clear();
        }
    }
    // The last row of the buffer cannot continue anywhere, even when it
    // says it does.
    if !group.is_empty() {
        rewrapper.logical_line(&mut group);
    }
    let Rewrapped {
        mut out,
        placed,
        top,
        dropped,
    } = rewrapper.finish();

    // Blank rows under the cursor absorb growth before real rows are
    // pushed into history: the whole buffer's growth on a screen that has
    // scrolled, only the screen block's on a cleared one (#86). They are
    // counted after the re-wrap: a blank row that continued a wrapped one
    // has been joined into its line and is gone already.
    let total = dropped + out.len();
    let growth = if scrolled {
        total.saturating_sub(source_len)
    } else {
        total.saturating_sub(top).saturating_sub(rows)
    };
    if growth > 0 {
        let after_cursor = placed.map_or(0, |p| total.saturating_sub(p.index + 1));
        // Counted up to the growth only: no need to scan more.
        let drop = out
            .iter()
            .rev()
            .take(after_cursor)
            .take_while(|line| blank_row(line))
            .take(growth)
            .count();
        out.truncate(out.len() - drop);
        if drop > 0
            && let Some(last) = out.last_mut()
        {
            // A dropped row may have continued this one (a one-column
            // re-wrap that dropped the wide characters after a blank);
            // nothing does now.
            last.wrapped = false;
        }
    }
    let total = dropped + out.len();
    let pushed_block = total.saturating_sub(top) > rows;
    // Where the window starts, counting the dropped rows: the last `rows`
    // rows (pulling history down when the block is short and the screen
    // was anchored at the bottom), or the block itself with blank rows
    // below it.
    let start = if anchored || total.saturating_sub(top) >= rows {
        total.saturating_sub(rows)
    } else {
        top
    };
    // The output keeps at least the cap plus twice the screen (`keep`),
    // and the prune removed at most one screen of it, so the window never
    // starts in the dropped rows; it always ends at or past the last row.
    debug_assert!(start >= dropped && start + rows >= total);
    let first = start.saturating_sub(dropped);
    out.resize_with(first + rows, || Line::blank(size.cols, Style::DEFAULT));
    *screen.lines = out.split_off(first);
    let placed = placed.unwrap_or(Placed {
        index: start + rows - 1,
        col: 0,
        pending_wrap: false,
    });
    if placed.index >= start {
        *screen.cursor = Point::new(clamp_row(placed.index - start, size.rows), placed.col);
        *screen.pending_wrap = placed.pending_wrap;
    } else {
        // The cursor's row went into history: keep its column on the top
        // row.
        *screen.cursor = Point::new(0, placed.col);
        *screen.pending_wrap = false;
    }
    for line in &mut out {
        line.graphemes.compact();
    }
    history.set_lines(out);
    pushed_block
}

/// What a [`Rewrapper`] produced.
struct Rewrapped {
    /// The kept rows.
    out: Vec<Line>,
    /// The cursor's place.
    placed: Option<Placed>,
    /// Index of the row holding the old top screen row's first cell,
    /// counting the dropped rows: where the screen block starts.
    top: usize,
    /// How many rows were dropped before the first kept one.
    dropped: usize,
}

/// The cursor's place in the buffer before a re-wrap.
#[derive(Debug, Clone, Copy)]
struct SourceCursor {
    /// Index of its row in scrollback followed by the screen.
    index: usize,
    /// Its column.
    col: usize,
    /// Whether a wrap was pending.
    pending_wrap: bool,
}

/// The cursor's place in the re-wrapped buffer.
#[derive(Debug, Clone, Copy)]
struct Placed {
    /// Index of its row in the new buffer, counting the rows dropped from
    /// its start ([`Rewrapper::dropped`]).
    index: usize,
    /// Its column, below the new width.
    col: u16,
    /// Whether a wrap is pending (only on the last column).
    pending_wrap: bool,
}

/// Re-wraps logical lines one at a time into a new buffer.
struct Rewrapper {
    /// The new width.
    cols: u16,
    /// Rows produced so far, except the oldest `dropped`.
    out: Vec<Line>,
    /// How many rows the buffer must keep: what history and the screen
    /// can hold plus the screen rows the prune may remove. Older rows are
    /// dropped as the re-wrap goes, so narrowing to a few columns does not
    /// hold every row it produces until history cuts them to its cap.
    keep: usize,
    /// Rows dropped from the start of the output.
    dropped: usize,
    /// Most rows the output ever held (tests check the bound).
    #[cfg(test)]
    peak: usize,
    /// Index of the next source row.
    next_source: usize,
    cursor: SourceCursor,
    placed: Option<Placed>,
    /// Index of the old top screen row in the source.
    top_index: usize,
    /// Whether the screen was anchored at the bottom
    /// ([`anchored_at_bottom`]).
    anchored: bool,
    /// Index of the output row holding that row's first cell (see
    /// [`Rewrapped::top`]), once re-wrapped.
    top: Option<usize>,
    /// Cells of the logical line being re-wrapped, joined (reused).
    joined: Vec<Cell>,
    /// Its clusters: index into `joined` and text, by ascending index
    /// (reused).
    clusters: Vec<(usize, Box<str>)>,
}

impl Rewrapper {
    /// A re-wrapper to `cols` columns that tracks `cursor` and the source
    /// row `top_index` (the old top screen row) of a screen `anchored` at
    /// the bottom or not, and keeps at least `keep` rows.
    fn new(cols: u16, cursor: SourceCursor, top_index: usize, anchored: bool, keep: usize) -> Self {
        Self {
            cols,
            out: Vec::new(),
            keep,
            dropped: 0,
            #[cfg(test)]
            peak: 0,
            next_source: 0,
            cursor,
            placed: None,
            top_index,
            anchored,
            top: None,
            joined: Vec::new(),
            clusters: Vec::new(),
        }
    }

    /// The kept rows, the cursor's place, where the screen block starts
    /// and how many rows were dropped before the first kept one. A source
    /// without its top row (never: the screen has a row) puts the block
    /// after the last row.
    fn finish(self) -> Rewrapped {
        let total = self.next_index();
        Rewrapped {
            out: self.out,
            placed: self.placed,
            top: self.top.unwrap_or(total),
            dropped: self.dropped,
        }
    }

    /// Index of the next output row, counting the dropped ones.
    fn next_index(&self) -> usize {
        self.dropped + self.out.len()
    }

    /// Appends an output row, then drops the oldest rows if the output
    /// has grown past its bound, so even one logical line spanning the
    /// whole history never holds more than that.
    fn push_row(&mut self, line: Line) {
        self.out.push(line);
        #[cfg(test)]
        {
            self.peak = self.peak.max(self.out.len());
        }
        self.drop_oldest();
    }

    /// Drops the oldest rows once the output holds twice what it must
    /// keep (so each row moves at most once on average): the output never
    /// holds more than `max(2 * keep, 1024)` rows.
    fn drop_oldest(&mut self) {
        if self.out.len() >= self.keep.saturating_mul(2).max(1024) {
            let excess = self.out.len() - self.keep;
            self.out.drain(..excess);
            self.dropped += excess;
        }
    }

    /// Joins the rows of one logical line (all but the last are wrapped)
    /// and appends it, split at the new width, to the output.
    ///
    /// The rows' cells are concatenated, except that a wrapped row's last
    /// cell is skipped when it is padding ([`Cell::is_pad`]): the column a
    /// wide character left behind when it did not fit, not text. Any other
    /// cell is text, a space before a wide character included. Trailing
    /// blanks of the whole line are trimmed. Marks of
    /// every row go to the first output row, in row order.
    fn logical_line(&mut self, rows: &mut [Line]) {
        let first = self.next_source;
        self.next_source += rows.len();
        self.joined.clear();
        self.clusters.clear();
        let mut marks: Vec<Mark> = Vec::new();
        let mut cursor_offset = None;
        let mut top_offset = None;
        self.joined
            .reserve_exact(rows.iter().map(|row| row.cells.len()).sum());
        for k in 0..rows.len() {
            let stranded = k + 1 < rows.len() && rows[k].cells.last().is_some_and(Cell::is_pad);
            let row = &mut rows[k];
            let len = row.cells.len() - usize::from(stranded);
            let base = self.joined.len();
            if first + k == self.cursor.index {
                cursor_offset =
                    Some(base + self.cursor.col + usize::from(self.cursor.pending_wrap));
            }
            if first + k == self.top_index {
                if k == 0 {
                    // The top row starts this line: so does the block.
                    self.top = Some(self.next_index());
                } else {
                    // A line straddling history and the screen.
                    top_offset = Some(base);
                }
            }
            self.joined.extend_from_slice(&row.cells[..len]);
            // Free the source row now: a long logical line would otherwise
            // be held twice (rows and joined) on top of its output rows.
            row.cells = Vec::new();
            self.clusters.extend(
                row.graphemes
                    .take()
                    .filter(|&(col, _)| col < len)
                    .map(|(col, text)| (base + col, text)),
            );
            if marks.is_empty() {
                marks = std::mem::take(&mut row.marks);
            } else {
                marks.append(&mut row.marks);
            }
        }
        let end = self
            .joined
            .iter()
            .rposition(|cell| !plain_blank(cell))
            .map_or(0, |i| i + 1);
        let first_out = self.next_index();
        let last_len = if self.joined[..end].iter().all(|c| c.flags.is_empty()) {
            self.split_plain(end, cursor_offset, top_offset)
        } else if let Some(top) = top_offset {
            self.split::<true>(end, cursor_offset, top)
        } else {
            // Every line but the one straddling history and the screen:
            // the per-cell check for the top row compiles away.
            self.split::<false>(end, cursor_offset, 0)
        };
        // The old top row continued a line from history but held only its
        // trailing blanks (ED 2 under a wrapped line leaves such a row): the
        // screen's part of the line is empty. On a screen not anchored at
        // the bottom it stays a blank row of its own that starts the block,
        // with the cursor if it was on it, so the line's rows stay in
        // history and the screen keeps its rows. (A screen anchored at the
        // bottom takes its last rows whatever the block.)
        let top_past = top_offset.is_some_and(|offset| offset >= end);
        if top_past && !self.anchored {
            self.top = Some(self.next_index());
            if cursor_offset.is_some_and(|offset| offset >= end) {
                self.placed = Some(Placed {
                    index: self.next_index(),
                    col: to_col(self.cursor.col.min(usize::from(self.cols) - 1)),
                    pending_wrap: false,
                });
            }
            self.push_row(Line::blank(self.cols, Style::DEFAULT));
        } else {
            if top_past {
                self.top = Some(self.next_index());
            }
            if let Some(offset) = cursor_offset.filter(|&offset| offset >= end) {
                self.place_past_text(offset - end, last_len);
            }
        }
        // A first row already dropped (past the history cap) takes its
        // marks with it.
        if let Some(line) = first_out
            .checked_sub(self.dropped)
            .and_then(|i| self.out.get_mut(i))
        {
            line.marks = marks;
        }
    }

    /// Splits `joined[..end]`, which has no wide, spacer or cluster cells,
    /// into rows of the new width, placing the cursor and the top row's
    /// first cell when their offsets are below `end`. Returns the number of
    /// cells on the last row.
    fn split_plain(
        &mut self,
        end: usize,
        cursor_offset: Option<usize>,
        top_offset: Option<usize>,
    ) -> usize {
        let cols = usize::from(self.cols);
        let first_out = self.next_index();
        if let Some(offset) = top_offset.filter(|&offset| offset < end) {
            self.top = Some(first_out + offset / cols);
        }
        if let Some(offset) = cursor_offset.filter(|&offset| offset < end) {
            self.placed = Some(Placed {
                index: first_out + offset / cols,
                col: to_col(offset % cols),
                pending_wrap: false,
            });
        }
        if end == 0 {
            self.push_row(Line::blank(self.cols, Style::DEFAULT));
            return 0;
        }
        let count = end.div_ceil(cols);
        // Indexed rather than `chunks`, which would borrow `joined` across
        // `push_row`.
        for i in 0..count {
            let from = i * cols;
            let mut cells = Vec::with_capacity(cols);
            cells.extend_from_slice(&self.joined[from..end.min(from + cols)]);
            cells.resize(cols, Cell::BLANK);
            self.push_row(Line {
                cells,
                marks: Vec::new(),
                graphemes: GraphemeTable::new(),
                wrapped: i + 1 < count,
            });
        }
        end - (count - 1) * cols
    }

    /// Splits `joined[..end]` into rows of the new width, never splitting a
    /// wide character from its spacer: one that does not fit starts the
    /// next row and leaves a padding cell ([`Cell::is_pad`]) with the wide
    /// character's background behind. A wide character wider than the whole
    /// row (one column) is dropped. Clusters move with
    /// their cells. The cursor and, when `TOP` is set, the top row's
    /// first cell at `top_offset` are placed like
    /// [`Rewrapper::split_plain`] places them. On a dropped wide character
    /// the top row's first cell maps to the row the next kept cell starts
    /// (the row being filled, or the next one when it is full); the
    /// cursor stays on the row being filled, on its next free column or
    /// its last one when it is full. Returns the number of cells on the
    /// last row.
    fn split<const TOP: bool>(
        &mut self,
        end: usize,
        cursor_offset: Option<usize>,
        top_offset: usize,
    ) -> usize {
        let cols = usize::from(self.cols);
        let mut row = empty_row(cols);
        // Drained through a local so the loop can update `self`; the
        // emptied vector goes back afterwards to be reused.
        let mut entries = std::mem::take(&mut self.clusters);
        let mut clusters = entries.drain(..).peekable();
        let mut p = 0;
        while p < end {
            let mut cell = self.joined[p];
            let width = if cell.is_wide() && self.joined.get(p + 1).is_some_and(Cell::is_spacer) {
                2
            } else {
                // A half without its other half (never produced by the
                // grid's own edits) becomes something that stands alone.
                if cell.is_spacer() {
                    cell = Cell::blank_with(cell.style);
                }
                cell.flags = cell.flags.difference(crate::CellFlags::WIDE);
                1
            };
            // Padding is only ever the last column of a row: the join skips
            // it on a wrapped row and the trim on the last one.
            debug_assert!(!cell.is_pad(), "padding reached the split");
            while clusters.next_if(|&(at, _)| at < p).is_some() {}
            let cluster = clusters.next_if(|&(at, _)| at == p).map(|(_, text)| text);
            let holds_cursor = cursor_offset.is_some_and(|offset| (p..p + width).contains(&offset));
            let holds_top = TOP && (p..p + width).contains(&top_offset);
            if width > cols {
                if holds_top {
                    // The row the next kept cell starts: this one, or the
                    // next when this one is full.
                    self.top = Some(self.next_index() + usize::from(row.cells.len() == cols));
                }
                if holds_cursor {
                    self.placed = Some(Placed {
                        index: self.next_index(),
                        col: to_col(row.cells.len().min(cols - 1)),
                        pending_wrap: false,
                    });
                }
                p += width;
                continue;
            }
            if row.cells.len() + width > cols {
                let pad_for = (row.cells.len() + 1 == cols).then_some(p);
                self.break_row(&mut row, pad_for);
            }
            let col = row.cells.len();
            if holds_top {
                self.top = Some(self.next_index());
            }
            if holds_cursor {
                self.placed = Some(Placed {
                    index: self.next_index(),
                    col: to_col(col),
                    pending_wrap: false,
                });
            }
            match cluster {
                Some(text) if cell.is_cluster() => row.graphemes.set(col, text),
                _ => cell.flags = cell.flags.difference(crate::CellFlags::CLUSTER),
            }
            row.cells.push(cell);
            if width == 2 {
                row.cells.push(self.joined[p + 1]);
            }
            p += width;
        }
        drop(clusters);
        self.clusters = entries;
        let last_len = row.cells.len();
        row.cells.resize(cols, Cell::BLANK);
        self.push_row(row);
        last_len
    }

    /// Ends the full output row `row` (wrapped), appends it to the output
    /// and leaves `row` empty. When `pad_for` is the index in `joined` of
    /// a wide character that does not fit in the one column left, that
    /// column becomes padding first, so the next join drops it again. The
    /// padding takes the character's background, as the padding a live
    /// write leaves does ([`Cell::pad_with`], like an erase), so a coloured
    /// band has no gap where the character wrapped (#88).
    ///
    /// Out of line: it runs once per row, and kept out of the per-cell
    /// loop of [`Rewrapper::split`] it leaves that loop's registers alone
    /// (#88).
    #[inline(never)]
    fn break_row(&mut self, row: &mut Line, pad_for: Option<usize>) {
        let cols = usize::from(self.cols);
        if let Some(p) = pad_for {
            row.cells.push(Cell::pad_with(self.joined[p].style));
        }
        row.cells.resize(cols, Cell::BLANK);
        row.wrapped = true;
        let full = std::mem::replace(row, empty_row(cols));
        self.push_row(full);
    }

    /// Places a cursor that sat `past` cells after the end of the logical
    /// line's text, given `last_len` cells of text on its last output row:
    /// after a row that is exactly full, on its last column with a wrap
    /// pending (so the next character wraps instead of overwriting the
    /// last one); otherwise at the same distance after the text while that
    /// fits, else on the last column (a blank, never a spacer).
    fn place_past_text(&mut self, past: usize, last_len: usize) {
        let cols = usize::from(self.cols);
        let target = last_len + past;
        let (col, pending_wrap) = if last_len == cols {
            (cols - 1, true)
        } else {
            (target.min(cols - 1), false)
        };
        self.placed = Some(Placed {
            index: self.next_index() - 1,
            col: to_col(col),
            pending_wrap,
        });
    }
}

/// An output row with room for `cols` cells and none yet.
fn empty_row(cols: usize) -> Line {
    Line {
        cells: Vec::with_capacity(cols),
        marks: Vec::new(),
        graphemes: GraphemeTable::new(),
        wrapped: false,
    }
}

/// A column below the new width (itself a `u16`) as a `u16`.
fn to_col(col: usize) -> u16 {
    u16::try_from(col).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str, wrapped: bool) -> Line {
        let mut line = Line::blank(4, Style::DEFAULT);
        for (cell, ch) in line.cells.iter_mut().zip(text.chars()) {
            cell.ch = ch;
        }
        line.wrapped = wrapped;
        line
    }

    #[test]
    fn removable_blank_rows_stop_at_the_cursor_and_at_content() {
        let lines = vec![
            row("ab", false),
            row("", false),
            row("", false),
            row("", false),
        ];
        assert_eq!(removable_blank_rows(&lines, 0, 9), 3);
        assert_eq!(removable_blank_rows(&lines, 2, 9), 1);
        assert_eq!(removable_blank_rows(&lines, 3, 9), 0);
        assert_eq!(removable_blank_rows(&lines, 0, 2), 2, "up to the limit");
        let lines = vec![row("abcd", true), row("", false), row("", false)];
        assert_eq!(
            removable_blank_rows(&lines, 0, 9),
            2,
            "an empty continuation of a wrapped row may go too"
        );
        let mut marked = vec![row("ab", false), row("", false)];
        marked[1].marks.push(Mark::PromptStart);
        assert_eq!(removable_blank_rows(&marked, 0, 9), 0);
        let mut filled = vec![row("ab", false), row("", false)];
        filled[1].cells[0] = Cell::blank_with(Style {
            bg: crate::Color::Indexed(4),
            ..Style::DEFAULT
        });
        assert_eq!(
            removable_blank_rows(&filled, 0, 9),
            0,
            "a background fill is content"
        );
    }

    #[test]
    fn blank_predicates() {
        let bg = Style {
            bg: crate::Color::Indexed(4),
            ..Style::DEFAULT
        };
        assert!(plain_blank(&Cell::BLANK));
        assert!(!plain_blank(&Cell::blank_with(bg)));
        assert!(!plain_blank(&Cell::new(
            ' ',
            Style::DEFAULT.with(Style::UNDERLINE)
        )));
        assert!(plain_blank(&Cell::pad_with(bg)), "padding is not text");
        assert!(!plain_blank(&Cell {
            flags: crate::CellFlags::PAD,
            ..Cell::new('x', Style::DEFAULT)
        }));
    }

    #[test]
    fn cut_or_pad_blanks_a_wide_cell_cut_in_half() {
        let mut line = Line::blank(4, Style::DEFAULT);
        line.cells[2] = Cell {
            flags: crate::CellFlags::WIDE | crate::CellFlags::CLUSTER,
            ..Cell::new('x', Style::DEFAULT)
        };
        line.cells[3] = Cell::spacer(Style::DEFAULT);
        line.graphemes.set(2, "x\u{301}".into());
        cut_or_pad(&mut line, 3);
        assert_eq!(line.cells, vec![Cell::BLANK; 3]);
        assert!(line.graphemes.is_empty());
        cut_or_pad(&mut line, 5);
        assert_eq!(line.cells.len(), 5);
    }

    /// #88: the padding `split` leaves before a wide character that does
    /// not fit takes that character's background (and nothing else, like
    /// the padding a live write leaves), so a coloured band has no gap
    /// where the character wrapped.
    #[test]
    fn split_pads_with_the_background_of_the_wide_cell() {
        let style = Style {
            fg: crate::Color::Indexed(2),
            bg: crate::Color::Indexed(4),
            ..Style::DEFAULT.with(Style::UNDERLINE)
        };
        let cursor = SourceCursor {
            index: usize::MAX,
            col: 0,
            pending_wrap: false,
        };
        let mut rewrapper = Rewrapper::new(5, cursor, usize::MAX, false, 100);
        rewrapper.joined = "abcd"
            .chars()
            .map(|ch| Cell::new(ch, Style::DEFAULT))
            .collect();
        rewrapper.joined.push(Cell {
            flags: crate::CellFlags::WIDE,
            ..Cell::new('中', style)
        });
        rewrapper.joined.push(Cell::spacer(style));
        let end = rewrapper.joined.len();
        assert_eq!(rewrapper.split::<false>(end, None, 0), 2);
        let out = rewrapper.finish().out;
        assert_eq!(out.len(), 2);
        let pad = out[0].cells[4];
        assert!(pad.is_pad() && out[0].wrapped);
        assert_eq!(pad.ch, ' ');
        assert_eq!(
            pad.style,
            Cell::blank_with(style).style,
            "the pad takes the wide cell's background only"
        );
        assert_eq!(pad.style.bg, crate::Color::Indexed(4));
        assert!(plain_blank(&pad), "and is still not text");
        assert!(out[1].cells[0].is_wide());
    }

    /// Review round 3: one logical line spanning the whole history never
    /// makes the output hold more than `max(2 * keep, 1024)` rows, on the
    /// plain path and the cell-by-cell path, and the rows it keeps are the
    /// newest.
    #[test]
    fn one_long_logical_line_stays_within_the_bound() {
        for wide in [false, true] {
            let keep = 30;
            let cursor = SourceCursor {
                index: 0,
                col: 0,
                pending_wrap: false,
            };
            let mut rewrapper = Rewrapper::new(1, cursor, 0, false, keep);
            let mut rows: Vec<Line> = (0..3000)
                .map(|n| {
                    let mut line = row("abcd", true);
                    line.cells[0].ch = char::from(b'a' + u8::try_from(n % 26).unwrap());
                    if wide {
                        line.cells[2] = Cell {
                            flags: crate::CellFlags::WIDE,
                            ..Cell::new('中', Style::DEFAULT)
                        };
                        line.cells[3] = Cell::spacer(Style::DEFAULT);
                    }
                    line
                })
                .collect();
            if let Some(last) = rows.last_mut() {
                last.wrapped = false;
            }
            rewrapper.logical_line(&mut rows);
            let bound = (2 * keep).max(1024);
            assert!(rewrapper.peak <= bound, "peak {} rows", rewrapper.peak);
            let produced = rewrapper.next_index();
            assert!(produced > bound, "the line is longer than the bound");
            let Rewrapped {
                out,
                placed,
                top,
                dropped,
            } = rewrapper.finish();
            assert_eq!(top, 0, "the first row is the top row");
            assert_eq!(dropped + out.len(), produced);
            assert_eq!(
                placed.map(|p| p.index),
                Some(0),
                "cursor on the first glyph"
            );
            // The last source row ends in `d`, or in `b` once its wide
            // character is dropped at one column.
            let newest = if wide { 'b' } else { 'd' };
            assert_eq!(
                out.last().map(|l| l.cells[0].ch),
                Some(newest),
                "the newest rows stay"
            );
        }
    }

    /// #86 fast path against its reference: on plain text `split_plain`
    /// (whole slices) and `split` (cell by cell) give the same rows, the
    /// same cursor place and the same block start, for every width,
    /// length, cursor offset and top-row offset below the text's end.
    #[test]
    fn split_plain_agrees_with_split() {
        let cursor = SourceCursor {
            index: usize::MAX,
            col: 0,
            pending_wrap: false,
        };
        let run = |plain: bool, cols: u16, len: usize, cur: Option<usize>, top: Option<usize>| {
            let mut rewrapper = Rewrapper::new(cols, cursor, usize::MAX, false, 1 << 20);
            // A row before, so indices do not start at zero.
            rewrapper.push_row(Line::blank(cols, Style::DEFAULT));
            rewrapper.joined = (0..len)
                .map(|i| {
                    Cell::new(
                        char::from(b'a' + u8::try_from(i % 26).unwrap()),
                        Style::DEFAULT,
                    )
                })
                .collect();
            let last_len = match (plain, top) {
                (true, _) => rewrapper.split_plain(len, cur, top),
                (false, Some(top)) => rewrapper.split::<true>(len, cur, top),
                (false, None) => rewrapper.split::<false>(len, cur, 0),
            };
            let Rewrapped {
                out, placed, top, ..
            } = rewrapper.finish();
            let rows: Vec<_> = out.iter().map(|l| (l.text(), l.wrapped)).collect();
            (
                last_len,
                rows,
                placed.map(|p| (p.index, p.col, p.pending_wrap)),
                top,
            )
        };
        for cols in 1..=9u16 {
            for len in 1..=30 {
                for offset in 0..len {
                    for (cur, top) in [
                        (Some(offset), None),
                        (None, Some(offset)),
                        (Some(len - 1 - offset), Some(offset)),
                    ] {
                        assert_eq!(
                            run(true, cols, len, cur, top),
                            run(false, cols, len, cur, top),
                            "cols {cols} len {len} cursor {cur:?} top {top:?}"
                        );
                    }
                }
            }
        }
    }

    /// Adversarial verify, cycle 1: at one column a top row that starts
    /// with a wide character (dropped there) starts the block on the row
    /// the next kept cell starts, not on the full row before it, which
    /// holds history.
    #[test]
    fn a_dropped_wide_char_on_the_top_row_starts_the_block_after_it() {
        let cursor = SourceCursor {
            index: usize::MAX,
            col: 0,
            pending_wrap: false,
        };
        for (text, top_at, block) in [("abcd中xy", 4, 4), ("abc中xy", 3, 3), ("中xy", 0, 0)] {
            let mut rewrapper = Rewrapper::new(1, cursor, usize::MAX, false, 1 << 20);
            rewrapper.joined = Vec::new();
            for ch in text.chars() {
                if ch == '中' {
                    rewrapper.joined.push(Cell {
                        flags: crate::CellFlags::WIDE,
                        ..Cell::new(ch, Style::DEFAULT)
                    });
                    rewrapper.joined.push(Cell::spacer(Style::DEFAULT));
                } else {
                    rewrapper.joined.push(Cell::new(ch, Style::DEFAULT));
                }
            }
            let end = rewrapper.joined.len();
            rewrapper.split::<true>(end, None, top_at);
            let Rewrapped { out, top, .. } = rewrapper.finish();
            assert_eq!(
                top,
                block,
                "{text}: block starts on {:?}",
                out.get(top).map(Line::text)
            );
        }
    }

    #[test]
    fn clamp_row_keeps_rows_on_screen() {
        assert_eq!(clamp_row(2, 5), 2);
        assert_eq!(clamp_row(9, 5), 4);
        assert_eq!(clamp_row(usize::MAX, 5), 4);
    }
}
