//! The `tessera` terminfo entry's overline capabilities (#110): tmux reads
//! the user-defined `Smol` (ncurses spells the pair `Smol`/`Rmol`), so the
//! entry must carry exactly those names, and their strings must do what the
//! names promise on a [`Terminal`].

use tessera_grid::{Point, Style};
use tessera_term::Terminal;

const TERMINFO: &str = include_str!("../../../shell-integration/terminfo/tessera.terminfo");

/// The capabilities of terminfo source `text`: split at every comma, with
/// the white space around each dropped, so one at the end of a line or
/// after a tab is found like one in the middle.
fn caps(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .flat_map(|line| line.split(','))
        .map(str::trim)
}

/// The string of capability `name` (case-sensitive) in `text`, `\E`
/// decoded to ESC.
fn cap_in(text: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    caps(text)
        .find_map(|c| c.strip_prefix(prefix.as_str()))
        .map(|value| value.replace("\\E", "\x1b"))
}

/// The string of capability `name` in the shipped entry.
fn cap(name: &str) -> String {
    cap_in(TERMINFO, name).unwrap_or_else(|| panic!("tessera.terminfo has no {name}"))
}

fn overline_at(term: &Terminal, col: u16) -> bool {
    let cell = term
        .grid()
        .cell(Point { row: 0, col })
        .expect("cell on screen");
    cell.style.flags & Style::OVERLINE != 0
}

/// #526 (24.7): where a capability sits on its line does not matter: last
/// on the line, after a tab, or between two others; a comment is not read.
#[test]
fn a_capability_is_found_wherever_it_sits_on_its_line() {
    let text = "#\tHidden=\\E[0m,\n\tfirst=1, Smol=\\E[53m,\n\tRmol=\\E[55m,\tlast=2,\n";
    assert_eq!(
        cap_in(text, "Smol").as_deref(),
        Some("\x1b[53m"),
        "line end"
    );
    assert_eq!(
        cap_in(text, "Rmol").as_deref(),
        Some("\x1b[55m"),
        "line start"
    );
    assert_eq!(cap_in(text, "last").as_deref(), Some("2"), "after a tab");
    assert_eq!(cap_in(text, "Hidden"), None, "a comment");
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
        let found = caps(TERMINFO).any(|c| c.starts_with(prefix.as_str()));
        assert!(!found, "{name} is not read by any consumer (#110)");
    }
}
