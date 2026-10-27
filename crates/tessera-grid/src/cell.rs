//! Cells and their attributes.

/// Zero-based cell position in the visible grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Point {
    /// Row from the top of the visible area.
    pub row: u16,
    /// Column from the left.
    pub col: u16,
}

impl Point {
    /// Creates a point.
    pub const fn new(row: u16, col: u16) -> Self {
        Self { row, col }
    }
}

/// A single character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// The character displayed, or `' '` when empty.
    pub ch: char,
    /// Style attributes.
    pub style: Style,
}

impl Default for Cell {
    fn default() -> Self {
        Self::BLANK
    }
}

impl Cell {
    /// An empty cell with default styling.
    pub const BLANK: Self = Self {
        ch: ' ',
        style: Style::DEFAULT,
    };

    /// Creates a cell showing `ch` with `style`.
    pub const fn new(ch: char, style: Style) -> Self {
        Self { ch, style }
    }

    /// An empty cell carrying only the background of `style`.
    ///
    /// This is what erase and scroll operations fill with: the background
    /// colour of the current pen survives (background colour erase), the
    /// rest of the attributes do not.
    pub const fn blank_with(style: Style) -> Self {
        Self {
            ch: ' ',
            style: Style {
                fg: Color::Default,
                bg: style.bg,
                flags: 0,
            },
        }
    }

    /// True when the cell shows a space with no attributes but background.
    pub fn is_blank(&self) -> bool {
        self.ch == ' ' && self.style.fg == Color::Default && self.style.flags == 0
    }
}

/// Visual attributes of a cell.
///
/// Stays at or below 16 bytes so a full-size grid fits comfortably in cache;
/// a test enforces this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    /// Foreground color.
    pub fg: Color,
    /// Background color.
    pub bg: Color,
    /// Bitwise OR of the `Style::*` flag constants.
    pub flags: u16,
}

impl Style {
    /// No colours, no attributes.
    pub const DEFAULT: Self = Self {
        fg: Color::Default,
        bg: Color::Default,
        flags: 0,
    };

    /// Bold (SGR 1).
    pub const BOLD: u16 = 1 << 0;
    /// Dim / faint (SGR 2).
    pub const DIM: u16 = 1 << 1;
    /// Italic (SGR 3).
    pub const ITALIC: u16 = 1 << 2;
    /// Underline (SGR 4).
    pub const UNDERLINE: u16 = 1 << 3;
    /// Slow blink (SGR 5).
    pub const BLINK: u16 = 1 << 4;
    /// Reverse video (SGR 7).
    pub const INVERSE: u16 = 1 << 5;
    /// Hidden / invisible (SGR 8).
    pub const HIDDEN: u16 = 1 << 6;
    /// Strikethrough (SGR 9).
    pub const STRIKETHROUGH: u16 = 1 << 7;
    /// Double underline (SGR 21).
    pub const DOUBLE_UNDERLINE: u16 = 1 << 8;

    /// True when every bit of `flag` is set.
    pub const fn has(self, flag: u16) -> bool {
        self.flags & flag == flag
    }

    /// Returns a copy with `flag` set.
    #[must_use]
    pub const fn with(mut self, flag: u16) -> Self {
        self.flags |= flag;
        self
    }

    /// Returns a copy with `flag` cleared.
    #[must_use]
    pub const fn without(mut self, flag: u16) -> Self {
        self.flags &= !flag;
        self
    }
}

/// A color reference, resolved against the active theme at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Color {
    /// Theme default for the slot (fg or bg).
    #[default]
    Default,
    /// One of the 256 indexed colors.
    Indexed(u8),
    /// A 24-bit color.
    Rgb(u8, u8, u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_fits_in_16_bytes() {
        assert!(std::mem::size_of::<Style>() <= 16);
    }

    #[test]
    fn blank_with_keeps_only_background() {
        let pen = Style {
            fg: Color::Indexed(3),
            bg: Color::Rgb(1, 2, 3),
            flags: Style::BOLD,
        };
        let cell = Cell::blank_with(pen);
        assert_eq!(cell.ch, ' ');
        assert_eq!(cell.style.fg, Color::Default);
        assert_eq!(cell.style.bg, Color::Rgb(1, 2, 3));
        assert_eq!(cell.style.flags, 0);
        assert!(cell.is_blank());
        assert!(!Cell::new('x', Style::DEFAULT).is_blank());
    }

    #[test]
    fn flag_helpers() {
        let s = Style::DEFAULT.with(Style::BOLD).with(Style::ITALIC);
        assert!(s.has(Style::BOLD));
        assert!(s.has(Style::BOLD | Style::ITALIC));
        assert!(!s.has(Style::UNDERLINE));
        let s = s.without(Style::BOLD);
        assert!(!s.has(Style::BOLD));
        assert!(s.has(Style::ITALIC));
    }
}
