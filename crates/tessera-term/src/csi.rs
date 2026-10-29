//! Control sequences (`CSI ... final`): the [`Action::Csi`] half of the
//! dispatcher.
//!
//! Implemented without intermediates or a private marker:
//!
//! | Final | Name | Effect |
//! |---|---|---|
//! | `A` `B` `C` `D` | CUU CUD CUF CUB | move the cursor; CUU/CUD stop at the scroll margins |
//! | `E` `F` | CNL CPL | CUD / CUU, then column zero |
//! | `G` `` ` `` | CHA HPA | absolute column |
//! | `d` | VPA | absolute row (from the top margin in origin mode) |
//! | `H` `f` | CUP HVP | absolute position (row from the top margin in origin mode) |
//! | `a` `e` | HPR VPR | relative position: right / down, clamped to the screen (VPR ignores the scroll region unless origin mode is on) |
//! | `h` `l` | SM RM | set / reset ANSI modes: IRM (4) and LNM (20), see the `modes` module |
//! | `I` `Z` | CHT CBT | tab forward / back |
//! | `J` `K` | ED EL | erase in display / line |
//! | `@` `P` `X` | ICH DCH ECH | insert / delete / erase characters |
//! | `L` `M` | IL DL | insert / delete lines inside the scroll region |
//! | `S` `T` `^` | SU SD | scroll the region up / down (`T` with more than one parameter is mouse tracking, ignored) |
//! | `b` | REP | repeat the last printed character |
//! | `g` | TBC | clear tab stops |
//! | `m` | SGR | set the pen's colours and attributes (see the `sgr` module) |
//! | `r` | DECSTBM | set the scroll region |
//! | `s` `u` | SCOSC SCORC | save / restore the cursor exactly like DECSC / DECRC, only without parameters |
//!
//! With a private marker or an intermediate (the `modes` module has the
//! details):
//!
//! | Sequence | Name | Effect |
//! |---|---|---|
//! | `CSI ? Pm h` `CSI ? Pm l` | DECSET DECRST | set / reset DEC private modes |
//! | `CSI > Pp ; Pv m` | XTMODKEYS | `modifyOtherKeys` (Pp 4) |
//! | `CSI > flags u` `CSI < n u` `CSI = flags ; mode u` | kitty keyboard | push / pop / set the keyboard flags |
//! | `CSI ! p` | DECSTR | soft terminal reset, see the crate documentation; parameters are ignored, as in xterm, so `CSI 5 ! p` resets too |
//! | `CSI Ps SP q` | DECSCUSR | cursor style: 1..=6 blinking/steady block, underline, beam; 0, absent and 7 the configured default; anything else ignored (see `State::set_cursor_style`) |
//! | `CSI Pl ; Pc " p` | DECSCL | select conformance level: for `Pl` 61..=65, the DECSTR soft reset, then level `Pl - 60` and the C1 reply mode from `Pc`; any other `Pl` does nothing |
//!
//! Parameters follow xterm: an absent parameter and an explicit `0` both
//! mean the default, positions are 1-based and clamp to the screen, and
//! every count saturates. In origin mode (DECOM, `CSI ? 6 h`) CUP, HVP,
//! VPA and VPR count rows from the top margin and stop at the bottom one
//! (xterm `CursorSet`); columns stay absolute, as there are no left and
//! right margins yet (#97). Nothing here can panic on hostile input.
//!
//! REP is not capped, as in xterm and Alacritty: a count up to 65535 costs
//! that many prints, each O(1) except in insert mode, where each one shifts
//! the rest of the line (O(count x cols)).
//!
//! HPR and VPR are xterm's `CursorSet(row, col + n)` and
//! `CursorSet(row + n, col)`: they clamp to the screen, not to the scroll
//! margins (CUD stops at the bottom margin, VPR does not), and clear a
//! pending wrap like every cursor move. In origin mode both count the row
//! from the top margin and stop at the bottom one, as CUP does.
//!
//! SCOSC (`CSI s`) and SCORC (`CSI u`) share the DECSC / DECRC slot
//! ([`tessera_grid::Grid::save_cursor`]): cursor, pen and pending wrap, and
//! a SCORC with nothing saved homes the cursor with the default pen. As in
//! xterm (`only_default()`) both act only without parameters, so
//! `CSI 1;10 s` (DECSLRM's shape) and `CSI 3 u` do nothing. xterm also
//! leaves its parser in the CSI state after a SCORC with parameters (no
//! `ResetState`), so it takes the next byte as another final; Tessera
//! returns to ground as after any sequence and does not copy that. The parser
//! reports `CSI s` and `CSI 0 s` alike, so the explicit zero saves too,
//! where xterm ignores it. `CSI s` is SCOSC only while left/right margin
//! mode (DECLRMM, `?69`, #97) is off; until #97 it always is. The kitty
//! keyboard sequences `CSI ? u`, `CSI > u`, `CSI < u` and `CSI = u` carry a
//! private marker and never reach SCORC; the query `CSI ? u` is #10.
//!
//! DECSCL follows xterm's `CASE_DECSCL`: the reset is DECSTR's, soft (DEC's
//! VT510 manual says RIS; xterm, the VT220 manual and DEC STD 070 say
//! soft), and `Pc` 0, 2 or absent selects 8-bit C1 replies, 1 selects
//! 7-bit and any other value keeps the mode; parameters after `Pc` are
//! ignored. Two deviations from xterm, both from the issue (#92): level 1
//! (`CSI 61 " p`, VT100 mode) always selects 7-bit replies, as DEC's VT100
//! mode has no 8-bit controls (xterm keeps the previous mode), and level 5
//! (`CSI 65 " p`) is accepted although the default level is 4 (xterm, with
//! its default `decTerminalID` 420, accepts only up to 64). Nothing is
//! answered; `State::select_conformance_level` has the details.
//!
//! DECSET/DECRST, SM/RM, DECSCL, DECSCUSR and the kitty keyboard sequences are
//! ignored as a whole when a parameter carries a colon sub-parameter
//! (`CSI ? 25:1049 h`), as in xterm, which accepts them only in SGR and
//! XTMODKEYS.
//!
//! Everything else (an unassigned final, any other private marker and
//! final pair such as `CSI > 25 h` or `CSI ? 5 m`, any other intermediate
//! such as `$`, `SP` with a final other than `q` and `"` with one other
//! than `p`, a private marker before `SP q`, and the xterm extension SL/SR
//! that is not in scope) is ignored and logged at trace level. The parser hands over private markers as intermediates, so one
//! match covers both.
//!
//! Expected behaviour per sequence and edge case is pinned by
//! `tests/fixtures/csi_spec.txt`, for DECSTR by
//! `tests/fixtures/decstr_spec.txt`, for the modes by
//! `tests/fixtures/modes_spec.txt` and for DECSCUSR by
//! `tests/fixtures/decscusr_spec.txt`.
//!
//! [`Action::Csi`]: tessera_vt::Action::Csi

