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
//! A session is live until the application closes it, then *closing*
//! until the application's reaper has reaped it (ADR 0018). Closing never waits for
//! the child. [`App::close_session`](crate::App::close_session) *mutes* the
//! session (no observer call for it starts after that point), closes its
//! outbox (input still pending is dropped), hangs up the child (`SIGHUP`),
//! waits for an observer call of the session already in progress to return
//! and hands the session to the application's reaper thread. That thread
//! waits up to [`HANGUP_GRACE`] for the child to exit, escalates to
//! `SIGKILL` and waits up to [`KILL_GRACE`] more, then joins the session's
//! reader and writer threads. A child that survives even `SIGKILL`'s grace
//! period (on Linux a grandchild can hold the terminal open) has its
//! threads detached rather than joined; being muted, they can no longer
//! reach the observer. Dropping the `App` closes the remaining sessions the
//! same way and waits for the reaper to finish, which takes at most about
//! `HANGUP_GRACE + KILL_GRACE` however many sessions are closing.
//!
//! # Locks
//!
//! A session has seven locks: the PTY, the terminal, the outbox, the reader
//! and writer handles, the input state (focus, scrollback view, last
//! mouse position) and the delivery lock its reader thread holds around an
//! observer call (the barrier a close waits on). No code path holds two of
//! them at once, with one exception: a close made from inside one of the
//! session's own observer calls holds that call's delivery lock while it
//! takes the outbox and PTY locks (`Session::begin_close`). Nothing takes
//! the delivery lock while holding another session lock, so this cannot
//! form a cycle. Input handling reads what it needs from
//! the terminal (grid size, scrollback length) under the terminal lock,
//! releases it, then takes the input lock. A value read that way can be
//! stale by the time it is used (the reader thread may have added a
//! scrollback line in between), so readers of the input state re-clamp it
//! against the current terminal (see `Session::scroll_offset`). The PTY
//! lock is only ever held for one non-blocking attempt, and the writer
//! thread releases the outbox lock before it writes. The reader thread
//! holds the terminal lock for one slice of output at a time and, between
//! slices, lets a thread waiting for it go first (see `ADVANCE_SLICE`).

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, TryLockError};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use tessera_pty::{ExitStatus, Pty, PtyError, PtyEvent, PtyEvents, PtySize};
use tessera_term::{Request, Terminal};

use crate::input::MouseEvent;
use crate::view::{MAX_VIEW_COLS, MAX_VIEW_ROWS};

/// How long a closing session's child gets to exit after `SIGHUP` before it
/// is sent `SIGKILL`.
pub const HANGUP_GRACE: Duration = Duration::from_secs(1);

/// How long a closing session waits after `SIGKILL` before its reader and
/// writer threads are detached instead of joined.
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

/// Refuses a size no session may have with [`PtyError::InvalidSize`]:
/// zero rows or columns (programs divide by the window size), or more than
/// [`MAX_VIEW_ROWS`] rows or [`MAX_VIEW_COLS`] columns (the largest grid a
/// view derives: the grid allocates every row and every scrollback line at
/// full width, so 65535 × 65535 would need tens of gigabytes). Spawning
/// and resizing apply the same bound.
pub(crate) fn check_size(size: PtySize) -> Result<(), PtyError> {
    if size.rows == 0 || size.cols == 0 || size.rows > MAX_VIEW_ROWS || size.cols > MAX_VIEW_COLS {
        return Err(PtyError::InvalidSize(size));
    }
    Ok(())
}

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
/// observer call can deadlock. The one exception is closing the session
/// the call is for, which does not wait for the call it is made from.
///
/// Any `Fn(SessionId, SessionEvent) + Send + Sync` closure is an observer.
pub trait SessionObserver: Send + Sync {
    /// Handles one event of session `id`.
    fn on_session_event(&self, id: SessionId, event: SessionEvent);

    /// The appearance every view draws with changed (theme, opacity or
    /// font size, applied by [`App::apply_config`](crate::App::apply_config));
    /// `opaque` is whether the background is now fully opaque. The host
    /// makes its windows translucent or opaque to match and redraws its
    /// views: the core switches each surface's compositing mode at its
    /// next frame, and a host whose display link idles would otherwise not
    /// draw the new colours until the next output.
    ///
    /// Called on the thread that applied the change, the configuration
    /// watch's thread when the change came from a file
    /// ([`App::start_config_watch`](crate::App::start_config_watch)). No
    /// lock of the `App` is held but the observer slot's, so unlike a
    /// session event this call may call back into the `App`: its views and
    /// sizes, its configuration, [`App::apply_config`](crate::App::apply_config),
    /// closing a session, stopping or restarting the watch. It must not
    /// replace the observer (`set_observer` waits for this call) or drop
    /// the last reference to the `App` (dropping replaces the observer),
    /// and should return quickly. Does nothing by default.
    fn on_appearance_changed(&self, opaque: bool) {
        let _ = opaque;
    }
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

/// The hook [`set_thread_start_hook`] installed, if any.
static THREAD_START_HOOK: OnceLock<fn()> = OnceLock::new();

/// Installs `hook`, which every thread this crate starts (each session's
/// reader and input writer, and the application's reaper) runs first.
///
/// For thread attributes this crate cannot set itself: it forbids
/// `unsafe`, so `tessera-ffi` installs a hook that gives these threads a
/// quality-of-service class on macOS (ADR 0018). Process-wide; only the
/// first hook is kept, and threads already running are not affected.
/// Returns whether `hook` was installed.
pub fn set_thread_start_hook(hook: fn()) -> bool {
    THREAD_START_HOOK.set(hook).is_ok()
}

/// Runs the hook [`set_thread_start_hook`] installed, if any. A panicking
/// hook is logged and the thread carries on: a reaper that died here would
/// leave every closing child unkilled.
pub(crate) fn run_thread_start_hook() {
    if let Some(hook) = THREAD_START_HOOK.get()
        && panic::catch_unwind(*hook).is_err()
    {
        tracing::error!("thread start hook panicked");
    }
}

/// An event count the reaper sleeps on: bumped when a session thread ends,
/// when sessions are handed over for reaping and when the application
/// shuts down.
#[derive(Debug, Default)]
pub(crate) struct Signal {
    epoch: Mutex<u64>,
    changed: Condvar,
}

impl Signal {
    /// The current count. Read it before checking what a wake-up could
    /// change, then pass it to [`Signal::wait`], and no wake-up is lost.
    fn epoch(&self) -> u64 {
        *lock(&self.epoch)
    }

    /// Wakes every thread in [`Signal::wait`].
    fn notify(&self) {
        let mut epoch = lock(&self.epoch);
        *epoch = epoch.wrapping_add(1);
        self.changed.notify_all();
    }

