//! Terminal modes: DECSET/DECRST (`CSI ? Pm h` / `CSI ? Pm l`), SM/RM
//! (`CSI Pm h` / `CSI Pm l`), DECKPAM/DECKPNM (`ESC =` / `ESC >`),
//! xterm's `modifyOtherKeys` (`CSI > 4 ; Pv m`) and the kitty keyboard
//! protocol's flag stack (`CSI > flags u`, `CSI < n u`, `CSI = flags ; mode
//! u`).
//!
//! [`Terminal`](crate::Terminal) owns every mode (ADR 0020). The host reads
//! them as two `Copy` snapshots, [`Modes`] from
//! [`Terminal::modes`](crate::Terminal::modes) and
//! [`KeyboardModes`] from
//! [`Terminal::keyboard_modes`](crate::Terminal::keyboard_modes), under
//! the same lock that feeds the terminal.
//!
//! # DEC private modes
//!
//! | Mode | Name | Effect |
//! |---|---|---|
//! | 1 | DECCKM | [`KeyboardModes::application_cursor_keys`] |
//! | 3 | DECCOLM | only while 40 is set: clear, reset the margins, home, [`Modes::columns_132`] and a [`Request::Resize`]; the grid keeps its width |
//! | 5 | DECSCNM | [`Modes::reverse_video`], stored only |
//! | 6 | DECOM | [`Modes::origin`]; set and reset home the cursor (to the region's top in origin mode) |
//! | 7 | DECAWM | [`Modes::auto_wrap`]; a pending wrap stays armed |
//! | 9, 1000, 1002, 1003 | mouse tracking | [`Modes::mouse`], one at a time; resetting any of them turns tracking off |
//! | 12 | cursor blink | [`Modes::cursor_blink`], stored only |
//! | 25 | DECTCEM | [`Modes::cursor_visible`] |
//! | 40 | allow 80 ⇄ 132 | [`Modes::allow_column_change`] |
//! | 47 | alternate screen | enter; reset leaves with the cursor where it is |
//! | 66 | DECNKM | [`KeyboardModes::application_keypad`], the DECKPAM flag |
//! | 1004 | focus events | [`Modes::focus_events`] |
//! | 1005, 1006, 1015, 1016 | mouse encoding | [`Modes::mouse_encoding`], the last set wins; a reset clears only the active one |
//! | 1047 | alternate screen | as 47, and a reset clears the alternate screen first |
//! | 1048 | save cursor | set is DECSC, reset is DECRC |
//! | 1049 | alternate screen | set saves the cursor, enters and clears; reset leaves and restores the cursor |
//! | 2004 | bracketed paste | [`Modes::bracketed_paste`] |
//! | 2026 | synchronized output | [`Modes::synchronized_output`], stored only |
//!
//! Every other mode, `0` and an empty parameter included, is ignored and
//! logged at trace level; one sequence may set several modes, and an
//! unknown one among them does not stop the others. Notably ignored: 2
//! (DECANM: no VT52 mode), 4 (DECSCLM, smooth scroll, not IRM), 45 and 69
//! (reverse wrap and DECLRMM, #97) and 1001 (highlight tracking).
//!
//! Setting a mode again repeats its side effects (DECOM homes again, 1049
//! clears again), as in xterm. A 1049 reset on the primary screen does
//! nothing at all, as in kitty and Alacritty (xterm would restore the
//! saved cursor). The alternate screen's contents are fresh on every entry,
//! also for 47, where xterm keeps them between visits.
//!
//! # ANSI modes (SM / RM)
//!
//! 4 (IRM, [`Modes::insert`]) and 20 (LNM, [`Modes::linefeed_newline`]:
//! LF, VT and FF also return the carriage). Every other mode, KAM (2) and
//! SRM (12) included, is ignored and logged.
//!
//! # Keyboard modes
//!
//! `CSI > 4 ; Pv m` sets `modifyOtherKeys` to level 0, 1 or 2 (a higher
//! level is ignored); `CSI > 4 m` and `CSI > m` reset it to 0. Other
//! resources (`CSI > 1 ; Pv m`, …) are ignored.
//!
//! The kitty keyboard flags live on a stack per screen (main and alternate,
//! as the protocol requires), each at most [`KITTY_STACK_DEPTH`] entries
//! deep: a push onto a full stack evicts the oldest entry. The current
//! flags are the top entry, none on an empty stack. `CSI > flags u`
//! pushes (flags default 0), `CSI < n u` pops `n` entries (default 1;
//! popping more than there are empties the stack), and `CSI = flags ; mode
//! u` changes the top entry (pushing one onto an empty stack) with mode 1
//! (replace, the default), 2 (set bits) or 3 (clear bits); another mode is
//! ignored. Bits above the five defined flags are masked off. The parser
//! reports `CSI < 0 u` exactly like `CSI < u`, so it pops one entry (kitty
//! pops none). The query `CSI ? u` is #10.
//!
//! # Resets
//!
//! RIS (`ESC c`) returns every mode to its default and empties both kitty
//! stacks, except [`Modes::allow_column_change`], which xterm keeps (it is
//! a resource there); in 132-column mode it also asks the host for 80
//! columns with a [`Request::Resize`] (the grid keeps its width). DECSTR (`CSI ! p`) resets origin
//! mode, cursor visibility (on), cursor blink, synchronized output,
//! insert mode, auto-wrap (on), DECCKM, DECKPAM, `modifyOtherKeys` and both
//! kitty stacks, and keeps the rest: reverse video, LNM, the alternate
//! screen, bracketed paste, focus events and the mouse modes.

