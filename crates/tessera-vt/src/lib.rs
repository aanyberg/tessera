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
//! [`Action::Esc`] with `byte == b'\\'`. A lone `0x9C` byte (the 8-bit
//! terminator) ends a DCS on its own; a `0x9C` that is part of a UTF-8
//! character (the second byte of `C9 9C`, U+025C) is payload, like the rest
//! of the character, and does not. `CAN` (`0x18`) and `SUB` (`0x1A`) abort a
//! DCS: the parser emits [`Action::DcsEnd`] and then [`Action::Execute`]
//! for the control byte.
//!
//! # 8-bit C1 controls
//!
//! The byte stream is UTF-8, so a C1 control (`0x80..=0x9F`) is ambiguous:
//! a raw byte in that range is also a UTF-8 continuation byte. By default
//! the parser reads it the way `vte` does, which is what a terminal in
//! UTF-8 mode does (xterm ignores C1 controls there): a lone raw byte or a
//! UTF-8 encoded C1 (`C2 80..=C2 9F`) in the ground state is an
//! [`Action::Execute`] that a consumer may ignore, and in any other state
//! it is dropped. [`Parser::advance_c1`] instead honours them while the
//! consumer says 8-bit controls are on (S8C1T): every C1 code, raw or UTF-8
//! encoded but never a byte of another UTF-8 character, is parsed exactly
//! as its 7-bit form `ESC (code - 0x40)` in every state, so `0x9B` starts a
//! CSI, `0x9C` ends a string and `0x84` is `ESC D`.
//!
//! # Chunking
//!
//! [`Parser::advance`] and [`Parser::advance_c1`] keep their state between
//! calls, so input may be fed as it arrives, split anywhere: the actions do
//! not depend on where the input was split. A call that ends inside a UTF-8
//! character holds the incomplete character (at most 3 bytes) back and
//! parses it with the start of the next call, so neither `vte` nor the
//! 8-bit C1 and DCS `0x9C` handling ever sees a character in two pieces.
//! `vte` 0.15 cannot be left to complete it itself: its
//! `advance_partial_utf8` consumes up to `valid_up_to` of its 4-byte
//! completion buffer rather than the completed character's length, so
//! `"\xC2"` then `"\xA1\0\xC2\xA1"` lost the `NUL` (#234, #146). An
//! invalid sequence that only the held character shows to be invalid
//! (`E1 80` before `C2`) is parsed with it, one call later. The `chunking`
//! fuzz target in `fuzz/` checks the terminal; the `parser` target does
//! not compare split and whole input.

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
    /// byte in `0x80..=0xFF` other than a lone `0x9C` terminator; a UTF-8
    /// character is dropped whole, even one with a `0x9C` byte. Consumers must
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
///
/// See the crate documentation for the memory bounds, the string
/// terminators and the 8-bit C1 controls.
#[derive(Default)]
pub struct Parser {
    inner: vte::Parser,
    dcs: DcsState,
    /// An incomplete UTF-8 sequence held back from the end of the last
    /// call (crate docs, "Chunking").
    carry: Carry,
}

/// An incomplete UTF-8 sequence (1 to 3 bytes) held back from the end of
/// one [`Parser::advance_c1`] call and parsed with the start of the next.
#[derive(Default)]
struct Carry {
    bytes: [u8; 3],
    len: usize,
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
    /// Set between [`Action::DcsStart`] and [`Action::DcsEnd`]: `vte` is in
    /// its DCS passthrough state, where any `0x9C` byte would end the DCS.
    active: bool,
}

impl Parser {
    /// Creates a parser in the ground state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses `bytes`, invoking `sink` once per decoded action, with 8-bit
    /// C1 controls read as `vte` reads them (crate docs, "8-bit C1
    /// controls").
    ///
    /// The input may be split anywhere: an incomplete UTF-8 character at
    /// the end is held back for the next call (crate docs, "Chunking").
    pub fn advance(&mut self, bytes: &[u8], sink: impl FnMut(Action)) {
        self.advance_c1(bytes, &mut SevenBit(sink));
    }

