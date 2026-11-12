//! The thread start hook (`tessera_pty::set_thread_start_hook`, #398, ADR
//! 0018) runs first on every thread the crate starts, and a hook that
//! panics neither kills the thread nor strands the child.
//!
//! The hook and the `tracing` subscriber that counts its panic log are
//! process-wide, so these tests live in a binary of their own: nothing else
//! in the crate's tests sees the hook. Each test is correct whether it runs
//! alone (nextest: one process per test) or beside the other (`cargo
//! test`): the hook records every thread before it may panic, and every
//! check names the threads of the test's own child.

#![cfg(unix)]

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Mutex, Once, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tessera_pty::{ExitStatus, NativePty, Pty, PtyEvent, PtyEvents, SpawnOptions};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The message `native.rs` logs when the hook panics.
const HOOK_PANICKED: &str = "pty thread start hook panicked";

/// Names of the threads that ran the hook, in start order.
static HOOKED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Names of the threads that logged [`HOOK_PANICKED`], once per log.
static PANIC_LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Makes every hook run panic after recording its thread.
static PANIC_IN_HOOK: AtomicBool = AtomicBool::new(false);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn current_name() -> String {
    thread::current().name().unwrap_or("?").to_owned()
}

/// The tests' hook: records the thread, then panics when asked to.
fn test_hook() {
    lock(&HOOKED).push(current_name());
    assert!(
        !PANIC_IN_HOOK.load(Ordering::SeqCst),
        "deliberate start hook panic"
    );
}

/// Records whether an event's `message` field is [`HOOK_PANICKED`].
#[derive(Default)]
struct IsHookPanic(bool);

impl tracing::field::Visit for IsHookPanic {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}") == HOOK_PANICKED;
        }
    }
}

/// Appends the logging thread's name to [`PANIC_LOGS`] for each
/// error-level [`HOOK_PANICKED`] event.
struct PanicLogCounter;

impl tracing::Subscriber for PanicLogCounter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::ERROR
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = IsHookPanic::default();
        event.record(&mut visitor);
        if visitor.0 {
            lock(&PANIC_LOGS).push(current_name());
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Installs [`test_hook`] and the [`PanicLogCounter`] for the process,
/// once, before any PTY is spawned.
fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        tracing::subscriber::set_global_default(PanicLogCounter).expect("first global subscriber");
        assert!(tessera_pty::set_thread_start_hook(test_hook));
        assert!(
            !tessera_pty::set_thread_start_hook(|| {}),
            "only the first hook is kept"
        );
    });
}

fn sh(script: &str) -> SpawnOptions {
    SpawnOptions::command("/bin/sh", "test-session").args(["-c", script])
}

/// Output (with `\r\n` normalised) and exit status, read until the child
/// exits and the stream ends.
fn collect_all(events: &PtyEvents) -> (String, Option<ExitStatus>) {
    let deadline = Instant::now() + TIMEOUT;
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

/// The reader and writer thread names of the PTY whose child is `pid`.
fn thread_names(pid: u32) -> [String; 2] {
    [format!("pty-reader-{pid}"), format!("pty-writer-{pid}")]
}

/// Waits until both of `pid`'s threads are in `list`.
fn wait_listed(list: &Mutex<Vec<String>>, pid: u32) -> Vec<String> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let seen = lock(list).clone();
        if thread_names(pid).iter().all(|name| seen.contains(name)) {
            return seen;
        }
        assert!(
            Instant::now() < deadline,
            "{pid}'s threads missing: {seen:?}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn reader_and_writer_threads_run_the_hook() {
    install();
    let (mut pty, events) = NativePty::spawn(&sh("read line; echo \"got $line\"")).unwrap();
    let pid = pty.child_pid().expect("child pid");
    pty.write(b"x\n").unwrap();
    let (text, exit) = collect_all(&events);
    assert!(text.contains("got x"), "{text:?}");
    assert_eq!(exit, Some(ExitStatus::Exited(0)));
    let hooked = wait_listed(&HOOKED, pid);
    for name in thread_names(pid) {
        // Once per thread start, not per read or write.
        let runs = hooked.iter().filter(|seen| **seen == name).count();
        assert_eq!(runs, 1, "{name} ran the hook {runs} times: {hooked:?}");
    }
}

#[test]
fn a_panicking_hook_is_logged_once_and_the_session_carries_on() {
    install();
    PANIC_IN_HOOK.store(true, Ordering::SeqCst);
    // The child reads a line first, so the writer thread (whose hook
    // panicked too) must still deliver input for it to finish.
    let (mut pty, events) = NativePty::spawn(&sh("read line; echo \"got $line\"; exit 4")).unwrap();
    let pid = pty.child_pid().expect("child pid");
    pty.write(b"y\n").unwrap();
    let (text, exit) = collect_all(&events);
    assert!(text.contains("got y"), "{text:?}");
    assert_eq!(exit, Some(ExitStatus::Exited(4)));
    assert_eq!(pty.exit_status(), Some(ExitStatus::Exited(4)));
    let logs = wait_listed(&PANIC_LOGS, pid);
    for name in thread_names(pid) {
        let count = logs.iter().filter(|seen| **seen == name).count();
        assert_eq!(count, 1, "{name} logged the panic {count} times: {logs:?}");
    }
}
