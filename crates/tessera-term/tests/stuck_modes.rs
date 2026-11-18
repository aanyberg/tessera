//! Modes a program leaves on when it dies without turning them off (#461).
//!
//! `ssh host` running `tmux` with `mouse on`: when the connection drops,
//! neither tmux nor ssh can send the sequences that turn tmux's modes off,
//! so the local shell's prompt would receive mouse motion, focus changes
//! and colour-scheme reports as escape-sequence text. The shell
//! integration's command end mark (`OSC 133 ; D`), which the shell writes
//! once the command it ran has ended, turns those input modes off (ADR
//! 0020, "Amendment: modes a dead program left on").
//!
//! [`TMUX_3_8_ATTACH`] is the mode part of what a real tmux 3.8 wrote on
//! attach with `mouse on` and `focus-events on`, captured from a pty with
//! `TERM=xterm-256color`; `crates/tessera-app-core/tests/stuck_modes.rs`
//! runs the real tmux in a session and kills its client.

use tessera_grid::Mark;
use tessera_term::{
    KeyboardModes, KittyFlags, ModifyOtherKeys, MouseEncoding, MouseReport, MouseReportButton,
    MouseReportKind, MouseTracking, Request, Terminal, encode_mouse,
};

/// The mode sequences tmux 3.8 wrote on attach, in order, with the
/// queries (DA1, DA2, XTVERSION, OSC 10/11, `CSI 14/18 t`, `CSI ? 996 n`)
/// and the drawing between them left out. Its final state: the alternate
/// screen, DECCKM and DECKPAM, bracketed paste, colour-scheme updates and
/// button-event mouse tracking in SGR encoding, cursor visible.
const TMUX_3_8_ATTACH: &[u8] = b"\x1b[?1049h\x1b[22;0;0t\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\
\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\
\x1b[?12l\x1b[?25h\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\
\x1b[?1006l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006h\x1b[?1000h\x1b[?1002h\
\x1b[?25l\x1b[?12l\x1b[?25h";

/// What tmux additionally turns on when the terminal's replies say it
/// supports them (focus events, extended keys), and what other programs
/// turn on: any-motion tracking, the kitty keyboard flags on the
/// alternate screen, `modifyOtherKeys` and a hidden cursor.
const MORE_MODES: &[u8] = b"\x1b[?1004h\x1b[?1003h\x1b[>1u\x1b[>4;2m\x1b[?25l";

/// What the local side writes once the connection is gone: ssh's message,
/// then the shell integration's command end (ssh's exit status 255) and
/// the next prompt.
const CONNECTION_LOST: &[u8] = b"Connection to host closed by remote host.\r\n";
const PROMPT_AFTER_SSH: &[u8] = b"\x1b]133;D;255\x07\x1b]133;A\x07$ \x1b]133;B\x07";

/// A mouse motion with no button over cell (5, 3): what moving the
/// pointer over the window sends while any-motion tracking is on.
fn motion() -> MouseReport {
    MouseReport::new(MouseReportKind::Motion, MouseReportButton::None, 5, 3)
}

/// A left-button press over cell (5, 3).
fn press() -> MouseReport {
    MouseReport::new(MouseReportKind::Press, MouseReportButton::Left, 5, 3)
}

/// What the terminal would send the child for a press and a motion.
fn mouse_bytes(term: &Terminal) -> Vec<Option<Vec<u8>>> {
    let modes = term.modes();
    [press(), motion()]
        .iter()
        .map(|report| encode_mouse(report, modes.mouse, modes.mouse_encoding))
        .collect()
}

/// The text of the first `n` visible rows, trailing blanks trimmed.
fn rows(term: &Terminal, n: usize) -> Vec<String> {
    term.grid().visible_lines()[..n]
        .iter()
        .map(|line| line.text().trim_end().to_owned())
        .collect()
}

/// A 10 x 40 terminal with `$ ssh host` on its primary screen.
fn shell() -> Terminal {
    let mut term = Terminal::new(10, 40);
    term.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ssh host\r\n\x1b]133;C\x07");
    term
}