use tessera_grid::EraseDisplay;
use tracing::trace;

use crate::keyboard::{KeyboardModes, KittyFlags, KittyFlagsMode, ModifyOtherKeys};
use crate::{Request, State};

/// The terminal modes that are not about the keyboard, as
/// [`Terminal::modes`](crate::Terminal::modes) reports them.
///
/// Defaults are xterm's: auto-wrap and a visible cursor on, everything else
/// off. Copied out under the terminal lock, so reading it is one copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
// One flag per DEC/ANSI mode, as the protocol defines them: an enum per
// flag would only rename `bool`.
#[allow(clippy::struct_excessive_bools)]
pub struct Modes {
    /// DECOM (DECSET 6): CUP, HVP, VPA, VPR and HPR count rows from the top
    /// margin and stop at the bottom one. Saved and restored by DECSC and
    /// DECRC.
    pub origin: bool,
    /// DECAWM (DECSET 7): a character printed past the last column starts
    /// the next line. Default on.
    pub auto_wrap: bool,
    /// DECTCEM (DECSET 25): the cursor is drawn. Default on.
    pub cursor_visible: bool,
    /// DECSET 12 (xterm's `att610` blink): the application asked for a
    /// blinking cursor. Stored only.
    pub cursor_blink: bool,
    /// DECSCNM (DECSET 5): the whole screen is shown in reverse video.
    /// Stored only.
    pub reverse_video: bool,
    /// IRM (SM 4): printed characters shift the rest of the line right.
    pub insert: bool,
    /// LNM (SM 20): LF, VT and FF also perform a carriage return.
    pub linefeed_newline: bool,
    /// DECSET 40 (xterm's `c132` resource): DECCOLM takes effect.
    pub allow_column_change: bool,
    /// DECCOLM (DECSET 3): the application asked for 132 columns, changed
    /// only while [`Modes::allow_column_change`] is on. The grid's width
    /// stays the host's (ADR 0020).
    pub columns_132: bool,
    /// The alternate screen is active (DECSET 47, 1047 or 1049).
    pub alt_screen: bool,
    /// DECSET 2004: pastes are wrapped in `ESC [ 200 ~` and `ESC [ 201 ~`.
    pub bracketed_paste: bool,
    /// DECSET 1004: focus changes are reported as `ESC [ I` and `ESC [ O`.
    pub focus_events: bool,
    /// The mouse tracking mode (DECSET 9, 1000, 1002, 1003). Stored only
    /// until #12.
    pub mouse: MouseTracking,
    /// The mouse report encoding (DECSET 1005, 1006, 1015, 1016). Stored
    /// only until #12.
    pub mouse_encoding: MouseEncoding,
    /// DECSET 2026: the application is in the middle of a synchronized
    /// update. Stored only.
    pub synchronized_output: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            origin: false,
            auto_wrap: true,
            cursor_visible: true,
            cursor_blink: false,
            reverse_video: false,
            insert: false,
            linefeed_newline: false,
            allow_column_change: false,
            columns_132: false,
            alt_screen: false,
            bracketed_paste: false,
            focus_events: false,
            mouse: MouseTracking::None,
            mouse_encoding: MouseEncoding::Default,
            synchronized_output: false,
        }
    }
}

