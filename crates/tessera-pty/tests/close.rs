//! Closing a session: hangup and drop must drop pending input, return
//! promptly, let both I/O threads finish and leave no child behind.
//!
//! These spawn real children under real kernel PTYs, so they only run on
//! Unix. Bounds are generous (the issue's 100 ms / 1 s / 1.5 s) so a loaded
//! machine does not flake them, and each test stays under three seconds.

#![cfg(unix)]

use std::io::ErrorKind;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{self, SigHandler, Signal};
use nix::unistd::Pid;
use tessera_pty::{
    ExitStatus, NativePty, Pty, PtyEvent, PtyEvents, SpawnOptions, WRITE_QUEUE_BYTES,
};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Pulls events until `text` has been printed; panics on timeout or exit.
fn wait_for_output(events: &PtyEvents, text: &str) {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains(text) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(PtyEvent::Output(bytes)) => seen.extend_from_slice(&bytes),
            Ok(PtyEvent::Exited(status)) => {
                panic!("child exited ({status:?}) before printing {text:?}: {seen:?}")
            }
            Err(err) => panic!("no {text:?} from child ({err}): {seen:?}"),
        }
    }
}

/// Drains `events` until the child's exit status arrives or `deadline`
/// passes.
fn exit_before(events: &PtyEvents, deadline: Instant) -> Option<ExitStatus> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(PtyEvent::Output(_)) => {}
            Ok(PtyEvent::Exited(status)) => return Some(status),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// True once `pid` names no process at all (not even a zombie), i.e. the
/// PTY's reader thread has reaped it.
fn reaped(pid: u32) -> bool {
    let pid = Pid::from_raw(i32::try_from(pid).unwrap());
    signal::kill(pid, None) == Err(Errno::ESRCH)
}

/// Bound on dropping all ten PTYs in one attempt on a quiet machine.
const DROP_BOUND: Duration = Duration::from_millis(100);
/// Attempts before the drop bound counts as broken.
const DROP_ATTEMPTS: usize = 3;

#[test]
fn close_with_pending_input_is_prompt_and_joins_threads() {
    // The regression this guards against costs seconds per attempt (one
    // unsliced 1 MiB write per session stalls `kill(2)` for 2-5 s), while
    // a loaded machine can push one attempt past 100 ms for a moment. So
    // the drop bound must hold in at least one of three attempts; reaping
    // and thread exit must hold in every attempt.
    let mut took = Vec::with_capacity(DROP_ATTEMPTS);
    for _ in 0..DROP_ATTEMPTS {
        let dropped_in = close_ten_cooked_sessions_with_pending_input();
        if dropped_in < DROP_BOUND {
            return;
        }
        took.push(dropped_in);
    }
    panic!("dropping ten PTYs took over {DROP_BOUND:?} in every attempt: {took:?}");
}

/// Spawns ten cooked-mode children that never read, queues 1 MiB without a
/// newline into each, drops all ten PTYs and returns how long the drops
/// took. Asserts that every child is reaped within 1.5 s and every I/O
/// thread has finished within 1 s of the close.
///
/// With no newline the kernel discards input past the line limit, so each
/// writer is not blocked but running in the kernel, processing its input
/// byte by byte under the terminal lock; that is what used to stall the
/// close.
fn close_ten_cooked_sessions_with_pending_input() -> Duration {
    const CHILDREN: usize = 10;
    let mut sessions = Vec::with_capacity(CHILDREN);
    for _ in 0..CHILDREN {
        let options = SpawnOptions::command("/bin/sleep", "close-test").args(["98770"]);
        let (mut pty, events) = NativePty::spawn(&options).unwrap();
        // Exactly the queue's budget, so it all fits at once.
        pty.write(&vec![b'x'; WRITE_QUEUE_BYTES]).unwrap();
        sessions.push((pty, events));
    }
    thread::sleep(Duration::from_millis(300));

    let mut threads = Vec::with_capacity(CHILDREN);
    let mut pids = Vec::with_capacity(CHILDREN);
    let mut receivers = Vec::with_capacity(CHILDREN);
    for (pty, _) in &sessions {
        threads.push(pty.io_threads());
        pids.push(pty.child_pid().unwrap());
        assert_eq!(pty.io_threads().running(), 2, "threads died before close");
    }
    let started = Instant::now();
    for (pty, events) in sessions {
        drop(pty);
        receivers.push(events);
    }
    let dropped_in = started.elapsed();

    let reap_deadline = started + Duration::from_millis(1500);
    for (events, pid) in receivers.iter().zip(&pids) {
        let status = exit_before(events, reap_deadline);
        assert_eq!(
            status,
            Some(ExitStatus::Signaled(Signal::SIGHUP as i32)),
            "child {pid} not reaped within 1.5 s of close"
        );
        assert!(
            reaped(*pid),
            "child {pid} still exists after its exit event"
        );
    }

    let thread_deadline = started + Duration::from_secs(1);
    while threads.iter().any(|t| t.running() > 0) && Instant::now() < thread_deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let running: usize = threads.iter().map(tessera_pty::IoThreads::running).sum();
    assert_eq!(
        running,
        0,
        "{running} I/O threads still running 1 s after close (after {:?})",
        started.elapsed()
    );
    dropped_in
}

