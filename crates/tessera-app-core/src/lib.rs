//! The application core.
//!
//! Owns the workspace model (windows contain tabs, tabs contain a pane tree,
//! leaves hold sessions), the command registry and keybinding resolution, the
//! configuration store, the plugin host and the control server. Frontends
//! drive it exclusively through `tessera-ffi`. This crate has no knowledge of
//! `AppKit` or any OS window system; it draws through `tessera-render` into
//! surfaces the frontend creates (see the [`view`] module).
//!
//! Today the core manages a flat set of sessions ([`App::spawn_session`],
//! [`App::close_session`], …) and reports what happens in them through one
//! [`SessionObserver`]; the window, tab and pane model builds on top.
//! Frontend input reaches a session through [`App::send_input`] (see the
//! [`input`] module). A session is drawn by at most one view
//! ([`App::attach_view`], [`App::render_view`], …), whose size decides the
//! session's grid size.
//!
//! # Threads
//!
//! [`App`] is `Send + Sync` and every method takes `&self`, so a frontend may
//! call it from any thread. Each session runs one reader thread, on which
//! the observer is called, and one writer thread that feeds queued
//! frontend input to the child. Closing a session returns at once; the
//! `App`'s reaper thread, started by the first close, waits for the child
//! and joins the session's threads. Dropping the `App` closes every
//! session and waits for the reaper (see the [`session`] module for the
//! close sequence). The view methods create, configure and present Metal
//! surfaces and must be called on the main thread (ADR 0014).

#![forbid(unsafe_code)]

mod config_watch;
pub mod input;
pub mod session;
pub mod view;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use tessera_pty::{NativePty, Pty, PtyEvents};
use tessera_render::{FontConfig, Fonts, Gpu, Theme};
use tessera_term::{Terminal, encode_key};

pub use input::{InputEvent, InputOutcome};
pub use session::{SessionEvent, SessionId, SessionObserver};
// Re-exported so frontends (through `tessera-ffi`) depend on this crate
// alone, as the layering in `docs/architecture.md` requires.
pub use config_watch::{CONFIG_DIR_POLL_INTERVAL, CONFIG_WATCH_STOP_CAP};
pub use tessera_config::{Config, ConfigPaths, ConfigStore, WatchOptions};
pub use tessera_pty::{ExitStatus, Program, PtyError, PtySize, SpawnOptions};

use config_watch::{Applied, ConfigWatch};
use input::{Bindings, FocusEvent, MouseEvent};
use session::{ObserverSlot, Reaper, Session, lock};
use view::{CellMetrics, FrameStatus, PhysicalSize, RenderError, View, ViewSize, wgpu};

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
    /// A session thread (reader or input writer) could not be started.
    #[error("failed to start a session thread (reader or input writer) for session {id}: {source}")]
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
    /// The GPU or the font system failed: no adapter or device, a surface
    /// the adapter cannot present to, a lost swapchain, a validation error
    /// while drawing, or no usable font for the renderer.
    #[error("rendering failed: {0}")]
    Gpu(#[from] RenderError),
    /// The session has no view attached.
    #[error("session {0} has no view")]
    NoView(SessionId),
    /// The session already has a view; detach it before attaching another.
    #[error("session {0} already has a view")]
    ViewAttached(SessionId),
}

/// Result alias for [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A view's geometry, font size and cursor cell, as [`App::view_metrics`]
/// reads them in one call. Every length is in physical pixels; a frontend
/// divides by [`ViewMetrics::scale`] for points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewMetrics {
    /// The surface's size (after clamping to the GPU's largest texture).
    pub pixels: PhysicalSize,
    /// The surface's backing scale as last attached or resized with, after
    /// [`tessera_render::sanitize_scale`] (raised to at least
    /// [`tessera_render::MIN_SCALE`]; not capped, unlike the renderer's).
    pub scale: f32,
    /// The cell size at the view's font size and scale.
    pub cell: CellMetrics,
    /// The padding between the surface's edges and the first row and
    /// column, on every side: the theme's padding at the renderer's scale
    /// (the backing scale capped at [`tessera_render::MAX_SCALE`]), rounded
    /// the way the renderer rounds it. Cell `(row, col)` starts at
    /// `(padding_px + col * cell.width_px, padding_px + row * cell.height_px)`.
    pub padding_px: u32,
    /// Rows of the session's grid. The grid follows the view
    /// ([`view::grid_size_for`]), except after an [`App::resize`] until the
    /// view's next resize or font-size change.
    pub rows: u16,
    /// Columns of the session's grid; see [`ViewMetrics::rows`].
    pub cols: u16,
    /// The view's font size in points, after
    /// [`tessera_render::sanitize_font_size`].
    pub font_size_pt: f32,
    /// The cursor's row on the live screen (scroll offset 0), below
    /// [`ViewMetrics::rows`].
    pub cursor_row: u16,
    /// The cursor's column, below [`ViewMetrics::cols`].
    pub cursor_col: u16,
    /// Whether the cursor is on the screen the view shows: `false` while
    /// the view is scrolled back ([`App::scroll_offset`] is not 0), when
    /// [`ViewMetrics::cursor_row`] and [`ViewMetrics::cursor_col`] are
    /// where it would be at scroll offset 0.
    pub cursor_on_screen: bool,
    /// Whether the cursor is on the left half of a two-column character,
    /// so the caret covers [`ViewMetrics::cursor_col`] and the column
    /// after it. On the right half it is `false`, though the renderer
    /// draws the caret over both halves there too.
    pub cursor_wide: bool,
}

/// Root application object.
pub struct App {
    /// The configuration in effect and the colours it resolved to; shared
    /// with the configuration watch's thread ([`App::start_config_watch`]).
    /// Written only while `views` is held, like `theme`.
    applied: Arc<Mutex<Applied>>,
    paths: ConfigPaths,
    sessions: Arc<Mutex<HashMap<SessionId, Arc<Session>>>>,
    next_id: AtomicU64,
    observers: ObserverSlot,
    bindings: Bindings,
    /// At most one view per session, by session id. Never held while
    /// `sessions` is locked.
    views: Arc<Mutex<HashMap<SessionId, View>>>,
    /// The process's GPU, brought up by the first [`App::attach_view`] and
    /// shared by every view after it. Replaced by the next attach once its
    /// device is lost, and dropped when the last view detaches. Locked only
    /// on its own or while `views` is held, never the other way round.
    gpu: Mutex<Option<Gpu>>,
    /// The font database, loaded by the first [`App::attach_view`] and
    /// shared by every view's shaper (loading it takes a noticeable
    /// fraction of a second).
    fonts: Mutex<Option<Fonts>>,
    /// The theme every view draws with, set by [`App::apply_config`].
    /// Written only while `views` is held (and read under it by
    /// [`App::attach_view`]), so a view attached during a re-theme never
    /// keeps the old colours.
    theme: Arc<Mutex<Theme>>,
    /// Finishes closing the sessions [`App::close_session`] hands it.
    reaper: Reaper,
    /// The configuration watch, if [`App::start_config_watch`] started one.
    config_watch: Mutex<Option<ConfigWatch>>,
}

impl App {
    /// Creates an application with the given configuration, read from (and
    /// saved to) the directory `paths` describes. No session is running and
    /// no observer is installed. Views draw with the theme
    /// `config.appearance.theme` names, resolved against `paths` like
    /// [`tessera_config::ConfigStore::theme`] (a name that matches nothing
    /// gives the default theme).
    ///
    /// The configuration is not watched: a frontend that reads it from a
    /// [`ConfigStore`] passes `(*store.config()).clone()` here and then
    /// hands the store to [`App::start_config_watch`].
    pub fn new(config: Config, paths: ConfigPaths) -> Self {
        let colours = tessera_config::Theme::resolve(&paths.themes_dir(), &config.appearance.theme);
        for warning in &colours.warnings {
            tracing::warn!(target: "tessera_app_core", "{warning}");
        }
        let theme = Arc::new(Mutex::new(view::theme_for(&config, &colours.theme)));
        Self {
            applied: Arc::new(Mutex::new(Applied {
                config: Arc::new(config),
                colours: colours.theme,
                source: match colours.source {
                    tessera_config::ThemeSource::User(path) => Some(path),
                    _ => None,
                },
            })),
            paths,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            observers: Arc::new(RwLock::new(None)),
            bindings: Bindings::default(),
            views: Arc::new(Mutex::new(HashMap::new())),
            gpu: Mutex::new(None),
            fonts: Mutex::new(None),
            theme,
            reaper: Reaper::new(),
            config_watch: Mutex::new(None),
        }
    }

