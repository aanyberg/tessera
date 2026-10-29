//! What one frame draws: rows of cells plus cursor, selection and overlays.
//!
//! [`FrameInput`] is deliberately decoupled from [`tessera_grid::Grid`]:
//! the caller (app-core) decides which rows are visible, including
//! scrollback, and hands the renderer slices of cells. The renderer never
//! learns about viewports, scroll offsets or the scrollback buffer
//! (ADR 0010).
//!
//! [`FrameInput::damage`] tells the renderer which rows changed since the
//! last frame it drew, so it re-shapes and re-uploads only those
//! (ADR 0016); [`FrameInput::clusters`] carries each row's grapheme
//! cluster text, so combining marks and emoji sequences are shaped whole.

use std::ops::Range;

use tessera_grid::{Cell, DamageRows, GraphemeTable, Point};

/// How the cursor is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum CursorShape {
    /// A filled cell: the cursor color over the cell, with the glyph
    /// redrawn in the cell's background color.
    #[default]
    Block,
    /// A vertical bar at the cell's left edge.
    Beam,
    /// A horizontal bar at the cell's bottom edge.
    Underline,
    /// The outline of the cell, what every shape turns into while the
    /// surface is unfocused.
    HollowBlock,
}

/// Where the cursor is and how it looks this frame.
///
/// Built with [`CursorState::new`] (and [`CursorState::with_visible`]).
/// A cursor on either half of a wide character covers both columns; the
/// renderer reads that from the cells, so no field says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct CursorState {
    /// Row within [`FrameInput::rows`], from the top.
    pub row: u16,
    /// Column within the row, from the left.
    pub col: u16,
    /// The shape to draw.
    pub shape: CursorShape,
    /// Whether the cursor is drawn this frame. Blinking is the host's job:
    /// it toggles this on its own clock; the renderer has none.
    pub visible: bool,
}

impl CursorState {
    /// A visible cursor of `shape` at `row`, `col`.
    #[must_use]
    pub const fn new(row: u16, col: u16, shape: CursorShape) -> Self {
        Self {
            row,
            col,
            shape,
            visible: true,
        }
    }

    /// The same cursor, drawn this frame or not.
    #[must_use]
    pub const fn with_visible(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }
}

/// How a [`Selection`] covers the cells between its two points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SelectionKind {
    /// Text order: from the first point's cell to the end of its row,
    /// every row in between whole, and the last row up to the other
    /// point's cell (inclusive). On one row, the cells between the two
    /// columns. Wrapped rows are consecutive rows, so a selection over a
    /// soft-wrapped line needs nothing special.
    #[default]
    Linear,
    /// A rectangle: the columns `min(col)..=max(col)` on every row
    /// `min(row)..=max(row)`.
    Block,
}

/// A selected range of cells, in [`FrameInput::rows`] coordinates.
///
/// Drawn by the overlay pass as a quad of [`crate::Theme::selection`]
/// alpha-blended over each covered cell's background; the text keeps its
/// own colors (no inverse video) and shaping is untouched, so selecting
/// part of a ligature never splits it. `start` and `end` may come in
/// either order. A point on the [`tessera_grid::CellFlags::SPACER`] half
/// of a wide character takes the whole character, and so does an end on
/// its [`tessera_grid::CellFlags::WIDE`] half; rows and columns outside
/// the frame are clipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Selection {
    /// Where the selection starts (the anchor).
    pub start: Point,
    /// Where it ends (inclusive).
    pub end: Point,
    /// Linear (text order) or block (rectangle).
    pub kind: SelectionKind,
}

impl Selection {
    /// A linear selection from `start` to `end`, both inclusive.
    #[must_use]
    pub const fn linear(start: Point, end: Point) -> Self {
        Self {
            start,
            end,
            kind: SelectionKind::Linear,
        }
    }

    /// A block selection of the rectangle with corners `start` and `end`.
    #[must_use]
    pub const fn block(start: Point, end: Point) -> Self {
        Self {
            start,
            end,
            kind: SelectionKind::Block,
        }
    }

