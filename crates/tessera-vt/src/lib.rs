//! Escape-sequence parsing.
//!
//! Wraps the `vte` state machine and turns raw bytes into a stream of typed
//! [`Action`] values. The parser never touches terminal state: it only
//! describes what the byte stream asked for. `tessera-term` applies actions to
//! a grid. Keeping these apart makes the parser fuzzable in isolation and lets
//! plugins observe actions without owning a grid.
//!
//! # Memory bounds
//!
//! The parser bounds its own allocations per sequence kind. The guarantees
//! are deliberately precise, because `tessera-term` and the plugin host rely
//! on them:
//!
//! - CSI and ESC headers (parameters and intermediates) are capped by `vte`.
//!   When a sequence overflows those caps the action carries `ignore: true`
//!   and consumers must drop it rather than act on a silently truncated
//!   header.
//! - DCS payloads are streamed as [`Action::DcsData`] chunks of at most
//!   [`MAX_DCS_CHUNK`] bytes, so a multi-megabyte Sixel image never lives in
//!   memory as one allocation inside the parser.
//! - APC, PM and SOS strings are swallowed: the parser consumes them up to the
//!   string terminator and emits nothing. They have no terminal semantics and
//!   buffering them would be an attacker-controlled allocation.
//! - **OSC is not yet bounded.** `vte` (with its default `std` feature)
//!   buffers the whole OSC string in a growable `Vec` and delivers it as one
//!   [`Action::Osc`], so an OSC payload is as large as the writer makes it.
//!   OSC 52 (clipboard) legitimately carries large payloads, so the cap is a
//!   policy decision tracked as a follow-up ("vt: bound OSC payloads") rather
//!   than something this crate imposes silently. Until then, consumers that
//!   retain OSC data must enforce their own limit. The test
//!   `osc_is_currently_unbounded` documents the gap.
//!
//! # String terminators
//!
//! `vte` leaves the 7-bit string terminator `ESC \` to the escape dispatcher,
//! so a DCS closed with `ESC \` yields [`Action::DcsEnd`] followed by
//! [`Action::Esc`] with `byte == b'\\'`. The 8-bit terminator (`0x9C`) ends a
//! DCS on its own. `CAN` (`0x18`) and `SUB` (`0x1A`) abort a DCS: the parser
//! emits [`Action::DcsEnd`] and then [`Action::Execute`] for the control byte.

#![forbid(unsafe_code)]

/// Upper bound on the payload of one [`Action::DcsData`].
///
/// A DCS payload longer than this is delivered as several consecutive
/// `DcsData` actions. Chunk boundaries do not depend on how the input was
/// split across [`Parser::advance`] calls: a chunk is flushed only when it
/// is full or when the sequence ends.
pub const MAX_DCS_CHUNK: usize = 4096;

