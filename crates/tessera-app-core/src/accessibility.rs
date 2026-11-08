//! The text a session's view shows, for assistive clients such as
//! `VoiceOver` (ADR 0041).
//!
//! [`App::accessible_text`] copies the rows the view shows under the
//! terminal lock (the same copy a frame's snapshot makes) and builds an
//! [`AccessibleText`] from the copy after the lock is released: the text as
//! UTF-16 (the unit `NSRange` counts in), where each line starts, and which
//! character every cell shows, so the host answers the text-area queries
//! (line of an index, range of a line, bounds of a range, character at a
//! point) without the terminal.
//!
//! # The text
//!
//! - One line per row the view shows, joined with `\n`, the last line with
//!   none; a soft-wrapped row ends with `\n` too, so line `n` is row `n`
//!   (fixture D-3).
//! - The blanks after the last text of a row are dropped, except on the
//!   cursor's row, which keeps the blanks left of the cursor so the cursor
//!   is inside the text (D-1). Blank rows below the cursor's row (below
//!   the last row with text when the view is scrolled back and shows no
//!   cursor) are dropped; blank rows above it stay, as empty lines.
//! - A wide character is one character over two cells: its right half adds
//!   no text and maps to it (D-2). A grapheme cluster gives its whole text.
//! - The text is the child's: it is sanitised (security rule S3), never
//!   logged (S8) and capped (S5). A NUL becomes a space; a control
//!   character (C0, DEL, C1) becomes U+FFFD; a bidirectional formatting
//!   control is removed and a line or paragraph separator becomes a space,
//!   so the text an assistive client reads cannot reorder or split what
//!   the screen shows. A cell left with no text shows a space. Past
//!   [`MAX_TEXT_UNITS`] UTF-16 units, cells add no text (the lines still
//!   end with their `\n`).
//!
//! [`announcement`] picks the lines new output added since an earlier
//! text, for the host to announce; it is capped at
//! [`MAX_ANNOUNCEMENT_CHARS`] characters, keeping the newest.
//!
//! [`App::accessible_text`]: crate::App::accessible_text

use std::ops::Range;

use tessera_grid::{Cell, GraphemeTable, Line, Point};
use tessera_render::{Selection as Highlight, SelectionKind};
use tessera_term::Terminal;

use crate::selection::SelectionState;

/// The most UTF-16 units of cell text an [`AccessibleText`] holds (4 Mi,
/// 8 MiB); the newlines that end its lines come on top. A full grid of the
/// largest size (`TESSERA_MAX_ROWS` x `TESSERA_MAX_COLS`) of plain
/// characters is 2 Mi units; only clusters of many scalars reach the cap.
pub const MAX_TEXT_UNITS: usize = 4 << 20;

/// The most characters an [`announcement`] holds (security rule S5): the
/// newest are kept. A character here is a cell's text or a newline, each
/// at most one Swift `Character` (an extended grapheme cluster), so the
/// host's string holds at most this many `Character`s too.
pub const MAX_ANNOUNCEMENT_CHARS: usize = 1024;

/// The rows a view shows, copied from the terminal under its lock: what
/// [`App::accessible_text`] holds the lock for. Building the text happens
/// after the lock is released ([`Capture::build`]).
///
/// [`App::accessible_text`]: crate::App::accessible_text
#[derive(Debug, Default)]
pub(crate) struct Capture {
    /// `rows * cols` cells, row by row.
    cells: Vec<Cell>,
    /// Each row's cluster text.
    clusters: Vec<GraphemeTable>,
    cols: usize,
    /// The cursor, when the view shows the live screen.
    cursor: Option<Point>,
    /// The selection, clipped to the rows shown.
    selection: Option<Highlight>,
    /// The number of the first row shown, counted in lines ever pushed
    /// into the scrollback: the same line keeps its number as output
    /// scrolls it up.
    top: u64,
    /// `Terminal::line_epoch`: line numbers compare only under one epoch
    /// (a resize or a clear to the cursor renumbers the lines).
    epoch: u64,
    /// Whether the alternate screen is shown.
    alt_screen: bool,
}

impl Capture {
    /// Everything [`App::accessible_text`] does under the selection and
    /// terminal locks (as [`crate::session::Session::with_selection`] gives
    /// them): the selection clipped to the view at `offset`, unchecked, as
    /// a frame draws it (a selection output overwrote is cleared by the
    /// next frame's check and reported at the next settle), and the rows
    /// copied ([`Self::of`]). The release test times this.
    ///
    /// [`App::accessible_text`]: crate::App::accessible_text
    pub(crate) fn of_view(selection: &SelectionState, term: &Terminal, offset: usize) -> Self {
        let highlight = selection.view_highlight(term.grid(), offset);
        Self::of(term, offset, highlight)
    }

    /// Copies the rows `term` shows `offset` lines above its live screen
    /// (the offset a view resolved, so at most the scrollback; always 0 on
    /// the alternate screen), with the view's `selection`. One straight
    /// copy of the cells and the clusters, as a frame's first capture
    /// makes (`Snapshot::capture`).
    pub(crate) fn of(term: &Terminal, offset: usize, selection: Option<Highlight>) -> Self {
        let grid = term.grid();
        let offset = if grid.is_alt_screen() {
            0
        } else {
            offset.min(grid.scrollback_len())
        };
        let cols = usize::from(grid.cols());
        let rows = usize::from(grid.rows());
        let mut capture = Self {
            cells: Vec::with_capacity(rows * cols),
            clusters: Vec::with_capacity(rows),
            cols,
            cursor: (offset == 0).then(|| grid.cursor()),
            selection,
            top: grid.scrollback_pushed().wrapping_sub(offset as u64),
            epoch: term.line_epoch(),
            alt_screen: grid.is_alt_screen(),
        };
        if offset == 0 {
            capture.copy(grid.visible_lines().iter());
        } else {
            let first = grid.scrollback_len() - offset;
            capture.copy((0..rows).filter_map(|r| grid.logical_line(first + r)));
        }
        capture
    }

