//! Sessions: one child process in a PTY plus the terminal that interprets
//! its output.
//!
//! Every session owns a reader thread that pulls [`PtyEvent`]s from
//! `tessera-pty`, feeds output into its [`Terminal`] and turns what the
//! terminal asks for into [`SessionEvent`]s pushed to the application's
//! [`SessionObserver`]. Nothing above this module polls a channel.
//!
//! # Closing
//!
//! Closing a session *mutes* it (no observer call for it starts after that
//! point) and hangs up the child (`SIGHUP`), then waits for an observer call
//! already in progress to return, waits up to [`HANGUP_GRACE`] for the child
//! to exit, escalates to `SIGKILL` and waits up to [`KILL_GRACE`] more, and
//! finally joins the reader thread. A child that survives even `SIGKILL`'s
//! grace period (on Linux a grandchild can hold the terminal open) is
//! detached rather than allowed to hang the caller; being muted, it can no
//! longer reach the observer.

use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tessera_pty::{ExitStatus, Pty, PtyEvent, PtyEvents, PtySize};
use tessera_term::{Request, Terminal};

/// How long a closing session's child gets to exit after `SIGHUP` before it
/// is sent `SIGKILL`.
pub const HANGUP_GRACE: Duration = Duration::from_secs(1);

/// How long a closing session waits after `SIGKILL` before it detaches the
/// reader thread instead of joining it.
pub const KILL_GRACE: Duration = Duration::from_secs(1);

/// Largest piece of input offered to the PTY's write queue at once. Smaller
/// than the queue's budget so a large paste makes progress as the child
/// drains it instead of waiting for the queue to empty completely.
const WRITE_CHUNK: usize = 64 * 1024;

/// First and last pause between attempts while the PTY's write queue is full.
const WRITE_BACKOFF_MIN: Duration = Duration::from_millis(1);
const WRITE_BACKOFF_MAX: Duration = Duration::from_millis(20);

/// Identifies a session within one [`App`](crate::App).
///
/// Ids are handed out in increasing order starting at `1` and never reused
/// while the application lives, so `0` never names a session and frontends
/// can use it as "none". The id is also exported to the child as
/// `TESSERA_SESSION_ID`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(u64);

impl SessionId {
    /// Wraps a raw id, for example one received across the FFI.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw id.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Something that happened in a session, reported to the
/// [`SessionObserver`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// The child wrote output and the terminal has applied it; the frontend
    /// should redraw.
    Output,
    /// The child asked for a new window or tab title.
    Title(String),
    /// The child rang the bell.
    Bell,
    /// The child exited. This is the last event of a session that was not
    /// closed by the application first.
    Exit(ExitStatus),
}

/// Receives [`SessionEvent`]s.
///
/// Called on the session's reader thread, never on the thread that drives
/// the [`App`](crate::App). Implementations must return quickly and must not
/// call back into the `App` or wait for the thread that does: closing a
/// session, replacing the observer and dropping the `App` wait for a call in
/// progress to return, so doing any of them from (or while blocking) an
/// observer call deadlocks.
///
/// Any `Fn(SessionId, SessionEvent) + Send + Sync` closure is an observer.
pub trait SessionObserver: Send + Sync {
    /// Handles one event of session `id`.
    fn on_session_event(&self, id: SessionId, event: SessionEvent);
}

impl<F> SessionObserver for F
where
    F: Fn(SessionId, SessionEvent) + Send + Sync,
{
    fn on_session_event(&self, id: SessionId, event: SessionEvent) {
        self(id, event);
    }
}

/// The application's observer, shared with every reader thread.
///
/// Readers hold the read lock for the duration of an observer call, so
/// taking the write lock is a barrier: once it is acquired no observer call
/// is in flight.
pub(crate) type ObserverSlot = Arc<RwLock<Option<Box<dyn SessionObserver>>>>;

/// Locks `mutex`, recovering the data from a poisoned lock: a panic caught
/// at the FFI boundary must leave the application usable.
pub(crate) fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running (or exited but not yet closed) session.
pub(crate) struct Session {
    id: SessionId,
    pty: Mutex<Box<dyn Pty>>,
    term: Arc<Mutex<Terminal>>,
    /// Set when the application starts closing the session; the reader
    /// thread stops delivering events and pending writes give up.
    muted: Arc<AtomicBool>,
    reader: Mutex<Option<Reader>>,
}

/// The reader thread of a session.
struct Reader {
    handle: JoinHandle<()>,
    /// Never receives a value; disconnects when the reader thread ends,
    /// which makes it a join with a timeout.
    done: Receiver<()>,
}

impl Session {
    /// Wraps a spawned PTY and starts the reader thread for its events.
    pub(crate) fn start(
        id: SessionId,
        pty: Box<dyn Pty>,
        events: PtyEvents,
        size: PtySize,
        observers: ObserverSlot,
    ) -> io::Result<Self> {
        let term = Arc::new(Mutex::new(Terminal::new(size.rows, size.cols)));
        let muted = Arc::new(AtomicBool::new(false));
        let (done_tx, done) = mpsc::channel();
        let pump = Pump {
            id,
            term: Arc::clone(&term),
            muted: Arc::clone(&muted),
            observers,
            _done: done_tx,
        };
        let handle = thread::Builder::new()
            .name(format!("tessera-session-{id}"))
            .spawn(move || pump.run(&events))?;
        Ok(Self {
            id,
            pty: Mutex::new(pty),
            term,
            muted,
            reader: Mutex::new(Some(Reader { handle, done })),
        })
    }

