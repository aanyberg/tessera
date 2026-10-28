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
/// screen becomes the last `size.rows` rows of the result.
pub(crate) fn resize_primary(
    mut screen: ScreenMut<'_>,
    history: &mut Scrollback,
    old_cols: u16,
    size: Size,
) {
    change_rows(&mut screen, Some(history), size.rows, old_cols);
    if size.cols != old_cols {
        rewrap(&mut screen, history, size);
    }
}

/// Resizes the alternate screen whose rows are `old_cols` wide to `size`
/// without re-wrapping: rows follow the primary row rule but rows leaving
/// the screen are discarded, and every row is cut or padded to the new
/// width. A wide character cut in half by the new edge is blanked. Pending
/// wrap is cleared when the width changes.
pub(crate) fn resize_alternate(mut screen: ScreenMut<'_>, old_cols: u16, size: Size) {
    change_rows(&mut screen, None, size.rows, old_cols);
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
/// - **Grow:** the newest lines of `history` (as many as exist, up to the
///   growth) move to the top of the screen and the cursor moves down with
///   the text; the rest is blank default rows at the bottom.
/// - **Shrink:** blank rows below the cursor go first (see
///   [`removable_blank_rows`]), then rows leave through the top into
///   `history` (or are discarded without one) and the cursor moves up with
///   the text, stopping at row zero.
///
/// The cursor column and pending wrap are untouched.
fn change_rows(screen: &mut ScreenMut<'_>, history: Option<&mut Scrollback>, rows: u16, cols: u16) {
    let lines = &mut *screen.lines;
    let old = lines.len();
    let new = usize::from(rows);
    match new.cmp(&old) {
        Ordering::Equal => {}
        Ordering::Greater => {
            let mut pulled = Vec::new();
            if let Some(history) = history {
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
            let dropped = excess.min(removable_blank_rows(lines, screen.cursor.row));
            lines.truncate(old - dropped);
            if dropped > 0
                && let Some(last) = lines.last_mut()
            {
                // A dropped row may have continued this one; nothing does
                // now.
                last.wrapped = false;
            }
            let pushed = excess - dropped;
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
}

/// How many rows at the bottom of `lines` a resize may drop instead of
/// pushing text into history: rows strictly below `cursor_row` that are
/// [`blank_row`]s. A dropped row may be the empty continuation of a
/// wrapped row (readline leaves one behind after erasing); the caller
/// clears that row's wrap flag.
fn removable_blank_rows(lines: &[Line], cursor_row: u16) -> usize {
    let below = lines
        .len()
        .saturating_sub(usize::from(cursor_row).saturating_add(1));
    lines
        .iter()
        .rev()
        .take(below)
        .take_while(|line| blank_row(line))
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
/// space with the default style, or padding ([`Cell::is_pad`]). Trailing
/// ones end a logical line's text and rows made of them may be dropped; a
/// space with any colour or attribute (a background fill, an underline,
/// inverse video) is content.
fn plain_blank(cell: &Cell) -> bool {
    *cell == Cell::BLANK || (cell.is_pad() && cell.ch == ' ')
}

/// Converts a row index known to be below `rows` (or clamps it there).
fn clamp_row(row: usize, rows: u16) -> u16 {
    u16::try_from(row).map_or(rows - 1, |row| row.min(rows - 1))
}

/// Re-wraps scrollback and screen to `size.cols` columns and makes the
/// screen the last `size.rows` rows of the result (the window is anchored
/// at the bottom). The cursor follows its character.
fn rewrap(screen: &mut ScreenMut<'_>, history: &mut Scrollback, size: Size) {
    let rows = usize::from(size.rows);
    let mut source = history.take_lines();
    let cursor = SourceCursor {
        index: source.len() + usize::from(screen.cursor.row),
        col: usize::from(screen.cursor.col),
        pending_wrap: *screen.pending_wrap,
    };
    source.append(screen.lines);
    let source_len = source.len();

    let keep = history.cap().saturating_add(rows.saturating_mul(2));
    let mut rewrapper = Rewrapper::new(size.cols, cursor, keep);
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
    let (mut out, placed, dropped) = rewrapper.finish();

    // A narrower screen makes the buffer longer; let blank rows under the
    // cursor absorb that before real lines are pushed into history. They
    // are counted after the re-wrap: a blank row that continued a wrapped
    // one has been joined into its line and is gone already.
    let total = dropped + out.len();
    if total > source_len {
        let after_cursor = placed.map_or(0, |p| total.saturating_sub(p.index + 1));
        let blank_below = out
            .iter()
            .rev()
            .take(after_cursor)
            .take_while(|line| blank_row(line))
            .count();
        let drop = (total - source_len).min(blank_below);
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
    if out.len() < rows {
        out.resize_with(rows, || Line::blank(size.cols, Style::DEFAULT));
    }
    let start = out.len() - rows;
    *screen.lines = out.split_off(start);
    let start = dropped + start;
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
    /// Cells of the logical line being re-wrapped, joined (reused).
    joined: Vec<Cell>,
    /// Its clusters: index into `joined` and text, by ascending index
    /// (reused).
    clusters: Vec<(usize, Box<str>)>,
}

impl Rewrapper {
    fn new(cols: u16, cursor: SourceCursor, keep: usize) -> Self {
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
            joined: Vec::new(),
            clusters: Vec::new(),
        }
    }

    /// The kept rows, the cursor's place and how many rows were dropped
    /// before the first kept one.
    fn finish(self) -> (Vec<Line>, Option<Placed>, usize) {
        (self.out, self.placed, self.dropped)
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
            self.split_plain(end, cursor_offset)
        } else {
            self.split(end, cursor_offset)
        };
        if let Some(offset) = cursor_offset.filter(|&offset| offset >= end) {
            self.place_past_text(offset - end, last_len);
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
    /// into rows of the new width. Returns the number of cells on the last
    /// row.
    fn split_plain(&mut self, end: usize, cursor_offset: Option<usize>) -> usize {
        let cols = usize::from(self.cols);
        let first_out = self.next_index();
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
    /// next row and leaves a padding cell ([`Cell::is_pad`]) behind. A wide
    /// character wider
    /// than the whole row (one column) is dropped. Clusters move with
    /// their cells. Returns the number of cells on the last row.
    fn split(&mut self, end: usize, cursor_offset: Option<usize>) -> usize {
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
            if width > cols {
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
                if row.cells.len() + 1 == cols {
                    // The column a wide character does not fit into is
                    // padding, so the next join drops it again.
                    row.cells.push(Cell::pad_with(Style::DEFAULT));
                }
                row.cells.resize(cols, Cell::BLANK);
                row.wrapped = true;
                let full = std::mem::replace(&mut row, empty_row(cols));
                self.push_row(full);
            }
            let col = row.cells.len();
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
        assert_eq!(removable_blank_rows(&lines, 0), 3);
        assert_eq!(removable_blank_rows(&lines, 2), 1);
        assert_eq!(removable_blank_rows(&lines, 3), 0);
        let lines = vec![row("abcd", true), row("", false), row("", false)];
        assert_eq!(
            removable_blank_rows(&lines, 0),
            2,
            "an empty continuation of a wrapped row may go too"
        );
        let mut marked = vec![row("ab", false), row("", false)];
        marked[1].marks.push(Mark::PromptStart);
        assert_eq!(removable_blank_rows(&marked, 0), 0);
        let mut filled = vec![row("ab", false), row("", false)];
        filled[1].cells[0] = Cell::blank_with(Style {
            bg: crate::Color::Indexed(4),
            ..Style::DEFAULT
        });
        assert_eq!(
            removable_blank_rows(&filled, 0),
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
            let mut rewrapper = Rewrapper::new(1, cursor, keep);
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
            let (out, placed, dropped) = rewrapper.finish();
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

    #[test]
    fn clamp_row_keeps_rows_on_screen() {
        assert_eq!(clamp_row(2, 5), 2);
        assert_eq!(clamp_row(9, 5), 4);
        assert_eq!(clamp_row(usize::MAX, 5), 4);
    }
}
