//! Rectangular area operations (ADR 0046): DECERA (`CSI Pt;Pl;Pb;Pr $ z`),
//! DECFRA (`CSI Pch;Pt;Pl;Pb;Pr $ x`), DECCRA
//! (`CSI Pts;Pls;Pbs;Prs;Pps;Ptd;Pld;Ppd $ v`) and DECSERA
//! (`CSI Pt;Pl;Pb;Pr $ {`), VT420 and up.
//!
//! # Coordinates
//!
//! Positions are 1-based. An absent parameter and an explicit `0` mean the
//! default: the first row or column for a top or left edge (and for
//! DECCRA's destination), the last one for a bottom or right edge. A
//! position past the screen is clamped to its last row or column, so a
//! rectangle starting beyond the screen still covers the last row or
//! column. A rectangle whose top is below its bottom, or whose left is
//! right of its right, after the defaults and the clamp, does nothing.
//!
//! In origin mode (DECOM) rows count from the top margin and columns from
//! the left one, and every edge is clamped to the origin region (the
//! scroll region and the left and right margins); the defaults are that
//! region's edges, counted once (xterm adds the margin twice to an omitted
//! parameter, Tessera does not). Outside origin mode the margins do not
//! limit the operations. DECCRA's destination is clipped to the same
//! bounds: the screen, or the origin region in origin mode. The page
//! parameters of DECCRA are accepted and ignored (one page).
//!
//! # Effects
//!
//! - DECERA erases every cell, protected or not, to a blank with the pen's
//!   background only ([`tessera_grid::Grid::erase_rect`]).
//! - DECFRA fills with `Pch` in the full pen style, [`Style::PROTECTED`]
//!   included ([`tessera_grid::Grid::fill_rect`]). `Pch` 0 or absent is a
//!   space; 32-126 and 160-255 (Latin-1) are taken, mapped through the
//!   character set invoked into GL (or GR, for 160-255) as printing would
//!   map them, without consuming a pending single shift; a value from 256
//!   up must be a one-column Unicode scalar that is not a noncharacter. A
//!   control character (1-31, 127-159), a wide, zero-width or combining
//!   character, a surrogate and a noncharacter (65535, what a saturated
//!   parameter becomes) make DECFRA do nothing.
//! - DECCRA copies the source to the destination, text, style, clusters
//!   and OSC 8 links included ([`tessera_grid::Grid::copy_rect`]).
//! - DECSERA erases to spaces that keep their style; in DEC protection mode
//!   (DECSCA) it keeps the protected cells, in ISO mode (SPA/EPA) and with
//!   protection off it erases them too, as xterm does
//!   ([`tessera_grid::Grid::selective_erase_rect`]).
//!
//! None of them moves the cursor, clears a pending wrap, changes the pen,
//! the margins or a mode, or touches the scrollback, the lines' soft-wrap
//! flags or their marks; none answers. Below conformance level 4 (DECSCL
//! 61-63) all four are ignored and the screen keeps its content. A colon
//! sub-parameter voids any of them; a private marker (`CSI ? ... $ z`) is
//! another sequence, ignored.
//!
//! Each costs a constant per cell of the clipped rectangle, with no per-cell
//! event; DECCRA's only buffers are scratch of at most one row (cells,
//! and the clusters and links of a row), at most the screen's width
//! (docs/security.md).

use tessera_grid::{Cell, Rect, char_width};
use tracing::trace;

use crate::State;
use crate::charset::Charsets;
use crate::csi::Protection;

/// The rows and columns positions may address: the screen, or the origin
/// region in origin mode. All four inclusive and 0-based.
#[derive(Clone, Copy)]
struct Bounds {
    top: u16,
    bottom: u16,
    left: u16,
    right: u16,
}

impl Bounds {
    /// Parameter `index` as a first row (`row`) or column, from `min`:
    /// absent and 0 give `min`, larger values are clamped to `max`.
    fn first(params: &[u16], index: usize, min: u16, max: u16) -> u16 {
        match params.get(index) {
            None | Some(0) => min,
            Some(&n) => min.saturating_add(n - 1).min(max),
        }
    }

    /// Parameter `index` as a last row or column: absent and 0 give `max`.
    fn last(params: &[u16], index: usize, min: u16, max: u16) -> u16 {
        match params.get(index) {
            None | Some(0) => max,
            Some(&n) => min.saturating_add(n - 1).min(max),
        }
    }

