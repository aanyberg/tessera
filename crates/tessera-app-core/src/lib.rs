//! The application core.
//!
//! Owns the workspace model (windows contain tabs, tabs contain a pane tree,
//! leaves hold sessions), the command registry and keybinding resolution, the
//! configuration store, the plugin host and the control server. Frontends
//! drive it exclusively through `tessera-ffi`. This crate has no knowledge of
//! `AppKit`, `wgpu` or any OS window system.
//!
//! Today the core manages a flat set of sessions ([`App::spawn_session`],
//! [`App::close_session`], …) and reports what happens in them through one
//! [`SessionObserver`]; the window, tab and pane model builds on top.
//!
//! # Threads
//!
//! [`App`] is `Send + Sync` and every method takes `&self`, so a frontend may
//! call it from any thread. Each session runs one reader thread, on which
//! the observer is called. Dropping the `App` closes every session and joins
//! its reader thread (see the [`session`] module for the close sequence).

#![forbid(unsafe_code)]

pub mod session;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Instant;

use tessera_pty::{NativePty, Pty, PtyEvents};
use tessera_term::Terminal;

pub use session::{SessionEvent, SessionId, SessionObserver};
// Re-exported so frontends (through `tessera-ffi`) depend on this crate
// alone, as the layering in `docs/architecture.md` requires.
pub use tessera_config::{Config, ConfigPaths};
pub use tessera_pty::{ExitStatus, Program, PtyError, PtySize, SpawnOptions};

use session::{ObserverSlot, Session, lock};

