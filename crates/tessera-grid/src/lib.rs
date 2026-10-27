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
//! Assumptions this crate documents rather than enforces:
//!
//! - Every [`Cell`] is one column wide. Wide-character spacers, combining
//!   marks and grapheme clusters are the emulator's concern and will extend
//!   [`Cell`] in a later milestone.
//! - Origin mode (DECOM) is a terminal mode. The grid exposes
//!   [`Grid::scroll_region`] so the emulator can translate coordinates before
//!   calling [`Grid::goto`].
//! - Resize and reflow are not part of this crate yet; a grid keeps the size
//!   it was created with.

#![forbid(unsafe_code)]

mod cell;
mod grid;
mod line;
mod scrollback;
mod tabs;

pub use cell::{Cell, Color, Point, Style};
pub use grid::{DEFAULT_MAX_SCROLLBACK, EraseDisplay, EraseLine, Grid, ScrollRegion};
pub use line::{Line, Mark};
pub use scrollback::Scrollback;
pub use tabs::TabStops;