    /// [`Parser::advance`] for a consumer that switches 8-bit C1 controls
    /// on and off: while [`Sink::c1_controls`] is true every C1 code is
    /// parsed as its 7-bit form `ESC Fe` (crate docs, "8-bit C1
    /// controls"). The parser asks before the first byte and after every
    /// ESC and CSI sequence, so a switch followed by a C1 control in the
    /// same call takes effect at once.
    ///
    /// Input with no byte in `0x80..=0x9F` is parsed in one go by `vte`
    /// alone, whatever the mode: a C1 code needs such a byte. Other input
    /// costs time linear in its length too, however many DCS strings and
    /// mode switches it holds.
    pub fn advance_c1(&mut self, bytes: &[u8], sink: &mut impl Sink) {
        let mut bytes = bytes;
        // Rare: only when the last call ended inside a character. Complete
        // it from a window of at most 4 bytes, so nothing is copied but the
        // character; the window may end inside the next one, so repeat.
        while self.carry.len != 0 && !bytes.is_empty() {
            let held = self.carry.len;
            let take = bytes.len().min(4 - held);
            let mut window = [0u8; 4];
            window[..held].copy_from_slice(&self.carry.bytes[..held]);
            window[held..held + take].copy_from_slice(&bytes[..take]);
            let len = held + take;
            let keep = incomplete_utf8_suffix(&window[..len]);
            self.carry.len = keep;
            self.carry.bytes[..keep].copy_from_slice(&window[len - keep..len]);
            self.feed(&window[..len - keep], sink);
            bytes = &bytes[take..];
        }
        if self.carry.len == 0 {
            let keep = incomplete_utf8_suffix(bytes);
            let (now, later) = bytes.split_at(bytes.len() - keep);
            self.carry.len = keep;
            self.carry.bytes[..keep].copy_from_slice(later);
            self.feed(now, sink);
        }
    }

