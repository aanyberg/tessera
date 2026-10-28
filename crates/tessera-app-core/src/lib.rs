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
//! frontend input to the child. Dropping the `App` closes every session
//! and joins both threads (see the [`session`] module for the close
//! sequence). The view methods create, configure and present Metal
//! surfaces and must be called on the main thread (ADR 0014).

#![forbid(unsafe_code)]

pub mod input;
pub mod session;
pub mod view;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Instant;

use tessera_pty::{NativePty, Pty, PtyEvents};
use tessera_render::{FontConfig, Fonts, Gpu, Theme};
use tessera_term::{KeyboardModes, Terminal, encode_key};

pub use input::{InputEvent, InputOutcome};
pub use session::{SessionEvent, SessionId, SessionObserver};
// Re-exported so frontends (through `tessera-ffi`) depend on this crate
// alone, as the layering in `docs/architecture.md` requires.
pub use tessera_config::{Config, ConfigPaths};
pub use tessera_pty::{ExitStatus, Program, PtyError, PtySize, SpawnOptions};

use input::{Bindings, FocusEvent, MouseEvent};
use session::{ObserverSlot, Session, lock};
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

/// Root application object.
pub struct App {
    config: Config,
    paths: ConfigPaths,
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
    next_id: AtomicU64,
    observers: ObserverSlot,
    bindings: Bindings,
    /// At most one view per session, by session id. Never held while
    /// `sessions` is locked.
    views: Mutex<HashMap<SessionId, View>>,
    /// The process's GPU, brought up by the first [`App::attach_view`] and
    /// shared by every view after it.
    gpu: Mutex<Option<Gpu>>,
    /// The font database, loaded by the first [`App::attach_view`] and
    /// shared by every view's shaper (loading it takes a noticeable
    /// fraction of a second).
    fonts: Mutex<Option<Fonts>>,
    /// The theme every view draws with, set by [`App::apply_theme`].
    /// Written only while `views` is held (and read under it by
    /// [`App::attach_view`]), so a view attached during a re-theme never
    /// keeps the old colours.
    theme: Mutex<Theme>,
}

