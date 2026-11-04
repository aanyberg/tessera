//! Lines and the semantic marks shell integration attaches to them.

use crate::{Cell, CellFlags, GraphemeTable, Hyperlink, Style};

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
/// [`Line::graphemes`]; a cell that belongs to an OSC 8 hyperlink carries
/// [`CellFlags::LINK`] and its link lives in the same table
/// ([`Line::hyperlink`]). Code that
/// edits `cells` directly must keep all three rules; the line's own
/// methods and every [`Grid`] primitive do.
///
/// [`CellFlags::WIDE`]: crate::CellFlags::WIDE
/// [`CellFlags::SPACER`]: crate::CellFlags::SPACER
/// [`CellFlags::CLUSTER`]: crate::CellFlags::CLUSTER
/// [`CellFlags::LINK`]: crate::CellFlags::LINK
/// [`Grid`]: crate::Grid
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    /// Cells, exactly `cols` long.
    pub cells: Vec<Cell>,
    /// Semantic marks on this line.
    pub marks: Vec<Mark>,
    /// Cluster text of the line's [`CellFlags::CLUSTER`] cells and the
    /// hyperlink of its [`CellFlags::LINK`] cells, by column.
    ///
    /// [`CellFlags::CLUSTER`]: crate::CellFlags::CLUSTER
    /// [`CellFlags::LINK`]: crate::CellFlags::LINK
    pub graphemes: GraphemeTable,
    /// True when this line continues onto the next (soft wrap).
    pub wrapped: bool,
    /// Where each mark sits in the line's text, by kind (D-MARKPOS,
    /// ADR 0022): private to the grid, kept so a re-wrap undoes itself.
    pub(crate) anchors: MarkAnchors,
}

/// For each kind of [`Mark`] a line carries, the column in the row where
/// it sits: the column of its place in the line's text, set when a re-wrap
/// places it (D-MARKPOS, ADR 0022). Zero for a mark set on the row
/// ([`Line::set_mark`]), so a re-wrap places a fresh mark on the row
/// holding the row's first cell, and a mark placed by a re-wrap keeps its
/// place in the text: a narrower width and back puts it on its old row
/// again. Past the end of the row (a mark from blank rows trimmed off the
/// end of the line) the column counts on along the line.
///
/// Four 14-bit columns packed into the seven bytes of padding after
/// [`Line::wrapped`], so `Line` stays 64 bytes and a line without marks
/// allocates nothing for it. A column saturates at [`MarkAnchors::MAX`]
/// (16 383): a re-wrap is exact for rows up to that many columns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MarkAnchors([u8; 7]);

// `MarkAnchors` lives in the padding after `Line::wrapped`: the ring slot of
// every scrollback line stays 64 bytes (`scrollback.rs`'s memory table).
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Line>() == 64);

impl MarkAnchors {
    /// Bits per column.
    const BITS: u32 = 14;
    /// The largest column kept; larger ones saturate here.
    pub(crate) const MAX: u16 = (1 << Self::BITS) - 1;

    /// The bit offset of `mark`'s kind.
    const fn shift(mark: &Mark) -> u32 {
        let slot = match mark {
            Mark::PromptStart => 0,
            Mark::CommandStart => 1,
            Mark::OutputStart => 2,
            Mark::CommandEnd { .. } => 3,
        };
        slot * Self::BITS
    }

    /// The seven bytes as one word.
    const fn word(self) -> u64 {
        let b = self.0;
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], 0])
    }

    /// The column of the line's mark of `mark`'s kind.
    pub(crate) const fn get(self, mark: &Mark) -> u16 {
        // Masked to 14 bits: fits.
        #[allow(clippy::cast_possible_truncation)]
        let col = ((self.word() >> Self::shift(mark)) & Self::MAX as u64) as u16;
        col
    }

    /// Sets the column of the line's mark of `mark`'s kind, saturating at
    /// [`MarkAnchors::MAX`].
    pub(crate) const fn set(&mut self, mark: &Mark, col: u16) {
        let col = if col > Self::MAX { Self::MAX } else { col };
        let shift = Self::shift(mark);
        let word = (self.word() & !((Self::MAX as u64) << shift)) | ((col as u64) << shift);
        let b = word.to_le_bytes();
        self.0 = [b[0], b[1], b[2], b[3], b[4], b[5], b[6]];
    }
}

