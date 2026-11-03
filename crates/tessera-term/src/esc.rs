//! Escape sequences (`ESC [intermediates] final`): the [`Action::Esc`]
//! half of the dispatcher.
//!
//! | Sequence | Name | Effect |
//! |---|---|---|
//! | `ESC 7` | DECSC | save cursor, pen, pending wrap, origin mode and the character sets (G0 to G3, GL, GR) |
//! | `ESC 8` | DECRC | restore them (home, default pen, origin mode off and default sets without a save) |
//! | `ESC D` | IND | down one row, scrolling at the bottom margin |
//! | `ESC E` | NEL | IND plus carriage return |
//! | `ESC H` | HTS | set a tab stop at the cursor column |
//! | `ESC M` | RI | up one row, scrolling at the top margin |
//! | `ESC =` | DECKPAM | application keypad |
//! | `ESC >` | DECKPNM | numeric keypad |
//! | `ESC c` | RIS | full reset, scrollback and modes included |
//! | `ESC # 8` | DECALN | reset the margins, fill the screen with `E` |
//! | `ESC 6` | DECBI | back index: at the left margin the region's columns move right, else the cursor moves left (level 4 and up) |
//! | `ESC 9` | DECFI | forward index: at the right margin the region's columns move left, else the cursor moves right (level 4 and up) |
//! | `ESC Z` | DECID | identify: the DA1 reply (see the `reports` module) |
//! | `ESC SP F` | S7C1T | 7-bit replies, C1 controls in the input ignored (level 2 and up) |
//! | `ESC SP G` | S8C1T | 8-bit replies, C1 controls in the input honoured (level 2 and up) |
//! | `ESC ( F`, `ESC ) F`, `ESC * F`, `ESC + F` | SCS | designate a 94-character set into G0 to G3 |
//! | `ESC - F`, `ESC . F`, `ESC / F` | SCS | designate a 96-character set into G1 to G3 (level 3 and up) |
//! | `ESC n`, `ESC o` | LS2, LS3 | invoke G2 or G3 as GL (level 2 and up) |
//! | `ESC ~`, `ESC }`, `ESC \|` | LS1R, LS2R, LS3R | invoke G1, G2 or G3 as GR (level 2 and up) |
//! | `ESC N`, `ESC O` | SS2, SS3 | G2 or G3 for the next printed character (level 2 and up) |
//!
//! SO and SI (LS1, LS0) are C0 controls, see the `control` module; the
//! sets, the levels and DECNRCM are in the `charset` module.
//!
//! Any other escape sequence is ignored and logged at trace level. Expected
//! behaviour is pinned by `tests/fixtures/csi_spec.txt`.
//!
//! [`Action::Esc`]: tessera_vt::Action::Esc

use tessera_grid::Style;
use tracing::trace;

use crate::State;
use crate::charset::{self, Charsets};