/// The terminal's input modes are the defaults: nothing is reported to
/// the shell that it did not ask for.
#[track_caller]
fn assert_input_modes_off(term: &Terminal) {
    let modes = term.modes();
    assert_eq!(modes.mouse, MouseTracking::None, "mouse tracking");
    assert_eq!(
        modes.mouse_encoding,
        MouseEncoding::Default,
        "mouse encoding"
    );
    assert!(!modes.focus_events, "focus events");
    assert!(!modes.color_scheme_updates, "colour scheme updates");
    assert!(!modes.alt_screen, "alternate screen");
    assert!(modes.cursor_visible, "cursor visible");
    assert_eq!(
        term.keyboard_modes(),
        KeyboardModes::default(),
        "keyboard modes"
    );
    assert_eq!(mouse_bytes(term), [None, None]);
}

/// The reproduction: after the attach sequence and a dead connection
/// (nothing more from tmux), every mode tmux set is still on, and a
/// mouse press is sent to the shell as an SGR report. Without a command
/// end this is also what must stay true while tmux, vim or htop runs.
#[test]
fn modes_stay_on_while_no_command_end_arrives() {
    let mut term = shell();
    term.advance(TMUX_3_8_ATTACH);
    term.advance(CONNECTION_LOST);
    let modes = term.modes();
    assert_eq!(modes.mouse, MouseTracking::Button);
    assert_eq!(modes.mouse_encoding, MouseEncoding::Sgr);
    assert!(modes.alt_screen && modes.bracketed_paste && modes.color_scheme_updates);
    let keyboard = term.keyboard_modes();
    assert!(keyboard.application_cursor_keys && keyboard.application_keypad);
    assert_eq!(
        mouse_bytes(&term),
        [Some(b"\x1b[<0;6;4M".to_vec()), None],
        "a press reaches the shell as text; button-event tracking sends no plain motion"
    );
}

/// The fix: the shell's command end after the dead connection turns
/// every input mode tmux left on off, leaves the alternate screen and
/// shows the cursor. Bracketed paste is the shell's own business and
/// stays.
#[test]
fn the_command_end_after_a_dead_tmux_turns_its_modes_off() {
    let mut term = shell();
    term.advance(TMUX_3_8_ATTACH);
    term.advance(MORE_MODES);
    term.advance(CONNECTION_LOST);
    assert_eq!(term.modes().mouse, MouseTracking::AnyMotion);
    assert!(term.modes().focus_events && !term.modes().cursor_visible);
    assert_ne!(term.keyboard_modes().kitty_flags, KittyFlags::NONE);
    term.advance(PROMPT_AFTER_SSH);
    assert_input_modes_off(&term);
    assert!(term.modes().bracketed_paste, "left to the shell");
    // Back on the primary screen: `$ ssh host` shows again, ssh's
    // message is carried over from the alternate screen onto the line the
    // cursor was restored to, after ssh's output start, and the next
    // prompt and its marks are below it. The command end arrived on the
    // alternate screen and is recorded on the primary screen it took
    // back, on the prompt's line (#489).
    let grid = term.grid();
    assert_eq!(
        rows(&term, 4),
        [
            "$ ssh host",
            "Connection to host closed by remote host",
            ".",
            "$"
        ]
    );
    assert!(
        grid.visible_lines()[1].wrapped,
        "the message stays one line"
    );
    assert_eq!(grid.visible_lines()[1].marks, [Mark::OutputStart]);
    assert_eq!(
        grid.visible_lines()[3].marks,
        [
            Mark::CommandEnd {
                exit_code: Some(255)
            },
            Mark::PromptStart,
            Mark::CommandStart
        ]
    );
}

