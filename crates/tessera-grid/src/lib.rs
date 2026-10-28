//! The terminal's data model.
//!
//! Owns cells, lines, the visible grid, scrollback history, the cursor,
//! selections and per-line semantic marks (prompt boundaries, command exit
//! codes, working directory). It knows nothing about escape sequences or
//! pixels: `tessera-term` mutates it, `tessera-render` and `tessera-search`
//! read it, and plugins observe its marks.
//!
//! # Model
//!
//! A [`Grid`] is `rows × cols` [`Cell`]s organised as [`Line`]s, plus a
//! [`Scrollback`] ring of lines that scrolled off the top, capped at a
//! configurable number of lines. History and the visible rows can be read as
//! one logical buffer ([`Grid::logical_line`], [`Grid::logical_lines`]) for
//! search and rendering. An alternate screen without scrollback
//! ([`Grid::enter_alt_screen`]) keeps the primary screen intact while
//! full-screen programs run. Every mutation goes through a cursor-relative
//! primitive (`put_char`, `insert_lines`, `erase_in_display`, …) that
//! mirrors one VT operation. The primitives are total: any argument is
//! clamped so the grid never leaves its bounds and never panics.
//!
//! The grid carries a *pen* ([`Grid::pen`]): the [`Style`] applied to cells
//! written at the cursor and whose background fills cells that are erased or
//! scrolled into view. `tessera-term` updates it from SGR sequences.
//!
//! # Resize
//!
//! [`Grid::resize`] changes the size. The primary screen re-wraps its
//! soft-wrapped lines, scrollback included, to the new width and moves
//! rows between screen and scrollback as the height changes; the cursor
//! follows its character. The alternate screen is cut or padded instead.
//! ADR 0009 records the rules and how they compare with other emulators.
//!
//! # Wide characters and grapheme clusters
//!
//! [`Grid::put_char`] places characters by their `unicode-width` 0.2 width
//! ([`char_width`]). A two-column character (CJK, most emoji) occupies a
//! [`CellFlags::WIDE`] cell plus the [`CellFlags::SPACER`] after it. A
//! scalar that extends the grapheme cluster (UAX #29, via
//! `unicode-segmentation`) of the character before the cursor (a
//! combining mark, ZWJ, variation selector, skin-tone modifier, a flag's
//! second regional indicator) joins that cell instead of taking one: the
//! cell gets [`CellFlags::CLUSTER`] and the line's [`GraphemeTable`]
//! holds the whole cluster text, so [`Cell`] stays 20 bytes. The width of
//! a cluster is the width of its whole text, so a cluster can grow from
//! one column to two or shrink back. Every primitive keeps wide pairs and
//! the table consistent: overwriting, erasing, inserting or deleting
//! through half of a wide character blanks the other half. ADR 0008
//! records the rules and why.
//!
//! Assumptions this crate documents rather than enforces:
//!
//! - Origin mode (DECOM) is a terminal mode. The grid exposes
//!   [`Grid::scroll_region`] so the emulator can translate coordinates before
//!   calling [`Grid::goto`].

#![forbid(unsafe_code)]

mod cell;
mod grapheme;
mod grid;
mod line;
mod reflow;
mod scrollback;
mod tabs;

pub use cell::{Cell, CellFlags, Color, Point, Style, UnderlineStyle};
pub use grapheme::{GraphemeTable, MAX_CLUSTER_SCALARS, char_width};
pub use grid::{DEFAULT_MAX_SCROLLBACK, EraseDisplay, EraseLine, Grid, ScrollRegion};
pub use line::{Line, Mark};
pub use scrollback::Scrollback;
pub use tabs::TabStops;
