//! The emulator proper.
//!
//! Combines a [`tessera_vt::Parser`] with a [`tessera_grid::Grid`] and the
//! terminal mode state (cursor keys, bracketed paste, alternate screen, …).
//! Bytes in, grid mutations and side-effect requests (bell, title change,
//! clipboard write) out. The [`keyboard`] module goes the other way: it
//! encodes key events into the bytes written to the PTY. OSC handlers for
//! shell integration are delegated to `tessera-shell-integration` so this
//! crate stays about VT semantics.

#![forbid(unsafe_code)]

mod control;
mod csi;
mod esc;
pub mod keyboard;
mod sgr;

pub use keyboard::{
    Key, KeyEvent, KeyEventKind, KeyboardModes, KittyFlags, KittyFlagsMode, Modifiers,
    ModifyOtherKeys, encode_key, encode_key_into,
};

use tessera_grid::{Cell, Grid};
use tessera_vt::{Action, Parser};
use tracing::trace;

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
    state: State,
    /// An incomplete UTF-8 sequence (1 to 3 bytes) held back from the end of
    /// the last [`Terminal::advance`] call. See [`incomplete_utf8_suffix`].
    carry: Vec<u8>,
}

impl Terminal {
    /// Creates a terminal with an empty grid, auto-wrap (DECAWM) on and
    /// insert mode (IRM) off.
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: Parser::new(),
            state: State {
                grid: Grid::new(rows, cols),
                pending: Vec::new(),
                auto_wrap: true,
                insert_mode: false,
                last_printed: None,
            },
            carry: Vec::new(),
        }
    }

    /// Feeds raw PTY output into the terminal.
    ///
    /// Splitting the output across calls at any byte boundary gives the same
    /// result as one call. An incomplete UTF-8 sequence at the end of `bytes`
    /// is held back and fed together with the next call, because `vte` 0.15
    /// drops the byte after a character it completes across two calls
    /// (`Parser::advance_partial_utf8` returns `valid_up_to - old_bytes`
    /// rather than the completed character's length). Holding bytes back is
    /// the same as concatenating the input, so it is safe in every parser
    /// state; a partial character is never visible before it completes.
    pub fn advance(&mut self, bytes: &[u8]) {
        if self.carry.is_empty() {
            let (now, later) = bytes.split_at(bytes.len() - incomplete_utf8_suffix(bytes));
            self.feed(now);
            self.carry.extend_from_slice(later);
        } else {
            // Rare: only when the previous read ended inside a character.
            let mut joined = std::mem::take(&mut self.carry);
            joined.extend_from_slice(bytes);
            let split = joined.len() - incomplete_utf8_suffix(&joined);
            self.feed(&joined[..split]);
            joined.drain(..split);
            self.carry = joined;
        }
    }

    /// Hands `bytes` to the parser and applies every action it produces.
    fn feed(&mut self, bytes: &[u8]) {
        let state = &mut self.state;
        self.parser.advance(bytes, |action| state.apply(action));
    }

    /// Drains side-effect requests accumulated since the last call.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.state.pending)
    }

    /// Read access to the grid.
    pub fn grid(&self) -> &Grid {
        &self.state.grid
    }

    /// Whether auto-wrap mode (DECAWM) is on. Defaults to `true`.
    pub fn auto_wrap(&self) -> bool {
        self.state.auto_wrap
    }

    /// Turns auto-wrap mode (DECAWM) on or off.
    ///
    /// With auto-wrap on, a character printed while the cursor sits on the
    /// last column with a wrap pending starts the next line and marks the
    /// finished one as soft-wrapped. With it off, that character overwrites
    /// the last column instead and no line is ever marked wrapped. Toggling
    /// the mode leaves an already pending wrap armed; the next printed
    /// character applies whichever mode is current.
    pub fn set_auto_wrap(&mut self, on: bool) {
        self.state.auto_wrap = on;
    }

    /// Whether insert mode (IRM) is on. Defaults to `false`.
    pub fn insert_mode(&self) -> bool {
        self.state.insert_mode
    }

    /// Turns insert mode (IRM) on or off.
    ///
    /// In insert mode a printed character shifts the cells from the cursor
    /// to the end of the line one column right before it is written; the
    /// cell pushed past the last column is lost. Control characters are not
    /// affected.
    pub fn set_insert_mode(&mut self, on: bool) {
        self.state.insert_mode = on;
    }
}

/// Length of the incomplete UTF-8 sequence that ends `bytes`: a lead byte
/// followed by fewer continuation bytes than it announces, so at most 3.
/// Zero when `bytes` ends on a complete character, an ASCII byte or bytes
/// that can never start a valid sequence.
fn incomplete_utf8_suffix(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0xC0 == 0x80 {
            continue; // continuation byte: keep looking for the lead
        }
        let len = match byte {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => return 0,
        };
        return if back < len { back } else { 0 };
    }
    0
}