    fn copy<'a>(&mut self, lines: impl Iterator<Item = &'a Line>) {
        let cols = self.cols;
        for line in lines {
            let src = &line.cells[..line.cells.len().min(cols)];
            self.cells.extend_from_slice(src);
            if src.len() < cols {
                self.cells
                    .resize(self.cells.len() + cols - src.len(), Cell::default());
            }
            self.clusters.push(line.graphemes.clone());
        }
    }

    /// The accessible text of the copied rows.
    pub(crate) fn build(&self) -> AccessibleText {
        self.build_capped(MAX_TEXT_UNITS)
    }

    /// [`Self::build`] with `cap` units of cell text at most.
    fn build_capped(&self, cap: usize) -> AccessibleText {
        let cols = self.cols;
        let rows = self.clusters.len();
        let row = |r: usize| &self.cells[r * cols..(r + 1) * cols];
        // Each row's text end: past its last cell with text, and on the
        // cursor's row at least at the cursor.
        let ends: Vec<usize> = (0..rows)
            .map(|r| {
                let cells = row(r);
                let end = cells
                    .iter()
                    .rposition(|cell| !blank(cell))
                    .map_or(0, |c| (c + usize::from(cells[c].width().max(1))).min(cols));
                match self.cursor {
                    Some(at) if usize::from(at.row) == r => end.max(usize::from(at.col).min(cols)),
                    _ => end,
                }
            })
            .collect();
        let last_text = ends.iter().rposition(|&end| end > 0);
        let kept = match self.cursor {
            Some(at) => usize::from(at.row).min(rows.saturating_sub(1)) + 1,
            None => last_text.map_or(0, |r| r + 1),
        }
        .max(last_text.map_or(0, |r| r + 1))
        .min(rows);
        let mut text: Vec<u16> = Vec::new();
        let mut line_starts = Vec::with_capacity(kept);
        let mut offsets = vec![0u32; rows * cols];
        let mut end_cols = Vec::with_capacity(kept);
        for r in 0..kept {
            if r > 0 {
                text.push(u16::from(b'\n'));
            }
            line_starts.push(unit(text.len()));
            let cells = row(r);
            let end = ends[r];
            let mut base = unit(text.len());
            for (c, cell) in cells.iter().enumerate().take(end) {
                if cell.is_spacer() && c > 0 {
                    offsets[r * cols + c] = base;
                    continue;
                }
                base = unit(text.len());
                offsets[r * cols + c] = base;
                if text.len() >= cap {
                    continue;
                }
                let cluster = cell.is_cluster().then(|| self.clusters[r].get(c)).flatten();
                let before = text.len();
                match cluster {
                    Some(cluster) => cluster.chars().for_each(|ch| push_sanitised(&mut text, ch)),
                    None => push_sanitised(&mut text, cell.ch),
                }
                if text.len() == before {
                    text.push(u16::from(b' '));
                }
            }
            let row_end = unit(text.len());
            offsets[r * cols + end..(r + 1) * cols].fill(row_end);
            end_cols.push(u16::try_from(end).unwrap_or(u16::MAX));
        }
        let length = unit(text.len());
        offsets[kept * cols..].fill(length);
        let mut accessible = AccessibleText {
            text,
            line_starts,
            end_cols,
            offsets,
            cols,
            cursor: None,
            cursor_row: None,
            selection: None,
            top: self.top,
            epoch: self.epoch,
            alt_screen: self.alt_screen,
        };
        if let Some(at) = self.cursor {
            let row = usize::from(at.row).min(kept.saturating_sub(1));
            accessible.cursor_row = Some(row);
            accessible.cursor = Some(accessible.offset_at(row, usize::from(at.col)));
        }
        accessible.selection = self
            .selection
            .map(|highlight| accessible.range_of(&highlight));
        accessible
    }
}

/// Whether `cell` shows no text: a space or NUL that is neither a cluster
/// nor a wide character's right half.
fn blank(cell: &Cell) -> bool {
    !cell.is_cluster() && !cell.is_spacer() && matches!(cell.ch, ' ' | '\0')
}

/// Appends `ch` to `text` as UTF-16, sanitised (see the module docs).
fn push_sanitised(text: &mut Vec<u16>, ch: char) {
    let ch = match ch {
        '\0' | '\u{2028}' | '\u{2029}' => ' ',
        '\u{061C}'
        | '\u{200E}'
        | '\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}' => {
            return;
        }
        ch if ch.is_control() => '\u{FFFD}',
        ch => ch,
    };
    let mut buf = [0; 2];
    text.extend_from_slice(ch.encode_utf16(&mut buf));
}

/// `n` as a UTF-16 offset: the text is capped well below `u32::MAX`.
fn unit(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The text a view shows, for assistive clients (see the module docs).
/// Offsets and lengths are UTF-16 units; lines are rows of the view.
/// `Debug` shows lengths and positions, never the text (security rule S8).
#[derive(Clone, PartialEq, Eq)]
pub struct AccessibleText {
    text: Vec<u16>,
    /// Where each line starts.
    line_starts: Vec<u32>,
    /// Each line's text end, in columns.
    end_cols: Vec<u16>,
    /// For every cell of the view, row by row, where the text of the
    /// character it shows starts; a cell past its row's text maps to the
    /// row's end, a cell of a row with no line to the text's end.
    offsets: Vec<u32>,
    cols: usize,
    cursor: Option<u32>,
    /// The cursor's line.
    cursor_row: Option<usize>,
    selection: Option<Range<u32>>,
    top: u64,
    /// [`Capture::epoch`].
    epoch: u64,
    alt_screen: bool,
}

impl std::fmt::Debug for AccessibleText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessibleText")
            .field("len", &self.text.len())
            .field("lines", &self.line_starts.len())
            .field("cols", &self.cols)
            .field("cursor", &self.cursor)
            .field("selection", &self.selection)
            .field("top", &self.top)
            .field("epoch", &self.epoch)
            .field("alt_screen", &self.alt_screen)
            .finish_non_exhaustive()
    }
}

impl AccessibleText {
    /// The text, UTF-16.
    #[must_use]
    pub fn text(&self) -> &[u16] {
        &self.text
    }

    /// The text's length in UTF-16 units.
    #[must_use]
    pub fn len(&self) -> usize {
        self.text.len()
    }

    /// Whether the text is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// How many lines the text has: 0 for an empty view.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Where the cursor is in the text, or `None` while the view is
    /// scrolled back (the cursor is not shown).
    #[must_use]
    pub fn cursor(&self) -> Option<usize> {
        self.cursor.map(|at| at as usize)
    }

    /// The insertion point: the cursor, or the end of the text when the
    /// view shows none.
    #[must_use]
    pub fn insertion_point(&self) -> usize {
        self.cursor().unwrap_or(self.len())
    }

    /// The selection clipped to the text, or `None` without one in view.
    #[must_use]
    pub fn selection(&self) -> Option<Range<usize>> {
        self.selection
            .as_ref()
            .map(|range| range.start as usize..range.end as usize)
    }

