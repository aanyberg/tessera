//! Run a command in a PTY and drive a [`Terminal`] with its output.
//!
//! [`run`] spawns `/bin/sh -c -- <command>` in a [`NativePty`] of the requested
//! size, feeds every chunk of output to [`Terminal::advance`] and returns
//! once the child exits or the timeout expires. On timeout the child gets
//! `SIGHUP`, then `SIGKILL` if it is still running 200 ms later, and the
//! terminal is returned anyway, with whatever the child printed until then.
//! Only the direct child (`/bin/sh`) is signalled; see [`run`].
//!
//! The child's environment is this process's plus what `tessera-pty`
//! injects (`TERM=xterm-256color`, `COLORTERM`, `TERM_PROGRAM`, …); nothing
//! else is added. Inherited `COLUMNS` and `LINES` are passed through too, and
//! programs that read them believe them over the PTY size. It starts in
//! this process's current directory. Nothing is
//! ever written to its input, so a command that waits for input runs until
//! the timeout.
//!
//! ```no_run
//! use tessera_devtool::run::{RunOptions, run};
//!
//! let outcome = run(&RunOptions::new("printf 'hello'"))?;
//! assert_eq!(outcome.exit_code(), 0);
//! print!("{}", tessera_devtool::dump::text(outcome.terminal.grid()));
//! # Ok::<(), tessera_devtool::run::Error>(())
//! ```

use std::env;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tessera_pty::{
    ExitStatus, NativePty, Pty, PtyError, PtyEvent, PtyEvents, PtySize, SpawnOptions,
};
use tessera_term::Terminal;

/// The shell that runs the command string.
pub const SHELL: &str = "/bin/sh";

/// Exit code reported when the command did not finish in time, as
/// `timeout(1)` does.
pub const TIMEOUT_EXIT_CODE: i32 = 124;

/// Default for [`RunOptions::timeout`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a timed-out child has to exit after `SIGHUP` before it gets
/// `SIGKILL`, as `timeout -k` does: long enough for an editor to save its
/// swap file or a shell its history.
const HANGUP_GRACE: Duration = Duration::from_millis(200);

/// How long to keep reading after killing a timed-out child, so output it
/// wrote just before the deadline still reaches the terminal. Bounded so a
/// grandchild that keeps the terminal open (Linux only reports exit once
/// every holder closes it) or keeps writing cannot stall the tool.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// The production [`Graces`].
const GRACES: Graces = Graces {
    hangup: HANGUP_GRACE,
    kill: KILL_GRACE,
};

/// Target size of a slice of output fed to [`Terminal::advance`] between
/// two deadline checks. One escape sequence can cost a full-screen operation
/// (`CSI L`, `CSI 2J`), so a whole read chunk on a large grid could overshoot
/// the deadline by seconds; a small slice bounds the overshoot to a few
/// hundred such operations. Slices end on a UTF-8 character boundary (see
/// [`slice_end`]), so they may be up to three bytes shorter.
const FEED_SLICE: usize = 512;

/// What to run and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    /// Command line, run as `/bin/sh -c -- <command>`.
    pub command: String,
    /// Terminal width in columns; must be non-zero.
    pub cols: u16,
    /// Terminal height in rows; must be non-zero.
    pub rows: u16,
    /// How long to wait for the child to exit before killing it.
    pub timeout: Duration,
}

impl RunOptions {
    /// Options for `command` on an 80x24 terminal with the
    /// [`DEFAULT_TIMEOUT`].
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cols: 80,
            rows: 24,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// How the child's run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Completion {
    /// The child exited (or was killed by a signal) on its own.
    Exited(ExitStatus),
    /// The timeout expired and the child was killed.
    TimedOut,
}

/// The result of [`run`]: the terminal after the last byte of output and how
/// the child ended.
pub struct Outcome {
    /// The terminal, fed with everything the child wrote.
    pub terminal: Terminal,
    /// How the child ended.
    pub completion: Completion,
}