use tessera_grid::{Cell, EraseDisplay, EraseLine, Style};
use tracing::trace;

use crate::{CursorShape, CursorStyle, State};

/// Parameter `index`, or `default` when it is absent or zero.
fn param_or(params: &[u16], index: usize, default: u16) -> u16 {
    match params.get(index) {
        None | Some(0) => default,
        Some(&n) => n,
    }
}

/// Parameter `index` as a count or a 1-based position: absent and zero
/// both mean one.
fn count(params: &[u16], index: usize) -> u16 {
    param_or(params, index, 1)
}

/// Whether the sequence had no parameter (xterm `only_default()`, used by
/// SCOSC and SCORC). The parser reports `CSI s` as `[0]`, so an explicit
/// `CSI 0 s` counts as parameterless too.
fn no_params(params: &[u16]) -> bool {
    matches!(params, [] | [0])
}

/// Parameter `index` as a selector (ED, EL, TBC): absent means zero.
fn selector(params: &[u16], index: usize) -> u16 {
    params.get(index).copied().unwrap_or(0)
}

impl State {
    /// Applies one control sequence. See the module documentation for the
    /// sequences handled; anything else is logged and ignored. `subparams`
    /// marks the `:`-introduced entries of `params`
    /// ([`tessera_vt::Action::Csi`]); only SGR reads it.
    pub(crate) fn csi(
        &mut self,
        params: &[u16],
        subparams: u32,
        intermediates: &[u8],
        action: char,
    ) {
        if !intermediates.is_empty() {
            match (intermediates, action) {
                // DECSTR. Parameters are ignored, as in xterm.
                (b"!", 'p') => self.soft_reset(),
                // DECSCL; sub-parameters void it, as for the mode sequences.
                (b"\"", 'p') => {
                    if subparams == 0 {
                        self.select_conformance_level(params);
                    } else {
                        trace!(?params, "DECSCL with sub-parameters ignored");
                    }
                }
                // xterm takes colon sub-parameters only in SGR and XTMODKEYS.
                (b"?", 'h' | 'l') | ([b'>' | b'<' | b'='], 'u') | (b" ", 'q') if subparams != 0 => {
                    trace!(
                        ?action,
                        ?params,
                        "mode sequence with sub-parameters ignored"
                    );
                }
                (b"?", 'h' | 'l') => self.set_dec_modes(params, action == 'h'),
                (b" ", 'q') => self.set_cursor_style(selector(params, 0)),
                (b">", 'm') => self.set_key_modifiers(params),
                ([marker @ (b'>' | b'<' | b'=')], 'u') => self.kitty_keyboard(*marker, params),
                _ => trace!(
                    ?action,
                    ?params,
                    ?intermediates,
                    "CSI with intermediates ignored"
                ),
            }
            return;
        }
        let grid = &mut self.grid;
        match action {
            'A' => grid.cursor_up(count(params, 0)),
            'B' => grid.cursor_down(count(params, 0)),
            'C' => grid.cursor_right(count(params, 0)),
            'D' => grid.cursor_left(count(params, 0)),
            'E' => {
                grid.cursor_down(count(params, 0));
                grid.carriage_return();
            }
            'F' => {
                grid.cursor_up(count(params, 0));
                grid.carriage_return();
            }
            'G' | '`' => grid.goto_col(count(params, 0) - 1),
            'd' => grid.goto_row_origin(count(params, 0) - 1),
            'H' | 'f' => grid.goto_origin(count(params, 0) - 1, count(params, 1) - 1),
            // HPR and VPR: xterm `CursorSet`, clamped to the screen rather
            // than to the scroll margins (to the region in origin mode).
            // `goto_origin` clamps and clears the pending wrap.
            'a' => {
                let col = grid.cursor().col.saturating_add(count(params, 0));
                grid.goto_origin(grid.origin_row(), col);
            }
            'e' => {
                let row = grid.origin_row().saturating_add(count(params, 0));
                grid.goto_origin(row, grid.cursor().col);
            }
            // With a wrap pending the cursor sits on the last column and no
            // stop lies ahead: xterm leaves the wrap armed, as for HT.
            'I' if grid.pending_wrap() => {}
            'I' => grid.tab_forward(count(params, 0)),
            'Z' => grid.tab_backward(count(params, 0)),
            'J' => self.erase_in_display(selector(params, 0)),
            'K' => self.erase_in_line(selector(params, 0)),
            '@' => grid.insert_chars(count(params, 0)),
            'P' => grid.delete_chars(count(params, 0)),
            'X' => grid.erase_chars(count(params, 0)),
            'L' | 'M' => self.edit_lines(action, count(params, 0)),
            'S' => grid.scroll_up(count(params, 0)),
            // xterm reads `CSI 0 T` as a mouse-tracking reset, but the parser
            // reports it exactly like `CSI T`; both scroll one line.
            // With more than one parameter `CSI T` is xterm's mouse
            // highlight tracking, not SD.
            'T' if params.len() > 1 => trace!(?params, "mouse highlight tracking ignored"),
            'T' | '^' => grid.scroll_down(count(params, 0)),
            'b' => self.repeat(count(params, 0)),
            'g' => match selector(params, 0) {
                0 => grid.clear_tab_stop(),
                3 => grid.clear_all_tab_stops(),
                mode => trace!(mode, "TBC mode ignored"),
            },
            'r' => self.set_scroll_region(params),
            'h' | 'l' if subparams != 0 => trace!(?params, "SM/RM with sub-parameters ignored"),
            'h' | 'l' => self.set_ansi_modes(params, action == 'h'),
            // SCOSC. #97: DECSLRM instead while DECLRMM (?69) is set.
            's' if no_params(params) => grid.save_cursor(),
            // SCORC: restores what DECRC restores.
            'u' if no_params(params) => grid.restore_cursor(),
            's' | 'u' => trace!(?action, ?params, "SCOSC/SCORC with parameters ignored"),
            'm' => self.sgr(params, subparams),
            _ => trace!(?action, ?params, "CSI ignored"),
        }
    }

