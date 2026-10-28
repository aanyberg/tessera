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
/// Built with [`CursorState::new`] (and [`CursorState::with_visible`]);
/// the struct is `#[non_exhaustive]` so a two-column cursor (#22) can add a
/// field.
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

/// A selected range of cells, in [`FrameInput::rows`] coordinates.
///
/// Data only in this release: the renderer accepts it but does not draw
/// it yet (issue #22 adds the selection pass).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Selection {
    /// Where the selection starts.
    pub start: Point,
    /// Where it ends (inclusive).
    pub end: Point,
}

/// Something drawn over the grid that is not a cell: search highlights,
/// hyperlink underlines, IME preedit and the like.
///
/// No overlay exists yet; issue #22 adds the first ones. The type is
/// uninhabited until then, so `&[]` is the only value of
/// [`FrameInput::overlays`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Overlay {}

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
    /// The selection, if any (not drawn yet, see [`Selection`]).
    pub selection: Option<Selection>,
    /// Overlays drawn over the grid (none exist yet, see [`Overlay`]).
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
    /// compared by the renderer itself, so they need no damage.
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
        assert_eq!(frame.overlays, [] as [Overlay; 0]);
        assert_eq!(frame.drawn_cursor(), None);
        assert_eq!(frame.damage, DamageRows::All, "full redraw by default");
        assert_eq!(frame.clusters, &[] as &[GraphemeTable]);
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