/// Each mode on its own, set on the primary screen by a program that
/// died, is off after the next command end.
#[test]
fn each_input_mode_is_turned_off_by_a_command_end() {
    let cases: &[(&str, &[u8])] = &[
        ("X10 mouse", b"\x1b[?9h"),
        ("normal mouse", b"\x1b[?1000h"),
        ("button mouse", b"\x1b[?1002h"),
        ("any-motion mouse", b"\x1b[?1003h"),
        ("UTF-8 encoding", b"\x1b[?1005h"),
        ("SGR encoding", b"\x1b[?1006h"),
        ("urxvt encoding", b"\x1b[?1015h"),
        ("SGR pixels encoding", b"\x1b[?1016h"),
        ("focus events", b"\x1b[?1004h"),
        ("colour scheme updates", b"\x1b[?2031h"),
        ("DECCKM", b"\x1b[?1h"),
        ("DECKPAM", b"\x1b="),
        ("DECNKM", b"\x1b[?66h"),
        ("modifyOtherKeys", b"\x1b[>4;2m"),
        ("kitty push", b"\x1b[>1u"),
        ("kitty set", b"\x1b[=31u"),
        ("hidden cursor", b"\x1b[?25l"),
        ("DECLRMM", b"\x1b[?69h"),
        ("DECLRMM with margins", b"\x1b[?69h\x1b[5;20s"),
        ("alternate screen 1049", b"\x1b[?1049h"),
        ("alternate screen 1047", b"\x1b[?1047h"),
        ("alternate screen 47", b"\x1b[?47h"),
    ];
    for (name, set) in cases {
        let mut term = shell();
        term.advance(set);
        assert_ne!(
            (term.modes(), term.keyboard_modes()),
            (shell().modes(), shell().keyboard_modes()),
            "{name}: the sequence sets something"
        );
        term.advance(b"\x1b]133;D;0\x07");
        assert_eq!(
            (term.modes(), term.keyboard_modes()),
            (shell().modes(), shell().keyboard_modes()),
            "{name}: off after the command end"
        );
        assert_eq!(
            term.grid().margins(),
            shell().grid().margins(),
            "{name}: margins at the full width"
        );
    }
}

/// Both kitty keyboard stacks are emptied: the main screen's, pushed by
/// a program that stayed there, and the alternate screen's, so the next
/// program that enters it starts with no flags.
#[test]
fn both_kitty_stacks_are_emptied() {
    let mut term = shell();
    term.advance(b"\x1b[>1u\x1b[?1049h\x1b[>3u\x1b]133;D\x07");
    assert_eq!(term.keyboard_modes().kitty_flags, KittyFlags::NONE, "main");
    term.advance(b"\x1b[?1049h");
    assert_eq!(
        term.keyboard_modes().kitty_flags,
        KittyFlags::NONE,
        "alternate"
    );
}

/// Every form of the command end resets; what is not a command end mark
/// (another kind, a malformed D, VS Code's 633) resets nothing.
#[test]
fn only_a_command_end_mark_resets() {
    for end in [
        &b"\x1b]133;D\x07"[..],
        b"\x1b]133;D;0\x07",
        b"\x1b]133;D;130\x1b\\",
        b"\x1b]133;D;1;aid=7\x07",
    ] {
        let mut term = shell();
        term.advance(b"\x1b[?1003h\x1b[?1006h");
        term.advance(end);
        assert_eq!(term.modes().mouse, MouseTracking::None, "{end:?}");
    }
    for other in [
        &b"\x1b]133;A\x07"[..],
        b"\x1b]133;B\x07",
        b"\x1b]133;C\x07",
        b"\x1b]133;D0\x07",
        b"\x1b]133;Dfoo\x07",
        b"\x1b]633;D;0\x07",
        b"\x1b]133;A;k=s\x07",
    ] {
        let mut term = shell();
        term.advance(b"\x1b[?1003h\x1b[?1006h");
        term.advance(other);
        assert_eq!(term.modes().mouse, MouseTracking::AnyMotion, "{other:?}");
    }
}

/// A program that runs at a prompt (vim, htop, a local tmux) gets the
/// prompt marks of the shell that started it before its own modes, and
/// none after: its modes stay, whatever marks came first.
#[test]
fn a_program_started_after_the_prompt_keeps_its_modes() {
    let mut term = shell();
    term.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07vim\r\n\x1b]133;C\x07");
    term.advance(b"\x1b[?1049h\x1b[?1h\x1b=\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[>1u\x1b[?2004h");
    term.advance(&b"~\r\n".repeat(30));
    let modes = term.modes();
    assert_eq!(
        (modes.mouse, modes.mouse_encoding),
        (MouseTracking::Button, MouseEncoding::Sgr)
    );
    assert!(modes.alt_screen && modes.focus_events && modes.bracketed_paste);
    let keyboard = term.keyboard_modes();
    assert!(keyboard.application_cursor_keys && keyboard.application_keypad);
    assert_eq!(keyboard.kitty_flags, KittyFlags::from_bits_truncate(1));
}