/// Everything an [`Action`] can change.
///
/// Kept apart from the [`Parser`] so the parser's sink can borrow it
/// mutably while the parser itself is borrowed.
struct State {
    grid: Grid,
    pending: Vec<Request>,
    auto_wrap: bool,
    insert_mode: bool,
    /// The last non-control character handed to [`State::print`], for REP
    /// (`CSI Ps b`), whether it took a cell, joined the previous cluster or
    /// was dropped; REP checks the cell it left behind. Every other action
    /// clears it once it has been applied, as xterm does, so REP only
    /// repeats a character printed immediately before it.
    last_printed: Option<char>,
}

impl State {
    /// Applies one parsed action.
    ///
    /// A printed non-control character becomes [`State::last_printed`];
    /// every other action (DEL included) clears it once it has been
    /// applied, REP included.
    fn apply(&mut self, action: Action) {
        // Printing is the hot path: handle it apart from the sequences so
        // it never reaches the drop glue of the heap-carrying variants.
        if let Action::Print(ch) = action {
            self.print(ch);
            // Recorded here rather than in `print`, whose early returns
            // would skip it.
            self.last_printed = (!ch.is_control()).then_some(ch);
        } else {
            self.apply_sequence(action);
            self.last_printed = None;
        }
    }

    /// Applies every action but [`Action::Print`].
    fn apply_sequence(&mut self, action: Action) {
        match action {
            Action::Execute(byte) => self.execute(byte),
            // The header overflowed the parser's storage: the sequence is
            // incomplete and must not be guessed at.
            dropped @ (Action::Csi { ignore: true, .. } | Action::Esc { ignore: true, .. }) => {
                trace!(action = ?dropped, "overflowing sequence dropped");
            }
            Action::Csi {
                params,
                subparams,
                intermediates,
                action,
                ..
            } => self.csi(&params, subparams, &intermediates, action),
            Action::Esc {
                intermediates,
                byte,
                ..
            } => self.esc(&intermediates, byte),
            ignored @ (Action::Print(_)
            | Action::Osc(_)
            | Action::DcsStart { .. }
            | Action::DcsData(_)
            | Action::DcsEnd) => trace!(action = ?ignored, "sequence not applied yet"),
            unknown => trace!(action = ?unknown, "unknown action ignored"),
        }
    }

    /// Writes `ch` at the cursor, honouring auto-wrap and insert mode.
    ///
    /// Width comes from the grid ([`tessera_grid::char_width`]): a wide
    /// character needs two columns, and a scalar that joins the previous
    /// character's grapheme cluster ([`Grid::would_attach`]) takes none, so
    /// it bypasses insert mode, wrapping and DECAWM entirely.
    fn print(&mut self, ch: char) {
        // The parser hands DEL (0x7F) over as a printable; like any other
        // control character it must never occupy a cell.
        if ch.is_control() {
            trace!(ch = ?ch, "control character dropped from print");
            return;
        }
        if self.auto_wrap && !self.insert_mode {
            // The default modes are exactly what the grid's own write does:
            // attach, drop zero-width scalars, wrap first when needed.
            self.grid.put_char(ch);
            return;
        }
        if self.grid.would_attach(ch) {
            self.grid.put_char(ch);
            return;
        }
        let width = match tessera_grid::char_width(ch) {
            Some(width) if width > 0 => width,
            _ => {
                trace!(ch = ?ch, "zero-width scalar with nothing to join dropped");
                return;
            }
        };
        let last = self.grid.cols() - 1;
        if width > self.grid.cols() {
            trace!(ch = ?ch, "wide character dropped: the grid has one column");
            return;
        }
        if !self.auto_wrap {
            if self.grid.pending_wrap() {
                // The grid always arms a pending wrap on the last column.
                // Without DECAWM the character overwrites that column.
                let row = self.grid.cursor().row;
                self.grid.goto(row, last);
            }
            if width == 2 && self.grid.cursor().col == last {
                // No room and no wrap: the character is dropped (xterm).
                return;
            }
            if self.insert_mode {
                self.grid.insert_chars(width);
            }
            if width == 1 {
                // `put_char` would look for a cluster to join again, now
                // left of the cursor the goto just cleared the pending wrap
                // for; the character was already found not to join one.
                let pen = self.grid.pen();
                self.grid.put_cell(Cell::new(ch, pen));
            } else {
                self.grid.put_char(ch);
            }
            return;
        }
        if self.insert_mode {
            let stranded = width == 2 && self.grid.cursor().col == last;
            if self.grid.pending_wrap() || stranded {
                // `insert_chars` disarms the pending wrap, so wrap first and
                // insert on the new line, as xterm does. A wide character
                // with one column left first blanks that column, as the
                // grid does in replace mode.
                if !self.grid.pending_wrap() {
                    self.grid.erase_chars(1);
                }
                self.wrap_line();
            }
            self.grid.insert_chars(width);
        }
        self.grid.put_char(ch);
    }

    /// Resolves a pending wrap: marks the cursor's line soft-wrapped and
    /// moves to the start of the next line, scrolling at the bottom of the
    /// scroll region. Mirrors what [`Grid::put_cell`] does before a write.
    fn wrap_line(&mut self) {
        let row = self.grid.cursor().row;
        if let Some(line) = self.grid.line_mut(row) {
            line.wrapped = true;
        }
        self.grid.carriage_return();
        self.grid.line_feed();
    }
}
