//! Sessions: one child process in a PTY plus the terminal that interprets
//! its output.
//!
//! Every session owns a reader thread that pulls [`PtyEvent`]s from
//! `tessera-pty`, feeds output into its [`Terminal`] and turns what the
//! terminal asks for into [`SessionEvent`]s pushed to the application's
//! [`SessionObserver`]. Nothing above this module polls a channel.
//!
//! # Input
//!
//! Input from the frontend (keys, pastes) is never written on the caller's
//! thread. It is appended to the session's *outbox*, bounded to
//! [`PENDING_INPUT_BYTES`], and a per-session writer thread feeds the outbox
//! into the PTY in order, waiting whenever the PTY's own write queue is
//! full. Queueing either takes all of an input or none of it, so a key
//! queued after a paste always reaches the child after the paste, and the
//! frontend's UI thread never waits for a child that stopped reading.
//! [`App::write`](crate::App::write) bypasses the outbox and writes
//! directly, waiting as needed, so its bytes may overtake or interleave
//! with pending input.
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
//! longer reach the observer. Muting also closes the outbox: input still
//! pending is dropped, and the writer thread stops within one write backoff
//! period and is joined.
//!
//! # Locks
//!
//! A session has six locks: the PTY, the terminal, the outbox, the reader
//! and writer handles and the input state (focus, scrollback view, last
//! mouse position). No code path holds two of them at once: input handling reads what it needs from
//! the terminal (grid size, scrollback length) under the terminal lock,
//! releases it, then takes the input lock. A value read that way can be
//! stale by the time it is used (the reader thread may have added a
//! scrollback line in between), so readers of the input state re-clamp it
//! against the current terminal (see `Session::scroll_offset`). The PTY
//! lock is only ever held for one non-blocking attempt, and the writer
//! thread releases the outbox lock before it writes.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tessera_pty::{ExitStatus, Pty, PtyEvent, PtyEvents, PtySize};
use tessera_term::{Request, Terminal};

use crate::input::MouseEvent;

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

/// Most bytes of frontend input (keys, pastes) a session holds while its
/// child is not reading, on top of the PTY's own write queue. Input that
/// would exceed it is refused whole.
///
/// Counts payload bytes, not memory: each queued input also costs its own
/// allocation and queue bookkeeping (about 40 bytes), so many one-byte keys
/// use far more memory than this.
pub const PENDING_INPUT_BYTES: usize = 16 * 1024 * 1024;

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
    pty: Arc<Mutex<Box<dyn Pty>>>,
    term: Arc<Mutex<Terminal>>,
    /// Set when the application starts closing the session; the reader
    /// thread stops delivering events and pending writes give up.
    muted: Arc<AtomicBool>,
    reader: Mutex<Option<Reader>>,
    outbox: Arc<Outbox>,
    writer: Mutex<Option<JoinHandle<()>>>,
    input: Mutex<InputState>,
}

/// Frontend input waiting for the writer thread, shared with it.
#[derive(Default)]
struct Outbox {
    state: Mutex<OutboxState>,
    /// Signalled when input arrives or the outbox closes.
    ready: Condvar,
}

#[derive(Default)]
struct OutboxState {
    /// Inputs in arrival order, each written whole before the next.
    inputs: VecDeque<Vec<u8>>,
    /// Bytes queued or being written; at most [`PENDING_INPUT_BYTES`].
    bytes: usize,
    /// Set by a close; the writer stops and new input is refused.
    closed: bool,
    /// Why the writer stopped, if writing failed (the child exited). New
    /// input is refused with the same kind of error.
    failed: Option<io::ErrorKind>,
}

impl Outbox {
    /// Appends `bytes` whole, or refuses them without queueing anything.
    fn push(&self, bytes: Vec<u8>) -> io::Result<()> {
        let mut state = lock(&self.state);
        if state.closed {
            return Err(closing_error());
        }
        if let Some(kind) = state.failed {
            return Err(io::Error::new(kind, "the session's input writer stopped"));
        }
        if bytes.len() > PENDING_INPUT_BYTES - state.bytes {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the session's pending input is full",
            ));
        }
        state.bytes += bytes.len();
        state.inputs.push_back(bytes);
        self.ready.notify_one();
        Ok(())
    }

    /// Stops the writer and drops pending input.
    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.inputs.clear();
        state.bytes = 0;
        self.ready.notify_all();
    }
}

/// The writer thread of a session: drains the outbox into the PTY.
struct Writer {
    pty: Arc<Mutex<Box<dyn Pty>>>,
    muted: Arc<AtomicBool>,
    outbox: Arc<Outbox>,
}