impl Line {
    /// A line of `cols` blank cells carrying the background of `style`.
    pub fn blank(cols: u16, style: Style) -> Self {
        Self {
            cells: vec![Cell::blank_with(style); usize::from(cols)],
            marks: Vec::new(),
            graphemes: GraphemeTable::new(),
            wrapped: false,
            anchors: MarkAnchors::default(),
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

    /// The hyperlink the cell at `col` belongs to, if any.
    pub fn hyperlink(&self, col: usize) -> Option<&Hyperlink> {
        self.cells
            .get(col)
            .filter(|cell| cell.is_link())
            .and_then(|_| self.graphemes.link(col))
    }

    /// Overwrites every cell in `range` (clamped to the line) with a blank
    /// carrying the background of `style`.
    ///
    /// A wide character cut by either end of the range is blanked whole:
    /// a range that starts on a spacer also blanks the wide cell to its
    /// left, and one that ends on a wide cell also blanks its spacer.
    /// Clusters and links in the blanked cells are dropped.
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

    /// [`Line::fill`] for the cells of `range` that are not protected
    /// ([`Cell::is_protected`]); returns whether `range` held a protected
    /// cell, which is kept as it is. Each run of unprotected cells is
    /// filled on its own, so a wide character cut by a run's end is still
    /// blanked whole; a protected wide character is never cut, because
    /// both its halves carry the bit (xterm `ClearInLine2`). Without a
    /// protected cell in `range` this is exactly [`Line::fill`].
    ///
    /// One pass over the cells and at most one over the line's cluster and
    /// link table, however many runs there are: the runs are blanked
    /// without touching the table, and the entries of the cells blanked
    /// are dropped together afterwards (S6: a screen of alternating
    /// protected clusters costs a constant times ED 2, not one table edit
    /// per run).
    pub(crate) fn fill_unprotected(&mut self, range: std::ops::Range<usize>, style: Style) -> bool {
        let end = range.end.min(self.cells.len());
        let start = range.start.min(end);
        let Some(first) = self.cells[start..end]
            .iter()
            .position(Cell::is_protected)
            .map(|at| start + at)
        else {
            self.fill(start..end, style);
            return false;
        };
        let blank = Cell::blank_with(style);
        // The columns blanked lie in `lo..hi`; `side` is whether one of them
        // had a cluster or a link.
        let (mut lo, mut hi, mut side) = (start, end, false);
        let mut blank_run = |cells: &mut [Cell], from: usize, to: usize| {
            if from == to {
                return;
            }
            // A wide character cut by either end of the run goes whole, as
            // in `fill`; it is unprotected, as both halves share a style.
            let from = if cells[from].is_spacer() && from > 0 {
                from - 1
            } else {
                from
            };
            let to = if cells[to - 1].is_wide() && to < cells.len() {
                to + 1
            } else {
                to
            };
            lo = lo.min(from);
            hi = hi.max(to);
            for cell in &mut cells[from..to] {
                side |= cell.is_cluster() || cell.is_link();
                *cell = blank;
            }
        };
        let mut run = start;
        for col in first..end {
            if self.cells[col].is_protected() {
                blank_run(&mut self.cells, run, col);
                run = col + 1;
            }
        }
        blank_run(&mut self.cells, run, end);
        if side {
            let cells = &self.cells;
            self.graphemes
                .retain_cols(|col| col < lo || col >= hi || cells[col].is_protected());
        }
        true
    }

    /// [`Line::clear`] that keeps the protected cells
    /// ([`Cell::is_protected`]) and their clusters and links; returns
    /// whether there was one. The marks and the wrap flag go either way,
    /// as the line counts as erased in full.
    pub(crate) fn clear_unprotected(&mut self, style: Style) -> bool {
        if !self.cells.iter().any(Cell::is_protected) {
            self.clear(style);
            return false;
        }
        self.fill_unprotected(0..self.cells.len(), style);
        self.marks.clear();
        self.anchors = MarkAnchors::default();
        self.wrapped = false;
        true
    }

    /// Resets the line to blanks and drops its marks, clusters, links and
    /// wrap flag.
    pub fn clear(&mut self, style: Style) {
        self.cells.fill(Cell::blank_with(style));
        self.graphemes.clear();
        self.marks.clear();
        self.anchors = MarkAnchors::default();
        self.wrapped = false;
    }

    /// Adds `mark` to the line, last. A mark of the same kind already on the
    /// line is removed first (whatever its exit code, for
    /// [`Mark::CommandEnd`]), so a line holds at most one of each kind and a
    /// prompt redrawn on the same line does not pile marks up.
    pub fn set_mark(&mut self, mark: Mark) {
        self.set_mark_at(mark, 0);
    }

    /// [`Line::set_mark`] with the mark's column in the line's text
    /// ([`MarkAnchors`]), for a re-wrap.
    pub(crate) fn set_mark_at(&mut self, mark: Mark, anchor: u16) {
        let kind = std::mem::discriminant(&mark);
        self.marks.retain(|m| std::mem::discriminant(m) != kind);
        self.anchors.set(&mark, anchor);
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
            .rposition(|c| {
                c.ch != ' '
                    || !c
                        .flags
                        .difference(CellFlags::PAD | CellFlags::LINK)
                        .is_empty()
            })
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

    /// Blanks the cell at `col` (if in range) and drops its cluster and
    /// link, without looking at its neighbours.
    pub(crate) fn blank_at(&mut self, col: usize, style: Style) {
        if let Some(cell) = self.cells.get_mut(col) {
            *cell = Cell::blank_with(style);
            self.graphemes.remove(col);
        }
    }

    /// Blanks `range` (already clamped) and drops its clusters and links,
    /// without looking at the neighbours.
    fn blank_raw(&mut self, range: std::ops::Range<usize>, style: Style) {
        self.cells[range.clone()].fill(Cell::blank_with(style));
        self.graphemes.remove_range(range);
    }

    /// Makes the cell at `col` (if in range) part of `link`.
    pub(crate) fn set_link(&mut self, col: usize, link: Hyperlink) {
        if let Some(cell) = self.cells.get_mut(col) {
            cell.flags |= CellFlags::LINK;
            self.graphemes.set_link(col, link);
        }
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

    /// Stores `cell` at `col` (if in range) and drops the old cluster and
    /// link there.
    pub(crate) fn set(&mut self, col: usize, cell: Cell) {
        if let Some(slot) = self.cells.get_mut(col) {
            *slot = cell;
            self.graphemes.remove(col);
        }
    }

    /// Blanks a wide character cut by either edge of `span`: one whose
    /// left half is just before `span.start` and spacer at it, or whose
    /// left half is the last cell of `span` and spacer just after it. Both
    /// halves become blanks of `style`'s background, so moving the cells of
    /// `span` never splits a pair (xterm `scrollInMargins`).
    pub(crate) fn split_wide_at_edges(&mut self, span: std::ops::Range<usize>, style: Style) {
        let cols = self.cells.len();
        if span.start > 0 && self.cells.get(span.start).is_some_and(Cell::is_spacer) {
            self.blank_at(span.start - 1, style);
            self.blank_at(span.start, style);
        }
        if span.end > 0 && span.end < cols && self.cells[span.end].is_spacer() {
            self.blank_at(span.end - 1, style);
            self.blank_at(span.end, style);
        }
    }

    /// Inserts `n` blanks at `col`, shifting the cells up to `end`
    /// (exclusive: the right margin plus one, or the line length) right;
    /// cells pushed past `end` are lost and cells from `end` on do not
    /// move (ICH, DECIC). A wide character split by the insertion point,
    /// cut by `end` or whose spacer is pushed past `end` is blanked whole.
    /// `col + n` must not exceed `end`, nor `end` the line length.
    pub(crate) fn insert_blanks(&mut self, col: usize, n: usize, end: usize, style: Style) {
        let end = end.min(self.cells.len());
        if n == 0 || col >= end {
            return;
        }
        // A wide character split by `col` or cut by `end` goes whole.
        self.split_wide_at_edges(col..end, style);
        self.cells[col..end].rotate_right(n);
        self.graphemes.shift_right(col, n, end);
        // The cells that wrapped around to `col..col + n` are blanked
        // below; their clusters were already dropped by the shift.
        if col + n < end && self.cells[col + n].is_spacer() {
            self.blank_at(col + n, style);
        }
        if self.cells[end - 1].is_wide() {
            self.blank_at(end - 1, style);
        }
        self.cells[col..col + n].fill(Cell::blank_with(style));
        // Padding (only ever the last column) was pushed off the end or,
        // with `col` on it, blanked; with `end` short of the last column
        // it does not move: nothing to unpad.
    }

    /// Deletes `n` cells at `col`, shifting the cells up to `end`
    /// (exclusive: the right margin plus one, or the line length) left and
    /// filling the `n` cells before `end` with blanks; cells from `end` on
    /// do not move (DCH, DECDC). A wide character split by either end of
    /// the deleted range or cut by `end` is blanked whole. `col + n` must
    /// not exceed `end`, nor `end` the line length.
    pub(crate) fn delete_cells(&mut self, col: usize, n: usize, end: usize, style: Style) {
        let end = end.min(self.cells.len());
        if n == 0 || col >= end {
            return;
        }
        self.split_wide_at_edges(col..end, style);
        self.cells[col..end].rotate_left(n);
        self.graphemes.delete(col, n, end);
        if self.cells[col].is_spacer() {
            self.blank_at(col, style);
        }
        // The deleted cells rotated to the end; their clusters are gone.
        self.cells[end - n..end].fill(Cell::blank_with(style));
        self.unpad(col..end - n);
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

    /// The four 14-bit columns are independent, saturate at the maximum
    /// and fit in `Line`'s padding (D-MARKPOS).
    #[test]
    fn mark_anchors_pack_four_columns_in_the_padding() {
        let kinds = [
            Mark::PromptStart,
            Mark::CommandStart,
            Mark::OutputStart,
            Mark::CommandEnd { exit_code: None },
        ];
        let mut anchors = MarkAnchors::default();
        for (i, mark) in kinds.iter().enumerate() {
            anchors.set(mark, 1000 * (u16::try_from(i).unwrap() + 1) + 7);
        }
        for (i, mark) in kinds.iter().enumerate() {
            assert_eq!(
                anchors.get(mark),
                1000 * (u16::try_from(i).unwrap() + 1) + 7
            );
        }
        anchors.set(&Mark::OutputStart, u16::MAX);
        assert_eq!(anchors.get(&Mark::OutputStart), MarkAnchors::MAX);
        assert_eq!(anchors.get(&Mark::CommandStart), 2007);
        assert_eq!(anchors.get(&Mark::CommandEnd { exit_code: Some(1) }), 4007);
        anchors.set(&Mark::CommandEnd { exit_code: None }, 0);
        assert_eq!(anchors.get(&Mark::CommandEnd { exit_code: None }), 0);
        assert_eq!(anchors.get(&Mark::OutputStart), MarkAnchors::MAX);
        assert_eq!(std::mem::size_of::<Line>(), 64);
    }

    /// `set_mark` puts a mark at the row's first cell; `clear` forgets the
    /// columns.
    #[test]
    fn set_mark_and_clear_reset_the_column() {
        let mut line = Line::blank(4, Style::DEFAULT);
        line.set_mark_at(Mark::PromptStart, 9);
        assert_eq!(line.anchors.get(&Mark::PromptStart), 9);
        line.set_mark(Mark::PromptStart);
        assert_eq!(line.anchors.get(&Mark::PromptStart), 0);
        line.set_mark_at(Mark::OutputStart, 3);
        line.clear(Style::DEFAULT);
        assert_eq!(line.anchors, MarkAnchors::default());
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