    /// Parses `bytes`, which never end with a character the next call could
    /// complete ([`incomplete_utf8_suffix`]). They may end with an invalid
    /// sequence that only the held character shows invalid; `vte` holds that
    /// one, and since the next bytes it gets start with a lead byte, it
    /// prints U+FFFD for it without the completion bug.
    fn feed(&mut self, bytes: &[u8], sink: &mut impl Sink) {
        let c1 = sink.c1_controls();
        let mut perform = Performer {
            sink,
            dcs: &mut self.dcs,
            c1,
            stop: false,
        };
        if may_hold_c1(bytes) {
            advance_careful(&mut self.inner, &mut perform, bytes);
        } else {
            self.inner.advance(&mut perform, bytes);
        }
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

/// What [`Parser::advance_c1`] drives: the consumer, which applies every
/// action and owns the 8-bit C1 mode.
pub trait Sink {
    /// Applies one action.
    fn action(&mut self, action: Action);

    /// Whether 8-bit C1 controls are on (S8C1T). Asked before the first
    /// byte of each [`Parser::advance_c1`] call and after every
    /// [`Action::Esc`] and [`Action::Csi`], the only actions that may
    /// switch them.
    fn c1_controls(&self) -> bool;
}

/// [`Parser::advance`]'s sink: 8-bit C1 controls always off.
struct SevenBit<F>(F);

impl<F: FnMut(Action)> Sink for SevenBit<F> {
    fn action(&mut self, action: Action) {
        (self.0)(action);
    }

    fn c1_controls(&self) -> bool {
        false
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

/// Length of the incomplete UTF-8 sequence that ends `bytes`: a lead byte
/// and the continuation bytes after it that are a valid start of a
/// character but not a whole one, so at most 3. These are exactly the
/// bytes `vte` would hold back itself (`str::from_utf8` reports them as
/// incomplete, not invalid). Zero when `bytes` ends on a complete
/// character, an ASCII byte or bytes already shown invalid.
fn incomplete_utf8_suffix(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let start = bytes.len() - back;
        if bytes[start] & 0xC0 == 0x80 {
            continue; // continuation byte: keep looking for the lead
        }
        return match std::str::from_utf8(&bytes[start..]) {
            Err(err) if err.valid_up_to() == 0 && err.error_len().is_none() => back,
            _ => 0,
        };
    }
    0
}

/// Whether `bytes` has a byte in `0x80..=0x9F`, the only bytes a C1 code
/// or the DCS fix in [`advance_careful`] can start at.
///
/// Branch-free per 32-byte block so it vectorises; it runs over every chunk
/// of output, ASCII included.
fn may_hold_c1(bytes: &[u8]) -> bool {
    bytes.chunks(32).any(|block| {
        block
            .iter()
            .fold(0u8, |any, &b| any | u8::from(b & 0xE0 == 0x80))
            != 0
    })
}

/// What [`advance_careful`] does at one place in the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    /// A C1 code in 8-bit mode: parse `ESC (code - 0x40)` instead.
    Control(u8),
    /// A UTF-8 character (or invalid sequence) with a `0x9C` byte inside a
    /// DCS: skip it. `vte` drops bytes above `0x7F` in a DCS payload anyway,
    /// but would end the DCS at the `0x9C`.
    Drop,
}

/// [`Parser::advance_c1`] for input that may hold a C1 code: feeds `vte` the
/// bytes between the places where a C1 code is translated or a character is
/// dropped, stopping whenever the mode or the DCS state changes so what
/// comes next is decided again.
///
/// The places come from two cursors that only move forward, one per kind
/// ([`next_code`], [`next_9c_char`]), so the whole input is scanned at most
/// once per kind however often the state changes: linear time. Every place
/// it splits the input is a character boundary, so `vte` never sees a
/// UTF-8 character split across two of its calls.
fn advance_careful(inner: &mut vte::Parser, perform: &mut Performer<'_>, bytes: &[u8]) {
    let mut codes = Cursor::default();
    let mut drops = Cursor::default();
    let mut pos = 0;
    while pos < bytes.len() {
        // The next C1 code (8-bit mode) and the next character with a 0x9C
        // byte (inside a DCS); a C2 9C is both, and a code wins.
        let code = if perform.c1 {
            codes.next(bytes, pos, next_code)
        } else {
            None
        };
        let drop = if perform.dcs.active {
            drops.next(bytes, pos, next_9c_char)
        } else {
            None
        };
        let item = match (code, drop) {
            (Some(code), Some(drop)) if drop.0 < code.0 => Some(drop),
            (Some(code), _) => Some(code),
            (None, drop) => drop,
        };
        let end = item.map_or(bytes.len(), |(start, _, _)| start);
        perform.stop = false;
        pos += inner.advance_until_terminated(perform, &bytes[pos..end]);
        if perform.stop {
            // The mode or the DCS state changed: what comes next may be
            // read differently.
            continue;
        }
        let Some((_, item_end, item)) = item else {
            break;
        };
        if let Item::Control(code) = item {
            inner.advance(perform, &[0x1B, code - 0x40]);
        }
        pos = item_end;
    }
}

/// A place found by [`next_code`] or [`next_9c_char`]: `(start, end, what)`.
type Place = (usize, usize, Item);

/// The last result of a forward scan: the first place at or after `from`,
/// or none up to the end.
#[derive(Default)]
struct Cursor {
    scanned: Option<(usize, Option<Place>)>,
}

impl Cursor {
    /// The first place at or after `pos`, scanning with `find` only when the
    /// last result lies behind `pos`: the scans never overlap.
    fn next(
        &mut self,
        bytes: &[u8],
        pos: usize,
        find: fn(&[u8], usize) -> Option<Place>,
    ) -> Option<Place> {
        match self.scanned {
            Some((from, found))
                if from <= pos && found.is_none_or(|(start, _, _)| start >= pos) =>
            {
                found
            }
            _ => {
                let found = find(bytes, pos);
                self.scanned = Some((pos, found));
                found
            }
        }
    }
}

/// The first C1 code at or after `from` (a character boundary): a lone
/// byte in `0x80..=0x9F` or the UTF-8 encoding `C2 80..=C2 9F`.
fn next_code(bytes: &[u8], from: usize) -> Option<Place> {
    let mut at = from;
    while let Some(offset) = bytes[at..].iter().position(|&b| b & 0xE0 == 0x80) {
        let (start, end, unit) = unit_at(bytes, from, at + offset);
        if let Unit::Lone(code) | Unit::Encoded(code) = unit {
            return Some((start, end, Item::Control(code)));
        }
        at = end;
    }
    None
}

/// The first character (or invalid sequence) with a `0x9C` byte at or after
/// `from` (a character boundary), not counting a lone `0x9C`: what a DCS
/// payload drops. In 7-bit mode a UTF-8 encoded ST (`C2 9C`) is one of
/// them.
fn next_9c_char(bytes: &[u8], from: usize) -> Option<Place> {
    let mut at = from;
    while let Some(offset) = bytes[at..].iter().position(|&b| b == 0x9C) {
        let (start, end, unit) = unit_at(bytes, from, at + offset);
        if !matches!(unit, Unit::Lone(_)) {
            return Some((start, end, Item::Drop));
        }
        at = end;
    }
    None
}

/// What the byte at an index in `0x80..=0x9F` belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    /// A C1 code as a lone byte: not part of any UTF-8 sequence.
    Lone(u8),
    /// A C1 code in its UTF-8 encoding `C2 80..=C2 9F`.
    Encoded(u8),
    /// A byte of another UTF-8 character, or of an invalid sequence longer
    /// than one byte (which `vte` prints as one U+FFFD).
    Other,
}

/// The unit holding `bytes[at]` (a byte in `0x80..=0x9F`), as `(start,
/// end, unit)`, splitting the input as [`str::from_utf8`] and `vte` do: a
/// lead byte at most three bytes back whose sequence covers `at` claims it.
/// No sequence starts before `floor`.
fn unit_at(bytes: &[u8], floor: usize, at: usize) -> (usize, usize, Unit) {
    let lone = (at, at + 1, Unit::Lone(bytes[at]));
    let mut start = at;
    while start > floor && at - start < 3 {
        start -= 1;
        let byte = bytes[start];
        if byte & 0xC0 == 0x80 {
            continue; // a continuation byte: keep looking for the lead
        }
        if byte < 0xC0 {
            return lone; // ASCII: no sequence reaches `at`
        }
        let window = &bytes[start..bytes.len().min(start + 4)];
        let len = match std::str::from_utf8(window) {
            Ok(text) => text.chars().next().map_or(1, char::len_utf8),
            Err(err) if err.valid_up_to() > 0 => {
                // The first character is valid; its length is its lead's.
                std::str::from_utf8(&window[..err.valid_up_to()])
                    .ok()
                    .and_then(|text| text.chars().next())
                    .map_or(1, char::len_utf8)
            }
            Err(err) => err.error_len().unwrap_or(window.len()),
        };
        if at >= start + len {
            return lone;
        }
        let unit = if len == 2 && byte == 0xC2 {
            Unit::Encoded(bytes[at])
        } else {
            Unit::Other
        };
        return (start, start + len, unit);
    }
    lone
}

struct Performer<'a> {
    sink: &'a mut dyn Sink,
    dcs: &'a mut DcsState,
    /// Whether 8-bit C1 controls are on, as the sink last said.
    c1: bool,
    /// Set when `c1` or the DCS state changed; [`advance_careful`] stops
    /// `vte` there (`vte::Perform::terminated`).
    stop: bool,
}