impl State {
    /// Applies one escape sequence. See the module documentation for the
    /// sequences handled; anything else is logged and ignored.
    pub(crate) fn esc(&mut self, intermediates: &[u8], byte: u8) {
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => self.grid.line_feed(),
            // Index first, then the carriage return (xterm `CASE_NEL`): the
            // index scrolls only for a cursor between the left and right
            // margins, so the order matters there.
            ([], b'E') => {
                self.grid.line_feed();
                self.grid.carriage_return();
            }
            ([], b'H') => self.grid.set_tab_stop(),
            ([], b'M') => self.grid.reverse_index(),
            ([], b'c') => self.reset(),
            ([], b'=') => self.set_application_keypad(true),
            ([], b'>') => self.set_application_keypad(false),
            ([b'#'], b'8') => self.screen_alignment(),
            // DECBI and DECFI, VT420 and up (xterm `CASE_DECBI`/`DECFI`).
            ([], b'6') if self.conformance_level >= 4 => {
                let wrap = self.reverse_wrap();
                self.grid.back_index(wrap);
            }
            ([], b'9') if self.conformance_level >= 4 => self.grid.forward_index(),
            // DECID: answered exactly like DA1 (xterm).
            ([], b'Z') => self.primary_device_attributes(),
            // S7C1T and S8C1T: VT200 and up (xterm `vtXX_level >= 2`). At
            // level 1 they fall through to the ignored arm; xterm also
            // swallows the next byte there, which Tessera does not copy.
            ([b' '], b'F') if self.conformance_level >= 2 => self.c1_8bit_replies = false,
            ([b' '], b'G') if self.conformance_level >= 2 => self.c1_8bit_replies = true,
            // Locking and single shifts, VT220 and up (xterm: `ansi_level >
            // 2`, `vtXX_level > 1`).
            ([], b'n') if self.conformance_level >= 2 => self.charsets.shift_left(2),
            ([], b'o') if self.conformance_level >= 2 => self.charsets.shift_left(3),
            ([], b'~') if self.conformance_level >= 2 => self.charsets.shift_right(1),
            ([], b'}') if self.conformance_level >= 2 => self.charsets.shift_right(2),
            ([], b'|') if self.conformance_level >= 2 => self.charsets.shift_right(3),
            ([], b'N') if self.conformance_level >= 2 => self.charsets.single_shift(2),
            ([], b'O') if self.conformance_level >= 2 => self.charsets.single_shift(3),
            _ => self.select_charset(intermediates, byte),
        }
    }

    /// SCS: designates a set into G0 to G3 when the sequence is one xterm
    /// acts on at this level (see [`charset::designation`]), else logs it.
    fn select_charset(&mut self, intermediates: &[u8], byte: u8) {
        let level = self.conformance_level;
        let Some((g, set)) = charset::designation(level, self.modes.nrcs, intermediates, byte)
        else {
            trace!(?intermediates, byte = ?char::from(byte), "ESC ignored");
            return;
        };
        self.charsets.designate(g, set);
    }

    /// DECSC (`ESC 7`, `CSI s` without DECLRMM, DECSET 1048 and the first
    /// half of 1049): the grid's saved cursor and, in the active screen's
    /// slot, the character sets.
    pub(crate) fn save_cursor(&mut self) {
        self.grid.save_cursor();
        let slot = usize::from(self.grid.is_alt_screen());
        self.saved_charsets[slot] = Some(self.charsets.saved());
    }

    /// DECRC: the grid's, then the character sets saved for the active
    /// screen.
    pub(crate) fn restore_cursor(&mut self) {
        self.grid.restore_cursor();
        self.restore_charsets(usize::from(self.grid.is_alt_screen()));
    }

    /// Restores the sets saved in `slot`; without a save, as xterm, the
    /// default sets (and no single shift).
    pub(crate) fn restore_charsets(&mut self, slot: usize) {
        match self.saved_charsets[slot] {
            Some(saved) => self.charsets.restore(saved),
            None => self.charsets.reset(),
        }
    }

    /// RIS: the grid's reset plus what xterm also resets: the scrollback,
    /// the character sets and every mode (see the `modes` module: all to their defaults, both
    /// kitty keyboard stacks emptied, 132-column mode left with a request for
    /// 80 columns) and replies back to 7-bit C1 controls (xterm's
    /// `show_8bit_control(False)`), and every colour a program set (OSC 4,
    /// 10, 11, 12; xterm would keep 10 to 12). The DECSCL conformance level
    /// and the theme's colours are kept: xterm sets the level only at
    /// start-up.
    /// DECSTR (`CSI ! p`) is the soft variant that keeps the screen, see
    /// the crate documentation.
    fn reset(&mut self) {
        self.grid.reset();
        self.grid.clear_scrollback();
        self.reset_modes();
        // The configured cursor shape and blink apply again (DECSCUSR,
        // DECSET 12), and the blink record is cleared.
        self.drop_cursor_overrides();
        self.c1_8bit_replies = false;
        // The sets are the default, and so is the primary screen's saved
        // copy (xterm saves the cursor at the end of a full reset); the
        // alternate screen's slot has never been saved to.
        self.charsets.reset();
        self.saved_charsets = [Some(Charsets::default().saved()), None];
        // Every colour a program set (OSC 4, 10, 11, 12) goes; the theme
        // stays (ADR 0024: xterm keeps the dynamic colours here).
        self.colors.reset_all();
    }

    /// DECALN: resets the scroll region and the left and right margins
    /// (xterm `resetMargins`; DECLRMM itself stays), fills every cell with an `E` in
    /// the default style (the pen is switched to the default and the open
    /// hyperlink closed for the fill, and both restored) and homes the cursor.
    ///
    /// Each row is filled on its own, starting from column zero, so no row
    /// is marked soft-wrapped. Like ED 2, the fill replaces every line
    /// entirely: a soft-wrap flag or shell-integration mark from before is
    /// cleared.
    fn screen_alignment(&mut self) {
        let grid = &mut self.grid;
        let pen = grid.pen();
        let link = grid.hyperlink().cloned();
        grid.set_pen(Style::DEFAULT);
        grid.end_hyperlink();
        grid.reset_scroll_region();
        grid.reset_margins();
        for row in 0..grid.rows() {
            grid.goto(row, 0);
            for _ in 0..grid.cols() {
                grid.put_char('E');
            }
            if let Some(line) = grid.line_mut(row) {
                line.wrapped = false;
                line.marks.clear();
            }
        }
        grid.set_pen(pen);
        grid.set_hyperlink(link);
        grid.goto(0, 0);
    }
}

#[cfg(test)]
mod tests {
    use tessera_grid::Mark;

    use crate::Terminal;

    #[test]
    fn decaln_clears_marks_like_ed_2() {
        for seq in [&b"\x1b#8"[..], b"\x1b[2J"] {
            let mut term = Terminal::new(2, 3);
            for row in 0..2 {
                let line = term.state.grid.line_mut(row).expect("row exists");
                line.marks.push(Mark::PromptStart);
            }
            term.advance(seq);
            assert!(
                term.grid()
                    .visible_lines()
                    .iter()
                    .all(|line| line.marks.is_empty()),
                "{seq:?} left a mark"
            );
        }
    }
}
