//! Operating system commands (`OSC Ps ; Pt BEL` or `ST`): the
//! [`Action::Osc`] half of the dispatcher (ADR 0022).
//!
//! | Command | Effect |
//! |---|---|
//! | `OSC 0 ; title` and `OSC 2 ; title` | [`Request::SetTitle`] |
//! | `OSC 1 ; name` | ignored (the icon name is not modelled) |
//! | `OSC 52 ; Pc ; base64` | [`Request::SetClipboard`]; `?` (a read) is never answered |
//! | `OSC 7 ; url` | [`Request::SetCwd`], raw |
//! | `OSC 133 ; A/B/C/D[;options]` | a [`Mark`] on the cursor's line |
//! | `OSC 1337 ; ShellIntegrationVersion=..` | [`Request::ShellIntegrationHandshake`], raw |
//!
//! Every other command, and any malformed one, is ignored and logged at
//! trace level; nothing here ever writes a reply. The command number must be
//! exactly one of the above (`02` and `+2` are not `2`). An OSC never moves
//! the cursor or touches a pending wrap.
//!
//! The parser hands over the payload split on `;` and keeps at most
//! [`MAX_OSC_PARAMS`] pieces, silently dropping the rest; titles, working
//! directories and the handshake rejoin the pieces with `;`. A payload that
//! fills all [`MAX_OSC_PARAMS`] may have been cut: a title is applied
//! anyway, a working directory or handshake is dropped (a cut path is
//! another, valid-looking path). The parser also removes C0 controls other
//! than BEL, CAN, SUB and ESC from the payload, and CAN, SUB and ESC end
//! it: what was collected is applied (xterm would discard it).
//!
//! # Titles
//!
//! The payload (after `0;` or `2;`) may be at most [`MAX_TITLE_BYTES`]
//! bytes; a longer one is ignored whole. Invalid UTF-8 becomes U+FFFD, then
//! control characters (DEL, C1) and the bidirectional and line-separator
//! format characters U+061C, U+200E, U+200F, U+202A..U+202E,
//! U+2066..U+2069, U+2028 and U+2029 are removed, so a title cannot reorder
//! or break the tab strip's text. Spaces are kept. An empty title is
//! [`Request::SetTitle`] with `""` (the host's default title); `OSC 2`
//! without a `;` is ignored.
//!
//! # Clipboard
//!
//! `OSC 52` needs exactly three parameters: the command, the selection list
//! `Pc` and the data. `Pc` is a list of `c`, `p`, `q`, `s` and `0`..`7`:
//! any other character rejects the sequence; empty, or naming at least one
//! of `c`, `p` and `s`, writes the one system clipboard macOS has (once);
//! only `q` and cut buffers writes nothing. The data must be strict
//! standard base64 (RFC 4648 alphabet, canonical padding, zero trailing
//! bits, no whitespace) of UTF-8 text of at most [`MAX_CLIPBOARD_BYTES`]
//! once decoded; text is passed on unchanged, control characters included.
//! Empty data (which xterm takes as "clear the selection") and `?` (a read)
//! do nothing.
//!
//! # Shell integration
//!
//! `OSC 7` and the `OSC 1337` handshake are forwarded as raw bytes, at most
//! [`MAX_FORWARDED_OSC_BYTES`] and never empty: `tessera-app-core` parses
//! them with `tessera-shell-integration`, which this crate does not depend
//! on. Only an `OSC 1337` payload starting with `ShellIntegrationVersion=`
//! is forwarded; `iTerm2`'s many other keys (`File=` images, `SetUserVar`,
//! queries such as `ReportCellSize`) are ignored.
//!
//! `OSC 133` marks go straight onto the grid ([`Mark::from_osc133`], one
//! mark of each kind per line, [`Line::set_mark`](tessera_grid::Line::set_mark)):
//! on the line the cursor is on when the sequence is parsed, a pending wrap
//! left as it is. Marks received on the alternate screen are dropped, as
//! the lines they would land on vanish when the program leaves it.
//!
//! [`Action::Osc`]: tessera_vt::Action::Osc

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use tessera_grid::Mark;
use tracing::trace;

use crate::{MAX_CLIPBOARD_BYTES, MAX_FORWARDED_OSC_BYTES, MAX_TITLE_BYTES, Request, State};

