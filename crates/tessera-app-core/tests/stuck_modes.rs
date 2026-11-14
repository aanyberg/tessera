//! Modes a program leaves on when it is killed (#461), on a real pty: a
//! session whose shell runs a program that turns on mouse tracking, focus
//! events, the kitty keyboard flags and the rest, and is killed with
//! `SIGKILL` before it can turn them off (what an ssh connection that
//! drops does to a remote tmux). The shell then writes its command end
//! mark (`OSC 133 ; D`) and a prompt, and reads raw input into a file:
//! mouse motion and focus changes must not reach it, and keys must come
//! as plain legacy keys again.
//!
//! [`tmux_killed_on_attach_leaves_the_shell_no_mode`] does the same with
//! the real tmux (`mouse on`, `focus-events on`), killing its client; it
//! is ignored, as it needs tmux. Run it with
//! `cargo nextest run -p tessera-app-core --test stuck_modes --run-ignored only --no-capture`.
//!
//! Waits are bounded by attempts (each sleeps [`POLL`]), not by a clock,
//! so a loaded machine stretches them instead of failing them.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use tessera_app_core::input::{
    FocusEvent, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, MouseEventKind,
};
use tessera_app_core::{
    App, Config, ConfigPaths, InputEvent, PtySize, SessionId, ShellIntegration, SpawnOptions,
};
use tessera_term::MouseTracking;

/// Pause between two polls.
const POLL: Duration = Duration::from_millis(10);
/// Polls before a wait gives up: 30 s on an idle machine.
const ATTEMPTS: usize = 3000;

/// What the shell does once the program is gone: the command end mark
/// with the killed program's status and the next prompt, as Tessera's
/// shell integration writes them, then raw input copied to the file `$1`.
const SHELL_TAIL: &str = "printf '\\033]133;D;137\\007\\033]133;A\\007'; \
stty raw -echo; printf 'ready> '; exec cat > \"$1\"";

/// Every mode the killed program turns on: the alternate screen,
/// any-motion mouse tracking in SGR, focus events, colour-scheme
/// updates, DECCKM and DECKPAM, a kitty flag, `modifyOtherKeys` and a
/// hidden cursor.
const PROGRAM_MODES: &str = "\\033[?1049h\\033[?1003h\\033[?1006h\\033[?1004h\\033[?2031h\
\\033[?1h\\033=\\033[>1u\\033[>4;2m\\033[?25l";

/// What the shell must read for [`send_inputs`]: no focus report, no
/// mouse report, Up as `CSI A` (no DECCKM), Escape as one ESC (no kitty
/// flags) and the `z` that ends the run.
const EXPECTED_INPUT: &[u8] = b"\x1b[A\x1bz";

fn app(root: &Path) -> App {
    App::new(Config::default(), ConfigPaths::new(root.join("config")))
}

/// Spawns `/bin/sh -c script sh out args...` at 24 x 80.
fn spawn(app: &App, script: &str, out: &Path, args: &[&str], env: &[(&str, &Path)]) -> SessionId {
    let mut options = SpawnOptions::command("/bin/sh", "stuck-modes")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(out.as_os_str())
        .args(args)
        .size(PtySize::new(24, 80));
    for (name, value) in env {
        options = options.env(name, value);
    }
    app.spawn_session(options).expect("spawn /bin/sh")
}

/// The visible rows as text.
fn screen(app: &App, id: SessionId) -> Vec<String> {
    app.with_terminal(id, |term| {
        term.grid()
            .visible_lines()
            .iter()
            .map(|line| line.text().trim_end().to_owned())
            .collect()
    })
    .unwrap_or_default()
}

/// Polls `done` until it holds; panics with `what` after [`ATTEMPTS`].
fn wait(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..ATTEMPTS {
        if done() {
            return;
        }
        thread::sleep(POLL);
    }
    assert!(done(), "never happened: {what}");
}

/// Waits for the shell's `ready>` prompt (raw input from then on).
fn wait_for_ready(app: &App, id: SessionId) {
    wait("the shell's ready prompt", || {
        screen(app, id).iter().any(|row| row.contains("ready>"))
    });
}

