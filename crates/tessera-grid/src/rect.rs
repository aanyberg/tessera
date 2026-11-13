//! Rectangular area operations on the visible screen (ADR 0046): erase
//! (DECERA), fill (DECFRA), copy (DECCRA) and selective erase (DECSERA).
//!
//! The terminal parses the sequences, applies origin mode and clamps the
//! parameters; these methods take a [`Rect`] in absolute screen cells and
//! clip it to the screen again, so no call writes outside the screen
//! whatever it is given.
//!
//! What every operation keeps: the cursor and its pending wrap, the pen,
//! the scroll region and margins, the tab stops, the scrollback, the
//! lines' soft-wrap flags and OSC 133 marks (a rectangle is not a line, so
//! the per-line metadata stays with the line). Only the rows the clipped
//! rectangle covers (for a copy: its destination rows) are damaged.
//!
//! A wide character cut by an edge of a rectangle is blanked whole, so no
//! half is ever left unpaired: an erased or filled half takes the other
//! with it, and a copied half that lost its partner arrives as a blank.
//!
//! Work and memory: each call costs a constant per cell of the clipped
//! rectangle and emits no per-cell event. Erase, fill and selective erase
//! allocate nothing. A copy of plain text allocates one scratch row of at
//! most the screen's width per call; a copy that carries clusters or links
//! also uses scratch lists of them (each at most one row's entries: the
//! copied ones and the destination row's own while they are merged back in
//! column order) and one allocation per copied cluster's text.

use crate::{Cell, CellFlags, Grid, Hyperlink, Style};

/// A rectangle of screen cells, all four edges inclusive and 0-based:
/// rows `top..=bottom`, columns `left..=right`.
///
/// The methods that take one clip it to the screen; one whose `top` is
/// below its `bottom` or whose `left` is right of its `right` (after the
/// clip) is empty and changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rect {
    /// The first row.
    pub top: u16,
    /// The first column.
    pub left: u16,
    /// The last row (inclusive).
    pub bottom: u16,
    /// The last column (inclusive).
    pub right: u16,
}

impl Rect {
    /// `self` clipped to a screen of `rows` x `cols`, or `None` when
    /// nothing of it is left.
    fn clip(self, rows: u16, cols: u16) -> Option<Self> {
        if rows == 0 || cols == 0 {
            return None;
        }
        let clipped = Self {
            top: self.top,
            left: self.left,
            bottom: self.bottom.min(rows - 1),
            right: self.right.min(cols - 1),
        };
        (clipped.top <= clipped.bottom && clipped.left <= clipped.right).then_some(clipped)
    }

    /// The columns as a half-open range.
    fn cols(self) -> std::ops::Range<usize> {
        usize::from(self.left)..usize::from(self.right) + 1
    }
}

impl Grid {
    /// Erases `rect` (DECERA): every cell becomes a blank carrying only the
    /// background of `pen`, whether protected or not; clusters and links
    /// in it are dropped.
    ///
    /// The blanks never carry [`Style::PROTECTED`] or any other attribute
    /// of the pen but its background, like every erase (ADR 0034).
    pub fn erase_rect(&mut self, rect: Rect, pen: Style) {
        let Some(rect) = rect.clip(self.rows(), self.cols()) else {
            return;
        };
        for row in rect.top..=rect.bottom {
            if let Some(line) = self.line_mut(row) {
                line.fill(rect.cols(), pen);
            }
        }
    }

    /// Fills `rect` with `cell` (DECFRA): every cell becomes a copy of it,
    /// style included ([`Style::PROTECTED`] too, as printing would), and
    /// loses its cluster and link.
    ///
    /// `cell` must be a plain one-column character: its flags are ignored
    /// and a control character, or a character that is not one column
    /// wide, leaves the screen unchanged (the terminal never passes one).
    pub fn fill_rect(&mut self, rect: Rect, cell: Cell) {
        if cell.ch.is_control() || crate::char_width(cell.ch) != Some(1) {
            return;
        }
        let Some(rect) = rect.clip(self.rows(), self.cols()) else {
            return;
        };
        let cell = Cell::new(cell.ch, cell.style);
        for row in rect.top..=rect.bottom {
            if let Some(line) = self.line_mut(row) {
                // Blanks a wide character cut by either edge and drops the
                // clusters and links of the span.
                line.fill(rect.cols(), cell.style);
                line.cells[rect.cols()].fill(cell);
            }
        }
    }

    /// Selectively erases `rect` (DECSERA): every cell becomes a space but
    /// keeps its style (colours, attributes and [`Style::PROTECTED`]),
    /// and loses its cluster and link.
    ///
    /// With `honour_protection` (DECSCA protection is in force) a protected
    /// cell ([`Cell::is_protected`]) is kept as it is; without, every cell
    /// is erased, as xterm does for ISO (SPA/EPA) protection. A wide
    /// character cut by an edge is erased whole unless it is kept.
    pub fn selective_erase_rect(&mut self, rect: Rect, honour_protection: bool) {
        let Some(rect) = rect.clip(self.rows(), self.cols()) else {
            return;
        };
        let erasable = |cell: &Cell| !(honour_protection && cell.is_protected());
        for row in rect.top..=rect.bottom {
            let Some(line) = self.line_mut(row) else {
                continue;
            };
            let cols = rect.cols();
            // A wide character cut by an edge goes whole; both its halves
            // share a style, so they are both erasable or both kept.
            let lo = if line.cells[cols.start].is_spacer() && cols.start > 0 {
                cols.start - 1
            } else {
                cols.start
            };
            let hi = if line.cells[cols.end - 1].is_wide() && cols.end < line.cells.len() {
                cols.end + 1
            } else {
                cols.end
            };
            let mut side = false;
            for cell in &mut line.cells[lo..hi] {
                if erasable(cell) {
                    side |= cell.is_cluster() || cell.is_link();
                    *cell = Cell::new(' ', cell.style);
                }
            }
            if side {
                let cells = &line.cells;
                line.graphemes
                    .retain_cols(|col| col < lo || col >= hi || !erasable(&cells[col]));
            }
        }
    }