    /// Queues `bytes` for the child, waiting while the write queue is full.
    ///
    /// The PTY lock is only held for each non-blocking attempt, so a child
    /// that stops reading cannot keep [`Session::resize`] or a close from
    /// reaching the PTY.
    pub(crate) fn write(&self, bytes: &[u8]) -> io::Result<()> {
        for chunk in bytes.chunks(WRITE_CHUNK) {
            let mut backoff = WRITE_BACKOFF_MIN;
            loop {
                if self.muted.load(Ordering::Acquire) {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "session is closing",
                    ));
                }
                // Bound first so the guard is dropped before a backoff sleep:
                // a `match` scrutinee's temporaries live through every arm.
                let attempt = lock(&self.pty).try_write(chunk);
                match attempt {
                    Ok(()) => break,
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(backoff);
                        backoff = (backoff * 2).min(WRITE_BACKOFF_MAX);
                    }
                    Err(err) => return Err(err),
                }
            }
        }
        Ok(())
    }

    /// Resizes the PTY.
    pub(crate) fn resize(&self, size: PtySize) -> Result<(), tessera_pty::PtyError> {
        // TODO(#3): resize the terminal as well once `tessera_term::Terminal`
        // exposes a resize; until then its grid keeps the spawn size.
        lock(&self.pty).resize(size)
    }

    /// Runs `f` with the session's terminal locked.
    pub(crate) fn with_terminal<R>(&self, f: impl FnOnce(&Terminal) -> R) -> R {
        f(&lock(&self.term))
    }

    /// First half of a close: stops event delivery and hangs up the child.
    ///
    /// The caller must take the observer slot's write lock after muting
    /// every session it closes and before waiting on any of them, so no
    /// observer call for a muted session is still running.
    pub(crate) fn begin_close(&self) {
        self.muted.store(true, Ordering::Release);
        if let Err(err) = lock(&self.pty).hangup() {
            tracing::debug!(session = %self.id, %err, "hangup on close failed");
        }
    }

    /// Waits until `deadline` for the reader thread to end, which happens
    /// once the child has exited and its terminal is closed. Returns whether
    /// it ended. Returns `true` at once if the reader was already collected.
    pub(crate) fn wait_until(&self, deadline: Instant) -> bool {
        lock(&self.reader)
            .as_ref()
            .is_none_or(|reader| reader.wait_until(deadline))
    }

    /// Sends `SIGKILL` to a child that outlived its hangup grace period.
    pub(crate) fn kill(&self) {
        tracing::debug!(session = %self.id, "child ignored SIGHUP; killing");
        if let Err(err) = lock(&self.pty).kill() {
            tracing::debug!(session = %self.id, %err, "kill on close failed");
        }
    }

    /// Last step of a close: joins the reader thread if it has ended, or
    /// detaches it if the terminal is still held open (it is muted, so it
    /// can no longer reach the observer).
    pub(crate) fn join_or_detach(&self) {
        let Some(reader) = lock(&self.reader).take() else {
            return;
        };
        if !reader.wait_until(Instant::now()) {
            tracing::warn!(
                session = %self.id,
                "terminal still held open after SIGKILL; detaching reader thread"
            );
            return;
        }
        if reader.handle.join().is_err() {
            tracing::warn!(session = %self.id, "session reader thread panicked");
        }
    }
}

impl Reader {
    /// Waits until `deadline` for the reader thread to end.
    fn wait_until(&self, deadline: Instant) -> bool {
        match self
            .done
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
            Err(RecvTimeoutError::Timeout) => false,
        }
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("muted", &self.muted.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// State moved onto a session's reader thread.
struct Pump {
    id: SessionId,
    term: Arc<Mutex<Terminal>>,
    muted: Arc<AtomicBool>,
    observers: ObserverSlot,
    /// Dropped when the thread ends; see [`Reader::done`].
    _done: Sender<()>,
}

impl Pump {
    fn run(&self, events: &PtyEvents) {
        for event in events {
            match event {
                PtyEvent::Output(bytes) => {
                    let requests = {
                        let mut term = lock(&self.term);
                        term.advance(&bytes);
                        term.take_requests()
                    };
                    self.deliver(SessionEvent::Output);
                    for request in requests {
                        match request {
                            Request::Bell => self.deliver(SessionEvent::Bell),
                            Request::SetTitle(title) => self.deliver(SessionEvent::Title(title)),
                            // `Request` is non-exhaustive; requests this
                            // layer does not route yet are dropped.
                            _ => {}
                        }
                    }
                }
                PtyEvent::Exited(status) => {
                    self.deliver(SessionEvent::Exit(status));
                    break;
                }
            }
        }
    }

    /// Hands `event` to the observer unless the session is being closed.
    fn deliver(&self, event: SessionEvent) {
        let observers = self
            .observers
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        // Checked under the read lock: a closer mutes, then takes the write
        // lock, so once it holds that lock this check can no longer pass.
        if self.muted.load(Ordering::Acquire) {
            return;
        }
        let Some(observer) = observers.as_ref() else {
            return;
        };
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            observer.on_session_event(self.id, event);
        }));
        if outcome.is_err() {
            tracing::error!(session = %self.id, "session observer panicked; event dropped");
        }
    }
}