    /// ED. Modes 0, 1 and 2 erase below, above and all. Mode 3 discards the
    /// scrollback without touching the screen or the pending wrap, as in
    /// xterm. Any other mode does nothing at all.
    fn erase_in_display(&mut self, mode: u16) {
        let mode = match mode {
            0 => EraseDisplay::Below,
            1 => EraseDisplay::Above,
            2 => EraseDisplay::All,
            3 => {
                // `erase_in_display(Scrollback)` would also disarm a pending
                // wrap, which xterm leaves alone.
                self.grid.clear_scrollback();
                return;
            }
            mode => {
                trace!(mode, "ED mode ignored");
                return;
            }
        };
        self.grid.erase_in_display(mode);
    }

    /// EL. Modes 0, 1 and 2 erase to the end, to the start and the whole
    /// line. Any other mode does nothing at all.
    fn erase_in_line(&mut self, mode: u16) {
        let mode = match mode {
            0 => EraseLine::ToEnd,
            1 => EraseLine::ToStart,
            2 => EraseLine::All,
            mode => {
                trace!(mode, "EL mode ignored");
                return;
            }
        };
        self.grid.erase_in_line(mode);
    }

    /// IL (`L`) and DL (`M`). Inside the scroll region they edit `n` lines
    /// and move the cursor to column zero (xterm `InsertLine` /
    /// `DeleteLine`). Outside it they do nothing, not even disarm a pending
    /// wrap.
    fn edit_lines(&mut self, action: char, n: u16) {
        let grid = &mut self.grid;
        if !grid.scroll_region().contains(grid.cursor().row) {
            return;
        }
        if action == 'L' {
            grid.insert_lines(n);
        } else {
            grid.delete_lines(n);
        }
        grid.carriage_return();
    }