impl Outcome {
    /// The exit code a wrapper should report: the child's own code, `128 +
    /// signal` when a signal ended it (the shell convention), or
    /// [`TIMEOUT_EXIT_CODE`] when it timed out.
    pub fn exit_code(&self) -> i32 {
        match &self.completion {
            Completion::Exited(ExitStatus::Exited(code)) => *code,
            Completion::Exited(ExitStatus::Signaled(signal)) => 128_i32.saturating_add(*signal),
            Completion::TimedOut => TIMEOUT_EXIT_CODE,
        }
    }
}

impl fmt::Debug for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `Terminal` has no `Debug`; its size identifies it well enough.
        let grid = self.terminal.grid();
        f.debug_struct("Outcome")
            .field("terminal", &format_args!("{}x{}", grid.cols(), grid.rows()))
            .field("completion", &self.completion)
            .finish()
    }
}

/// Why [`run`] could not produce an [`Outcome`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The PTY could not be opened or the shell could not be started.
    #[error("cannot start {SHELL}: {0}")]
    Spawn(#[source] PtyError),
    /// The PTY's reader stopped without reporting the child's exit.
    #[error("the pty reader stopped before the child exited")]
    Disconnected,
}

/// Runs `options.command` in a fresh PTY and returns the terminal it drew
/// on together with how the child ended.
///
/// The command is passed as `/bin/sh -c -- <command>`, so a command that
/// starts with `-` is a command, not a shell option.
///
/// Waits about [`RunOptions::timeout`] for the child to exit, even while it
/// floods the terminal with output. Then it sends `SIGHUP`, gives the child
/// 200 ms to exit (still reading its output), sends `SIGKILL` if it has not,
/// keeps reading for up to one more second and returns
/// [`Completion::TimedOut`]. The timeout covers the emulator processing the
/// output too: a child that exits in time while the tool is still behind on
/// its output is reported as timed out (exit code 124).
///
/// The deadlines are checked between slices of about 512 bytes of output,
/// so each can be overshot by the time one slice takes to process. That is
/// microseconds normally, but on extreme sizes one slice can take seconds
/// (a 2000-column repeat such as `a ESC[65535b`, or DECALN on a 2000x2000
/// screen), and the bound stretches accordingly.
///
/// Only the direct child (the `/bin/sh` running the command) is signalled:
/// `tessera-pty` has no process-group signal, so processes it started are
/// left to the hangup the kernel sends when the shell, as session leader,
/// dies.
///
/// # Errors
///
/// [`Error::Spawn`] if the size has a zero dimension, no PTY is available
/// or `/bin/sh` cannot be started; [`Error::Disconnected`] if the PTY's
/// event stream ends without an exit status.
pub fn run(options: &RunOptions) -> Result<Outcome, Error> {
    let mut spawn = SpawnOptions::command(SHELL, session_id())
        .arg("-c")
        .arg("--")
        .arg(&options.command)
        .size(PtySize::new(options.rows, options.cols));
    // Without a working directory tessera-pty starts the child in $HOME; a
    // debugging tool should run where it was invoked.
    if let Ok(dir) = env::current_dir() {
        spawn = spawn.cwd(dir);
    }
    let (mut pty, events) = NativePty::spawn(&spawn).map_err(Error::Spawn)?;
    let terminal = Terminal::new(options.rows, options.cols);
    drive(&events, terminal, options.timeout, GRACES, |stop| {
        // Both are no-ops for a child that is already being reaped, and if
        // `kill(2)` itself fails there is nothing better to do than the
        // bounded drains and the hangup when `pty` drops.
        let _ = match stop {
            Stop::Hangup => pty.hangup(),
            Stop::Kill => pty.kill(),
        };
    })
}

/// A signal [`drive`] asks for when the child times out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// `SIGHUP`: ask the child to finish.
    Hangup,
    /// `SIGKILL`: end it.
    Kill,
}

/// How long [`drive`] keeps reading after each [`Stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Graces {
    /// After [`Stop::Hangup`], before escalating to [`Stop::Kill`].
    hangup: Duration,
    /// After [`Stop::Kill`], before giving up on the rest of the output.
    kill: Duration,
}

