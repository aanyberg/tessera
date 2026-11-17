//! The `tessera` terminfo entry's overline capabilities (#110): tmux reads
//! the user-defined `Smol` (ncurses spells the pair `Smol`/`Rmol`), so the
//! entry must carry exactly those names, and their strings must do what the
//! names promise on a [`Terminal`].

use tessera_grid::{Point, Style};
use tessera_term::Terminal;

const TERMINFO: &str = include_str!("../../../shell-integration/terminfo/tessera.terminfo");

/// The string of capability `name` (case-sensitive), `\E` decoded to ESC.
fn cap(name: &str) -> String {
    let prefix = format!("{name}=");
    TERMINFO
        .lines()
        .filter(|line| !line.starts_with('#'))
        .flat_map(|line| line.split(", "))
        .find_map(|c| c.trim().strip_prefix(prefix.as_str()))
        .unwrap_or_else(|| panic!("tessera.terminfo has no {name}"))
        .trim_end_matches(',')
        .replace("\\E", "\x1b")
}

fn overline_at(term: &Terminal, col: u16) -> bool {
    let cell = term
        .grid()
        .cell(Point { row: 0, col })
        .expect("cell on screen");
    cell.style.flags & Style::OVERLINE != 0
}

#[test]
fn smol_and_rmol_set_and_clear_overline() {
    let smol = cap("Smol");
    let rmol = cap("Rmol");
    assert_eq!(smol, "\x1b[53m");
    assert_eq!(rmol, "\x1b[55m");
    let mut term = Terminal::new(2, 10);
    term.advance(smol.as_bytes());
    term.advance(b"a");
    term.advance(rmol.as_bytes());
    term.advance(b"b");
    assert!(overline_at(&term, 0), "Smol sets overline");
    assert!(!overline_at(&term, 1), "Rmol clears overline");
}

#[test]
fn lower_case_overline_names_are_not_shipped() {
    for name in ["smol", "rmol"] {
        let prefix = format!("{name}=");
        let found = TERMINFO
            .lines()
            .filter(|line| !line.starts_with('#'))
            .flat_map(|line| line.split(", "))
            .any(|c| c.trim().starts_with(prefix.as_str()));
        assert!(!found, "{name} is not read by any consumer (#110)");
    }
}