/// A fully decoded instruction from the byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Print a grapheme at the cursor.
    Print(char),
    /// A C0/C1 control byte such as `\n` or `\r`.
    Execute(u8),
    /// A CSI sequence: parameters, intermediates and final byte.
    #[non_exhaustive]
    Csi {
        /// Numeric parameters, sub-parameters flattened.
        params: Vec<u16>,
        /// Which entries of `params` are sub-parameters: bit `i` is set when
        /// `params[i]` was introduced by `:` rather than `;` (or was first).
        ///
        /// `CSI 4:3 m` gives `params == [4, 3]` and `subparams == 0b10`;
        /// `CSI 38:2::1:2:3 m` gives `[38, 2, 0, 1, 2, 3]` and `0b11_1110`;
        /// `CSI 1;4:3;5 m` gives `[1, 4, 3, 5]` and `0b100`. A parameter
        /// whose bit is clear starts a group; the set bits that follow it
        /// are that group's sub-parameters. `vte` keeps at most 32 entries
        /// (more set `ignore`), so every entry has a bit.
        subparams: u32,
        /// Intermediate bytes.
        intermediates: Vec<u8>,
        /// Final byte selecting the command.
        action: char,
        /// `true` when the sequence carried more parameters or intermediates
        /// than the parser keeps. The header is incomplete; drop the action.
        ignore: bool,
    },
    /// An OSC sequence split on `;`.
    Osc(Vec<Vec<u8>>),
    /// An ESC sequence.
    #[non_exhaustive]
    Esc {
        /// Intermediate bytes.
        intermediates: Vec<u8>,
        /// Final byte.
        byte: u8,
        /// `true` when the sequence carried more intermediates than the
        /// parser keeps. The header is incomplete; drop the action.
        ignore: bool,
    },
    /// Start of a device control string (`ESC P` / `0x90`).
    ///
    /// Followed by zero or more [`Action::DcsData`] and exactly one
    /// [`Action::DcsEnd`]. The header mirrors [`Action::Csi`]; the payload
    /// is streamed separately so it never has to be held in one allocation.
    #[non_exhaustive]
    DcsStart {
        /// Numeric parameters, sub-parameters flattened.
        params: Vec<u16>,
        /// Intermediate bytes.
        intermediates: Vec<u8>,
        /// Final byte selecting the command (`q` for Sixel, `p` for tmux
        /// control mode, …).
        action: char,
        /// `true` when the header overflowed the parser's parameter or
        /// intermediate storage. The payload is then dropped: no
        /// [`Action::DcsData`] follows, only [`Action::DcsEnd`].
        ignore: bool,
    },
    /// A slice of the current device control string's payload.
    ///
    /// Never empty and never longer than [`MAX_DCS_CHUNK`] bytes. Only
    /// emitted between [`Action::DcsStart`] and [`Action::DcsEnd`].
    ///
    /// # Known limitation
    ///
    /// The payload is 7-bit only. `vte`'s DCS passthrough state forwards
    /// `0x00..=0x7E` (except `CAN`, `SUB` and `ESC`) and silently drops every
    /// byte in `0x80..=0xFF` other than the `0x9C` terminator. Consumers must
    /// not expect raw 8-bit or UTF-8 data here. This is harmless for Sixel,
    /// whose alphabet is `0x20..=0x7E`, but it will strip non-ASCII window and
    /// pane names from future tmux `-CC` notifications; that is tracked
    /// alongside the OSC follow-up.
    DcsData(Vec<u8>),
    /// End of the current device control string, whether terminated or
    /// aborted.
    DcsEnd,
}

/// Incremental parser. Feed it bytes, collect actions.
#[derive(Default)]
pub struct Parser {
    inner: vte::Parser,
    dcs: DcsState,
}

/// Payload accumulator for the DCS currently being parsed.
///
/// Lives on the [`Parser`] rather than the per-call performer so that chunk
/// boundaries are independent of how bytes arrive from the PTY.
#[derive(Default)]
struct DcsState {
    /// Pending payload bytes, at most [`MAX_DCS_CHUNK`].
    buf: Vec<u8>,
    /// Set when the active DCS header was flagged `ignore`; payload bytes are
    /// discarded instead of buffered.
    ignoring: bool,
}

impl Parser {
    /// Creates a parser in the ground state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses `bytes`, invoking `sink` once per decoded action.
    pub fn advance(&mut self, bytes: &[u8], mut sink: impl FnMut(Action)) {
        let mut perform = Performer {
            sink: &mut sink,
            dcs: &mut self.dcs,
        };
        self.inner.advance(&mut perform, bytes);
    }

    /// Bytes currently held back for the next [`Action::DcsData`].
    ///
    /// Always at most [`MAX_DCS_CHUNK`]. Exposed for tests and diagnostics;
    /// consumers do not need it to drive the parser.
    #[must_use]
    pub fn pending_dcs_len(&self) -> usize {
        self.dcs.buf.len()
    }
}

/// Flattens `vte`'s parameter groups into one list plus the sub-parameter
/// bitmask described on [`Action::Csi::subparams`].
fn flatten_params(params: &vte::Params) -> (Vec<u16>, u32) {
    let mut flat = Vec::with_capacity(params.len());
    let mut subparams = 0u32;
    for group in params {
        if let Some((&head, rest)) = group.split_first() {
            flat.push(head);
            for &sub in rest {
                // `vte` stores at most 32 entries, so the shift never
                // overflows; `checked_shl` keeps that an assumption rather
                // than a panic.
                let index = u32::try_from(flat.len()).unwrap_or(u32::MAX);
                subparams |= 1u32.checked_shl(index).unwrap_or(0);
                flat.push(sub);
            }
        }
    }
    (flat, subparams)
}

struct Performer<'a> {
    sink: &'a mut dyn FnMut(Action),
    dcs: &'a mut DcsState,
}