    /// The selected range: the selection, or an empty range at the
    /// insertion point without one (fixture A10).
    #[must_use]
    pub fn selected_range(&self) -> Range<usize> {
        self.selection().unwrap_or_else(|| {
            let at = self.insertion_point();
            at..at
        })
    }

    /// The line holding offset `index`; an index at or past the end gives
    /// the last line, and an empty text line 0 (fixture A5).
    #[must_use]
    pub fn line_for_index(&self, index: usize) -> usize {
        let index = unit(index);
        self.line_starts
            .partition_point(|&start| start <= index)
            .saturating_sub(1)
    }

    /// The range of line `line`, its `\n` included, or `None` past the last
    /// line (fixture A3, A4).
    #[must_use]
    pub fn range_for_line(&self, line: usize) -> Option<Range<usize>> {
        let start = *self.line_starts.get(line)? as usize;
        let end = self
            .line_starts
            .get(line + 1)
            .map_or(self.len(), |&next| next as usize);
        Some(start..end)
    }

    /// The range of the character holding offset `index`: a whole cluster,
    /// a whole surrogate pair, the `\n` alone; an empty range at the end for
    /// an index at or past it (fixture A7).
    #[must_use]
    pub fn range_for_index(&self, index: usize) -> Range<usize> {
        if index >= self.len() {
            return self.len()..self.len();
        }
        let line = self.line_for_index(index);
        let row_end = self.row_end(line);
        if index >= row_end {
            return index..index + 1;
        }
        let offsets = self.row_offsets(line);
        let at = unit(index);
        let k = offsets.partition_point(|&o| o <= at);
        let start = offsets[k - 1] as usize;
        let end = offsets.get(k).map_or(row_end, |&o| o as usize);
        start..end
    }

    /// The range of the character cell (`row`, `col`) of the view shows
    /// (fixture A8): its whole character for a cell with text (either half
    /// of a wide one), the line's last character for a cell right of the
    /// text, an empty range at the start of an empty line, and an empty
    /// range at the end for a row below the last line.
    #[must_use]
    pub fn range_at_cell(&self, row: usize, col: usize) -> Range<usize> {
        if row >= self.line_count() {
            return self.len()..self.len();
        }
        let end_col = usize::from(self.end_cols[row]);
        if end_col == 0 {
            let start = self.line_starts[row] as usize;
            return start..start;
        }
        let col = col.min(end_col - 1);
        self.range_for_index(self.offsets[row * self.cols + col] as usize)
    }

    /// The cells the range `range` (clamped to the text) covers, as the
    /// rectangle of rows and columns that encloses them: a range over
    /// several lines encloses each line's part (fixture A9). An empty range
    /// is zero columns wide, one row high, at the left edge of the cell its
    /// offset is in.
    #[must_use]
    pub fn cells_for_range(&self, range: Range<usize>) -> CellRect {
        let start = range.start.min(self.len());
        let end = range.end.clamp(start, self.len());
        let (top, left) = self.cell_of(start);
        if end == start {
            return CellRect {
                row: top,
                col: left,
                rows: 1,
                cols: 0,
            };
        }
        let (bottom, last) = self.cell_of(end - 1);
        let last_right = last + self.width_at(bottom, last, end - 1);
        let (left, right) = if bottom == top {
            (left, last_right)
        } else {
            let widest = (top..bottom)
                .map(|line| self.end_cols.get(line).map_or(0, |&c| usize::from(c)))
                .max()
                .unwrap_or(0);
            (0, widest.max(last_right))
        };
        CellRect {
            row: top,
            col: left,
            rows: bottom - top + 1,
            cols: right.saturating_sub(left),
        }
    }

    /// The (line, column) of the cell offset `index` is in: the first cell
    /// of its character, or the line's text end for its `\n` or the end.
    fn cell_of(&self, index: usize) -> (usize, usize) {
        if self.line_count() == 0 {
            return (0, 0);
        }
        let line = self.line_for_index(index);
        if index >= self.row_end(line) {
            return (line, usize::from(self.end_cols[line]));
        }
        let offsets = self.row_offsets(line);
        let at = unit(index);
        let base = offsets[offsets.partition_point(|&o| o <= at) - 1];
        (line, offsets.partition_point(|&o| o < base))
    }

    /// The columns the character at `index`, in cell (`line`, `col`),
    /// spans: none for a `\n` or the end.
    fn width_at(&self, line: usize, col: usize, index: usize) -> usize {
        if index >= self.row_end(line) {
            return 0;
        }
        let offsets = self.row_offsets(line);
        let base = offsets[col];
        let next = offsets[col..]
            .iter()
            .position(|&o| o > base)
            .unwrap_or(usize::from(self.end_cols[line]) - col);
        next.max(1)
    }

    /// The offsets of line `line`'s cells.
    fn row_offsets(&self, line: usize) -> &[u32] {
        &self.offsets[line * self.cols..(line + 1) * self.cols]
    }

    /// Where line `line`'s text ends, before its `\n`.
    fn row_end(&self, line: usize) -> usize {
        self.line_starts
            .get(line + 1)
            .map_or(self.len(), |&next| next as usize - 1)
    }

    /// The offset cell (`row`, `col`) maps to, for a row of the text.
    fn offset_at(&self, row: usize, col: usize) -> u32 {
        if row >= self.line_count() || self.cols == 0 {
            return unit(self.len());
        }
        self.offsets[row * self.cols + col.min(self.cols - 1)]
    }

    /// The text range `highlight` (in view cells) covers: from its first
    /// cell's character to the end of its last cell's character, both ends
    /// clamped to their line's text (fixture B19-B22). A block selection
    /// gives the range from its top-left to its bottom-right cell.
    fn range_of(&self, highlight: &Highlight) -> Range<u32> {
        let (mut first, mut last) = (highlight.start, highlight.end);
        if (last.row, last.col) < (first.row, first.col) {
            std::mem::swap(&mut first, &mut last);
        }
        if highlight.kind == SelectionKind::Block {
            let (left, right) = (first.col.min(last.col), first.col.max(last.col));
            first.col = left;
            last.col = right;
        }
        let start = self.offset_at(usize::from(first.row), usize::from(first.col));
        let row = usize::from(last.row);
        let end = if row >= self.line_count() {
            unit(self.len())
        } else {
            let offsets = self.row_offsets(row);
            let base = self.offset_at(row, usize::from(last.col));
            let k = offsets.partition_point(|&o| o <= base);
            offsets
                .get(k)
                .copied()
                .unwrap_or_else(|| unit(self.row_end(row)))
                .min(unit(self.row_end(row)))
        };
        start..end.max(start)
    }