/// What a shell turns on for its own line editor after the command end
/// (zsh's `zle-line-init` running `echoti smkx`, readline's bracketed
/// paste, fish 4's kitty flags and `modifyOtherKeys`) stays on: the
/// reset is at the command end, before the next prompt is drawn.
#[test]
fn what_the_shell_turns_on_after_the_command_end_stays() {
    let mut term = shell();
    term.advance(b"\x1b[?1003h");
    term.advance(
        b"\x1b]133;D;0\x07\x1b]133;A\x07\x1b[?2004h\x1b[?1h\x1b=\x1b[=5u\x1b[>4;1m$ \x1b]133;B\x07",
    );
    let keyboard = term.keyboard_modes();
    assert!(keyboard.application_cursor_keys && keyboard.application_keypad);
    assert_eq!(keyboard.kitty_flags, KittyFlags::from_bits_truncate(5));
    assert_eq!(keyboard.modify_other_keys, ModifyOtherKeys::Level1);
    assert!(term.modes().bracketed_paste);
    assert_eq!(term.modes().mouse, MouseTracking::None);
}

/// The other modes are a program's to keep or a reset's to clear, not
/// the command end's: insert, origin, auto-wrap, LNM, reverse video,
/// bracketed paste and synchronized output stay as they were.
#[test]
fn other_modes_are_left_alone() {
    let mut term = shell();
    term.advance(b"\x1b[4h\x1b[20h\x1b[?5h\x1b[?7l\x1b[?2004h\x1b[?2026h");
    let before = term.modes();
    term.advance(b"\x1b]133;D;0\x07");
    assert_eq!(term.modes(), before);
}

/// A command end received on the alternate screen leaves it and restores
/// the cursor saved on entry, as `CSI ? 1049 l` would, then prints what
/// the alternate screen showed from there (its blank rows before the
/// first text and after the last left out) and puts the cursor at the
/// start of the line below it. The mark is then recorded on that line of
/// the primary screen, and reported once (#489, ADR 0039 N12).
#[test]
fn a_command_end_on_the_alternate_screen_leaves_it() {
    let mut term = shell();
    let cursor = term.grid().cursor();
    term.advance(b"\x1b[?1049h\x1b[5;5Hremote\x1b]133;D;255\x07");
    assert!(!term.modes().alt_screen);
    assert_eq!(rows(&term, 3), ["$ ssh host", "    remote", ""]);
    assert_eq!(
        (term.grid().cursor().row, term.grid().cursor().col),
        (cursor.row + 1, 0)
    );
    let marks: Vec<Request> = term
        .take_requests()
        .into_iter()
        .filter(|request| {
            matches!(
                request,
                Request::Mark {
                    mark: Mark::CommandEnd { .. },
                    ..
                }
            )
        })
        .collect();
    let end = Mark::CommandEnd {
        exit_code: Some(255),
    };
    assert_eq!(
        marks,
        [Request::Mark {
            mark: end.clone(),
            line: u64::from(cursor.row) + 1
        }]
    );
    assert_eq!(term.grid().visible_lines()[2].marks, [end]);
}

/// S6: a flood of command ends, each after a program's modes and an
/// alternate screen, queues one coalesced command end mark (each is
/// recorded on the primary screen it took back, #489) and nothing else,
/// and leaves no mode on.
#[test]
fn a_flood_of_command_ends_queues_nothing() {
    let mut term = shell();
    let _ = term.take_requests();
    let unit = b"\x1b[?1049h\x1b[?1003h\x1b[?1006h\x1b[>1uremote\x1b]133;D;1\x07";
    term.advance(&unit.repeat(20_000));
    let requests = term.take_requests();
    assert!(
        matches!(
            requests.as_slice(),
            [Request::Mark {
                mark: Mark::CommandEnd { exit_code: Some(1) },
                ..
            }]
        ),
        "{requests:?}"
    );
    assert_input_modes_off(&term);
}