    /// The columns this selection covers on `row`, inclusive, before wide
    /// characters are taken whole and before clipping to the frame:
    /// `None` when the row is outside it. `last_col` is the row's last
    /// column, where a linear selection's inner rows end.
    #[must_use]
    pub fn cols_on(&self, row: u16, last_col: u16) -> Option<(u16, u16)> {
        let (top, bottom) = if (self.start.row, self.start.col) <= (self.end.row, self.end.col) {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        };
        if row < top.row || row > bottom.row {
            return None;
        }
        match self.kind {
            SelectionKind::Linear => {
                let first = if row == top.row { top.col } else { 0 };
                let last = if row == bottom.row {
                    bottom.col
                } else {
                    last_col.max(first)
                };
                Some((first, last))
            }
            SelectionKind::Block => Some((
                self.start.col.min(self.end.col),
                self.start.col.max(self.end.col),
            )),
        }
    }
}

/// What an [`Overlay`] draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OverlayKind {
    /// A search match: [`crate::Theme::search_match`] over the cells.
    Match,
    /// The current search match: [`crate::Theme::search_match_current`]
    /// over the cells.
    CurrentMatch,
    /// A thin underline in each covered cell's foreground color, at the
    /// underline position (a hovered hyperlink).
    Underline,
}

/// Something drawn over a run of cells that is not part of them: search
/// matches and hyperlink underlines.
///
/// Overlays are drawn by the overlay pass between the cell backgrounds and
/// the glyphs, over the [`Selection`], by kind from bottom to top: matches,
/// the current match, link underlines. Overlapping overlays of one kind
/// draw their union, so a cell covered by several matches is highlighted
/// once. Like the selection, a run starting on a wide character's spacer
/// or ending on its first half takes the whole character, and anything
/// outside the frame is clipped.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Overlay {
    /// What to draw.
    pub kind: OverlayKind,
    /// The row, in [`FrameInput::rows`] indices.
    pub row: u16,
    /// The columns covered (end exclusive).
    pub cols: Range<u16>,
}

impl Overlay {
    /// An overlay of `kind` over `cols` of `row`.
    #[must_use]
    pub const fn new(kind: OverlayKind, row: u16, cols: Range<u16>) -> Self {
        Self { kind, row, cols }
    }
}

/// Everything the renderer needs to draw one frame.
///
/// Build one with [`FrameInput::new`] and the `with_*` methods (or set the
/// public fields directly).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FrameInput<'a> {
    /// The visible rows, top to bottom. Rows may differ in length; cells
    /// past the target's right edge and rows past its bottom are not
    /// drawn.
    pub rows: &'a [&'a [Cell]],
    /// The cursor, if the host shows one.
    pub cursor: Option<CursorState>,
    /// The selection, if any.
    pub selection: Option<Selection>,
    /// Search matches and link underlines, drawn over the selection (see
    /// [`Overlay`] for the order and how overlaps combine).
    pub overlays: &'a [Overlay],
    /// Whether the surface has keyboard focus. An unfocused surface draws
    /// every cursor shape as [`CursorShape::HollowBlock`].
    pub focused: bool,
    /// The grapheme cluster text of each row (`clusters[r]` for
    /// `rows[r]`, a [`tessera_grid::Line::graphemes`] copy). A cell with
    /// [`tessera_grid::CellFlags::CLUSTER`] is shaped from its whole
    /// cluster when its row has an entry here, else from its base
    /// character alone. Rows past the end of the slice have none.
    pub clusters: &'a [GraphemeTable],
    /// The rows that changed since the last frame this renderer drew, in
    /// [`Self::rows`] indices: rows outside it must hold the same cells
    /// and clusters as then, and the renderer reuses what it shaped and
    /// built for them. [`DamageRows::All`] (the default) redraws
    /// everything; the cursor, focus, theme, font and target size are
    /// compared by the renderer itself, so they need no damage; so are the
    /// selection and the overlays, row by row (a changed selection
    /// rebuilds only the rows whose coverage changed).
    pub damage: DamageRows,
}