    /// The configuration in effect: the one the `App` was created with,
    /// or the last one [`App::apply_config`] applied (which the
    /// configuration watch does on every change of `config.toml`).
    pub fn config(&self) -> Arc<Config> {
        self.live().config()
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
    /// [`Error::Pty`] when no PTY is available or the program cannot be
    /// started, and with [`PtyError::InvalidSize`] (as from
    /// [`App::resize`], before anything is spawned) when the size has zero
    /// rows or columns or more than [`view::MAX_VIEW_ROWS`] rows or
    /// [`view::MAX_VIEW_COLS`] columns: the grid allocates every row at
    /// full width, so an unbounded size could ask for gigabytes.
    /// [`PtyError::ArgumentsTooLong`] when the arguments and environment
    /// exceed what `execve(2)` accepts. [`Error::ReaderThread`] when a
    /// session thread (reader or input writer) cannot be created (the
    /// child is then hung up).
    pub fn spawn_session(&self, options: SpawnOptions) -> Result<SessionId> {
        session::check_size(options.size)?;
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
        let session = Session::start(
            id,
            pty,
            events,
            size,
            Arc::clone(&self.observers),
            self.reaper.signal(),
        )
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

    /// Closes session `id` and returns without waiting for its child.
    ///
    /// Once this returns the id is forgotten (every call with it fails with
    /// [`Error::SessionNotFound`]), its view is detached, its pending input
    /// is dropped and no event of it reaches the observer, not even the
    /// child's exit. The child has been hung up (`SIGHUP`); the `App`'s
    /// reaper thread kills it if it is still running
    /// [`HANGUP_GRACE`](session::HANGUP_GRACE) later and joins the
    /// session's threads in the background. The only wait is for an
    /// observer call of the session already in progress. May be called from
    /// inside an observer call for the same session; any other call into
    /// the `App` from an observer can deadlock. The view is released on the
    /// calling thread, and a view must be released on the main thread
    /// (ADR 0014), so a close from an observer call must come after
    /// [`App::detach_view`] on the main thread.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session.
    pub fn close_session(&self, id: SessionId) -> Result<()> {
        let session = lock(&self.sessions)
            .remove(&id)
            .ok_or(Error::SessionNotFound(id))?;
        // Detach the view first; its surface is released here, before the
        // child is hung up. The last view releases the GPU too.
        let mut views = lock(&self.views);
        let removed = self.remove_view(&mut views, id);
        drop(views);
        drop(removed);
        let session = vec![session];
        Self::begin_closing(&session);
        self.reaper.reap(session);
        Ok(())
    }

    /// Mutes and hangs up `sessions`, then waits for their observer calls
    /// in progress. The caller hands them to the reaper, which shares the
    /// grace periods between sessions handed over together.
    fn begin_closing(sessions: &[Arc<Session>]) {
        for session in sessions {
            session.begin_close();
        }
        for session in sessions {
            session.wait_for_delivery();
        }
    }

    /// Sessions closed but not yet reaped: their threads are neither
    /// joined nor detached. For tests of the background close.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn closing_sessions(&self) -> usize {
        self.reaper.closing()
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
    /// Bypasses the queue that [`App::send_input`] feeds, so these bytes
    /// may overtake or interleave with pending keys and pastes (both are
    /// written in 64 KiB pieces, so a pending paste can be split around
    /// them).
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

    /// Resizes session `id`'s terminal (re-wrapping its text, ADR 0009)
    /// and informs its child of the new window size.
    ///
    /// Takes the terminal lock like [`App::with_terminal`], with the same
    /// rule: do not call it in a tight loop.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::Pty`] if the size has zero rows or columns, more than
    /// [`view::MAX_VIEW_ROWS`] rows or [`view::MAX_VIEW_COLS`] columns
    /// (nothing changes then), or the kernel rejects it.
    pub fn resize(&self, id: SessionId, size: PtySize) -> Result<()> {
        Ok(self.session(id)?.resize(size)?)
    }

    /// Clears session `id` to its cursor line (⌘K, "Clear to Start" in
    /// Terminal.app): discards the scrollback and, on the primary screen,
    /// the rows above the cursor, so the prompt line becomes the top row
    /// (see [`tessera_term::Terminal::clear_to_cursor`]). On the alternate
    /// screen only the scrollback goes; the full-screen application's
    /// screen is not touched. The view returns to the live screen.
    ///
    /// No [`SessionEvent::Output`] is sent: like [`App::resize`], this runs
    /// on the caller's thread, which redraws when it is done. The next
    /// frame copies the whole screen, since the clear damages all of it.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session.
    pub fn clear_scrollback(&self, id: SessionId) -> Result<()> {
        let session = self.session(id)?;
        session.with_terminal_mut(Terminal::clear_to_cursor);
        // TODO(#31): every path that shrinks the scrollback (ED 3, a
        // smaller `max_scrollback`, leaving the alternate screen) must
        // reset or clamp the session's stored scroll offset, as here.
        session.scroll_to_bottom();
        Ok(())
    }

    /// Routes one frontend event to session `id` and reports whether the
    /// core used it.
    ///
    /// * [`InputEvent::Key`]: a key bound to an application command runs it
    ///   (no key is bound yet, #43); any other key is encoded for the child
    ///   with the terminal's keyboard modes
    ///   ([`Terminal::keyboard_modes`], copied under the terminal lock)
    ///   and queued for the session's writer thread **without blocking**
    ///   (see the [`session`] module), and the view returns to the live
    ///   screen. A key that encodes to nothing (a bare modifier,
    ///   a release in legacy mode) is [`InputOutcome::Ignored`].
    /// * [`InputEvent::Mouse`]: recorded as the last mouse event (cell
    ///   clamped to the grid; see [`App::last_mouse`]); always
    ///   [`InputOutcome::Ignored`] until mouse reporting and selection
    ///   exist.
    /// * [`InputEvent::Scroll`]: moves the scrollback view (see
    ///   [`App::scroll_offset`]); [`InputOutcome::Consumed`] only when the
    ///   offset changed.
    /// * [`InputEvent::Paste`]: as [`App::paste`]: sanitised with
    ///   [`input::sanitize_paste_into`], or in bracketed-paste mode
    ///   (DECSET 2004) wrapped in `ESC [ 200 ~` / `ESC [ 201 ~` and
    ///   sanitised with [`input::sanitize_bracketed_paste_into`], which
    ///   keeps line breaks; queued like a key, **without blocking**, behind
    ///   any input still pending, and the view returns to the live screen;
    ///   text that sanitises to nothing is [`InputOutcome::Ignored`] and
    ///   sends nothing, brackets included. The free space is checked on the
    ///   raw length plus the 12 bytes of brackets when bracketing, before
    ///   sanitising.
    /// * [`InputEvent::Focus`]: recorded (see [`App::is_focused`]); in
    ///   focus-event mode (DECSET 1004) a change of focus also queues
    ///   `ESC [ I` (gained) or `ESC [ O` (lost) for the child, which can
    ///   fail like a key. An event repeating the recorded state sends
    ///   nothing. [`InputOutcome::Consumed`]. The check and the update of
    ///   the recorded state are not one atomic step: focus events must come
    ///   from one thread (the frontend's main thread), or two racing events
    ///   may both be reported.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session;
    /// [`Error::Write`] if the child has exited, the session is being
    /// closed, or the key, paste or focus report does not fit in what is
    /// left of [`session::PENDING_INPUT_BYTES`] (kind
    /// [`io::ErrorKind::WouldBlock`]); nothing of it is queued then (the
    /// focus state is recorded all the same).
    pub fn send_input(&self, id: SessionId, event: InputEvent) -> Result<InputOutcome> {
        let session = self.session(id)?;
        let write_error = |source| Error::Write { id, source };
        match event {
            InputEvent::Key(key) => {
                if let Some(binding) = self.bindings.resolve(&key) {
                    match binding {}
                }
                // One copy under the terminal lock, released before the
                // write is queued.
                let modes = session.with_terminal(Terminal::keyboard_modes);
                let bytes = encode_key(&key, &modes);
                if bytes.is_empty() {
                    return Ok(InputOutcome::Ignored);
                }
                session.queue_input(bytes).map_err(write_error)?;
                session.scroll_to_bottom();
                Ok(InputOutcome::Consumed)
            }
            InputEvent::Mouse(mouse) => {
                session.record_mouse(mouse);
                Ok(InputOutcome::Ignored)
            }
            InputEvent::Scroll(scroll) => Ok(if session.scroll(scroll.delta_lines) {
                InputOutcome::Consumed
            } else {
                InputOutcome::Ignored
            }),
            InputEvent::Paste(text) => Self::queue_paste(&session, id, &text),
            InputEvent::Focus(focus) => {
                let focused = focus == FocusEvent::Gained;
                let changed = session.is_focused() != focused;
                session.set_focused(focused);
                // Focus reporting (DECSET 1004) reports changes only, so a
                // repeated event from the frontend sends nothing.
                if changed && session.with_terminal(|term| term.modes().focus_events) {
                    let report: &[u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
                    session.queue_input(report.to_vec()).map_err(write_error)?;
                }
                Ok(InputOutcome::Consumed)
            }
        }
    }

    /// Pastes `text` into session `id` exactly as
    /// [`InputEvent::Paste`] through [`App::send_input`] does, but borrows
    /// the text: the free pending space is checked on its raw length
    /// first, then it is sanitised straight into the buffer that is
    /// queued, so it is copied once, and not at all when it cannot fit.
    ///
    /// # Errors
    ///
    /// As [`App::send_input`]; a paste whose raw length (plus 12 bytes of
    /// brackets in bracketed-paste mode) does not fit in what is left of
    /// [`session::PENDING_INPUT_BYTES`] is refused even if sanitising would
    /// have shortened it enough.
    pub fn paste(&self, id: SessionId, text: &str) -> Result<InputOutcome> {
        let session = self.session(id)?;
        Self::queue_paste(&session, id, text)
    }

    /// Fails exactly as a paste or key of `len` bytes would fail now
    /// (unknown session, closing, child exited, not enough pending space),
    /// without queueing anything. Lets a caller refuse large input before
    /// it validates or copies it.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session;
    /// [`Error::Write`] (kind [`io::ErrorKind::WouldBlock`] for want of
    /// space) as from [`App::send_input`].
    pub fn check_input_room(&self, id: SessionId, len: usize) -> Result<()> {
        self.session(id)?
            .check_input_room(len)
            .map_err(|source| Error::Write { id, source })
    }

    fn queue_paste(session: &Session, id: SessionId, text: &str) -> Result<InputOutcome> {
        let write_error = |source| Error::Write { id, source };
        // Bracketed-paste mode (DECSET 2004), read once under the terminal
        // lock: the whole paste is wrapped, or none of it.
        let bracketed_paste = session.with_terminal(|term| term.modes().bracketed_paste);
        let (open, close): (&[u8], &[u8]) = if bracketed_paste {
            (b"\x1b[200~", b"\x1b[201~")
        } else {
            (b"", b"")
        };
        // Sanitising never lengthens text, so this bounds what is queued.
        let most = text.len().saturating_add(open.len() + close.len());
        session.check_input_room(most).map_err(write_error)?;
        let mut bytes = Vec::with_capacity(most);
        bytes.extend_from_slice(open);
        if bracketed_paste {
            input::sanitize_bracketed_paste_into(text, &mut bytes);
        } else {
            input::sanitize_paste_into(text, &mut bytes);
        }
        if bytes.len() == open.len() {
            return Ok(InputOutcome::Ignored);
        }
        bytes.extend_from_slice(close);
        // The outbox charges the sanitised length; do not hold the raw
        // length's capacity behind it when sanitising removed most of it.
        if bytes.len() < bytes.capacity() / 2 {
            bytes.shrink_to_fit();
        }
        session.queue_input(bytes).map_err(write_error)?;
        session.scroll_to_bottom();
        Ok(InputOutcome::Consumed)
    }

    /// How many lines session `id`'s view is scrolled back from the live
    /// screen: 0 at the bottom, at most the scrollback length.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session.
    pub fn scroll_offset(&self, id: SessionId) -> Result<usize> {
        Ok(self.session(id)?.scroll_offset())
    }

    /// Whether session `id`'s view has keyboard focus, as last reported
    /// with [`InputEvent::Focus`]. A new session is not focused.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session.
    pub fn is_focused(&self, id: SessionId) -> Result<bool> {
        Ok(self.session(id)?.is_focused())
    }

    /// The last [`InputEvent::Mouse`] sent to session `id`, its cell
    /// clamped to the grid, or `None` if there was none.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session.
    pub fn last_mouse(&self, id: SessionId) -> Result<Option<MouseEvent>> {
        Ok(self.session(id)?.last_mouse())
    }

    /// Runs `f` with session `id`'s terminal locked.
    ///
    /// The reader thread cannot apply output while `f` runs, so keep it
    /// short.
    ///
    /// Leave a gap between calls (a frame's worth, at least about 1 ms):
    /// between slices of output the reader gives the lock to every thread
    /// waiting for it and is not counted as waiting itself (ADR 0014), so a
    /// caller that takes the lock again the moment it releases it can
    /// starve the session's output. Paced callers, such as rendering and
    /// resizing from the display link at up to ~120 Hz, share the lock
    /// fairly.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session.
    pub fn with_terminal<R>(&self, id: SessionId, f: impl FnOnce(&Terminal) -> R) -> Result<R> {
        Ok(self.session(id)?.with_terminal(f))
    }

    /// The instance the frontend creates a view's surface on: the one the
    /// process's GPU was brought up on, or a fresh one (from
    /// [`Gpu::new_instance`]) before the first view exists.
    ///
    /// Pass the same instance to [`App::attach_view`].
    pub fn view_instance(&self) -> wgpu::Instance {
        lock(&self.gpu)
            .as_ref()
            .map_or_else(Gpu::new_instance, |gpu| gpu.instance().clone())
    }

    /// Whether session `id` has a view attached.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session.
    pub fn has_view(&self, id: SessionId) -> Result<bool> {
        self.session(id)?;
        Ok(lock(&self.views).contains_key(&id))
    }

    /// Ids of the sessions that have a view, in spawn order.
    pub fn view_ids(&self) -> Vec<SessionId> {
        let mut ids: Vec<_> = lock(&self.views).keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// The geometry of session `id`'s view: its surface in physical
    /// pixels, its backing scale and the grid that fits in it.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::NoView`] if it has no view.
    pub fn view_size(&self, id: SessionId) -> Result<ViewSize> {
        self.with_view(id, |view| view.size())
    }

    /// The font size session `id`'s view draws with, in points (after
    /// [`tessera_render::sanitize_font_size`]).
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::NoView`] if it has no view.
    pub fn view_font_size(&self, id: SessionId) -> Result<f32> {
        self.with_view(id, |view| view.font_size())
    }

    /// The cell size of session `id`'s view at its current font size and
    /// backing scale, in physical pixels.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::NoView`] if it has no view.
    pub fn view_cell_metrics(&self, id: SessionId) -> Result<CellMetrics> {
        self.with_view(id, |view| view.cell_metrics())
    }

    /// Everything a frontend needs to map between session `id`'s view and
    /// its cells, read in one call: the surface, its cell size and padding,
    /// the font size, the session's grid and its cursor cell.
    ///
    /// Takes the session table, the view table, then the session's
    /// terminal and input state, one after the other: the view table is released before the
    /// terminal is locked, so this adds no edge to the order
    /// [`App::render_view`] uses (views, then terminal). It draws nothing
    /// and waits for no GPU work of its own, but it does wait for the view
    /// table, which a [`App::render_view`] holds while it draws; the
    /// main-thread rule serialises the two (ADR 0014). The terminal is held
    /// only to read the grid size and the cursor cell, so the same pacing
    /// rule as [`App::with_terminal`] applies: once per frame or resize,
    /// not per mouse event. Frontends keep the result for the events in
    /// between.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::NoView`] if it has no view.
    pub fn view_metrics(&self, id: SessionId) -> Result<ViewMetrics> {
        let session = self.session(id)?;
        let (size, cell, font_size_pt, padding_px) = {
            let views = lock(&self.views);
            let view = views.get(&id).ok_or(Error::NoView(id))?;
            (
                view.size(),
                view.cell_metrics(),
                view.font_size(),
                view.padding_px(),
            )
        };
        let scrolled_back = session.scroll_offset() > 0;
        let (rows, cols, cursor, cursor_wide) = session.with_terminal(|term| {
            let grid = term.grid();
            let cursor = grid.cursor();
            let wide = grid.cell(cursor).is_some_and(tessera_grid::Cell::is_wide);
            (grid.rows(), grid.cols(), cursor, wide)
        });
        Ok(ViewMetrics {
            pixels: size.pixels,
            scale: size.scale,
            cell,
            padding_px,
            rows,
            cols,
            font_size_pt,
            cursor_row: cursor.row.min(rows.saturating_sub(1)),
            cursor_col: cursor.col.min(cols.saturating_sub(1)),
            // TODO(#8): also false while DECTCEM hides the cursor.
            cursor_on_screen: !scrolled_back,
            cursor_wide,
        })
    }

    /// Makes `surface` session `id`'s view, `size` physical pixels at
    /// backing `scale`, and resizes the session to the grid that fits.
    ///
    /// `surface` must have been created on `instance`, which must come
    /// from [`App::view_instance`]. The first view brings up the process's
    /// GPU on `instance` (an adapter that can present to `surface`; about
    /// 100 ms) and loads the font database; later views share both. An
    /// attach after the GPU's device was lost, or after the last view
    /// detached, brings up a new GPU (the font database is kept). The
    /// view draws with the configured font family, font size and opacity
    /// and the current theme's colours (see [`App::apply_theme`]). A zero
    /// `size` is allowed: nothing is drawn until a non-empty
    /// [`App::resize_view`], and the session keeps its size meanwhile.
    ///
    /// The session's grid follows the view: it is resized to
    /// [`view::grid_size_for`] the surface here and on every
    /// [`App::resize_view`] and [`App::set_view_font_size`]. A PTY that
    /// rejects the size is logged, not reported; the view is attached
    /// either way.
    ///
    /// Must be called on the main thread.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::ViewAttached`] if it already has a view,
    /// [`Error::Gpu`] if no GPU can present to the surface or the renderer
    /// cannot be created (no usable font); nothing is attached then.
    pub fn attach_view(
        &self,
        id: SessionId,
        instance: wgpu::Instance,
        surface: wgpu::Surface<'static>,
        size: PhysicalSize,
        scale: f32,
    ) -> Result<()> {
        self.attach_with(
            id,
            instance,
            surface,
            |surface| Some(surface),
            |gpu, surface, fonts, theme| View::new(gpu, surface, fonts, theme, size, scale),
        )
    }

    /// [`App::attach_view`] for any target: `surface_of` names the surface
    /// the GPU must be able to present to (if any), and `build` makes the
    /// view on the GPU, the fonts and the current theme.
    fn attach_with<T>(
        &self,
        id: SessionId,
        instance: wgpu::Instance,
        target: T,
        surface_of: impl FnOnce(&T) -> Option<&wgpu::Surface<'static>>,
        build: impl FnOnce(Gpu, T, Fonts, Theme) -> Result<View, RenderError>,
    ) -> Result<()> {
        self.session(id)?;
        // Declared before the lock guard, so it is dropped after it even on
        // an early return: dropping a GPU waits for it to go idle.
        let retired: Option<Gpu>;
        let mut views = lock(&self.views);
        if views.contains_key(&id) {
            return Err(Error::ViewAttached(id));
        }
        let gpu;
        (gpu, retired) = self.gpu_for(instance, surface_of(&target))?;
        let theme = lock(&self.theme).clone();
        let fonts = self.fonts_for(&theme)?;
        let view = build(gpu, target, fonts, theme)?;
        let pty_size = view.pty_size();
        views.insert(id, view);
        drop(views);
        drop(retired);
        // A close that raced with this call (only possible when the
        // main-thread rule is broken) must not leave a view behind.
        if self.session(id).is_err() {
            let mut views = lock(&self.views);
            let removed = self.remove_view(&mut views, id);
            drop(views);
            drop(removed);
            return Err(Error::SessionNotFound(id));
        }
        self.follow_view(id, pty_size);
        Ok(())
    }

    /// Reconfigures session `id`'s view for `size` physical pixels at
    /// backing `scale` and resizes the session to the grid that fits.
    ///
    /// Call it whenever the view's size or backing scale changes (a window
    /// resize, a move to a display with another scale), and after a frame
    /// reported [`FrameStatus::Suboptimal`]. A zero `size` suspends drawing
    /// ([`App::render_view`] reports [`FrameStatus::Skipped`]) and leaves
    /// the session's size alone. Must be called on the main thread.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::NoView`] if it has no view.
    pub fn resize_view(&self, id: SessionId, size: PhysicalSize, scale: f32) -> Result<()> {
        let pty_size = self.with_view(id, |view| {
            view.resize(size, scale);
            view.pty_size()
        })?;
        self.follow_view(id, pty_size);
        Ok(())
    }

    /// Changes session `id`'s view to `size_pt` points (clamped like
    /// [`tessera_render::sanitize_font_size`]; a non-finite size becomes
    /// the default) and resizes the session to the grid that fits. Must be
    /// called on the main thread.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::NoView`] if it has no view.
    pub fn set_view_font_size(&self, id: SessionId, size_pt: f32) -> Result<()> {
        let pty_size = self.with_view(id, |view| {
            view.set_font_size(size_pt);
            view.pty_size()
        })?;
        self.follow_view(id, pty_size);
        Ok(())
    }

    /// Draws session `id`'s visible screen into its view and presents it.
    ///
    /// The session's terminal is locked only while its visible rows and
    /// cursor are copied out; shaping, drawing and waiting for a drawable
    /// happen after the lock is released. The cursor is a block, drawn
    /// hollow while the session is not focused ([`App::is_focused`]).
    /// Must be called on the main thread.
    ///
    /// [`FrameStatus::Skipped`] means nothing was drawn by this call, for
    /// one of three reasons: nothing changed since the last presented
    /// frame (an idle terminal does no GPU work), the call came sooner than
    /// the [`view::MIN_FRAME_INTERVAL`] schedule allows (paced, at most 240
    /// frames per second: the newest content is drawn by a later call), or
    /// the surface had nothing to draw into (zero-sized, occluded, no
    /// drawable). A host that renders from a display link of up to 240 Hz
    /// is never paced and need not retry: its next tick draws whatever is
    /// pending. A host that renders faster, or from output notifications,
    /// must schedule another render after a skip, since it cannot yet tell
    /// a paced skip from the others (ADR 0016).
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::NoView`] if it has no view, [`Error::Gpu`] if drawing
    /// failed. A swapchain that cannot be recovered
    /// ([`RenderError::SurfaceLost`]) and a lost GPU device
    /// ([`RenderError::DeviceLost`], reported by the first render after
    /// the loss, whether or not the frame had anything to draw) also
    /// **detach the view**: the frontend attaches a new one, which after a
    /// device loss is drawn whole on a new GPU. After any other GPU error
    /// the view stays attached and the next frame may succeed.
    pub fn render_view(&self, id: SessionId) -> Result<FrameStatus> {
        let session = self.session(id)?;
        let focused = session.is_focused();
        let mut views = lock(&self.views);
        let view = views.get_mut(&id).ok_or(Error::NoView(id))?;
        // Every view on a lost device is detached by its next render, even
        // one with nothing to draw, so none keeps showing a dead frame.
        let result = if view.gpu().is_lost() {
            Err(RenderError::DeviceLost)
        } else {
            session.with_terminal(|term| view.capture(term));
            view.render(focused)
        };
        // A loss during the frame may surface as any error (or none).
        let result = match result {
            Err(_) if view.gpu().is_lost() => Err(RenderError::DeviceLost),
            result => result,
        };
        match result {
            Ok(status) => Ok(status),
            Err(error @ (RenderError::SurfaceLost | RenderError::DeviceLost)) => {
                let removed = self.remove_view(&mut views, id);
                drop(views);
                drop(removed);
                Err(Error::Gpu(error))
            }
            Err(error) => Err(Error::Gpu(error)),
        }
    }

    /// Applies `config` with the colours `theme` (the `appearance.theme`
    /// it resolves to, [`tessera_config::ConfigStore::theme`]) to every
    /// view and to every view attached later, and makes `config` the one
    /// [`App::config`] returns.
    ///
    /// The renderer theme is built from the *new* configuration
    /// ([`view::theme_for`]: colours, the background's alpha times
    /// `appearance.opacity`, font family and size; line height and padding
    /// are the renderer's defaults, since the configuration has no keys for
    /// them). The new colours show from each view's next frame. When the
    /// configured `font_size` changed, every view is set to it, a size the
    /// user zoomed to included, and its session is resized to the grid
    /// that fits, after the views lock is released; otherwise each view
    /// keeps its size. A background that turns translucent (or opaque)
    /// switches each surface's compositing mode as its next frame starts,
    /// on the thread that draws it. A font *family* change applies only to
    /// a font database loaded later: the views keep the font they were
    /// attached with.
    ///
    /// When anything the views draw changed, the observer's
    /// [`SessionObserver::on_appearance_changed`] is then called on this
    /// thread with whether the background is opaque.
    ///
    /// May be called from any thread (the configuration watch calls it
    /// from its own, [`App::start_config_watch`]); it does no GPU work.
    pub fn apply_config(&self, config: &Config, theme: &tessera_config::Theme) {
        self.live().apply(config, theme);
    }

    /// Re-themes every view with `theme`'s colours under the configuration
    /// in effect: [`App::apply_config`] with [`App::config`]. Check, then
    /// act: the configuration is read first and applied with `theme` after,
    /// so a configuration the watch applies in between (on its own thread)
    /// is replaced by the one read; call it from one thread, or use
    /// [`App::apply_config`] with the configuration you mean.
    pub fn apply_theme(&self, theme: &tessera_config::Theme) {
        self.apply_config(&self.config(), theme);
    }

    /// The renderer theme views draw with now: the configured theme, or
    /// the last [`App::apply_config`].
    pub fn view_theme(&self) -> Theme {
        lock(&self.theme).clone()
    }

    /// Detaches and releases session `id`'s view. The session keeps its
    /// size. The last view to go releases the process's GPU too (the font
    /// database stays loaded), so a GPU that went away (an external GPU
    /// unplugged) is not reused: the next attach brings up a new one. Must
    /// be called on the main thread.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::NoView`] if it has no view.
    pub fn detach_view(&self, id: SessionId) -> Result<()> {
        self.session(id)?;
        let mut views = lock(&self.views);
        let removed = self.remove_view(&mut views, id).ok_or(Error::NoView(id))?;
        drop(views);
        drop(removed);
        Ok(())
    }

    /// Removes session `id`'s view from `views`, and takes the cached GPU
    /// out of its slot when it was the last one. The caller drops what this
    /// returns after releasing the `views` lock: dropping a view's surface
    /// and the last clone of a GPU (its queue waits for the GPU to go idle)
    /// must not happen under a lock.
    #[must_use = "drop it after releasing the view lock"]
    fn remove_view(&self, views: &mut HashMap<SessionId, View>, id: SessionId) -> Option<Removed> {
        let view = views.remove(&id)?;
        let gpu = if views.is_empty() {
            lock(&self.gpu).take()
        } else {
            None
        };
        Some(Removed { _view: view, gpu })
    }

    /// Runs `f` on session `id`'s view.
    fn with_view<R>(&self, id: SessionId, f: impl FnOnce(&mut View) -> R) -> Result<R> {
        self.session(id)?;
        let mut views = lock(&self.views);
        let view = views.get_mut(&id).ok_or(Error::NoView(id))?;
        Ok(f(view))
    }

    /// The process's GPU, brought up on `instance` (for `surface`, if
    /// given) when there is none yet or its device was lost, and the lost
    /// one it replaced, for the caller to drop once it holds no lock.
    /// Views still on a lost GPU keep their clone of it until their next
    /// render detaches them.
    fn gpu_for(
        &self,
        instance: wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
    ) -> Result<(Gpu, Option<Gpu>)> {
        let mut slot = lock(&self.gpu);
        if let Some(gpu) = slot.as_ref().filter(|gpu| !gpu.is_lost()) {
            return Ok((gpu.clone(), None));
        }
        let gpu = Gpu::with_compatible_surface(instance, surface)?;
        let retired = slot.replace(gpu.clone());
        Ok((gpu, retired))
    }

    /// The process's font database, loaded for `theme`'s family if this is
    /// the first view.
    fn fonts_for(&self, theme: &Theme) -> Result<Fonts> {
        let mut slot = lock(&self.fonts);
        if let Some(fonts) = slot.as_ref() {
            return Ok(fonts.clone());
        }
        let fonts = Fonts::new(&FontConfig {
            family: theme.font_family.clone(),
            size_pt: theme.font_size,
            scale: 1.0,
            fallback: Vec::new(),
        })?;
        *slot = Some(fonts.clone());
        Ok(fonts)
    }

    /// Resizes session `id` to follow its view; `None` (an empty surface)
    /// leaves it alone. A failure is logged: the view itself is fine.
    fn follow_view(&self, id: SessionId, size: Option<PtySize>) {
        if let Some(size) = size
            && let Err(error) = self.resize(id, size)
        {
            tracing::warn!(session = %id, %error, ?size, "resizing a session to its view failed");
        }
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

    /// The theme session `id`'s view's renderer draws with, or `None`
    /// without a view.
    ///
    /// Exists only so tests can prove that [`App::apply_theme`] reaches the
    /// renderers of views already attached.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn view_renderer_theme(&self, id: SessionId) -> Option<Theme> {
        lock(&self.views).get(&id).map(|view| view.theme().clone())
    }

    /// The compositing mode session `id`'s surface uses now, and whether
    /// its next frame switches to a translucent one; `None` without a view.
    ///
    /// Exists only so tests can prove that a translucent re-theme switches
    /// the surfaces of views already attached.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn view_alpha_modes(&self, id: SessionId) -> Option<(wgpu::CompositeAlphaMode, bool)> {
        lock(&self.views).get(&id).and_then(View::alpha_modes)
    }

    /// The compositing modes session `id`'s surface supports; `None`
    /// without a view. For tests that must know whether a translucent
    /// surface is possible before they expect one.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn view_supported_alpha_modes(
        &self,
        id: SessionId,
    ) -> Option<Vec<wgpu::CompositeAlphaMode>> {
        lock(&self.views)
            .get(&id)
            .and_then(View::supported_alpha_modes)
    }