    /// Whether `ch`, just printed, took a cell of its own: the cell it
    /// would have been written to (under the cursor while a wrap is pending,
    /// otherwise left of it, through a wide character's spacer) holds
    /// exactly `ch` and no cluster. A character that joined the previous
    /// cluster (a combining mark, a skin-tone modifier, the second regional
    /// indicator of a flag, a ZWJ tail, VS16) fails the check, and so does
    /// a zero-width or control character. A dropped character may pass when
    /// the neighbouring cell happens to hold the same character; repeating
    /// it then drops it again, which changes nothing.
    ///
    /// O(1), so printing pays nothing for REP; xterm likewise refuses a
    /// REP whose character has no width of its own.
    fn took_a_cell(&self, ch: char) -> bool {
        if !matches!(tessera_grid::char_width(ch), Some(1..)) {
            return false;
        }
        let grid = &self.grid;
        let cursor = grid.cursor();
        let Some(line) = grid.line(cursor.row) else {
            return false;
        };
        let col = usize::from(cursor.col);
        let col = if grid.pending_wrap() {
            Some(col)
        } else {
            col.checked_sub(1)
        };
        let col = match col {
            Some(col) if line.cells.get(col).is_some_and(Cell::is_spacer) => col.checked_sub(1),
            col => col,
        };
        col.and_then(|col| line.cells.get(col))
            .is_some_and(|cell| cell.ch == ch && !cell.is_cluster())
    }