impl App {
    /// Creates an application with the given configuration, read from (and
    /// saved to) the directory `paths` describes. No session is running and
    /// no observer is installed. Views draw with the theme
    /// `config.appearance.theme` names, resolved against `paths` like
    /// [`tessera_config::ConfigStore::theme`] (a name that matches nothing
    /// gives the default theme).
    pub fn new(config: Config, paths: ConfigPaths) -> Self {
        let colours = tessera_config::Theme::resolve(&paths.themes_dir(), &config.appearance.theme);
        for warning in &colours.warnings {
            tracing::warn!(target: "tessera_app_core", "{warning}");
        }
        let theme = Mutex::new(view::theme_for(&config, &colours.theme));
        Self {
            config,
            paths,
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            observers: Arc::new(RwLock::new(None)),
            bindings: Bindings::default(),
            views: Mutex::new(HashMap::new()),
            gpu: Mutex::new(None),
            fonts: Mutex::new(None),
            theme,
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
    /// program cannot be started; [`Error::ReaderThread`] when a session
    /// thread (reader or input writer) cannot be created (the child is
    /// then hung up).
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
        // Detach the view first; its surface is released here, before the
        // child is hung up.
        let view = lock(&self.views).remove(&id);
        drop(view);
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
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::Pty`] if the size has zero rows or columns, more than
    /// [`view::MAX_VIEW_ROWS`] rows or [`view::MAX_VIEW_COLS`] columns
    /// (nothing changes then), or the kernel rejects it.
    pub fn resize(&self, id: SessionId, size: PtySize) -> Result<()> {
        Ok(self.session(id)?.resize(size)?)
    }

    /// Discards session `id`'s scrollback and returns its view to the live
    /// screen.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session.
    pub fn clear_scrollback(&self, id: SessionId) -> Result<()> {
        let session = self.session(id)?;
        // TODO(#2): call `Grid::clear_scrollback` through the terminal once
        // `Terminal` gives mutable access to its grid.
        // TODO(#2, #8): every path that shrinks the scrollback (this one,
        // ED 3, a smaller `max_scrollback`, leaving the alternate screen)
        // must reset or clamp the session's stored scroll offset, as here.
        session.scroll_to_bottom();
        Ok(())
    }

    /// Routes one frontend event to session `id` and reports whether the
    /// core used it.
    ///
    /// * [`InputEvent::Key`]: a key bound to an application command runs it
    ///   (no key is bound yet, #43); any other key is encoded for the child
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
    /// * [`InputEvent::Paste`]: sanitised with [`input::sanitize_paste`]
    ///   and queued like a key, **without blocking**, behind any input
    ///   still pending, and the view returns to the live screen; text that
    ///   sanitises to nothing is [`InputOutcome::Ignored`].
    /// * [`InputEvent::Focus`]: recorded (see [`App::is_focused`]);
    ///   [`InputOutcome::Consumed`].
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session;
    /// [`Error::Write`] if the child has exited, the session is being
    /// closed, or the key or paste does not fit in what is left of
    /// [`session::PENDING_INPUT_BYTES`] (kind
    /// [`io::ErrorKind::WouldBlock`]); nothing of it is queued then.
    pub fn send_input(&self, id: SessionId, event: InputEvent) -> Result<InputOutcome> {
        let session = self.session(id)?;
        let write_error = |source| Error::Write { id, source };
        match event {
            InputEvent::Key(key) => {
                if let Some(binding) = self.bindings.resolve(&key) {
                    match binding {}
                }
                // TODO(#8): encode with the terminal's own keyboard modes
                // (DECCKM, DECKPAM, modifyOtherKeys, kitty flags) once
                // `Terminal` tracks them.
                let bytes = encode_key(&key, &KeyboardModes::default());
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
            InputEvent::Paste(text) => {
                let text = input::sanitize_paste(&text);
                if text.is_empty() {
                    return Ok(InputOutcome::Ignored);
                }
                // TODO(#8): wrap in `CSI 200 ~` ... `CSI 201 ~` when the
                // terminal is in bracketed-paste mode (DECSET 2004).
                let bracketed_paste = false;
                let bytes = if bracketed_paste {
                    [b"\x1b[200~", text.as_bytes(), b"\x1b[201~"].concat()
                } else {
                    text.into_bytes()
                };
                session.queue_input(bytes).map_err(write_error)?;
                session.scroll_to_bottom();
                Ok(InputOutcome::Consumed)
            }
            InputEvent::Focus(focus) => {
                // TODO(#8): report `CSI I` / `CSI O` in focus-event mode
                // (DECSET 1004).
                session.set_focused(focus == FocusEvent::Gained);
                Ok(InputOutcome::Consumed)
            }
        }
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

    /// Makes `surface` session `id`'s view, `size` physical pixels at
    /// backing `scale`, and resizes the session to the grid that fits.
    ///
    /// `surface` must have been created on `instance`, which must come
    /// from [`App::view_instance`]. The first view brings up the process's
    /// GPU on `instance` (an adapter that can present to `surface`; about
    /// 100 ms) and loads the font database; later views share both. The
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
        self.session(id)?;
        let mut views = lock(&self.views);
        if views.contains_key(&id) {
            return Err(Error::ViewAttached(id));
        }
        let gpu = self.gpu_for(instance, &surface)?;
        let theme = lock(&self.theme).clone();
        let fonts = self.fonts_for(&theme)?;
        let view = View::new(gpu, surface, fonts, theme, size, scale)?;
        let pty_size = view.pty_size();
        views.insert(id, view);
        drop(views);
        // A close that raced with this call (only possible when the
        // main-thread rule is broken) must not leave a view behind.
        if self.session(id).is_err() {
            lock(&self.views).remove(&id);
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
    /// ([`RenderError::SurfaceLost`]) also **detaches the view**: the
    /// frontend attaches a new one. After any other GPU error the view
    /// stays attached and the next frame may succeed.
    pub fn render_view(&self, id: SessionId) -> Result<FrameStatus> {
        let session = self.session(id)?;
        let focused = session.is_focused();
        let mut views = lock(&self.views);
        let view = views.get_mut(&id).ok_or(Error::NoView(id))?;
        session.with_terminal(|term| view.capture(term.grid()));
        match view.render(focused) {
            Ok(status) => Ok(status),
            Err(RenderError::SurfaceLost) => {
                let view = views.remove(&id);
                drop(views);
                drop(view);
                Err(Error::Gpu(RenderError::SurfaceLost))
            }
            Err(error) => Err(Error::Gpu(error)),
        }
    }

    /// Re-themes every view, and every view attached later, with `theme`'s
    /// colours (through [`view::theme_for`], so the configured opacity
    /// still scales the background). Fonts are not reloaded: each view
    /// keeps its font family and size.
    ///
    /// Call it with [`tessera_config::ConfigStore::theme`] whenever the
    /// configuration watcher reports
    /// [`tessera_config::ConfigEvent::Changed`]; the new colours show from
    /// each view's next frame. A view's surface keeps the transparency mode
    /// it was attached with. A theme (or opacity) that makes an opaque
    /// background translucent is therefore see-through only in views
    /// attached after it: an existing, opaque view shows the background's
    /// premultiplied colour (each channel times its alpha, so darker) as
    /// an opaque colour. Must be called on the main thread.
    pub fn apply_theme(&self, theme: &tessera_config::Theme) {
        let theme = view::theme_for(&self.config, theme);
        let mut views = lock(&self.views);
        for view in views.values_mut() {
            view.set_theme(theme.clone());
        }
        *lock(&self.theme) = theme;
    }

    /// The renderer theme views draw with now: the last
    /// [`App::apply_theme`], or the configured theme.
    pub fn view_theme(&self) -> Theme {
        lock(&self.theme).clone()
    }

    /// Detaches and releases session `id`'s view. The session keeps its
    /// size. Must be called on the main thread.
    ///
    /// # Errors
    ///
    /// [`Error::SessionNotFound`] if `id` is not a live session,
    /// [`Error::NoView`] if it has no view.
    pub fn detach_view(&self, id: SessionId) -> Result<()> {
        self.session(id)?;
        let view = lock(&self.views).remove(&id).ok_or(Error::NoView(id))?;
        drop(view);
        Ok(())
    }

    /// Runs `f` on session `id`'s view.
    fn with_view<R>(&self, id: SessionId, f: impl FnOnce(&mut View) -> R) -> Result<R> {
        self.session(id)?;
        let mut views = lock(&self.views);
        let view = views.get_mut(&id).ok_or(Error::NoView(id))?;
        Ok(f(view))
    }

    /// The process's GPU, brought up on `instance` for `surface` if this
    /// is the first view.
    fn gpu_for(&self, instance: wgpu::Instance, surface: &wgpu::Surface<'_>) -> Result<Gpu> {
        let mut slot = lock(&self.gpu);
        if let Some(gpu) = slot.as_ref() {
            return Ok(gpu.clone());
        }
        let gpu = Gpu::with_compatible_surface(instance, Some(surface))?;
        *slot = Some(gpu.clone());
        Ok(gpu)
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

impl Drop for App {
    /// Detaches every view, then closes every session and joins its
    /// threads.
    fn drop(&mut self) {
        let views: Vec<_> = lock(&self.views).drain().collect();
        drop(views);
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

    #[test]
    fn config_path_is_config_toml_under_the_root() {
        assert_eq!(
            app().config_path(),
            PathBuf::from("/nonexistent/tessera/config.toml")
        );
    }
}
