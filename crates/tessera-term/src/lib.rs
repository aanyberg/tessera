//! The emulator proper.
//!
//! Combines a [`tessera_vt::Parser`] with a [`tessera_grid::Grid`] and the
//! terminal mode state (cursor keys, bracketed paste, alternate screen, …).
//! Bytes in, grid mutations and side-effect requests (bell, title change,
//! clipboard write) out. The [`keyboard`] module goes the other way: it
//! encodes key events into the bytes written to the PTY, and
//! [`encode_mouse`] does the same for mouse reports in the tracking mode and
//! encoding the program chose ([`Modes::mouse`], [`Modes::mouse_encoding`];
//! ADR 0027).
//!
//! ## OSC
//!
//! Operating system commands (ADR 0022): OSC 0 and 2 set the title, OSC 52
//! writes the clipboard (never read), OSC 133 puts prompt marks on the
//! grid's lines, and OSC 7 (working directory) and the OSC 1337 shell
//! integration handshake are forwarded raw as [`Request`]s for the host to
//! parse with `tessera-shell-integration`, which this crate does not depend
//! on. See the `osc` module for the caps and the rules.
//!
//! ## Device reports
//!
//! Queries a program sends to learn what the terminal is and what state it
//! is in are answered with [`Request::Reply`] (ADR 0024): DA1/DA2/DA3 and
//! DECID, XTVERSION, DSR and CPR, the DEC DSRs, DECRQM (one reply per
//! query, 0 for an unknown mode), DECRQSS (`DCS $ q`, for SGR, DECSTBM,
//! DECSCUSR and DECSCL), the kitty keyboard flags query `CSI ? u` and the
//! XTWINOPS size reports (`CSI 14/16/18/19 t`; the pixel sizes once
//! [`Terminal::set_cell_pixels`] knows the cell). Replies are 7-bit unless
//! 8-bit mode is on (see "8-bit C1 controls" below). Tessera identifies as itself, not as xterm ([`DA2_PV`]). Reports that
//! would echo text a program supplied (the title reports `CSI 20/21 t`,
//! XTGETTCAP) are never answered. `CSI 22 t` and `CSI 23 t` push and pop
//! the window title, at most [`TITLE_STACK_DEPTH`] deep; a pop sends
//! [`Request::SetTitle`] with the restored title.
//!
//! ## DECSTR
//!
//! `CSI ! p` (soft terminal reset, sent by the terminfo `rs2` and `is2`
//! strings, so by every `reset` and `tput init`) follows xterm's
//! `ReallyReset` soft path. It resets:
//!
//! - the pen to [`Style::DEFAULT`](tessera_grid::Style::DEFAULT): colours,
//!   underline colour and shape and every attribute flag. This follows
//!   DEC, kitty and `WezTerm`; xterm keeps faint, strikeout and double
//!   underline;
//! - insert mode (IRM) off;
//! - auto-wrap (DECAWM) on, xterm's default; DEC's table 5-9 says off;
//! - the scroll region (DECSTBM) to the full screen, and left/right
//!   margin mode (DECLRMM) off with the left and right margins (DECSLRM)
//!   at the full width;
//! - reverse wraparound (DECSET 45 and 1045) off;
//! - the saved cursor (DECSC, and SCOSC, which shares its slot) to the
//!   home position with the default pen and no pending wrap (xterm would
//!   keep the live pending wrap in the copy). Only the active screen's
//!   saved cursor is reset, as xterm resets only `sc[whichBuf]`.
//!
//! It keeps the screen contents, the scrollback, the cursor position, a
//! pending wrap and the tab stops; RIS (`ESC c`) is the reset that clears
//! them. Both keep the title stack (ADR 0024). Its parameters are ignored,
//! as in xterm (`CSI 5 ! p` resets too).
//! REP has nothing to repeat afterwards, as after any sequence.
//!
//! - the modes on DECSTR's reset list (ADR 0020): origin mode (DECOM)
//!   off before the saved cursor is reset, so a DECRC homes to the screen's
//!   corner; the cursor visible (DECTCEM) and not blinking (DECSET 12);
//!   synchronized output (2026) off; cursor keys (DECCKM) and keypad
//!   (DECKPAM) to normal, `modifyOtherKeys` to 0 and both kitty keyboard
//!   flag stacks emptied;
//! - the cursor style the program set (DECSCUSR, `CSI Ps SP q`, and DECSET
//!   12): [`Terminal::cursor_style`] is the configured default again, as
//!   after RIS (xterm's `ReallyReset`, ADR 0020).
//!
//! KAM is not modelled (SM 2 is ignored), so it has nothing to reset.
//! Still waiting on state that does not exist yet (`// #112, #95, #10` in
//! `soft_reset`): the character sets G0-G3/GL/GR (SCS, #112), DECSCA
//! (#95) and the ANSI palette (colours set with OSC 4) back to the theme
//! (#10).
//!
//! ## Left and right margins
//!
//! From conformance level 4, DECSET 69 (DECLRMM) turns on left/right
//! margin mode, in which `CSI Pl ; Pr s` sets the left and right margins
//! (DECSLRM) instead of saving the cursor (SCOSC). Printing then wraps at
//! the right margin to the left one, and scrolling, IL/DL, ICH/DCH and
//! the VT420 column operations DECIC, DECDC, DECBI and DECFI move only the
//! cells between the margins; only a scroll of the full width and the full
//! screen feeds the scrollback. DECSET 45 and 1045 (reverse wraparound)
//! let BS and CUB wrap back to the line above. DA1 lists 21 from level 4,
//! which is what makes tmux use the margins for side-by-side panes. ADR
//! 0028 has the rules and the deviations from xterm.
//!
//! ## 8-bit C1 controls
//!
//! S8C1T (`ESC SP G`) turns 8-bit mode on and S7C1T (`ESC SP F`) turns it
//! off, both from conformance level 2 (ignored at level 1); DECSCL's `Pc`
//! sets it too, RIS clears it and DECSTR keeps it
//! ([`Terminal::c1_8bit_replies`]). In 8-bit mode every reply starts with
//! the single byte `0x9B`, `0x90` or `0x9D` instead of `ESC [`, `ESC P` or
//! `ESC ]` and ends a string with `0x9C` instead of `ESC \` (a
//! BEL-terminated reply keeps its BEL), and a C1 control in the output,
//! raw or UTF-8 encoded, acts as its `ESC Fe` form: `0x84` is IND, `0x9B`
//! starts a CSI, `0x9C` ends a string. A default 7-bit session ignores C1
//! controls, as xterm does in UTF-8 mode. [`to_8bit_controls`] is the same
//! rewrite for the keys, mouse reports, focus reports and paste brackets
//! the host sends. ADR 0024 has the rules.
//!
//! DECSCL (`CSI 61..65 ; Pc " p`, select conformance level) runs the same
//! soft reset and then records the level ([`Terminal::conformance_level`])
//! and the C1 reply mode its `Pc` selects ([`Terminal::c1_8bit_replies`]);
//! any other first parameter does nothing, as in xterm. See the `csi`
//! module for the details.