/// A program that crashed on the alternate screen (a TUI that panicked
/// without restoring the terminal) keeps its last screen and the messages
/// after it, its own panic and the shell's job status, above the next
/// prompt: they are printed on the primary screen from the cursor saved
/// on entry, with their characters, clusters and colours, and the prompt
/// starts below them in the pen it had before.
#[test]
fn a_crashed_programs_screen_and_messages_survive_the_command_end() {
    let mut term = Terminal::new(10, 50);
    term.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07./tui\r\n\x1b]133;C\x07");
    term.advance(b"\x1b[?1049h\x1b[?1003h\x1b[H\x1b[2J\x1b[1;31m\xe4\xb8\xad\xe6\x96\x87 e\xcc\x81 frame\x1b[m");
    term.advance(b"\x1b[3;1Hthread 'main' panicked at src/main.rs:3:5\r\n");
    term.advance(b"zsh: segmentation fault  ./tui\r\n");
    term.advance(b"\x1b]133;D;139\x07\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_input_modes_off(&term);
    assert_eq!(
        rows(&term, 7),
        [
            "$ ./tui",
            "\u{4e2d}\u{6587} e\u{301} frame",
            "",
            "thread 'main' panicked at src/main.rs:3:5",
            "zsh: segmentation fault  ./tui",
            "$",
            ""
        ]
    );
    let grid = term.grid();
    let frame = &grid.visible_lines()[1];
    assert!(frame.cells[0].is_wide() && frame.cells[1].is_spacer());
    assert_eq!(frame.cells[0].style.fg, tessera_grid::Color::Indexed(1));
    assert_eq!(
        frame.cells[0].style.flags & tessera_grid::Style::BOLD,
        tessera_grid::Style::BOLD
    );
    let prompt = &grid.visible_lines()[5];
    assert_eq!(
        prompt.cells[0].style,
        tessera_grid::Style::default(),
        "the pen is the shell's"
    );
    assert_eq!((grid.cursor().row, grid.cursor().col), (5, 2));
}

/// A program that entered the alternate screen with the cursor in the
/// middle of a line gets its screen printed from the start of the next
/// line, not over the end of that one.
#[test]
fn the_carried_text_starts_on_a_line_of_its_own() {
    let mut term = shell();
    term.advance(b"partial");
    term.advance(b"\x1b[?1049h\x1b[Hgone\x1b]133;D;1\x07");
    assert_eq!(rows(&term, 4), ["$ ssh host", "partial", "gone", ""]);
    assert_eq!((term.grid().cursor().row, term.grid().cursor().col), (3, 0));
}

/// A full alternate screen is carried whole: the lines it pushes off the
/// primary screen go to the scrollback in order, and nothing more than
/// the screen's height is printed.
#[test]
fn a_full_alternate_screen_scrolls_into_the_scrollback() {
    let mut term = shell();
    term.advance(b"\x1b[?1049h\x1b[H");
    for n in 0..10 {
        term.advance(format!("\x1b[{};1Hrow {n}", n + 1).as_bytes());
    }
    term.advance(b"\x1b]133;D;1\x07");
    let grid = term.grid();
    let text: Vec<String> = grid
        .logical_lines()
        .map(|line| line.text().trim_end().to_owned())
        .collect();
    let mut expected = vec!["$ ssh host".to_owned()];
    expected.extend((0..10).map(|n| format!("row {n}")));
    expected.push(String::new());
    assert_eq!(text, expected);
    assert_eq!(grid.scrollback_len(), 2);
}

/// An alternate-screen program that exits normally (vim, htop, a local
/// tmux detaching) leaves it itself before the shell's command end: the
/// command end then finds the primary screen and changes no text.
#[test]
fn a_program_that_left_the_alternate_screen_itself_is_unchanged() {
    let run = b"\x1b[?1049h\x1b[?1002h\x1b[Hvim's screen\r\n~\r\n~\x1b[?1002l\x1b[?1049l";
    let mut with_end = shell();
    with_end.advance(run);
    with_end.advance(b"\x1b]133;D;0\x07");
    let mut without = shell();
    without.advance(run);
    assert_eq!(rows(&with_end, 10), rows(&without, 10));
    assert_eq!(with_end.grid().cursor(), without.grid().cursor());
    assert!(rows(&with_end, 10).iter().all(|row| !row.contains("vim")));
}

/// The last carried row ends its line even when it is a full soft-wrapped
/// row whose continuation is blank, and the pen afterwards is the one the
/// leave restored, not the carried text's: the next prompt starts on a
/// line of its own in the shell's colours.
#[test]
fn the_last_carried_row_ends_its_line_in_the_shells_pen() {
    let mut term = shell();
    let pen = term.grid().pen();
    term.advance(b"\x1b[?1049h\x1b[H\x1b[31m");
    term.advance(&[b'x'; 40]);
    term.advance(b"\x1b[m \x1b[31m\x1b]133;D;1\x07");
    let full = "x".repeat(40);
    assert_eq!(rows(&term, 3), ["$ ssh host", full.as_str(), ""]);
    let grid = term.grid();
    assert_eq!((grid.cursor().row, grid.cursor().col), (2, 0));
    assert!(!grid.pending_wrap());
    assert!(
        !grid.visible_lines()[1].wrapped,
        "the prompt is no continuation"
    );
    assert_eq!(grid.pen(), pen);
}