    /// Waits until the count is no longer `seen`, or until `deadline` if
    /// there is one.
    fn wait(&self, seen: u64, deadline: Option<Instant>) {
        let mut epoch = lock(&self.epoch);
        while *epoch == seen {
            match deadline {
                None => {
                    epoch = self
                        .changed
                        .wait(epoch)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return;
                    }
                    epoch = self
                        .changed
                        .wait_timeout(epoch, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        }
    }
}

/// Dropped as the last thing a session thread does: disconnects
/// [`Worker::done`], then wakes the reaper.
struct Finished {
    done: Option<Sender<()>>,
    signal: Arc<Signal>,
}

impl Drop for Finished {
    fn drop(&mut self) {
        // Disconnect first, so a reaper woken by the notification sees the
        // thread as ended.
        drop(self.done.take());
        self.signal.notify();
    }
}

/// A session thread (reader or input writer).
struct Worker {
    handle: JoinHandle<()>,
    /// Never receives a value; disconnects once the thread's body has
    /// returned (or unwound), so checking it never blocks.
    done: Receiver<()>,
}

impl Worker {
    /// Starts `body` on a thread named `name` that runs the thread start
    /// hook first and wakes `signal` when it ends.
    fn spawn(
        name: String,
        signal: &Arc<Signal>,
        body: impl FnOnce() + Send + 'static,
    ) -> io::Result<Self> {
        let (done_tx, done) = mpsc::channel();
        let finished = Finished {
            done: Some(done_tx),
            signal: Arc::clone(signal),
        };
        let handle = thread::Builder::new().name(name).spawn(move || {
            let _finished = finished;
            run_thread_start_hook();
            body();
        })?;
        Ok(Self { handle, done })
    }

    /// Whether the thread's body has returned. Never blocks.
    fn has_ended(&self) -> bool {
        matches!(self.done.try_recv(), Err(TryRecvError::Disconnected))
    }

    /// Joins the thread if it has ended (which then takes no time), or
    /// detaches it; returns whether it was joined. `what` names it in the
    /// log.
    fn join_or_detach(self, id: SessionId, what: &str) -> bool {
        if !self.has_ended() {
            return false;
        }
        if self.handle.join().is_err() {
            tracing::warn!(session = %id, "session {what} thread panicked");
        }
        true
    }
}

/// A session's terminal, shared by the reader thread and the frontend's
/// threads, with a count of the threads waiting for its lock.
///
/// The reader thread feeds output in slices and, between slices, gives the
/// lock to a waiting thread (see [`ADVANCE_SLICE`]); the count tells it
/// whether anyone is waiting, so it only gives way when someone is.
struct SharedTerminal {
    term: Mutex<Terminal>,
    /// Threads inside [`SharedTerminal::lock`] that found the lock taken
    /// and have not acquired it yet.
    ///
    /// Every access is `Relaxed`. The count is only a hint for the reader
    /// thread: mutual exclusion comes from the mutex, and no other memory
    /// is published through the count. A waiter the reader does not see
    /// yet is seen after its next slice (or gets the lock while the reader
    /// is between slices anyway), so a late read costs at most one slice of
    /// waiting; a stale non-zero read costs at most [`GIVE_WAY_YIELDS`]
    /// needless yields.
    waiting: AtomicUsize,
}

/// Registration of one thread in [`SharedTerminal::waiting`], undone when
/// dropped, so the count cannot leak even if acquiring the lock unwound.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn register(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Self(count)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl SharedTerminal {
    fn new(term: Terminal) -> Self {
        Self {
            term: Mutex::new(term),
            waiting: AtomicUsize::new(0),
        }
    }

    /// Locks the terminal for anyone but the reader thread. When the lock
    /// is taken, this thread is counted as waiting from before it blocks
    /// until it holds the lock. Recovers a poisoned lock as [`lock`] does.
    fn lock(&self) -> MutexGuard<'_, Terminal> {
        match self.term.try_lock() {
            Ok(guard) => return guard,
            Err(TryLockError::Poisoned(poisoned)) => return poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {}
        }
        let _waiting = Waiting::register(&self.waiting);
        lock(&self.term)
    }

    /// Whether a thread is waiting in [`SharedTerminal::lock`]. A hint: it
    /// may change as soon as it is read.
    fn has_waiters(&self) -> bool {
        self.waiting.load(Ordering::Relaxed) > 0
    }
}

/// A running (or exited but not yet closed) session.
pub(crate) struct Session {
    id: SessionId,
    pty: Arc<Mutex<Box<dyn Pty>>>,
    term: Arc<SharedTerminal>,
    /// Set when the application starts closing the session; the reader
    /// thread stops delivering events and pending writes give up.
    muted: Arc<AtomicBool>,
    /// Held by the reader thread around each observer call (and the
    /// `muted` check before it), so taking it after muting is a barrier:
    /// once it is acquired no observer call for this session is running
    /// and none can start.
    delivering: Arc<Mutex<()>>,
    /// The reader thread's id, so a close made from inside one of this
    /// session's observer calls does not wait for that call.
    reader_thread: ThreadId,
    /// Taken by the reaper when it joins or detaches the thread.
    reader: Mutex<Option<Worker>>,
    outbox: Arc<Outbox>,
    /// Taken by the reaper when it joins or detaches the thread.
    writer: Mutex<Option<Worker>>,
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

impl OutboxState {
    /// Refuses `len` more bytes exactly as [`Outbox::push`] would.
    fn check_room(&self, len: usize) -> io::Result<()> {
        if self.closed {
            return Err(closing_error());
        }
        if let Some(kind) = self.failed {
            return Err(io::Error::new(kind, "the session's input writer stopped"));
        }
        if len > PENDING_INPUT_BYTES - self.bytes {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the session's pending input is full",
            ));
        }
        Ok(())
    }
}

impl Outbox {
    /// Appends `bytes` whole, or refuses them without queueing anything.
    fn push(&self, bytes: Vec<u8>) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.check_room(bytes.len())?;
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

impl Session {
    /// Wraps a spawned PTY and starts the writer thread for its input and
    /// the reader thread for its events. Both wake `signal` when they end.
    pub(crate) fn start(
        id: SessionId,
        pty: Box<dyn Pty>,
        events: PtyEvents,
        size: PtySize,
        observers: ObserverSlot,
        signal: &Arc<Signal>,
    ) -> io::Result<Self> {
        let pty = Arc::new(Mutex::new(pty));
        let term = Arc::new(SharedTerminal::new(Terminal::new(size.rows, size.cols)));
        let muted = Arc::new(AtomicBool::new(false));
        let outbox = Arc::new(Outbox::default());
        let writer = Writer {
            pty: Arc::clone(&pty),
            muted: Arc::clone(&muted),
            outbox: Arc::clone(&outbox),
        };
        let writer = Worker::spawn(format!("tessera-input-{id}"), signal, move || writer.run())?;
        let delivering = Arc::new(Mutex::new(()));
        let pump = Pump {
            id,
            term: Arc::clone(&term),
            muted: Arc::clone(&muted),
            delivering: Arc::clone(&delivering),
            observers,
        };
        let reader = match Worker::spawn(format!("tessera-session-{id}"), signal, move || {
            pump.run(&events);
        }) {
            Ok(reader) => reader,
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
            delivering,
            reader_thread: reader.handle.thread().id(),
            reader: Mutex::new(Some(reader)),
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
        self.check_accepts_input()?;
        self.outbox.push(bytes)
    }

    /// Fails exactly as [`Session::queue_input`] would for `len` bytes,
    /// without queueing anything, so a caller can refuse large input
    /// before it copies it. Only the frontend thread queues input and the
    /// writer only frees room, so `len` bytes that pass still fit when
    /// they are queued, unless the session closes or its child exits in
    /// between.
    pub(crate) fn check_input_room(&self, len: usize) -> io::Result<()> {
        self.check_accepts_input()?;
        lock(&self.outbox.state).check_room(len)
    }

    /// Refuses input once the session is closing or its child has exited.
    fn check_accepts_input(&self) -> io::Result<()> {
        if self.muted.load(Ordering::Acquire) {
            return Err(closing_error());
        }
        if lock(&self.pty).exit_status().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child has exited",
            ));
        }
        Ok(())
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