    /// Loses the process GPU's device, as a GPU reset or an unplugged
    /// external GPU would (through [`Gpu::lose_device_for_test`]). Returns
    /// `false` when no GPU is up.
    ///
    /// Exists only so tests can prove that views recover from a device
    /// loss.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn lose_gpu_for_test(&self) -> bool {
        let gpu = lock(&self.gpu).clone();
        gpu.inspect(Gpu::lose_device_for_test).is_some()
    }

    /// Panics while holding the view table lock, poisoning it.
    ///
    /// Exists only so tests can prove that a panic inside a view call
    /// caught at the FFI boundary leaves the views usable.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn panic_holding_view_lock(&self) {
        let _views = lock(&self.views);
        panic!("deliberate panic for testing");
    }
}

/// A view taken out of the view table, and the cached GPU when it was the
/// last view. Dropped after the locks are released: the view first, then
/// the GPU (fields drop in declaration order).
#[must_use = "drop it after releasing the view lock"]
struct Removed {
    _view: View,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "held only to be dropped after the locks")
    )]
    gpu: Option<Gpu>,
}

impl Drop for App {
    /// Detaches every view, closes every session and waits for the reaper
    /// to finish every close, including those [`App::close_session`]
    /// started: at most about
    /// [`HANGUP_GRACE`](session::HANGUP_GRACE) +
    /// [`KILL_GRACE`](session::KILL_GRACE) however many sessions there are.
    fn drop(&mut self) {
        // First, so no change is applied to views being released.
        self.stop_config_watch();
        let views: Vec<_> = lock(&self.views).drain().collect();
        drop(views);
        let gpu = lock(&self.gpu).take();
        drop(gpu);
        let sessions: Vec<_> = lock(&self.sessions)
            .drain()
            .map(|(_, session)| session)
            .collect();
        Self::begin_closing(&sessions);
        // Queued without starting the reaper thread: if no close started
        // it, the shutdown runs the closes on this thread instead.
        self.reaper.queue(sessions);
        self.reaper.shut_down();
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
        /// While set, `try_write` fails with `BrokenPipe` (the child exited
        /// under a pending write).
        broken: Arc<AtomicBool>,
        /// While set, `try_write` panics.
        panics: Arc<AtomicBool>,
        /// Every size the PTY was resized to.
        resizes: Arc<Mutex<Vec<PtySize>>>,
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
            assert!(!self.panics.load(Ordering::SeqCst), "fake PTY write panics");
            if self.broken.load(Ordering::SeqCst) {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if self.would_block.load(Ordering::SeqCst) {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.write(bytes)
        }

        fn resize(&mut self, size: PtySize) -> Result<(), PtyError> {
            lock(&self.resizes).push(size);
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
        broken: Arc<AtomicBool>,
        panics: Arc<AtomicBool>,
        resizes: Arc<Mutex<Vec<PtySize>>>,
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
        let broken = Arc::new(AtomicBool::new(false));
        let panics = Arc::new(AtomicBool::new(false));
        let resizes = Arc::new(Mutex::new(Vec::new()));
        let pty = FakePty {
            events: Arc::clone(&events),
            written: Arc::clone(&written),
            hangups: Arc::clone(&hangups),
            would_block: Arc::clone(&would_block),
            ignore_hangup,
            ignore_kill,
            broken: Arc::clone(&broken),
            panics: Arc::clone(&panics),
            resizes: Arc::clone(&resizes),
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
                broken,
                panics,
                resizes,
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

    /// How long `close_session` may take for a child that ignores
    /// `SIGHUP` (issue #79).
    const CLOSE_RETURNS_WITHIN: Duration = Duration::from_millis(5);

    /// Slack on top of the grace periods for the reaper to finish a close.
    /// Generous because the reaper may be scheduled late on a loaded
    /// machine: under `taskpolicy -b` (background quality of service) fake closes finished
    /// 1.21-1.25 s after a 1 s grace.
    const REAP_SLACK: Duration = Duration::from_millis(500);

    /// Waits until the reaper has finished every close; returns how long
    /// that took from `since`.
    fn wait_reaped(app: &App, since: Instant) -> Duration {
        let deadline = Instant::now() + session::HANGUP_GRACE + session::KILL_GRACE + TIMEOUT;
        while app.closing_sessions() > 0 {
            assert!(Instant::now() < deadline, "closes never finished");
            thread::sleep(Duration::from_millis(1));
        }
        since.elapsed()
    }

    #[test]
    fn close_returns_at_once_and_kills_a_child_that_ignores_hangup() {
        let app = app();
        let (id, fake) = attach_fake(&app, true);
        let started = Instant::now();
        app.close_session(id).unwrap();
        let took = started.elapsed();
        assert!(took < CLOSE_RETURNS_WITHIN, "close took {took:?}");
        assert_eq!(fake.hangups.load(Ordering::SeqCst), 1);
        // Closed: the id is gone at once, the child is not killed yet.
        assert_eq!(app.session_ids(), Vec::<SessionId>::new());
        assert!(matches!(
            app.write(id, b"x"),
            Err(Error::SessionNotFound(_))
        ));
        assert!(matches!(
            app.resize(id, PtySize::new(30, 100)),
            Err(Error::SessionNotFound(_))
        ));
        assert!(matches!(
            app.close_session(id),
            Err(Error::SessionNotFound(_))
        ));
        assert!(lock(&fake.events).is_some(), "killed before the grace");
        assert_eq!(app.closing_sessions(), 1);
        let reaped = wait_reaped(&app, started);
        assert!(lock(&fake.events).is_none(), "kill should end the fake");
        assert!(reaped >= session::HANGUP_GRACE, "reaped after {reaped:?}");
        assert!(
            reaped < session::HANGUP_GRACE + REAP_SLACK,
            "reaped after {reaped:?}"
        );
    }

    /// Closes `ids` one after another, as Close Window closes its tabs, and
    /// checks that no close waited for its child.
    ///
    /// Issue #79's bar is 5 ms for 20 closes, 0.25 ms each. The median close
    /// is held to that: a loaded scheduler preempts a few closes, not most of
    /// them. The whole loop is held to a quarter of `HANGUP_GRACE`, which a
    /// single close waiting for a grace period would exceed, but a few
    /// preemptions do not (20 closes took 15 ms at load 10 and 51 ms at
    /// background priority with three test processes, against 0.2-0.5 ms
    /// idle).
    fn close_in_a_loop(app: &App, ids: &[SessionId]) {
        let started = Instant::now();
        let mut each: Vec<Duration> = ids
            .iter()
            .map(|&id| {
                let close = Instant::now();
                app.close_session(id).unwrap();
                close.elapsed()
            })
            .collect();
        let took = started.elapsed();
        each.sort_unstable();
        let median = each[each.len() / 2];
        let per_close = CLOSE_RETURNS_WITHIN / 20;
        assert!(median < per_close, "median close took {median:?}");
        assert!(
            took < session::HANGUP_GRACE / 4,
            "{} closes took {took:?}",
            ids.len()
        );
    }

    #[test]
    fn closing_twenty_stubborn_sessions_in_a_loop_returns_at_once() {
        let app = app();
        let sessions: Vec<_> = (0..20).map(|_| attach_fake(&app, true)).collect();
        let started = Instant::now();
        let ids: Vec<_> = sessions.iter().map(|&(id, _)| id).collect();
        close_in_a_loop(&app, &ids);
        assert_eq!(app.session_ids(), Vec::<SessionId>::new());
        assert_eq!(app.closing_sessions(), 20);
        let reaped = wait_reaped(&app, started);
        assert!(reaped >= session::HANGUP_GRACE, "reaped after {reaped:?}");
        // One shared grace, not twenty in a row.
        assert!(
            reaped < session::HANGUP_GRACE + REAP_SLACK,
            "reaped after {reaped:?}"
        );
        for (_, fake) in &sessions {
            assert!(lock(&fake.events).is_none(), "every child should be killed");
        }
    }

    #[test]
    fn a_closed_session_that_exits_later_reports_nothing() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, true);
        fake.send(PtyEvent::Output(b"before".to_vec()));
        assert_eq!(next(&events), (id, SessionEvent::Output));
        app.close_session(id).unwrap();
        thread::sleep(Duration::from_millis(300));
        // Output, a bell, a title and the exit, all after the close: the
        // reader still applies them, but none reaches the observer.
        fake.send(PtyEvent::Output(b"after\x07\x1b]0;title\x07".to_vec()));
        fake.exit(ExitStatus::Exited(0));
        let reaped = wait_reaped(&app, Instant::now());
        assert!(reaped < REAP_SLACK, "reaped {reaped:?} after the exit");
        assert_eq!(
            events.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn a_session_can_close_itself_from_its_observer() {
        let app = Arc::new(app());
        let (tx, events) = mpsc::channel();
        let weak = Arc::downgrade(&app);
        app.set_observer(Some(Box::new(move |id, event| {
            if event == SessionEvent::Bell
                && let Some(app) = weak.upgrade()
            {
                let _ = tx.send((id, Some(app.close_session(id).is_ok())));
            } else {
                let _ = tx.send((id, None));
            }
        })));
        let (id, fake) = attach_fake(&app, true);
        let (other, other_fake) = attach_fake(&app, false);
        fake.send(PtyEvent::Output(b"\x07".to_vec()));
        assert_eq!(events.recv_timeout(TIMEOUT).unwrap(), (id, None));
        // Closing from inside the bell's own call neither deadlocks nor
        // fails, and nothing of the session follows it.
        assert_eq!(events.recv_timeout(TIMEOUT).unwrap(), (id, Some(true)));
        fake.send(PtyEvent::Output(b"more".to_vec()));
        other_fake.send(PtyEvent::Output(b"x".to_vec()));
        assert_eq!(events.recv_timeout(TIMEOUT).unwrap(), (other, None));
        assert_eq!(app.session_ids(), vec![other]);
        assert_eq!(
            events.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
        app.set_observer(None);
    }

    #[test]
    fn close_waits_for_an_observer_call_in_progress() {
        let app = app();
        let (entered_tx, entered) = mpsc::channel();
        let (release_tx, release) = mpsc::channel::<()>();
        let release = Mutex::new(release);
        let returned = Arc::new(AtomicBool::new(false));
        let returned_in_observer = Arc::clone(&returned);
        app.set_observer(Some(Box::new(move |_, _| {
            let _ = entered_tx.send(());
            let _ = lock(&release).recv_timeout(TIMEOUT);
            returned_in_observer.store(true, Ordering::SeqCst);
        })));
        let (id, fake) = attach_fake(&app, false);
        fake.send(PtyEvent::Output(b"x".to_vec()));
        entered.recv_timeout(TIMEOUT).unwrap();
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(50));
                release_tx.send(()).unwrap();
            });
            app.close_session(id).unwrap();
            // The call that was running when the close began has returned.
            assert!(returned.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn a_close_while_the_reader_is_about_to_deliver_silences_it() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, true);
        // Stop the reader between taking the observer slot and the delivery
        // lock, once: a close there must still silence the event.
        let (paused_tx, paused) = mpsc::channel();
        let (go_tx, go) = mpsc::channel::<()>();
        let go = Mutex::new(go);
        let fired = AtomicBool::new(false);
        // Held until the pause is removed, so its key stays unique.
        let session = app.session(id).unwrap();
        session::set_delivery_pause(Some((
            session.delivery_key(),
            Arc::new(move || {
                if !fired.swap(true, Ordering::SeqCst) {
                    let _ = paused_tx.send(());
                    let _ = lock(&go).recv_timeout(TIMEOUT);
                }
            }),
        )));
        fake.send(PtyEvent::Output(b"x".to_vec()));
        paused.recv_timeout(TIMEOUT).expect("reader never paused");
        let started = Instant::now();
        app.close_session(id).unwrap();
        // The paused reader holds no delivery lock, so the close is prompt.
        assert!(started.elapsed() < TIMEOUT, "close waited for the reader");
        go_tx.send(()).unwrap();
        session::set_delivery_pause(None);
        drop(session);
        assert_eq!(
            events.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn dropping_the_app_finishes_earlier_closes_within_the_same_cap() {
        let app = app();
        let fakes: Vec<_> = (0..4).map(|_| attach_fake(&app, true)).collect();
        let started = Instant::now();
        app.close_session(fakes[0].0).unwrap();
        app.close_session(fakes[1].0).unwrap();
        drop(app);
        let elapsed = started.elapsed();
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

    /// Names of the threads that ran the start hook.
    static HOOKED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Makes the next reaper thread's start hook panic, once.
    static PANIC_IN_NEXT_REAPER: AtomicBool = AtomicBool::new(false);

    /// The tests' start hook: records the thread's name and panics in a
    /// reaper when asked to. Harmless to every other test in the process
    /// (under `cargo test`): it only records, and a panic it raises is
    /// caught.
    fn test_start_hook() {
        let name = thread::current().name().unwrap_or("?").to_owned();
        let reaper = name == "tessera-reaper";
        lock(&HOOKED).push(name);
        assert!(
            !(reaper && PANIC_IN_NEXT_REAPER.swap(false, Ordering::SeqCst)),
            "deliberate start hook panic"
        );
    }

    /// Installs [`test_start_hook`] for the process, once.
    fn install_test_start_hook() {
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| assert!(session::set_thread_start_hook(test_start_hook)));
    }

    #[test]
    fn a_panicking_start_hook_does_not_strand_closing_children() {
        install_test_start_hook();
        PANIC_IN_NEXT_REAPER.store(true, Ordering::SeqCst);
        let app = app();
        let (id, fake) = attach_fake(&app, true);
        let started = Instant::now();
        app.close_session(id).unwrap();
        // The reaper's hook panicked, yet it kills and reaps the child.
        let reaped = wait_reaped(&app, started);
        assert!(lock(&fake.events).is_none(), "kill should end the fake");
        assert!(
            reaped < session::HANGUP_GRACE + REAP_SLACK,
            "reaped after {reaped:?}"
        );
        // Under nextest this process ran no other reaper, so the panic was
        // this one's.
        assert!(!PANIC_IN_NEXT_REAPER.load(Ordering::SeqCst));
    }

    #[test]
    fn every_core_thread_runs_the_start_hook() {
        install_test_start_hook();
        assert!(
            !session::set_thread_start_hook(|| {}),
            "only the first hook is kept"
        );
        let app = app();
        let (id, _fake) = attach_fake(&app, false);
        app.close_session(id).unwrap();
        wait_reaped(&app, Instant::now());
        let hooked = lock(&HOOKED).clone();
        for name in [
            format!("tessera-session-{id}"),
            format!("tessera-input-{id}"),
            "tessera-reaper".to_owned(),
        ] {
            assert!(hooked.contains(&name), "{name} missing from {hooked:?}");
        }
    }

    #[test]
    fn close_racing_resize_and_input_from_another_thread() {
        let app = Arc::new(app());
        for _ in 0..50 {
            let (id, fake) = attach_fake(&app, false);
            let racer = {
                let app = Arc::clone(&app);
                thread::spawn(move || {
                    let key = InputEvent::Key(KeyEvent::new(Key::Char('k'), Modifiers::NONE));
                    loop {
                        let resized = app.resize(id, PtySize::new(30, 100));
                        let typed = app.send_input(id, key.clone());
                        match (resized, typed) {
                            (Err(Error::SessionNotFound(_)), _) => return,
                            (
                                Ok(()),
                                Ok(_) | Err(Error::Write { .. } | Error::SessionNotFound(_)),
                            ) => {}
                            other => panic!("unexpected {other:?}"),
                        }
                    }
                })
            };
            thread::sleep(Duration::from_micros(200));
            app.close_session(id).unwrap();
            racer.join().unwrap();
            drop(fake);
        }
        wait_reaped(&app, Instant::now());
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
    fn spawn_refuses_sizes_resize_would_refuse() {
        let app = app();
        for (rows, cols) in [
            (0, 80),
            (24, 0),
            (view::MAX_VIEW_ROWS + 1, 80),
            (24, view::MAX_VIEW_COLS + 1),
            (u16::MAX, u16::MAX),
        ] {
            let size = PtySize::new(rows, cols).with_pixels(u16::MAX, u16::MAX);
            let result = app.spawn_session(sh("exit 0").size(size));
            assert!(
                matches!(result, Err(Error::Pty(PtyError::InvalidSize(s))) if s == size),
                "{rows}x{cols}: {result:?}"
            );
        }
        assert_eq!(app.session_ids(), Vec::<SessionId>::new());
        // The largest size a view derives (and `resize` accepts) spawns,
        // with any pixel size.
        let largest =
            PtySize::new(view::MAX_VIEW_ROWS, view::MAX_VIEW_COLS).with_pixels(u16::MAX, u16::MAX);
        let id = app.spawn_session(sh("exit 0").size(largest)).unwrap();
        let grid = app
            .with_terminal(id, |t| (t.grid().rows(), t.grid().cols()))
            .unwrap();
        assert_eq!(grid, (view::MAX_VIEW_ROWS, view::MAX_VIEW_COLS));
        app.close_session(id).unwrap();
    }

    #[test]
    fn oversized_spawn_arguments_are_refused() {
        let app = app();
        let result = app.spawn_session(sh("exit 0").arg("x".repeat(5 * 1024 * 1024)));
        assert!(
            matches!(result, Err(Error::Pty(PtyError::ArgumentsTooLong { .. }))),
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

    /// Which of `pids` still exist, zombies included (so a pid that is
    /// missing was reaped), from one `ps` run.
    fn existing(pids: &[u32]) -> Vec<u32> {
        let list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let output = std::process::Command::new("/bin/ps")
            .args(["-o", "pid=", "-p", &list])
            .output()
            .expect("run ps");
        let mut found: Vec<u32> = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(|pid| pid.parse().expect("ps prints pids"))
            .collect();
        found.sort_unstable();
        found
    }

    /// Asserts that every one of `pids` has been reaped (`waitpid`).
    fn assert_gone(pids: &[u32]) {
        assert_eq!(existing(pids), Vec::<u32>::new(), "children not reaped");
    }

    /// Spawns `sh -c script` and waits until its screen shows `ready`;
    /// returns the session and the child's pid.
    fn spawn_ready(app: &App, script: &str) -> (SessionId, u32) {
        let id = app.spawn_session(sh(script)).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        while !app
            .with_terminal(id, |term| {
                let grid = term.grid();
                (0..grid.rows())
                    .filter_map(|row| grid.line(row))
                    .any(|line| line.text().contains("ready"))
            })
            .unwrap()
        {
            assert!(Instant::now() < deadline, "{script:?} never got ready");
            thread::sleep(Duration::from_millis(2));
        }
        let pid = app.session(id).unwrap().child_pid().expect("child pid");
        (id, pid)
    }

    /// A shell that ignores `SIGHUP` and never exits on its own.
    const STUBBORN: &str = "trap '' HUP; echo ready; exec sleep 100";

    #[test]
    fn close_returns_at_once_for_a_shell_that_ignores_sighup() {
        let app = app();
        let (id, pid) = spawn_ready(&app, STUBBORN);
        let started = Instant::now();
        app.close_session(id).unwrap();
        let took = started.elapsed();
        assert!(took < CLOSE_RETURNS_WITHIN, "close took {took:?}");
        assert_eq!(app.session_ids(), Vec::<SessionId>::new());
        assert_eq!(existing(&[pid]), vec![pid], "killed before the grace");
        // The session's reader ends only after the child's exit, which the
        // PTY reports after `waitpid`; `ps` then confirms the reap.
        let reaped = wait_reaped(&app, started);
        assert_gone(&[pid]);
        assert!(reaped >= session::HANGUP_GRACE, "reaped after {reaped:?}");
        let bound = session::HANGUP_GRACE + session::KILL_GRACE + REAP_SLACK;
        assert!(reaped < bound, "reaped after {reaped:?}");
    }

    #[test]
    fn closing_twenty_shells_that_ignore_sighup_returns_at_once() {
        let app = app();
        let sessions: Vec<_> = (0..20).map(|_| spawn_ready(&app, STUBBORN)).collect();
        let started = Instant::now();
        let ids: Vec<_> = sessions.iter().map(|&(id, _)| id).collect();
        close_in_a_loop(&app, &ids);
        let pids: Vec<_> = sessions.iter().map(|&(_, pid)| pid).collect();
        let reaped = wait_reaped(&app, started);
        assert_gone(&pids);
        assert!(reaped >= session::HANGUP_GRACE, "reaped after {reaped:?}");
        let bound = session::HANGUP_GRACE + session::KILL_GRACE + REAP_SLACK;
        assert!(reaped < bound, "reaped after {reaped:?}");
    }

    #[test]
    fn a_closed_shell_that_exits_later_reports_nothing() {
        let app = app();
        // When each observer call ran.
        let stamps = Arc::new(Mutex::new(Vec::new()));
        {
            let stamps = Arc::clone(&stamps);
            app.set_observer(Some(Box::new(move |_, _| {
                lock(&stamps).push(Instant::now());
            })));
        }
        let (id, pid) = spawn_ready(
            &app,
            "trap '' HUP; echo ready; sleep 0.3; printf '\\a'; exit 0",
        );
        app.close_session(id).unwrap();
        let closed = Instant::now();
        let reaped = wait_reaped(&app, closed);
        assert_gone(&[pid]);
        // It exited on its own, before the kill.
        assert!(reaped < session::HANGUP_GRACE, "reaped after {reaped:?}");
        let stamps = lock(&stamps);
        assert!(!stamps.is_empty(), "the ready output was observed");
        let late = stamps.iter().filter(|&&at| at >= closed).count();
        assert_eq!(late, 0, "observer calls after close returned");
    }

    #[test]
    fn close_during_an_output_flood_is_prompt_and_silent() {
        let app = app();
        let after_close = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let (tx, events) = mpsc::channel();
        {
            let after_close = Arc::clone(&after_close);
            let closed = Arc::clone(&closed);
            app.set_observer(Some(Box::new(move |id, event| {
                if closed.load(Ordering::SeqCst) {
                    after_close.fetch_add(1, Ordering::SeqCst);
                }
                let _ = tx.send((id, event));
            })));
        }
        // No `ready` marker: the flood would scroll it away before it is
        // seen. The trap runs before `yes` starts, so flood output proves it
        // is installed.
        let id = app
            .spawn_session(sh("trap '' HUP; exec yes flood"))
            .unwrap();
        let pid = app.session(id).unwrap().child_pid().expect("child pid");
        // Let the flood run: many outputs, the reader busy under the lock.
        let flooding = Instant::now();
        let mut outputs = 0;
        while outputs < 200 && flooding.elapsed() < TIMEOUT {
            if let Ok((_, SessionEvent::Output)) = events.recv_timeout(TIMEOUT) {
                outputs += 1;
            }
        }
        assert_eq!(outputs, 200, "no flood");
        let started = Instant::now();
        app.close_session(id).unwrap();
        closed.store(true, Ordering::SeqCst);
        let took = started.elapsed();
        assert!(took < CLOSE_RETURNS_WITHIN, "close took {took:?}");
        let reaped = wait_reaped(&app, started);
        assert_gone(&[pid]);
        assert!(
            reaped < session::HANGUP_GRACE + session::KILL_GRACE + REAP_SLACK,
            "reaped after {reaped:?}"
        );
        assert_eq!(after_close.load(Ordering::SeqCst), 0, "events after close");
    }

    #[test]
    fn close_with_input_pending_to_a_shell_that_ignores_sighup() {
        let app = app();
        let (id, pid) = spawn_ready(&app, STUBBORN);
        // `sleep` never reads: the paste fills the terminal's input buffer
        // and the PTY writer blocks in write(2) (ADR 0011).
        let paste = "p".repeat(4 * 1024 * 1024);
        let outcome = app.send_input(id, InputEvent::Paste(paste)).unwrap();
        assert_eq!(outcome, InputOutcome::Consumed);
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        app.close_session(id).unwrap();
        let took = started.elapsed();
        assert!(took < CLOSE_RETURNS_WITHIN, "close took {took:?}");
        let reaped = wait_reaped(&app, started);
        assert_gone(&[pid]);
        let bound = session::HANGUP_GRACE + session::KILL_GRACE + REAP_SLACK;
        assert!(reaped < bound, "reaped after {reaped:?}");
    }

    /// Waits until exactly `expected` has reached the fake PTY.
    fn written_eventually(fake: &Fake, expected: &[u8]) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let written = lock(&fake.written).clone();
            if written == expected {
                return;
            }
            assert!(
                written.len() < expected.len() && Instant::now() < deadline,
                "wrote {} bytes ending {:?}, expected {} bytes ending {:?}",
                written.len(),
                &written[written.len().saturating_sub(16)..],
                expected.len(),
                &expected[expected.len().saturating_sub(16)..],
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Proves nothing was queued for the child so far: input is written in
    /// order, so a marker key sent now must be the first byte to arrive.
    fn assert_nothing_written(app: &App, id: SessionId, fake: &Fake) {
        let marker = KeyEvent::new(Key::Char('#'), Modifiers::NONE);
        assert_eq!(key(app, id, marker), InputOutcome::Consumed);
        written_eventually(fake, b"#");
    }

    /// [`assert_nothing_written`] after `before` was written.
    fn assert_nothing_written_after(fake: &Fake, before: &[u8], app: &App, id: SessionId) {
        let marker = KeyEvent::new(Key::Char('#'), Modifiers::NONE);
        assert_eq!(key(app, id, marker), InputOutcome::Consumed);
        written_eventually(fake, &[before, b"#"].concat());
    }

    fn key(app: &App, id: SessionId, event: KeyEvent) -> InputOutcome {
        app.send_input(id, InputEvent::Key(event)).unwrap()
    }

    fn scroll(app: &App, id: SessionId, delta_lines: f32) -> InputOutcome {
        let event = input::ScrollEvent {
            delta_lines,
            ..input::ScrollEvent::default()
        };
        app.send_input(id, InputEvent::Scroll(event)).unwrap()
    }

    /// Feeds `lines` newline-terminated lines and waits until the terminal
    /// has applied them.
    fn fill_scrollback(fake: &Fake, events: &Receiver<(SessionId, SessionEvent)>, lines: usize) {
        let text = "x\r\n".repeat(lines);
        fake.send(PtyEvent::Output(text.into_bytes()));
        assert!(matches!(next(events), (_, SessionEvent::Output)));
    }

    use input::{FocusEvent, Key, KeyEvent, KeyEventKind, Modifiers, MouseButton, MouseEventKind};

    #[test]
    fn keys_are_encoded_and_written() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        let a = KeyEvent::new(Key::Char('a'), Modifiers::NONE);
        assert_eq!(key(&app, id, a.clone()), InputOutcome::Consumed);
        let enter = KeyEvent::new(Key::Enter, Modifiers::NONE);
        assert_eq!(key(&app, id, enter), InputOutcome::Consumed);
        let ctrl_c = KeyEvent::new(Key::Char('c'), Modifiers::CTRL);
        assert_eq!(key(&app, id, ctrl_c), InputOutcome::Consumed);
        let up = KeyEvent::new(Key::Up, Modifiers::NONE);
        assert_eq!(key(&app, id, up), InputOutcome::Consumed);
        written_eventually(&fake, b"a\r\x03\x1b[A");
    }

    #[test]
    fn keys_that_encode_to_nothing_are_ignored() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        let release =
            KeyEvent::new(Key::Char('a'), Modifiers::NONE).with_kind(KeyEventKind::Release);
        let shift = KeyEvent::new(Key::LeftShift, Modifiers::SHIFT);
        let control_char = KeyEvent::new(Key::Char('\u{1b}'), Modifiers::NONE);
        for event in [release, shift, control_char] {
            assert_eq!(key(&app, id, event), InputOutcome::Ignored);
        }
        assert_nothing_written(&app, id, &fake);
    }

    #[test]
    fn control_text_falls_back_to_the_key() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        let event = KeyEvent::new(Key::Char('a'), Modifiers::NONE).with_text("\x1b[31m");
        assert_eq!(key(&app, id, event), InputOutcome::Consumed);
        let commit = KeyEvent::new(Key::Unidentified, Modifiers::NONE).with_text("\x1b[31m");
        assert_eq!(key(&app, id, commit), InputOutcome::Ignored);
        written_eventually(&fake, b"a");
    }

    #[test]
    fn composed_text_is_sent_verbatim() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        // Option+E then E on a US layout: the frontend passes the composed
        // text without ALT.
        let event = KeyEvent::new(Key::Char('e'), Modifiers::NONE).with_text("é");
        assert_eq!(key(&app, id, event), InputOutcome::Consumed);
        written_eventually(&fake, "é".as_bytes());
    }

    #[test]
    fn a_key_is_queued_while_the_child_is_not_reading() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        fake.would_block.store(true, Ordering::SeqCst);
        // The PTY refuses every write, so returning at all proves the call
        // did not wait for it.
        assert_eq!(
            key(&app, id, KeyEvent::new(Key::Char('x'), Modifiers::NONE)),
            InputOutcome::Consumed
        );
        thread::sleep(Duration::from_millis(30));
        assert!(lock(&fake.written).is_empty());
        fake.would_block.store(false, Ordering::SeqCst);
        written_eventually(&fake, b"x");
    }

    #[test]
    fn a_paste_larger_than_the_pty_queue_never_blocks_and_keeps_order() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        fake.would_block.store(true, Ordering::SeqCst);
        let big = "p".repeat(3 * tessera_pty::WRITE_QUEUE_BYTES);
        let paste = app.send_input(id, InputEvent::Paste(big.clone())).unwrap();
        assert_eq!(paste, InputOutcome::Consumed);
        let after = KeyEvent::new(Key::Char('k'), Modifiers::NONE);
        assert_eq!(key(&app, id, after), InputOutcome::Consumed);
        assert!(lock(&fake.written).is_empty());
        fake.would_block.store(false, Ordering::SeqCst);
        let mut expected = big.into_bytes();
        expected.push(b'k');
        written_eventually(&fake, &expected);
    }

    #[test]
    fn pending_input_is_bounded_and_refused_whole() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        fake.would_block.store(true, Ordering::SeqCst);
        let would_block = |result: Result<InputOutcome>| {
            write_error_kind(&result) == Some(io::ErrorKind::WouldBlock)
        };
        let too_big = "x".repeat(session::PENDING_INPUT_BYTES + 1);
        assert!(would_block(app.send_input(id, InputEvent::Paste(too_big))));
        let exactly = "y".repeat(session::PENDING_INPUT_BYTES);
        let paste = app.send_input(id, InputEvent::Paste(exactly)).unwrap();
        assert_eq!(paste, InputOutcome::Consumed);
        let key_event = InputEvent::Key(KeyEvent::new(Key::Char('z'), Modifiers::NONE));
        assert!(would_block(app.send_input(id, key_event.clone())));
        fake.would_block.store(false, Ordering::SeqCst);
        // Once the child reads, there is room again.
        let deadline = Instant::now() + TIMEOUT;
        while lock(&fake.written).len() < session::PENDING_INPUT_BYTES {
            assert!(Instant::now() < deadline, "pending input never drained");
            thread::sleep(Duration::from_millis(5));
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match app.send_input(id, key_event.clone()) {
                Ok(outcome) => break assert_eq!(outcome, InputOutcome::Consumed),
                Err(err) => {
                    assert!(Instant::now() < deadline, "still refused: {err}");
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
        let deadline = Instant::now() + TIMEOUT;
        while lock(&fake.written).last() != Some(&b'z') {
            assert!(Instant::now() < deadline, "the key never arrived");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            lock(&fake.written)[..session::PENDING_INPUT_BYTES]
                .iter()
                .all(|&b| b == b'y')
        );
    }

    /// The kind of the `Error::Write` in `result`, if it is one.
    fn write_error_kind(result: &Result<InputOutcome>) -> Option<io::ErrorKind> {
        match result {
            Err(Error::Write { source, .. }) => Some(source.kind()),
            _ => None,
        }
    }

    /// Queues a paste the fake refuses, breaks the fake with `break_it`,
    /// then waits until input to the session fails with `kind`, and checks
    /// that closing it is still prompt.
    fn writer_failure_is_reported(break_it: impl FnOnce(&Fake), kind: io::ErrorKind) {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        fake.would_block.store(true, Ordering::SeqCst);
        let paste = app.send_input(id, InputEvent::Paste("pending".into()));
        assert_eq!(paste.unwrap(), InputOutcome::Consumed);
        break_it(&fake);
        fake.would_block.store(false, Ordering::SeqCst);
        let key_event = InputEvent::Key(KeyEvent::new(Key::Char('k'), Modifiers::NONE));
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let result = app.send_input(id, key_event.clone());
            if write_error_kind(&result) == Some(kind) {
                break;
            }
            assert!(
                result.is_ok() && Instant::now() < deadline,
                "unexpected {result:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        app.close_session(id).unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "close took {:?}",
            started.elapsed()
        );
        assert!(lock(&fake.written).is_empty());
    }

    #[test]
    fn a_failed_writer_refuses_later_input() {
        writer_failure_is_reported(
            |fake| fake.broken.store(true, Ordering::SeqCst),
            io::ErrorKind::BrokenPipe,
        );
    }

    #[test]
    fn a_panicking_writer_refuses_later_input() {
        writer_failure_is_reported(
            |fake| fake.panics.store(true, Ordering::SeqCst),
            io::ErrorKind::Other,
        );
    }

    #[test]
    fn close_drops_pending_input_promptly() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        fake.would_block.store(true, Ordering::SeqCst);
        let big = "p".repeat(2 * tessera_pty::WRITE_QUEUE_BYTES);
        let paste = app.send_input(id, InputEvent::Paste(big)).unwrap();
        assert_eq!(paste, InputOutcome::Consumed);
        let started = Instant::now();
        app.close_session(id).unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "close took {:?}",
            started.elapsed()
        );
        fake.would_block.store(false, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(30));
        assert!(lock(&fake.written).is_empty(), "pending input was written");
    }

    #[test]
    fn paste_is_sanitised_before_it_is_written() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        let outcome = app
            .send_input(id, InputEvent::Paste("a\nb\x1b[31mc\r\nd\u{9b}".into()))
            .unwrap();
        assert_eq!(outcome, InputOutcome::Consumed);
        let empty = app
            .send_input(id, InputEvent::Paste("\x1b\x07".into()))
            .unwrap();
        assert_eq!(empty, InputOutcome::Ignored);
        let marker = KeyEvent::new(Key::Char('m'), Modifiers::NONE);
        assert_eq!(key(&app, id, marker), InputOutcome::Consumed);
        written_eventually(&fake, b"a\rb[31mc\rdm");
    }

    #[test]
    fn paste_room_is_checked_on_the_raw_length() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        fake.would_block.store(true, Ordering::SeqCst);
        let room = 1024;
        let filler = "f".repeat(session::PENDING_INPUT_BYTES - room);
        assert_eq!(app.paste(id, &filler).unwrap(), InputOutcome::Consumed);
        // Sanitising would shrink this to `room` bytes, but its raw length
        // is checked first.
        let shrinks = "\r\n".repeat(room);
        assert_eq!(
            write_error_kind(&app.paste(id, &shrinks)),
            Some(io::ErrorKind::WouldBlock)
        );
        assert_eq!(
            app.check_input_room(id, room + 1)
                .map_err(|e| e.to_string()),
            Err(format!(
                "failed to write to session {id}: the session's pending input is full"
            ))
        );
        app.check_input_room(id, room).unwrap();
        // A pure-control paste that fits sends nothing.
        assert_eq!(app.paste(id, "\x1b\x07").unwrap(), InputOutcome::Ignored);
        let text = "p".repeat(room - 1) + "\n";
        assert_eq!(app.paste(id, &text).unwrap(), InputOutcome::Consumed);
        app.check_input_room(id, 0).unwrap();
        assert!(app.check_input_room(id, 1).is_err(), "the room is used up");
        fake.would_block.store(false, Ordering::SeqCst);
        let mut expected = filler.into_bytes();
        expected.extend_from_slice(text.trim_end().as_bytes());
        expected.push(b'\r');
        written_eventually(&fake, &expected);
        assert!(matches!(
            app.check_input_room(SessionId::from_raw(999), 1),
            Err(Error::SessionNotFound(_))
        ));
    }