/// The text of every line, scrollback first, trailing blanks trimmed.
fn all_lines(term: &Terminal) -> Vec<String> {
    term.grid()
        .logical_lines()
        .map(|line| line.text().trim_end().to_owned())
        .collect()
}

/// A dead program's scroll region (`CSI 1 ; 9 r` on a 10-row screen, as
/// tmux sets for each scroll burst) does not hold the carried text: with
/// the shell's cursor below the region, every carried row gets a line of
/// its own instead of overwriting the last one, the command line stays,
/// and the rows scroll into the scrollback.
#[test]
fn a_stale_scroll_region_above_the_cursor_loses_no_line() {
    let mut term = Terminal::new(10, 40);
    for n in 0..8 {
        term.advance(format!("hist {n}\r\n").as_bytes());
    }
    term.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ssh host\r\n\x1b]133;C\x07");
    assert_eq!(term.grid().cursor().row, 9);
    term.advance(b"\x1b[?1049h\x1b[1;9r");
    for n in 0..9 {
        term.advance(format!("\x1b[{};1Hremote {n}", n + 1).as_bytes());
    }
    term.advance(b"\x1b[10;1HConnection closed.\x1b]133;D;255\x07");
    let mut expected: Vec<String> = (0..8).map(|n| format!("hist {n}")).collect();
    expected.push("$ ssh host".to_owned());
    expected.extend((0..9).map(|n| format!("remote {n}")));
    expected.push("Connection closed.".to_owned());
    expected.push(String::new());
    assert_eq!(all_lines(&term), expected);
    let grid = term.grid();
    assert_eq!(grid.scrollback_len(), 10);
    assert_eq!((grid.cursor().row, grid.cursor().col), (9, 0));
    assert_eq!(
        (grid.scroll_region().top, grid.scroll_region().bottom),
        (0, 9)
    );
}

/// With the shell's cursor inside a dead program's scroll region, the
/// carried rows scroll the whole screen, not the partial region whose
/// rows would be discarded: the primary screen's history and the command
/// line go to the scrollback with the carried text after them.
#[test]
fn a_stale_scroll_region_around_the_cursor_loses_no_line() {
    let mut term = Terminal::new(10, 40);
    for n in 0..4 {
        term.advance(format!("hist {n}\r\n").as_bytes());
    }
    term.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ssh host\r\n\x1b]133;C\x07");
    assert_eq!(term.grid().cursor().row, 5);
    term.advance(b"\x1b[?1049h\x1b[1;9r");
    for n in 0..10 {
        term.advance(format!("\x1b[{};1Hremote {n}", n + 1).as_bytes());
    }
    term.advance(b"\x1b]133;D;255\x07");
    let mut expected: Vec<String> = (0..4).map(|n| format!("hist {n}")).collect();
    expected.push("$ ssh host".to_owned());
    expected.extend((0..10).map(|n| format!("remote {n}")));
    expected.push(String::new());
    assert_eq!(all_lines(&term), expected);
    let grid = term.grid();
    assert_eq!(grid.scrollback_len(), 6);
    assert_eq!((grid.cursor().row, grid.cursor().col), (9, 0));
}

/// The background of every cell of a visible row.
fn backgrounds(term: &Terminal, row: usize) -> Vec<tessera_grid::Color> {
    term.grid().visible_lines()[row]
        .cells
        .iter()
        .map(|cell| cell.style.bg)
        .collect()
}

