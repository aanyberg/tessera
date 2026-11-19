//! Output a child writes just before it exits reaches the event stream,
//! however late the reader thread gets to it (#562).
//!
//! On macOS a PTY's last slave close waits about 0.6 s for the master to
//! drain the terminal's output queue and then discards what is left. When
//! the child's own exit made that last close, a child that printed and
//! exited while its reader thread was not yet scheduled (many sessions in
//! one busy process) lost its output: `/bin/echo` reported `Exited(0)` and
//! no text. These tests make the late reader deterministic: the thread
//! start hook (`tessera_pty::set_thread_start_hook`) holds every reader
//! back until its child has passed that window, so they need a binary of
//! their own (the hook is process-wide).

#![cfg(target_os = "macos")]

use std::process::Command;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Mutex, Once, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tessera_pty::{ExitStatus, NativePty, Pty, PtyEvent, PtyEvents, SpawnOptions};

/// Upper bound on any wait for events, or for a child to start exiting.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How long a held-back reader lets its child sit in its exit before it
/// reads: past the kernel's drain window of 60 ticks (0.6 s), so a child
/// whose last close would discard its output has done so by then.
const PAST_DRAIN_WINDOW: Duration = Duration::from_millis(1500);

/// Sessions started at once by the many-sessions test.
const SESSIONS: usize = 8;

/// Children whose reader the hook held back and then let go, by pid.
static HELD: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// The `ps` state letters of `pid`, empty once it is reaped.
fn state(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Holds each reader thread back until its child is a zombie (its exit
/// is complete, so any output its last close discarded is gone), or has
/// been exiting (`E`) for [`PAST_DRAIN_WINDOW`] (it is waiting in its exit
/// for the master to drain, which only a reader can do). Polls the state,
/// never just sleeps: a loaded machine that starts the child late only
/// makes the hook wait longer.
fn hold_reader() {
    let Some(pid) = thread::current()
        .name()
        .and_then(|name| name.strip_prefix("pty-reader-"))
        .and_then(|pid| pid.parse::<u32>().ok())
    else {
        return;
    };
    let deadline = Instant::now() + TIMEOUT;
    let mut exiting_since = None;
    loop {
        let now = Instant::now();
        let state = state(pid);
        if state.starts_with('Z') {
            break;
        }
        if state.contains('E')
            && now.duration_since(*exiting_since.get_or_insert(now)) >= PAST_DRAIN_WINDOW
        {
            break;
        }
        // Not recorded: the test reports the child that never exited.
        if now >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    HELD.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(pid);
}

fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| assert!(tessera_pty::set_thread_start_hook(hold_reader)));
}

/// Output (with `\r\n` normalised) and exit status, read until the child
/// exits and the stream ends.
fn collect_all(events: &PtyEvents) -> (String, Option<ExitStatus>) {
    let deadline = Instant::now() + TIMEOUT + TIMEOUT;
    let mut text = String::new();
    let mut exit = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(PtyEvent::Output(bytes)) => {
                text.push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n"));
            }
            Ok(PtyEvent::Exited(status)) => exit = Some(status),
            Err(RecvTimeoutError::Timeout) => panic!("timed out; output so far: {text:?}"),
            Err(RecvTimeoutError::Disconnected) => return (text, exit),
        }
    }
}

/// Whether the hook held `pid`'s reader back and then let it go.
fn held(pid: u32) -> bool {
    HELD.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&pid)
}

#[test]
fn many_sessions_keep_the_output_of_a_child_that_exits_before_its_reader_runs() {
    install();
    let sessions: Vec<_> = (0..SESSIONS)
        .map(|i| {
            let options = SpawnOptions::command("/bin/echo", "s").arg(format!("from-echo-{i}"));
            let (pty, events) = NativePty::spawn(&options).expect("spawn /bin/echo");
            (i, pty, events)
        })
        .collect();
    for (i, pty, events) in &sessions {
        let (text, exit) = collect_all(events);
        assert_eq!(exit, Some(ExitStatus::Exited(0)), "session {i}");
        assert_eq!(text, format!("from-echo-{i}\n"), "session {i}");
        let pid = pty.child_pid().expect("child pid");
        assert!(
            held(pid),
            "session {i}: the hook never let its reader go (the child did not start exiting \
             within {TIMEOUT:?})"
        );
    }
}
