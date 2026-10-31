//! Device control strings (`DCS ... ST`): the [`Action::DcsStart`],
//! [`Action::DcsData`] and [`Action::DcsEnd`] half of the dispatcher.
//!
//! Only DECRQSS (`DCS $ q Pt ST`, request status string) is handled: its
//! request is collected, at most `MAX_DECRQSS_LEN` (64) bytes, and
//! answered when the string ends (see the `reports` module). A longer
//! request is answered as invalid, never echoed. A DECRQSS with
//! parameters in its header (`DCS 1 $ q`) is a different string and gets
//! no reply. Every other DCS (Sixel, XTGETTCAP `DCS + q`, tmux control
//! mode, …) is consumed silently; its payload is dropped as it arrives, so
//! a megabyte of image data costs no memory here.
//!
//! The parser ends a string the same way whether ST finished it or CAN or
//! SUB aborted it, so an aborted DECRQSS is answered too (xterm discards
//! it).
//!
//! [`Action::DcsStart`]: tessera_vt::Action::DcsStart
//! [`Action::DcsData`]: tessera_vt::Action::DcsData
//! [`Action::DcsEnd`]: tessera_vt::Action::DcsEnd

use tracing::trace;

use crate::State;
use crate::reports::MAX_DECRQSS_LEN;

/// The device control string in progress.
#[derive(Debug, Default)]
pub(crate) enum Dcs {
    /// None, or one whose payload is dropped.
    #[default]
    Ignored,
    /// A DECRQSS whose request is being collected.
    StatusRequest {
        /// The request so far, at most [`MAX_DECRQSS_LEN`] bytes.
        pt: Vec<u8>,
        /// Set once the request outgrew [`MAX_DECRQSS_LEN`].
        overflowed: bool,
    },
}

impl State {
    /// A string starts: `params`, `intermediates` and `action` from its
    /// header; `ignore` when the header overflowed the parser.
    pub(crate) fn dcs_start(
        &mut self,
        params: &[u16],
        intermediates: &[u8],
        action: char,
        ignore: bool,
    ) {
        self.dcs = match (intermediates, action) {
            (b"$", 'q') if !ignore && matches!(params, [] | [0]) => Dcs::StatusRequest {
                pt: Vec::new(),
                overflowed: false,
            },
            _ => {
                trace!(?params, ?intermediates, ?action, "DCS ignored");
                Dcs::Ignored
            }
        };
    }

    /// A piece of the current string's payload.
    pub(crate) fn dcs_data(&mut self, bytes: &[u8]) {
        if let Dcs::StatusRequest { pt, overflowed } = &mut self.dcs {
            if pt.len() + bytes.len() > MAX_DECRQSS_LEN {
                *overflowed = true;
            } else {
                pt.extend_from_slice(bytes);
            }
        }
    }

    /// The current string ended (ST) or was aborted (CAN, SUB).
    pub(crate) fn dcs_end(&mut self) {
        match std::mem::take(&mut self.dcs) {
            Dcs::StatusRequest {
                overflowed: true, ..
            } => {
                self.reply_dcs(format_args!("0$r"));
            }
            Dcs::StatusRequest { pt, .. } => self.status_string(&pt),
            Dcs::Ignored => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Terminal;

    /// A request never keeps more than `MAX_DECRQSS_LEN` bytes, however
    /// long the string: the bound is memory, the reply is the same invalid
    /// one either way.
    #[test]
    fn a_long_request_keeps_at_most_the_bound() {
        let mut term = Terminal::new(2, 10);
        term.advance(b"\x1bP$q");
        term.advance(&b"m".repeat(1 << 20));
        match &term.state.dcs {
            Dcs::StatusRequest { pt, overflowed } => {
                assert!(*overflowed);
                assert!(pt.len() <= MAX_DECRQSS_LEN, "{} bytes kept", pt.len());
            }
            Dcs::Ignored => panic!("the request was dropped"),
        }
        term.advance(b"\x1b\\");
        assert_eq!(
            term.take_requests(),
            [crate::Request::Reply(b"\x1bP0$r\x1b\\".to_vec())]
        );
    }
}