/// Most pieces the parser keeps of one OSC payload split on `;` (vte's
/// `MAX_OSC_PARAMS`); a payload with this many may have been cut.
pub(crate) const MAX_OSC_PARAMS: usize = 16;

/// Longest OSC 52 data accepted before decoding: the base64 length of
/// [`MAX_CLIPBOARD_BYTES`] (`4 * ceil(cap / 3)`). Longer data cannot decode
/// to the cap or less, so it is dropped without decoding.
const MAX_CLIPBOARD_BASE64: usize = MAX_CLIPBOARD_BYTES.div_ceil(3) * 4;

/// What an OSC 1337 payload must start with to be forwarded.
const HANDSHAKE_PREFIX: &[u8] = b"ShellIntegrationVersion=";

impl State {
    /// Applies one operating system command, `params` being its payload
    /// split on `;`. See the module documentation.
    pub(crate) fn osc(&mut self, params: &[Vec<u8>]) {
        let Some((command, args)) = params.split_first() else {
            return;
        };
        let cut = params.len() >= MAX_OSC_PARAMS;
        match command.as_slice() {
            b"0" | b"2" => self.osc_title(args),
            b"52" => self.osc_clipboard(args),
            b"7" => {
                if let Some(payload) = forwarded(args, cut) {
                    self.pending.push(Request::SetCwd(payload));
                }
            }
            b"133" => self.osc_mark(args),
            b"1337" => {
                let handshake = args
                    .first()
                    .is_some_and(|a| a.starts_with(HANDSHAKE_PREFIX));
                if handshake && let Some(payload) = forwarded(args, cut) {
                    self.pending
                        .push(Request::ShellIntegrationHandshake(payload));
                } else {
                    trace!("OSC 1337 other than the handshake ignored");
                }
            }
            other => trace!(command = ?String::from_utf8_lossy(other), "OSC ignored"),
        }
    }

    /// `OSC 0` / `OSC 2`: see "Titles" in the module documentation.
    fn osc_title(&mut self, args: &[Vec<u8>]) {
        if args.is_empty() {
            trace!("OSC title without `;` ignored");
            return;
        }
        if joined_len(args) > MAX_TITLE_BYTES {
            trace!(len = joined_len(args), "over-long OSC title ignored");
            return;
        }
        let raw = args.join(&b';');
        let title = String::from_utf8_lossy(&raw)
            .chars()
            .filter(|&ch| shown_in_title(ch))
            .collect();
        self.pending.push(Request::SetTitle(title));
    }

    /// `OSC 52`: see "Clipboard" in the module documentation.
    fn osc_clipboard(&mut self, args: &[Vec<u8>]) {
        let [selection, data] = args else {
            trace!(
                params = args.len() + 1,
                "OSC 52 without exactly 3 params ignored"
            );
            return;
        };
        let known = |b: &u8| matches!(b, b'c' | b'p' | b'q' | b's' | b'0'..=b'7');
        if !selection.iter().all(known) {
            trace!("OSC 52 with an unknown selection ignored");
            return;
        }
        let clipboard = selection.is_empty() || selection.iter().any(|b| b"cps".contains(b));
        if !clipboard || data.is_empty() || data == b"?" {
            // A cut buffer or the secondary selection, nothing to write, or
            // a read: reads are never answered.
            trace!("OSC 52 that writes no clipboard ignored");
            return;
        }
        if data.len() > MAX_CLIPBOARD_BASE64 {
            trace!(len = data.len(), "OSC 52 over the clipboard cap dropped");
            return;
        }
        let Ok(decoded) = STANDARD.decode(data) else {
            trace!("OSC 52 with invalid base64 dropped");
            return;
        };
        if decoded.len() > MAX_CLIPBOARD_BYTES {
            trace!(len = decoded.len(), "OSC 52 over the clipboard cap dropped");
            return;
        }
        if let Ok(text) = String::from_utf8(decoded) {
            self.pending.push(Request::SetClipboard(text));
        } else {
            trace!("OSC 52 text that is not UTF-8 dropped");
        }
    }

