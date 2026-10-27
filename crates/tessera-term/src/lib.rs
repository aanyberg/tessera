//! The emulator proper.
//!
//! Combines a [`tessera_vt::Parser`] with a [`tessera_grid::Grid`] and the
//! terminal mode state (cursor keys, bracketed paste, alternate screen, …).
//! Bytes in, grid mutations and side-effect requests (bell, title change,
//! clipboard write) out. OSC handlers for shell integration are delegated to
//! `tessera-shell-integration` so this crate stays about VT semantics.

#![forbid(unsafe_code)]

use tessera_grid::Grid;
use tessera_vt::{Action, Parser};

/// Side effects the host must perform on behalf of the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// Ring the bell.
    Bell,
    /// Set the window or tab title.
    SetTitle(String),
}

/// A terminal: parser + grid + modes.
pub struct Terminal {
    parser: Parser,
    grid: Grid,
    pending: Vec<Request>,
}

impl Terminal {
    /// Creates a terminal with an empty grid.
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: Parser::new(),
            grid: Grid::new(rows, cols),
            pending: Vec::new(),
        }
    }

    /// Feeds raw PTY output into the terminal.
    pub fn advance(&mut self, bytes: &[u8]) {
        let pending = &mut self.pending;
        self.parser.advance(bytes, |action| {
            if let Action::Execute(0x07) = action {
                pending.push(Request::Bell);
            }
            // Grid mutation lands in M1.
        });
    }

    /// Drains side-effect requests accumulated since the last call.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.pending)
    }

    /// Read access to the grid.
    pub fn grid(&self) -> &Grid {
        &self.grid
    }
}