    /// Line `line`'s text without its `\n`.
    fn line_text(&self, line: usize) -> &[u16] {
        let start = self.line_starts[line] as usize;
        &self.text[start..self.row_end(line)]
    }

    /// The starts of line `line`'s characters, from its offsets table.
    fn char_starts(&self, line: usize) -> Vec<usize> {
        let end = unit(self.row_end(line));
        let mut starts: Vec<usize> = self
            .row_offsets(line)
            .iter()
            .copied()
            .filter(|&o| o < end)
            .map(|o| o as usize)
            .collect();
        starts.dedup();
        starts
    }
}

/// A rectangle of cells: `rows` rows from `row` and `cols` columns from
/// `col`, view-relative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRect {
    /// The top row.
    pub row: usize,
    /// The left column.
    pub col: usize,
    /// The number of rows.
    pub rows: usize,
    /// The number of columns.
    pub cols: usize,
}

/// The text to announce for the output that turned `previous` into
/// `current` (two texts of the same view), UTF-16; empty when there is
/// nothing to announce (fixture N11-N14).
///
/// The lines announced are those from the line `previous`'s cursor was on
/// to the line `current`'s cursor is on, both included, that `current`
/// shows (by line number, so output that scrolled is followed), joined
/// with `\n`, without:
///
/// - the old cursor line when its text is unchanged (the command line
///   that was typed is not read again), blanks at its end ignored;
/// - blank lines.
///
/// Nothing is announced while either text shows no cursor (the view is
/// scrolled back), while the cursor stays on its line (typing, a progress
/// bar redrawing its line: the host's value-change notification covers
/// those), or when the lines were renumbered or replaced between the two:
/// the terminal was resized (a split, a zoom, a window resize) or cleared
/// to the cursor, or the alternate screen was entered or left (a
/// full-screen program started or quit). When the cursor moved up (the
/// screen was cleared) the lines from the top of the view to the cursor
/// are announced. Only the newest [`MAX_ANNOUNCEMENT_CHARS`]
/// characters are kept, cut between characters (fixture D-5).
#[must_use]
pub fn announcement(previous: &AccessibleText, current: &AccessibleText) -> Vec<u16> {
    let (Some(old_row), Some(new_row)) = (previous.cursor_row, current.cursor_row) else {
        return Vec::new();
    };
    if previous.epoch != current.epoch || previous.alt_screen != current.alt_screen {
        return Vec::new();
    }
    let old_line = previous.top.wrapping_add(old_row as u64);
    let new_line = current.top.wrapping_add(new_row as u64);
    if new_line == old_line {
        return Vec::new();
    }
    let first = if new_line > old_line {
        old_line.max(current.top)
    } else {
        current.top
    };
    let old_text = trim_end(previous.line_text(old_row));
    let mut picked = Vec::new();
    let mut number = first;
    while number <= new_line {
        let Ok(row) = usize::try_from(number - current.top) else {
            break;
        };
        if row >= current.line_count() {
            break;
        }
        let text = current.line_text(row);
        let unchanged_prompt = number == old_line && trim_end(text) == old_text;
        if !unchanged_prompt && !is_blank(text) {
            picked.push(row);
        }
        number += 1;
    }
    tail(current, &picked)
}

/// The lines `rows` of `text` joined with `\n`, the newest
/// [`MAX_ANNOUNCEMENT_CHARS`] characters of it.
fn tail(text: &AccessibleText, rows: &[usize]) -> Vec<u16> {
    let mut budget = MAX_ANNOUNCEMENT_CHARS;
    let mut parts: Vec<&[u16]> = Vec::new();
    for (i, &row) in rows.iter().enumerate().rev() {
        let line = text.line_text(row);
        let starts = text.char_starts(row);
        let base = text.line_starts[row] as usize;
        let joiner = usize::from(i + 1 < rows.len());
        if budget <= joiner {
            break;
        }
        budget -= joiner;
        if starts.len() <= budget {
            budget -= starts.len();
            parts.push(line);
            continue;
        }
        let cut = starts[starts.len() - budget] - base;
        parts.push(&line[cut..]);
        break;
    }
    let mut out = Vec::new();
    for (i, part) in parts.iter().rev().enumerate() {
        if i > 0 {
            out.push(u16::from(b'\n'));
        }
        out.extend_from_slice(part);
    }
    out
}

/// `text` without the spaces at its end.
fn trim_end(text: &[u16]) -> &[u16] {
    let end = text
        .iter()
        .rposition(|&u| u != u16::from(b' '))
        .map_or(0, |i| i + 1);
    &text[..end]
}