    /// Resizes the terminal, then the PTY.
    ///
    /// The grid is resized (and re-wrapped) first, so output the child
    /// writes in answer to `SIGWINCH` lands on a grid of the new size. The
    /// two locks are taken one after the other, never together.
    ///
    /// A size [`check_size`] refuses (zero rows or columns, or larger
    /// than the largest grid a view derives) is refused with
    /// [`PtyError::InvalidSize`] before anything changes.
    pub(crate) fn resize(&self, size: PtySize) -> Result<(), PtyError> {
        check_size(size)?;
        self.term.lock().resize(size.rows, size.cols);
        lock(&self.pty).resize(size)
    }

    /// Runs `f` with the session's terminal locked.
    pub(crate) fn with_terminal<R>(&self, f: impl FnOnce(&Terminal) -> R) -> R {
        f(&self.term.lock())
    }

    /// Runs `f` with the session's terminal locked for changes made on the
    /// application's behalf rather than by child output. The lock is held
    /// only while `f` runs; do not notify observers or take a view lock
    /// from `f`.
    pub(crate) fn with_terminal_mut<R>(&self, f: impl FnOnce(&mut Terminal) -> R) -> R {
        f(&mut self.term.lock())
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

    /// First step of a close: stops event delivery, drops pending input
    /// and hangs up the child. Never waits for the child.
    ///
    /// The caller must then call [`Session::wait_for_delivery`] before it
    /// reports the session closed, so no observer call for it is still
    /// running.
    pub(crate) fn begin_close(&self) {
        self.muted.store(true, Ordering::Release);
        self.outbox.close();
        if let Err(err) = lock(&self.pty).hangup() {
            tracing::debug!(session = %self.id, %err, "hangup on close failed");
        }
    }

    /// Second step of a close: waits for an observer call of this session
    /// that started before [`Session::begin_close`] muted it. Observer
    /// calls are short (they must return quickly), so this is the only wait
    /// in a close. Returns at once when called from inside one of this
    /// session's observer calls, which is the call it would wait for.
    pub(crate) fn wait_for_delivery(&self) {
        if thread::current().id() != self.reader_thread {
            drop(lock(&self.delivering));
        }
    }

    /// Whether the reader and writer threads have both ended (or were
    /// already joined or detached). The reader ends once the child has
    /// exited and been reaped and its terminal is closed; the writer, once
    /// the session is muted, within one write backoff period. Never blocks.
    pub(crate) fn threads_ended(&self) -> bool {
        let ended =
            |worker: &Mutex<Option<Worker>>| lock(worker).as_ref().is_none_or(Worker::has_ended);
        ended(&self.writer) && ended(&self.reader)
    }

    /// Sends `SIGKILL` to a child that outlived its hangup grace period.
    pub(crate) fn kill(&self) {
        tracing::debug!(session = %self.id, "child ignored SIGHUP; killing");
        if let Err(err) = lock(&self.pty).kill() {
            tracing::debug!(session = %self.id, %err, "kill on close failed");
        }
    }

    /// Last step of a close, run by the reaper: joins the reader and
    /// writer threads that have ended and detaches those still running
    /// (the terminal is still held open after `SIGKILL`; being muted, they
    /// can no longer reach the observer). Never blocks.
    pub(crate) fn join_or_detach(&self) {
        if let Some(writer) = lock(&self.writer).take()
            && !writer.join_or_detach(self.id, "writer")
        {
            tracing::warn!(session = %self.id, "session writer still running after SIGKILL; detaching it");
        }
        if let Some(reader) = lock(&self.reader).take()
            && !reader.join_or_detach(self.id, "reader")
        {
            tracing::warn!(
                session = %self.id,
                "terminal still held open after SIGKILL; detaching reader thread"
            );
        }
    }

    /// Identifies this session among the sessions of every `App` in the
    /// process while it lives (the address of its delivery lock), for
    /// [`set_delivery_pause`].
    #[cfg(test)]
    pub(crate) fn delivery_key(&self) -> usize {
        Arc::as_ptr(&self.delivering).addr()
    }

    /// The child's process id, if the PTY knows it.
    #[cfg(test)]
    pub(crate) fn child_pid(&self) -> Option<u32> {
        lock(&self.pty).child_pid()
    }
}

impl Drop for Session {
    /// Stops the writer thread even if the session was never closed (it
    /// holds the PTY until it stops).
    fn drop(&mut self) {
        self.outbox.close();
    }
}

/// The application's reaper: one thread that finishes closing every
/// session the application closes (ADR 0018).
///
/// A closed session is handed over muted and hung up. The reaper waits for
/// its threads to end, sends `SIGKILL` to a child still running
/// [`HANGUP_GRACE`] after the session was handed over, and joins its
/// threads, or detaches them once [`KILL_GRACE`] has passed after the kill.
/// Each session keeps its own deadlines, so sessions closed together share
/// their grace periods. The thread is started by the first close and lives
/// until [`Reaper::shut_down`]; it sleeps on [`Signal`] between deadlines
/// and is woken by every session thread that ends.
#[derive(Debug)]
pub(crate) struct Reaper {
    shared: Arc<ReaperShared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug, Default)]
struct ReaperShared {
    signal: Arc<Signal>,
    queue: Mutex<ReapQueue>,
    /// Sessions handed over and not yet joined or detached.
    closing: AtomicUsize,
}

#[derive(Debug, Default)]
struct ReapQueue {
    /// Handed over since the reaper last looked.
    incoming: Vec<Closing>,
    /// Set by [`Reaper::shut_down`]: finish what is queued, then stop.
    shut_down: bool,
}

/// A session being reaped, with its deadlines.
#[derive(Debug)]
struct Closing {
    session: Arc<Session>,
    /// When the child is sent `SIGKILL` if it is still running.
    kill_at: Instant,
    /// When its threads are detached, set once the child was killed.
    detach_at: Option<Instant>,
}

impl Closing {
    /// Moves the close on as far as `now` allows; returns whether it is
    /// finished (the threads were joined or detached).
    fn step(&mut self, now: Instant) -> bool {
        let finished = match self.detach_at {
            _ if self.session.threads_ended() => true,
            None if now >= self.kill_at => {
                self.session.kill();
                self.detach_at = Some(now + KILL_GRACE);
                false
            }
            Some(detach_at) => now >= detach_at,
            None => false,
        };
        if finished {
            self.session.join_or_detach();
        }
        finished
    }

    /// [`Closing::step`], except that a panic finishes the close the hard
    /// way: the child is sent `SIGKILL` and the threads are joined if they
    /// have ended, else detached. Keeping the close would retry a step that
    /// may panic again, at a deadline already past.
    fn step_or_abandon(&mut self, now: Instant) -> bool {
        if let Ok(finished) = panic::catch_unwind(AssertUnwindSafe(|| self.step(now))) {
            return finished;
        }
        tracing::error!(session = %self.session.id, "reaping the session panicked; killing it");
        let session = &self.session;
        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            session.kill();
            session.join_or_detach();
        }));
        true
    }

    /// When [`Closing::step`] next has something to do without being woken.
    fn deadline(&self) -> Instant {
        self.detach_at.unwrap_or(self.kill_at)
    }
}