    #[test]
    fn concurrent_pastes_are_queued_whole_or_refused_whole() {
        const PASTE: usize = 64 * 1024;
        let app = Arc::new(app());
        let (id, fake) = attach_fake(&app, false);
        fake.would_block.store(true, Ordering::SeqCst);
        // 4 x 80 pastes of 64 KiB offer 20 MiB to 16 MiB of room.
        let pasters: Vec<_> = (0..4u8)
            .map(|n| {
                let app = Arc::clone(&app);
                thread::spawn(move || {
                    let text = String::from(char::from(b'a' + n)).repeat(PASTE);
                    (0..80)
                        .filter(|_| match app.paste(id, &text) {
                            Ok(outcome) => outcome == InputOutcome::Consumed,
                            Err(err) => {
                                assert_eq!(
                                    write_error_kind(&Err(err)),
                                    Some(io::ErrorKind::WouldBlock)
                                );
                                false
                            }
                        })
                        .count()
                })
            })
            .collect();
        let accepted: usize = pasters.into_iter().map(|t| t.join().unwrap()).sum();
        assert_eq!(
            accepted * PASTE,
            session::PENDING_INPUT_BYTES,
            "room left over"
        );
        fake.would_block.store(false, Ordering::SeqCst);
        let deadline = Instant::now() + TIMEOUT;
        while lock(&fake.written).len() < accepted * PASTE {
            assert!(Instant::now() < deadline, "the pastes never drained");
            thread::sleep(Duration::from_millis(5));
        }
        let written = lock(&fake.written).clone();
        assert_eq!(written.len(), accepted * PASTE);
        for block in written.chunks(PASTE) {
            assert!(block.iter().all(|&b| b == block[0]), "pastes interleaved");
        }
    }

