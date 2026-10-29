//! Control sequences (`CSI ... final`): the [`Action::Csi`] half of the
//! dispatcher.
//!
//! Implemented, all without intermediates or a private marker:
//!
//! | Final | Name | Effect |
//! |---|---|---|
//! | `A` `B` `C` `D` | CUU CUD CUF CUB | move the cursor; CUU/CUD stop at the scroll margins |
//! | `E` `F` | CNL CPL | CUD / CUU, then column zero |
//! | `G` `` ` `` | CHA HPA | absolute column |
//! | `d` | VPA | absolute row |
//! | `H` `f` | CUP HVP | absolute position |
//! | `a` `e` | HPR VPR | relative position: right / down, clamped to the screen (VPR ignores the scroll region) |
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
//! One sequence with an intermediate is implemented: `CSI ! p` (DECSTR,
//! soft terminal reset, see the crate documentation). Its parameters are
//! ignored, as in xterm, so `CSI 5 ! p` resets too.
//!
//! Parameters follow xterm: an absent parameter and an explicit `0` both
//! mean the default, positions are 1-based and clamp to the screen (origin
//! mode does not exist yet, so CUP, HVP, CHA, HPA and VPA ignore the scroll
//! region), and every count saturates. Nothing here can panic on hostile
//! input.
//!
//! REP is not capped, as in xterm and Alacritty: a count up to 65535 costs
//! that many prints, each O(1) except in insert mode, where each one shifts
//! the rest of the line (O(count x cols)).
//!
//! HPR and VPR are xterm's `CursorSet(row, col + n)` and
//! `CursorSet(row + n, col)`: they clamp to the screen, not to the scroll
//! margins (CUD stops at the bottom margin, VPR does not), and clear a
//! pending wrap like every cursor move. Origin mode (#8) will make both
//! relative to and clamped by the region.
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
//! keyboard queries `CSI ? u`, `CSI > u`, `CSI < u` and `CSI = u` carry a
//! private marker and never reach SCORC.
//!
//! Everything else (an unassigned final, any private marker such as `?`,
//! any other intermediate such as `SP`, `$` or `"` (DECSCL is #92), and the
//! xterm extension SL/SR that is not in scope) is ignored and logged at
//! trace level. The parser hands over private markers as intermediates, so
//! one check covers both.
//!
//! Expected behaviour per sequence and edge case is pinned by
//! `tests/fixtures/csi_spec.txt`, and for DECSTR by
//! `tests/fixtures/decstr_spec.txt`.
//!
//! [`Action::Csi`]: tessera_vt::Action::Csi

use tessera_grid::{Cell, EraseDisplay, EraseLine, Style};
use tracing::trace;

use crate::State;

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
            if (intermediates, action) == (b"!", 'p') {
                // DECSTR. Parameters are ignored, as in xterm.
                self.soft_reset();
                return;
            }
            trace!(
                ?action,
                ?params,
                ?intermediates,
                "CSI with intermediates ignored"
            );
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
            'd' => grid.goto_row(count(params, 0) - 1),
            'H' | 'f' => grid.goto(count(params, 0) - 1, count(params, 1) - 1),
            // HPR and VPR: xterm `CursorSet`, clamped to the screen rather
            // than to the scroll margins. `goto` clamps and clears the
            // pending wrap.
            'a' => {
                let cursor = grid.cursor();
                grid.goto(cursor.row, cursor.col.saturating_add(count(params, 0)));
            }
            'e' => {
                let cursor = grid.cursor();
                grid.goto(cursor.row.saturating_add(count(params, 0)), cursor.col);
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
    /// documentation. Resets the pen, insert mode, auto-wrap, the scroll
    /// region and the saved cursor; keeps the screen, the scrollback, the
    /// cursor position, a pending wrap and the tab stops. REP's remembered
    /// character is cleared by [`State::apply`], as after any sequence.
    fn soft_reset(&mut self) {
        // The whole pen, faint, strikeout and double underline included:
        // DEC, kitty and WezTerm; xterm keeps those three.
        self.grid.set_pen(Style::DEFAULT);
        self.insert_mode = false;
        // xterm's resource default; DEC's table 5-9 says off.
        self.auto_wrap = true;
        self.grid.reset_scroll_region();
        // xterm saves the cursor after the resets above and then homes the
        // copy; its pending wrap is cleared too (xterm copies the live one).
        // DECSC and SCOSC share this slot, so DECRC and SCORC both home.
        self.grid.reset_saved_cursor();
        // #8: reset here: DECOM off, DECTCEM on, G0-G3/GL/GR and DECNRCM to
        // their defaults, DECCKM and DECKPAM off, KAM unlocked, DECSCA off,
        // DECLRMM off with full left/right margins, reverse wrap (?45) to
        // its default, modifyOtherKeys and the kitty keyboard flags to their
        // defaults (once Terminal owns KeyboardModes). Reset DECOM before
        // the saved cursor so a DECRC lands on the screen's home.
        // Not #8, same place: the cursor shape and blink (DECSCUSR,
        // `CSI Ps SP q`; no issue yet) to the default, and the OSC 4 palette
        // to the theme (xterm's ResetAnsiColorRequest; #9).
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
        // Origin mode does not exist yet, so home is the screen's corner.
        // #8 (DECOM) homes to the region's top instead when it is set.
        self.grid.goto(0, 0);
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
