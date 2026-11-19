//! Closing a session ends a session leader that exits while the terminal's
//! output is stopped (#562, adversarial B1).
//!
//! On macOS the parent holds a descriptor on the slave side so a fast
//! child's last output is not discarded (see `exit_output.rs`). A leader
//! that exits with output queued on a stopped terminal (`Ctrl-S` with
//! `IXON`, or its own `tcflow(TCOOFF)`) then waits in its exit for a drain
//! that never comes, and `SIGKILL` cannot touch a process that is already
//! exiting. These tests put a directly exec'd leader (perl, `zsh -f`) in
//! that state and check that `kill`, `hangup` and dropping the handle each
//! end the session: `Exited` arrives, the child is reaped, the reader and
//! writer threads finish and the descriptors are released. Every wait has a
//! deadline, so a regression fails instead of hanging.

#![cfg(target_os = "macos")]

use std::process::Command;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::{Duration, Instant};

use tessera_pty::{NativePty, Pty, PtyEvent, PtyEvents, SpawnOptions};

/// Upper bound on any wait in these tests.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How long the leader must sit in its exit before the close: past the
/// kernel's 60-tick (0.6 s) drain window, after which a leader whose own
/// close was the last one would have been reaped already.
const PAST_DRAIN_WINDOW: Duration = Duration::from_millis(1500);

/// Prints `ready`, waits for a line, stops its own output, queues a line
/// on the stopped terminal and exits.
const TCOOFF_SCRIPT: &str = r#"use POSIX; $| = 1; print "ready"; <STDIN>;
POSIX::tcflow(1, POSIX::TCOOFF) or die "tcflow: $!"; print "pending-at-exit\n"; exit 0"#;

/// How the test closes the session.
#[derive(Debug, Clone, Copy)]
enum Close {
    Kill,
    Hangup,
    Drop,
}

/// How the terminal's output gets stopped.
#[derive(Debug, Clone, Copy)]
enum Stop {
    /// The child calls `tcflow(TCOOFF)` itself.
    ChildTcooff,
    /// The parent types `Ctrl-S` into a terminal with `IXON` and no `IXANY`.
    ParentCtrlS,
}

/// The `ps` state letters of `pid`, empty once it is reaped.
fn state(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn open_fds() -> usize {
    std::fs::read_dir("/dev/fd").expect("list /dev/fd").count()
}

/// Polls `done` until it holds or [`TIMEOUT`] passes.
fn eventually(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    done()
}

/// Reads events until the output contains `marker`.
fn wait_for_output(events: &PtyEvents, marker: &str) {
    let deadline = Instant::now() + TIMEOUT;
    let mut text = String::new();
    while !text.contains(marker) {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(PtyEvent::Output(bytes)) => text.push_str(&String::from_utf8_lossy(&bytes)),
            other => panic!("waiting for {marker:?}: {other:?}; output so far {text:?}"),
        }
    }
}

/// Waits for `Exited`, failing after [`TIMEOUT`].
fn wait_for_exit(events: &PtyEvents, what: &str) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(PtyEvent::Output(_)) => {}
            Ok(PtyEvent::Exited(_)) => return,
            Err(RecvTimeoutError::Timeout) => {
                panic!("{what}: no Exited within {TIMEOUT:?}")
            }
            Err(RecvTimeoutError::Disconnected) => panic!("{what}: stream ended without Exited"),
        }
    }
}

/// Runs one scenario: the leader exits on stopped output, sits in its exit
/// past the drain window, and `close` must end the session.
fn stuck_leader_ends_on(stop: Stop, close: Close) {
    // nextest runs each test alone in its process, so the descriptor count
    // is this test's own; elsewhere it is not checked.
    let alone = std::env::var_os("NEXTEST").is_some();
    let fds_before = open_fds();
    let options = match stop {
        Stop::ChildTcooff => SpawnOptions::command("/usr/bin/perl", "perl")
            .arg("-e")
            .arg(TCOOFF_SCRIPT),
        Stop::ParentCtrlS => SpawnOptions::command("/bin/zsh", "zsh")
            .arg("-f")
            .arg("-c")
            .arg("stty ixon -ixany; print -n ready; read x; print -n pending-at-exit; exit 0"),
    };
    let what = format!("{stop:?} then {close:?}");
    let (mut pty, events) = NativePty::spawn(&options).expect("spawn the leader");
    let pid = pty.child_pid().expect("child pid");
    let threads = pty.io_threads();

    wait_for_output(&events, "ready");
    if let Stop::ParentCtrlS = stop {
        pty.write(b"\x13").expect("write Ctrl-S");
    }
    pty.write(b"go\n").expect("write the go line");

    // The leader is stuck: exiting, and still not reaped past the window.
    assert!(
        eventually(|| state(pid).contains('E')),
        "{what}: the leader never started exiting (state {:?})",
        state(pid)
    );
    let stuck_since = Instant::now();
    while stuck_since.elapsed() < PAST_DRAIN_WINDOW {
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(PtyEvent::Exited(status)) => {
                panic!("{what}: precondition: the leader exited ({status:?}) before the close")
            }
            Ok(PtyEvent::Output(_)) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("{what}: stream ended early"),
        }
    }
    assert!(
        state(pid).contains('E'),
        "{what}: precondition: the leader is not stuck exiting (state {:?})",
        state(pid)
    );

    let closed = Instant::now();
    let mut pty = Some(pty);
    match close {
        Close::Kill => pty.as_mut().expect("still held").kill().expect("kill"),
        Close::Hangup => pty.as_mut().expect("still held").hangup().expect("hangup"),
        Close::Drop => drop(pty.take()),
    }
    assert!(
        closed.elapsed() < Duration::from_millis(300),
        "{what}: the close blocked for {:?}",
        closed.elapsed()
    );
    wait_for_exit(&events, &what);
    assert!(
        eventually(|| state(pid).is_empty()),
        "{what}: the child was not reaped (state {:?})",
        state(pid)
    );
    assert!(
        eventually(|| threads.running() == 0),
        "{what}: {} io threads still running",
        threads.running()
    );
    drop(pty);
    drop(events);
    if alone {
        assert!(
            eventually(|| open_fds() <= fds_before),
            "{what}: {} descriptors open, {fds_before} before the spawn",
            open_fds()
        );
    }
}

#[test]
fn kill_ends_a_leader_exiting_on_output_it_stopped_itself() {
    stuck_leader_ends_on(Stop::ChildTcooff, Close::Kill);
}

#[test]
fn hangup_ends_a_leader_exiting_on_output_stopped_by_ctrl_s() {
    stuck_leader_ends_on(Stop::ParentCtrlS, Close::Hangup);
}

#[test]
fn dropping_the_handle_ends_a_leader_exiting_on_stopped_output() {
    stuck_leader_ends_on(Stop::ChildTcooff, Close::Drop);
}