impl Reaper {
    /// A reaper with no thread yet.
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::default(),
            thread: Mutex::new(None),
        }
    }

    /// What session threads wake when they end; pass it to
    /// [`Session::start`].
    pub(crate) fn signal(&self) -> &Arc<Signal> {
        &self.shared.signal
    }

    /// Hands over `sessions`, each already through [`Session::begin_close`]
    /// and [`Session::wait_for_delivery`], and returns without waiting.
    /// Starts the reaper thread if it is not running; if it cannot be
    /// started the sessions stay queued (muted and hung up) until the next
    /// call starts it or [`Reaper::shut_down`] reaps them.
    pub(crate) fn reap(&self, sessions: Vec<Arc<Session>>) {
        self.queue(sessions);
        let mut thread = lock(&self.thread);
        if thread.is_none() {
            let shared = Arc::clone(&self.shared);
            match thread::Builder::new()
                .name("tessera-reaper".into())
                .spawn(move || {
                    run_thread_start_hook();
                    shared.run();
                }) {
                Ok(handle) => *thread = Some(handle),
                Err(err) => {
                    tracing::error!(%err, "cannot start the session reaper; retrying on the next close");
                }
            }
        }
    }

    /// Queues `sessions` like [`Reaper::reap`] without starting the
    /// thread, for a [`Reaper::shut_down`] that follows at once.
    pub(crate) fn queue(&self, sessions: Vec<Arc<Session>>) {
        let kill_at = Instant::now() + HANGUP_GRACE;
        self.shared
            .closing
            .fetch_add(sessions.len(), Ordering::Release);
        lock(&self.shared.queue)
            .incoming
            .extend(sessions.into_iter().map(|session| Closing {
                session,
                kill_at,
                detach_at: None,
            }));
        self.shared.signal.notify();
    }

    /// Sessions handed over and not yet joined or detached.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn closing(&self) -> usize {
        self.shared.closing.load(Ordering::Acquire)
    }

    /// Finishes every queued close and stops the reaper thread, waiting for
    /// both: at most about [`HANGUP_GRACE`] + [`KILL_GRACE`] after the last
    /// hand-over. When no close started the thread (or it could not be
    /// started), or it died, the closes still queued run on the calling
    /// thread.
    pub(crate) fn shut_down(&self) {
        lock(&self.shared.queue).shut_down = true;
        self.shared.signal.notify();
        let thread = lock(&self.thread).take();
        if let Some(thread) = thread
            && thread.join().is_ok()
        {
            return;
        }
        // Either there was no thread or it panicked outside a step (which
        // `step_or_abandon` catches); what it had not taken is still queued.
        self.shared.run();
    }
}

