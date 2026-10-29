//! Escape sequences (`ESC [intermediates] final`): the [`Action::Esc`]
//! half of the dispatcher.
//!
//! | Sequence | Name | Effect |
//! |---|---|---|
//! | `ESC 7` | DECSC | save cursor, pen, pending wrap and origin mode |
//! | `ESC 8` | DECRC | restore them (home, default pen and origin mode off without a save) |
//! | `ESC D` | IND | down one row, scrolling at the bottom margin |
//! | `ESC E` | NEL | IND plus carriage return |
//! | `ESC H` | HTS | set a tab stop at the cursor column |
//! | `ESC M` | RI | up one row, scrolling at the top margin |
//! | `ESC =` | DECKPAM | application keypad |
//! | `ESC >` | DECKPNM | numeric keypad |
//! | `ESC c` | RIS | full reset, scrollback and modes included |
//! | `ESC # 8` | DECALN | fill the screen with `E` |
//!
//! Any other escape sequence is ignored and logged at trace level. Expected
//! behaviour is pinned by `tests/fixtures/csi_spec.txt`.
//!
//! [`Action::Esc`]: tessera_vt::Action::Esc

use tessera_grid::Style;
use tracing::trace;

use crate::State;

impl State {
    /// Applies one escape sequence. See the module documentation for the
    /// sequences handled; anything else is logged and ignored.
    pub(crate) fn esc(&mut self, intermediates: &[u8], byte: u8) {
        match (intermediates, byte) {
            // Character sets are not saved yet: #112 extends the saved
            // state when it adds SCS.
            ([], b'7') => self.grid.save_cursor(),
            ([], b'8') => self.grid.restore_cursor(),
            ([], b'D') => self.grid.line_feed(),
            ([], b'E') => {
                self.grid.carriage_return();
                self.grid.line_feed();
            }
            ([], b'H') => self.grid.set_tab_stop(),
            ([], b'M') => self.grid.reverse_index(),
            ([], b'c') => self.reset(),
            ([], b'=') => self.set_application_keypad(true),
            ([], b'>') => self.set_application_keypad(false),
            ([b'#'], b'8') => self.screen_alignment(),
            _ => trace!(?intermediates, byte = ?char::from(byte), "ESC ignored"),
        }
    }

    /// RIS: the grid's reset plus what xterm also resets: the scrollback
    /// and every mode (see the `modes` module: all to their defaults, both
    /// kitty keyboard stacks emptied, 132-column mode left with a request for
    /// 80 columns).
    /// DECSTR (`CSI ! p`) is the soft variant that keeps the screen, see
    /// the crate documentation.
    fn reset(&mut self) {
        self.grid.reset();
        self.grid.clear_scrollback();
        self.reset_modes();
    }

    /// DECALN: resets the scroll region, fills every cell with an `E` in
    /// the default style (the pen is switched to the default for the fill
    /// and then restored) and homes the cursor.
    ///
    /// Each row is filled on its own, starting from column zero, so no row
    /// is marked soft-wrapped. Like ED 2, the fill replaces every line
    /// entirely: a soft-wrap flag or shell-integration mark from before is
    /// cleared.
    fn screen_alignment(&mut self) {
        let grid = &mut self.grid;
        let pen = grid.pen();
        grid.set_pen(Style::DEFAULT);
        grid.reset_scroll_region();
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
