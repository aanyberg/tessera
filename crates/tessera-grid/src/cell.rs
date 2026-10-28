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
/// A small bit set; combine with [`CellFlags::union`] or `|`. Four markers
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
/// - [`CellFlags::PAD`]: the last column of a soft-wrapped row that a wide
///   character did not fit into, so it continued on the next row. The
///   cell shows a blank and is not text: a resize that joins the row with
///   the next one drops it. Writing or erasing the cell clears the marker.
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
    /// Padding left in the last column when a wide character wrapped.
    pub const PAD: Self = Self(1 << 3);

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
/// 20 bytes (a test enforces it): a 4-byte `char`, the 14-byte [`Style`]
/// and the 1-byte `flags`, padded to the `char`'s 4-byte alignment. It was
/// 16 bytes until [`Style`] gained an underline colour (ADR 0013).
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
    /// rest of the attributes, the underline colour included, do not.
    pub const fn blank_with(style: Style) -> Self {
        Self {
            ch: ' ',
            style: Style {
                bg: style.bg,
                ..Style::DEFAULT
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

    /// True when the cell shows a space with no attributes but background:
    /// default foreground and underline colour, no flags.
    ///
    /// A [`CellFlags::SPACER`] is not blank: it is part of the wide
    /// character to its left. A [`CellFlags::PAD`] cell is.
    pub fn is_blank(&self) -> bool {
        self.ch == ' '
            && self.style.fg == Color::Default
            && self.style.underline == Color::Default
            && self.style.flags == 0
            && self.flags.difference(CellFlags::PAD).is_empty()
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

    /// True for the padding a wrapped wide character left in the last
    /// column of a row ([`CellFlags::PAD`]).
    pub const fn is_pad(&self) -> bool {
        self.flags.contains(CellFlags::PAD)
    }

    /// The padding a wide character leaves in the last column when it
    /// wraps: a blank with the background of `style`, marked
    /// [`CellFlags::PAD`].
    pub(crate) const fn pad_with(style: Style) -> Self {
        Self {
            flags: CellFlags::PAD,
            ..Self::blank_with(style)
        }
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

/// Visual attributes of a cell: what SGR sets on the pen.
///
/// 14 bytes (three 4-byte [`Color`]s and the `u16` flags) and never more
/// than 16, so a full-size grid fits comfortably in cache; a test enforces
/// this.
///
/// # Underline
///
/// [`Style::UNDERLINE`] means "some underline". At most one of
/// [`Style::DOUBLE_UNDERLINE`], [`Style::CURLY_UNDERLINE`],
/// [`Style::DOTTED_UNDERLINE`] and [`Style::DASHED_UNDERLINE`] accompanies
/// it and picks the shape; none of them means a single line. Read the shape
/// with [`Style::underline_style`] and set it with
/// [`Style::with_underline_style`], which keep that invariant; setting the
/// bits by hand with [`Style::with`] does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    /// Foreground color.
    pub fg: Color,
    /// Background color.
    pub bg: Color,
    /// Underline color (SGR 58 / 59). [`Color::Default`] draws the
    /// underline in the foreground colour.
    pub underline: Color,
    /// Bitwise OR of the `Style::*` flag constants.
    pub flags: u16,
}

impl Style {
    /// No colours, no attributes.
    pub const DEFAULT: Self = Self {
        fg: Color::Default,
        bg: Color::Default,
        underline: Color::Default,
        flags: 0,
    };

    /// Bold (SGR 1).
    pub const BOLD: u16 = 1 << 0;
    /// Dim / faint (SGR 2).
    pub const DIM: u16 = 1 << 1;
    /// Italic (SGR 3).
    pub const ITALIC: u16 = 1 << 2;
    /// Some underline (SGR 4, `4:1`..`4:5`, 21); the shape bits below pick
    /// which. See [`Style::underline_style`].
    pub const UNDERLINE: u16 = 1 << 3;
    /// Blink (SGR 5 and 6).
    pub const BLINK: u16 = 1 << 4;
    /// Reverse video (SGR 7).
    pub const INVERSE: u16 = 1 << 5;
    /// Hidden / invisible (SGR 8).
    pub const HIDDEN: u16 = 1 << 6;
    /// Strikethrough (SGR 9).
    pub const STRIKETHROUGH: u16 = 1 << 7;
    /// Double underline (SGR 21, `4:2`); only together with
    /// [`Style::UNDERLINE`].
    pub const DOUBLE_UNDERLINE: u16 = 1 << 8;
    /// Curly underline (SGR `4:3`); only together with [`Style::UNDERLINE`].
    pub const CURLY_UNDERLINE: u16 = 1 << 9;
    /// Dotted underline (SGR `4:4`); only together with
    /// [`Style::UNDERLINE`].
    pub const DOTTED_UNDERLINE: u16 = 1 << 10;
    /// Dashed underline (SGR `4:5`); only together with
    /// [`Style::UNDERLINE`].
    pub const DASHED_UNDERLINE: u16 = 1 << 11;
    /// Every underline bit: [`Style::UNDERLINE`] and the four shape bits.
    pub const UNDERLINE_STYLE_MASK: u16 = Self::UNDERLINE
        | Self::DOUBLE_UNDERLINE
        | Self::CURLY_UNDERLINE
        | Self::DOTTED_UNDERLINE
        | Self::DASHED_UNDERLINE;

    /// The underline shape the flags describe.
    ///
    /// Without [`Style::UNDERLINE`] this is [`UnderlineStyle::None`]
    /// whatever the shape bits say. Should several shape bits be set (only
    /// possible by setting them by hand), the first of double, curly,
    /// dotted, dashed wins.
    pub const fn underline_style(self) -> UnderlineStyle {
        if !self.has(Self::UNDERLINE) {
            UnderlineStyle::None
        } else if self.has(Self::DOUBLE_UNDERLINE) {
            UnderlineStyle::Double
        } else if self.has(Self::CURLY_UNDERLINE) {
            UnderlineStyle::Curly
        } else if self.has(Self::DOTTED_UNDERLINE) {
            UnderlineStyle::Dotted
        } else if self.has(Self::DASHED_UNDERLINE) {
            UnderlineStyle::Dashed
        } else {
            UnderlineStyle::Single
        }
    }

    /// Returns a copy with the underline set to `style`: every underline
    /// bit is cleared, then [`Style::UNDERLINE`] and at most one shape bit
    /// are set. The underline colour is left alone.
    #[must_use]
    pub const fn with_underline_style(self, style: UnderlineStyle) -> Self {
        let shape = match style {
            UnderlineStyle::None => return self.without(Self::UNDERLINE_STYLE_MASK),
            UnderlineStyle::Single => 0,
            UnderlineStyle::Double => Self::DOUBLE_UNDERLINE,
            UnderlineStyle::Curly => Self::CURLY_UNDERLINE,
            UnderlineStyle::Dotted => Self::DOTTED_UNDERLINE,
            UnderlineStyle::Dashed => Self::DASHED_UNDERLINE,
        };
        self.without(Self::UNDERLINE_STYLE_MASK)
            .with(Self::UNDERLINE | shape)
    }

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

/// The shape of a cell's underline, as [`Style::underline_style`] reads it
/// from the flags.
///
/// SGR `4:0`..`4:5` select these in order (kitty's extension); plain SGR 4
/// is [`UnderlineStyle::Single`], SGR 21 [`UnderlineStyle::Double`] and
/// SGR 24 [`UnderlineStyle::None`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UnderlineStyle {
    /// No underline.
    #[default]
    None,
    /// One straight line.
    Single,
    /// Two straight lines.
    Double,
    /// A wavy line.
    Curly,
    /// A dotted line.
    Dotted,
    /// A dashed line.
    Dashed,
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
    fn cell_is_20_bytes() {
        assert_eq!(std::mem::size_of::<Cell>(), 20);
    }

    #[test]
    fn underline_style_round_trips() {
        let all = [
            UnderlineStyle::None,
            UnderlineStyle::Single,
            UnderlineStyle::Double,
            UnderlineStyle::Curly,
            UnderlineStyle::Dotted,
            UnderlineStyle::Dashed,
        ];
        for from in all {
            for to in all {
                let base = Style::DEFAULT.with(Style::BOLD).with_underline_style(from);
                let style = base.with_underline_style(to);
                assert_eq!(style.underline_style(), to, "{from:?} -> {to:?}");
                assert!(style.has(Style::BOLD), "other flags survive");
                let shapes =
                    (style.flags & Style::UNDERLINE_STYLE_MASK & !Style::UNDERLINE).count_ones();
                assert!(shapes <= 1, "{from:?} -> {to:?}: {shapes} shape bits");
                assert_eq!(
                    style.has(Style::UNDERLINE),
                    to != UnderlineStyle::None,
                    "{from:?} -> {to:?}"
                );
            }
        }
        // Shape bits without UNDERLINE are no underline at all.
        assert_eq!(
            Style::DEFAULT
                .with(Style::CURLY_UNDERLINE)
                .underline_style(),
            UnderlineStyle::None
        );
        // Shape bits set by hand, several at once: the first of double,
        // curly, dotted, dashed wins.
        let all_shapes = Style::DEFAULT.with(Style::UNDERLINE_STYLE_MASK);
        assert_eq!(all_shapes.underline_style(), UnderlineStyle::Double);
        let without = |bits: u16| all_shapes.without(bits).underline_style();
        assert_eq!(without(Style::DOUBLE_UNDERLINE), UnderlineStyle::Curly);
        assert_eq!(
            without(Style::DOUBLE_UNDERLINE | Style::CURLY_UNDERLINE),
            UnderlineStyle::Dotted
        );
        assert_eq!(
            without(Style::DOUBLE_UNDERLINE | Style::CURLY_UNDERLINE | Style::DOTTED_UNDERLINE),
            UnderlineStyle::Dashed
        );
        // The renderer's bits keep their meaning.
        assert!(
            Style::DEFAULT
                .with_underline_style(UnderlineStyle::Double)
                .has(Style::UNDERLINE | Style::DOUBLE_UNDERLINE)
        );
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
            underline: Color::Indexed(9),
            flags: Style::BOLD,
        };
        let cell = Cell::blank_with(pen);
        assert_eq!(cell.ch, ' ');
        assert_eq!(cell.style.fg, Color::Default);
        assert_eq!(cell.style.bg, Color::Rgb(1, 2, 3));
        assert_eq!(cell.style.underline, Color::Default);
        assert_eq!(cell.style.flags, 0);
        assert!(cell.is_blank());
        assert!(!Cell::new('x', Style::DEFAULT).is_blank());
        let coloured_underline = Style {
            underline: Color::Indexed(1),
            ..Style::DEFAULT
        };
        assert!(
            !Cell::new(' ', coloured_underline).is_blank(),
            "an underline colour is an attribute"
        );
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