    /// REP: prints the last printed character `n` more times through the
    /// normal print path, so auto-wrap, insert mode and pending wrap apply.
    /// A no-op when the previous action was not a printed character that
    /// took a cell of its own (see [`State::took_a_cell`]).
    /// Uncapped: costs `n` prints (see the module documentation).
    fn repeat(&mut self, n: u16) {
        let Some(ch) = self.last_printed.filter(|&ch| self.took_a_cell(ch)) else {
            return;
        };
        for _ in 0..n {
            self.print(ch);
        }
    }

    /// DECSTR (`CSI ! p`): the soft reset described in the crate
    /// documentation. Resets the pen, the modes in the `modes` module's
    /// list (insert mode, auto-wrap, origin mode, the cursor, the keyboard
    /// modes, …), the scroll region and the saved cursor; keeps the
    /// screen, the scrollback, the cursor position, a pending wrap and the
    /// tab stops. REP's remembered character is cleared by
    /// [`State::apply`], as after any sequence.
    fn soft_reset(&mut self) {
        // The whole pen, faint, strikeout and double underline included:
        // DEC, kitty and WezTerm; xterm keeps those three.
        self.grid.set_pen(Style::DEFAULT);
        // Origin mode goes off here, before the saved cursor is reset, so
        // a DECRC lands on the screen's home.
        self.soft_reset_modes();
        self.grid.reset_scroll_region();
        // xterm saves the cursor after the resets above and then homes the
        // copy; its pending wrap is cleared too (xterm copies the live one).
        // DECSC and SCOSC share this slot, so DECRC and SCORC both home.
        self.grid.reset_saved_cursor();
        // The program's cursor style (DECSCUSR and DECSET 12) gives way to
        // the configured one; `soft_reset_modes` cleared the blink record.
        // xterm's `ReallyReset` common part does the same for DECSTR, DECSCL
        // and RIS (ADR 0020).
        self.cursor_override = None;
        // #112, #95, #97, #9: reset here once the state exists:
        // G0-G3/GL/GR and DECNRCM to their defaults (SCS, #112), DECSCA off
        // (#95), DECLRMM off with full left/right margins and reverse wrap
        // (?45) to its default (#97), and the OSC 4 palette to the theme
        // (xterm's ResetAnsiColorRequest, #9).
    }

    /// DECSCUSR (`CSI Ps SP q`) with its first parameter `ps`: 1 and 2 a
    /// blinking and a steady block, 3 and 4 an underline, 5 and 6 a beam
    /// (xterm's bar). 0, an absent or empty first parameter and 7 (xterm's
    /// "initial resources") drop the program's style, so the configured
    /// default applies again (xterm and the VT510 manual make 0 a blinking
    /// block; ADR 0020). Any other value changes nothing. Every accepted
    /// value also records its blink bit in [`Modes::cursor_blink`], the one
    /// program-controlled blink bit DECSET 12 shares (0 and 7 clear it).
    ///
    /// [`Modes::cursor_blink`]: crate::Modes::cursor_blink
    fn set_cursor_style(&mut self, ps: u16) {
        let style = |shape, blink| Some(CursorStyle { shape, blink });
        let style = match ps {
            0 | 7 => None,
            1 => style(CursorShape::Block, true),
            2 => style(CursorShape::Block, false),
            3 => style(CursorShape::Underline, true),
            4 => style(CursorShape::Underline, false),
            5 => style(CursorShape::Beam, true),
            6 => style(CursorShape::Beam, false),
            _ => {
                trace!(ps, "DECSCUSR style ignored");
                return;
            }
        };
        self.cursor_override = style;
        self.modes.cursor_blink = style.is_some_and(|style| style.blink);
    }

