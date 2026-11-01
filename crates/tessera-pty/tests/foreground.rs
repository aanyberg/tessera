//! `Pty::foreground_process` against real kernel PTYs and an interactive
//! `/bin/sh` with job control (issue #34).
//!
//! Waits are bounded by attempts, not by a clock: each poll sleeps
//! [`POLL`], so a loaded machine stretches the wait instead of failing it.

#![cfg(unix)]

use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;

use tessera_pty::{
    ExitStatus, ForegroundProcess, NativePty, Pty, PtyEvent, PtyEvents, SpawnOptions,
};

/// Pause between two polls.
const POLL: Duration = Duration::from_millis(10);
/// Polls before giving up: 10 s on an idle machine. The job shows up within
/// the first few polls there (the 2 s bar of #34); the rest is headroom
/// for a loaded machine, as the other wall-clock bars keep (#190).
const ATTEMPTS: usize = 1000;
/// Longest wait for one event before a test fails instead of hanging.
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

/// An interactive `/bin/sh` that reads no start-up file, with a prompt the
/// tests can wait for.
fn interactive_sh() -> (NativePty, PtyEvents) {
    let options = SpawnOptions::command("/bin/sh", "fg-test")
        .arg("-i")
        .env("ENV", "/dev/null")
        .env("PS1", "prompt> ");
    NativePty::spawn(&options).expect("spawn /bin/sh -i")
}

/// Reads output until it contains `marker` (or the child exits).
fn wait_for_output(events: &PtyEvents, seen: &mut String, marker: &str) {
    while !seen.contains(marker) {
        match events.recv_timeout(EVENT_TIMEOUT) {
            Ok(PtyEvent::Output(bytes)) => seen.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(PtyEvent::Exited(status)) => panic!("child exited ({status}); output {seen:?}"),
            Err(err) => panic!("no {marker:?} ({err:?}); output {seen:?}"),
        }
    }
}

/// Polls until `done` accepts the foreground job, at most [`ATTEMPTS`]
/// times, and returns the last value read.
fn poll(
    pty: &NativePty,
    done: impl Fn(&Option<ForegroundProcess>) -> bool,
) -> Option<ForegroundProcess> {
    let mut last = pty.foreground_process();
    for _ in 0..ATTEMPTS {
        if done(&last) {
            return last;
        }
        thread::sleep(POLL);
        last = pty.foreground_process();
    }
    panic!("foreground job still {last:?} after {ATTEMPTS} polls");
}

/// Drains the stream until the child exits and returns its status.
fn wait_for_exit(events: &PtyEvents) -> ExitStatus {
    loop {
        match events.recv_timeout(EVENT_TIMEOUT) {
            Ok(PtyEvent::Output(_)) => {}
            Ok(PtyEvent::Exited(status)) => return status,
            Err(RecvTimeoutError::Timeout) => panic!("child did not exit"),
            Err(RecvTimeoutError::Disconnected) => panic!("stream ended without an exit"),
        }
    }
}

#[test]
fn an_idle_shell_has_no_foreground_job() {
    let (mut pty, events) = interactive_sh();
    let mut seen = String::new();
    pty.write(b"echo ready-$((6 * 7))\n").unwrap();
    wait_for_output(&events, &mut seen, "ready-42");
    wait_for_output(&events, &mut seen, "ready-42\r\nprompt> ");
    assert_eq!(pty.foreground_process(), None, "output {seen:?}");
}

#[test]
fn a_running_job_is_reported_until_it_is_interrupted() {
    let (mut pty, events) = interactive_sh();
    let mut seen = String::new();
    pty.write(b"echo ready-$((6 * 7))\n").unwrap();
    wait_for_output(&events, &mut seen, "ready-42\r\nprompt> ");

    pty.write(b"sleep 100\n").unwrap();
    let job = poll(&pty, |job| {
        job.as_ref().is_some_and(|job| job.name == "sleep")
    })
    .expect("a foreground job");
    let shell = pty.child_pid().unwrap();
    assert_ne!(job.pid, shell);
    // The reported pid is the job's process group: `sleep` leads it.
    let group = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(
        i32::try_from(job.pid).unwrap(),
    )))
    .expect("the job's leader is alive");
    assert_eq!(u32::try_from(group.as_raw()).unwrap(), job.pid);

    // Ctrl-C ends the job; the shell takes the terminal back.
    pty.write(b"\x03").unwrap();
    poll(&pty, Option::is_none);

    pty.write(b"exit 0\n").unwrap();
    assert_eq!(wait_for_exit(&events), ExitStatus::Exited(0));
}

#[test]
fn an_exited_child_has_no_foreground_job() {
    let (mut pty, events) = interactive_sh();
    pty.write(b"sleep 100\n").unwrap();
    poll(&pty, Option::is_some);
    // Killing the shell leaves its job behind on the terminal; the session
    // is over all the same.
    pty.kill().unwrap();
    assert_eq!(wait_for_exit(&events), ExitStatus::Signaled(9));
    assert_eq!(pty.foreground_process(), None);
    assert_eq!(pty.foreground_process(), None, "asking again is harmless");
}

#[test]
fn a_program_spawned_without_a_shell_is_not_a_job() {
    // `sleep` is the session leader here: its group owns the terminal.
    let (pty, _events) =
        NativePty::spawn(&SpawnOptions::command("/bin/sleep", "fg-direct").arg("100"))
            .expect("spawn sleep");
    for _ in 0..10 {
        assert_eq!(pty.foreground_process(), None);
        thread::sleep(POLL);
    }
}
