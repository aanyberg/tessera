//! The terminal's data model.
//!
//! Owns cells, lines, the visible grid, scrollback history, the cursor,
//! selections and per-line semantic marks (prompt boundaries, command exit
//! codes, working directory). It knows nothing about escape sequences or
//! pixels: `tessera-term` mutates it, `tessera-render` and `tessera-search`
//! read it, and plugins observe its marks.

#![forbid(unsafe_code)]

/// Zero-based cell position in the visible grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Point {
    /// Row from the top of the visible area.
    pub row: u16,
    /// Column from the left.
    pub col: u16,
}

/// A single character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cell {
    /// The character displayed, or `' '` when empty.
    pub ch: char,
    /// Style attributes.
    pub style: Style,
}

/// Visual attributes of a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// Foreground color.
    pub fg: Color,
    /// Background color.
    pub bg: Color,
    /// Bold, italic, underline and friends.
    pub flags: u16,
}

/// A color reference, resolved against the active theme at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// Theme default for the slot (fg or bg).
    #[default]
    Default,
    /// One of the 256 indexed colors.
    Indexed(u8),
    /// A 24-bit color.
    Rgb(u8, u8, u8),
}

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
#[derive(Debug, Clone, Default)]
pub struct Line {
    /// Cells, exactly `cols` long.
    pub cells: Vec<Cell>,
    /// Semantic marks on this line.
    pub marks: Vec<Mark>,
    /// True when this line continues onto the next (soft wrap).
    pub wrapped: bool,
}

/// The visible screen plus scrollback.
#[derive(Debug)]
pub struct Grid {
    rows: u16,
    cols: u16,
    lines: Vec<Line>,
    cursor: Point,
}

impl Grid {
    /// Creates an empty grid of the given size.
    pub fn new(rows: u16, cols: u16) -> Self {
        let blank = Line {
            cells: vec![Cell::default(); usize::from(cols)],
            ..Line::default()
        };
        Self {
            rows,
            cols,
            lines: vec![blank; usize::from(rows)],
            cursor: Point::default(),
        }
    }

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

    /// Visible lines, top to bottom.
    pub fn visible_lines(&self) -> &[Line] {
        &self.lines
    }
}