impl Writer {
    fn run(&self) {
        loop {
            let input = {
                let mut state = lock(&self.outbox.state);
                loop {
                    if state.closed {
                        return;
                    }
                    if let Some(input) = state.inputs.pop_front() {
                        break input;
                    }
                    state = self
                        .outbox
                        .ready
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            };
            // A panic must not leave the outbox accepting input that no
            // thread will ever write.
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                write_blocking(&self.pty, &self.muted, &input)
            }))
            .unwrap_or_else(|_| Err(io::Error::other("the session's input writer panicked")));
            let mut state = lock(&self.outbox.state);
            state.bytes = state.bytes.saturating_sub(input.len());
            if let Err(err) = result {
                state.failed = Some(err.kind());
                state.inputs.clear();
                state.bytes = 0;
                return;
            }
        }
    }
}

fn closing_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "session is closing")
}

/// Queues `bytes` for the child, waiting while the PTY's write queue is
/// full, until `muted` is set.
///
/// The PTY lock is only held for each non-blocking attempt, so a child
/// that stops reading cannot keep a resize or a close from reaching the
/// PTY.
fn write_blocking(pty: &Mutex<Box<dyn Pty>>, muted: &AtomicBool, bytes: &[u8]) -> io::Result<()> {
    for chunk in bytes.chunks(WRITE_CHUNK) {
        let mut backoff = WRITE_BACKOFF_MIN;
        loop {
            if muted.load(Ordering::Acquire) {
                return Err(closing_error());
            }
            // Bound first so the guard is dropped before a backoff sleep:
            // a `match` scrutinee's temporaries live through every arm.
            let attempt = lock(pty).try_write(chunk);
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

/// Per-session state the frontend's input events maintain.
#[derive(Debug, Default)]
struct InputState {
    /// Whether the session's view has keyboard focus. A new session does
    /// not until the frontend says so.
    focused: bool,
    /// Lines the view is scrolled back from the bottom; 0 shows the live
    /// screen. Anchored to the bottom, so the view drifts while output
    /// arrives (keeping it on its content is #31). May exceed the
    /// scrollback length after the scrollback shrinks until the next read,
    /// which clamps it and stores the result.
    scroll_offset: usize,
    /// Fraction of a line scrolled but not yet applied, in `(-1, 1)`.
    scroll_remainder: f64,
    /// The last mouse event, its cell clamped to the grid.
    last_mouse: Option<MouseEvent>,
}

/// The reader thread of a session.
struct Reader {
    handle: JoinHandle<()>,
    /// Never receives a value; disconnects when the reader thread ends,
    /// which makes it a join with a timeout.
    done: Receiver<()>,
}

impl Session {
    /// Wraps a spawned PTY and starts the writer thread for its input and
    /// the reader thread for its events.
    pub(crate) fn start(
        id: SessionId,
        pty: Box<dyn Pty>,
        events: PtyEvents,
        size: PtySize,
        observers: ObserverSlot,
    ) -> io::Result<Self> {
        let pty = Arc::new(Mutex::new(pty));
        let term = Arc::new(Mutex::new(Terminal::new(size.rows, size.cols)));
        let muted = Arc::new(AtomicBool::new(false));
        let outbox = Arc::new(Outbox::default());
        let writer = Writer {
            pty: Arc::clone(&pty),
            muted: Arc::clone(&muted),
            outbox: Arc::clone(&outbox),
        };
        let writer = thread::Builder::new()
            .name(format!("tessera-input-{id}"))
            .spawn(move || writer.run())?;
        let (done_tx, done) = mpsc::channel();
        let pump = Pump {
            id,
            term: Arc::clone(&term),
            muted: Arc::clone(&muted),
            observers,
            _done: done_tx,
        };
        let handle = match thread::Builder::new()
            .name(format!("tessera-session-{id}"))
            .spawn(move || pump.run(&events))
        {
            Ok(handle) => handle,
            Err(err) => {
                // The writer is idle; closing the outbox ends it, and the
                // PTY (hung up when dropped) goes with it.
                outbox.close();
                return Err(err);
            }
        };
        Ok(Self {
            id,
            pty,
            term,
            muted,
            reader: Mutex::new(Some(Reader { handle, done })),
            outbox,
            writer: Mutex::new(Some(writer)),
            input: Mutex::new(InputState::default()),
        })
    }

    /// Queues `bytes` (one key, one paste) for the writer thread; never
    /// waits. Takes all of `bytes` or none: fails with
    /// [`io::ErrorKind::WouldBlock`] when they do not fit in what is left of
    /// [`PENDING_INPUT_BYTES`], and with [`io::ErrorKind::BrokenPipe`] once
    /// the child has exited or the session is closing.
    pub(crate) fn queue_input(&self, bytes: Vec<u8>) -> io::Result<()> {
        if self.muted.load(Ordering::Acquire) {
            return Err(closing_error());
        }
        if lock(&self.pty).exit_status().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child has exited",
            ));
        }
        self.outbox.push(bytes)
    }

    /// Queues `bytes` for the child, waiting while the PTY's write queue is
    /// full. Does not go through the outbox, so it may overtake or interleave
    /// with pending frontend input (both feed the PTY in [`WRITE_CHUNK`]
    /// pieces).
    ///
    /// The PTY lock is only held for each non-blocking attempt, so a child
    /// that stops reading cannot keep [`Session::resize`] or a close from
    /// reaching the PTY.
    pub(crate) fn write(&self, bytes: &[u8]) -> io::Result<()> {
        write_blocking(&self.pty, &self.muted, bytes)
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

    /// Whether the session's view has keyboard focus.
    pub(crate) fn is_focused(&self) -> bool {
        lock(&self.input).focused
    }

    /// Records whether the session's view has keyboard focus.
    pub(crate) fn set_focused(&self, focused: bool) {
        lock(&self.input).focused = focused;
    }

    /// Lines the view is scrolled back, clamped to the current scrollback
    /// (and stored clamped).
    pub(crate) fn scroll_offset(&self) -> usize {
        let max = self.with_terminal(|term| term.grid().scrollback_len());
        let mut input = lock(&self.input);
        if input.scroll_offset > max {
            input.scroll_offset = max;
            input.scroll_remainder = 0.0;
        }
        input.scroll_offset
    }

    /// Moves the view `delta` lines towards older output (negative: towards
    /// the live screen), keeping the fraction for the next call and
    /// clamping to `0..=scrollback_len`. Returns whether the visible offset
    /// changed. A `delta` that is not finite does nothing.
    pub(crate) fn scroll(&self, delta: f32) -> bool {
        if !delta.is_finite() {
            return false;
        }
        let max = self.with_terminal(|term| term.grid().scrollback_len());
        let mut input = lock(&self.input);
        let current = input.scroll_offset.min(max);
        let total = input.scroll_remainder + f64::from(delta);
        let whole = total.trunc();
        let mut remainder = total - whole;
        // `whole` is integral and `as` saturates, so huge deltas pin to the
        // ends below instead of wrapping.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "float-to-int `as` saturates and `whole` is integral"
        )]
        let steps = whole as i64;
        let distance = usize::try_from(steps.unsigned_abs()).unwrap_or(usize::MAX);
        let target = if steps >= 0 {
            current.saturating_add(distance).min(max)
        } else {
            current.saturating_sub(distance)
        };
        // At either end, a fraction pushing further out is dropped so that
        // reversing direction responds at once.
        if (target == max && remainder > 0.0) || (target == 0 && remainder < 0.0) {
            remainder = 0.0;
        }
        input.scroll_offset = target;
        input.scroll_remainder = remainder;
        target != current
    }

    /// Returns the view to the live screen, as typing does.
    pub(crate) fn scroll_to_bottom(&self) {
        let mut input = lock(&self.input);
        input.scroll_offset = 0;
        input.scroll_remainder = 0.0;
    }

    /// Clamps `event`'s cell to the grid and records it as the last mouse
    /// event.
    pub(crate) fn record_mouse(&self, mut event: MouseEvent) {
        let (rows, cols) = self.with_terminal(|term| (term.grid().rows(), term.grid().cols()));
        event.col = event.col.min(cols.saturating_sub(1));
        event.row = event.row.min(rows.saturating_sub(1));
        lock(&self.input).last_mouse = Some(event);
    }

    /// The last mouse event recorded, if any.
    pub(crate) fn last_mouse(&self) -> Option<MouseEvent> {
        lock(&self.input).last_mouse
    }

    /// First half of a close: stops event delivery and hangs up the child.
    ///
    /// The caller must take the observer slot's write lock after muting
    /// every session it closes and before waiting on any of them, so no
    /// observer call for a muted session is still running.
    pub(crate) fn begin_close(&self) {
        self.muted.store(true, Ordering::Release);
        self.outbox.close();
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
        // Muted and with its outbox closed, the writer ends within one
        // backoff period.
        let writer = lock(&self.writer).take();
        if let Some(writer) = writer
            && writer.join().is_err()
        {
            tracing::warn!(session = %self.id, "session writer thread panicked");
        }
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

impl Drop for Session {
    /// Stops the writer thread even if the session was never closed (it
    /// holds the PTY until it stops).
    fn drop(&mut self) {
        self.outbox.close();
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