/// Sends focus gained, a mouse motion and a press over cell (5, 3), Up,
/// Escape and `z`, as the frontend sends them, and returns what the shell
/// read once the `z` arrived.
fn send_inputs(app: &App, id: SessionId, out: &Path) -> Vec<u8> {
    let send = |event| {
        let _ = app.send_input(id, event).expect("send input");
    };
    send(InputEvent::Focus(FocusEvent::Gained));
    send(InputEvent::Mouse(MouseEvent::new(
        MouseEventKind::Motion,
        MouseButton::None,
        5,
        3,
    )));
    send(InputEvent::Mouse(MouseEvent::new(
        MouseEventKind::Press,
        MouseButton::Left,
        5,
        3,
    )));
    send(InputEvent::Mouse(MouseEvent::new(
        MouseEventKind::Release,
        MouseButton::Left,
        5,
        3,
    )));
    send(InputEvent::Key(KeyEvent::new(Key::Up, Modifiers::NONE)));
    send(InputEvent::Key(KeyEvent::new(Key::Escape, Modifiers::NONE)));
    send(InputEvent::Key(
        KeyEvent::new(Key::Char('z'), Modifiers::NONE).with_text("z"),
    ));
    let read = || std::fs::read(out).unwrap_or_default();
    wait("the z reached the shell", || read().ends_with(b"z"));
    read()
}

/// The reproduction and the fix on a real pty: a program killed with its
/// modes on, then the shell's command end and prompt.
#[test]
fn a_program_killed_with_its_modes_on_leaves_the_shell_none_of_them() {
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());
    let out = root.path().join("input");
    let program = format!("printf '{PROGRAM_MODES}'; kill -9 $$");
    let script = format!("sh -c \"$2\"; {SHELL_TAIL}");
    let id = spawn(&app, &script, &out, &[&program], &[]);
    wait_for_ready(&app, id);
    let modes = app
        .with_terminal(id, tessera_term::Terminal::modes)
        .unwrap();
    let input = send_inputs(&app, id, &out);
    assert_eq!(
        String::from_utf8_lossy(&input),
        String::from_utf8_lossy(EXPECTED_INPUT)
    );
    assert_eq!(modes.mouse, MouseTracking::None, "{modes:?}");
    assert!(!modes.alt_screen && modes.cursor_visible, "{modes:?}");
    app.close_session(id).unwrap();
}

/// The real zsh with Tessera's shell integration: a program it runs
/// keeps the modes it turns on while it runs (the command end of the
/// command before it came first), and loses them at the command end zsh
/// writes once it has exited without turning them off. zsh's own
/// bracketed paste, which its line editor turns on after the command
/// end, stays on.
#[test]
fn zsh_keeps_a_running_programs_modes_and_drops_them_after_it() {
    let zsh = Path::new("/bin/zsh");
    if !zsh.is_file() {
        eprintln!("SKIP: no /bin/zsh");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join(".zshrc"), "PS1='prompt> '\n").unwrap();
    let mut config = Config::default();
    config.terminal.shell_integration = ShellIntegration::Auto;
    let app = App::new(config, ConfigPaths::new(root.path().join("config")));
    let options = SpawnOptions::command(zsh, "zsh")
        .arg("-i")
        .env("HOME", &home)
        .size(PtySize::new(24, 80));
    let id = app.spawn_session(options).expect("spawn zsh");
    let modes = || {
        app.with_terminal(id, tessera_term::Terminal::modes)
            .unwrap()
    };
    let shows = |text: &str| screen(&app, id).iter().any(|row| row.contains(text));
    app.write(id, b"echo ready-$((6 * 7))\r").unwrap();
    wait("zsh's first command", || shows("ready-42"));
    // The program prints its marker after turning the modes on, then
    // waits for a line.
    app.write(
        id,
        b"sh -c 'printf \"\\033[?1003h\\033[?1006h\\033[?1004h\\033[?1h\"; echo running-$((6*7)); read x'\r",
    )
    .unwrap();
    wait("the program runs", || shows("running-42"));
    let running = modes();
    assert_eq!(running.mouse, MouseTracking::AnyMotion, "{running:?}");
    assert!(running.focus_events, "{running:?}");
    app.write(id, b"\r").unwrap();
    app.write(id, b"echo done-$((6 * 7))\r").unwrap();
    wait("the next command after the program", || shows("done-42"));
    wait("zsh's line editor is back", || modes().bracketed_paste);
    let after = modes();
    assert_eq!(after.mouse, MouseTracking::None, "{after:?}");
    assert!(!after.focus_events, "{after:?}");
    app.close_session(id).unwrap();
}