/// Watches a raw-mode child that ignores SIGHUP and copies its input to a
/// file.
struct RawCopy {
    pid: u32,
    events: PtyEvents,
    dir: std::path::PathBuf,
    file: std::path::PathBuf,
}

impl RawCopy {
    /// Starts the child, queues [`WRITE_QUEUE_BYTES`] of `x` as one chunk
    /// and returns once the child has received some of it, so the writer is
    /// working through the chunk when the test closes the session:
    /// finishing it after the close would deliver all of it, which
    /// `assert_input_stopped` rejects. (Dropping chunks that are still
    /// queued is covered by the queue's unit test.)
    fn start(tag: &str) -> (NativePty, Self) {
        let dir =
            std::env::temp_dir().join(format!("tessera-pty-close-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("input");
        let script = format!(
            "trap '' HUP; stty raw -echo; echo READY; exec cat > '{}'",
            file.display()
        );
        let options = SpawnOptions::command("/bin/sh", "close-test").args(["-c", &script]);
        let (mut pty, events) = NativePty::spawn(&options).unwrap();
        wait_for_output(&events, "READY");
        pty.write(&vec![b'x'; WRITE_QUEUE_BYTES]).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        while std::fs::metadata(&file).map_or(0, |m| m.len()) == 0 {
            assert!(Instant::now() < deadline, "the child received no input");
            thread::sleep(Duration::from_millis(1));
        }
        let pid = pty.child_pid().unwrap();
        (
            pty,
            Self {
                pid,
                events,
                dir,
                file,
            },
        )
    }

    /// Checks that, from `closed` on, the file stops growing and never
    /// holds all of the queued input or anything but `x`. `kill` ends the
    /// child afterwards; it must die of `SIGKILL`.
    fn assert_input_stopped(self, closed: Instant, kill: impl FnOnce(u32)) {
        thread::sleep(Duration::from_millis(500).saturating_sub(closed.elapsed()));
        let early = std::fs::metadata(&self.file).unwrap().len();
        thread::sleep(Duration::from_secs(2).saturating_sub(closed.elapsed()));
        let contents = std::fs::read(&self.file).unwrap();
        let alive = !reaped(self.pid);
        kill(self.pid);
        let status = exit_before(&self.events, Instant::now() + TIMEOUT);
        let _ = std::fs::remove_dir_all(&self.dir);

        assert!(alive, "child ignoring SIGHUP exited anyway");
        assert_eq!(status, Some(ExitStatus::Signaled(Signal::SIGKILL as i32)));
        assert_eq!(
            early,
            contents.len() as u64,
            "child kept receiving input after close ({early} bytes at 500 ms, {} at 2 s)",
            contents.len()
        );
        assert!(
            contents.len() < WRITE_QUEUE_BYTES,
            "all {WRITE_QUEUE_BYTES} queued bytes reached the child; nothing was dropped"
        );
        assert!(
            !contents.contains(&b'M'),
            "a byte written after hangup reached the child"
        );
        assert!(
            contents.iter().all(|&b| b == b'x'),
            "child received foreign bytes"
        );
    }
}

#[test]
fn no_bytes_reach_the_child_after_close() {
    // Hanging up must drop what is still queued: the file stops growing
    // once `hangup` returns, and input written afterwards (the marker `M`)
    // is refused.
    let (mut pty, child) = RawCopy::start("hangup");
    pty.hangup().unwrap();
    let closed = Instant::now();
    for attempt in [pty.write(b"M"), pty.try_write(b"M")] {
        assert_eq!(
            attempt.unwrap_err().kind(),
            ErrorKind::BrokenPipe,
            "input accepted after hangup"
        );
    }
    child.assert_input_stopped(closed, |_| pty.kill().unwrap());
}

#[test]
fn no_bytes_reach_the_child_after_drop() {
    // Dropping the handle closes the session the same way. The child is
    // then killed by pid: it ignores SIGHUP, so it is still running and the
    // pid cannot have been recycled (checked before the kill).
    let (pty, child) = RawCopy::start("drop");
    drop(pty);
    let closed = Instant::now();
    child.assert_input_stopped(closed, |pid| {
        let pid = Pid::from_raw(i32::try_from(pid).unwrap());
        signal::kill(pid, Signal::SIGKILL).unwrap();
    });
}

/// Input that may still reach the child after a close in
/// `a_chunk_being_written_is_abandoned_at_hangup`: the writer's slice in
/// progress (1 KiB), what the kernel had already accepted (about 1 KiB of
/// raw input on macOS) and the reader's own 1 KiB buffer, rounded up.
const CLOSE_RESIDUE: u64 = 4 * 1024;

#[test]
fn a_chunk_being_written_is_abandoned_at_hangup() {
    // A slow single-process reader (1 KiB every 20 ms) keeps the writer
    // busy with one 512 KiB chunk for many seconds. Hanging up must stop
    // it after the slice in progress instead of finishing the chunk: the
    // file grows by at most `CLOSE_RESIDUE` after the hangup and then
    // stops. Growth is bounded rather than timed so a starved reader on a
    // loaded machine only delays the check.
    let dir = std::env::temp_dir().join(format!("tessera-pty-close-slow-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("input");
    let script = format!(
        "trap '' HUP; stty raw -echo; echo READY; exec perl -e \
         'while (sysread STDIN, $b, 1024) {{ syswrite STDOUT, $b; select undef, undef, undef, 0.02 }}' \
         >> '{}'",
        file.display()
    );
    let size = || std::fs::metadata(&file).map_or(0, |m| m.len());
    let options = SpawnOptions::command("/bin/sh", "close-test").args(["-c", &script]);
    let (mut pty, events) = NativePty::spawn(&options).unwrap();
    wait_for_output(&events, "READY");
    let chunk = WRITE_QUEUE_BYTES / 2;
    pty.write(&vec![b'x'; chunk]).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while size() == 0 {
        assert!(Instant::now() < deadline, "the child received no input");
        thread::sleep(Duration::from_millis(1));
    }
    pty.hangup().unwrap();
    let at_close = size();

    // Wait until two samples half a second apart agree, for up to 10 s.
    let deadline = Instant::now() + TIMEOUT;
    let mut last = at_close;
    let stopped = loop {
        thread::sleep(Duration::from_millis(500));
        let now = size();
        if now == last {
            break true;
        }
        last = now;
        if Instant::now() >= deadline {
            break false;
        }
    };
    let alive = pty.exit_status().is_none();
    pty.kill().unwrap();
    let status = exit_before(&events, Instant::now() + TIMEOUT);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(alive, "child ignoring SIGHUP exited anyway");
    assert_eq!(status, Some(ExitStatus::Signaled(Signal::SIGKILL as i32)));
    assert!(
        stopped,
        "input still flowing 10 s after hangup ({at_close} bytes at close, {last} now)"
    );
    assert!(
        last - at_close <= CLOSE_RESIDUE,
        "{} bytes reached the child after hangup ({at_close} at close, {last} at the end)",
        last - at_close
    );
}

#[test]
fn the_writer_of_a_child_that_never_reads_ends_with_the_child() {
    // Known limit (see `NativePty`): a child that ignores SIGHUP and never
    // reads leaves the writer asleep in `write(2)` after hangup. It must
    // not outlive the child: once the child is killed, both threads end.
    let script = "trap '' HUP; stty raw -echo; echo READY; exec sleep 98770";
    let options = SpawnOptions::command("/bin/sh", "close-test").args(["-c", script]);
    let (mut pty, events) = NativePty::spawn(&options).unwrap();
    wait_for_output(&events, "READY");
    let threads = pty.io_threads();

    pty.write(&vec![b'x'; WRITE_QUEUE_BYTES]).unwrap();
    thread::sleep(Duration::from_millis(100));
    pty.hangup().unwrap();
    pty.kill().unwrap();
    let status = exit_before(&events, Instant::now() + TIMEOUT);
    let killed = Instant::now();
    while threads.running() > 0 && killed.elapsed() < Duration::from_secs(1) {
        thread::sleep(Duration::from_millis(5));
    }

    assert_eq!(status, Some(ExitStatus::Signaled(Signal::SIGKILL as i32)));
    assert_eq!(
        threads.running(),
        0,
        "I/O threads outlived the child by 1 s"
    );
}

#[test]
fn kill_refuses_input_at_once() {
    // `kill` ends the input like `hangup`, without waiting for the reader
    // thread to notice the child's death.
    let options = SpawnOptions::command("/bin/sleep", "close-test").args(["98770"]);
    let (mut pty, events) = NativePty::spawn(&options).unwrap();
    pty.kill().unwrap();
    for attempt in [pty.try_write(b"M"), pty.write(b"M")] {
        assert_eq!(
            attempt.unwrap_err().kind(),
            ErrorKind::BrokenPipe,
            "input accepted after kill"
        );
    }
    assert_eq!(
        exit_before(&events, Instant::now() + TIMEOUT),
        Some(ExitStatus::Signaled(Signal::SIGKILL as i32))
    );
}

#[test]
fn echoed_input_arrives_intact_through_sliced_writes() {
    // The writer hands each chunk to the kernel in slices, resuming after
    // partial writes. A raw `cat` echoes input back while more is being
    // written, which exercises both threads at once; every byte must come
    // back exactly once and in order.
    let options = SpawnOptions::command("/bin/sh", "close-test")
        .args(["-c", "stty raw -echo; echo READY; exec cat"]);
    let (mut pty, events) = NativePty::spawn(&options).unwrap();
    wait_for_output(&events, "READY");
    let sent: Vec<u8> = (0..4 * WRITE_QUEUE_BYTES / 10)
        .map(|i| b"abcdefghijklmnopqrstuvwxyz"[i % 26])
        .collect();
    let writer = {
        let sent = sent.clone();
        thread::spawn(move || {
            for piece in sent.chunks(7 * 1024) {
                pty.write(piece).unwrap();
            }
            pty
        })
    };
    let deadline = Instant::now() + TIMEOUT;
    let mut echoed = Vec::with_capacity(sent.len());
    while echoed.len() < sent.len() {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(PtyEvent::Output(bytes)) => echoed.extend_from_slice(&bytes),
            other => panic!("echo stopped after {} bytes: {other:?}", echoed.len()),
        }
    }
    let mut pty = writer.join().unwrap();
    pty.kill().unwrap();
    assert!(echoed == sent, "echo differs from input");
}

#[test]
fn child_gets_default_signal_dispositions() {
    // portable-pty resets signal dispositions in the child before `exec`,
    // so a parent that ignores SIGINT/SIGTERM (as GUI apps and some test
    // runners do) does not hand that on to the shell. Ignored dispositions
    // otherwise survive `exec`.
    struct Restore([(Signal, SigHandler); 2]);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (sig, handler) in self.0 {
                // SAFETY: restores the disposition this test replaced; the
                // previous handler was valid when it was installed.
                unsafe { signal::signal(sig, handler) }.unwrap();
            }
        }
    }
    // SAFETY: `SigIgn` installs no handler code; the guard restores the
    // previous dispositions even if an assertion below fails.
    let _restore = unsafe {
        Restore([
            (
                Signal::SIGINT,
                signal::signal(Signal::SIGINT, SigHandler::SigIgn).unwrap(),
            ),
            (
                Signal::SIGTERM,
                signal::signal(Signal::SIGTERM, SigHandler::SigIgn).unwrap(),
            ),
        ])
    };

    let options = SpawnOptions::command("/bin/sleep", "signal-test").args(["100"]);
    let (pty, events) = NativePty::spawn(&options).unwrap();
    let pid = Pid::from_raw(i32::try_from(pty.child_pid().unwrap()).unwrap());
    // Give the child time to `exec`: before that it is a forked copy of
    // this process, still ignoring SIGINT until portable-pty resets it.
    thread::sleep(Duration::from_millis(200));
    signal::kill(pid, Signal::SIGINT).unwrap();
    let status = exit_before(&events, Instant::now() + Duration::from_secs(1));
    assert_eq!(status, Some(ExitStatus::Signaled(Signal::SIGINT as i32)));
}