#![forbid(unsafe_code)]

mod control;
mod csi;
mod dcs;
mod esc;
pub mod keyboard;
mod modes;
mod mouse;
mod osc;
mod reports;
mod sgr;

pub use keyboard::{
    Key, KeyEvent, KeyEventKind, KeyboardModes, KittyFlags, KittyFlagsMode, Modifiers,
    ModifyOtherKeys, encode_key, encode_key_into,
};
pub use modes::{KITTY_STACK_DEPTH, Modes, MouseEncoding, MouseTracking};
pub use mouse::{MouseReport, MouseReportButton, MouseReportKind, encode_mouse};
pub use reports::{DA2_PV, TITLE_STACK_DEPTH};

use dcs::Dcs;
use reports::TitleStack;

use modes::KittyStacks;

use tessera_grid::{Cell, Grid, Point};
use tessera_vt::{Action, Parser, Sink};
use tracing::trace;

/// Rewrites `sequence`, one control sequence the terminal sends to the
/// host, into its 8-bit form for a session in 8-bit mode
/// ([`Terminal::c1_8bit_replies`], S8C1T): a leading `ESC [`, `ESC O`,
/// `ESC P` or `ESC ]` becomes the single C1 byte `0x9B`, `0x8F`, `0x90` or
/// `0x9D`, and after `ESC P` or `ESC ]` a trailing `ESC \` becomes `0x9C`.
/// Nothing else changes: not the bytes in between (an X10 mouse report's
/// payload), not a BEL terminator, and not a two-byte sequence, which is an
/// Alt-prefixed key (`ESC [` for Alt+`[`), not an introducer.
///
/// The bytes are raw C1 bytes, not their UTF-8 encoding: a receiver that
/// decodes UTF-8 sees invalid UTF-8, which is what S8C1T asks for. The
/// terminal's own replies already go through it; a host applies it to
/// each key encoding ([`encode_key`]), mouse report ([`encode_mouse`]),
/// focus report and bracketed-paste marker separately, before repeating or
/// joining them.
pub fn to_8bit_controls(sequence: &mut Vec<u8>) {
    let introducer = match sequence.as_slice() {
        [0x1B, byte @ (b'[' | b'O' | b'P' | b']'), _, ..] => *byte,
        _ => return,
    };
    sequence.remove(0);
    sequence[0] = introducer + 0x40;
    if matches!(introducer, b'P' | b']') && sequence.ends_with(b"\x1b\\") {
        sequence.truncate(sequence.len() - 2);
        sequence.push(0x9C);
    }
}

