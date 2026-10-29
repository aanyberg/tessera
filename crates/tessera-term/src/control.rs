//! C0 and C1 control functions: the [`Action::Execute`] half of the
//! dispatcher.
//!
//! Only the format effectors and the bell have an effect today. Every other
//! control byte is ignored; the ones with no meaning to a terminal emulator
//! silently, the rest with a trace-level log so unimplemented controls are
//! visible while debugging.
//!
//! [`Action::Execute`]: tessera_vt::Action::Execute

use tracing::trace;

use crate::{Request, State};

/// Null: padding, ignored.
const NUL: u8 = 0x00;
/// Bell.
const BEL: u8 = 0x07;
/// Backspace.
const BS: u8 = 0x08;
/// Horizontal tab.
const HT: u8 = 0x09;
/// Line feed.
const LF: u8 = 0x0A;
/// Vertical tab, treated as LF.
const VT: u8 = 0x0B;
/// Form feed, treated as LF.
const FF: u8 = 0x0C;
/// Carriage return.
const CR: u8 = 0x0D;
/// Shift out (invoke G1). G0 and G1 are both ASCII until character sets land.
const SO: u8 = 0x0E;
/// Shift in (invoke G0).
const SI: u8 = 0x0F;
/// Delete. The parser reports it as a printable, but it is a no-op either way.
const DEL: u8 = 0x7F;

impl State {
    /// Executes one control byte.
    ///
    /// - BEL queues exactly one [`Request::Bell`].
    /// - BS moves one column left, never wrapping back to the previous
    ///   line, and disarms a pending wrap.
    /// - HT moves to the next tab stop or the last column. With a wrap
    ///   pending the cursor already sits on the last column, so HT leaves
    ///   the pending wrap armed, as xterm and `WezTerm` do.
    /// - LF, VT and FF move down one line, scrolling at the bottom of the
    ///   scroll region. The column is kept unless newline mode (LNM, `CSI
    ///   20 h`) is on; then they also return the carriage.
    /// - CR moves to column zero.
    /// - NUL, SO, SI and DEL are ignored; any other byte is ignored and
    ///   logged at trace level.
    pub(crate) fn execute(&mut self, byte: u8) {
        match byte {
            BEL => self.pending.push(Request::Bell),
            BS => self.grid.cursor_left(1),
            HT => {
                if !self.grid.pending_wrap() {
                    self.grid.tab_forward(1);
                }
            }
            LF | VT | FF => {
                self.grid.line_feed();
                if self.modes.linefeed_newline {
                    self.grid.carriage_return();
                }
            }
            CR => self.grid.carriage_return(),
            NUL | SO | SI | DEL => {}
            _ => trace!("control byte {byte:#04x} ignored"),
        }
    }
}