/// Feeds `events` to `terminal` until the child exits or `timeout` passes.
/// On timeout asks `stop` for a hangup and keeps feeding until the child
/// exits or `graces.hangup` passes; then asks for a kill and keeps feeding
/// for at most `graces.kill` more. Each phase starts with the part of a
/// chunk the previous one was interrupted in, so the stream stays
/// contiguous.
fn drive(
    events: &PtyEvents,
    mut terminal: Terminal,
    timeout: Duration,
    graces: Graces,
    mut stop: impl FnMut(Stop),
) -> Result<Outcome, Error> {
    let mut unfed = Vec::new();
    let mut pump_for = |terminal: &mut Terminal, deadline: Option<Instant>| {
        pump(events, terminal, &mut unfed, deadline)
    };
    // A timeout too large to represent as an instant means "no timeout".
    if let Some(status) = pump_for(&mut terminal, Instant::now().checked_add(timeout))? {
        return Ok(Outcome {
            terminal,
            completion: Completion::Exited(status),
        });
    }
    // Output that arrives after the deadline still counts; the exit status
    // does not, because the tool reports a timeout either way.
    stop(Stop::Hangup);
    let hung_up = pump_for(&mut terminal, Instant::now().checked_add(graces.hangup))?;
    if hung_up.is_none() {
        stop(Stop::Kill);
        pump_for(&mut terminal, Instant::now().checked_add(graces.kill))?;
    }
    Ok(Outcome {
        terminal,
        completion: Completion::TimedOut,
    })
}