/// Whether `text` is only white space.
fn is_blank(text: &[u16]) -> bool {
    char::decode_utf16(text.iter().copied()).all(|ch| ch.is_ok_and(char::is_whitespace))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use proptest::prelude::*;
    use tessera_config::SmartRules;

    use super::*;
    use crate::selection::Granularity;
    use crate::view::Snapshot;

    /// A terminal of `rows` x `cols` fed `bytes`.
    fn term(rows: u16, cols: u16, bytes: &str) -> Terminal {
        let mut term = Terminal::new(rows, cols);
        term.advance(bytes.as_bytes());
        term
    }

    fn text_of(term: &Terminal) -> AccessibleText {
        Capture::of(term, 0, None).build()
    }

    fn with_selection(term: &Terminal, highlight: Highlight) -> AccessibleText {
        Capture::of(term, 0, Some(highlight)).build()
    }

    fn string(text: &AccessibleText) -> String {
        String::from_utf16(text.text()).unwrap()
    }

    fn string_of(units: &[u16]) -> String {
        String::from_utf16(units).unwrap()
    }

    fn sub(text: &AccessibleText, range: Range<usize>) -> String {
        String::from_utf16(&text.text()[range]).unwrap()
    }

    fn at(row: u16, col: u16) -> Point {
        Point::new(row, col)
    }

    /// B1: rows `ls` / `a.txt  b.txt` / `$ `, the cursor after the prompt.
    fn b1() -> Terminal {
        term(5, 20, "ls\r\na.txt  b.txt\r\n$ ")
    }

    /// B5: `ab` / `cd` / blank / `ef`, the cursor after `ef`.
    fn b5() -> Terminal {
        term(6, 10, "ab\r\ncd\r\n\r\nef")
    }

    /// B9: `日本語x` (three wide characters and a narrow one).
    fn b9(cursor_col: u16) -> AccessibleText {
        let term = term(2, 10, &format!("日本語x\x1b[1;{}H", cursor_col + 1));
        text_of(&term)
    }

    #[test]
    fn b1_the_cursor_row_keeps_the_blanks_left_of_the_cursor() {
        let text = text_of(&b1());
        assert_eq!(string(&text), "ls\na.txt  b.txt\n$ ");
        assert_eq!(text.len(), 18);
        assert_eq!(text.selected_range(), 18..18);
        assert_eq!(text.line_for_index(text.insertion_point()), 2);
        assert_eq!(text.selection(), None);
    }

    #[test]
    fn b2_an_empty_terminal_is_an_empty_text() {
        let text = text_of(&term(4, 10, ""));
        assert_eq!(string(&text), "");
        assert_eq!(text.selected_range(), 0..0);
        assert_eq!(text.line_for_index(0), 0);
        assert_eq!(text.line_count(), 1, "the cursor's line, empty");
        assert_eq!(text.range_for_line(0), Some(0..0));
        assert_eq!(text.range_for_line(1), None);
        assert_eq!(text.range_for_index(0), 0..0);
        assert_eq!(text.range_at_cell(0, 5), 0..0);
        assert_eq!(text.range_at_cell(3, 0), 0..0);
    }

    #[test]
    fn b3_blank_rows_below_the_cursor_row_are_dropped() {
        let text = text_of(&term(4, 10, "$"));
        assert_eq!(string(&text), "$");
        assert_eq!(text.selected_range(), 1..1);
        assert_eq!(text.line_for_index(1), 0);
    }

    #[test]
    fn b4_blank_rows_down_to_the_cursor_row_are_kept() {
        let text = text_of(&term(3, 10, "$\r\n\r\n"));
        assert_eq!(string(&text), "$\n\n");
        assert_eq!(text.selected_range(), 3..3);
        assert_eq!(text.line_for_index(3), 2);
    }

    #[test]
    fn b5_to_b8_lines_and_ranges() {
        let text = text_of(&b5());
        assert_eq!(string(&text), "ab\ncd\n\nef");
        assert_eq!(text.len(), 9);
        assert_eq!(text.selected_range(), 9..9);
        assert_eq!(text.line_for_index(9), 3);
        // B6
        let lines: Vec<_> = (0..5).map(|n| text.range_for_line(n)).collect();
        assert_eq!(
            lines,
            [Some(0..3), Some(3..6), Some(6..7), Some(7..9), None]
        );
        // B7
        let expected = [0, 0, 0, 1, 1, 1, 2, 3, 3, 3];
        for (index, line) in expected.into_iter().enumerate() {
            assert_eq!(text.line_for_index(index), line, "index {index}");
        }
        assert_eq!(text.line_for_index(100), 3);
        // B8 (substrings are the host's; the ranges are the text's)
        assert_eq!(sub(&text, 0..2), "ab");
        assert_eq!(sub(&text, 3..5), "cd");
        assert_eq!(sub(&text, 2..3), "\n");
    }

    #[test]
    fn b9_to_b11_a_wide_character_is_one_unit_over_two_cells() {
        let text = b9(7);
        assert_eq!(string(&text), "日本語x");
        assert_eq!(text.len(), 4);
        assert_eq!(text.selected_range(), 4..4);
        assert_eq!(b9(6).selected_range(), 3..3, "B10: col 6 is x");
        assert_eq!(b9(3).selected_range(), 1..1, "B11: the right half of 本");
        assert_eq!(b9(2).selected_range(), 1..1);
        assert_eq!(text.range_at_cell(0, 3), 1..2);
        assert_eq!(text.range_at_cell(0, 9), 3..4, "right of the text: x");
    }

    #[test]
    fn b12_an_astral_wide_character_is_a_surrogate_pair() {
        let text = text_of(&term(2, 10, "😀a\x1b[1;3H"));
        assert_eq!(string(&text), "😀a");
        assert_eq!(text.len(), 3);
        assert_eq!(text.selected_range(), 2..2);
        assert_eq!(text.range_for_index(0), 0..2);
        assert_eq!(text.range_for_index(1), 0..2);
        assert_eq!(text.range_for_index(2), 2..3);
    }

    #[test]
    fn b13_a_combining_sequence_is_one_character() {
        let text = text_of(&term(2, 10, "e\u{301}x\x1b[1;2H"));
        assert_eq!(string(&text), "e\u{301}x");
        assert_eq!(text.selected_range(), 2..2);
        assert_eq!(text.range_for_index(1), 0..2);
        assert_eq!(text.range_at_cell(0, 0), 0..2);
    }

    #[test]
    fn b14_a_zwj_sequence_is_one_character_whatever_its_width() {
        let family = "👨\u{200D}👩\u{200D}👧";
        let term = term(2, 12, &format!("{family}x"));
        let width = term.grid().visible_lines()[0]
            .cells
            .iter()
            .position(|cell| cell.ch == 'x')
            .unwrap();
        let text = Capture::of(&term, 0, None).build();
        assert_eq!(string(&text), format!("{family}x"));
        assert_eq!(text.len(), 9);
        assert_eq!(text.range_for_index(3), 0..8);
        assert_eq!(text.range_at_cell(0, width), 8..9);
        let moved = Capture {
            cursor: Some(at(0, u16::try_from(width).unwrap())),
            ..Capture::of(&term, 0, None)
        }
        .build();
        assert_eq!(moved.selected_range(), 8..8);
    }

    #[test]
    fn b15_b16_a_soft_wrapped_row_ends_its_line_and_loses_trailing_blanks() {
        let text = text_of(&term(3, 3, "abcd"));
        assert_eq!(string(&text), "abc\nd");
        let text = text_of(&term(3, 5, "x   \x1b[1;2H"));
        assert_eq!(string(&text), "x");
        assert_eq!(text.selected_range(), 1..1);
    }

    #[test]
    fn b17_empty_cells_inside_a_row_are_spaces() {
        let text = text_of(&term(2, 10, "a\x1b[3Cb"));
        assert_eq!(string(&text), "a   b");
        assert!(!text.text().contains(&0));
    }

    /// B18 (S3): controls in cells become U+FFFD, bidi controls go, line
    /// separators become spaces, a NUL is a space; nothing an assistive
    /// client reads reorders or splits the row.
    #[test]
    fn b18_child_text_is_sanitised() {
        let mut term = Terminal::new(2, 20);
        term.advance(b"ok");
        let mut capture = Capture::of(&term, 0, None);
        let row = [
            'a', '\u{202E}', 'b', '\u{0085}', 'c', '\u{2028}', 'd', '\0', 'e', '\u{7f}', 'f',
            '\u{2067}', 'g', '\u{1b}',
        ];
        for (cell, ch) in capture.cells.iter_mut().zip(row) {
            cell.ch = ch;
        }
        capture.cursor = Some(at(0, 0));
        let text = capture.build();
        assert_eq!(string(&text), "a b\u{FFFD}c d e\u{FFFD}f g\u{FFFD}");
        for unit in text.text() {
            let ch = char::from_u32(u32::from(*unit)).unwrap_or('x');
            assert!(!ch.is_control(), "{ch:?}");
            assert!(!matches!(ch, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'));
        }
        let mut cluster = Terminal::new(2, 20);
        cluster.advance("x\u{301}\u{200F}y".as_bytes());
        let text = text_of(&cluster);
        assert!(
            !text.text().contains(&0x200F),
            "a bidi mark in a cluster is removed: {:?}",
            string(&text)
        );
    }

    /// B18, every listed code point: each bidi control is removed, the
    /// ends of the C1 range become U+FFFD, and the neighbours of the
    /// removed ranges are kept.
    #[test]
    fn b18_every_bidi_control_goes_and_c1_is_replaced() {
        let removed = [
            '\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}',
            '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ];
        let kept = [
            ('\u{0080}', "\u{FFFD}"),
            ('\u{009F}', "\u{FFFD}"),
            ('\u{2029}', " "),
            ('\u{202F}', "\u{202F}"),
            ('\u{206A}', "\u{206A}"),
        ];
        let table = removed.iter().map(|&ch| (ch, "")).chain(kept);
        for (ch, expected) in table {
            let mut text = Vec::new();
            push_sanitised(&mut text, ch);
            let got = String::from_utf16_lossy(&text);
            assert_eq!(got, expected, "U+{:04X}", u32::from(ch));
        }
    }

    /// A terminal of `rows` x `cols` that printed 40 lines and a prompt.
    fn history(rows: u16, cols: u16) -> Terminal {
        let mut term = Terminal::new(rows, cols);
        for i in 0..40 {
            term.advance(format!("line {i:02} some output text here\r\n").as_bytes());
        }
        term.advance(b"$ ");
        term
    }

    /// N15: a resize renumbers the lines (a re-wrap moves rows between the
    /// screen and the history): fewer rows, other columns, a split or zoom
    /// and back to the same size announce nothing; output after it is
    /// announced again.
    #[test]
    fn n15_a_resize_announces_nothing_but_later_output_is() {
        for (rows, cols) in [(11, 80), (24, 66), (40, 80), (11, 40)] {
            let mut term = history(24, 80);
            let before = text_of(&term);
            term.resize(rows, cols);
            let after = text_of(&term);
            assert!(announcement(&before, &after).is_empty(), "{rows}x{cols}");
            term.advance(b"new\r\n$ ");
            assert_eq!(
                string_of(&announcement(&after, &text_of(&term))),
                "$ new\n$ ",
                "{rows}x{cols}"
            );
        }
        let mut term = history(24, 80);
        let before = text_of(&term);
        term.resize(11, 40);
        term.resize(24, 80);
        assert!(
            announcement(&before, &text_of(&term)).is_empty(),
            "a split and back to the same size"
        );
    }

    /// N16: entering or leaving the alternate screen announces nothing
    /// (quitting a full-screen program does not read the old output); output
    /// after it is announced again.
    #[test]
    fn n16_switching_screens_announces_nothing() {
        let mut term = history(6, 40);
        let primary = text_of(&term);
        term.advance(b"\x1b[?1049h\x1b[H\x1b[2Jeditor line one\r\nline two\r\nline three");
        let alt = text_of(&term);
        assert!(announcement(&primary, &alt).is_empty(), "entering");
        term.advance(b"\x1b[?1049l");
        let back = text_of(&term);
        assert!(announcement(&alt, &back).is_empty(), "leaving");
        term.advance(b"\r\nnew\r\n$ ");
        assert_eq!(string_of(&announcement(&back, &text_of(&term))), "new\n$ ");
    }

    /// S8: `Debug` shows lengths, not the child's text.
    #[test]
    fn debug_leaves_the_text_out() {
        let text = text_of(&term(2, 20, "secret"));
        let debug = format!("{text:?}");
        assert!(
            !debug.contains("secret") && !debug.contains("115"),
            "{debug}"
        );
        assert!(debug.contains("len: "), "{debug}");
    }

    #[test]
    fn b19_b20_a_selection_is_its_characters() {
        let text = with_selection(&b1(), Highlight::linear(at(1, 0), at(1, 4)));
        assert_eq!(text.selected_range(), 3..8);
        assert_eq!(sub(&text, 3..8), "a.txt");
        assert_eq!(text.line_for_index(text.insertion_point()), 2);
        let text = with_selection(&b1(), Highlight::linear(at(0, 1), at(1, 2)));
        assert_eq!(text.selected_range(), 1..6);
        assert_eq!(sub(&text, 1..6), "s\na.t");
    }

    #[test]
    fn b21_b22_selection_ends_clamp_to_the_text_and_snap_to_characters() {
        let text = with_selection(&b1(), Highlight::linear(at(0, 0), at(0, 15)));
        assert_eq!(
            text.selected_range(),
            0..2,
            "the end clamps to the row's text"
        );
        let text = with_selection(&b1(), Highlight::linear(at(0, 10), at(1, 0)));
        assert_eq!(text.selected_range(), 2..4);
        let wide = term(2, 10, "日本語x");
        let text = with_selection(&wide, Highlight::linear(at(0, 3), at(0, 4)));
        assert_eq!(text.selected_range(), 1..3, "both halves snap whole");
        let text = with_selection(&wide, Highlight::linear(at(0, 6), at(0, 2)));
        assert_eq!(text.selected_range(), 1..4, "reversed ends are ordered");
        let text = with_selection(&wide, Highlight::linear(at(1, 0), at(1, 5)));
        assert_eq!(
            text.selected_range(),
            4..4,
            "below the text: empty at its end"
        );
    }

    /// C1-C8 in cells (the host multiplies by the cell size and converts
    /// to screen coordinates, C9).
    #[test]
    fn c_rows_cells_for_ranges() {
        let rect = |row, col, rows, cols| CellRect {
            row,
            col,
            rows,
            cols,
        };
        let text = text_of(&b1());
        assert_eq!(text.cells_for_range(3..8), rect(1, 0, 1, 5), "C1");
        assert_eq!(text.cells_for_range(0..5), rect(0, 0, 2, 2), "C2");
        assert_eq!(text.cells_for_range(18..18), rect(2, 2, 1, 0), "C6");
        assert_eq!(text.cells_for_range(0..0), rect(0, 0, 1, 0), "C7");
        assert_eq!(text.cells_for_range(17..100), rect(2, 1, 1, 1), "C8 clamps");
        assert_eq!(
            text.cells_for_range(50..60),
            rect(2, 2, 1, 0),
            "C8 past the end"
        );
        assert_eq!(
            text.cells_for_range(0..18),
            rect(0, 0, 3, 12),
            "the whole text"
        );
        let wide = b9(7);
        assert_eq!(wide.cells_for_range(1..2), rect(0, 2, 1, 2), "C3");
        assert_eq!(wide.cells_for_range(3..4), rect(0, 6, 1, 1), "C4");
        let astral = text_of(&term(2, 10, "😀a"));
        assert_eq!(astral.cells_for_range(2..3), rect(0, 2, 1, 1), "C5");
        assert_eq!(astral.cells_for_range(0..2), rect(0, 0, 1, 2));
    }

    /// The scrolled-back view: the rows shown, no cursor, the insertion
    /// point at the end.
    #[test]
    fn a_scrolled_view_reads_the_history_it_shows() {
        let mut term = Terminal::new(3, 10);
        for n in 0..10 {
            term.advance(format!("line {n}\r\n").as_bytes());
        }
        let text = Capture::of(&term, 4, None).build();
        assert_eq!(string(&text), "line 4\nline 5\nline 6");
        assert_eq!(text.cursor(), None);
        assert_eq!(text.selected_range(), text.len()..text.len());
        let live = text_of(&term);
        assert_eq!(string(&live), "line 8\nline 9\n");
    }

    /// S5: past the cap cells add no text, every line still ends and every
    /// lookup stays inside the text.
    #[test]
    fn the_text_is_capped() {
        let term = term(4, 10, "abcdefghij\r\nklmnopqrst\r\nuv");
        let text = Capture::of(&term, 0, None).build_capped(12);
        assert_eq!(string(&text), "abcdefghij\nk\n");
        assert_eq!(text.line_count(), 3);
        assert!(text.insertion_point() <= text.len());
        for row in 0..4 {
            for col in 0..10 {
                assert!(text.range_at_cell(row, col).end <= text.len());
            }
        }
    }

    fn lines_text(lines: &[&str], cursor_row: u16, cursor_col: u16, top: u64) -> AccessibleText {
        let rows = u16::try_from(lines.len()).unwrap().max(cursor_row + 1);
        let bytes = format!(
            "{}\x1b[{};{}H",
            lines.join("\r\n"),
            cursor_row + 1,
            cursor_col + 1
        );
        let term = term(rows + 1, 40, &bytes);
        Capture {
            top,
            ..Capture::of(&term, 0, None)
        }
        .build()
    }

    fn announced(previous: &AccessibleText, current: &AccessibleText) -> String {
        String::from_utf16(&announcement(previous, current)).unwrap()
    }

    /// N12 (a): the old cursor line, unchanged, is not read again, nor is a
    /// blank line.
    #[test]
    fn n12_a_the_unchanged_cursor_line_and_blank_lines_are_skipped() {
        let before = lines_text(&["pwd   "], 0, 6, 100);
        let after = lines_text(&["pwd", "", "Users/deonnel"], 2, 13, 100);
        assert_eq!(announced(&before, &after), "Users/deonnel");
    }

    /// N12 (b): a changed old cursor line is read.
    #[test]
    fn n12_b_a_changed_cursor_line_is_read() {
        let before = lines_text(&["echo hello"], 0, 10, 7);
        let after = lines_text(&["hello", "world"], 1, 5, 7);
        assert_eq!(announced(&before, &after), "hello\nworld");
    }

    /// N12 (c): only the original cursor line is skipped, not a later line
    /// with the same text.
    #[test]
    fn n12_c_only_the_original_cursor_line_is_skipped() {
        let before = lines_text(&["prompt"], 0, 6, 0);
        let after = lines_text(&["prompt", "prompt"], 1, 6, 0);
        assert_eq!(announced(&before, &after), "prompt");
    }

    /// `ls` at a prompt, through a real terminal: the listing and the new
    /// prompt, not the command line.
    #[test]
    fn output_after_a_command_announces_the_new_lines() {
        let mut term = Terminal::new(6, 30);
        term.advance(b"$ ls");
        let before = text_of(&term);
        term.advance(b"\r\na.txt  b.txt\r\n$ ");
        let after = text_of(&term);
        assert_eq!(announced(&before, &after), "a.txt  b.txt\n$ ");
        assert_eq!(announced(&after, &after), "", "nothing new");
    }

    /// Output that scrolls the old cursor line off the view announces the
    /// lines the view shows, followed by line number.
    #[test]
    fn output_that_scrolls_announces_what_the_view_shows() {
        let mut term = Terminal::new(3, 20);
        term.advance(b"$ seq");
        let before = text_of(&term);
        term.advance(b"\r\n1\r\n2\r\n3\r\n4\r\n$ ");
        let after = text_of(&term);
        assert_eq!(announced(&before, &after), "3\n4\n$ ");
    }

    /// The cursor staying on its line (typing) announces nothing, and so
    /// does a scrolled-back view; the cursor moving up (a cleared screen)
    /// announces from the top of the view.
    #[test]
    fn typing_and_scrolled_views_announce_nothing() {
        let mut term = Terminal::new(4, 20);
        term.advance(b"one\r\ntwo\r\n$ ");
        let before = text_of(&term);
        term.advance(b"l");
        assert_eq!(announced(&before, &text_of(&term)), "");
        term.advance(b"s\r\nout\r\n");
        let scrolled = Capture::of(&term, 1, None).build();
        assert_eq!(announced(&before, &scrolled), "");
        assert_eq!(announced(&scrolled, &text_of(&term)), "");
        term.advance(b"\x1b[H\x1b[2J$ ");
        assert_eq!(announced(&before, &text_of(&term)), "$ ");
    }

    /// N14: a whitespace-only announcement is empty.
    #[test]
    fn n14_white_space_only_is_not_announced() {
        let before = lines_text(&["$ x"], 0, 3, 0);
        let after = lines_text(&["$ x", "   ", "  "], 2, 2, 0);
        assert_eq!(announced(&before, &after), "");
    }

    /// N13 / D-5: at most 1024 characters, the newest kept, cut between
    /// characters, no marker.
    #[test]
    fn n13_the_announcement_keeps_the_newest_1024_characters() {
        let mut term = Terminal::new(40, 100);
        term.advance(b"$ cat");
        let before = text_of(&term);
        for n in 0..30 {
            // 2 + 47 wide characters = 96 columns, 49 characters a line.
            term.advance(format!("\r\n{n:02}{}", "語".repeat(47)).as_bytes());
        }
        term.advance(b"\r\n$ ");
        let after = text_of(&term);
        let text = String::from_utf16(&announcement(&before, &after)).unwrap();
        assert_eq!(text.chars().count(), MAX_ANNOUNCEMENT_CHARS);
        assert!(text.ends_with("語\n$ "), "the tail");
        let full: String = (0..30)
            .map(|n| format!("{n:02}{}\n", "語".repeat(47)))
            .chain(["$ ".to_owned()])
            .collect();
        assert!(full.ends_with(&text));

        let emoji = "😀".repeat(40);
        let mut term = Terminal::new(40, 100);
        term.advance(b"$");
        let before = text_of(&term);
        for _ in 0..30 {
            term.advance(format!("{emoji}\r\n").as_bytes());
        }
        let text = String::from_utf16(&announcement(&before, &text_of(&term))).unwrap();
        assert!(text.chars().count() <= MAX_ANNOUNCEMENT_CHARS);
        assert!(
            text.starts_with('😀'),
            "no half surrogate pair: {:?}",
            &text[..8]
        );
    }

    proptest! {
        /// B23: for any output the length is the text's, the lines tile it,
        /// the cursor is inside it on a character boundary, every index is
        /// in its line, and every cell maps inside the text.
        #[test]
        fn b23_invariants(
            bytes in proptest::collection::vec(
                prop_oneof![
                    Just("a".to_owned()), Just(" ".to_owned()), Just("\r\n".to_owned()),
                    Just("日".to_owned()), Just("😀".to_owned()), Just("e\u{301}".to_owned()),
                    Just("\x1b[3C".to_owned()), Just("\x1b[H".to_owned()), Just("\x1b[2;4H".to_owned()),
                    Just("\x1b[K".to_owned()), Just("\x1b[2J".to_owned()), Just("\u{202E}".to_owned()),
                ],
                0..60,
            ),
            sel in (0u16..6, 0u16..9, 0u16..6, 0u16..9),
        ) {
            let term = term(5, 8, &bytes.concat());
            let text = with_selection(&term, Highlight::linear(at(sel.0, sel.1), at(sel.2, sel.3)));
            let len = text.len();
            let mut next = 0;
            for line in 0..text.line_count() {
                let range = text.range_for_line(line).unwrap();
                prop_assert_eq!(range.start, next);
                next = range.end;
            }
            prop_assert_eq!(next, len);
            prop_assert!(text.range_for_line(text.line_count()).is_none());
            let cursor = text.insertion_point();
            prop_assert!(cursor <= len);
            if cursor < len {
                prop_assert_eq!(text.range_for_index(cursor).start, cursor);
            }
            for index in 0..=len {
                let line = text.line_for_index(index);
                let range = text.range_for_line(line).unwrap_or(0..0);
                prop_assert!(range.contains(&index) || index == len, "{} in {:?}", index, range);
                let char_range = text.range_for_index(index);
                prop_assert!(char_range.start <= index && char_range.end <= len);
                let rect = text.cells_for_range(index..len);
                prop_assert!(rect.col + rect.cols <= 8 && rect.row + rect.rows <= 5);
            }
            let selected = text.selected_range();
            prop_assert!(selected.start <= selected.end && selected.end <= len);
            for row in 0..6 {
                for col in 0..9 {
                    prop_assert!(text.range_at_cell(row, col).end <= len);
                }
            }
        }
    }

    /// Everything `App::accessible_text` does under the terminal lock
    /// ([`Capture::of_view`]) costs no more than a frame's first capture of
    /// the same grid (`Snapshot::capture`, which copies it whole): a full
    /// 50x200 grid with clusters, a 46-row live selection made and one line
    /// of output printed before each call (so a check of the selection
    /// would compare every line it guards), best of 300 each, within 1.25x,
    /// three attempts as the other release timing bars. Building the text
    /// happens after the lock is released and is not timed. Release only.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
    fn accessible_text_lock_hold_is_no_longer_than_a_capture() {
        const ATTEMPTS: usize = 3;
        let mut term = Terminal::new(50, 200);
        let line: String = "The quick brown fox e\u{301} 日本 😀 jumps "
            .chars()
            .cycle()
            .take(195)
            .collect();
        let output = format!("{line}\r\n");
        for _ in 0..60 {
            term.advance(output.as_bytes());
        }
        let mut selection = SelectionState::default();
        let mut best = |timed: &mut dyn FnMut(&mut SelectionState, &Terminal)| {
            let mut best = Duration::MAX;
            for _ in 0..300 {
                let grid = term.grid();
                selection.begin(
                    grid,
                    0,
                    Point::new(2, 3),
                    Granularity::Line,
                    false,
                    &SmartRules::default(),
                );
                selection.extend(grid, 0, Point::new(47, 3));
                term.advance(output.as_bytes());
                let start = Instant::now();
                timed(&mut selection, &term);
                best = best.min(start.elapsed());
                assert!(
                    selection.view_highlight(term.grid(), 0).is_some(),
                    "the selection stays live"
                );
            }
            best
        };
        let mut failures = Vec::new();
        for attempt in 1..=ATTEMPTS {
            let capture = best(&mut |_, term| {
                let mut snapshot = Snapshot::default();
                std::hint::black_box(snapshot.capture(term, 0));
                std::hint::black_box(&snapshot);
            });
            let locked = best(&mut |selection, term| {
                std::hint::black_box(Capture::of_view(selection, term, 0));
            });
            let ratio = locked.as_secs_f64() / capture.as_secs_f64();
            eprintln!("attempt {attempt}: capture {capture:?}, locked {locked:?} ({ratio:.3}x)");
            if ratio <= 1.25 {
                return;
            }
            failures.push(format!("attempt {attempt}: {ratio:.3}x"));
        }
        panic!("{failures:#?}");
    }
}