    /// `OSC 133`: marks the cursor's line on the main screen.
    fn osc_mark(&mut self, args: &[Vec<u8>]) {
        let Some(mark) = Mark::from_osc133(&args.join(&b';')) else {
            trace!("OSC 133 that is no mark ignored");
            return;
        };
        if self.grid.is_alt_screen() {
            trace!(?mark, "OSC 133 on the alternate screen ignored");
            return;
        }
        let row = self.grid.cursor().row;
        if let Some(line) = self.grid.line_mut(row) {
            line.set_mark(mark);
        }
    }
}

/// The payload after the command number, rejoined, for a request that
/// carries it raw: `None` when it is empty, longer than
/// [`MAX_FORWARDED_OSC_BYTES`] or possibly `cut` by the parser.
fn forwarded(args: &[Vec<u8>], cut: bool) -> Option<Vec<u8>> {
    let len = joined_len(args);
    if cut || len == 0 || len > MAX_FORWARDED_OSC_BYTES {
        trace!(len, cut, "forwarded OSC payload dropped");
        return None;
    }
    Some(args.join(&b';'))
}

/// Length of `args` joined with `;`, without joining them.
fn joined_len(args: &[Vec<u8>]) -> usize {
    args.iter().map(Vec::len).sum::<usize>() + args.len().saturating_sub(1)
}