/// Application version reported to frontends and the control API.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Errors from driving the application.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No live session has this id (it was never spawned or is closed).
    #[error("no session with id {0}")]
    SessionNotFound(SessionId),
    /// The PTY could not be opened, the child could not be started, or a
    /// resize was rejected.
    #[error(transparent)]
    Pty(#[from] PtyError),
    /// The session's reader thread could not be started.
    #[error("failed to start the reader thread for session {id}: {source}")]
    ReaderThread {
        /// Session whose spawn failed.
        id: SessionId,
        /// Why the thread could not be created.
        #[source]
        source: io::Error,
    },
    /// Writing to a session failed (its child exited, or it is closing).
    #[error("failed to write to session {id}: {source}")]
    Write {
        /// Session that was written to.
        id: SessionId,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
}

/// Result alias for [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Root application object.
pub struct App {
    config: Config,
    paths: ConfigPaths,
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
    next_id: AtomicU64,
    observers: ObserverSlot,
}

impl App {
    /// Creates an application with the given configuration, read from (and
    /// saved to) the directory `paths` describes. No session is running and
    /// no observer is installed.
    pub fn new(config: Config, paths: ConfigPaths) -> Self {
        Self {
            config,
            paths,
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            observers: Arc::new(RwLock::new(None)),
        }
    }

    /// Current configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Path of the main configuration file, `config.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.paths.config_file()
    }

    /// Installs the observer that receives every session's events, or
    /// removes it with `None`. Applies to sessions already running.
    ///
    /// Waits for an observer call in progress to return, so once this
    /// returns the previous observer is never called again. Must therefore
    /// not be called from inside an observer.
    pub fn set_observer(&self, observer: Option<Box<dyn SessionObserver>>) {
        let previous = std::mem::replace(
            &mut *self
                .observers
                .write()
                .unwrap_or_else(PoisonError::into_inner),
            observer,
        );
        // Dropped after the lock is released so a slow destructor does not
        // hold up the reader threads.
        drop(previous);
    }

    /// Starts `options.program` in a new PTY and returns the new session's
    /// id.
    ///
    /// The application assigns the id and overrides `options.session_id`
    /// with it, so the child's `TESSERA_SESSION_ID` always matches. Events
    /// of the new session may reach the observer before this returns.
    ///
    /// # Errors
    ///
    /// [`Error::Pty`] when the size is invalid, no PTY is available or the
    /// program cannot be started; [`Error::ReaderThread`] when the reader
    /// thread cannot be created (the child is then hung up).
    pub fn spawn_session(&self, options: SpawnOptions) -> Result<SessionId> {
        let id = self.allocate_id();
        let options = SpawnOptions {
            session_id: id.to_string(),
            ..options
        };
        let (pty, events) = NativePty::spawn(&options)?;
        self.attach(id, Box::new(pty), events, options.size)
    }

    /// Registers an already spawned PTY as session `id`.
    fn attach(
        &self,
        id: SessionId,
        pty: Box<dyn Pty>,
        events: PtyEvents,
        size: PtySize,
    ) -> Result<SessionId> {
        let session = Session::start(id, pty, events, size, Arc::clone(&self.observers))
            .map_err(|source| Error::ReaderThread { id, source })?;
        lock(&self.sessions).insert(id, Arc::new(session));
        Ok(id)
    }

    fn allocate_id(&self) -> SessionId {
        SessionId::from_raw(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn session(&self, id: SessionId) -> Result<Arc<Session>> {
        lock(&self.sessions)
            .get(&id)
            .cloned()
            .ok_or(Error::SessionNotFound(id))
    }

    /// Closes session `id`: no further event of it reaches the observer,
    /// its child is hung up (and killed if it lingers) and its reader
    /// thread is joined. Takes at most about
    /// [`HANGUP_GRACE`](session::HANGUP_GRACE) +
    /// [`KILL_GRACE`](session::KILL_GRACE).
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session.
    pub fn close_session(&self, id: SessionId) -> Result<()> {
        let session = lock(&self.sessions)
            .remove(&id)
            .ok_or(Error::SessionNotFound(id))?;
        self.close_all_of(&[session]);
        Ok(())
    }

    /// Closes `sessions` against shared deadlines: all are hung up, then all
    /// lingering children are killed at once after one
    /// [`HANGUP_GRACE`](session::HANGUP_GRACE), and readers still running
    /// one [`KILL_GRACE`](session::KILL_GRACE) later are detached. Closing
    /// any number of sessions therefore takes at most about the sum of the
    /// two grace periods.
    fn close_all_of(&self, sessions: &[Arc<Session>]) {
        for session in sessions {
            session.begin_close();
        }
        // Barrier: taking the write lock waits out any observer call that
        // started before the sessions were muted.
        drop(
            self.observers
                .write()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let hangup_deadline = Instant::now() + session::HANGUP_GRACE;
        let lingering: Vec<_> = sessions
            .iter()
            .filter(|session| !session.wait_until(hangup_deadline))
            .collect();
        for session in &lingering {
            session.kill();
        }
        let kill_deadline = Instant::now() + session::KILL_GRACE;
        for session in &lingering {
            session.wait_until(kill_deadline);
        }
        for session in sessions {
            session.join_or_detach();
        }
    }

    /// Ids of the live sessions, in spawn order.
    pub fn session_ids(&self) -> Vec<SessionId> {
        let mut ids: Vec<_> = lock(&self.sessions).keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Queues `bytes` as input for session `id`.
    ///
    /// Waits while the session's write queue is full (the child is not
    /// reading); gives up if the session is closed meanwhile.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::Write`] if its child has exited or it is being closed.
    pub fn write(&self, id: SessionId, bytes: &[u8]) -> Result<()> {
        self.session(id)?
            .write(bytes)
            .map_err(|source| Error::Write { id, source })
    }

    /// Informs session `id`'s child of a new window size.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::Pty`] if the size has zero rows or columns or the kernel
    /// rejects it.
    pub fn resize(&self, id: SessionId, size: PtySize) -> Result<()> {
        Ok(self.session(id)?.resize(size)?)
    }

    /// Discards session `id`'s scrollback.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session.
    pub fn clear_scrollback(&self, id: SessionId) -> Result<()> {
        let _session = self.session(id)?;
        // TODO(#2): call `Grid::clear_scrollback` through the terminal once
        // the grid exposes it; until then there is nothing to discard
        // because the terminal does not yet write into its grid.
        Ok(())
    }

    /// Runs `f` with session `id`'s terminal locked.
    ///
    /// The reader thread cannot apply output while `f` runs, so keep it
    /// short.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session.
    pub fn with_terminal<R>(&self, id: SessionId, f: impl FnOnce(&Terminal) -> R) -> Result<R> {
        Ok(self.session(id)?.with_terminal(f))
    }

    /// Panics while holding the session table lock, poisoning it.
    ///
    /// Exists only so tests can prove that a panic caught at the FFI
    /// boundary leaves the application usable.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn panic_holding_session_lock(&self) {
        let _sessions = lock(&self.sessions);
        panic!("deliberate panic for testing");
    }
}

impl Drop for App {
    /// Closes every session and joins its reader thread.
    fn drop(&mut self) {
        let sessions: Vec<_> = lock(&self.sessions)
            .drain()
            .map(|(_, session)| session)
            .collect();
        self.close_all_of(&sessions);
        // A detached reader still holds the observer slot; empty it so the
        // observer is dropped here rather than later on that thread.
        self.set_observer(None);
    }
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("paths", &self.paths)
            .field("sessions", &self.session_ids())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::panic::AssertUnwindSafe;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
    use std::thread;
    use std::time::{Duration, Instant};

    use tessera_pty::{ExitStatus, PtyEvent};

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// A PTY whose "child" is the test: output is injected through the
    /// event sender, and hanging up or killing ends the stream unless the
    /// fake was told to ignore that signal.
    struct FakePty {
        events: Arc<Mutex<Option<SyncSender<PtyEvent>>>>,
        written: Arc<Mutex<Vec<u8>>>,
        hangups: Arc<AtomicUsize>,
        /// While set, `try_write` reports a full queue (a child that stopped
        /// reading).
        would_block: Arc<AtomicBool>,
        ignore_hangup: bool,
        /// Models a terminal held open by a grandchild after `SIGKILL`.
        ignore_kill: bool,
    }

    impl FakePty {
        fn exit(&self, status: ExitStatus) {
            if let Some(events) = lock(&self.events).take() {
                let _ = events.send(PtyEvent::Exited(status));
            }
        }
    }

    impl Pty for FakePty {
        fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            lock(&self.written).extend_from_slice(bytes);
            Ok(())
        }

        fn try_write(&mut self, bytes: &[u8]) -> io::Result<()> {
            if self.would_block.load(Ordering::SeqCst) {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.write(bytes)
        }

        fn resize(&mut self, _size: PtySize) -> Result<(), PtyError> {
            Ok(())
        }

        fn size(&self) -> PtySize {
            PtySize::default()
        }

        fn child_pid(&self) -> Option<u32> {
            None
        }

        fn exit_status(&self) -> Option<ExitStatus> {
            None
        }

        fn hangup(&mut self) -> io::Result<()> {
            self.hangups.fetch_add(1, Ordering::SeqCst);
            if !self.ignore_hangup {
                self.exit(ExitStatus::Signaled(1));
            }
            Ok(())
        }

        fn kill(&mut self) -> io::Result<()> {
            if !self.ignore_kill {
                self.exit(ExitStatus::Signaled(9));
            }
            Ok(())
        }
    }

    struct Fake {
        events: Arc<Mutex<Option<SyncSender<PtyEvent>>>>,
        written: Arc<Mutex<Vec<u8>>>,
        hangups: Arc<AtomicUsize>,
        would_block: Arc<AtomicBool>,
    }

    impl Fake {
        fn exit(&self, status: ExitStatus) {
            if let Some(events) = lock(&self.events).take() {
                let _ = events.send(PtyEvent::Exited(status));
            }
        }

        fn send(&self, event: PtyEvent) {
            lock(&self.events)
                .as_ref()
                .expect("fake child already exited")
                .send(event)
                .expect("reader thread gone");
        }
    }

    fn attach_fake(app: &App, ignore_hangup: bool) -> (SessionId, Fake) {
        attach_fake_with(app, ignore_hangup, false)
    }

    fn attach_fake_with(app: &App, ignore_hangup: bool, ignore_kill: bool) -> (SessionId, Fake) {
        let (tx, rx) = mpsc::sync_channel(16);
        let events = Arc::new(Mutex::new(Some(tx)));
        let written = Arc::new(Mutex::new(Vec::new()));
        let hangups = Arc::new(AtomicUsize::new(0));
        let would_block = Arc::new(AtomicBool::new(false));
        let pty = FakePty {
            events: Arc::clone(&events),
            written: Arc::clone(&written),
            hangups: Arc::clone(&hangups),
            would_block: Arc::clone(&would_block),
            ignore_hangup,
            ignore_kill,
        };
        let id = app.allocate_id();
        app.attach(id, Box::new(pty), rx, PtySize::default())
            .expect("attach fake session");
        (
            id,
            Fake {
                events,
                written,
                hangups,
                would_block,
            },
        )
    }

    fn app() -> App {
        App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"))
    }

    /// Installs an observer forwarding every event to the returned receiver.
    fn observe(app: &App) -> Receiver<(SessionId, SessionEvent)> {
        let (tx, rx) = mpsc::channel();
        app.set_observer(Some(Box::new(move |id, event| {
            let _ = tx.send((id, event));
        })));
        rx
    }

    fn next(events: &Receiver<(SessionId, SessionEvent)>) -> (SessionId, SessionEvent) {
        events.recv_timeout(TIMEOUT).expect("no session event")
    }

    /// Waits for session `id` to exit, skipping other events.
    fn wait_exit(events: &Receiver<(SessionId, SessionEvent)>, id: SessionId) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(remaining) {
                Ok((event_id, SessionEvent::Exit(status))) if event_id == id => return status,
                Ok(_) => {}
                Err(err) => panic!("session {id} did not exit: {err:?}"),
            }
        }
    }

    fn sh(script: &str) -> SpawnOptions {
        SpawnOptions::command("/bin/sh", "ignored").args(["-c", script])
    }

    #[test]
    fn ids_start_at_one_and_increase() {
        let app = app();
        let (first, _a) = attach_fake(&app, false);
        let (second, _b) = attach_fake(&app, false);
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 2);
        assert_eq!(app.session_ids(), vec![first, second]);
        assert_eq!(SessionId::from_raw(7).to_string(), "7");
    }

    #[test]
    fn output_is_applied_then_reported_with_bell_and_exit() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        fake.send(PtyEvent::Output(b"hi\x07".to_vec()));
        assert_eq!(next(&events), (id, SessionEvent::Output));
        assert_eq!(next(&events), (id, SessionEvent::Bell));
        fake.send(PtyEvent::Output(b"plain".to_vec()));
        assert_eq!(next(&events), (id, SessionEvent::Output));
        fake.send(PtyEvent::Exited(ExitStatus::Exited(4)));
        assert_eq!(
            next(&events),
            (id, SessionEvent::Exit(ExitStatus::Exited(4)))
        );
    }

    #[test]
    fn writes_reach_the_pty() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        let big = vec![b'x'; 3 * 64 * 1024 + 5];
        app.write(id, b"echo hi\n").unwrap();
        app.write(id, &big).unwrap();
        let written = lock(&fake.written);
        assert_eq!(&written[..8], b"echo hi\n");
        assert_eq!(written.len(), 8 + big.len());
    }

    #[test]
    fn unknown_ids_are_rejected() {
        let app = app();
        let id = SessionId::from_raw(42);
        let not_found =
            |result: Result<()>| matches!(result, Err(Error::SessionNotFound(i)) if i == id);
        assert!(not_found(app.write(id, b"x")));
        assert!(not_found(app.resize(id, PtySize::default())));
        assert!(not_found(app.clear_scrollback(id)));
        assert!(not_found(app.close_session(id)));
        assert!(matches!(
            app.with_terminal(id, |_| ()),
            Err(Error::SessionNotFound(_))
        ));
    }

    #[test]
    fn close_mutes_hangs_up_and_forgets_the_session() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        app.close_session(id).unwrap();
        assert_eq!(fake.hangups.load(Ordering::SeqCst), 1);
        // The fake's hangup produced an exit, but the session was muted
        // first, so nothing reaches the observer.
        assert_eq!(
            events.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        );
        assert_eq!(app.session_ids(), Vec::<SessionId>::new());
        assert!(matches!(
            app.write(id, b"x"),
            Err(Error::SessionNotFound(_))
        ));
        assert!(matches!(
            app.close_session(id),
            Err(Error::SessionNotFound(_))
        ));
    }

    #[test]
    fn close_kills_a_child_that_ignores_hangup() {
        let app = app();
        let (id, fake) = attach_fake(&app, true);
        let started = Instant::now();
        app.close_session(id).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(fake.hangups.load(Ordering::SeqCst), 1);
        assert!(lock(&fake.events).is_none(), "kill should end the fake");
        assert!(elapsed >= session::HANGUP_GRACE, "closed after {elapsed:?}");
        assert!(elapsed < session::HANGUP_GRACE + TIMEOUT);
    }

    const fn assert_send_sync<T: Send + Sync>() {}
    // The FFI lets any thread write to a session; that relies on this.
    const _: () = assert_send_sync::<App>();

    /// Starts a writer that keeps retrying against a full queue and waits
    /// until it has made a few attempts.
    fn blocked_writer(
        app: &Arc<App>,
        id: SessionId,
        fake: &Fake,
    ) -> thread::JoinHandle<Result<()>> {
        fake.would_block.store(true, Ordering::SeqCst);
        let writer = {
            let app = Arc::clone(app);
            thread::spawn(move || app.write(id, b"stuck"))
        };
        // Long enough for the backoff to reach its 20 ms ceiling.
        thread::sleep(Duration::from_millis(100));
        assert!(!writer.is_finished(), "write should be waiting");
        writer
    }

    #[test]
    fn resize_is_not_starved_by_a_blocked_writer() {
        let app = Arc::new(app());
        let (id, fake) = attach_fake(&app, false);
        let writer = blocked_writer(&app, id, &fake);
        let mut worst = Duration::ZERO;
        for _ in 0..20 {
            let started = Instant::now();
            app.resize(id, PtySize::new(30, 100)).unwrap();
            worst = worst.max(started.elapsed());
        }
        // One backoff period is 20 ms; holding the lock across it would make
        // most of these calls wait that long.
        assert!(
            worst < Duration::from_millis(10),
            "worst resize took {worst:?}"
        );
        fake.would_block.store(false, Ordering::SeqCst);
        writer.join().unwrap().unwrap();
        assert_eq!(&lock(&fake.written)[..], b"stuck");
    }

    #[test]
    fn close_releases_a_blocked_writer_promptly() {
        let app = Arc::new(app());
        let (id, fake) = attach_fake(&app, false);
        let writer = blocked_writer(&app, id, &fake);
        let started = Instant::now();
        app.close_session(id).unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "close took {:?}",
            started.elapsed()
        );
        assert!(matches!(writer.join().unwrap(), Err(Error::Write { .. })));
    }

    #[test]
    fn closing_many_stubborn_sessions_shares_the_grace_periods() {
        let app = app();
        let fakes: Vec<_> = (0..4).map(|_| attach_fake(&app, true)).collect();
        let started = Instant::now();
        drop(app);
        let elapsed = started.elapsed();
        // Serialized grace periods would take 4 * HANGUP_GRACE.
        assert!(
            elapsed >= session::HANGUP_GRACE,
            "dropped after {elapsed:?}"
        );
        assert!(
            elapsed < session::HANGUP_GRACE + Duration::from_millis(500),
            "dropped after {elapsed:?}"
        );
        for (_, fake) in &fakes {
            assert!(lock(&fake.events).is_none(), "every child should be killed");
        }
    }

    #[test]
    fn closing_sessions_that_survive_kill_detaches_within_both_graces() {
        let app = app();
        let (tx, events) = mpsc::channel();
        app.set_observer(Some(Box::new(move |id, event| {
            let _ = tx.send((id, event));
        })));
        let fakes: Vec<_> = (0..3).map(|_| attach_fake_with(&app, true, true)).collect();
        let started = Instant::now();
        drop(app);
        let elapsed = started.elapsed();
        let both = session::HANGUP_GRACE + session::KILL_GRACE;
        assert!(elapsed >= both, "dropped after {elapsed:?}");
        assert!(
            elapsed < both + Duration::from_millis(500),
            "dropped after {elapsed:?}"
        );
        // The detached readers no longer hold the observer alive.
        assert_eq!(events.try_recv(), Err(TryRecvError::Disconnected));
        // Let the detached reader threads finish.
        for (_, fake) in &fakes {
            fake.exit(ExitStatus::Signaled(9));
        }
    }

    #[test]
    fn replacing_the_observer_redirects_events() {
        let app = app();
        let first = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        fake.send(PtyEvent::Output(b"a".to_vec()));
        assert_eq!(next(&first), (id, SessionEvent::Output));
        let second = observe(&app);
        fake.send(PtyEvent::Output(b"b".to_vec()));
        assert_eq!(next(&second), (id, SessionEvent::Output));
        // The replaced observer was dropped without seeing "b".
        assert_eq!(first.try_recv(), Err(TryRecvError::Disconnected));
        app.set_observer(None);
        fake.send(PtyEvent::Output(b"c".to_vec()));
        assert_eq!(
            second.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn a_panicking_observer_does_not_stop_the_reader() {
        let app = app();
        let (tx, events) = mpsc::channel();
        app.set_observer(Some(Box::new(move |id, event: SessionEvent| {
            assert!(event != SessionEvent::Bell, "observer panics on bell");
            let _ = tx.send((id, event));
        })));
        let (id, fake) = attach_fake(&app, false);
        fake.send(PtyEvent::Output(b"\x07".to_vec()));
        assert_eq!(next(&events), (id, SessionEvent::Output));
        fake.send(PtyEvent::Exited(ExitStatus::Exited(0)));
        assert_eq!(
            next(&events),
            (id, SessionEvent::Exit(ExitStatus::Exited(0)))
        );
    }

    #[test]
    fn app_is_usable_after_a_panic_poisons_its_lock() {
        let app = app();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            app.panic_holding_session_lock();
        }));
        assert!(result.is_err());
        let (id, _fake) = attach_fake(&app, false);
        app.write(id, b"still works").unwrap();
        app.close_session(id).unwrap();
    }

    #[test]
    fn shell_session_reports_bell_and_exit_code() {
        let app = app();
        let events = observe(&app);
        let id = app.spawn_session(sh("printf '\\a'; exit 3")).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        let mut saw_bell = false;
        let status = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(remaining).expect("shell did not exit") {
                (event_id, _) if event_id != id => {}
                (_, SessionEvent::Bell) => saw_bell = true,
                (_, SessionEvent::Exit(status)) => break status,
                (_, _) => {}
            }
        };
        assert!(saw_bell);
        assert_eq!(status, ExitStatus::Exited(3));
        // The session stays registered until the application closes it.
        assert_eq!(app.session_ids(), vec![id]);
        assert!(matches!(app.write(id, b"x"), Err(Error::Write { .. })));
        app.close_session(id).unwrap();
    }

    #[test]
    fn shell_session_exits_on_written_exit() {
        let app = app();
        let events = observe(&app);
        let id = app
            .spawn_session(SpawnOptions::command("/bin/sh", "x"))
            .unwrap();
        app.resize(id, PtySize::new(30, 100).with_pixels(800, 600))
            .unwrap();
        assert!(matches!(
            app.resize(id, PtySize::new(0, 100)),
            Err(Error::Pty(PtyError::InvalidSize(_)))
        ));
        app.write(id, b"exit 0\n").unwrap();
        assert_eq!(wait_exit(&events, id), ExitStatus::Exited(0));
    }

    #[test]
    fn missing_program_fails_to_spawn() {
        let app = app();
        let result = app.spawn_session(SpawnOptions::command("/nonexistent/tessera-no-such", "x"));
        assert!(
            matches!(result, Err(Error::Pty(PtyError::Spawn { .. }))),
            "{result:?}"
        );
        assert_eq!(app.session_ids(), Vec::<SessionId>::new());
    }

    #[test]
    fn dropping_the_app_closes_live_sessions_promptly() {
        let app = app();
        let started = Instant::now();
        for _ in 0..3 {
            app.spawn_session(SpawnOptions::command("/bin/sh", "x"))
                .unwrap();
        }
        drop(app);
        assert!(started.elapsed() < TIMEOUT, "took {:?}", started.elapsed());
    }

    #[test]
    fn config_path_is_config_toml_under_the_root() {
        assert_eq!(
            app().config_path(),
            PathBuf::from("/nonexistent/tessera/config.toml")
        );
    }
}