/// Which mouse events the application asked for. xterm keeps exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MouseTracking {
    /// No mouse reporting.
    #[default]
    None,
    /// DECSET 9: X10 compatibility, button presses only.
    X10,
    /// DECSET 1000: presses and releases.
    Normal,
    /// DECSET 1002: presses, releases and motion while a button is down.
    Button,
    /// DECSET 1003: presses, releases and all motion.
    AnyMotion,
}

/// How mouse reports are encoded. xterm keeps exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MouseEncoding {
    /// `CSI M Cb Cx Cy` with one byte per value.
    #[default]
    Default,
    /// DECSET 1005: coordinates as UTF-8.
    Utf8,
    /// DECSET 1006: `CSI < Cb ; Cx ; Cy M/m`.
    Sgr,
    /// DECSET 1015: `CSI Cb ; Cx ; Cy M` (urxvt).
    Urxvt,
    /// DECSET 1016: SGR with pixel coordinates.
    SgrPixels,
}

/// Entries each kitty keyboard flag stack keeps; a push onto a full stack
/// evicts the oldest. kitty and Ghostty use 8; the protocol only asks for
/// a limit.
pub const KITTY_STACK_DEPTH: usize = 8;

/// One screen's kitty keyboard flag stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct KittyStack {
    /// Oldest first; only `..len` is in use.
    entries: [KittyFlags; KITTY_STACK_DEPTH],
    len: usize,
}

impl KittyStack {
    /// The top entry, or no flags on an empty stack.
    pub(crate) fn current(&self) -> KittyFlags {
        self.len
            .checked_sub(1)
            .map_or(KittyFlags::NONE, |top| self.entries[top])
    }

    /// `CSI > flags u`: pushes `flags`, evicting the oldest entry when the
    /// stack is full.
    fn push(&mut self, flags: KittyFlags) {
        if self.len == KITTY_STACK_DEPTH {
            self.entries.copy_within(1.., 0);
            self.len -= 1;
        }
        self.entries[self.len] = flags;
        self.len += 1;
    }

    /// `CSI < n u`: pops up to `n` entries.
    fn pop(&mut self, n: usize) {
        self.len = self.len.saturating_sub(n);
    }

    /// `CSI = flags ; mode u`: changes the top entry, pushing one first on
    /// an empty stack.
    fn set(&mut self, flags: KittyFlags, mode: KittyFlagsMode) {
        if self.len == 0 {
            self.push(KittyFlags::NONE);
        }
        let top = &mut self.entries[self.len - 1];
        *top = top.apply(flags, mode);
    }
}

/// The two kitty stacks: the main screen's and the alternate screen's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct KittyStacks {
    main: KittyStack,
    alt: KittyStack,
}

impl KittyStacks {
    /// The stack of the main screen or, with `alt`, the alternate one.
    pub(crate) fn get(&self, alt: bool) -> &KittyStack {
        if alt { &self.alt } else { &self.main }
    }

    fn get_mut(&mut self, alt: bool) -> &mut KittyStack {
        if alt { &mut self.alt } else { &mut self.main }
    }
}

/// Parameter `index`, absent meaning zero.
fn param(params: &[u16], index: usize) -> u16 {
    params.get(index).copied().unwrap_or(0)
}

impl State {
    /// DECSET (`on`) or DECRST: every parameter in order.
    pub(crate) fn set_dec_modes(&mut self, params: &[u16], on: bool) {
        for &mode in params {
            self.set_dec_mode(mode, on);
        }
    }