impl Performer<'_> {
    fn flush_dcs(&mut self) {
        if self.dcs.buf.is_empty() {
            return;
        }
        // Hand the buffer to the consumer and leave an empty, unallocated one
        // behind; `put` reserves the next chunk on demand, so a parser that
        // has finished a DCS holds no payload memory at all.
        let chunk = std::mem::take(&mut self.dcs.buf);
        (self.sink)(Action::DcsData(chunk));
    }
}

impl vte::Perform for Performer<'_> {
    fn print(&mut self, c: char) {
        // `vte` dispatches a UTF-8 encoded C1 control (U+0080..=U+009F) as
        // `execute` when both bytes arrive in one `advance` call but as
        // `print` when the sequence straddles two calls. Normalise to
        // `Execute` so the action stream never depends on chunk boundaries;
        // this also matches xterm, which honours 8-bit C1 controls in UTF-8
        // mode.
        if ('\u{80}'..='\u{9f}').contains(&c) {
            // Range check above guarantees the value fits in a byte.
            #[allow(clippy::cast_possible_truncation)]
            (self.sink)(Action::Execute(c as u8));
        } else {
            (self.sink)(Action::Print(c));
        }
    }

    fn execute(&mut self, byte: u8) {
        (self.sink)(Action::Execute(byte));
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        let (params, subparams) = flatten_params(params);
        (self.sink)(Action::Csi {
            params,
            subparams,
            intermediates: intermediates.to_vec(),
            action,
            ignore,
        });
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        (self.sink)(Action::Osc(params.iter().map(|p| p.to_vec()).collect()));
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        (self.sink)(Action::Esc {
            intermediates: intermediates.to_vec(),
            byte,
            ignore,
        });
    }

    fn hook(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        // A DCS cannot nest, but be defensive: never carry stale payload into
        // a new sequence.
        self.dcs.buf.clear();
        self.dcs.ignoring = ignore;
        let params = params.iter().flatten().copied().collect();
        (self.sink)(Action::DcsStart {
            params,
            intermediates: intermediates.to_vec(),
            action,
            ignore,
        });
    }

    fn put(&mut self, byte: u8) {
        if self.dcs.ignoring {
            return;
        }
        if self.dcs.buf.capacity() == 0 {
            self.dcs.buf.reserve_exact(MAX_DCS_CHUNK);
        }
        self.dcs.buf.push(byte);
        if self.dcs.buf.len() == MAX_DCS_CHUNK {
            self.flush_dcs();
        }
    }

    fn unhook(&mut self) {
        self.flush_dcs();
        self.dcs.ignoring = false;
        (self.sink)(Action::DcsEnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn parse(bytes: &[u8]) -> Vec<Action> {
        let mut parser = Parser::new();
        let mut out = Vec::new();
        parser.advance(bytes, |a| out.push(a));
        out
    }

    fn csi(params: &[u16], action: char) -> Action {
        csi_sub(params, 0, action)
    }

    fn csi_sub(params: &[u16], subparams: u32, action: char) -> Action {
        Action::Csi {
            params: params.to_vec(),
            subparams,
            intermediates: vec![],
            action,
            ignore: false,
        }
    }

    fn st() -> Action {
        Action::Esc {
            intermediates: vec![],
            byte: b'\\',
            ignore: false,
        }
    }

    #[test]
    fn decodes_print_and_csi() {
        assert_eq!(
            parse(b"a\x1b[2;3Hb"),
            vec![Action::Print('a'), csi(&[2, 3], 'H'), Action::Print('b')]
        );
    }

    #[test]
    fn subparams_mark_colon_separated_entries() {
        assert_eq!(parse(b"\x1b[4:3m"), vec![csi_sub(&[4, 3], 0b10, 'm')]);
        assert_eq!(
            parse(b"\x1b[38:2::1:2:3m"),
            vec![csi_sub(&[38, 2, 0, 1, 2, 3], 0b11_1110, 'm')]
        );
        assert_eq!(
            parse(b"\x1b[1;4:3;5m"),
            vec![csi_sub(&[1, 4, 3, 5], 0b100, 'm')]
        );
        assert_eq!(parse(b"\x1b[m"), vec![csi_sub(&[0], 0, 'm')]);
    }

    #[test]
    fn subparams_edge_cases() {
        // Semicolons only: no bit set.
        assert_eq!(
            parse(b"\x1b[38;2;1;2;3m"),
            vec![csi(&[38, 2, 1, 2, 3], 'm')]
        );
        // A leading colon makes an empty head (0) with one sub-parameter.
        assert_eq!(parse(b"\x1b[:1m"), vec![csi_sub(&[0, 1], 0b10, 'm')]);
        // An empty sub-parameter slot is 0 and still a sub-parameter.
        assert_eq!(parse(b"\x1b[4:m"), vec![csi_sub(&[4, 0], 0b10, 'm')]);
        // A colon inside the tail of a semicolon list.
        assert_eq!(
            parse(b"\x1b[38;5:1m"),
            vec![csi_sub(&[38, 5, 1], 0b100, 'm')]
        );
        // The 32nd entry, a sub-parameter, gets bit 31.
        let mut input = b"\x1b[".to_vec();
        input.extend(std::iter::repeat_n(b"1;", 30).flatten());
        input.extend_from_slice(b"4:3m");
        let mut params = vec![1; 30];
        params.extend([4, 3]);
        assert_eq!(parse(&input), vec![csi_sub(&params, 1 << 31, 'm')]);
    }

    #[test]
    fn dcs_is_bracketed_by_start_and_end() {
        assert_eq!(
            parse(b"\x1bP1;2q#0;2;0;0;0\x1b\\x"),
            vec![
                Action::DcsStart {
                    params: vec![1, 2],
                    intermediates: vec![],
                    action: 'q',
                    ignore: false,
                },
                Action::DcsData(b"#0;2;0;0;0".to_vec()),
                Action::DcsEnd,
                st(),
                Action::Print('x'),
            ]
        );
    }

    #[test]
    fn empty_dcs_emits_no_data() {
        assert_eq!(
            parse(b"\x1bP1000p\x1b\\"),
            vec![
                Action::DcsStart {
                    params: vec![1000],
                    intermediates: vec![],
                    action: 'p',
                    ignore: false,
                },
                Action::DcsEnd,
                st(),
            ]
        );
    }

    #[test]
    fn dcs_aborted_by_can_ends_then_executes() {
        assert_eq!(
            parse(b"\x1bPqab\x18c"),
            vec![
                Action::DcsStart {
                    params: vec![0],
                    intermediates: vec![],
                    action: 'q',
                    ignore: false,
                },
                Action::DcsData(b"ab".to_vec()),
                Action::DcsEnd,
                Action::Execute(0x18),
                Action::Print('c'),
            ]
        );
    }

    #[test]
    fn dcs_terminated_by_8bit_st() {
        assert_eq!(
            parse(b"\x1bPqab\x9cc"),
            vec![
                Action::DcsStart {
                    params: vec![0],
                    intermediates: vec![],
                    action: 'q',
                    ignore: false,
                },
                Action::DcsData(b"ab".to_vec()),
                Action::DcsEnd,
                Action::Print('c'),
            ]
        );
    }

    /// Feeding a 1 MiB DCS must not grow memory with the payload: every
    /// chunk is bounded, the parser retains at most one chunk, and nothing is
    /// lost. The input is delivered in irregular slices to prove that chunk
    /// boundaries are independent of PTY read sizes.
    #[test]
    fn one_megabyte_dcs_is_streamed_in_bounded_chunks() {
        const PAYLOAD: usize = 1 << 20;
        let mut input = b"\x1bPq".to_vec();
        input.extend((0..PAYLOAD).map(|i| b'0' + u8::try_from(i % 10).unwrap()));
        input.extend_from_slice(b"\x1b\\");

        let mut parser = Parser::new();
        let mut out = Vec::new();
        let mut max_pending = 0;
        // Deterministic, awkward slice sizes: 1, 2, …, up to 7000 bytes.
        let mut offset = 0;
        let mut step = 1;
        while offset < input.len() {
            let end = (offset + step).min(input.len());
            parser.advance(&input[offset..end], |a| out.push(a));
            max_pending = max_pending.max(parser.pending_dcs_len());
            offset = end;
            step = step % 7000 + 1;
        }

        assert!(max_pending <= MAX_DCS_CHUNK);
        assert_eq!(parser.pending_dcs_len(), 0, "nothing left after ST");
        assert!(matches!(
            out.first(),
            Some(Action::DcsStart { action: 'q', .. })
        ));
        assert_eq!(out[out.len() - 2], Action::DcsEnd);
        assert_eq!(out[out.len() - 1], st());

        let chunks: Vec<&Vec<u8>> = out
            .iter()
            .filter_map(|a| match a {
                Action::DcsData(d) => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(chunks.len(), PAYLOAD.div_ceil(MAX_DCS_CHUNK));
        assert_eq!(out.len(), chunks.len() + 3, "only start, data, end, st");
        let mut total = 0;
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(
                (1..=MAX_DCS_CHUNK).contains(&chunk.len()),
                "chunk {i} is {} bytes",
                chunk.len()
            );
            // `reserve_exact` only promises a lower bound on capacity, so
            // allow slack for allocators that round up; the length bound and
            // `pending_dcs_len` are what actually prove the memory bound.
            assert!(
                chunk.capacity() <= 2 * MAX_DCS_CHUNK,
                "chunk {i} over-allocated"
            );
            total += chunk.len();
        }
        assert_eq!(total, PAYLOAD);
        let joined: Vec<u8> = chunks.into_iter().flatten().copied().collect();
        assert_eq!(joined, input[3..3 + PAYLOAD]);
    }

    #[test]
    fn oversized_csi_is_flagged_not_truncated() {
        // vte keeps 32 parameters; send more.
        let mut input = b"\x1b[".to_vec();
        input.extend(std::iter::repeat_n(b"1;", 40).flatten());
        input.push(b'm');
        let out = parse(&input);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            Action::Csi {
                action: 'm',
                ignore: true,
                ..
            }
        ));
    }

    #[test]
    fn oversized_esc_is_flagged_not_truncated() {
        // vte keeps two intermediates; send three.
        let out = parse(b"\x1b!!!A");
        assert_eq!(
            out,
            vec![Action::Esc {
                intermediates: vec![b'!', b'!'],
                byte: b'A',
                ignore: true,
            }]
        );
    }

    #[test]
    fn oversized_dcs_header_drops_payload() {
        let mut input = b"\x1bP".to_vec();
        input.extend(std::iter::repeat_n(b"1;", 40).flatten());
        input.extend_from_slice(b"qpayload\x1b\\");
        let out = parse(&input);
        assert!(matches!(
            &out[0],
            Action::DcsStart {
                action: 'q',
                ignore: true,
                ..
            }
        ));
        assert_eq!(out[1..], [Action::DcsEnd, st()]);
    }

    #[test]
    fn apc_pm_sos_are_swallowed() {
        for intro in [b"\x1b_", b"\x1b^", b"\x1bX"] {
            let mut input = b"a".to_vec();
            input.extend_from_slice(intro);
            input.extend_from_slice(b"G=anything;goes\x07\x9c\xff\x1b\\b");
            assert_eq!(
                parse(&input),
                vec![Action::Print('a'), st(), Action::Print('b')],
                "intro {intro:?}"
            );
        }
    }

    #[test]
    fn apc_of_one_megabyte_costs_nothing() {
        let mut input = b"\x1b_".to_vec();
        input.resize(1 << 20, b'x');
        input.extend_from_slice(b"\x1b\\z");
        let mut parser = Parser::new();
        let mut out = Vec::new();
        for slice in input.chunks(4099) {
            parser.advance(slice, |a| out.push(a));
        }
        assert_eq!(out, vec![st(), Action::Print('z')]);
        assert_eq!(parser.pending_dcs_len(), 0);
    }

    /// Documents the gap called out in the crate docs: `vte` buffers OSC in
    /// full, so a 1 MiB OSC 52 arrives as one action. When OSC gets a cap
    /// this test should flip to assert the bounded behaviour.
    #[test]
    fn osc_is_currently_unbounded() {
        const PAYLOAD: usize = 1 << 20;
        let mut input = b"\x1b]52;c;".to_vec();
        input.resize(input.len() + PAYLOAD, b'A');
        input.extend_from_slice(b"\x1b\\");
        let out = parse(&input);
        assert_eq!(out.len(), 2);
        match &out[0] {
            Action::Osc(params) => {
                assert_eq!(params.len(), 3);
                assert_eq!(params[2].len(), PAYLOAD);
            }
            other => panic!("expected Osc, got {other:?}"),
        }
        assert_eq!(out[1], st());
    }

    /// Documents the 7-bit limitation on [`Action::DcsData`]: high bytes
    /// are dropped by `vte` before they reach the parser.
    #[test]
    fn dcs_payload_drops_high_bytes() {
        let out = parse(b"\x1bPq\xc3\xa9ab\x1b\\");
        assert_eq!(out[1], Action::DcsData(b"ab".to_vec()));
    }

    /// A UTF-8 encoded C1 control decodes to `Execute` whether or not the
    /// two bytes arrive in the same `advance` call.
    #[test]
    fn utf8_c1_control_is_execute_regardless_of_split() {
        let whole = parse(b"a\xc2\x86b");
        let mut parser = Parser::new();
        let mut split = Vec::new();
        parser.advance(b"a\xc2", |a| split.push(a));
        parser.advance(b"\x86b", |a| split.push(a));
        let expected = vec![
            Action::Print('a'),
            Action::Execute(0x86),
            Action::Print('b'),
        ];
        assert_eq!(whole, expected);
        assert_eq!(split, expected);
    }

    #[test]
    fn apc_aborted_by_can_resumes_ground() {
        assert_eq!(
            parse(b"\x1b_abc\x18d"),
            vec![Action::Execute(0x18), Action::Print('d')]
        );
    }

    /// Byte streams that are well-formed UTF-8 in ground state.
    ///
    /// `vte` deliberately drops bytes when an *invalid* UTF-8 sequence
    /// straddles two `advance` calls (see `Parser::advance_partial_utf8`
    /// upstream), so split invariance is only promised for valid text. ASCII
    /// control bytes and escape introducers are still fully covered.
    fn utf8_stream() -> impl Strategy<Value = Vec<u8>> {
        let token = prop_oneof![
            4 => (0u8..=0x7F).prop_map(|b| vec![b]),
            1 => any::<char>().prop_map(|c| c.to_string().into_bytes()),
            1 => Just(b"\x1bP".to_vec()),
            1 => Just(b"\x1b_".to_vec()),
            1 => Just(b"\x1b\\".to_vec()),
        ];
        proptest::collection::vec(token, 0..256).prop_map(|t| t.concat())
    }

    proptest! {
        /// The action stream never depends on where the byte stream was split.
        #[test]
        fn split_invariant(bytes in utf8_stream(), cut in 0usize..1024) {
            let whole = parse(&bytes);
            let cut = cut.min(bytes.len());
            let mut parser = Parser::new();
            let mut split = Vec::new();
            parser.advance(&bytes[..cut], |a| split.push(a));
            parser.advance(&bytes[cut..], |a| split.push(a));
            prop_assert_eq!(whole, split);
        }

        /// A DCS payload is delivered intact, in order, in bounded chunks,
        /// however the bytes were sliced.
        #[test]
        fn dcs_payload_round_trips(
            payload in proptest::collection::vec(0x20u8..=0x7E, 0..10_000),
            slice in 1usize..3000,
        ) {
            let mut input = b"\x1bPq".to_vec();
            input.extend_from_slice(&payload);
            input.extend_from_slice(b"\x1b\\");
            let mut parser = Parser::new();
            let mut out = Vec::new();
            for piece in input.chunks(slice) {
                parser.advance(piece, |a| out.push(a));
                prop_assert!(parser.pending_dcs_len() <= MAX_DCS_CHUNK);
            }
            let mut joined = Vec::new();
            for a in &out {
                if let Action::DcsData(d) = a {
                    prop_assert!(!d.is_empty() && d.len() <= MAX_DCS_CHUNK);
                    joined.extend_from_slice(d);
                }
            }
            prop_assert_eq!(out.len(), payload.len().div_ceil(MAX_DCS_CHUNK) + 3);
            prop_assert_eq!(joined, payload);
        }

        /// Every action is bounded, DCS actions are well-nested, and the
        /// parser holds back at most one chunk, for any bytes at all.
        #[test]
        fn actions_are_bounded_and_dcs_well_formed(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut parser = Parser::new();
            let mut in_dcs = false;
            let mut ignoring = false;
            let mut ok = true;
            parser.advance(&bytes, |a| match a {
                Action::DcsStart { ignore, .. } => {
                    ok &= !in_dcs;
                    in_dcs = true;
                    ignoring = ignore;
                }
                Action::DcsData(d) => {
                    ok &= in_dcs && !ignoring && !d.is_empty() && d.len() <= MAX_DCS_CHUNK;
                }
                Action::DcsEnd => {
                    ok &= in_dcs;
                    in_dcs = false;
                }
                _ => {}
            });
            prop_assert!(ok);
            prop_assert!(parser.pending_dcs_len() <= MAX_DCS_CHUNK);
        }
    }
}