    #[test]
    fn closing_during_pastes_is_prompt_and_refuses_the_rest() {
        let app = Arc::new(app());
        let (id, _fake) = attach_fake(&app, false);
        let (started_tx, started) = mpsc::channel();
        let paster = {
            let app = Arc::clone(&app);
            thread::spawn(move || {
                let text = "p".repeat(1024 * 1024);
                let mut sent = 0usize;
                loop {
                    match app.paste(id, &text) {
                        Ok(_) => {
                            sent += 1;
                            let _ = started_tx.send(());
                        }
                        Err(Error::SessionNotFound(_)) => return sent,
                        Err(err @ Error::Write { .. }) => {
                            // Full, or refused because the session is closing.
                            assert!(write_error_kind(&Err(err)).is_some());
                            thread::yield_now();
                        }
                        Err(err) => panic!("unexpected error: {err}"),
                    }
                }
            })
        };
        // Close while pastes are still being made.
        started.recv_timeout(TIMEOUT).expect("the first paste");
        let closing = Instant::now();
        app.close_session(id).unwrap();
        assert!(
            closing.elapsed() < Duration::from_secs(2),
            "close took {:?}",
            closing.elapsed()
        );
        assert!(paster.join().unwrap() > 0);
        assert!(matches!(
            app.paste(id, "late"),
            Err(Error::SessionNotFound(_))
        ));
    }