/// Longest window title an OSC 0 or OSC 2 may set, in bytes of payload as
/// received (before invalid UTF-8 is replaced and control characters are
/// removed). A longer title is ignored whole, not truncated, as xterm does.
pub const MAX_TITLE_BYTES: usize = 1024;

/// Most text one OSC 52 may write to the clipboard, in decoded bytes
/// (1 MiB). A larger write is ignored whole.
pub const MAX_CLIPBOARD_BYTES: usize = 1 << 20;

/// Longest OSC 7 or OSC 1337 payload the terminal forwards as a
/// [`Request`], in bytes: `tessera_shell_integration::osc7::MAX_OSC7_LEN`,
/// so a flood cannot queue megabytes the parser above would refuse anyway.
pub const MAX_FORWARDED_OSC_BYTES: usize = 4096;

/// Side effects the host must perform on behalf of the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// Ring the bell.
    Bell,
    /// Set the window or tab title (OSC 0 and OSC 2). Empty means "back to
    /// the default title". Control and bidirectional formatting characters
    /// are already removed (ADR 0022).
    SetTitle(String),
    /// Write this text to the system clipboard (OSC 52 with a valid,
    /// strictly decoded base64 payload of at most [`MAX_CLIPBOARD_BYTES`]).
    /// The text is passed on unchanged: it may contain NUL, ESC and other
    /// control characters. Clipboard reads (`?`) are never answered.
    SetClipboard(String),
    /// The shell reported its working directory (OSC 7): the raw payload
    /// after `7;`, at most [`MAX_FORWARDED_OSC_BYTES`] bytes and never
    /// empty. The terminal does not decode it; `tessera-app-core` parses it
    /// with `tessera_shell_integration::parse_osc7`.
    SetCwd(Vec<u8>),
    /// A shell integration script announced itself (OSC 1337 whose payload
    /// starts with `ShellIntegrationVersion=`): the raw payload after
    /// `1337;`, at most [`MAX_FORWARDED_OSC_BYTES`] bytes. Parsed by
    /// `tessera_shell_integration::parse_osc1337` above this crate. Every
    /// other OSC 1337 key is ignored.
    ShellIntegrationHandshake(Vec<u8>),
    /// DECCOLM (DECSET 3, allowed by DECSET 40) or RIS asked for a grid
    /// `rows` × `cols`. The terminal does not resize its grid: the host's
    /// window and PTY decide the width (ADR 0020). A host that can resize
    /// its window may act on it by resizing the view and the PTY together
    /// and calling [`Terminal::resize`]; one that cannot ignores it, as
    /// `tessera-app-core` does.
    Resize {
        /// Rows: the grid's current height.
        rows: u16,
        /// Columns: 132 or 80.
        cols: u16,
    },
    /// Bytes to write to the child: the answer to a query it sent (DA,
    /// DSR, DECRQM, DECRQSS, XTVERSION, window size, ...; ADR 0024).
    /// Always 7-bit controls. One request per answered query, in the order
    /// the queries completed; the host writes them to the PTY in that
    /// order, after any input it already queued. The terminal never drops
    /// one: bounding them is the host's job.
    Reply(Vec<u8>),
}