    /// DECSCL (`CSI Pl ; Pc " p`), xterm's `CASE_DECSCL`. For `Pl` 61..=65
    /// runs the DECSTR `State::soft_reset`, sets the conformance level
    /// to `Pl - 60` and the C1 reply mode from `Pc`: 0 (or absent) and 2
    /// select 8-bit replies, 1 selects 7-bit, any other value keeps the
    /// mode, and level 1 always selects 7-bit. Any other `Pl` (absent, 0,
    /// 60, 66, …) does nothing at all. Parameters after `Pc` are ignored
    /// and nothing is answered.
    fn select_conformance_level(&mut self, params: &[u16]) {
        let level = params
            .first()
            .and_then(|&pl| pl.checked_sub(60))
            .and_then(|level| u8::try_from(level).ok())
            .filter(|level| (1..=5).contains(level));
        let Some(level) = level else {
            trace!(?params, "DECSCL with an unknown level ignored");
            return;
        };
        self.soft_reset();
        self.conformance_level = level;
        match (level, selector(params, 1)) {
            // VT100 mode has no 8-bit controls (DEC); xterm would keep the
            // previous mode here.
            (1, _) | (_, 1) => self.c1_8bit_replies = false,
            (_, 0 | 2) => self.c1_8bit_replies = true,
            (_, pc) => trace!(pc, "DECSCL C1 mode unknown, kept"),
        }
    }

    /// DECSTBM. The top defaults to the first row; the bottom defaults to
    /// the last row and so does any bottom beyond it. A region whose bottom
    /// is not below its top is ignored entirely, as in xterm: the old region
    /// stays, the cursor does not move and a pending wrap stays armed. A
    /// valid region homes the cursor.
    fn set_scroll_region(&mut self, params: &[u16]) {
        let rows = self.grid.rows();
        let top = count(params, 0);
        let bottom = match param_or(params, 1, rows) {
            bottom if bottom > rows => rows,
            bottom => bottom,
        };
        if bottom <= top {
            trace!(top, bottom, "DECSTBM with an empty region ignored");
            return;
        }
        self.grid.set_scroll_region(top - 1, bottom - 1);
        // Home: the region's top-left corner in origin mode.
        self.grid.goto_origin(0, 0);
    }
}

#[cfg(test)]
mod tests {
    use crate::Terminal;

    /// Whether REP would repeat `ch` after feeding `text` to a fresh
    /// terminal.
    fn took_a_cell(text: &str, ch: char, rows: u16, cols: u16, auto_wrap: bool) -> bool {
        let mut term = Terminal::new(rows, cols);
        term.set_auto_wrap(auto_wrap);
        term.advance(text.as_bytes());
        term.state.took_a_cell(ch)
    }

    #[test]
    fn a_character_with_its_own_cell_repeats() {
        assert!(took_a_cell("a", 'a', 1, 5, true));
        assert!(
            took_a_cell("abcde", 'e', 1, 5, true),
            "pending wrap: the cell under the cursor"
        );
        assert!(took_a_cell("中", '中', 1, 5, true), "through the spacer");
        assert!(
            took_a_cell("abc中", '中', 1, 5, true),
            "wide with a pending wrap"
        );
        assert!(took_a_cell("🇸", '🇸', 1, 5, true), "a lone indicator");
        assert!(
            took_a_cell("\u{17d8}", '\u{17d8}', 1, 5, true),
            "clamped to two columns"
        );
    }

    #[test]
    fn a_joined_or_dropped_character_does_not() {
        assert!(
            !took_a_cell("e\u{301}", '\u{301}', 1, 5, true),
            "zero width"
        );
        assert!(!took_a_cell("👍🏽", '🏽', 1, 5, true), "modifier joined");
        assert!(
            !took_a_cell("🇸🇪", '🇪', 1, 5, true),
            "second indicator joined"
        );
        assert!(
            !took_a_cell("🇸🇸", '🇸', 1, 5, true),
            "same indicator, but a cluster"
        );
        assert!(
            !took_a_cell("abcd中", '中', 1, 5, false),
            "dropped at the margin"
        );
        assert!(!took_a_cell("", 'a', 1, 5, true), "nothing printed");
    }
}