    #[test]
    fn focus_round_trips_and_writes_nothing() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        assert!(!app.is_focused(id).unwrap(), "new sessions are not focused");
        let gained = app.send_input(id, InputEvent::Focus(FocusEvent::Gained));
        assert_eq!(gained.unwrap(), InputOutcome::Consumed);
        assert!(app.is_focused(id).unwrap());
        let lost = app.send_input(id, InputEvent::Focus(FocusEvent::Lost));
        assert_eq!(lost.unwrap(), InputOutcome::Consumed);
        assert!(!app.is_focused(id).unwrap());
        assert_nothing_written(&app, id, &fake);
    }

    /// Feeds `bytes` to session `id` as child output and waits until the
    /// terminal has applied it.
    fn feed_child(
        events: &Receiver<(SessionId, SessionEvent)>,
        fake: &Fake,
        id: SessionId,
        bytes: &[u8],
    ) {
        fake.send(PtyEvent::Output(bytes.to_vec()));
        assert_eq!(next(events), (id, SessionEvent::Output));
    }

    #[test]
    fn keys_are_encoded_with_the_terminal_s_keyboard_modes() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        let up = KeyEvent::new(Key::Up, Modifiers::NONE);
        let escape = KeyEvent::new(Key::Escape, Modifiers::NONE);
        assert_eq!(key(&app, id, up.clone()), InputOutcome::Consumed);
        // DECCKM: application cursor keys.
        feed_child(&events, &fake, id, b"\x1b[?1h");
        assert_eq!(key(&app, id, up.clone()), InputOutcome::Consumed);
        // kitty flags: disambiguate escape codes.
        assert_eq!(key(&app, id, escape.clone()), InputOutcome::Consumed);
        feed_child(&events, &fake, id, b"\x1b[>1u");
        assert_eq!(key(&app, id, escape.clone()), InputOutcome::Consumed);
        // RIS: every keyboard mode back to its default.
        feed_child(&events, &fake, id, b"\x1bc");
        assert_eq!(key(&app, id, up), InputOutcome::Consumed);
        assert_eq!(key(&app, id, escape), InputOutcome::Consumed);
        written_eventually(&fake, b"\x1b[A\x1bOA\x1b\x1b[27u\x1b[A\x1b");
    }

    #[test]
    fn pastes_are_bracketed_under_decset_2004() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        feed_child(&events, &fake, id, b"\x1b[?2004h");
        let pasted = app
            .send_input(id, InputEvent::Paste("a\nb\x1b[201~c".into()))
            .unwrap();
        assert_eq!(pasted, InputOutcome::Consumed);
        let empty = app.paste(id, "\x1b\x07").unwrap();
        assert_eq!(empty, InputOutcome::Ignored, "nothing left: no brackets");
        feed_child(&events, &fake, id, b"\x1b[?2004l");
        assert_eq!(app.paste(id, "d\ne").unwrap(), InputOutcome::Consumed);
        written_eventually(&fake, b"\x1b[200~a\nb[201~c\x1b[201~d\re");
    }

    #[test]
    fn focus_changes_are_reported_under_decset_1004() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        let focus = |event| app.send_input(id, InputEvent::Focus(event)).unwrap();
        assert_eq!(focus(FocusEvent::Gained), InputOutcome::Consumed);
        // Setting the mode reports nothing, not even the current state.
        feed_child(&events, &fake, id, b"\x1b[?1004h");
        assert_eq!(focus(FocusEvent::Lost), InputOutcome::Consumed);
        assert_eq!(focus(FocusEvent::Lost), InputOutcome::Consumed);
        assert_eq!(focus(FocusEvent::Gained), InputOutcome::Consumed);
        assert_eq!(focus(FocusEvent::Gained), InputOutcome::Consumed);
        feed_child(&events, &fake, id, b"\x1b[?1004l");
        assert_eq!(focus(FocusEvent::Lost), InputOutcome::Consumed);
        assert!(!app.is_focused(id).unwrap());
        assert_nothing_written_after(&fake, b"\x1b[O\x1b[I", &app, id);
    }

    #[test]
    fn mouse_is_clamped_to_the_grid_recorded_and_ignored() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        assert_eq!(app.last_mouse(id).unwrap(), None);
        let event = input::MouseEvent {
            kind: MouseEventKind::Press,
            button: MouseButton::Left,
            col: u16::MAX,
            row: 3,
            x_px: -12.5,
            y_px: 1e9,
            modifiers: Modifiers::SHIFT,
        };
        let outcome = app.send_input(id, InputEvent::Mouse(event)).unwrap();
        assert_eq!(outcome, InputOutcome::Ignored);
        let recorded = app.last_mouse(id).unwrap().unwrap();
        assert_eq!(recorded, input::MouseEvent { col: 79, ..event });
        assert_nothing_written(&app, id, &fake);
    }

    #[test]
    fn scroll_clamps_to_the_scrollback() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        // Nothing to scroll into yet.
        assert_eq!(scroll(&app, id, 3.0), InputOutcome::Ignored);
        assert_eq!(app.scroll_offset(id).unwrap(), 0);
        // 24 rows; 30 lines plus the cursor's empty row push 7 into the
        // scrollback.
        fill_scrollback(&fake, &events, 30);
        let max = app
            .with_terminal(id, |term| term.grid().scrollback_len())
            .unwrap();
        assert_eq!(max, 7);
        assert_eq!(scroll(&app, id, 3.0), InputOutcome::Consumed);
        assert_eq!(app.scroll_offset(id).unwrap(), 3);
        assert_eq!(scroll(&app, id, 100.0), InputOutcome::Consumed);
        assert_eq!(app.scroll_offset(id).unwrap(), max);
        assert_eq!(scroll(&app, id, f32::MAX), InputOutcome::Ignored);
        assert_eq!(app.scroll_offset(id).unwrap(), max);
        // The overshoot was not banked: one line back moves at once.
        assert_eq!(scroll(&app, id, -1.0), InputOutcome::Consumed);
        assert_eq!(app.scroll_offset(id).unwrap(), max - 1);
        assert_eq!(scroll(&app, id, -1e30), InputOutcome::Consumed);
        assert_eq!(app.scroll_offset(id).unwrap(), 0);
        assert_eq!(scroll(&app, id, f32::MIN), InputOutcome::Ignored);
        assert_eq!(scroll(&app, id, -0.5), InputOutcome::Ignored);
        assert_eq!(app.scroll_offset(id).unwrap(), 0);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(scroll(&app, id, bad), InputOutcome::Ignored);
        }
        assert_eq!(app.scroll_offset(id).unwrap(), 0);
        assert_nothing_written(&app, id, &fake);
    }

    #[test]
    fn scroll_fractions_accumulate() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        fill_scrollback(&fake, &events, 40);
        // Quarters are exact in binary, so the arithmetic below is too.
        for _ in 0..3 {
            for _ in 0..3 {
                assert_eq!(scroll(&app, id, 0.25), InputOutcome::Ignored);
            }
            assert_eq!(scroll(&app, id, 0.25), InputOutcome::Consumed);
        }
        assert_eq!(app.scroll_offset(id).unwrap(), 3);
        assert_eq!(scroll(&app, id, 0.75), InputOutcome::Ignored);
        // 1.25 lines: one applied, a quarter banked.
        assert_eq!(scroll(&app, id, 0.5), InputOutcome::Consumed);
        assert_eq!(app.scroll_offset(id).unwrap(), 4);
        // The banked quarter counts against a change of direction.
        assert_eq!(scroll(&app, id, -0.5), InputOutcome::Ignored);
        assert_eq!(scroll(&app, id, -0.5), InputOutcome::Ignored);
        assert_eq!(scroll(&app, id, -0.5), InputOutcome::Consumed);
        assert_eq!(app.scroll_offset(id).unwrap(), 3);
    }

    /// ⌘K (#107): the scrollback and the rows above the cursor go, the
    /// cursor's line becomes the top row and the view returns to the live
    /// screen, without an event and without writing to the child.
    #[test]
    fn clear_scrollback_clears_to_the_cursor_line() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        fill_scrollback(&fake, &events, 40);
        fake.send(PtyEvent::Output(b"$ prompt".to_vec()));
        assert_eq!(next(&events), (id, SessionEvent::Output));
        assert_eq!(scroll(&app, id, 5.0), InputOutcome::Consumed);
        app.clear_scrollback(id).unwrap();
        let (scrollback, top, rest_blank, cursor_row) = app
            .with_terminal(id, |term| {
                let grid = term.grid();
                let lines = grid.visible_lines();
                (
                    grid.scrollback_len(),
                    lines[0].text(),
                    lines[1..].iter().all(|line| line.text().is_empty()),
                    grid.cursor().row,
                )
            })
            .unwrap();
        assert_eq!(scrollback, 0);
        assert_eq!(top, "$ prompt");
        assert!(rest_blank);
        assert_eq!(cursor_row, 0);
        assert_eq!(app.scroll_offset(id).unwrap(), 0);
        assert_eq!(
            events.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
        assert_nothing_written(&app, id, &fake);
    }

    #[test]
    fn typing_and_pasting_return_the_view_to_the_bottom() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        fill_scrollback(&fake, &events, 40);
        assert_eq!(scroll(&app, id, 5.0), InputOutcome::Consumed);
        let shift = KeyEvent::new(Key::LeftShift, Modifiers::SHIFT);
        assert_eq!(key(&app, id, shift), InputOutcome::Ignored);
        assert_eq!(
            app.scroll_offset(id).unwrap(),
            5,
            "ignored keys keep the view"
        );
        let a = KeyEvent::new(Key::Char('a'), Modifiers::NONE);
        assert_eq!(key(&app, id, a), InputOutcome::Consumed);
        assert_eq!(app.scroll_offset(id).unwrap(), 0);
        assert_eq!(scroll(&app, id, 5.0), InputOutcome::Consumed);
        let paste = app.send_input(id, InputEvent::Paste("x".into())).unwrap();
        assert_eq!(paste, InputOutcome::Consumed);
        assert_eq!(app.scroll_offset(id).unwrap(), 0);
    }

    #[test]
    fn input_to_unknown_sessions_is_rejected() {
        let app = app();
        let id = SessionId::from_raw(9);
        let events = [
            InputEvent::Key(KeyEvent::new(Key::Enter, Modifiers::NONE)),
            InputEvent::Mouse(input::MouseEvent::default()),
            InputEvent::Scroll(input::ScrollEvent::default()),
            InputEvent::Paste("x".into()),
            InputEvent::Focus(FocusEvent::Gained),
        ];
        for event in events {
            assert!(matches!(
                app.send_input(id, event),
                Err(Error::SessionNotFound(_))
            ));
        }
        assert!(matches!(
            app.scroll_offset(id),
            Err(Error::SessionNotFound(_))
        ));
        assert!(matches!(app.is_focused(id), Err(Error::SessionNotFound(_))));
        assert!(matches!(app.last_mouse(id), Err(Error::SessionNotFound(_))));
    }

    #[test]
    fn input_to_a_closing_session_fails_without_writing() {
        let app = app();
        let (id, fake) = attach_fake(&app, true);
        let session = app.session(id).unwrap();
        session.begin_close();
        let result = app.send_input(
            id,
            InputEvent::Key(KeyEvent::new(Key::Enter, Modifiers::NONE)),
        );
        assert!(matches!(result, Err(Error::Write { .. })), "{result:?}");
        let result = app.send_input(id, InputEvent::Paste("x".into()));
        assert!(matches!(result, Err(Error::Write { .. })), "{result:?}");
        assert!(lock(&fake.written).is_empty());
        fake.exit(ExitStatus::Signaled(1));
    }

    #[test]
    fn view_calls_need_a_live_session_and_a_view() {
        let app = app();
        let (id, _fake) = attach_fake(&app, false);
        let size = PhysicalSize::new(800, 600);
        let no_view = |result: Result<()>| matches!(result, Err(Error::NoView(i)) if i == id);
        assert!(!app.has_view(id).unwrap());
        assert!(no_view(app.resize_view(id, size, 2.0)));
        assert!(no_view(app.set_view_font_size(id, 14.0)));
        assert!(no_view(app.detach_view(id)));
        assert!(matches!(app.render_view(id), Err(Error::NoView(i)) if i == id));
        assert!(matches!(app.view_size(id), Err(Error::NoView(i)) if i == id));
        assert!(matches!(app.view_font_size(id), Err(Error::NoView(_))));
        assert!(matches!(app.view_cell_metrics(id), Err(Error::NoView(_))));
        assert_eq!(app.view_ids(), Vec::<SessionId>::new());

        let gone = SessionId::from_raw(99);
        let not_found =
            |result: Result<()>| matches!(result, Err(Error::SessionNotFound(i)) if i == gone);
        assert!(matches!(app.has_view(gone), Err(Error::SessionNotFound(_))));
        assert!(not_found(app.resize_view(gone, size, 1.0)));
        assert!(not_found(app.set_view_font_size(gone, 14.0)));
        assert!(not_found(app.detach_view(gone)));
        assert!(matches!(
            app.render_view(gone),
            Err(Error::SessionNotFound(_))
        ));

        app.close_session(id).unwrap();
        assert!(matches!(app.has_view(id), Err(Error::SessionNotFound(_))));
        assert_eq!(app.view_ids(), Vec::<SessionId>::new());
    }

    #[test]
    fn the_session_follows_its_view_unless_the_view_is_empty() {
        let app = app();
        let (id, fake) = attach_fake(&app, false);
        let size = PtySize::new(39, 99).with_pixels(800, 600);
        app.follow_view(id, Some(size));
        app.follow_view(id, None);
        assert_eq!(*lock(&fake.resizes), [size]);
        // A session that is gone is logged, not reported.
        app.follow_view(SessionId::from_raw(99), Some(size));
    }

    #[test]
    fn the_view_table_survives_a_panic() {
        let app = app();
        let (id, _fake) = attach_fake(&app, false);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| app.panic_holding_view_lock()));
        assert!(result.is_err());
        assert!(!app.has_view(id).unwrap());
        assert!(matches!(app.detach_view(id), Err(Error::NoView(_))));
    }

    /// Attaches a headless view of `size` at `scale` to session `id`, the
    /// way [`App::attach_view`] attaches a surface, and resizes the session
    /// to its grid; `false` when GPU tests are skipped.
    fn attach_offscreen(app: &App, id: SessionId, size: PhysicalSize, scale: f32) -> bool {
        if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
            return false;
        }
        let theme = app.view_theme();
        let gpu = Gpu::headless().expect("a Metal adapter");
        let fonts = app.fonts_for(&theme).expect("the configured font");
        let view = View::offscreen(gpu, fonts, theme, size, scale).expect("view");
        let pty_size = view.pty_size();
        lock(&app.views).insert(id, view);
        app.follow_view(id, pty_size);
        true
    }

    /// Sends `bytes` as the fake child's output and waits until the reader
    /// has applied it.
    fn output(fake: &Fake, events: &Receiver<(SessionId, SessionEvent)>, bytes: &[u8]) {
        fake.send(PtyEvent::Output(bytes.to_vec()));
        loop {
            if matches!(next(events), (_, SessionEvent::Output)) {
                return;
            }
        }
    }

    #[test]
    fn view_metrics_need_a_live_session_and_a_view() {
        let app = app();
        let (id, _fake) = attach_fake(&app, false);
        assert!(matches!(app.view_metrics(id), Err(Error::NoView(i)) if i == id));
        let gone = SessionId::from_raw(99);
        assert!(matches!(
            app.view_metrics(gone),
            Err(Error::SessionNotFound(i)) if i == gone
        ));
    }

    #[test]
    fn view_metrics_report_the_geometry_the_renderer_uses() {
        let app = app();
        let (id, _fake) = attach_fake(&app, false);
        // 2x, so the padding (4 pt) and the cell are in physical pixels.
        let size = PhysicalSize::new(1601, 1203);
        if !attach_offscreen(&app, id, size, 2.0) {
            return;
        }
        let metrics = app.view_metrics(id).unwrap();
        let cell = app.view_cell_metrics(id).unwrap();
        let view = app.view_size(id).unwrap();
        assert_eq!(metrics.pixels, size);
        assert!((metrics.scale - 2.0).abs() < f32::EPSILON);
        assert_eq!(metrics.cell, cell);
        assert_eq!(metrics.padding_px, 8);
        assert!((metrics.font_size_pt - app.view_font_size(id).unwrap()).abs() < f32::EPSILON);
        assert_eq!((metrics.rows, metrics.cols), (view.rows, view.cols));
        // The grid exactly fills the surface inside the padding.
        let used = |cells: u16, px: u32| metrics.padding_px * 2 + u32::from(cells) * px;
        assert!(used(metrics.cols, cell.width_px) <= size.width);
        assert!(used(metrics.cols + 1, cell.width_px) > size.width);
        assert!(used(metrics.rows, cell.height_px) <= size.height);
        assert!(used(metrics.rows + 1, cell.height_px) > size.height);
    }

    /// The renderer caps its scale at `MAX_SCALE` (8): at a backing scale
    /// of 10 the padding is 4 pt x 8, not x 10, and the cell is the 8x one.
    #[test]
    fn view_metrics_report_the_renderers_padding_above_the_scale_cap() {
        let app = app();
        let (id, _fake) = attach_fake(&app, false);
        let size = PhysicalSize::new(4000, 3000);
        if !attach_offscreen(&app, id, size, 10.0) {
            return;
        }
        let metrics = app.view_metrics(id).unwrap();
        assert!((metrics.scale - 10.0).abs() < f32::EPSILON);
        assert_eq!(metrics.padding_px, 32);
        let view = app.view_size(id).unwrap();
        assert_eq!((metrics.rows, metrics.cols), (view.rows, view.cols));
        let used = |cells: u16, px: u32| metrics.padding_px * 2 + u32::from(cells) * px;
        assert!(used(metrics.cols, metrics.cell.width_px) <= size.width);
        assert!(used(metrics.cols + 1, metrics.cell.width_px) > size.width);
        assert!(used(metrics.rows, metrics.cell.height_px) <= size.height);
        assert!(used(metrics.rows + 1, metrics.cell.height_px) > size.height);
    }

    #[test]
    fn view_metrics_follow_the_font_size_and_a_session_resize() {
        let app = app();
        let (id, _fake) = attach_fake(&app, false);
        if !attach_offscreen(&app, id, PhysicalSize::new(800, 600), 1.0) {
            return;
        }
        app.set_view_font_size(id, 26.0).unwrap();
        let zoomed = app.view_metrics(id).unwrap();
        assert!((zoomed.font_size_pt - 26.0).abs() < f32::EPSILON);
        assert_eq!(zoomed.cell, app.view_cell_metrics(id).unwrap());
        let view = app.view_size(id).unwrap();
        assert_eq!((zoomed.rows, zoomed.cols), (view.rows, view.cols));
        // `rows` and `cols` are the session's grid, which an explicit
        // resize changes until the view's next resize.
        app.resize(id, PtySize::new(10, 20)).unwrap();
        let resized = app.view_metrics(id).unwrap();
        assert_eq!((resized.rows, resized.cols), (10, 20));
        app.resize_view(id, PhysicalSize::new(800, 600), 1.0)
            .unwrap();
        let back = app.view_metrics(id).unwrap();
        assert_eq!((back.rows, back.cols), (view.rows, view.cols));
    }

    #[test]
    fn view_metrics_track_the_cursor_wide_cells_and_the_scrollback() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        if !attach_offscreen(&app, id, PhysicalSize::new(800, 600), 1.0) {
            return;
        }
        let cursor = |app: &App| {
            let m = app.view_metrics(id).unwrap();
            (
                m.cursor_row,
                m.cursor_col,
                m.cursor_on_screen,
                m.cursor_wide,
            )
        };
        assert_eq!(cursor(&app), (0, 0, true, false));
        output(&fake, &events, b"ab\r\n");
        assert_eq!(cursor(&app), (1, 0, true, false));
        output(&fake, &events, b"x");
        assert_eq!(cursor(&app), (1, 1, true, false));
        // On the left half of a wide character, then on its right half.
        output(&fake, &events, "\u{4e2d}\x1b[2D".as_bytes());
        assert_eq!(cursor(&app), (1, 1, true, true));
        output(&fake, &events, b"\x1b[C");
        assert_eq!(cursor(&app), (1, 2, true, false));

        // Scrolled back, the cursor is off the screen the view shows and
        // keeps its live position.
        let rows = app.view_metrics(id).unwrap().rows;
        let lines: Vec<u8> = (0..u32::from(rows) * 2)
            .flat_map(|n| format!("line {n}\r\n").into_bytes())
            .collect();
        output(&fake, &events, &lines);
        let live = cursor(&app);
        assert_eq!(live, (rows - 1, 0, true, false));
        let scroll = InputEvent::Scroll(input::ScrollEvent::new(3.0));
        assert_eq!(app.send_input(id, scroll).unwrap(), InputOutcome::Consumed);
        assert_ne!(app.scroll_offset(id).unwrap(), 0);
        assert_eq!(cursor(&app), (rows - 1, 0, false, false));
        let paste = InputEvent::Paste("x".into());
        assert_eq!(app.send_input(id, paste).unwrap(), InputOutcome::Consumed);
        assert_eq!(app.scroll_offset(id).unwrap(), 0);
        assert_eq!(cursor(&app), live);
    }

    /// `view_metrics` takes the view table and the terminal one after the
    /// other: while it waits for the terminal, the view table is free.
    #[test]
    fn view_metrics_never_hold_the_view_table_while_waiting_for_the_terminal() {
        let app = Arc::new(app());
        let (id, _fake) = attach_fake(&app, false);
        if !attach_offscreen(&app, id, PhysicalSize::new(800, 600), 1.0) {
            return;
        }
        let (entered, wait) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let holder = {
            let app = Arc::clone(&app);
            thread::spawn(move || {
                app.with_terminal(id, |_| {
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                })
                .unwrap();
            })
        };
        wait.recv().unwrap();
        let reader = {
            let app = Arc::clone(&app);
            thread::spawn(move || app.view_metrics(id).unwrap())
        };
        // Give the call time to reach the terminal lock, then probe the
        // view table. A preempted call may hold it for a moment on its way
        // there; one that holds it across the terminal wait fails every
        // probe.
        thread::sleep(Duration::from_millis(50));
        let mut held = 0;
        for _ in 0..40 {
            if app.views.try_lock().is_err() {
                held += 1;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(held < 20, "view table held in {held} of 40 probes");
        assert!(
            !reader.is_finished(),
            "view_metrics did not wait for the terminal"
        );
        release.send(()).unwrap();
        holder.join().unwrap();
        assert_eq!(reader.join().unwrap(), app.view_metrics(id).unwrap());
    }

    /// The terminal is held only for a few field reads, so under a flood of
    /// output a call waits for at most a slice of the reader's work: of 200
    /// calls 1 ms apart, the median stays under 2 ms and the 99th
    /// percentile under 10 ms (in release, unthrottled: median ~40 µs, p99
    /// ~0.3–1.4 ms; under `taskpolicy -b` at load 30: median 0.3–0.7 ms).
    ///
    /// The median carries the bar: a call that held the terminal for 2 ms
    /// or more fails it on every attempt, while a throttled or
    /// preempted test thread (`taskpolicy -b`, a loaded machine) only
    /// stretches the tail, which gets the loose bar. Up to five attempts.
    ///
    /// Timing-based, so like `session::tests::reader_lock_hold_is_bounded`
    /// it runs only in release builds (`cargo nextest run --release -p
    /// tessera-app-core --test-threads 1 view_metrics_are_fast`); unoptimised,
    /// one 4 KiB slice of scrolling output can hold the lock for longer than
    /// the bar. It prints every attempt's 99th percentile.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
    fn view_metrics_are_fast_while_output_floods_the_terminal() {
        let app = app();
        let events = observe(&app);
        let (id, fake) = attach_fake(&app, false);
        if !attach_offscreen(&app, id, PhysicalSize::new(800, 600), 1.0) {
            return;
        }
        let chunk: Vec<u8> = b"y\r\n".repeat(64 * 1024 / 3);
        let median_bar = Duration::from_millis(2);
        let tail_bar = Duration::from_millis(10);
        let mut attempts = Vec::new();
        for _ in 0..5 {
            let stop = Arc::new(AtomicBool::new(false));
            let flood = {
                let stop = Arc::clone(&stop);
                let sender = lock(&fake.events).clone().expect("fake child alive");
                let chunk = chunk.clone();
                thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if sender.send(PtyEvent::Output(chunk.clone())).is_err() {
                            return;
                        }
                    }
                })
            };
            let mut waits = Vec::with_capacity(200);
            for _ in 0..200 {
                let start = Instant::now();
                app.view_metrics(id).unwrap();
                waits.push(start.elapsed());
                thread::sleep(Duration::from_millis(1));
            }
            stop.store(true, Ordering::Relaxed);
            flood.join().unwrap();
            while events.try_recv().is_ok() {}
            waits.sort_unstable();
            let at = |percent: usize| waits[(waits.len() * percent / 100).min(waits.len() - 1)];
            let (median, p90, p99) = (at(50), at(90), at(99));
            eprintln!(
                "view_metrics under a flood: median {median:?}, p90 {p90:?}, p99 {p99:?}, max {:?}",
                waits[waits.len() - 1]
            );
            attempts.push((median, p99));
            if median < median_bar && p99 < tail_bar {
                return;
            }
        }
        panic!(
            "no attempt had a median under {median_bar:?} and a p99 under {tail_bar:?}: {attempts:?}"
        );
    }

    #[test]
    fn config_path_is_config_toml_under_the_root() {
        assert_eq!(
            app().config_path(),
            PathBuf::from("/nonexistent/tessera/config.toml")
        );
    }

    /// Device loss: a view on a lost GPU is detached by its next render,
    /// the next attach brings up a new GPU, and the last detach drops the
    /// cached one. Views are offscreen (`View::offscreen`) and attached
    /// through the same path as [`App::attach_view`]; the device is lost
    /// through [`App::lose_gpu_for_test`] (`wgpu::Device::destroy`).
    mod device_loss {
        use std::time::{Duration, Instant};

        use super::*;
        use crate::view::MIN_FRAME_INTERVAL;

        /// Offscreen views are this large: a few rows of the default font.
        const SIZE: PhysicalSize = PhysicalSize::new(240, 96);

        /// How long one device-loss test may take before [`watchdog`]
        /// fails it.
        const LIMIT: Duration = Duration::from_secs(60);

        /// Runs `f` on its own thread and fails the test if it has not returned
        /// within `limit`: a GPU call blocked inside `wgpu` (a deadlock, a wait on
        /// a lost device) never reaches an in-test deadline, and would hang the
        /// suite. On a timeout it records a stack sample of the whole process
        /// (macOS `sample`) and names the file in the failure.
        fn watchdog(f: impl FnOnce() + Send + 'static) {
            let (tx, rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                f();
                let _ = tx.send(());
            });
            match rx.recv_timeout(LIMIT) {
                Ok(()) => worker.join().expect("finished"),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    std::panic::resume_unwind(worker.join().expect_err("panicked"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let pid = std::process::id();
                    let file = std::env::temp_dir().join(format!("tessera-hang-{pid}.txt"));
                    let sampled = std::process::Command::new("/usr/bin/sample")
                        .args([pid.to_string().as_str(), "2", "-file"])
                        .arg(&file)
                        .output()
                        .is_ok_and(|out| out.status.success());
                    let dump = if sampled {
                        format!("stack sample in {}", file.display())
                    } else {
                        "no stack sample (`sample` failed)".to_owned()
                    };
                    panic!("hung for over {LIMIT:?} inside a GPU call; {dump}");
                }
            }
        }

        /// An app, or `None` when GPU tests are skipped.
        fn gpu_app() -> Option<App> {
            if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
                eprintln!("TESSERA_SKIP_GPU_TESTS is set; skipping");
                return None;
            }
            Some(app())
        }

        /// Attaches an offscreen view to `id` the way `attach_view` attaches
        /// a surface.
        fn attach(app: &App, id: SessionId) -> Result<()> {
            app.attach_with(
                id,
                app.view_instance(),
                (),
                |()| None,
                |gpu, (), fonts, theme| View::offscreen(gpu, fonts, theme, SIZE, 1.0),
            )
        }

        /// The GPU session `id`'s view draws with.
        fn gpu_of(app: &App, id: SessionId) -> Gpu {
            lock(&app.views).get(&id).expect("a view").gpu().clone()
        }

        /// The cached GPU, if one is cached.
        fn cached(app: &App) -> Option<Gpu> {
            lock(&app.gpu).clone()
        }

        /// Whether `a` and `b` are the same device: losing `a` loses `b`.
        /// (`wgpu::Device` equality compares ids, which a new instance
        /// hands out again.) Loses `a`.
        fn same_device(a: &Gpu, b: &Gpu) -> bool {
            assert!(!b.is_lost());
            a.lose_device_for_test();
            b.is_lost()
        }

        fn output(fake: &Fake, events: &Receiver<(SessionId, SessionEvent)>, text: &str) {
            fake.send(PtyEvent::Output(text.as_bytes().to_vec()));
            assert!(matches!(next(events), (_, SessionEvent::Output)));
        }

        /// Renders until a frame is not paced, and returns it.
        fn render(app: &App, id: SessionId) -> Result<FrameStatus> {
            std::thread::sleep(MIN_FRAME_INTERVAL + Duration::from_millis(1));
            app.render_view(id)
        }

        /// Asserts that session `id`'s view shows exactly what a fresh view
        /// of its screen draws (on another device), and that is not blank.
        fn assert_shows_the_whole_screen(app: &App, id: SessionId) {
            let recovered = lock(&app.views).get(&id).expect("view").read_back();

            let fonts = lock(&app.fonts).clone().expect("fonts");
            let gpu = Gpu::headless().expect("a Metal adapter");
            let mut reference =
                View::offscreen(gpu, fonts, app.view_theme(), SIZE, 1.0).expect("view");
            app.with_terminal(id, |term| reference.capture(term))
                .unwrap();
            assert_eq!(
                reference.render(false).expect("reference frame"),
                FrameStatus::Presented
            );
            let expected = reference.read_back();
            let background = expected[..4].to_vec();
            assert!(
                expected.chunks(4).any(|px| px != background.as_slice()),
                "the reference draws the text"
            );
            assert!(
                recovered == expected,
                "the recovered frame differs from a fresh view's"
            );
        }

        fn is_device_lost<T: fmt::Debug>(result: &Result<T>) -> bool {
            matches!(result, Err(Error::Gpu(RenderError::DeviceLost)))
        }

        #[test]
        fn a_lost_device_detaches_the_view_and_the_next_attach_gets_a_new_one() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let (id, _fake) = attach_fake(&app, false);
                attach(&app, id).unwrap();
                assert_eq!(render(&app, id).unwrap(), FrameStatus::Presented);
                let old = gpu_of(&app, id);

                assert!(app.lose_gpu_for_test());
                assert!(old.is_lost());
                let result = render(&app, id);
                assert!(is_device_lost(&result), "{result:?}");
                assert!(!app.has_view(id).unwrap());
                // Once: the host is told to re-attach, not to retry forever.
                assert!(matches!(app.render_view(id), Err(Error::NoView(_))));
                assert!(matches!(app.view_metrics(id), Err(Error::NoView(_))));

                attach(&app, id).unwrap();
                let new = gpu_of(&app, id);
                assert!(!new.is_lost(), "the dead device is not reused");
                assert!(!cached(&app).expect("a GPU").is_lost());
                assert_eq!(render(&app, id).unwrap(), FrameStatus::Presented);
                // And it settles: an idle view does no more work.
                assert_eq!(render(&app, id).unwrap(), FrameStatus::Skipped);
                assert!(
                    same_device(&new, &cached(&app).unwrap()),
                    "the new GPU is cached"
                );
            });
        }

        #[test]
        fn the_last_detach_drops_the_gpu_and_earlier_ones_keep_it() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let (a, _fa) = attach_fake(&app, false);
                let (b, _fb) = attach_fake(&app, false);
                attach(&app, a).unwrap();
                attach(&app, b).unwrap();
                let first = gpu_of(&app, a);

                app.detach_view(a).unwrap();
                let kept = cached(&app).expect("b keeps the GPU");
                assert_eq!(render(&app, b).unwrap(), FrameStatus::Presented);
                app.detach_view(b).unwrap();
                assert!(cached(&app).is_none(), "the last detach drops the GPU");
                assert!(lock(&app.fonts).is_some(), "fonts outlive the GPU");

                attach(&app, a).unwrap();
                let second = gpu_of(&app, a);
                assert!(same_device(&kept, &first), "b kept the GPU a was on");
                assert!(!second.is_lost(), "the next attach brings up a new GPU");
                assert_eq!(render(&app, a).unwrap(), FrameStatus::Presented);
            });
        }

        #[test]
        fn every_view_on_a_lost_device_detaches_even_with_nothing_to_draw() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let (a, _fa) = attach_fake(&app, false);
                let (b, _fb) = attach_fake(&app, false);
                attach(&app, a).unwrap();
                attach(&app, b).unwrap();
                assert_eq!(render(&app, a).unwrap(), FrameStatus::Presented);
                assert_eq!(render(&app, b).unwrap(), FrameStatus::Presented);
                // Idle: the next frames would be skipped without touching the GPU.
                assert_eq!(render(&app, a).unwrap(), FrameStatus::Skipped);

                app.lose_gpu_for_test();
                assert!(is_device_lost(&render(&app, a)));
                // `a` re-attaches before `b` notices: it gets a new GPU ...
                attach(&app, a).unwrap();
                assert!(!gpu_of(&app, a).is_lost());
                // ... and `b`, still on the dead one, is detached by its next
                // render instead of showing its last frame forever.
                assert!(gpu_of(&app, b).is_lost());
                assert!(is_device_lost(&render(&app, b)));
                assert!(cached(&app).is_some(), "b's loss keeps a's GPU");
                attach(&app, b).unwrap();
                assert_eq!(render(&app, a).unwrap(), FrameStatus::Presented);
                assert_eq!(render(&app, b).unwrap(), FrameStatus::Presented);
                assert!(
                    same_device(&gpu_of(&app, a), &gpu_of(&app, b)),
                    "one new GPU for every view"
                );
            });
        }

        #[test]
        fn resizing_and_zooming_a_view_on_a_lost_device_is_harmless() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let (id, _fake) = attach_fake(&app, false);
                attach(&app, id).unwrap();
                assert_eq!(render(&app, id).unwrap(), FrameStatus::Presented);

                app.lose_gpu_for_test();
                app.resize_view(id, PhysicalSize::new(320, 160), 2.0)
                    .unwrap();
                app.set_view_font_size(id, 20.0).unwrap();
                app.resize_view(id, SIZE, 1.0).unwrap();
                assert!(is_device_lost(&render(&app, id)));

                attach(&app, id).unwrap();
                app.resize_view(id, PhysicalSize::new(320, 160), 2.0)
                    .unwrap();
                assert_eq!(render(&app, id).unwrap(), FrameStatus::Presented);
            });
        }

        #[test]
        fn repeated_losses_each_recover_onto_a_new_device() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let (id, _fake) = attach_fake(&app, false);
                for _ in 0..4 {
                    attach(&app, id).unwrap();
                    assert!(!gpu_of(&app, id).is_lost(), "a lost device came back");
                    assert_eq!(render(&app, id).unwrap(), FrameStatus::Presented);
                    assert!(app.lose_gpu_for_test());
                    assert!(is_device_lost(&render(&app, id)));
                    assert!(!app.has_view(id).unwrap());
                }
            });
        }

        #[test]
        fn detaching_during_recovery_leaves_nothing_behind() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let (a, _fa) = attach_fake(&app, false);
                let (b, _fb) = attach_fake(&app, false);
                attach(&app, a).unwrap();
                attach(&app, b).unwrap();

                app.lose_gpu_for_test();
                assert!(is_device_lost(&render(&app, a)));
                // The host closes `b`'s window before rendering it again.
                app.detach_view(b).unwrap();
                assert!(cached(&app).is_none(), "no view left: GPU dropped");
                assert!(matches!(app.render_view(b), Err(Error::NoView(_))));
                assert!(matches!(app.detach_view(a), Err(Error::NoView(_))));
                assert!(!app.lose_gpu_for_test(), "nothing cached to lose");

                attach(&app, b).unwrap();
                assert!(!gpu_of(&app, b).is_lost());
                assert_eq!(render(&app, b).unwrap(), FrameStatus::Presented);
            });
        }

        /// The last removal takes the cached GPU out of its slot and hands
        /// it to the caller, who drops it once no lock is held; earlier
        /// removals leave it cached.
        #[test]
        fn the_last_removal_hands_the_gpu_to_the_caller() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let (a, _fa) = attach_fake(&app, false);
                let (b, _fb) = attach_fake(&app, false);
                attach(&app, a).unwrap();
                attach(&app, b).unwrap();

                let mut views = lock(&app.views);
                let first = app.remove_view(&mut views, a).expect("a's view");
                let last = app.remove_view(&mut views, b).expect("b's view");
                drop(views);
                assert!(first.gpu.is_none(), "b still used the GPU");
                let gpu = last.gpu.as_ref().expect("the cached GPU, not dropped");
                assert!(cached(&app).is_none());
                // Still alive and usable here, with no lock held: the caller
                // decides where the queue's wait for idle happens.
                gpu.capture_errors(|| ()).expect("a live GPU");
                drop(first);
                drop(last);
            });
        }

        /// Replacing a lost cached GPU hands the lost one to the caller
        /// instead of dropping it under the GPU (and view) lock.
        #[test]
        fn replacing_a_lost_gpu_hands_the_old_one_to_the_caller() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let lost = Gpu::headless().expect("a Metal adapter");
                lost.lose_device_for_test();
                *lock(&app.gpu) = Some(lost);

                let (gpu, retired) = app.gpu_for(app.view_instance(), None).unwrap();

                assert!(!gpu.is_lost());
                assert!(retired.expect("the lost GPU").is_lost());
                let (again, retired) = app.gpu_for(app.view_instance(), None).unwrap();
                assert!(retired.is_none(), "a live GPU is reused, not replaced");
                assert!(same_device(&gpu, &again), "the new GPU is cached");
            });
        }

        /// Closing sessions removes their views like a detach: the last one
        /// releases the GPU.
        #[test]
        fn closing_the_last_session_with_a_view_drops_the_gpu() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let (a, _fa) = attach_fake(&app, false);
                let (b, _fb) = attach_fake(&app, false);
                attach(&app, a).unwrap();
                attach(&app, b).unwrap();
                app.close_session(a).unwrap();
                assert!(cached(&app).is_some(), "b still draws");
                assert_eq!(render(&app, b).unwrap(), FrameStatus::Presented);
                app.close_session(b).unwrap();
                assert!(cached(&app).is_none());
            });
        }

        /// The recovered view draws every row, not just the ones damaged
        /// since its last frame on the old device: its frame equals a fresh
        /// view's of the same screen.
        #[test]
        fn a_loss_with_pending_damage_recovers_the_whole_screen() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let events = observe(&app);
                let (id, fake) = attach_fake(&app, false);
                attach(&app, id).unwrap();
                output(&fake, &events, "first row\r\nsecond row\r\n");
                assert_eq!(render(&app, id).unwrap(), FrameStatus::Presented);
                // One more row of damage that the dead device never draws.
                output(&fake, &events, "third");

                app.lose_gpu_for_test();
                assert!(is_device_lost(&render(&app, id)));
                attach(&app, id).unwrap();
                assert_eq!(render(&app, id).unwrap(), FrameStatus::Presented);
                assert_shows_the_whole_screen(&app, id);
            });
        }

        /// Fills `gpu`'s Metal command queue (4096 command buffers, as
        /// `wgpu-hal` allows) with recorded, unsubmitted command buffers, so
        /// the next one recorded fails with a device error and `wgpu` loses
        /// the device right there, as a real loss does. Keep the buffers
        /// alive until then.
        fn fill_command_queue(gpu: &Gpu) -> Vec<wgpu::CommandBuffer> {
            gpu.device()
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("the device goes idle");
            let buffer = gpu.device().create_buffer(&wgpu::BufferDescriptor {
                label: Some("fill"),
                size: 16,
                usage: wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            (1..4096)
                .map(|_| {
                    let mut encoder = gpu
                        .device()
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                    encoder.clear_buffer(&buffer, 0, None);
                    encoder.finish()
                })
                .collect()
        }

        /// The device is lost by a device error inside the frame itself
        /// (not before it, where the render's first check would see it):
        /// that frame already reports `DeviceLost` and detaches.
        #[test]
        fn a_loss_inside_the_frame_detaches_at_once() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let events = observe(&app);
                let (id, fake) = attach_fake(&app, false);
                attach(&app, id).unwrap();
                assert_eq!(render(&app, id).unwrap(), FrameStatus::Presented);
                let gpu = gpu_of(&app, id);
                output(&fake, &events, "damage");
                let held = fill_command_queue(&gpu);
                assert!(!gpu.is_lost(), "one command buffer short of the limit");

                let result = render(&app, id);

                assert!(gpu.is_lost(), "the frame hit the limit");
                assert!(is_device_lost(&result), "{result:?}");
                assert!(!app.has_view(id).unwrap());
                drop(held);
                attach(&app, id).unwrap();
                assert_eq!(render(&app, id).unwrap(), FrameStatus::Presented);
            });
        }

        /// The device is lost while output keeps streaming in and every
        /// frame has damage: the next render reports `DeviceLost` and
        /// detaches, the output keeps arriving with no view (no GPU work),
        /// and the re-attached view draws all of it.
        ///
        /// The loss is injected on the rendering thread: `wgpu-core`
        /// 30.0.1 deadlocks when a lost device's resources are released
        /// (in `Device::maintain`, holding its tracker lock, then taking the
        /// snatch lock) while another thread writes to the queue (snatch
        /// lock, then tracker lock). Every frame is drawn on the main
        /// thread (ADR 0014), and `wgpu` releases them only from a poll or
        /// submit on that thread, so this needs a hook on another thread.
        #[test]
        fn a_loss_during_streaming_output_detaches_and_recovers_it_all() {
            watchdog(|| {
                let Some(app) = gpu_app() else { return };
                let events = observe(&app);
                let (id, fake) = attach_fake(&app, false);
                attach(&app, id).unwrap();
                for frame in 0..5 {
                    output(&fake, &events, &format!("line {frame}\r\n"));
                    assert_eq!(render(&app, id).unwrap(), FrameStatus::Presented);
                }
                output(&fake, &events, "pending\r\n");
                assert!(app.lose_gpu_for_test());
                let start = Instant::now();
                assert!(is_device_lost(&render(&app, id)));
                assert!(
                    start.elapsed() < Duration::from_secs(1),
                    "a lost frame is cheap"
                );
                for frame in 0..50 {
                    output(&fake, &events, &format!("while detached {frame}\r\n"));
                }
                assert!(!app.has_view(id).unwrap());
                attach(&app, id).unwrap();
                assert_eq!(render(&app, id).unwrap(), FrameStatus::Presented);
                assert_shows_the_whole_screen(&app, id);
            });
        }
    }
}
