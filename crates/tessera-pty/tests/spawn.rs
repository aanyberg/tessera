//! End-to-end tests against real kernel PTYs.
//!
//! Every test spawns `/bin/sh` or the login shell, so they only run on Unix.
//! Each is bounded by [`TIMEOUT`] so a regression hangs for seconds, not
//! forever.

#![cfg(unix)]

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tessera_pty::{
    ExitStatus, NativePty, Pty, PtyError, PtyEvent, PtyEvents, PtySize, SpawnOptions,
    WRITE_QUEUE_BYTES,
};

const TIMEOUT: Duration = Duration::from_secs(10);

// Signal numbers are identical on every Unix this crate targets.
const SIGHUP: i32 = 1;
const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

/// Output gathered from a session, with `\r\n` normalised to `\n`.
#[derive(Default)]
struct Collected {
    text: String,
    exit: Option<ExitStatus>,
}

impl Collected {
    fn push(&mut self, event: PtyEvent) {
        match event {
            PtyEvent::Output(bytes) => {
                self.text
                    .push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n"));
            }
            PtyEvent::Exited(status) => self.exit = Some(status),
        }
    }
}

/// Pulls events until `done` holds or the child exits.
fn collect_until(events: &PtyEvents, mut done: impl FnMut(&Collected) -> bool) -> Collected {
    let deadline = Instant::now() + TIMEOUT;
    let mut collected = Collected::default();
    while !done(&collected) && collected.exit.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(event) => collected.push(event),
            Err(RecvTimeoutError::Timeout) => {
                panic!(
                    "timed out waiting for pty; output so far: {:?}",
                    collected.text
                )
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    collected
}

/// Drains the stream until the child exits.
fn collect_all(events: &PtyEvents) -> Collected {
    collect_until(events, |_| false)
}

fn sh(script: &str) -> SpawnOptions {
    SpawnOptions::command("/bin/sh", "test-session").args(["-c", script])
}

#[test]
fn exit_code_is_reported() {
    let (pty, events) = NativePty::spawn(&sh("exit 7")).unwrap();
    assert!(pty.child_pid().is_some());
    let collected = collect_all(&events);
    assert_eq!(collected.exit, Some(ExitStatus::Exited(7)));
    assert_eq!(pty.exit_status(), Some(ExitStatus::Exited(7)));
    // The stream ends after `Exited`: the sender is gone, not merely idle.
    assert!(matches!(
        events.recv_timeout(Duration::from_secs(2)),
        Err(RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn login_shell_runs_exit_and_reports_code() {
    // Point zsh at an empty ZDOTDIR so the developer's rc files cannot
    // interfere; other shells read nothing from it.
    let dir = std::env::temp_dir().join(format!("tessera-pty-login-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let options = SpawnOptions::login_shell("login-session").env("ZDOTDIR", &dir);
    let (mut pty, events) = NativePty::spawn(&options).unwrap();
    pty.write(b"exit 5\n").unwrap();
    let collected = collect_all(&events);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        collected.exit,
        Some(ExitStatus::Exited(5)),
        "output: {:?}",
        collected.text
    );
}

#[test]
fn identification_env_is_injected() {
    let script = r#"printf '%s|%s|%s|%s|%s\n' "$TERM" "$COLORTERM" "$TERM_PROGRAM" "$TERM_PROGRAM_VERSION" "$TESSERA_SESSION_ID""#;
    let options = sh(script);
    let (_pty, events) = NativePty::spawn(&options).unwrap();
    let collected = collect_all(&events);
    let expected = format!(
        "xterm-256color|truecolor|Tessera|{}|test-session\n",
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(collected.text, expected);
    assert_eq!(collected.exit, Some(ExitStatus::Exited(0)));
}

#[test]
fn caller_env_overrides_injected_env() {
    let options = sh(r#"printf '%s|%s\n' "$TERM" "$TESSERA_EXTRA""#)
        .env("TERM", "dumb")
        .env("TESSERA_EXTRA", "yes");
    let (_pty, events) = NativePty::spawn(&options).unwrap();
    assert_eq!(collect_all(&events).text, "dumb|yes\n");
}

#[test]
fn cwd_is_honoured() {
    let (_pty, events) = NativePty::spawn(&sh("pwd").cwd("/")).unwrap();
    assert_eq!(collect_all(&events).text, "/\n");
}

#[test]
fn resize_delivers_sigwinch_and_new_size() {
    // The child prints its size, arms a SIGWINCH trap that prints the new
    // size and exits, then idles. Only a delivered signal ends the script.
    let script = "trap 'echo WINCH; stty size; exit 0' WINCH; stty size; echo READY; \
                  while :; do sleep 0.02; done";
    let options = sh(script).size(PtySize::new(24, 80));
    let (mut pty, events) = NativePty::spawn(&options).unwrap();
    assert_eq!(pty.size(), PtySize::new(24, 80));

    let before = collect_until(&events, |c| c.text.contains("READY\n"));
    assert!(
        before.exit.is_none(),
        "child exited early: {:?}",
        before.text
    );
    assert_eq!(before.text, "24 80\nREADY\n");

    pty.resize(PtySize::new(30, 100)).unwrap();
    assert_eq!(pty.size(), PtySize::new(30, 100));

    let after = collect_all(&events);
    assert_eq!(after.text, "WINCH\n30 100\n");
    assert_eq!(after.exit, Some(ExitStatus::Exited(0)));
}

#[test]
fn stdin_reaches_the_child() {
    let (mut pty, events) = NativePty::spawn(&sh("read line; echo \"got:$line\"")).unwrap();
    pty.write(b"hello\n").unwrap();
    let collected = collect_all(&events);
    assert!(
        collected.text.ends_with("got:hello\n"),
        "output: {:?}",
        collected.text
    );
    assert_eq!(collected.exit, Some(ExitStatus::Exited(0)));
}

#[test]
fn kill_terminates_with_signal() {
    let (mut pty, events) = NativePty::spawn(&sh("sleep 30")).unwrap();
    pty.kill().unwrap();
    let collected = collect_all(&events);
    assert_eq!(collected.exit, Some(ExitStatus::Signaled(SIGKILL)));
    let status = collected.exit.unwrap();
    assert!(!status.success());
    assert_eq!(status.signal_name(), Some("SIGKILL"));
}

#[test]
fn hangup_terminates_with_sighup() {
    let (mut pty, events) = NativePty::spawn(&sh("sleep 30")).unwrap();
    pty.hangup().unwrap();
    let collected = collect_all(&events);
    assert_eq!(collected.exit, Some(ExitStatus::Signaled(SIGHUP)));
}

#[test]
fn drop_hangs_up_a_running_child() {
    let (pty, events) = NativePty::spawn(&sh("sleep 30")).unwrap();
    drop(pty);
    let collected = collect_all(&events);
    assert_eq!(collected.exit, Some(ExitStatus::Signaled(SIGHUP)));
}

#[test]
fn closing_the_session_injects_no_input() {
    // Regression test: dropping the handle used to let portable-pty's writer
    // push "\n" + VEOF into the child. The child ignores SIGHUP, copies raw
    // (non-canonical, so no line editing hides stray bytes) input to a file
    // and is killed by a helper subshell a second later, which is the only
    // way the session ends. The file must contain exactly what we wrote.
    let dir = std::env::temp_dir().join(format!("tessera-pty-noinject-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("input");
    let script = format!(
        "stty -icanon -echo; trap '' HUP; echo READY; \
         (sleep 1; kill -TERM $$) & exec cat > '{}'",
        file.display()
    );
    let (mut pty, events) = NativePty::spawn(&sh(&script)).unwrap();
    let ready = collect_until(&events, |c| c.text.contains("READY\n"));
    assert!(ready.exit.is_none(), "child exited early: {:?}", ready.text);
    pty.write(b"partial").unwrap();
    // Let the writer thread hand the bytes to the kernel before closing.
    std::thread::sleep(Duration::from_millis(100));
    drop(pty);

    let collected = collect_all(&events);
    let contents = std::fs::read(&file).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(collected.exit, Some(ExitStatus::Signaled(SIGTERM)));
    assert_eq!(
        contents,
        b"partial",
        "child received {:?}",
        String::from_utf8_lossy(&contents)
    );
}

#[test]
fn try_write_reports_would_block_when_queue_is_full() {
    // Non-canonical mode makes the kernel block master writes once its
    // input queue is full (in canonical mode an over-long line is discarded
    // instead), so with a child that never reads, our queue fills up.
    let (mut pty, events) =
        NativePty::spawn(&sh("stty -icanon -echo; echo READY; sleep 30")).unwrap();
    let ready = collect_until(&events, |c| c.text.contains("READY\n"));
    assert!(ready.exit.is_none(), "child exited early: {:?}", ready.text);

    let chunk = vec![b'x'; 64 * 1024];
    let max_chunks = WRITE_QUEUE_BYTES / chunk.len() + 2;
    let mut queued = 0;
    let error = loop {
        match pty.try_write(&chunk) {
            Ok(()) => {
                queued += 1;
                assert!(queued <= max_chunks, "queue accepted {queued} chunks");
            }
            Err(err) => break err,
        }
    };
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock, "{error}");
    assert!(queued > 0, "nothing was queued");
    pty.kill().unwrap();
    assert_eq!(
        collect_all(&events).exit,
        Some(ExitStatus::Signaled(SIGKILL))
    );
}

#[test]
fn zero_sized_windows_are_rejected() {
    let (mut pty, _events) = NativePty::spawn(&sh("sleep 30")).unwrap();
    assert!(matches!(
        pty.resize(PtySize::new(0, 80)),
        Err(PtyError::InvalidSize(_))
    ));
    assert!(matches!(
        pty.resize(PtySize::new(24, 0)),
        Err(PtyError::InvalidSize(_))
    ));
    assert_eq!(pty.size(), PtySize::default(), "size must not change");
    assert!(matches!(
        NativePty::spawn(&sh("true").size(PtySize::new(0, 0))),
        Err(PtyError::InvalidSize(_))
    ));
}

#[test]
fn writes_fail_after_exit() {
    let (mut pty, events) = NativePty::spawn(&sh("exit 0")).unwrap();
    collect_all(&events);
    assert_eq!(
        pty.write(b"x").unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        pty.try_write(b"x").unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
    // Signals after exit are no-ops rather than hitting a recycled pid.
    pty.hangup().unwrap();
    pty.kill().unwrap();
}

#[test]
fn missing_program_is_a_spawn_error() {
    let options = SpawnOptions::command("/nonexistent/tessera-no-such-binary", "s");
    match NativePty::spawn(&options) {
        Err(PtyError::Spawn { program, .. }) => {
            assert_eq!(program, "/nonexistent/tessera-no-such-binary");
        }
        Err(other) => panic!("unexpected error: {other}"),
        Ok(_) => panic!("spawn unexpectedly succeeded"),
    }
}

#[test]
fn large_output_is_delivered_in_order() {
    // 2048 lines of 1004 bytes (~2 MiB) through a 64 KiB read buffer and a
    // bounded queue, checked for completeness and ordering.
    let script = "pad=$(printf '%01000d' 0 | tr 0 x); i=0; \
                  while [ $i -lt 2048 ]; do printf '%04d%s\\n' $i \"$pad\"; i=$((i+1)); done";
    let (_pty, events) = NativePty::spawn(&sh(script)).unwrap();
    let collected = collect_all(&events);
    assert_eq!(collected.exit, Some(ExitStatus::Exited(0)));
    let lines: Vec<&str> = collected.text.lines().collect();
    assert_eq!(lines.len(), 2048);
    for (i, line) in lines.iter().enumerate() {
        assert!(line.starts_with(&format!("{i:04}")), "line {i}: {line:?}");
        assert_eq!(line.len(), 1004, "line {i} length");
    }
}