/// Whether `ch` may appear in a window title: not a control character and
/// not a bidirectional or line-separator format character.
fn shown_in_title(ch: char) -> bool {
    !ch.is_control()
        && !matches!(
            ch,
            '\u{061C}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{2028}'
                | '\u{2029}'
        )
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use tessera_grid::Line;

    use super::*;
    use crate::Terminal;

    /// One OSC 0/2/52/7/133/1337 sequence with a small payload, BEL or ST
    /// terminated, or a little text between them.
    fn piece() -> impl Strategy<Value = Vec<u8>> {
        let text = "[ -~\u{e9}\u{65e5};]{0,12}";
        let osc = prop_oneof![
            (prop_oneof![Just("0"), Just("2")], text).prop_map(|(n, t)| format!("{n};{t}")),
            ("[cps]{0,2}", "[ -~\u{e9}]{0,9}")
                .prop_map(|(pc, t)| format!("52;{pc};{}", STANDARD.encode(t))),
            "[a-z/;%0-9]{0,12}".prop_map(|p| format!("7;file://h/{p}")),
            prop_oneof![
                Just("A"),
                Just("B"),
                Just("C"),
                Just("D"),
                Just("D;0"),
                Just("D;130"),
                Just("A;k=s")
            ]
            .prop_map(|m| format!("133;{m}")),
            "[0-9]{1,2}".prop_map(|v| format!("1337;ShellIntegrationVersion={v};shell=zsh")),
        ];
        prop_oneof![
            3 => (osc, any::<bool>()).prop_map(|(osc, bel)| {
                let end = if bel { "\x07" } else { "\x1b\\" };
                format!("\x1b]{osc}{end}").into_bytes()
            }),
            1 => prop_oneof![Just("x"), Just("\r\n"), Just("\u{e9}"), Just("0123456789")]
                .prop_map(|t| t.as_bytes().to_vec()),
        ]
    }

    /// The requests and every line's marks after feeding `chunks`.
    fn outcome(chunks: &[&[u8]]) -> (Vec<Request>, Vec<Vec<Mark>>) {
        let mut term = Terminal::new(3, 10);
        for chunk in chunks {
            term.advance(chunk);
        }
        let grid = term.grid();
        let marks = (0..grid.scrollback_len())
            .filter_map(|i| grid.scrollback_line(i))
            .chain(grid.visible_lines())
            .map(|line: &Line| line.marks.clone())
            .collect();
        (term.take_requests(), marks)
    }

    proptest! {
        /// Splitting the input at every byte boundary (one call per byte),
        /// or at any few places, gives the requests and marks of one call.
        #[test]
        fn osc_sequences_split_anywhere_give_the_same_requests_and_marks(
            pieces in prop::collection::vec(piece(), 1..8),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 1..4),
        ) {
            let bytes = pieces.concat();
            let whole = outcome(&[&bytes]);
            let bytewise: Vec<&[u8]> = bytes.chunks(1).collect();
            prop_assert_eq!(&outcome(&bytewise), &whole);
            let mut cuts: Vec<usize> = cuts.iter().map(|c| c.index(bytes.len() + 1)).collect();
            cuts.sort_unstable();
            let mut chunks = Vec::new();
            let mut start = 0;
            for cut in cuts {
                chunks.push(&bytes[start..cut]);
                start = cut;
            }
            chunks.push(&bytes[start..]);
            prop_assert_eq!(&outcome(&chunks), &whole);
        }
    }

    /// A flood of OSCs in one `advance` call: the cursor's line keeps one
    /// mark of each kind, and OSC 52 and OSC 2 make exactly one request per
    /// sequence (bounded by the input). The timings are printed, not
    /// checked.
    #[test]
    fn a_flood_of_oscs_stays_bounded() {
        let flood = |unit: &[u8], n: usize| {
            let bytes = unit.repeat(n);
            let mut term = Terminal::new(3, 10);
            let start = std::time::Instant::now();
            term.advance(&bytes);
            let took = start.elapsed();
            (term, took)
        };
        let (mut term, small) = flood(b"\x1b]133;A\x07\x1b]133;D;1\x07", 10_000);
        let (_, large) = flood(b"\x1b]133;A\x07\x1b]133;D;1\x07", 100_000);
        eprintln!("OSC 133 x 20 000: {small:?}, x 200 000: {large:?}");
        assert_eq!(term.take_requests(), []);
        let marks: usize = term
            .grid()
            .visible_lines()
            .iter()
            .map(|l| l.marks.len())
            .sum();
        assert_eq!(marks, 2, "one mark per kind on the cursor's line");

        let unit = b"\x1b]52;c;eA==\x07";
        let (mut term, small) = flood(unit, 10_000);
        let (mut large_term, large) = flood(unit, 100_000);
        eprintln!("OSC 52 x 10 000: {small:?}, x 100 000: {large:?}");
        assert_eq!(term.take_requests().len(), 10_000);
        let requests = large_term.take_requests();
        assert_eq!(requests.len(), 100_000, "one request per sequence");
        assert!(
            requests
                .iter()
                .all(|r| *r == Request::SetClipboard("x".into()))
        );

        let (mut term, took) = flood(b"\x1b]2;title\x07", 100_000);
        eprintln!("OSC 2 x 100 000: {took:?}");
        assert_eq!(term.take_requests().len(), 100_000);
    }

    #[test]
    fn the_pre_decode_cap_is_the_base64_length_of_the_decoded_cap() {
        assert_eq!(MAX_CLIPBOARD_BASE64, 1_398_104);
        assert_eq!(
            STANDARD.encode(vec![0; MAX_CLIPBOARD_BYTES]).len(),
            MAX_CLIPBOARD_BASE64
        );
    }

    /// `cut` starts at [`MAX_OSC_PARAMS`] pieces: an OSC 7 of 15 pieces (13
    /// `;` in the URL) is forwarded, one of 16 (14 `;`, maybe cut by the
    /// parser) is dropped.
    #[test]
    fn osc_7_is_dropped_from_the_parsers_param_cap() {
        let url = |semicolons: usize| format!("file://h/a{}", ";x".repeat(semicolons));
        let mut term = Terminal::new(2, 10);
        term.advance(format!("\x1b]7;{}\x07", url(13)).as_bytes());
        assert_eq!(
            term.take_requests(),
            [Request::SetCwd(url(13).into_bytes())]
        );
        term.advance(format!("\x1b]7;{}\x07", url(14)).as_bytes());
        assert_eq!(term.take_requests(), []);
    }

    #[test]
    fn joined_len_counts_the_separators() {
        assert_eq!(joined_len(&[]), 0);
        assert_eq!(joined_len(&[b"ab".to_vec()]), 2);
        assert_eq!(joined_len(&[b"ab".to_vec(), Vec::new(), b"c".to_vec()]), 5);
    }

    #[test]
    fn title_filter_drops_controls_and_bidi_formatting_only() {
        for ch in [
            '\u{7f}', '\u{85}', '\u{9b}', '\u{61c}', '\u{200e}', '\u{202a}', '\u{202e}',
            '\u{2066}', '\u{2069}', '\u{2029}',
        ] {
            assert!(!shown_in_title(ch), "{ch:?}");
        }
        for ch in [
            ' ',
            'a',
            '\u{e9}',
            '\u{200d}',
            '\u{fffd}',
            '\u{1f980}',
            '\u{2065}',
            '\u{202f}',
        ] {
            assert!(shown_in_title(ch), "{ch:?}");
        }
    }
}