/// Where `name` is on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// The real tmux, attached with `mouse on` and `focus-events on` and its
/// client killed with `SIGKILL`: prints the modes it turned on in
/// Tessera (the ground truth for what a dropped connection leaves), then
/// checks that the shell's command end turned them off.
#[test]
#[ignore = "live: runs tmux; not part of the gate"]
fn tmux_killed_on_attach_leaves_the_shell_no_mode() {
    let Some(tmux) = which("tmux") else {
        eprintln!("SKIP tmux: not installed (brew install tmux)");
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let app = app(root.path());
    let out = root.path().join("input");
    let conf = root.path().join("tmux.conf");
    std::fs::write(&conf, "set -g mouse on\nset -g focus-events on\n").unwrap();
    let socket = format!("tessera-461-{}", std::process::id());
    let script = format!(
        "\"$2\" -L \"$3\" -f \"$4\" new-session sh; printf 'dead> '; exec </dev/tty; head -c 1 >/dev/null; {SHELL_TAIL}"
    );
    let tmux_arg = tmux.to_string_lossy().into_owned();
    let conf_arg = conf.to_string_lossy().into_owned();
    let id = spawn(
        &app,
        &script,
        &out,
        &[&tmux_arg, &socket, &conf_arg],
        &[
            ("TMUX_TMPDIR", root.path()),
            ("HOME", root.path()),
            ("TERM", Path::new("xterm-256color")),
        ],
    );
    let tmux_cmd = |args: &[&str]| {
        Command::new(&tmux)
            .arg("-L")
            .arg(&socket)
            .args(args)
            .env("TMUX_TMPDIR", root.path())
            .env("HOME", root.path())
            .env_remove("TMUX")
            .output()
            .map_or_else(
                |_| String::new(),
                |out| String::from_utf8_lossy(&out.stdout).trim().to_owned(),
            )
    };
    wait("tmux turned mouse tracking on", || {
        app.with_terminal(id, |term| term.modes().mouse != MouseTracking::None)
            .unwrap_or(false)
    });
    // Let it finish its start-up (the replies to its queries).
    thread::sleep(Duration::from_secs(1));
    let (modes, keyboard) = app
        .with_terminal(id, |term| (term.modes(), term.keyboard_modes()))
        .unwrap();
    eprintln!("tmux on attach: {modes:?}\n{keyboard:?}");
    // A dropped connection: neither the server (which writes to the
    // client's tty, and resets it when a client goes away) nor the client
    // writes anything more. Stop the client, kill the server, then the
    // client.
    let client = tmux_cmd(&["list-clients", "-F", "#{client_pid}"]);
    let server = tmux_cmd(&["display", "-p", "#{pid}"]);
    for (signal, pid) in [("-STOP", &client), ("-KILL", &server), ("-KILL", &client)] {
        let status = Command::new("kill").args([signal, pid.as_str()]).status();
        assert!(
            status.is_ok_and(|status| status.success()),
            "kill {signal} {pid:?}"
        );
    }
    wait("the shell after tmux", || {
        screen(&app, id).iter().any(|row| row.contains("dead>"))
    });
    let stuck = app
        .with_terminal(id, |term| (term.modes(), term.keyboard_modes()))
        .unwrap();
    eprintln!("after the connection died: {stuck:?}");
    assert_ne!(stuck.0.mouse, MouseTracking::None, "the reproduction");
    app.write(id, b"x").unwrap();
    wait_for_ready(&app, id);
    let after = app
        .with_terminal(id, |term| (term.modes(), term.keyboard_modes()))
        .unwrap();
    eprintln!("after the command end: {after:?}");
    let input = send_inputs(&app, id, &out);
    let _ = tmux_cmd(&["kill-server"]);
    assert_eq!(
        String::from_utf8_lossy(&input),
        String::from_utf8_lossy(EXPECTED_INPUT)
    );
    app.close_session(id).unwrap();
}