/// The shape of the cursor, as DECSCUSR (`CSI Ps SP q`) and the
/// configuration name it: what the renderer draws while the cursor is
/// visible (DECTCEM) and the view has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CursorShape {
    /// A filled cell (DECSCUSR 1 and 2).
    #[default]
    Block,
    /// A horizontal bar at the bottom of the cell (DECSCUSR 3 and 4).
    Underline,
    /// A vertical bar at the left edge of the cell (DECSCUSR 5 and 6,
    /// xterm's "bar").
    Beam,
}

/// How the cursor looks: its shape and whether it blinks.
///
/// [`Terminal::cursor_style`] reports the one in effect, shape and blink
/// each on its own: the shape the program asked for (DECSCUSR) and the
/// blink it asked for (DECSCUSR, DECSET/DECRST 12) or, until it asks and
/// again after DECSCUSR 0 or 7 and after every reset, the configured
/// default's ([`Terminal::set_default_cursor_style`]). The default is a
/// steady block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CursorStyle {
    /// The shape drawn.
    pub shape: CursorShape,
    /// Whether the cursor blinks. The terminal only records it: blinking is
    /// the host's clock.
    pub blink: bool,
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
    /// Creates a terminal with an empty grid and every mode at its default
    /// ([`Modes::default`], [`KeyboardModes::default`]): auto-wrap (DECAWM)
    /// on, insert mode (IRM) off, the cursor visible.
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: Parser::new(),
            state: State {
                grid: Grid::new(rows, cols),
                pending: Vec::new(),
                modes: Modes::default(),
                keyboard: KeyboardModes::default(),
                kitty: KittyStacks::default(),
                last_printed: None,
                conformance_level: 4,
                c1_8bit_replies: false,
                shape_override: None,
                blink_override: None,
                default_cursor_style: CursorStyle::default(),
                dcs: Dcs::default(),
                title: String::new(),
                title_stack: TitleStack::default(),
                cell_pixels: None,
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

    /// Hands `bytes` to the parser and applies every action it produces,
    /// with 8-bit C1 controls honoured while 8-bit mode is on
    /// ([`Terminal::c1_8bit_replies`]; ADR 0024).
    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance_c1(bytes, &mut self.state);
    }

    /// Drains side-effect requests accumulated since the last call.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.state.pending)
    }

    /// Read access to the grid.
    pub fn grid(&self) -> &Grid {
        &self.state.grid
    }

    /// The terminal modes (DECSET/DECRST, SM/RM), as a copy. See the
    /// [`Modes`] fields for each mode and ADR 0020 for which ones the host
    /// acts on.
    pub fn modes(&self) -> Modes {
        let grid = &self.state.grid;
        Modes {
            origin: grid.origin_mode(),
            alt_screen: grid.is_alt_screen(),
            ..self.state.modes
        }
    }

    /// The modes key encoding needs, as a copy for
    /// [`encode_key`]: DECCKM, DECKPAM, `modifyOtherKeys` and the active
    /// screen's kitty keyboard flags.
    pub fn keyboard_modes(&self) -> KeyboardModes {
        let alt = self.state.grid.is_alt_screen();
        KeyboardModes {
            kitty_flags: self.state.kitty.get(alt).current(),
            ..self.state.keyboard
        }
    }

    /// The conformance level DECSCL (`CSI Pl ; Pc " p`) last selected:
    /// 1 (VT100) to 5 (VT500), `Pl - 60`. A new terminal is at level 4
    /// (VT400-class, as xterm's default `decTerminalID` 420); DECSTR and
    /// RIS keep the level. The device reports follow it: DA1, DA3, the DEC
    /// DSRs and DECRQSS (ADR 0024).
    pub fn conformance_level(&self) -> u8 {
        self.state.conformance_level
    }

    /// Whether 8-bit mode is on: replies to the host use 8-bit C1 controls
    /// (`CSI` as the single byte `0x9B` rather than `ESC [`, `0x90`, `0x9D`
    /// and `0x9C` for `ESC P`, `ESC ]` and `ESC \`), and C1 controls in the
    /// output are honoured (`0x84` is IND, `0x9B` starts a CSI; raw or
    /// UTF-8 encoded). S8C1T (`ESC SP G`) turns it on and S7C1T (`ESC SP
    /// F`) off, both only from conformance level 2; DECSCL sets it from its
    /// `Pc` (0 or absent, and 2: on; 1: off; level 1 always off) and RIS
    /// turns it off; DECSTR keeps it. It starts off.
    ///
    /// The host passes the keys, mouse reports, focus reports and paste
    /// brackets it writes through [`to_8bit_controls`] while it is on
    /// (ADR 0024).
    pub fn c1_8bit_replies(&self) -> bool {
        self.state.c1_8bit_replies
    }

    /// The cursor style in effect. Its shape is the one the program set
    /// with DECSCUSR (`CSI Ps SP q`) and its blink the one the program set
    /// with DECSCUSR or DECSET/DECRST 12; each is the configured default's
    /// ([`Terminal::set_default_cursor_style`]) while the program has set
    /// none, after DECSCUSR 0 or 7 and after DECSTR, DECSCL and RIS. So
    /// DECSET 12 never fixes the shape: a session that ran `vim` or `tmux`
    /// (whose `cnorm` is `CSI ? 12 l CSI ? 25 h`) still follows a new
    /// configured shape. See the `csi` module and ADR 0020 for the
    /// details.
    pub fn cursor_style(&self) -> CursorStyle {
        self.state.cursor_style()
    }

    /// Sets the configured cursor style: the one in effect while the
    /// program has asked for none, and the one DECSCUSR 0 and 7 and every
    /// reset return to. A shape the program set stays in effect until then.
    /// Starts as a steady block.
    ///
    /// A default whose blink differs from the current one is a change of
    /// the configured blink, which takes over from the program's: the
    /// blink the program set (DECSCUSR, DECSET/DECRST 12) is dropped, as
    /// DECSCUSR 0 would, and [`Modes::cursor_blink`] is cleared with it.
    /// tmux and vim send `CSI ? 12 l` on their own, so without this a tab
    /// that ever ran them would ignore every later edit of the configured
    /// blink (ADR 0020). Setting a default with the same blink, the same
    /// style included, changes nothing else.
    pub fn set_default_cursor_style(&mut self, style: CursorStyle) {
        if style.blink != self.state.default_cursor_style.blink {
            self.state.drop_blink_override();
        }
        self.state.default_cursor_style = style;
    }

    /// Sets the size of one cell in physical pixels, for the pixel size
    /// reports `CSI 14 t` (text area) and `CSI 16 t` (cell). Zero in either
    /// dimension means unknown, as before the first call: both reports
    /// then stay silent rather than answer a size a program would divide
    /// by. The host calls it whenever its view's cell size changes.
    pub fn set_cell_pixels(&mut self, width: u16, height: u16) {
        self.state.cell_pixels = (width > 0 && height > 0).then_some((width, height));
    }

    /// Whether auto-wrap mode (DECAWM) is on: [`Modes::auto_wrap`].
    /// Defaults to `true`.
    pub fn auto_wrap(&self) -> bool {
        self.state.modes.auto_wrap
    }

    /// Turns auto-wrap mode (DECAWM) on or off.
    ///
    /// With auto-wrap on, a character printed while the cursor sits on the
    /// last column with a wrap pending starts the next line and marks the
    /// finished one as soft-wrapped. With it off, that character overwrites
    /// the last column instead and no line is ever marked wrapped. Toggling
    /// the mode leaves an already pending wrap armed; the next printed
    /// character applies whichever mode is current.
    /// Same as `CSI ? 7 h` / `CSI ? 7 l`.
    pub fn set_auto_wrap(&mut self, on: bool) {
        self.state.modes.auto_wrap = on;
    }

    /// Whether insert mode (IRM) is on: [`Modes::insert`]. Defaults to
    /// `false`.
    pub fn insert_mode(&self) -> bool {
        self.state.modes.insert
    }

    /// Turns insert mode (IRM) on or off.
    ///
    /// In insert mode a printed character shifts the cells from the cursor
    /// to the end of the line one column right before it is written; the
    /// cell pushed past the last column is lost. Control characters are not
    /// affected. Same as `CSI 4 h` / `CSI 4 l`.
    pub fn set_insert_mode(&mut self, on: bool) {
        self.state.modes.insert = on;
    }

    /// Clears the screen to the cursor line (⌘K, "Clear to Start"): drops
    /// the scrollback and, on the primary screen, the rows above the
    /// cursor, so the cursor's line becomes the top row; on the alternate
    /// screen only the scrollback goes. See [`Grid::clear_to_cursor`].
    ///
    /// REP has nothing to repeat afterwards, as after any sequence: the
    /// cell the last character took may be gone.
    pub fn clear_to_cursor(&mut self) {
        self.state.grid.clear_to_cursor();
        self.state.last_printed = None;
    }

    /// Resizes the screen to `rows` × `cols` (a zero dimension counts as
    /// one), re-wrapping soft-wrapped lines on the primary screen and its
    /// scrollback and cutting or padding the alternate screen; see
    /// [`Grid::resize`]. The scroll region and the left and right margins
    /// become the full screen (xterm; left/right margin mode stays on if it
    /// was, ADR 0028). Call it
    /// before telling the child about the new size, so output written for
    /// the new size lands on a grid that has it.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.state.grid.resize(rows, cols);
    }

    /// Keeps at most `lines` lines of scrollback from now on (zero keeps
    /// none); the oldest lines over the new cap are dropped at once. See
    /// [`Grid::set_max_scrollback`].
    pub fn set_max_scrollback(&mut self, lines: usize) {
        self.state.grid.set_max_scrollback(lines);
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
    /// Every mode but the two the grid keeps: `origin` and `alt_screen`
    /// are never written here; [`Terminal::modes`] reads them from the
    /// grid, which saves origin mode with the cursor and owns the screens.
    modes: Modes,
    /// DECCKM, DECKPAM and `modifyOtherKeys`; `kitty_flags` is never
    /// written here, [`Terminal::keyboard_modes`] reads it from `kitty`.
    keyboard: KeyboardModes,
    /// The kitty keyboard flag stacks of both screens.
    kitty: KittyStacks,
    /// The last non-control character handed to [`State::print`], for REP
    /// (`CSI Ps b`), whether it took a cell, joined the previous cluster or
    /// was dropped; REP checks the cell it left behind. Every other action
    /// clears it once it has been applied, as xterm does, so REP only
    /// repeats a character printed immediately before it.
    last_printed: Option<char>,
    /// The operating level DECSCL (`CSI 6n ; Pc " p`) last selected, 1..=5;
    /// 4 (VT400-class, xterm's default `decTerminalID` 420) until then. Kept
    /// by DECSTR and RIS, as in xterm. The device reports read it.
    conformance_level: u8,
    /// 8-bit mode: replies use 8-bit C1 controls (`0x9B` for `ESC [`) and
    /// C1 controls in the input are honoured. Set by S8C1T and DECSCL's
    /// `Pc`, cleared by S7C1T, DECSCL level 1 and RIS; see
    /// [`Terminal::c1_8bit_replies`].
    c1_8bit_replies: bool,
    /// The cursor shape the program asked for: set by DECSCUSR 1..=6,
    /// dropped by DECSCUSR 0 and 7, DECSTR, DECSCL and RIS. DECSET 12 does
    /// not touch it. `None` follows `default_cursor_style`.
    shape_override: Option<CursorShape>,
    /// Whether the program asked for a blinking cursor: set by DECSCUSR
    /// 1..=6 and DECSET/DECRST 12, dropped with `shape_override` and by a
    /// configured default with another blink
    /// ([`Terminal::set_default_cursor_style`]). `None` follows
    /// `default_cursor_style`. [`Modes::cursor_blink`] is
    /// `blink_override.unwrap_or(false)`, the bit DECRQM 12 reports.
    blink_override: Option<bool>,
    /// The configured cursor style ([`Terminal::set_default_cursor_style`]):
    /// configuration, not terminal state, so no reset touches it.
    default_cursor_style: CursorStyle,
    /// The device control string in progress (DECRQSS, see `dcs`).
    dcs: Dcs,
    /// The title the terminal last emitted as [`Request::SetTitle`] (empty
    /// before any): what the title stack pushes.
    title: String,
    /// The XTWINOPS title stack (`CSI 22 t` / `CSI 23 t`). Kept by DECSTR
    /// and RIS.
    title_stack: TitleStack,
    /// The cell size in pixels, `(width, height)`, both non-zero, from
    /// [`Terminal::set_cell_pixels`]; `None` while unknown.
    cell_pixels: Option<(u16, u16)>,
}

