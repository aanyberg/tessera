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

impl Mark {
    /// Parses an OSC 133 payload (the bytes after `133;`): `A`, `B`, `C` or
    /// `D`, alone or followed by `;` and options (`FinalTerm` semantic
    /// prompts).
    ///
    /// - `A` is [`Mark::PromptStart`], except for a secondary, continuation
    ///   or right-hand prompt (an option `k=s`, `k=c` or `k=r`), which gets
    ///   no mark: prompt navigation must not stop at a PS2 line.
    /// - `B` is [`Mark::CommandStart`], `C` [`Mark::OutputStart`]; their
    ///   options are ignored.
    /// - `D` is [`Mark::CommandEnd`]; its first option is the exit code (an
    ///   `i32` in decimal, `None` when it is missing, empty or not a
    ///   number), later options are ignored.
    ///
    /// Strict: the letter must be followed by the end of the payload or
    /// `;` (`Afoo`, `AB` and `D0` are not marks), and any other letter (the
    /// `L`, `N`, `P` and `I` of the semantic prompts proposal, lower case)
    /// gives `None`.
    #[must_use]
    pub fn from_osc133(payload: &[u8]) -> Option<Self> {
        let (&letter, rest) = payload.split_first()?;
        let options = match rest {
            [] => &[][..],
            [b';', options @ ..] => options,
            _ => return None,
        };
        let mut options = options.split(|&b| b == b';');
        match letter {
            b'A' => {
                let secondary = options.any(|option| matches!(option, b"k=s" | b"k=c" | b"k=r"));
                (!secondary).then_some(Self::PromptStart)
            }
            b'B' => Some(Self::CommandStart),
            b'C' => Some(Self::OutputStart),
            b'D' => {
                let exit_code = options
                    .next()
                    .and_then(|code| std::str::from_utf8(code).ok())
                    .and_then(|code| code.parse().ok());
                Some(Self::CommandEnd { exit_code })
            }
            _ => None,
        }
    }
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

    /// Adds `mark` to the line, last. A mark of the same kind already on the
    /// line is removed first (whatever its exit code, for
    /// [`Mark::CommandEnd`]), so a line holds at most one of each kind and a
    /// prompt redrawn on the same line does not pile marks up.
    pub fn set_mark(&mut self, mark: Mark) {
        let kind = std::mem::discriminant(&mark);
        self.marks.retain(|m| std::mem::discriminant(m) != kind);
        self.marks.push(mark);
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

    #[expect(
        clippy::unnecessary_wraps,
        reason = "compared with what Mark::from_osc133 returns"
    )]
    fn end(exit_code: Option<i32>) -> Option<Mark> {
        Some(Mark::CommandEnd { exit_code })
    }

    /// The `layer: parse` rows P133-01..P133-18 of
    /// `crates/tessera-term/tests/fixtures/osc_spec.txt`, by row id.
    #[test]
    fn from_osc133_follows_the_fixture_parse_rows() {
        let rows: &[(&str, &[u8], Option<Mark>)] = &[
            ("P133-01", b"A", Some(Mark::PromptStart)),
            ("P133-02", b"B", Some(Mark::CommandStart)),
            ("P133-03", b"C", Some(Mark::OutputStart)),
            ("P133-04", b"D", end(None)),
            ("P133-05", b"D;130", end(Some(130))),
            ("P133-06", b"D;-1", end(Some(-1))),
            ("P133-07", b"D;+3", end(Some(3))),
            ("P133-08", b"D;0;aid=1", end(Some(0))),
            ("P133-09", b"A;aid=1;cl=line", Some(Mark::PromptStart)),
            ("P133-10", b"A;k=s", None),
            ("P133-11", b"A;k=c", None),
            ("P133-12", b"A;k=r", None),
            ("P133-13", b"Afoo", None),
            ("P133-14", b"AB", None),
            ("P133-15", b"D0", None),
            ("P133-16", b"a", None),
            ("P133-17", b"", None),
            ("P133-18", b"L", None),
            ("P133-18", b"N", None),
            ("P133-18", b"P", None),
            ("P133-18", b"I", None),
            ("P133-18", b"Z", None),
        ];
        for (id, payload, expected) in rows {
            assert_eq!(
                &Mark::from_osc133(payload),
                expected,
                "{id}: {:?}",
                String::from_utf8_lossy(payload)
            );
        }
    }

    #[test]
    fn from_osc133_reads_kinds_among_other_options_and_odd_codes() {
        assert_eq!(Mark::from_osc133(b"A;aid=1;k=s"), None);
        assert_eq!(Mark::from_osc133(b"A;k=i"), Some(Mark::PromptStart));
        assert_eq!(Mark::from_osc133(b"A;k=sx"), Some(Mark::PromptStart));
        assert_eq!(Mark::from_osc133(b"A;"), Some(Mark::PromptStart));
        assert_eq!(Mark::from_osc133(b"B;x"), Some(Mark::CommandStart));
        for code in [
            &b"D;"[..],
            b"D;abc",
            b"D; 1",
            b"D;2147483648",
            b"D;\xff",
            b"D;;aid=1",
        ] {
            assert_eq!(Mark::from_osc133(code), end(None), "{code:?}");
        }
        assert_eq!(Mark::from_osc133(b"D;2147483647"), end(Some(i32::MAX)));
    }

    #[test]
    fn set_mark_keeps_one_mark_of_each_kind_in_arrival_order() {
        let mut line = Line::blank(4, Style::DEFAULT);
        line.set_mark(Mark::CommandEnd { exit_code: Some(1) });
        line.set_mark(Mark::PromptStart);
        line.set_mark(Mark::CommandStart);
        line.set_mark(Mark::PromptStart);
        line.set_mark(Mark::CommandEnd { exit_code: Some(0) });
        assert_eq!(
            line.marks,
            [
                Mark::CommandStart,
                Mark::PromptStart,
                Mark::CommandEnd { exit_code: Some(0) }
            ]
        );
    }

    #[test]
    fn text_trims_trailing_blanks_only() {
        let mut line = Line::blank(6, Style::DEFAULT);
        line.cells[1] = Cell::new('a', Style::DEFAULT);
        line.cells[3] = Cell::new('b', Style::DEFAULT);
        assert_eq!(line.text(), " a b");
    }
}