/// Feeds `unfed`, then each received chunk of output, to `terminal` until
/// the child exits (returning its status) or `deadline` passes (returning
/// `None`). No deadline waits for the exit.
///
/// The deadline is checked before every receive and between slices of about
/// [`FEED_SLICE`] bytes, so a child that writes faster than the terminal
/// can parse still cannot hold the loop past it. When the deadline passes
/// in the middle of a chunk, the rest stays in `unfed` for the next call.
fn pump(
    events: &PtyEvents,
    terminal: &mut Terminal,
    unfed: &mut Vec<u8>,
    deadline: Option<Instant>,
) -> Result<Option<ExitStatus>, Error> {
    let expired = || deadline.is_some_and(|deadline| Instant::now() >= deadline);
    loop {
        if !feed(terminal, unfed, expired) || expired() {
            return Ok(None);
        }
        let event = match deadline {
            Some(deadline) => {
                events.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            }
            None => events.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match event {
            // `unfed` is empty here: `feed` returned true.
            Ok(PtyEvent::Output(bytes)) => *unfed = bytes,
            Ok(PtyEvent::Exited(status)) => return Ok(Some(status)),
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => return Err(Error::Disconnected),
        }
    }
}

/// Feeds `unfed` to `terminal` slice by slice. Returns `true` once all of
/// it is fed, or `false` when `expired` reports the deadline first; the
/// bytes not yet fed then remain in `unfed`.
fn feed(terminal: &mut Terminal, unfed: &mut Vec<u8>, expired: impl Fn() -> bool) -> bool {
    let mut fed = 0;
    while fed < unfed.len() {
        if expired() {
            unfed.drain(..fed);
            return false;
        }
        let end = fed + slice_end(&unfed[fed..]);
        terminal.advance(&unfed[fed..end]);
        // Bells and title changes have no effect on a dump; drain them so
        // a chatty child cannot grow the queue.
        drop(terminal.take_requests());
        fed = end;
    }
    unfed.clear();
    true
}

/// Length of the next slice of `bytes` to feed: everything when it is at
/// most [`FEED_SLICE`] long, else [`FEED_SLICE`] backed up (by at most three
/// bytes) so the slice does not split a UTF-8 character. Splitting would be
/// correct, because [`Terminal::advance`] carries a partial character over,
/// but it takes a slower path. Bytes that are not UTF-8 are cut at
/// [`FEED_SLICE`].
fn slice_end(bytes: &[u8]) -> usize {
    if bytes.len() <= FEED_SLICE {
        return bytes.len();
    }
    let is_continuation = |byte: u8| byte & 0b1100_0000 == 0b1000_0000;
    (FEED_SLICE - 3..=FEED_SLICE)
        .rev()
        .find(|&end| !is_continuation(bytes[end]))
        .unwrap_or(FEED_SLICE)
}

/// A session identifier unique to this call: process id plus a counter, so
/// concurrent runs (parallel tests) never share one.
fn session_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("devtool-{}-{n}", std::process::id())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use tessera_term::Request;

    use super::*;

    fn screen(terminal: &Terminal) -> String {
        crate::dump::text(terminal.grid())
    }

    #[test]
    fn grace_drain_keeps_output_queued_at_the_deadline() {
        // The output is already queued when the (zero) timeout expires, so
        // only the drain after the kill can feed it.
        let (tx, rx) = mpsc::sync_channel(4);
        tx.send(PtyEvent::Output(b"late".to_vec())).expect("queued");
        tx.send(PtyEvent::Exited(ExitStatus::Signaled(9)))
            .expect("queued");
        let mut stops = Vec::new();
        let outcome = drive(&rx, Terminal::new(1, 8), Duration::ZERO, GRACES, |stop| {
            stops.push(stop);
        })
        .expect("the stream ends with an exit");
        // The exit arrived during the hangup grace: no kill needed.
        assert_eq!(stops, [Stop::Hangup]);
        assert_eq!(outcome.completion, Completion::TimedOut);
        assert_eq!(outcome.exit_code(), TIMEOUT_EXIT_CODE);
        assert_eq!(screen(&outcome.terminal), "late\n");
    }

    #[test]
    fn exit_before_the_deadline_is_not_a_timeout() {
        let (tx, rx) = mpsc::sync_channel(4);
        tx.send(PtyEvent::Output(b"done".to_vec())).expect("queued");
        tx.send(PtyEvent::Exited(ExitStatus::Exited(4)))
            .expect("queued");
        let outcome = drive(&rx, Terminal::new(1, 8), DEFAULT_TIMEOUT, GRACES, |stop| {
            panic!("a finished child gets no {stop:?}")
        })
        .expect("the stream ends with an exit");
        assert_eq!(
            outcome.completion,
            Completion::Exited(ExitStatus::Exited(4))
        );
        assert_eq!(screen(&outcome.terminal), "done\n");
    }

    #[test]
    fn stream_without_exit_is_an_error() {
        let (tx, rx) = mpsc::sync_channel(4);
        tx.send(PtyEvent::Output(b"x".to_vec())).expect("queued");
        drop(tx);
        let result = drive(&rx, Terminal::new(1, 8), DEFAULT_TIMEOUT, GRACES, |_| {});
        assert!(matches!(result, Err(Error::Disconnected)), "{result:?}");
    }

    #[test]
    fn requests_are_drained_while_feeding() {
        // A BEL flood and a title change leave nothing queued behind.
        let (tx, rx) = mpsc::sync_channel(4);
        tx.send(PtyEvent::Output([b'\x07'; 3000].to_vec()))
            .expect("queued");
        tx.send(PtyEvent::Output(b"\x1b]2;title\x07".to_vec()))
            .expect("queued");
        tx.send(PtyEvent::Exited(ExitStatus::Exited(0)))
            .expect("queued");
        let mut outcome = drive(&rx, Terminal::new(1, 8), DEFAULT_TIMEOUT, GRACES, |_| {})
            .expect("the stream ends with an exit");
        assert_eq!(outcome.terminal.take_requests(), Vec::<Request>::new());
    }

    fn graces(hangup_ms: u64, kill_ms: u64) -> Graces {
        Graces {
            hangup: Duration::from_millis(hangup_ms),
            kill: Duration::from_millis(kill_ms),
        }
    }

    #[test]
    fn child_that_honours_the_hangup_is_not_killed() {
        let (tx, rx) = mpsc::sync_channel(4);
        let mut stops = Vec::new();
        let outcome = drive(&rx, Terminal::new(1, 8), Duration::ZERO, GRACES, |stop| {
            stops.push(stop);
            if stop == Stop::Hangup {
                tx.send(PtyEvent::Output(b"bye".to_vec())).expect("queued");
                tx.send(PtyEvent::Exited(ExitStatus::Exited(0)))
                    .expect("queued");
            }
        })
        .expect("the stream ends with an exit");
        assert_eq!(stops, [Stop::Hangup]);
        assert_eq!(outcome.completion, Completion::TimedOut);
        assert_eq!(screen(&outcome.terminal), "bye\n");
    }

    #[test]
    fn child_that_ignores_the_hangup_is_killed() {
        // Nothing ever arrives, but the sender stays alive: only the graces
        // end the run.
        let (_tx, rx) = mpsc::sync_channel::<PtyEvent>(4);
        let mut stops = Vec::new();
        let started = Instant::now();
        let outcome = drive(
            &rx,
            Terminal::new(1, 8),
            Duration::ZERO,
            graces(20, 20),
            |stop| {
                stops.push(stop);
            },
        )
        .expect("a quiet child is not an error");
        assert_eq!(stops, [Stop::Hangup, Stop::Kill]);
        assert_eq!(outcome.completion, Completion::TimedOut);
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    /// `n` copies of insert-line plus erase-display: two full-screen
    /// operations every seven bytes.
    fn heavy(n: usize) -> Vec<u8> {
        b"\x1b[L\x1b[2J".repeat(n)
    }

    #[test]
    fn deadline_interrupts_a_single_huge_chunk() {
        // One 2 MB chunk on a 300x300 screen takes far longer than the
        // timeout to parse; only the check between slices can stop it.
        let (tx, rx) = mpsc::sync_channel(4);
        tx.send(PtyEvent::Output(heavy(2_000_000 / 7)))
            .expect("queued");
        tx.send(PtyEvent::Exited(ExitStatus::Exited(0)))
            .expect("queued");
        let started = Instant::now();
        let outcome = drive(
            &rx,
            Terminal::new(300, 300),
            Duration::from_millis(50),
            graces(25, 25),
            |_| {},
        )
        .expect("the stream ends with an exit");
        let elapsed = started.elapsed();
        assert_eq!(outcome.completion, Completion::TimedOut);
        assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
    }

    #[test]
    fn grace_drain_resumes_an_interrupted_chunk() {
        // The timeout expires inside the first chunk; the grace drain must
        // feed the rest of that chunk (ending in `tail`) before the next
        // one (`!`), so the screen shows the stream in order.
        let (tx, rx) = mpsc::sync_channel(4);
        let mut first = heavy(FEED_SLICE * 50 / 7);
        first.extend_from_slice(b"\x1b[2J\x1b[Htail");
        tx.send(PtyEvent::Output(first)).expect("queued");
        tx.send(PtyEvent::Output(b"!".to_vec())).expect("queued");
        tx.send(PtyEvent::Exited(ExitStatus::Exited(0)))
            .expect("queued");
        let outcome = drive(
            &rx,
            Terminal::new(100, 100),
            Duration::from_millis(1),
            graces(10_000, 10_000),
            |_| {},
        )
        .expect("the stream ends with an exit");
        assert_eq!(outcome.completion, Completion::TimedOut);
        let screen = screen(&outcome.terminal);
        assert_eq!(screen.lines().next(), Some("tail!"));
    }

    #[test]
    fn slices_end_on_character_boundaries() {
        assert_eq!(slice_end(b"short"), 5);
        assert_eq!(slice_end(&[b'a'; FEED_SLICE + 1]), FEED_SLICE);
        // Three-byte characters (U+65E5): FEED_SLICE = 512 = 3 * 170 + 2, so
        // a plain cut would land inside the 171st character.
        let cjk = "日".repeat(FEED_SLICE);
        let end = slice_end(cjk.as_bytes());
        assert_eq!(end, 510);
        assert!(cjk.is_char_boundary(end));
        // Four-byte characters: 512 is a boundary already.
        let emoji = "😀".repeat(FEED_SLICE);
        assert_eq!(slice_end(emoji.as_bytes()), FEED_SLICE);
        // Not UTF-8 at all: cut at the target size.
        assert_eq!(slice_end(&[0x80; FEED_SLICE * 2]), FEED_SLICE);
    }

    #[test]
    fn signal_exit_code_saturates() {
        let outcome = Outcome {
            terminal: Terminal::new(1, 1),
            completion: Completion::Exited(ExitStatus::Signaled(i32::MAX)),
        };
        assert_eq!(outcome.exit_code(), i32::MAX);
        let outcome = Outcome {
            completion: Completion::Exited(ExitStatus::Signaled(9)),
            ..outcome
        };
        assert_eq!(outcome.exit_code(), 137);
    }
}