/// The carry scrolls in rows in the shell's colours: tmux's green status
/// bar, the last row carried, does not fill the row the line break after
/// it scrolls in (BCE), so the next prompt has the default background,
/// while the status bar keeps its green, blanks included (#489: a blank
/// in a background other than the shell's is not trimmed).
#[test]
fn a_row_the_carry_scrolls_in_has_the_shells_background() {
    let green = tessera_grid::Color::Indexed(2);
    let mut term = Terminal::new(4, 20);
    term.advance(b"$ ssh host\r\n");
    term.advance(b"\x1b[?1049h\x1b[H\x1b[2Jremote$\x1b[4;1H\x1b[42m[0] 0:zsh*\x1b[K\x1b[m");
    term.advance(b"\x1b]133;D;255\x07$ ");
    assert_eq!(
        all_lines(&term),
        ["$ ssh host", "remote$", "", "", "[0] 0:zsh*", "$"]
    );
    // The whole status bar keeps its green: its trailing blanks are not
    // in the shell's background, so they are not trimmed.
    assert_eq!(backgrounds(&term, 2), vec![green; 20], "the status bar");
    assert_eq!(
        backgrounds(&term, 3),
        vec![tessera_grid::Color::Default; 20],
        "the prompt row"
    );
}

/// A coloured row followed by a plain one: the plain row and the blanks
/// after the coloured text carry no colour of their own, and the coloured
/// cells keep theirs.
#[test]
fn a_coloured_row_does_not_bleed_into_the_next() {
    let blue = tessera_grid::Color::Indexed(4);
    let mut term = shell();
    term.advance(b"\x1b[?1049h\x1b[H\x1b[44mblue\x1b[m\r\nplain\x1b]133;D;1\x07");
    assert_eq!(rows(&term, 4), ["$ ssh host", "blue", "plain", ""]);
    let blue_row = backgrounds(&term, 1);
    assert_eq!(blue_row[..4], [blue; 4]);
    assert!(
        blue_row[4..]
            .iter()
            .all(|bg| *bg == tessera_grid::Color::Default)
    );
    assert!(
        backgrounds(&term, 2)
            .iter()
            .all(|bg| *bg == tessera_grid::Color::Default)
    );
}

/// A coloured soft-wrapped row whose wrap scrolls at the bottom of the
/// screen: the row scrolled in for its continuation takes the shell's
/// background past the carried text, as after a line break.
#[test]
fn a_wrap_the_carry_scrolls_in_has_the_shells_background() {
    let red = tessera_grid::Color::Indexed(1);
    let mut term = Terminal::new(2, 10);
    term.advance(b"$ ssh\r\n");
    term.advance(b"\x1b[?1049h\x1b[H\x1b[41mxxxxxxxxxxy\x1b[m\x1b]133;D;1\x07");
    assert_eq!(all_lines(&term), ["$ ssh", "xxxxxxxxxx", "y", ""]);
    let continuation = &term.grid().visible_lines()[0];
    assert_eq!(continuation.cells[0].style.bg, red, "the carried cell");
    assert!(
        continuation.cells[1..]
            .iter()
            .all(|cell| cell.style.bg == tessera_grid::Color::Default),
        "{:?}",
        continuation
            .cells
            .iter()
            .map(|cell| cell.style.bg)
            .collect::<Vec<_>>()
    );
}

/// #489 (#461 r2-M1): the carried cells are moved, not printed again, so
/// a cell that printing would merge with its neighbour stays where the
/// program put it: a skin-tone modifier placed by cursor addressing next
/// to an emoji, a combining mark on a cell of its own, and a hyperlink,
/// with their clusters and styles.
#[test]
fn the_carried_cells_are_the_alternate_screens_own() {
    let mut term = shell();
    term.advance("\x1b[?1049h\x1b[H\u{1f44d}\x1b[1;3H\x1b[32m\u{1f3fd}\x1b[m".as_bytes());
    term.advance("\x1b[1;6He\u{301}\x1b[1;8H\u{301}x".as_bytes());
    term.advance(b"\x1b[1;12H\x1b]8;;https://a.example/\x1b\\link\x1b]8;;\x1b\\");
    let alt = term.grid().visible_lines()[0].clone();
    term.advance(b"\x1b]133;D;1\x07");
    let carried = &term.grid().visible_lines()[1];
    assert_eq!(carried.cells, alt.cells);
    assert_eq!(carried.graphemes, alt.graphemes);
    assert_eq!(carried.text(), alt.text());
    assert_eq!(rows(&term, 3)[2], "");
}