impl ReaperShared {
    /// The reaper's loop: returns once shut down with nothing left to do.
    fn run(&self) {
        let mut closing: Vec<Closing> = Vec::new();
        loop {
            let seen = self.signal.epoch();
            let shut_down = {
                let mut queue = lock(&self.queue);
                closing.append(&mut queue.incoming);
                queue.shut_down
            };
            let now = Instant::now();
            closing.retain_mut(|close| {
                let finished = close.step_or_abandon(now);
                if finished {
                    // Release: what the close did (the kill, the joins)
                    // is visible to whoever sees the count drop.
                    self.closing.fetch_sub(1, Ordering::Release);
                }
                !finished
            });
            if closing.is_empty() && shut_down {
                return;
            }
            self.signal
                .wait(seen, closing.iter().map(Closing::deadline).min());
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

/// Most bytes of PTY output the reader thread feeds the terminal while
/// holding its lock once.
///
/// The native PTY reader delivers output in chunks of up to 64 KiB, and
/// parsing a whole chunk of escape-heavy or CJK output takes milliseconds,
/// during which the frontend's `render_view` (the first lock taker of every
/// frame) would wait. The reader instead takes the lock once per slice of
/// this size, so one hold lasts one slice. That bounds bytes, not time: a
/// few sequences are expensive per byte (DECALN or `REP` on a wide screen,
/// `RIS`, `SU`/`IL` with huge counts) and make a single slice take tens to
/// hundreds of milliseconds (ADR 0014; #117 for DECALN, `REP` and `SU`/`IL`,
/// #107 for `RIS`).
///
/// Releasing the lock is not enough on its own: `std::sync::Mutex` is not
/// fair, and the reader, still running, would take the lock back before the
/// waiter it just woke is scheduled. Under a flood a waiter could lose for
/// many chunks (a macOS PTY hands out 1 KiB per read, which made this, not
/// the chunk size, the cause of the long waits). So after each slice the
/// reader gives way to a thread counted as waiting by `SharedTerminal::lock`
/// (`give_way`). With nobody waiting it does not yield, so output is
/// as fast as before. Measured waits, the throughput cost and the choice of
/// size are in ADR 0014 ("Frame input, and how long the terminal lock is
/// held").
///
/// A slice that would end inside a UTF-8 encoded character is extended to
/// the character's end, so slices are at most three bytes longer (see
/// `advance_slice_len`). The parser would be correct anyway
/// (`Terminal::advance` is split-invariant), but a cut inside a character
/// makes it copy the whole next slice to join the pieces.
const ADVANCE_SLICE: usize = 4096;

/// Most times the reader yields after a slice while a thread waits for
/// the terminal lock (see `give_way`).
const GIVE_WAY_YIELDS: usize = 16;

/// Length of the next slice of `rest` to feed the terminal under one lock
/// hold: [`ADVANCE_SLICE`] bytes, extended by up to three bytes so it does
/// not end inside a UTF-8 encoded character. All of `rest` if that is no
/// longer.
fn advance_slice_len(rest: &[u8]) -> usize {
    if rest.len() <= ADVANCE_SLICE {
        return rest.len();
    }
    // Extend while the next byte is a continuation byte (`0b10xx_xxxx`), at
    // most three times (the longest tail of a four-byte character). Bytes
    // that are not valid UTF-8 just keep the plain length.
    let limit = rest.len().min(ADVANCE_SLICE + 4);
    match (ADVANCE_SLICE..limit).find(|&end| rest[end] & 0xC0 != 0x80) {
        Some(end) => end,
        None if rest.len() <= ADVANCE_SLICE + 3 => rest.len(),
        None => ADVANCE_SLICE,
    }
}

/// Lets a thread waiting for `term`'s lock take it before the reader takes
/// it again, if one is waiting; returns how often it called `yield_now`
/// (`thread::yield_now` outside tests).
///
/// `std::sync::Mutex` is not fair: the release just before this woke the
/// waiter, but the reader, still running, would win the lock back before
/// the waiter is scheduled. Yielding until the waiter has the lock (the
/// count drops) hands it over; the bound of [`GIVE_WAY_YIELDS`] keeps a
/// waiter that is itself descheduled from stalling output. With nobody
/// waiting this is one relaxed load.
fn give_way(term: &SharedTerminal, mut yield_now: impl FnMut()) -> usize {
    for yields in 0..GIVE_WAY_YIELDS {
        if !term.has_waiters() {
            return yields;
        }
        yield_now();
    }
    GIVE_WAY_YIELDS
}

/// State moved onto a session's reader thread.
struct Pump {
    id: SessionId,
    term: Arc<SharedTerminal>,
    muted: Arc<AtomicBool>,
    /// See [`Session::delivering`].
    delivering: Arc<Mutex<()>>,
    observers: ObserverSlot,
}

impl Pump {
    fn run(&self, events: &PtyEvents) {
        for event in events {
            match event {
                PtyEvent::Output(bytes) => {
                    // The lock is taken once per slice, never for the whole
                    // chunk, and a frame waiting for it goes first between
                    // slices (see `ADVANCE_SLICE`). Requests are collected
                    // in order and delivered after the chunk's one `Output`,
                    // so observers see no more callbacks than before.
                    let mut requests = Vec::new();
                    let mut rest = &bytes[..];
                    while !rest.is_empty() {
                        let (slice, after) = rest.split_at(advance_slice_len(rest));
                        // The reader locks the mutex directly: it never
                        // counts as waiting, so it never makes itself yield.
                        let mut term = lock(&self.term.term);
                        term.advance(slice);
                        requests.append(&mut term.take_requests());
                        drop(term);
                        give_way(&self.term, thread::yield_now);
                        rest = after;
                    }
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
        // Checked under the delivery lock: a closer mutes, then takes that
        // lock, so once it has held it this check can no longer pass. Taken
        // after the read lock, so a closer never waits for this thread while
        // it queues for the read lock behind `set_observer`.
        #[cfg(test)]
        pause_before_delivery(Arc::as_ptr(&self.delivering).addr());
        let _delivering = lock(&self.delivering);
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

/// A pause for tests: the reader thread of the session it names runs it in
/// `Pump::deliver` just before it takes the delivery lock, which is where a
/// `muted` check outside the lock would race with a close.
///
/// The session is named by [`Session::delivery_key`], not its id: every
/// `App` numbers its sessions from 1, and tests run many `App`s in one
/// process (`cargo test`).
#[cfg(test)]
type DeliveryPause = (usize, Arc<dyn Fn() + Send + Sync>);

#[cfg(test)]
static DELIVERY_PAUSE: Mutex<Option<DeliveryPause>> = Mutex::new(None);

/// Installs (or with `None` removes) the pause `Pump::deliver` runs before
/// the delivery lock of the session whose [`Session::delivery_key`] it
/// names. The caller keeps that session alive while the pause is set, so
/// the key cannot be reused.
#[cfg(test)]
pub(crate) fn set_delivery_pause(pause: Option<DeliveryPause>) {
    *lock(&DELIVERY_PAUSE) = pause;
}

#[cfg(test)]
fn pause_before_delivery(key: usize) {
    let pause = lock(&DELIVERY_PAUSE)
        .as_ref()
        .filter(|(target, _)| *target == key)
        .map(|(_, pause)| Arc::clone(pause));
    if let Some(pause) = pause {
        pause();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{RecvTimeoutError, SyncSender};

    use tessera_pty::PtyEvent;

    use super::*;
    use crate::{App, Config, ConfigPaths};

    /// A PTY that only records the sizes it is given and rejects an empty
    /// one, as the real PTY does. Hanging up ends its event stream.
    struct SizePty {
        events: Option<SyncSender<PtyEvent>>,
        sizes: Arc<Mutex<Vec<PtySize>>>,
    }

    impl Pty for SizePty {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<()> {
            Ok(())
        }

        fn try_write(&mut self, _bytes: &[u8]) -> io::Result<()> {
            Ok(())
        }

        fn resize(&mut self, size: PtySize) -> Result<(), PtyError> {
            if size.rows == 0 || size.cols == 0 {
                return Err(PtyError::InvalidSize(size));
            }
            lock(&self.sizes).push(size);
            Ok(())
        }

        fn size(&self) -> PtySize {
            lock(&self.sizes).last().copied().unwrap_or_default()
        }

        fn child_pid(&self) -> Option<u32> {
            None
        }

        fn exit_status(&self) -> Option<ExitStatus> {
            None
        }

        fn hangup(&mut self) -> io::Result<()> {
            if let Some(events) = self.events.take() {
                let _ = events.send(PtyEvent::Exited(ExitStatus::Signaled(1)));
            }
            Ok(())
        }

        fn kill(&mut self) -> io::Result<()> {
            self.hangup()
        }
    }

    #[test]
    fn app_resize_resizes_the_grid_then_the_pty() {
        let app = App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"));
        let (tx, rx) = mpsc::sync_channel(4);
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let pty = SizePty {
            events: Some(tx.clone()),
            sizes: Arc::clone(&sizes),
        };
        let id = app.allocate_id();
        app.attach(id, Box::new(pty), rx, PtySize::new(3, 8))
            .expect("attach");
        tx.send(PtyEvent::Output(b"abcdefghijkl".to_vec()))
            .expect("reader alive");
        drop(tx);
        // The output is applied once the grid shows it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.with_terminal(id, |t| t.grid().cursor().row).unwrap() != 1 {
            assert!(Instant::now() < deadline, "output never reached the grid");
            thread::yield_now();
        }

        app.resize(id, PtySize::new(4, 5)).expect("resize");
        let (rows, cols, line0) = app
            .with_terminal(id, |t| {
                let grid = t.grid();
                (
                    grid.rows(),
                    grid.cols(),
                    grid.line(0).expect("row 0").text(),
                )
            })
            .unwrap();
        assert_eq!((rows, cols), (4, 5));
        assert_eq!(line0, "abcde", "the grid re-wrapped");
        assert_eq!(&lock(&sizes)[..], [PtySize::new(4, 5)]);

        let refused = app.resize(id, PtySize::new(0, 9));
        assert!(refused.is_err(), "an empty size is refused");
        let size = app
            .with_terminal(id, |t| (t.grid().rows(), t.grid().cols()))
            .unwrap();
        assert_eq!(size, (4, 5), "a refused size leaves the grid alone");

        for (rows, cols) in [
            (MAX_VIEW_ROWS + 1, 80),
            (24, MAX_VIEW_COLS + 1),
            (u16::MAX, u16::MAX),
        ] {
            let refused = app.resize(id, PtySize::new(rows, cols));
            assert!(
                matches!(
                    refused,
                    Err(crate::Error::Pty(PtyError::InvalidSize(s))) if s == PtySize::new(rows, cols)
                ),
                "{rows}x{cols}: {refused:?}"
            );
        }
        app.resize(id, PtySize::new(MAX_VIEW_ROWS, MAX_VIEW_COLS))
            .expect("the largest view size is accepted");
        let size = app
            .with_terminal(id, |t| (t.grid().rows(), t.grid().cols()))
            .unwrap();
        assert_eq!(size, (MAX_VIEW_ROWS, MAX_VIEW_COLS));
        assert_eq!(
            lock(&sizes).last().copied(),
            Some(PtySize::new(MAX_VIEW_ROWS, MAX_VIEW_COLS)),
            "oversized requests never reached the PTY"
        );
        assert_eq!(lock(&sizes).len(), 2);
        app.close_session(id).expect("close");
    }

    /// The `sgr_heavy` line of the wave 5 performance corpus: an SGR
    /// sequence every few cells, including colon sub-parameters.
    const SGR_HEAVY: &str = "\x1b[1;31mbold red\x1b[0m \x1b[4:3;58:2::255:0:0mcurly\x1b[0m \
        \x1b[38;2;10;20;30;48;5;240mtrue\x1b[0m \x1b[3;9mital-strike\x1b[0m \x1b[7mrev\x1b[27m";

    /// The `cjk` line of the wave 5 performance corpus: wide characters,
    /// three UTF-8 bytes each.
    const CJK: &str =
        "日本語のテキストと中文的文本と한국어 텍스트を混ぜた行です。漢字かな交じり文。";

    /// The largest chunk the native PTY reader delivers (one `read(2)`).
    const PTY_CHUNK: usize = 64 * 1024;

    /// What one `read(2)` of a macOS PTY master returns under a flood: the
    /// tty layer hands out 1 KiB at a time, so the reader sees a stream of
    /// small chunks rather than 64 KiB ones.
    const MACOS_PTY_READ: usize = 1024;

    /// `line` repeated with `\r\n` between copies until at least `len`
    /// bytes.
    fn repeated(line: &str, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + line.len() + 2);
        while out.len() < len {
            out.extend_from_slice(line.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out
    }

    /// A session on a [`SizePty`] whose output the test injects, with an
    /// observer forwarding every event. Returns the event sender (dropping
    /// it is not a hangup; close the session for that).
    fn fake_session(
        app: &App,
        size: PtySize,
    ) -> (SessionId, SyncSender<PtyEvent>, Receiver<SessionEvent>) {
        let (events_tx, events) = mpsc::channel();
        app.set_observer(Some(Box::new(move |_, event| {
            let _ = events_tx.send(event);
        })));
        let (tx, rx) = mpsc::sync_channel(16);
        let pty = SizePty {
            events: Some(tx.clone()),
            sizes: Arc::new(Mutex::new(Vec::new())),
        };
        let id = app.allocate_id();
        app.attach(id, Box::new(pty), rx, size).expect("attach");
        (id, tx, events)
    }

    /// Everything a frame or a scrollback view can show of `term`.
    fn screen(term: &Terminal) -> (Vec<tessera_grid::Line>, Vec<tessera_grid::Line>, String) {
        let grid = term.grid();
        let scrollback = (0..grid.scrollback_len())
            .map(|i| grid.scrollback_line(i).expect("in range").clone())
            .collect();
        (
            grid.visible_lines().to_vec(),
            scrollback,
            format!("{:?}", grid.cursor()),
        )
    }

    #[test]
    fn slices_are_bounded_and_end_on_character_boundaries() {
        for line in [SGR_HEAVY, CJK, "🦀 👨‍👩‍👧‍👦 ⣿ Åsa", "a"] {
            let bytes = repeated(line, 5 * ADVANCE_SLICE + 7);
            let text = std::str::from_utf8(&bytes).expect("corpus is UTF-8");
            let mut rest = &bytes[..];
            let mut at = 0;
            while !rest.is_empty() {
                let len = advance_slice_len(rest);
                assert!(len >= ADVANCE_SLICE || len == rest.len(), "{line:?}: {len}");
                assert!(len <= ADVANCE_SLICE + 3, "{line:?}: {len}");
                at += len;
                assert!(
                    text.is_char_boundary(at),
                    "{line:?}: slice ends inside a character at {at}"
                );
                rest = &rest[len..];
            }
            assert_eq!(at, bytes.len());
        }
        // Short input is one slice; bytes that are not UTF-8 keep the full
        // length rather than an empty slice.
        assert_eq!(advance_slice_len(b"abc"), 3);
        assert_eq!(advance_slice_len(&[]), 0);
        assert_eq!(
            advance_slice_len(&[0x80; ADVANCE_SLICE + 10]),
            ADVANCE_SLICE
        );
        let mut cut = vec![b'a'; ADVANCE_SLICE + 10];
        cut[ADVANCE_SLICE - 1..ADVANCE_SLICE + 2].copy_from_slice("語".as_bytes());
        assert_eq!(advance_slice_len(&cut), ADVANCE_SLICE + 2);
        // A character cut by the end of the input ends the slice there.
        assert_eq!(advance_slice_len(&cut[..=ADVANCE_SLICE]), ADVANCE_SLICE + 1);
        // Four or more continuation bytes are not one character's tail.
        for len in [ADVANCE_SLICE + 4, ADVANCE_SLICE + 5] {
            assert_eq!(advance_slice_len(&vec![0x80; len]), ADVANCE_SLICE, "{len}");
        }
        assert_eq!(
            advance_slice_len(&[0x80; ADVANCE_SLICE + 3]),
            ADVANCE_SLICE + 3
        );
    }

    #[test]
    fn give_way_yields_only_while_someone_waits() {
        let term = SharedTerminal::new(Terminal::new(2, 2));
        let mut yields = 0;
        assert_eq!(give_way(&term, || yields += 1), 0, "nobody waits");
        assert_eq!(yields, 0);

        // A waiter that never gets the lock: the bound applies.
        let stuck = Waiting::register(&term.waiting);
        assert_eq!(give_way(&term, || {}), GIVE_WAY_YIELDS);
        drop(stuck);
        assert!(!term.has_waiters(), "dropping the registration undoes it");

        // A waiter that takes the lock during the third yield.
        let mut waiter = Some(Waiting::register(&term.waiting));
        let mut yields = 0;
        let given = give_way(&term, || {
            yields += 1;
            if yields == 3 {
                waiter.take();
            }
        });
        assert_eq!(given, 3);
    }

    #[test]
    fn a_blocked_lock_taker_is_counted_until_it_has_the_lock() {
        let term = SharedTerminal::new(Terminal::new(2, 2));
        drop(term.lock());
        assert!(!term.has_waiters(), "an uncontended lock is not counted");

        let held = lock(&term.term);
        let got_it = AtomicBool::new(false);
        thread::scope(|scope| {
            scope.spawn(|| {
                let guard = term.lock();
                got_it.store(true, Ordering::Release);
                drop(guard);
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !term.has_waiters() {
                assert!(
                    Instant::now() < deadline,
                    "the blocked thread was never counted"
                );
                thread::yield_now();
            }
            assert!(!got_it.load(Ordering::Acquire));
            drop(held);
        });
        assert!(got_it.load(Ordering::Acquire));
        assert!(!term.has_waiters(), "counted only until it had the lock");

        // A poisoned lock is recovered without counting anyone.
        let poison = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = lock(&term.term);
            panic!("poison the terminal lock");
        }));
        assert!(poison.is_err());
        drop(term.lock());
        assert!(!term.has_waiters());
    }

    /// Several frame-like threads taking the lock while a flood of output
    /// arrives: the reader keeps going, every thread keeps getting the
    /// lock, and nobody waits anywhere near the time the flood takes.
    ///
    /// A liveness check that runs in debug builds too; its bounds are far
    /// too loose to catch a slow handoff. The timing bar for several
    /// waiters is `several_waiters_each_wait_briefly` (release only).
    #[test]
    fn several_waiters_and_the_reader_all_progress() {
        const THREADS: usize = 4;
        let app = App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"));
        let (id, tx, events) = fake_session(&app, PtySize::new(24, 80));
        let corpus = repeated(&format!("{SGR_HEAVY}\r\n{CJK}"), 8 * 1024 * 1024);
        let chunks = corpus.chunks(PTY_CHUNK).count();
        let done = AtomicBool::new(false);
        let started = Instant::now();
        let probes = thread::scope(|scope| {
            let probes: Vec<_> = (0..THREADS)
                .map(|_| {
                    scope.spawn(|| {
                        let mut waits = Vec::new();
                        while !done.load(Ordering::Acquire) {
                            let start = Instant::now();
                            let acquired =
                                app.with_terminal(id, |_| Instant::now()).expect("session");
                            waits.push(acquired - start);
                            thread::sleep(Duration::from_micros(200));
                        }
                        waits
                    })
                })
                .collect();
            for chunk in corpus.chunks(PTY_CHUNK) {
                tx.send(PtyEvent::Output(chunk.to_vec()))
                    .expect("reader alive");
            }
            for _ in 0..chunks {
                let event = events
                    .recv_timeout(Duration::from_secs(60))
                    .expect("the reader stalled");
                assert_eq!(event, SessionEvent::Output);
            }
            done.store(true, Ordering::Release);
            probes
                .into_iter()
                .map(|probe| probe.join().expect("probe"))
                .collect::<Vec<_>>()
        });
        let flood = started.elapsed();
        for (i, waits) in probes.iter().enumerate() {
            let max = waits.iter().max().copied().unwrap_or_default();
            assert!(
                waits.len() >= 5,
                "thread {i}: only {} locks in {flood:?}",
                waits.len()
            );
            assert!(
                max < flood / 4 || max < Duration::from_millis(50),
                "thread {i} waited {max:?} of a {flood:?} flood"
            );
        }
        app.close_session(id).expect("close");
    }

    /// Split invariance through the pump: a 64 KiB chunk that ends inside a
    /// CSI sequence (and is itself fed in slices that cut sequences and
    /// characters), followed by its continuation, leaves the same screen,
    /// scrollback and cursor as one `advance` of the whole stream.
    #[test]
    fn a_chunk_ending_mid_csi_renders_like_one_advance() {
        let mut corpus = repeated(&format!("{SGR_HEAVY}\r\n{CJK}"), 2 * PTY_CHUNK);
        let csi = b"\x1b[38;2;10;20";
        let rest = b";30;48;5;240mtrue\x1b[0m tail";
        // Fill the first chunk with whole corpus lines and pad it so it ends
        // exactly inside the CSI.
        let filler_len = PTY_CHUNK - csi.len();
        let line_end = corpus[..filler_len]
            .iter()
            .rposition(|&b| b == b'\n')
            .expect("a line ends in the first chunk")
            + 1;
        let mut first = corpus[..line_end].to_vec();
        first.resize(filler_len, b'z');
        first.extend_from_slice(csi);
        assert_eq!(first.len(), PTY_CHUNK);
        let mut second = rest.to_vec();
        corpus.truncate(PTY_CHUNK / 2);
        second.extend_from_slice(&corpus);

        let app = App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"));
        let (id, tx, events) = fake_session(&app, PtySize::new(24, 80));
        tx.send(PtyEvent::Output(first.clone()))
            .expect("reader alive");
        tx.send(PtyEvent::Output(second.clone()))
            .expect("reader alive");
        for _ in 0..2 {
            let event = events.recv_timeout(Duration::from_secs(5)).expect("output");
            assert_eq!(event, SessionEvent::Output);
        }
        let pumped = app.with_terminal(id, screen).expect("session");

        let mut reference = Terminal::new(24, 80);
        let mut whole = first;
        whole.extend_from_slice(&second);
        reference.advance(&whole);
        assert_eq!(pumped, screen(&reference));
        app.close_session(id).expect("close");
    }

    /// The sliced pump against one `advance` of the same bytes, over seeded
    /// random streams (corpus fragments, escape sequence prefixes, raw
    /// bytes including invalid UTF-8) cut into chunks of random sizes up to
    /// past 64 KiB, so slices and chunks end inside sequences and
    /// characters everywhere.
    #[test]
    fn the_sliced_pump_matches_one_advance() {
        /// xorshift64*: small, seeded, no dependency.
        struct Rng(u64);
        impl Rng {
            fn below(&mut self, n: usize) -> usize {
                self.0 ^= self.0 >> 12;
                self.0 ^= self.0 << 25;
                self.0 ^= self.0 >> 27;
                let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
                usize::try_from(x % n as u64).expect("below n")
            }
        }
        let fragments: [&[u8]; 12] = [
            SGR_HEAVY.as_bytes(),
            CJK.as_bytes(),
            "🦀 👨‍👩‍👧‍👦 Z͑ͫ̓ͫ͡a̸̵ḻ̕g̐ò ⣿".as_bytes(),
            b"\x1b[",
            b"\x1b[38;2;1",
            b"\x1b]0;title",
            b"\x07",
            b"\r\n",
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[5;10r\x1b[2J\x1b[H",
            b"\x1bM\t",
        ];
        for seed in 1..=24_u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let target = 64 * 1024 + rng.below(160 * 1024);
            let mut stream = Vec::with_capacity(target + 512);
            while stream.len() < target {
                if rng.below(4) == 0 {
                    for _ in 0..rng.below(8) {
                        stream.push(u8::try_from(rng.below(256)).expect("byte"));
                    }
                } else {
                    let fragment = fragments[rng.below(fragments.len())];
                    let start = rng.below(fragment.len());
                    let end = start + rng.below(fragment.len() - start + 1);
                    stream.extend_from_slice(&fragment[start..end]);
                }
            }
            let app = App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"));
            let (id, tx, events) = fake_session(&app, PtySize::new(24, 80));
            let mut rest = &stream[..];
            let mut chunks = 0;
            while !rest.is_empty() {
                let len = (1 + rng.below(70_000)).min(rest.len());
                tx.send(PtyEvent::Output(rest[..len].to_vec()))
                    .expect("reader alive");
                rest = &rest[len..];
                chunks += 1;
            }
            let mut outputs = 0;
            while outputs < chunks {
                if events.recv_timeout(Duration::from_secs(10)).expect("event")
                    == SessionEvent::Output
                {
                    outputs += 1;
                }
            }
            let pumped = app.with_terminal(id, screen).expect("session");
            let mut reference = Terminal::new(24, 80);
            reference.advance(&stream);
            assert_eq!(pumped, screen(&reference), "seed {seed}");
            app.close_session(id).expect("close");
        }
    }

    /// Bells from every slice of a chunk are delivered after the chunk's
    /// single `Output`, in order, and a flood of chunks yields one `Output`
    /// per chunk, not one per slice.
    #[test]
    fn one_output_per_chunk_then_its_requests() {
        let app = App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"));
        let (id, tx, events) = fake_session(&app, PtySize::new(24, 80));
        // A bell at the start, in the middle of a slice, on a slice boundary
        // and at the end of a chunk spanning five slices.
        let mut chunk = repeated(SGR_HEAVY, 5 * ADVANCE_SLICE);
        chunk.truncate(5 * ADVANCE_SLICE);
        for at in [0, ADVANCE_SLICE / 2, 2 * ADVANCE_SLICE, chunk.len()] {
            chunk.insert(at, 0x07);
        }
        tx.send(PtyEvent::Output(chunk)).expect("reader alive");
        let mut got = Vec::new();
        for _ in 0..5 {
            got.push(events.recv_timeout(Duration::from_secs(5)).expect("event"));
        }
        assert_eq!(
            got,
            [
                SessionEvent::Output,
                SessionEvent::Bell,
                SessionEvent::Bell,
                SessionEvent::Bell,
                SessionEvent::Bell
            ]
        );

        let flood = repeated(CJK, 8 * PTY_CHUNK);
        let chunks = flood.chunks(PTY_CHUNK).count();
        for chunk in flood.chunks(PTY_CHUNK) {
            tx.send(PtyEvent::Output(chunk.to_vec()))
                .expect("reader alive");
        }
        for _ in 0..chunks {
            let event = events.recv_timeout(Duration::from_secs(5)).expect("output");
            assert_eq!(event, SessionEvent::Output);
        }
        assert_eq!(
            events.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout),
            "no event beyond one Output per chunk"
        );
        app.close_session(id).expect("close");
    }

    /// The reader releases the lock between the slices of one chunk: a
    /// thread taking it in a loop sees the chunk partly applied. Every
    /// state it can see between slices is a line boundary, because each
    /// line is 16 bytes and slices are 4096. Holding the lock for a whole
    /// chunk would only ever show the state before or after it.
    #[test]
    fn other_lock_takers_get_in_between_slices() {
        let app = App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"));
        let (id, tx, events) = fake_session(&app, PtySize::new(4, 20));
        // The last line written so far is the screen's third row: `\r\n`
        // after each line leaves the cursor on the empty row below it.
        let last_line = |term: &Terminal| term.grid().line(2).expect("row 2").text();
        let mut next = 0;
        let mut seen_partial = false;
        for _ in 0..50 {
            let mut chunk = Vec::with_capacity(PTY_CHUNK);
            while chunk.len() < PTY_CHUNK {
                chunk.extend_from_slice(format!("line {next:09}\r\n").as_bytes());
                next += 1;
            }
            let before = app.with_terminal(id, last_line).expect("session");
            let after = format!("line {:09}", next - 1);
            let done = AtomicBool::new(false);
            let seen = thread::scope(|scope| {
                let probe = scope.spawn(|| {
                    let mut seen = Vec::new();
                    while !done.load(Ordering::Acquire) {
                        let line = app.with_terminal(id, last_line).expect("session");
                        if line != before && line != after && seen.last() != Some(&line) {
                            seen.push(line);
                        }
                        // A frame-like pause: a thread that retakes the
                        // lock the instant it releases it would starve the
                        // reader, which never counts as waiting.
                        thread::sleep(Duration::from_micros(20));
                    }
                    seen
                });
                tx.send(PtyEvent::Output(chunk)).expect("reader alive");
                let event = events
                    .recv_timeout(Duration::from_secs(10))
                    .expect("output");
                assert_eq!(event, SessionEvent::Output);
                done.store(true, Ordering::Release);
                probe.join().expect("probe")
            });
            for line in &seen {
                assert!(line.starts_with("line "), "a torn line: {line:?}");
            }
            if !seen.is_empty() {
                seen_partial = true;
                break;
            }
        }
        assert!(
            seen_partial,
            "the lock was never free in the middle of a chunk"
        );
        app.close_session(id).expect("close");
    }

    /// Feeds 50 MiB of `line` (repeated) to a fresh session in chunks of
    /// `chunk_len` bytes while `probes` threads each take the terminal lock
    /// every 200 µs, as `render_view` does once per frame; returns each
    /// probe's waits, sorted.
    fn lock_waits(line: &str, chunk_len: usize, probes: usize) -> Vec<Vec<Duration>> {
        const VOLUME: usize = 50 * 1024 * 1024;
        let app = App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"));
        let (id, tx, events) = fake_session(&app, PtySize::new(24, 80));
        let corpus = repeated(line, VOLUME);
        let chunks = corpus.chunks(chunk_len).count();
        let done = AtomicBool::new(false);
        let mut waits = thread::scope(|scope| {
            let probes: Vec<_> = (0..probes)
                .map(|_| {
                    scope.spawn(|| {
                        let mut waits = Vec::new();
                        while !done.load(Ordering::Acquire) {
                            let start = Instant::now();
                            let acquired =
                                app.with_terminal(id, |_| Instant::now()).expect("session");
                            waits.push(acquired - start);
                            thread::sleep(Duration::from_micros(200));
                        }
                        waits
                    })
                })
                .collect();
            for chunk in corpus.chunks(chunk_len) {
                tx.send(PtyEvent::Output(chunk.to_vec()))
                    .expect("reader alive");
            }
            for _ in 0..chunks {
                let event = events
                    .recv_timeout(Duration::from_secs(60))
                    .expect("output");
                assert_eq!(event, SessionEvent::Output);
            }
            done.store(true, Ordering::Release);
            probes
                .into_iter()
                .map(|probe| probe.join().expect("probe"))
                .collect::<Vec<_>>()
        });
        app.close_session(id).expect("close");
        for probe in &mut waits {
            probe.sort_unstable();
        }
        waits
    }

    /// The lock-hold bar of #83: while the reader parses 50 MiB each of the
    /// `sgr_heavy` and `cjk` corpus lines, delivered in 64 KiB chunks (the
    /// largest the native PTY reader delivers) and in 1 KiB chunks (what a
    /// macOS PTY delivers under a flood), a thread taking the terminal lock
    /// the way `render_view` does waits at most 2 ms at the 99th percentile
    /// and 5 ms at worst.
    ///
    /// Timing-based, so run it only in release builds, one test at a time:
    /// `cargo nextest run --release -p tessera-app-core --test-threads 1
    /// reader_lock_hold`. In debug builds (where `scripts/quality-gate.sh`
    /// runs the suite) it is ignored; `--run-ignored` there would run it
    /// unoptimised, where the bar does not apply. It prints every attempt's
    /// percentiles.
    ///
    /// Each case gets up to three attempts and passes on the first that
    /// meets the bar. A measured wait includes the probe's own scheduling
    /// delay, so on a loaded machine a single preemption of the probe (or
    /// of the reader inside a slice) can push one sample past 5 ms; a
    /// reader that keeps the lock (main before #83: hundreds of
    /// milliseconds, a few dozen samples) fails every attempt.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
    fn reader_lock_hold_is_bounded() {
        const ATTEMPTS: usize = 3;
        let cases = [
            ("sgr_heavy", SGR_HEAVY, PTY_CHUNK),
            ("cjk", CJK, PTY_CHUNK),
            ("sgr_heavy/1KiB", SGR_HEAVY, MACOS_PTY_READ),
            ("cjk/1KiB", CJK, MACOS_PTY_READ),
        ];
        let mut failures = Vec::new();
        for (name, line, chunk_len) in cases {
            let mut misses = Vec::new();
            for attempt in 1..=ATTEMPTS {
                let waits = lock_waits(line, chunk_len, 1).remove(0);
                let p99 = waits[waits.len() * 99 / 100];
                let max = *waits.last().expect("samples");
                eprintln!(
                    "{name} attempt {attempt}: {} samples, p50 {:?}, p99 {p99:?}, max {max:?}",
                    waits.len(),
                    waits[waits.len() / 2],
                );
                let mut miss = Vec::new();
                if waits.len() < 100 {
                    miss.push(format!("only {} samples", waits.len()));
                }
                if p99 > Duration::from_millis(2) {
                    miss.push(format!("p99 wait {p99:?} > 2 ms"));
                }
                if max > Duration::from_millis(5) {
                    miss.push(format!("max wait {max:?} > 5 ms"));
                }
                if miss.is_empty() {
                    misses.clear();
                    break;
                }
                misses.push(format!("{name} attempt {attempt}: {}", miss.join(", ")));
            }
            failures.extend(misses);
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// Several frame-like threads at once (three and six, 64 KiB chunks of
    /// `sgr_heavy`, 50 MiB): each one's 99th-percentile wait stays under
    /// 5 ms, a generous bound (measured 0.2-2.8 ms on an Apple M4 at load
    /// 10-20). A reader that never gives way makes them wait 100-300
    /// milliseconds. Release only, like `reader_lock_hold_is_bounded`, and
    /// with the same three attempts.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
    fn several_waiters_each_wait_briefly() {
        const ATTEMPTS: usize = 3;
        let mut failures = Vec::new();
        for probes in [3, 6] {
            let mut misses = Vec::new();
            for attempt in 1..=ATTEMPTS {
                let mut miss = Vec::new();
                for (i, waits) in lock_waits(SGR_HEAVY, PTY_CHUNK, probes).iter().enumerate() {
                    let p99 = waits[waits.len() * 99 / 100];
                    eprintln!(
                        "{probes} waiters attempt {attempt} thread {i}: {} samples, p99 {p99:?}, max {:?}",
                        waits.len(),
                        waits.last().expect("samples"),
                    );
                    if waits.len() < 50 {
                        miss.push(format!("thread {i}: only {} samples", waits.len()));
                    }
                    if p99 > Duration::from_millis(5) {
                        miss.push(format!("thread {i}: p99 wait {p99:?} > 5 ms"));
                    }
                }
                if miss.is_empty() {
                    misses.clear();
                    break;
                }
                misses.push(format!(
                    "{probes} waiters attempt {attempt}: {}",
                    miss.join(", ")
                ));
            }
            failures.extend(misses);
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }
}