/// The parser's consumer: [`State::apply`] for every action, and 8-bit mode
/// ([`State::c1_8bit_replies`]) for the C1 controls in the output.
impl Sink for State {
    fn action(&mut self, action: Action) {
        self.apply(action);
    }

    fn c1_controls(&self) -> bool {
        self.c1_8bit_replies
    }
}

impl State {
    /// The cursor style in effect: the program's shape and blink, each
    /// else the configured one.
    fn cursor_style(&self) -> CursorStyle {
        let default = self.default_cursor_style;
        CursorStyle {
            shape: self.shape_override.unwrap_or(default.shape),
            blink: self.blink_override.unwrap_or(default.blink),
        }
    }

    /// Drops the program's cursor style, shape and blink, so the
    /// configured default applies (DECSCUSR 0 and 7, DECSTR, DECSCL and
    /// RIS); clears the blink record [`Modes::cursor_blink`] too.
    fn drop_cursor_overrides(&mut self) {
        self.shape_override = None;
        self.drop_blink_override();
    }

    /// Drops the blink the program asked for, so the configured blink
    /// applies, and clears its record [`Modes::cursor_blink`].
    fn drop_blink_override(&mut self) {
        self.blink_override = None;
        self.modes.cursor_blink = false;
    }

    /// Records the blink the program asked for (DECSCUSR 1..=6, DECSET
    /// and DECRST 12) in [`State::blink_override`] and in its DECRQM
    /// record [`Modes::cursor_blink`], the one bit both share.
    fn set_blink_override(&mut self, on: bool) {
        self.blink_override = Some(on);
        self.modes.cursor_blink = on;
    }

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
            Action::Osc(params) => {
                let before = self.pending.len();
                self.osc(&params);
                // The title stack pushes the title last emitted.
                if let Some(Request::SetTitle(title)) = self.pending[before..].last() {
                    self.title.clone_from(title);
                }
            }
            Action::DcsStart {
                params,
                intermediates,
                action,
                ignore,
                ..
            } => self.dcs_start(&params, &intermediates, action, ignore),
            Action::DcsData(bytes) => self.dcs_data(&bytes),
            Action::DcsEnd => self.dcs_end(),
            ignored @ Action::Print(_) => trace!(action = ?ignored, "sequence not applied yet"),
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
        if self.modes.auto_wrap && !self.modes.insert {
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
        if width > self.grid.cols() {
            trace!(ch = ?ch, "wide character dropped: the grid has one column");
            return;
        }
        if !self.modes.auto_wrap {
            if self.grid.pending_wrap() {
                // The grid always arms a pending wrap where the line ends
                // (the last column or the right margin). Without DECAWM the
                // character overwrites that column (xterm `dotext`).
                let Point { row, col } = self.grid.cursor();
                self.grid.goto(row, col);
            }
            if width == 2 && self.grid.cursor().col == self.grid.line_end() {
                // No room and no wrap: the character is dropped (xterm).
                return;
            }
            if self.modes.insert {
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
        if self.modes.insert {
            let stranded = width == 2 && self.grid.cursor().col == self.grid.line_end();
            if self.grid.pending_wrap() || stranded {
                // `insert_chars` disarms the pending wrap, so wrap first and
                // insert on the new line, as xterm does. A wide character
                // with one column left first pads that column, as the
                // grid does in replace mode.
                if !self.grid.pending_wrap() {
                    self.grid.pad_line_end();
                }
                self.grid.wrap_line();
            }
            self.grid.insert_chars(width);
        }
        self.grid.put_char(ch);
    }
}

#[cfg(test)]
mod tests {
    use super::to_8bit_controls;

    fn eight_bit(sequence: &[u8]) -> Vec<u8> {
        let mut bytes = sequence.to_vec();
        to_8bit_controls(&mut bytes);
        bytes
    }

    #[test]
    fn introducers_and_the_string_terminator_become_c1_bytes() {
        assert_eq!(eight_bit(b"\x1b[?64;21;22c"), b"\x9b?64;21;22c");
        assert_eq!(eight_bit(b"\x1bOA"), b"\x8fA");
        assert_eq!(eight_bit(b"\x1bP1$r0m\x1b\\"), b"\x901$r0m\x9c");
        assert_eq!(
            eight_bit(b"\x1b]10;rgb:0000/0000/0000\x1b\\"),
            b"\x9d10;rgb:0000/0000/0000\x9c"
        );
    }

    /// A BEL-terminated OSC reply keeps its BEL (BEL is not a C1 control).
    #[test]
    fn a_bel_terminator_stays() {
        assert_eq!(eight_bit(b"\x1b]10;?\x07"), b"\x9d10;?\x07");
    }

    /// Only the leading introducer and, after a DCS or OSC one, the
    /// trailing ST change: an ESC elsewhere, a two-byte Alt-prefixed key and
    /// a CSI ending in `ESC \` stay as they are.
    #[test]
    fn nothing_else_changes() {
        for same in [
            &b""[..],
            b"\x1b",
            b"\x1b[",
            b"\x1bO",
            b"\x1bP",
            b"\x1b]",
            b"\x1b\\",
            b"\x1ba",
            b"\x1b\x1b[A",
            b"a\x1b[A",
            b"\x03",
        ] {
            assert_eq!(eight_bit(same), same, "{same:x?}");
        }
        assert_eq!(eight_bit(b"\x1b[M\x1b\x1b\\"), b"\x9bM\x1b\x1b\\");
        assert_eq!(eight_bit(b"\x1bPq\x1b[A\x1b\\"), b"\x90q\x1b[A\x9c");
    }
}