    /// One DEC private mode; see the module documentation.
    fn set_dec_mode(&mut self, mode: u16, on: bool) {
        match mode {
            1 => self.keyboard.application_cursor_keys = on,
            3 => self.set_column_mode(on),
            5 => self.modes.reverse_video = on,
            6 => {
                // xterm: flag first, then `CursorSet(0, 0)`, on every set
                // and reset, which also clears a pending wrap.
                self.grid.set_origin_mode(on);
                self.grid.goto_origin(0, 0);
            }
            7 => self.modes.auto_wrap = on,
            9 => self.set_mouse_tracking(MouseTracking::X10, on),
            12 => self.modes.cursor_blink = on,
            25 => self.modes.cursor_visible = on,
            40 => self.modes.allow_column_change = on,
            47 => {
                if on {
                    self.grid.enter_alt_screen();
                } else {
                    self.grid.leave_alt_screen_in_place();
                }
            }
            66 => self.keyboard.application_keypad = on,
            1000 => self.set_mouse_tracking(MouseTracking::Normal, on),
            1002 => self.set_mouse_tracking(MouseTracking::Button, on),
            1003 => self.set_mouse_tracking(MouseTracking::AnyMotion, on),
            1004 => self.modes.focus_events = on,
            1005 => self.set_mouse_encoding(MouseEncoding::Utf8, on),
            1006 => self.set_mouse_encoding(MouseEncoding::Sgr, on),
            1015 => self.set_mouse_encoding(MouseEncoding::Urxvt, on),
            1016 => self.set_mouse_encoding(MouseEncoding::SgrPixels, on),
            1047 => {
                if on {
                    self.grid.enter_alt_screen();
                } else if self.grid.is_alt_screen() {
                    // xterm `ClearScreen` before `FromAlternate`. The next
                    // entry starts blank anyway, so what it shows is the
                    // disarmed pending wrap the cursor takes back (ALT15).
                    self.grid.erase_in_display(EraseDisplay::All);
                    self.grid.leave_alt_screen_in_place();
                }
            }
            1048 => {
                if on {
                    self.grid.save_cursor();
                } else {
                    self.grid.restore_cursor();
                }
            }
            1049 => {
                if on {
                    // Into the active screen's slot: the alternate one when
                    // already there, as in xterm.
                    self.grid.save_cursor();
                    self.grid.enter_alt_screen();
                    self.grid.erase_in_display(EraseDisplay::All);
                } else if self.grid.is_alt_screen() {
                    // Restores the cursor, pen, pending wrap and origin
                    // mode saved on entry (moved with the text by a resize
                    // in between), which is what the DECSC above saved.
                    self.grid.leave_alt_screen();
                }
            }
            2004 => self.modes.bracketed_paste = on,
            2026 => self.modes.synchronized_output = on,
            _ => trace!(mode, on, "DECSET/DECRST mode ignored"),
        }
    }

    /// One of 9, 1000, 1002 and 1003: setting one replaces the others;
    /// resetting any of them turns tracking off (xterm).
    fn set_mouse_tracking(&mut self, tracking: MouseTracking, on: bool) {
        self.modes.mouse = if on { tracking } else { MouseTracking::None };
    }

    /// One of 1005, 1006, 1015 and 1016: the last set wins; a reset only
    /// clears the encoding it names (xterm `extend_coords`).
    fn set_mouse_encoding(&mut self, encoding: MouseEncoding, on: bool) {
        if on {
            self.modes.mouse_encoding = encoding;
        } else if self.modes.mouse_encoding == encoding {
            self.modes.mouse_encoding = MouseEncoding::Default;
        }
    }

    /// DECCOLM. Ignored entirely unless DECSET 40 allows it (xterm's
    /// `c132`, default off). Then xterm's side effects, in every case: the
    /// screen cleared, the scroll region reset and the cursor homed
    /// (origin-relative in origin mode). The grid keeps its width, which
    /// the host's window and PTY decide (ADR 0020): the mode is recorded
    /// in [`Modes::columns_132`] and, when it changes to a width (132 for
    /// `on`, 80 otherwise) the grid does not have, a [`Request::Resize`]
    /// asks the host for it.
    fn set_column_mode(&mut self, on: bool) {
        if !self.modes.allow_column_change {
            trace!(on, "DECCOLM ignored without DECSET 40");
            return;
        }
        if self.modes.columns_132 != on {
            self.modes.columns_132 = on;
            self.request_columns(if on { 132 } else { 80 });
        }
        self.grid.erase_in_display(EraseDisplay::All);
        self.grid.reset_scroll_region();
        self.grid.goto_origin(0, 0);
    }

    /// Asks the host for a grid `cols` columns wide at the current height,
    /// unless the grid already is. The grid itself is not resized.
    fn request_columns(&mut self, cols: u16) {
        if self.grid.cols() != cols {
            let rows = self.grid.rows();
            self.pending.push(Request::Resize { rows, cols });
        }
    }

    /// SM (`on`) or RM: every parameter in order.
    pub(crate) fn set_ansi_modes(&mut self, params: &[u16], on: bool) {
        for &mode in params {
            match mode {
                4 => self.modes.insert = on,
                20 => self.modes.linefeed_newline = on,
                _ => trace!(mode, on, "SM/RM mode ignored"),
            }
        }
    }

