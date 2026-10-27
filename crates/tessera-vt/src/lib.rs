//! Escape-sequence parsing.
//!
//! Wraps the `vte` state machine and turns raw bytes into a stream of typed
//! [`Action`] values. The parser never touches terminal state: it only
//! describes what the byte stream asked for. `tessera-term` applies actions to
//! a grid. Keeping these apart makes the parser fuzzable in isolation and lets
//! plugins observe actions without owning a grid.

#![forbid(unsafe_code)]

/// A fully decoded instruction from the byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Print a grapheme at the cursor.
    Print(char),
    /// A C0/C1 control byte such as `\n` or `\r`.
    Execute(u8),
    /// A CSI sequence: parameters, intermediates and final byte.
    Csi {
        /// Numeric parameters, sub-parameters flattened.
        params: Vec<u16>,
        /// Intermediate bytes.
        intermediates: Vec<u8>,
        /// Final byte selecting the command.
        action: char,
    },
    /// An OSC sequence split on `;`.
    Osc(Vec<Vec<u8>>),
    /// An ESC sequence.
    Esc {
        /// Intermediate bytes.
        intermediates: Vec<u8>,
        /// Final byte.
        byte: u8,
    },
}

/// Incremental parser. Feed it bytes, collect actions.
#[derive(Default)]
pub struct Parser {
    inner: vte::Parser,
}

impl Parser {
    /// Creates a parser in the ground state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses `bytes`, invoking `sink` once per decoded action.
    pub fn advance(&mut self, bytes: &[u8], mut sink: impl FnMut(Action)) {
        let mut perform = Performer { sink: &mut sink };
        self.inner.advance(&mut perform, bytes);
    }
}

struct Performer<'a> {
    sink: &'a mut dyn FnMut(Action),
}

impl vte::Perform for Performer<'_> {
    fn print(&mut self, c: char) {
        (self.sink)(Action::Print(c));
    }

    fn execute(&mut self, byte: u8) {
        (self.sink)(Action::Execute(byte));
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let params = params.iter().flatten().copied().collect();
        (self.sink)(Action::Csi {
            params,
            intermediates: intermediates.to_vec(),
            action,
        });
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        (self.sink)(Action::Osc(params.iter().map(|p| p.to_vec()).collect()));
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        (self.sink)(Action::Esc {
            intermediates: intermediates.to_vec(),
            byte,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_print_and_csi() {
        let mut parser = Parser::new();
        let mut out = Vec::new();
        parser.advance(b"a\x1b[2;3Hb", |a| out.push(a));
        assert_eq!(
            out,
            vec![
                Action::Print('a'),
                Action::Csi {
                    params: vec![2, 3],
                    intermediates: vec![],
                    action: 'H',
                },
                Action::Print('b'),
            ]
        );
    }
}