    /// Copies the cells of `src` to the rectangle of the same size whose
    /// top-left cell is (`dst_top`, `dst_left`) (DECCRA), clipped to `clip`
    /// (the screen, or the origin region in origin mode) and to the
    /// screen.
    ///
    /// Text, style ([`Style::PROTECTED`] included), clusters and links are
    /// copied; overlapping rectangles copy as if through a buffer. A wide
    /// character whose other half lies outside `src` arrives as a blank of
    /// its own background, and one in the destination that loses a half
    /// to the copy is blanked whole with the background of `pen`. A
    /// destination at or past an edge of `clip` copies nothing.
    pub fn copy_rect(&mut self, src: Rect, dst_top: u16, dst_left: u16, clip: Rect, pen: Style) {
        let (rows, cols) = (self.rows(), self.cols());
        let (Some(src), Some(clip)) = (src.clip(rows, cols), clip.clip(rows, cols)) else {
            return;
        };
        if dst_top < clip.top || dst_top > clip.bottom || dst_left < clip.left {
            return;
        }
        if dst_left > clip.right {
            return;
        }
        let height = (src.bottom - src.top).min(clip.bottom - dst_top) + 1;
        let width = usize::from((src.right - src.left).min(clip.right - dst_left)) + 1;
        let (sl, dl) = (usize::from(src.left), usize::from(dst_left));
        // Scratch of one row, so a row copied onto itself (or onto an
        // overlapping span of itself) reads its source before it is
        // written; rows are visited so no row is written before it is read.
        let mut cells: Vec<Cell> = Vec::with_capacity(width);
        let mut clusters: Vec<(usize, Box<str>)> = Vec::new();
        let mut links: Vec<(usize, Hyperlink)> = Vec::new();
        // The destination row's own entries while the copy is merged in.
        let mut old_clusters: Vec<(usize, Box<str>)> = Vec::new();
        let mut old_links: Vec<(usize, Hyperlink)> = Vec::new();
        let down = dst_top > src.top;
        for i in 0..height {
            let i = if down { height - 1 - i } else { i };
            let Some(from) = self.line(src.top + i) else {
                continue;
            };
            cells.clear();
            cells.extend_from_slice(&from.cells[sl..sl + width]);
            if cells[0].is_spacer() {
                cells[0] = Cell::blank_with(cells[0].style);
            }
            if cells[width - 1].is_wide() {
                cells[width - 1] = Cell::blank_with(cells[width - 1].style);
            }
            for cell in &mut cells {
                cell.flags = cell.flags.difference(CellFlags::PAD);
            }
            clusters.clear();
            links.clear();
            if !from.graphemes.is_empty() {
                let at = |col: usize| (sl..sl + width).contains(&col).then(|| col - sl);
                clusters.extend(from.graphemes.iter().filter_map(|(col, text)| {
                    at(col)
                        .filter(|&off| cells[off].is_cluster())
                        .map(|off| (off, text.into()))
                }));
                links.extend(from.graphemes.links().filter_map(|(col, link)| {
                    at(col)
                        .filter(|&off| cells[off].is_link())
                        .map(|off| (off, link.clone()))
                }));
            }
            let Some(to) = self.line_mut(dst_top + i) else {
                continue;
            };
            to.split_wide_at_edges(dl..dl + width, pen);
            to.cells[dl..dl + width].copy_from_slice(&cells);
            to.graphemes.remove_range(dl..dl + width);
            // The table keeps each kind sorted by column, and a `set` left
            // of other entries moves them all: with entries right of the
            // span, setting the copied ones one by one would cost the row's
            // width per entry. So the row's entries are taken out and set
            // back in column order (left of the span, the copy, right of
            // it), every `set` an append: linear in the row.
            if !clusters.is_empty() {
                old_clusters.clear();
                old_clusters.extend(to.graphemes.take());
                let split = old_clusters.partition_point(|&(col, _)| col < dl);
                let copied = clusters.drain(..).map(|(off, text)| (dl + off, text));
                let mut old = old_clusters.drain(..);
                for (col, text) in old.by_ref().take(split).chain(copied) {
                    to.graphemes.set(col, text);
                }
                for (col, text) in old {
                    to.graphemes.set(col, text);
                }
            }
            if !links.is_empty() {
                old_links.clear();
                old_links.extend(to.graphemes.take_links());
                let split = old_links.partition_point(|&(col, _)| col < dl);
                let copied = links.drain(..).map(|(off, link)| (dl + off, link));
                let mut old = old_links.drain(..);
                for (col, link) in old.by_ref().take(split).chain(copied) {
                    to.graphemes.set_link(col, link);
                }
                for (col, link) in old {
                    to.graphemes.set_link(col, link);
                }
            }
        }
    }
}