    /// `CSI > Pp ; Pv m` (XTMODKEYS). Only `modifyOtherKeys` (Pp 4) is
    /// kept: levels 0 to 2, a missing level meaning 0. Pp 0 (or none)
    /// resets every resource, here just `modifyOtherKeys`.
    pub(crate) fn set_key_modifiers(&mut self, params: &[u16]) {
        let level = match (param(params, 0), param(params, 1)) {
            (0 | 4, 0) => ModifyOtherKeys::Disabled,
            (4, 1) => ModifyOtherKeys::Level1,
            (4, 2) => ModifyOtherKeys::Level2,
            _ => {
                trace!(?params, "XTMODKEYS resource or level ignored");
                return;
            }
        };
        self.keyboard.modify_other_keys = level;
    }

    /// The kitty keyboard protocol's `CSI > flags u` (`b'>'`), `CSI < n u`
    /// (`b'<'`) and `CSI = flags ; mode u` (`b'='`), on the active screen's
    /// stack.
    pub(crate) fn kitty_keyboard(&mut self, marker: u8, params: &[u16]) {
        let stack = self.kitty.get_mut(self.grid.is_alt_screen());
        let flags = KittyFlags::from_bits_truncate(u32::from(param(params, 0)));
        match marker {
            b'>' => stack.push(flags),
            // `CSI < u` and `CSI < 0 u` look alike: both pop one.
            b'<' => stack.pop(usize::from(param(params, 0).max(1))),
            b'=' => {
                let mode = match param(params, 1) {
                    0 => Some(KittyFlagsMode::Replace),
                    mode => KittyFlagsMode::from_param(u32::from(mode)),
                };
                if let Some(mode) = mode {
                    stack.set(flags, mode);
                } else {
                    trace!(?params, "kitty keyboard set mode ignored");
                }
            }
            _ => trace!(marker, ?params, "kitty keyboard sequence ignored"),
        }
    }

    /// DECKPAM (`ESC =`, `on`) and DECKPNM (`ESC >`).
    pub(crate) fn set_application_keypad(&mut self, on: bool) {
        self.keyboard.application_keypad = on;
    }

    /// The keyboard and mode part of DECSTR: see the module documentation.
    /// Origin mode is turned off here, before the caller resets the saved
    /// cursor.
    pub(crate) fn soft_reset_modes(&mut self) {
        self.grid.set_origin_mode(false);
        self.modes.cursor_visible = true;
        self.modes.cursor_blink = false;
        self.modes.insert = false;
        // xterm's resource default; DEC's table 5-9 says off.
        self.modes.auto_wrap = true;
        // The spec is silent; a program that died between the begin and
        // the end of an update must not freeze the screen after `reset`.
        self.modes.synchronized_output = false;
        self.keyboard = KeyboardModes::default();
        self.kitty = KittyStacks::default();
    }

    /// The mode part of RIS: every mode and both kitty stacks to their
    /// defaults, except [`Modes::allow_column_change`]; leaving 132-column
    /// mode asks the host for 80 columns (xterm), without resizing the
    /// grid. Runs after the grid's own reset.
    pub(crate) fn reset_modes(&mut self) {
        let allow_column_change = self.modes.allow_column_change;
        if self.modes.columns_132 && allow_column_change {
            self.request_columns(80);
        }
        // The grid's reset already turned origin mode off; say so here
        // too, so RIS never depends on how the grid resets.
        self.grid.set_origin_mode(false);
        self.modes = Modes {
            allow_column_change,
            ..Modes::default()
        };
        self.keyboard = KeyboardModes::default();
        self.kitty = KittyStacks::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(bits: u32) -> KittyFlags {
        KittyFlags::from_bits_truncate(bits)
    }

    #[test]
    fn a_full_stack_evicts_the_oldest_entry() {
        let mut stack = KittyStack::default();
        for bits in 1..=9 {
            stack.push(flags(bits));
        }
        assert_eq!(stack.len, KITTY_STACK_DEPTH);
        assert_eq!(stack.current(), flags(9));
        for bits in (3..=8).rev() {
            stack.pop(1);
            assert_eq!(stack.current(), flags(bits));
        }
        stack.pop(1);
        assert_eq!(stack.current(), flags(2), "1 was evicted");
        stack.pop(1);
        assert_eq!(stack.current(), KittyFlags::NONE);
    }

    #[test]
    fn set_on_an_empty_stack_pushes_one_entry() {
        let mut stack = KittyStack::default();
        stack.set(flags(3), KittyFlagsMode::Set);
        assert_eq!((stack.len, stack.current()), (1, flags(3)));
        stack.pop(1);
        assert_eq!((stack.len, stack.current()), (0, KittyFlags::NONE));
    }
}
