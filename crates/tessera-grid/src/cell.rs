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

/// Width and cluster markers of a [`Cell`].
///
/// A small bit set; combine with [`CellFlags::union`] or `|`. Three markers
/// exist:
///
/// - [`CellFlags::WIDE`]: the cell holds the left half of a character two
///   columns wide; the next cell in the line is its [`CellFlags::SPACER`].
/// - [`CellFlags::SPACER`]: the right half of a wide character. It shows a
///   `' '` with the wide cell's [`Style`] (so background passes need no
///   special case) and is never drawn on its own.
/// - [`CellFlags::CLUSTER`]: the cell's grapheme cluster has more scalars
///   than [`Cell::ch`]; the full text is in [`Line::graphemes`] at the
///   cell's column (see [`Line::cluster`]).
///
/// [`Line::graphemes`]: crate::Line::graphemes
/// [`Line::cluster`]: crate::Line::cluster
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CellFlags(u8);

impl CellFlags {
    /// No markers: a plain one-column cell.
    pub const EMPTY: Self = Self(0);
    /// Left half of a two-column character.
    pub const WIDE: Self = Self(1 << 0);
    /// Right half of a two-column character.
    pub const SPACER: Self = Self(1 << 1);
    /// The cell's text continues in the line's grapheme table.
    pub const CLUSTER: Self = Self(1 << 2);

    /// The raw bits.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// True when no marker is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// True when every marker of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The markers of both sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The markers of `self` that are not in `other`.
    #[must_use]
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl std::ops::BitOr for CellFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for CellFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

/// A single character cell.
///
/// Stays at 16 bytes (a test enforces it): `flags` fits in the padding
/// after [`Style`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// The character displayed, or `' '` when empty. For a
    /// [`CellFlags::CLUSTER`] cell this is the cluster's first scalar (its
    /// base); for a [`CellFlags::SPACER`] it is `' '`.
    pub ch: char,
    /// Style attributes.
    pub style: Style,
    /// Width and cluster markers.
    pub flags: CellFlags,
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
        flags: CellFlags::EMPTY,
    };

    /// Creates a plain one-column cell showing `ch` with `style`.
    pub const fn new(ch: char, style: Style) -> Self {
        Self {
            ch,
            style,
            flags: CellFlags::EMPTY,
        }
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
            flags: CellFlags::EMPTY,
        }
    }

    /// The right half of a wide character whose left half has `style`.
    pub(crate) const fn spacer(style: Style) -> Self {
        Self {
            ch: ' ',
            style,
            flags: CellFlags::SPACER,
        }
    }

    /// True when the cell shows a space with no attributes but background.
    ///
    /// A [`CellFlags::SPACER`] is not blank: it is part of the wide
    /// character to its left.
    pub fn is_blank(&self) -> bool {
        self.ch == ' '
            && self.style.fg == Color::Default
            && self.style.flags == 0
            && self.flags.is_empty()
    }

    /// True for the left half of a two-column character
    /// ([`CellFlags::WIDE`]).
    pub const fn is_wide(&self) -> bool {
        self.flags.contains(CellFlags::WIDE)
    }

    /// True for the right half of a two-column character
    /// ([`CellFlags::SPACER`]).
    pub const fn is_spacer(&self) -> bool {
        self.flags.contains(CellFlags::SPACER)
    }

    /// True when the cell's text continues in the line's grapheme table
    /// ([`CellFlags::CLUSTER`]).
    pub const fn is_cluster(&self) -> bool {
        self.flags.contains(CellFlags::CLUSTER)
    }

    /// Columns the cell's character occupies starting at this cell: 2 for a
    /// wide cell, 0 for a spacer (its column belongs to the cell on its
    /// left), 1 otherwise.
    pub const fn width(&self) -> u16 {
        if self.is_wide() {
            2
        } else if self.is_spacer() {
            0
        } else {
            1
        }
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
    fn cell_fits_in_16_bytes() {
        assert!(std::mem::size_of::<Cell>() <= 16);
    }

    #[test]
    fn cell_flags_and_width() {
        let plain = Cell::new('a', Style::DEFAULT);
        assert_eq!(plain.flags, CellFlags::EMPTY);
        assert_eq!(plain.width(), 1);
        let wide = Cell {
            flags: CellFlags::WIDE | CellFlags::CLUSTER,
            ..Cell::new('\u{4E2D}', Style::DEFAULT)
        };
        assert!(wide.is_wide() && wide.is_cluster() && !wide.is_spacer());
        assert_eq!(wide.width(), 2);
        let spacer = Cell::spacer(Style::DEFAULT.with(Style::BOLD));
        assert!(spacer.is_spacer() && !spacer.is_wide());
        assert_eq!(spacer.width(), 0);
        assert!(!spacer.is_blank(), "a spacer belongs to its wide cell");
        assert!(spacer.style.has(Style::BOLD));
        let mut flags = CellFlags::WIDE;
        flags |= CellFlags::CLUSTER;
        assert!(flags.contains(CellFlags::WIDE | CellFlags::CLUSTER));
        assert_eq!(flags.difference(CellFlags::WIDE), CellFlags::CLUSTER);
        assert_eq!(flags.bits(), 0b101);
        assert!(CellFlags::default().is_empty());
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
