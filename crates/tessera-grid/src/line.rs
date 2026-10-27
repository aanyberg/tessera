//! Lines and the semantic marks shell integration attaches to them.

use crate::{Cell, Style};

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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    /// Cells, exactly `cols` long.
    pub cells: Vec<Cell>,
    /// Semantic marks on this line.
    pub marks: Vec<Mark>,
    /// True when this line continues onto the next (soft wrap).
    pub wrapped: bool,
}

impl Line {
    /// A line of `cols` blank cells carrying the background of `style`.
    pub fn blank(cols: u16, style: Style) -> Self {
        Self {
            cells: vec![Cell::blank_with(style); usize::from(cols)],
            marks: Vec::new(),
            wrapped: false,
        }
    }

    /// Overwrites every cell in `range` (clamped to the line) with a blank
    /// carrying the background of `style`.
    pub fn fill(&mut self, range: std::ops::Range<usize>, style: Style) {
        let end = range.end.min(self.cells.len());
        let start = range.start.min(end);
        self.cells[start..end].fill(Cell::blank_with(style));
    }

    /// Resets the line to blanks and drops its marks and wrap flag.
    pub fn clear(&mut self, style: Style) {
        let cols = self.cells.len();
        self.fill(0..cols, style);
        self.marks.clear();
        self.wrapped = false;
    }

    /// The line's text with trailing blanks trimmed.
    ///
    /// Intended for tests, search and debugging rather than rendering.
    pub fn text(&self) -> String {
        let end = self
            .cells
            .iter()
            .rposition(|c| c.ch != ' ')
            .map_or(0, |i| i + 1);
        self.cells[..end].iter().map(|c| c.ch).collect()
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
