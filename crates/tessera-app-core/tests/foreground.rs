//! `App::foreground_process` on real sessions (issue #34): an interactive
//! `/bin/sh` at its prompt has no foreground job, `sleep 100` typed into it
//! is one until Ctrl-C, and a closed session is unknown.
//!
//! Waits are bounded by attempts (each sleeps [`POLL`]), not by a clock, so
//! a loaded machine stretches them instead of failing them.

use std::thread;
use std::time::Duration;

use tessera_app_core::{
    App, Config, ConfigPaths, Error, ForegroundProcess, PtySize, SessionId, SpawnOptions,
};

/// Pause between two polls.
const POLL: Duration = Duration::from_millis(10);
/// Polls before a wait for the job gives up: 10 s on an idle machine. The job shows up within
/// the first few polls there (the 2 s bar of #34); the rest is headroom
/// for a loaded machine, as the other wall-clock bars keep (#190).
const JOB_ATTEMPTS: usize = 1000;
/// Polls before a wait for output gives up: 30 s on an idle machine.
const OUTPUT_ATTEMPTS: usize = 3000;

fn app(root: &tempfile::TempDir) -> App {
    App::new(
        Config::default(),
        ConfigPaths::new(root.path().join("config")),
    )
}

/// An interactive `/bin/sh` that reads no start-up file, waited for until
/// it has run one command and printed its next prompt.
fn interactive_sh(app: &App) -> SessionId {
    let options = SpawnOptions::command("/bin/sh", "")
        .arg("-i")
        .env("ENV", "/dev/null")
        .env("PS1", "prompt> ")
        .size(PtySize::new(24, 80));
    let id = app.spawn_session(options).expect("spawn /bin/sh -i");
    app.write(id, b"echo ready-$((6 * 7))\n").unwrap();
    wait_for_screen(app, id, |rows| {
        rows.iter().any(|row| row.starts_with("ready-42"))
            && rows.last().is_some_and(|row| row == "prompt>")
    });
    id
}

/// The visible rows that hold text, trailing blanks trimmed.
fn screen(app: &App, id: SessionId) -> Vec<String> {
    let rows: Vec<String> = app
        .with_terminal(id, |term| {
            term.grid()
                .visible_lines()
                .iter()
                .map(|line| line.text().trim_end().to_owned())
                .collect()
        })
        .expect("live session");
    let used = rows
        .iter()
        .rposition(|row| !row.is_empty())
        .map_or(0, |i| i + 1);
    rows[..used].to_vec()
}

fn wait_for_screen(app: &App, id: SessionId, done: impl Fn(&[String]) -> bool) {
    for _ in 0..OUTPUT_ATTEMPTS {
        if done(&screen(app, id)) {
            return;
        }
        thread::sleep(POLL);
    }
    panic!("screen never matched: {:?}", screen(app, id));
}

/// Polls until `done` accepts the foreground job and returns it.
fn wait_for_job(
    app: &App,
    id: SessionId,
    done: impl Fn(&Option<ForegroundProcess>) -> bool,
) -> Option<ForegroundProcess> {
    let mut last = None;
    for _ in 0..JOB_ATTEMPTS {
        last = app.foreground_process(id).expect("live session");
        if done(&last) {
            return last;
        }
        thread::sleep(POLL);
    }
    panic!("foreground job still {last:?} after {JOB_ATTEMPTS} polls");
}

#[test]
fn a_shell_at_its_prompt_is_idle_and_a_running_command_is_busy() {
    let root = tempfile::tempdir().unwrap();
    let app = app(&root);
    let id = interactive_sh(&app);
    assert_eq!(app.foreground_process(id).unwrap(), None);

    app.write(id, b"sleep 100\n").unwrap();
    let job = wait_for_job(&app, id, |job| {
        job.as_ref().is_some_and(|job| job.name == "sleep")
    })
    .expect("a job");
    assert_ne!(job.pid, 0);

    app.write(id, b"\x03").unwrap();
    wait_for_job(&app, id, Option::is_none);

    app.close_session(id).unwrap();
    assert!(matches!(
        app.foreground_process(id),
        Err(Error::SessionNotFound(closed)) if closed == id
    ));
}

#[test]
fn a_session_whose_shell_exited_is_idle() {
    let root = tempfile::tempdir().unwrap();
    let app = app(&root);
    let id = interactive_sh(&app);
    app.write(id, b"exit 3\n").unwrap();
    // The session stays listed until it is closed; input is refused once
    // its child has been reaped.
    let mut exited = false;
    for _ in 0..OUTPUT_ATTEMPTS {
        exited = app.check_input_room(id, 1).is_err();
        if exited {
            break;
        }
        thread::sleep(POLL);
    }
    assert!(exited, "the shell never exited: {:?}", screen(&app, id));
    assert_eq!(app.foreground_process(id).unwrap(), None);
    app.close_session(id).unwrap();
}

#[test]
fn a_fake_pty_reports_no_job() {
    let root = tempfile::tempdir().unwrap();
    let app = app(&root);
    let id = app.spawn_test_session(PtySize::new(24, 80)).unwrap();
    assert_eq!(app.foreground_process(id).unwrap(), None);
    assert!(matches!(
        app.foreground_process(SessionId::from_raw(9999)),
        Err(Error::SessionNotFound(_))
    ));
}
