//! Lines and the semantic marks shell integration attaches to them.

use crate::{Cell, CellFlags, GraphemeTable, Style};

/// Semantic annotation attached to a line by shell integration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mark {
    /// A prompt begins on this line.
    PromptStart,
    /// The user's command input begins here.
    CommandStart,
    /// Command output begins here.
    OutputStart,
    /// The command finished with this exit code.
    CommandEnd {
        /// Process exit status, if reported.
        exit_code: Option<i32>,
    },
}

/// One row of cells plus optional marks.
///
/// # Wide characters and clusters
///
/// A character two columns wide occupies a [`CellFlags::WIDE`] cell and
/// the [`CellFlags::SPACER`] right after it; the pair never straddles the
/// end of the line. A cell whose grapheme cluster has more than one scalar
/// carries [`CellFlags::CLUSTER`] and its full text lives in
/// [`Line::graphemes`]. Code that edits `cells` directly must keep both
/// rules; the line's own methods and every [`Grid`] primitive do.
///
/// [`CellFlags::WIDE`]: crate::CellFlags::WIDE
/// [`CellFlags::SPACER`]: crate::CellFlags::SPACER
/// [`CellFlags::CLUSTER`]: crate::CellFlags::CLUSTER
/// [`Grid`]: crate::Grid
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    /// Cells, exactly `cols` long.
    pub cells: Vec<Cell>,
    /// Semantic marks on this line.
    pub marks: Vec<Mark>,
    /// Cluster text of the line's [`CellFlags::CLUSTER`] cells, by column.
    ///
    /// [`CellFlags::CLUSTER`]: crate::CellFlags::CLUSTER
    pub graphemes: GraphemeTable,
    /// True when this line continues onto the next (soft wrap).
    pub wrapped: bool,
}

impl Line {
    /// A line of `cols` blank cells carrying the background of `style`.
    pub fn blank(cols: u16, style: Style) -> Self {
        Self {
            cells: vec![Cell::blank_with(style); usize::from(cols)],
            marks: Vec::new(),
            graphemes: GraphemeTable::new(),
            wrapped: false,
        }
    }

    /// The full text of the grapheme cluster at `col` (base scalar
    /// included) when that cell is a [`CellFlags::CLUSTER`] cell, else
    /// `None`. A plain cell's text is just its [`Cell::ch`].
    ///
    /// [`CellFlags::CLUSTER`]: crate::CellFlags::CLUSTER
    pub fn cluster(&self, col: usize) -> Option<&str> {
        self.graphemes.get(col)
    }

    /// Overwrites every cell in `range` (clamped to the line) with a blank
    /// carrying the background of `style`.
    ///
    /// A wide character cut by either end of the range is blanked whole:
    /// a range that starts on a spacer also blanks the wide cell to its
    /// left, and one that ends on a wide cell also blanks its spacer.
    /// Clusters in the blanked cells are dropped.
    pub fn fill(&mut self, range: std::ops::Range<usize>, style: Style) {
        let end = range.end.min(self.cells.len());
        let start = range.start.min(end);
        if start == end {
            return;
        }
        if self.cells[start].is_spacer() && start > 0 {
            self.blank_at(start - 1, style);
        }
        if self.cells[end - 1].is_wide() {
            self.blank_at(end, style);
        }
        self.blank_raw(start..end, style);
    }

    /// Resets the line to blanks and drops its marks, clusters and wrap
    /// flag.
    pub fn clear(&mut self, style: Style) {
        self.cells.fill(Cell::blank_with(style));
        self.graphemes.clear();
        self.marks.clear();
        self.wrapped = false;
    }

    /// The line's text with trailing blanks trimmed.
    ///
    /// Cluster cells contribute their whole cluster text and spacers
    /// contribute nothing, so `中文` reads back as two characters, not
    /// four. Intended for tests, search and debugging rather than
    /// rendering.
    pub fn text(&self) -> String {
        let end = self
            .cells
            .iter()
            .rposition(|c| c.ch != ' ' || !c.flags.difference(CellFlags::PAD).is_empty())
            .map_or(0, |i| i + 1);
        let mut out = String::with_capacity(end);
        for (col, cell) in self.cells[..end].iter().enumerate() {
            if cell.is_spacer() {
                continue;
            }
            match self.cluster(col) {
                Some(text) if cell.is_cluster() => out.push_str(text),
                _ => out.push(cell.ch),
            }
        }
        out
    }

    // ---------------------------------------------- crate-internal edits

    /// Blanks the cell at `col` (if in range) and drops its cluster,
    /// without looking at its neighbours.
    pub(crate) fn blank_at(&mut self, col: usize, style: Style) {
        if let Some(cell) = self.cells.get_mut(col) {
            *cell = Cell::blank_with(style);
            self.graphemes.remove(col);
        }
    }