impl Performer<'_> {
    /// Hands `action` to the sink.
    fn emit(&mut self, action: Action) {
        self.sink.action(action);
    }

    /// Asks the sink for the C1 mode after a sequence that may switch it.
    fn check_c1(&mut self) {
        let c1 = self.sink.c1_controls();
        if c1 != self.c1 {
            self.c1 = c1;
            self.stop = true;
        }
    }

    fn flush_dcs(&mut self) {
        if self.dcs.buf.is_empty() {
            return;
        }
        // Hand the buffer to the consumer and leave an empty, unallocated one
        // behind; `put` reserves the next chunk on demand, so a parser that
        // has finished a DCS holds no payload memory at all.
        let chunk = std::mem::take(&mut self.dcs.buf);
        self.emit(Action::DcsData(chunk));
    }
}

impl vte::Perform for Performer<'_> {
    fn print(&mut self, c: char) {
        // `vte` dispatches a UTF-8 encoded C1 control (U+0080..=U+009F) as
        // `execute` when both bytes arrive in one `advance` call but as
        // `print` when the sequence straddles two calls. Normalise to
        // `Execute` so the action stream never depends on chunk boundaries.
        // (xterm in UTF-8 mode ignores U+0080..=U+009F altogether; the
        // consumer does too unless 8-bit controls are on, and then
        // `advance_careful` has turned them into `ESC Fe` before `vte`.)
        if ('\u{80}'..='\u{9f}').contains(&c) {
            // Range check above guarantees the value fits in a byte.
            #[allow(clippy::cast_possible_truncation)]
            self.emit(Action::Execute(c as u8));
        } else {
            self.emit(Action::Print(c));
        }
    }

    fn execute(&mut self, byte: u8) {
        self.emit(Action::Execute(byte));
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        let (params, subparams) = flatten_params(params);
        self.emit(Action::Csi {
            params,
            subparams,
            intermediates: intermediates.to_vec(),
            action,
            ignore,
        });
        self.check_c1();
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.emit(Action::Osc(params.iter().map(|p| p.to_vec()).collect()));
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.emit(Action::Esc {
            intermediates: intermediates.to_vec(),
            byte,
            ignore,
        });
        self.check_c1();
    }

    fn hook(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        // A DCS cannot nest, but be defensive: never carry stale payload into
        // a new sequence.
        self.dcs.buf.clear();
        self.dcs.ignoring = ignore;
        self.dcs.active = true;
        self.stop = true;
        let params = params.iter().flatten().copied().collect();
        self.emit(Action::DcsStart {
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
        self.dcs.active = false;
        self.stop = true;
        self.emit(Action::DcsEnd);
    }

    #[inline]
    fn terminated(&self) -> bool {
        self.stop
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

    /// A consumer for the tests: records the actions; 8-bit controls on
    /// from `on`, switched at `ESC SP G` / `ESC SP F` unless `fixed`.
    struct Recorder {
        out: Vec<Action>,
        on: bool,
        fixed: bool,
    }

    impl Sink for Recorder {
        fn action(&mut self, action: Action) {
            if let Action::Esc {
                intermediates,
                byte,
                ..
            } = &action
                && intermediates == b" "
                && !self.fixed
            {
                match byte {
                    b'G' => self.on = true,
                    b'F' => self.on = false,
                    _ => {}
                }
            }
            self.out.push(action);
        }

        fn c1_controls(&self) -> bool {
            self.on
        }
    }

    /// Parses `bytes` with [`Parser::advance_c1`], 8-bit controls on or off
    /// from the start, switching them on at `ESC SP G` and off at `ESC SP F`
    /// as a consumer would.
    fn parse_c1(bytes: &[u8], c1: bool) -> Vec<Action> {
        let mut sink = Recorder {
            out: Vec::new(),
            on: c1,
            fixed: false,
        };
        Parser::new().advance_c1(bytes, &mut sink);
        sink.out
    }

    /// Parses `bytes` with 8-bit controls fixed at `c1`, whatever the
    /// input asks for.
    fn parse_fixed(bytes: &[u8], c1: bool) -> Vec<Action> {
        let mut sink = Recorder {
            out: Vec::new(),
            on: c1,
            fixed: true,
        };
        Parser::new().advance_c1(bytes, &mut sink);
        sink.out
    }

    fn dcs_start(action: char) -> Action {
        Action::DcsStart {
            params: vec![0],
            intermediates: vec![],
            action,
            ignore: false,
        }
    }

    /// The reference for 8-bit mode: every C1 code (a lone byte in
    /// `0x80..=0x9F` or its UTF-8 encoding) replaced by `ESC (code - 0x40)`
    /// over the whole input, split exactly as `str::from_utf8` splits it.
    fn translate_c1(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for chunk in bytes.utf8_chunks() {
            for ch in chunk.valid().chars() {
                if ('\u{80}'..='\u{9f}').contains(&ch) {
                    out.extend([0x1B, u8::try_from(u32::from(ch) - 0x40).unwrap()]);
                } else {
                    out.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
            }
            match chunk.invalid() {
                [code @ 0x80..=0x9F] => out.extend([0x1B, code - 0x40]),
                invalid => out.extend_from_slice(invalid),
            }
        }
        out
    }

    #[test]
    fn eight_bit_c1_codes_are_their_7bit_forms() {
        // Raw and UTF-8 encoded CSI, an 8-bit OSC with 8-bit ST, IND.
        assert_eq!(
            parse_c1(b"\x9b2;3H\xc2\x9b4H\x9d0;t\x9c\x84", true),
            vec![
                csi(&[2, 3], 'H'),
                csi(&[4], 'H'),
                Action::Osc(vec![b"0".to_vec(), b"t".to_vec()]),
                st(),
                Action::Esc {
                    intermediates: vec![],
                    byte: b'D',
                    ignore: false,
                },
            ]
        );
        // The same input in 7-bit mode is what vte has always made of it.
        assert_eq!(
            parse_c1(b"\x9b2;3H", false),
            vec![
                Action::Execute(0x9b),
                Action::Print('2'),
                Action::Print(';'),
                Action::Print('3'),
                Action::Print('H'),
            ]
        );
    }

    #[test]
    fn bytes_of_utf8_characters_are_never_c1_codes() {
        // Ü (c3 9c), ⅛ (e2 85 9b), 😛 (f0 9f 98 9b), then a lone 9c (ST).
        let out = parse_c1(b"\xc3\x9c\xe2\x85\x9b\xf0\x9f\x98\x9b\x9cx", true);
        assert_eq!(
            out,
            vec![
                Action::Print('Ü'),
                Action::Print('⅛'),
                Action::Print('😛'),
                st(),
                Action::Print('x'),
            ]
        );
        // An invalid two-byte sequence (e2 9b, then ASCII) is one U+FFFD.
        assert_eq!(
            parse_c1(b"\xe2\x9bx", true),
            vec![Action::Print('\u{fffd}'), Action::Print('x')]
        );
    }

    #[test]
    fn a_switch_applies_to_the_rest_of_the_same_call() {
        let ind = Action::Esc {
            intermediates: vec![],
            byte: b'D',
            ignore: false,
        };
        let s8c1t = Action::Esc {
            intermediates: vec![b' '],
            byte: b'G',
            ignore: false,
        };
        let s7c1t = Action::Esc {
            intermediates: vec![b' '],
            byte: b'F',
            ignore: false,
        };
        assert_eq!(
            parse_c1(b"\x84\x1b G\x84\x1b F\x84", false),
            vec![
                Action::Execute(0x84),
                s8c1t,
                ind,
                s7c1t,
                Action::Execute(0x84)
            ]
        );
    }

    /// A 0x9C byte of a UTF-8 character does not end a DCS (U+025C is c9
    /// 9c); a lone 0x9C still does, and in 8-bit mode the UTF-8 encoded ST
    /// does too.
    #[test]
    fn dcs_ends_only_at_a_lone_or_8bit_st() {
        let ended = [
            dcs_start('q'),
            Action::DcsData(b"ab".to_vec()),
            Action::DcsEnd,
        ];
        // 7-bit mode: `vte`'s own 8-bit terminator; 8-bit mode: `ESC \`.
        assert_eq!(
            parse_c1(b"\x1bPqa\xc9\x9cb\x9cc", false),
            [&ended[..], &[Action::Print('c')]].concat()
        );
        assert_eq!(
            parse_c1(b"\x1bPqa\xc9\x9cb\x9cc", true),
            [&ended[..], &[st(), Action::Print('c')]].concat()
        );
        // In 7-bit mode the UTF-8 encoded ST is payload (dropped like any
        // other high byte); in 8-bit mode it is ST.
        assert_eq!(
            parse_c1(b"\x1bPqa\xc2\x9cb\x1b\\", false),
            vec![
                dcs_start('q'),
                Action::DcsData(b"ab".to_vec()),
                Action::DcsEnd,
                st(),
            ]
        );
        assert_eq!(
            parse_c1(b"\x1bPqa\xc2\x9cb", true),
            vec![
                dcs_start('q'),
                Action::DcsData(b"a".to_vec()),
                Action::DcsEnd,
                st(),
                Action::Print('b'),
            ]
        );
        // `Parser::advance` has the fix too.
        assert_eq!(
            parse(b"\x1bPq\xe4\xb8\x9cz\x1b\\")[1],
            Action::DcsData(b"z".to_vec())
        );
    }

    #[test]
    fn apc_aborted_by_can_resumes_ground() {
        assert_eq!(
            parse(b"\x1b_abc\x18d"),
            vec![Action::Execute(0x18), Action::Print('d')]
        );
    }

    /// Parses `bytes` with one [`Parser::advance_c1`] call per piece
    /// between the `cuts` (clamped and sorted), with 8-bit controls
    /// switching as in [`parse_c1`].
    fn parse_split(bytes: &[u8], cuts: &[usize], c1: bool) -> Vec<Action> {
        let mut cuts: Vec<usize> = cuts.iter().map(|&c| c.min(bytes.len())).collect();
        cuts.sort_unstable();
        let mut sink = Recorder {
            out: Vec::new(),
            on: c1,
            fixed: false,
        };
        let mut parser = Parser::new();
        let mut from = 0;
        for cut in cuts.into_iter().chain([bytes.len()]) {
            parser.advance_c1(&bytes[from..cut], &mut sink);
            from = cut;
        }
        sink.out
    }

    /// #234 (#146): a character completed across two calls keeps the bytes
    /// after it. `vte` 0.15 alone consumed every valid byte in its 4-byte
    /// completion window but printed only the completed character, so the
    /// `NUL` after `¡` (and the `a` after `é`) was lost.
    #[test]
    fn a_character_completed_across_calls_keeps_the_bytes_after_it() {
        let bytes = [0xC2, 0xA1, 0x00, 0xC2, 0xA1];
        let want = vec![Action::Print('¡'), Action::Execute(0), Action::Print('¡')];
        assert_eq!(parse(&bytes), want);
        for cut in 0..=bytes.len() {
            assert_eq!(parse_split(&bytes, &[cut], false), want, "cut {cut}");
        }
        // The second seed of #234 (cut 60 splits the last but one `¡`).
        let bytes = [
            27, 80, 27, 80, 27, 80, 0, 0, 0, 0, 0, 27, 80, 0, 0, 0, 0, 240, 144, 128, 128, 27, 80,
            27, 80, 0, 27, 80, 224, 160, 128, 0, 27, 80, 27, 80, 27, 80, 27, 80, 194, 128, 0, 0, 0,
            27, 80, 0, 0, 0, 0, 27, 92, 194, 161, 0, 0, 0, 0, 194, 161, 0, 194, 161,
        ];
        assert_eq!(parse_split(&bytes, &[60], false), parse(&bytes));
        let bytes = "éa中".as_bytes();
        let want = vec![Action::Print('é'), Action::Print('a'), Action::Print('中')];
        for cut in 0..=bytes.len() {
            assert_eq!(parse_split(bytes, &[cut], false), want, "cut {cut}");
        }
        // A character completed one byte per call.
        let cuts: Vec<usize> = (1..bytes.len()).collect();
        assert_eq!(parse_split(bytes, &cuts, false), want);
    }

    /// #234: a `0x9C` byte of a UTF-8 character inside a DCS is payload
    /// even when the character is split across two calls; before the fix
    /// the second call saw a lone `0x9C` and ended the DCS there.
    #[test]
    fn a_split_character_with_a_9c_byte_does_not_end_a_dcs() {
        let bytes = b"\x1bPqa\xf2\xaf\xa4\x9cb\x1b\\";
        let want = vec![
            dcs_start('q'),
            Action::DcsData(b"ab".to_vec()),
            Action::DcsEnd,
            st(),
        ];
        for c1 in [false, true] {
            assert_eq!(parse_c1(bytes, c1), want);
            for cut in 0..=bytes.len() {
                assert_eq!(parse_split(bytes, &[cut], c1), want, "cut {cut} c1 {c1}");
            }
        }
    }

    /// An incomplete character at the end of the input is held back, not
    /// parsed, until the next call completes it or shows it invalid; the
    /// parser holds at most 3 bytes for it.
    #[test]
    fn an_incomplete_character_waits_for_the_next_call() {
        let mut parser = Parser::new();
        let mut out = Vec::new();
        parser.advance(b"a\xf0\x9f", |a| out.push(a));
        assert_eq!(out, vec![Action::Print('a')]);
        parser.advance(b"\x98", |a| out.push(a));
        assert_eq!(out, vec![Action::Print('a')]);
        parser.advance(b"\x80b", |a| out.push(a));
        assert_eq!(
            out,
            vec![Action::Print('a'), Action::Print('😀'), Action::Print('b')]
        );
        // Shown invalid by the next byte: one replacement character, then
        // the byte, as in one call.
        let mut out = Vec::new();
        parser.advance(b"\xe4\xb8", |a| out.push(a));
        parser.advance(b"z", |a| out.push(a));
        assert_eq!(out, vec![Action::Print('\u{FFFD}'), Action::Print('z')]);
        assert_eq!(out, parse(b"\xe4\xb8z"));
        // A sequence already invalid is parsed at once, not held.
        let mut out = Vec::new();
        parser.advance(b"\xf4\x91", |a| out.push(a));
        assert_eq!(out, vec![Action::Print('\u{FFFD}'), Action::Execute(0x91)]);
        // An invalid sequence that only the held character shows invalid
        // (`E1 80` then a lead byte) waits with it.
        let mut out = Vec::new();
        parser.advance(b"\xe1\x80\xc2", |a| out.push(a));
        assert_eq!(out, vec![]);
        parser.advance(b"\xa1", |a| out.push(a));
        assert_eq!(out, vec![Action::Print('\u{FFFD}'), Action::Print('¡')]);
        assert_eq!(out, parse(b"\xe1\x80\xc2\xa1"));
    }

    /// Byte streams that are well-formed UTF-8 in ground state, rich in
    /// multi-byte characters next to ASCII and controls: the pattern `vte`
    /// 0.15 alone lost bytes on when a character was split (#234, #146).
    fn utf8_stream() -> impl Strategy<Value = Vec<u8>> {
        let token = prop_oneof![
            4 => (0u8..=0x7F).prop_map(|b| vec![b]),
            3 => any::<char>().prop_map(|c| c.to_string().into_bytes()),
            2 => (0x80u32..0x800).prop_map(|c| char::from_u32(c).unwrap_or('x').to_string().into_bytes()),
            1 => Just(b"\x1bP".to_vec()),
            1 => Just(b"\x1b_".to_vec()),
            1 => Just(b"\x1b\\".to_vec()),
        ];
        proptest::collection::vec(token, 0..256).prop_map(|t| t.concat())
    }

    /// Hostile byte streams for the split properties: any byte, valid and
    /// truncated UTF-8, C1 codes raw and encoded, characters with a `0x9C`
    /// byte, DCS, APC and OSC introducers, terminators and the S7C1T/S8C1T
    /// switches.
    fn hostile_stream() -> impl Strategy<Value = Vec<u8>> {
        let token = prop_oneof![
            3 => any::<u8>().prop_map(|b| vec![b]),
            2 => (0x20u8..=0x7E).prop_map(|b| vec![b]),
            2 => any::<char>().prop_map(|c| c.to_string().into_bytes()),
            1 => (0x80u8..=0x9F).prop_map(|b| vec![0xC2, b]),
            1 => (0xC2u8..=0xF4, 0x80u8..=0xBF).prop_map(|(lead, cont)| vec![lead, cont]),
            1 => prop_oneof![Just(&b"\xc9\x9c"[..]), Just(&b"\xe4\xb8\x9c"[..]),
                Just(&b"\xf2\xaf\xa4\x9c"[..])].prop_map(<[u8]>::to_vec),
            1 => prop_oneof![Just(&b"\x1bPq"[..]), Just(&b"\x90q"[..]), Just(&b"\x1b_"[..]),
                Just(&b"\x1b]0;"[..]), Just(&b"\x1b\\"[..]), Just(&b"\x9c"[..]), Just(&b"\x07"[..]),
                Just(&b"\x1b[1;2"[..]), Just(&b"\x1b G"[..]), Just(&b"\x1b F"[..]), Just(&b"\x18"[..])]
                .prop_map(<[u8]>::to_vec),
        ];
        proptest::collection::vec(token, 0..128).prop_map(|t| t.concat())
    }

    /// Byte streams full of C1 codes, raw and UTF-8 encoded, UTF-8
    /// characters whose continuation bytes are in `0x80..=0x9F`, invalid
    /// sequences and escape sequences, but no byte that could open a DCS
    /// (`P`, `0x90`): see `c1_paths_agree_with_their_references`.
    fn c1_stream() -> impl Strategy<Value = Vec<u8>> {
        let c1 = 0x80u8..=0x9F;
        let token = prop_oneof![
            4 => prop_oneof![Just(b'a'), Just(b'1'), Just(b';'), Just(b'H'), Just(b'm'), Just(b'['),
                Just(b']'), Just(b'\\'), Just(b'_'), Just(b' '), Just(0x07), Just(0x18), Just(b'\n')]
                .prop_map(|b| vec![b]),
            2 => c1.clone().prop_map(|b| vec![b]),
            2 => c1.prop_map(|b| vec![0xC2, b]),
            1 => Just(vec![0x1B]),
            1 => any::<char>().prop_map(|c| c.to_string().into_bytes()),
            1 => (0x80u32..0x800).prop_map(|c| char::from_u32(c).unwrap_or('x').to_string().into_bytes()),
            1 => (0xC2u8..=0xF4, 0x80u8..=0x9F).prop_map(|(lead, cont)| vec![lead, cont]),
            1 => (0xA0u8..=0xFF).prop_map(|b| vec![b]),
        ];
        proptest::collection::vec(token, 0..64)
            .prop_map(|t| t.concat())
            // `ESC` and a `P` (even with ignored bytes between them) open a
            // DCS, and so may any 0x90 byte, which an invalid sequence can
            // leave lone.
            .prop_filter("no DCS", |b| !b.contains(&0x90) && !b.contains(&b'P'))
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

        /// Any bytes at all, split anywhere into up to five calls, give the
        /// actions of one call, with 8-bit controls on or off and switched
        /// by the input.
        #[test]
        fn split_invariant_for_any_bytes(
            bytes in hostile_stream(),
            cuts in proptest::collection::vec(0usize..512, 0..5),
            c1 in any::<bool>(),
        ) {
            prop_assert_eq!(parse_c1(&bytes, c1), parse_split(&bytes, &cuts, c1));
        }

        /// With 8-bit controls on, the careful path gives exactly what `vte` gives
        /// for the input with every C1 code replaced by its 7-bit form over
        /// the whole input ([`translate_c1`]); in 7-bit mode exactly what
        /// `vte` gives for the input itself. No DCS in the input: a DCS is
        /// where the two differ on purpose (the 0x9C fix). The input ends in
        /// ASCII, so nothing is held back for a next call (crate docs,
        /// "Chunking").
        #[test]
        fn c1_paths_agree_with_their_references(bytes in c1_stream(), c1 in any::<bool>()) {
            let mut bytes = bytes;
            bytes.push(b'a');
            let reference = if c1 { translate_c1(&bytes) } else { bytes.clone() };
            let mut vte_parser = vte::Parser::new();
            let mut want = Vec::new();
            let mut dcs = DcsState::default();
            let mut sink = SevenBit(|a| want.push(a));
            let mut perform = Performer { sink: &mut sink, dcs: &mut dcs, c1: false, stop: false };
            vte_parser.advance(&mut perform, &reference);
            prop_assert_eq!(parse_fixed(&bytes, c1), want);
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