    /// The rectangle of `params[at..at + 4]` (top, left, bottom, right),
    /// or `None` when it is inverted.
    fn rect(self, params: &[u16], at: usize) -> Option<Rect> {
        let rect = Rect {
            top: Self::first(params, at, self.top, self.bottom),
            left: Self::first(params, at + 1, self.left, self.right),
            bottom: Self::last(params, at + 2, self.top, self.bottom),
            right: Self::last(params, at + 3, self.left, self.right),
        };
        (rect.top <= rect.bottom && rect.left <= rect.right).then_some(rect)
    }

    /// The bounds as a rectangle, for DECCRA's destination clip.
    const fn as_rect(self) -> Rect {
        Rect {
            top: self.top,
            left: self.left,
            bottom: self.bottom,
            right: self.right,
        }
    }
}

/// Whether `ch` is a Unicode noncharacter (U+FDD0..U+FDEF and the last two
/// code points of every plane).
fn is_noncharacter(ch: char) -> bool {
    let code = u32::from(ch);
    (0xFDD0..=0xFDEF).contains(&code) || code & 0xFFFE == 0xFFFE
}

/// Whether DECFRA may fill with `ch`: a one-column scalar that is neither a
/// control character nor a noncharacter.
fn fillable(ch: char) -> bool {
    !ch.is_control() && !is_noncharacter(ch) && char_width(ch) == Some(1)
}

impl State {
    /// The bounds positions are clamped to now.
    fn rect_bounds(&self) -> Bounds {
        let grid = &self.grid;
        if grid.origin_mode() {
            let region = grid.scroll_region();
            let margins = grid.margins();
            Bounds {
                top: region.top,
                bottom: region.bottom,
                left: margins.left,
                right: margins.right,
            }
        } else {
            Bounds {
                top: 0,
                bottom: grid.rows().saturating_sub(1),
                left: 0,
                right: grid.cols().saturating_sub(1),
            }
        }
    }

    /// DECERA, DECFRA, DECCRA or DECSERA by `action` (`z`, `x`, `v`, `{`,
    /// with the `$` intermediate). The caller checked the conformance level
    /// and the sub-parameters.
    pub(crate) fn rect_op(&mut self, params: &[u16], action: char) {
        let bounds = self.rect_bounds();
        let pen = self.grid.pen();
        match action {
            'z' => {
                if let Some(rect) = bounds.rect(params, 0) {
                    self.grid.erase_rect(rect, pen);
                }
            }
            'x' => {
                let Some(ch) = self.fill_char(params.first().copied().unwrap_or(0)) else {
                    trace!(?params, "DECFRA with an unfillable character ignored");
                    return;
                };
                if let Some(rect) = bounds.rect(params, 1) {
                    self.grid.fill_rect(rect, Cell::new(ch, pen));
                }
            }
            'v' => {
                let Some(src) = bounds.rect(params, 0) else {
                    return;
                };
                // Parameter 4 is the source page, 7 the destination page.
                let dst_top = Bounds::first(params, 5, bounds.top, bounds.bottom);
                let dst_left = Bounds::first(params, 6, bounds.left, bounds.right);
                self.grid
                    .copy_rect(src, dst_top, dst_left, bounds.as_rect(), pen);
            }
            '{' => {
                if let Some(rect) = bounds.rect(params, 0) {
                    let honour = self.protection == Protection::Dec;
                    self.grid.selective_erase_rect(rect, honour);
                }
            }
            _ => trace!(?action, "not a rectangle operation"),
        }
        // DECFRA with a protected pen writes protected cells: the pen
        // already marked the screen as possibly holding some (`note_pen`).
        // DECCRA copies only what the screen holds, the erases add none.
    }

    /// The character DECFRA fills with for `pch`, or `None` when it is not
    /// one it takes.
    fn fill_char(&self, pch: u16) -> Option<char> {
        let code = match pch {
            0 => 0x20,
            32..=126 | 160.. => u32::from(pch),
            _ => return None,
        };
        let ch = char::from_u32(code).filter(|&ch| fillable(ch))?;
        if !self.charsets.active || code > 0xFF {
            return Some(ch);
        }
        // Printing would map it through GL or GR; a copy of the sets
        // without the single shift, so a pending SS2 or SS3 stays pending.
        let mut sets = Charsets::from(self.charsets.saved());
        sets.translate(ch, self.modes.nrcs, self.conformance_level)
            .filter(|&ch| fillable(ch))
    }
}