    /// Blanks `range` (already clamped) and drops its clusters, without
    /// looking at the neighbours.
    fn blank_raw(&mut self, range: std::ops::Range<usize>, style: Style) {
        self.cells[range.clone()].fill(Cell::blank_with(style));
        self.graphemes.remove_range(range);
    }

    /// Prepares cell `col` to be overwritten: when it is one half of a wide
    /// character, the other half becomes a blank, so no half is left
    /// unpaired. The cell itself keeps its content (and cluster) until the
    /// caller writes it with [`Line::set`].
    pub(crate) fn unpair(&mut self, col: usize, style: Style) {
        let Some(cell) = self.cells.get(col) else {
            return;
        };
        if cell.is_wide() {
            self.blank_at(col + 1, style);
        } else if cell.is_spacer() && col > 0 {
            self.blank_at(col - 1, style);
        }
    }

    /// Stores `cell` at `col` (if in range) and drops the old cluster there.
    pub(crate) fn set(&mut self, col: usize, cell: Cell) {
        if let Some(slot) = self.cells.get_mut(col) {
            *slot = cell;
            self.graphemes.remove(col);
        }
    }

    /// Inserts `n` blanks at `col`, shifting the rest of the line right;
    /// cells pushed past the end are lost (ICH). A wide character split by
    /// the insertion point, or whose spacer is pushed off the end, is
    /// blanked whole. `col + n` must not exceed the line length.
    pub(crate) fn insert_blanks(&mut self, col: usize, n: usize, style: Style) {
        let cols = self.cells.len();
        if n == 0 || col >= cols {
            return;
        }
        if self.cells[col].is_spacer() && col > 0 {
            self.blank_at(col - 1, style);
        }
        self.cells[col..].rotate_right(n);
        self.graphemes.shift_right(col, n, cols);
        // The cells that wrapped around to `col..col + n` are blanked
        // below; their clusters were already dropped by the shift.
        if self.cells.get(col + n).is_some_and(Cell::is_spacer) {
            self.blank_at(col + n, style);
        }
        if self.cells[cols - 1].is_wide() {
            self.blank_at(cols - 1, style);
        }
        self.cells[col..col + n].fill(Cell::blank_with(style));
        // Padding (only ever the last column) was pushed off the end or,
        // with `col` on it, blanked: nothing to unpad.
    }

    /// Deletes `n` cells at `col`, shifting the rest of the line left and
    /// filling the end with blanks (DCH). A wide character split by either
    /// end of the deleted range is blanked whole. `col + n` must not exceed
    /// the line length.
    pub(crate) fn delete_cells(&mut self, col: usize, n: usize, style: Style) {
        let cols = self.cells.len();
        if n == 0 || col >= cols {
            return;
        }
        if self.cells[col].is_spacer() && col > 0 {
            self.blank_at(col - 1, style);
        }
        self.cells[col..].rotate_left(n);
        self.graphemes.delete(col, n);
        if self.cells[col].is_spacer() {
            self.blank_at(col, style);
        }
        // The deleted cells rotated to the end; their clusters are gone.
        self.cells[cols - n..].fill(Cell::blank_with(style));
        self.unpad(col..cols - n);
    }

    /// Clears [`CellFlags::PAD`] in `range`: a padding cell that an
    /// insertion or deletion moved away from the last column is an
    /// ordinary blank.
    fn unpad(&mut self, range: std::ops::Range<usize>) {
        for cell in &mut self.cells[range] {
            cell.flags = cell.flags.difference(CellFlags::PAD);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Color;

    #[test]
    fn blank_line_has_cols_cells() {
        let line = Line::blank(5, Style::DEFAULT);
        assert_eq!(line.cells.len(), 5);
        assert_eq!(line.text(), "");
        assert!(!line.wrapped);
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn fill_clamps_range() {
        let pen = Style {
            bg: Color::Indexed(1),
            ..Style::DEFAULT
        };
        let mut line = Line::blank(3, Style::DEFAULT);
        line.cells[0] = Cell::new('a', Style::DEFAULT);
        line.cells[2] = Cell::new('c', Style::DEFAULT);
        line.fill(1..100, pen);
        assert_eq!(line.text(), "a");
        assert_eq!(line.cells[2].style.bg, Color::Indexed(1));
        line.fill(50..10, pen);
        assert_eq!(line.text(), "a");
    }

    #[test]
    fn clear_drops_marks_and_wrap() {
        let mut line = Line::blank(2, Style::DEFAULT);
        line.cells[0] = Cell::new('x', Style::DEFAULT);
        line.marks.push(Mark::PromptStart);
        line.wrapped = true;
        line.clear(Style::DEFAULT);
        assert_eq!(line, Line::blank(2, Style::DEFAULT));
    }

    #[test]
    fn text_trims_trailing_blanks_only() {
        let mut line = Line::blank(6, Style::DEFAULT);
        line.cells[1] = Cell::new('a', Style::DEFAULT);
        line.cells[3] = Cell::new('b', Style::DEFAULT);
        assert_eq!(line.text(), " a b");
    }
}