/// #489 (#461 r2-M2): a soft-wrapped row that is not full, because the
/// wide character after it did not fit in the last column, stays wrapped
/// with its pad, so the carried rows are still one line.
#[test]
fn a_padded_soft_wrap_stays_wrapped() {
    let mut term = Terminal::new(4, 10);
    term.advance(b"$ ssh\r\n");
    term.advance("\x1b[?1049h\x1b[Habcdefghi\u{4e2d}".as_bytes());
    assert!(term.grid().visible_lines()[0].wrapped);
    term.advance(b"\x1b]133;D;1\x07");
    let grid = term.grid();
    let row = &grid.visible_lines()[1];
    assert!(row.wrapped, "the row continues");
    assert!(row.cells[9].is_pad(), "its pad is kept");
    assert!(
        !grid.visible_lines()[2].wrapped,
        "the last carried row ends"
    );
    assert_eq!(rows(&term, 4), ["$ ssh", "abcdefghi", "\u{4e2d}", ""]);
}

/// #489 (#461 adv-M2): the primary screen is cleared from the restored
/// cursor to its end before the carry: the rest of the cursor's line
/// (its mark kept) and the stale rows below it do not show between or
/// after the carried text, and the stale marks go with them, on the row
/// a carried row lands on too.
#[test]
fn the_carry_clears_from_the_restored_cursor_first() {
    let mut term = shell();
    term.advance(
        b"\x1b[3;1H\x1b]133;A\x07\x1b[5;1H\x1b]133;A\x07stale prompt\x1b[7;1Hstale output",
    );
    term.advance(b"\x1b[2;1Hpartial-stale\x1b[2;8H");
    term.advance(b"\x1b[?1049h\x1b[Hgone\x1b]133;D;1\x07");
    assert_eq!(
        rows(&term, 10),
        ["$ ssh host", "partial", "gone", "", "", "", "", "", "", ""]
    );
    let grid = term.grid();
    assert_eq!(grid.visible_lines()[1].marks, [Mark::OutputStart]);
    assert!(
        grid.visible_lines()[2..]
            .iter()
            .all(|line| line.marks.is_empty()
                || line.marks == [Mark::CommandEnd { exit_code: Some(1) }]),
        "the stale prompt marks are gone"
    );
    assert_eq!((grid.cursor().row, grid.cursor().col), (3, 0));
    assert_eq!(term.screen_clears(), 0, "the carry's clear is no ED");
}

/// #489: with the restored cursor's wrap pending, its last character
/// stays (no clear erases it) and the carry starts on the next line.
#[test]
fn a_pending_wrap_keeps_the_last_character() {
    let mut term = Terminal::new(4, 10);
    term.advance(b"0123456789");
    assert!(term.grid().pending_wrap());
    term.advance(b"\x1b[?1049h\x1b[Hgone\x1b]133;D;1\x07");
    assert_eq!(rows(&term, 4), ["0123456789", "gone", "", ""]);
    assert!(!term.grid().visible_lines()[0].wrapped);
}

/// A 200 x 500 alternate screen of text after `$ tui`, left by the
/// program itself (`leave`, DECRST 1049) or not, then a command end.
fn full_screen_unit(leave: bool) -> Terminal {
    let mut term = Terminal::new(200, 500);
    term.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07tui\r\n\x1b]133;C\x07\x1b[?1049h");
    let mut fill = Vec::new();
    for row in 1..=200 {
        fill.extend(format!("\x1b[{row};1H").as_bytes());
        fill.extend(b"0123456789".repeat(50));
    }
    term.advance(&fill);
    if leave {
        term.advance(b"\x1b[?1049l");
    }
    term.advance(b"\x1b]133;D;0\x07");
    term
}

/// #489 (#461 r2-M3): the carry costs at most 1.1 times the same unit
/// without it (a 200 x 500 alternate screen drawn, then a command end
/// after the program left it itself), the best of 15 runs each,
/// interleaved: the rows are moved, not cloned and printed again (2.3
/// times before).
#[test]
#[cfg_attr(debug_assertions, ignore = "timing: release only")]
fn the_carry_of_a_full_screen_costs_a_tenth_more_at_most() {
    let best = |leave: bool, best: &mut std::time::Duration| {
        let start = std::time::Instant::now();
        let term = full_screen_unit(leave);
        *best = (*best).min(start.elapsed());
        assert!(!term.modes().alt_screen);
    };
    let (mut carried, mut left) = (std::time::Duration::MAX, std::time::Duration::MAX);
    for _ in 0..15 {
        best(false, &mut carried);
        best(true, &mut left);
    }
    assert!(
        carried.as_secs_f64() <= 1.1 * left.as_secs_f64(),
        "{carried:?} with the carry, {left:?} without"
    );
}