impl<'a> FrameInput<'a> {
    /// A focused frame of `rows` with no cursor, selection or overlays.
    #[must_use]
    pub const fn new(rows: &'a [&'a [Cell]]) -> Self {
        Self {
            rows,
            cursor: None,
            selection: None,
            overlays: &[],
            focused: true,
            clusters: &[],
            damage: DamageRows::All,
        }
    }

    /// The same frame with `cursor`.
    #[must_use]
    pub const fn with_cursor(mut self, cursor: Option<CursorState>) -> Self {
        self.cursor = cursor;
        self
    }

    /// The same frame with `selection`.
    #[must_use]
    pub const fn with_selection(mut self, selection: Option<Selection>) -> Self {
        self.selection = selection;
        self
    }

    /// The same frame with `overlays`.
    #[must_use]
    pub const fn with_overlays(mut self, overlays: &'a [Overlay]) -> Self {
        self.overlays = overlays;
        self
    }

    /// The same frame with `focused`.
    #[must_use]
    pub const fn with_focus(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// The same frame with each row's grapheme cluster text.
    #[must_use]
    pub const fn with_clusters(mut self, clusters: &'a [GraphemeTable]) -> Self {
        self.clusters = clusters;
        self
    }

    /// The same frame with `damage`.
    #[must_use]
    pub fn with_damage(mut self, damage: DamageRows) -> Self {
        self.damage = damage;
        self
    }

    /// The shape the cursor is drawn with this frame, or `None` when no
    /// cursor is drawn (absent or not [`CursorState::visible`]).
    #[must_use]
    pub fn drawn_cursor(&self) -> Option<(CursorState, CursorShape)> {
        let cursor = self.cursor.filter(|c| c.visible)?;
        let shape = if self.focused {
            cursor.shape
        } else {
            CursorShape::HollowBlock
        };
        Some((cursor, shape))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_frames_are_focused_and_bare() {
        let frame = FrameInput::new(&[]);
        assert!(frame.focused);
        assert_eq!(frame.cursor, None);
        assert_eq!(frame.selection, None);
        assert_eq!(frame.overlays, &[] as &[Overlay]);
        assert_eq!(frame.drawn_cursor(), None);
        assert_eq!(frame.damage, DamageRows::All, "full redraw by default");
        assert_eq!(frame.clusters, &[] as &[GraphemeTable]);
    }

    #[test]
    fn linear_selections_run_in_text_order_either_way() {
        let a = Point::new(1, 5);
        let b = Point::new(3, 2);
        for sel in [Selection::linear(a, b), Selection::linear(b, a)] {
            assert_eq!(sel.cols_on(0, 9), None);
            assert_eq!(sel.cols_on(1, 9), Some((5, 9)), "first row to the end");
            assert_eq!(sel.cols_on(2, 9), Some((0, 9)), "inner rows whole");
            assert_eq!(sel.cols_on(3, 9), Some((0, 2)), "last row to the end point");
            assert_eq!(sel.cols_on(4, 9), None);
        }
        let one_row = Selection::linear(Point::new(2, 7), Point::new(2, 3));
        assert_eq!(one_row.cols_on(2, 9), Some((3, 7)));
        // A first row starting past the row's end still yields a range.
        let past = Selection::linear(Point::new(0, 50), Point::new(1, 0));
        assert_eq!(past.cols_on(0, 9), Some((50, 50)));
    }

    #[test]
    fn block_selections_are_rectangles() {
        let sel = Selection::block(Point::new(3, 2), Point::new(1, 6));
        assert_eq!(sel.cols_on(0, 9), None);
        for row in 1..=3 {
            assert_eq!(sel.cols_on(row, 9), Some((2, 6)), "row {row}");
        }
        assert_eq!(sel.cols_on(4, 9), None);
        assert_eq!(SelectionKind::default(), SelectionKind::Linear);
    }

    #[test]
    fn hidden_cursor_is_not_drawn() {
        let cursor = CursorState::new(1, 2, CursorShape::Beam).with_visible(false);
        let frame = FrameInput::new(&[]).with_cursor(Some(cursor));
        assert_eq!(frame.drawn_cursor(), None);
    }

    #[test]
    fn unfocused_cursor_is_hollow_whatever_its_shape() {
        for shape in [
            CursorShape::Block,
            CursorShape::Beam,
            CursorShape::Underline,
            CursorShape::HollowBlock,
        ] {
            let cursor = CursorState::new(0, 0, shape);
            let frame = FrameInput::new(&[]).with_cursor(Some(cursor));
            assert_eq!(frame.drawn_cursor(), Some((cursor, shape)));
            assert_eq!(
                frame.with_focus(false).drawn_cursor(),
                Some((cursor, CursorShape::HollowBlock))
            );
        }
    }
}
