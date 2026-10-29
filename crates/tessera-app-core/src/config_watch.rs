//! Live configuration: applying a configuration and a theme to every view,
//! and the one thread per [`App`] that does so when `config.toml` or a
//! theme file changes (ADR 0006, ADR 0019).
//!
//! [`App::start_config_watch`] starts a [`tessera_config::Watcher`] on the
//! store and a thread, `tessera-config-apply`, that consumes its events:
//!
//! - [`ConfigEvent::Changed`]: resolves the theme again
//!   ([`ConfigStore::theme`]) and applies the new configuration with it
//!   ([`App::apply_config`]).
//! - [`ConfigEvent::ThemesChanged`]: resolves the theme again and applies
//!   it only when it differs from the theme in use, so editing a theme file
//!   that is not in use changes nothing on screen.
//! - [`ConfigEvent::Invalid`]: logged; the last good configuration stays.
//!
//! Applying runs on that thread. It changes the views' colours and font
//! size under the views lock (no GPU work), resizes the sessions whose grid
//! changed after releasing it, and leaves the surface's compositing mode to
//! the next frame, which reconfigures the layer on the main thread
//! (`View::set_appearance`). The observer then hears
//! [`SessionObserver::on_appearance_changed`] so the host can make its
//! window translucent or opaque and redraw.
//!
//! [`SessionObserver::on_appearance_changed`]: crate::SessionObserver::on_appearance_changed

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use tessera_config::{
    Config, ConfigEvent, ConfigPaths, ConfigStore, ThemeSource, Warning, WatchGuard, WatchOptions,
};
use tessera_pty::PtySize;
use tessera_render::{Theme, sanitize_font_size};

use crate::session::{ObserverSlot, Session, lock, run_thread_start_hook};
use crate::view::{self, View};
use crate::{App, SessionId};

/// How long dropping the [`App`] waits for the apply thread to finish the
/// change it is applying; past it the thread is detached (and logged). The
/// thread wakes at once when the watch stops, so only an observer call that
/// does not return quickly (against the [`crate::SessionObserver`]
/// contract) can make it wait.
pub const CONFIG_WATCH_STOP_CAP: Duration = Duration::from_secs(1);

/// How often an application started without a configuration directory
/// looks for it (one `stat`), so a directory created later by hand, by
/// Open Config or by the first session's shell integration (#44) is
/// watched without a restart. Slow, since an idle application should not
/// wake up often.
pub const CONFIG_DIR_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The configuration views draw with, the colours it resolved to and the
/// renderer theme built from both, kept together so a change replaces all
/// three at once.
#[derive(Debug, Clone)]
pub(crate) struct Applied {
    /// The configuration in effect ([`App::config`]).
    pub(crate) config: Arc<Config>,
    /// The resolved `appearance.theme`, compared with a newly resolved one
    /// on [`ConfigEvent::ThemesChanged`].
    pub(crate) colours: tessera_config::Theme,
    /// The theme file `colours` came from, `None` for a bundled theme, the
    /// default, or colours handed to [`App::apply_config`] directly.
    pub(crate) source: Option<PathBuf>,
}

/// What looking the theme up again gave.
enum Lookup {
    /// These colours, from this file (`None`: bundled or the default).
    Use(tessera_config::Theme, Option<PathBuf>),
    /// The theme in use is a file that does not load right now (an
    /// editor's half-written save, a typo): keep the colours in use rather
    /// than fall back to another theme until it is fixed.
    KeepBroken,
}

/// The parts of an [`App`] that applying a configuration touches, shared
/// with the apply thread (which cannot borrow the `App`).
#[derive(Clone)]
pub(crate) struct Live {
    pub(crate) sessions: Arc<Mutex<HashMap<SessionId, Arc<Session>>>>,
    pub(crate) views: Arc<Mutex<HashMap<SessionId, View>>>,
    pub(crate) theme: Arc<Mutex<Theme>>,
    pub(crate) applied: Arc<Mutex<Applied>>,
    pub(crate) observers: ObserverSlot,
}

impl Live {
    /// Applies `config` with the colours `colours` to every view and to
    /// the views attached later, and its cursor style to every session;
    /// see [`App::apply_config`]. Returns whether anything the views draw
    /// changed (and so whether the observer was told).
    pub(crate) fn apply(&self, config: &Config, colours: &tessera_config::Theme) -> bool {
        self.apply_from(config, colours, None)
    }

    /// [`Live::apply`] of colours read from the theme file `source`.
    fn apply_from(
        &self,
        config: &Config,
        colours: &tessera_config::Theme,
        source: Option<PathBuf>,
    ) -> bool {
        let theme = view::theme_for(config, colours);
        let opaque = !view::is_translucent(&theme);
        let mut views = lock(&self.views);
        let mut applied = lock(&self.applied);
        let mut current = lock(&self.theme);
        let theme_changed = *current != theme;
        // A configured size that changed resets every view's size, zoomed
        // or not (lead decision D6, as Terminal.app does on a profile
        // change); any other change keeps each view's own size.
        let font_size = sanitize_font_size(config.appearance.font_size);
        let font_changed = sanitize_font_size(applied.config.appearance.font_size).to_bits()
            != font_size.to_bits();
        let cursor = view::cursor_style_for(config);
        let cursor_changed = view::cursor_style_for(&applied.config) != cursor;
        let mut resizes: Vec<SessionId> = Vec::new();
        for (id, view) in views.iter_mut() {
            if theme_changed {
                view.set_appearance(theme.clone());
            }
            if font_changed && view.font_size().to_bits() != font_size.to_bits() {
                view.set_font_size(font_size);
                resizes.push(*id);
            }
        }
        *current = theme;
        *applied = Applied {
            config: Arc::new(config.clone()),
            colours: colours.clone(),
            source,
        };
        drop(current);
        drop(applied);
        // Resizing takes each session's terminal lock and re-wraps its
        // text: never under the views lock (ADR 0014's lock order).
        drop(views);
        for id in resizes {
            // The size is read again now rather than kept from under the
            // lock, so a resize of the view in between (the main thread's
            // `resize_view`) is not undone by a stale size. The window left
            // is the one `resize_view` itself has between its view update
            // and its session resize.
            let size: Option<PtySize> = lock(&self.views).get(&id).and_then(View::pty_size);
            let session = lock(&self.sessions).get(&id).cloned();
            if let (Some(size), Some(session)) = (size, session)
                && let Err(error) = session.resize(size)
            {
                tracing::warn!(session = %id, %error, ?size, "resizing a session to its view failed");
            }
        }
        if cursor_changed {
            // Each terminal's lock, after the views lock is released and
            // with the sessions lock held only to list them (ADR 0014). A
            // session the program gave a style of its own keeps it until it
            // asks for the default (DECSCUSR 0) or resets (ADR 0020). One
            // that starts meanwhile reads the configuration again once
            // listed (`App::attach`).
            let sessions: Vec<Arc<Session>> = lock(&self.sessions).values().cloned().collect();
            for session in sessions {
                session.with_terminal_mut(|term| term.set_default_cursor_style(cursor));
            }
        }
        let changed = theme_changed || font_changed || cursor_changed;
        if changed {
            self.notify(opaque);
        }
        changed
    }

    /// Tells the observer the appearance changed; `opaque` is whether the
    /// background is now fully opaque.
    fn notify(&self, opaque: bool) {
        let observers = self
            .observers
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(observer) = observers.as_ref()
            && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observer.on_appearance_changed(opaque);
            }))
            .is_err()
        {
            tracing::error!("session observer panicked in on_appearance_changed");
        }
    }

    /// The configuration in effect.
    pub(crate) fn config(&self) -> Arc<Config> {
        Arc::clone(&lock(&self.applied).config)
    }

    /// Handles one watcher event.
    fn handle(&self, store: &ConfigStore, event: ConfigEvent) {
        match event {
            ConfigEvent::Changed(changed) => {
                for warning in &changed.warnings {
                    tracing::warn!(target: "tessera_app_core", "{warning}");
                }
                self.apply_changed(store, &changed.config);
            }
            ConfigEvent::ThemesChanged => {
                self.apply_theme_if_changed(store);
            }
            ConfigEvent::Invalid(error) => {
                tracing::warn!(
                    target: "tessera_app_core",
                    %error,
                    "config.toml could not be loaded; keeping the last good configuration"
                );
            }
            other => tracing::debug!(target: "tessera_app_core", ?other, "config event ignored"),
        }
    }

    /// Applies `config` (now the store's) with the theme it names, or with
    /// the colours in use when that theme's file does not load.
    fn apply_changed(&self, store: &ConfigStore, config: &Config) {
        let same_name = self.config().appearance.theme == config.appearance.theme;
        match self.look_up(store, same_name) {
            Lookup::Use(colours, source) => {
                self.apply_from(config, &colours, source);
            }
            Lookup::KeepBroken => {
                let (colours, source) = {
                    let applied = lock(&self.applied);
                    (applied.colours.clone(), applied.source.clone())
                };
                self.apply_from(config, &colours, source);
            }
        }
    }

    /// Brings the views up to date with `store` when a watch starts: its
    /// configuration if it differs from the one in effect (an `App`
    /// created with another one, or a `config.toml` that appeared since),
    /// otherwise the theme, which may have changed on disk since the `App`
    /// resolved it.
    fn reconcile(&self, store: &ConfigStore) {
        let config = store.config();
        if *config == *self.config() {
            self.apply_theme_if_changed(store);
        } else {
            self.apply_changed(store, &config);
        }
    }

    /// Looks the configured theme up again ([`ConfigStore::resolve_theme`]).
    /// A theme file that is there but does not load gives
    /// [`Lookup::KeepBroken`] instead of the theme the lookup falls back
    /// to: one whose stem matches the name (the lookup reports it as
    /// [`Warning::ThemeInvalid`]) unless the lookup still found the very
    /// file the colours in use came from, or, while the name is
    /// `same_name` as the one in effect, the file the colours in use came
    /// from (which may match by its `name` key, so its failure is not
    /// reported).
    fn look_up(&self, store: &ConfigStore, same_name: bool) -> Lookup {
        let resolved = store.resolve_theme();
        let source = match resolved.source {
            ThemeSource::User(path) => Some(path),
            _ => None,
        };
        let previous = lock(&self.applied).source.clone();
        if let Some(warning) = resolved
            .warnings
            .iter()
            .find(|warning| matches!(warning, Warning::ThemeInvalid { .. }))
            && (source.is_none() || source != previous)
        {
            tracing::warn!(target: "tessera_app_core", "{warning}; keeping the colours in use");
            return Lookup::KeepBroken;
        }
        if source.is_none()
            && same_name
            && let Some(previous) = previous
            && previous.is_file()
            && let Err(error) = tessera_config::Theme::load(&previous)
        {
            tracing::warn!(
                target: "tessera_app_core",
                path = %previous.display(),
                %error,
                "the theme file in use does not load; keeping its colours"
            );
            return Lookup::KeepBroken;
        }
        Lookup::Use(resolved.theme, source)
    }

    /// Looks the theme up again and applies it if it differs from the
    /// colours in use (lead decision D3). Returns `false` when the theme
    /// file in use does not load (the colours in use are kept).
    fn apply_theme_if_changed(&self, store: &ConfigStore) -> bool {
        let Lookup::Use(colours, source) = self.look_up(store, true) else {
            return false;
        };
        let (config, same) = {
            let applied = lock(&self.applied);
            (Arc::clone(&applied.config), applied.colours == colours)
        };
        if same {
            lock(&self.applied).source = source;
        } else {
            self.apply_from(&config, &colours, source);
        }
        true
    }
}

/// The running watch: the watcher's guard and the apply thread.
#[derive(Debug)]
pub(crate) struct ConfigWatch {
    /// The watcher's guard, once it runs, and whether the watch was stopped;
    /// shared with the apply thread, which installs the guard itself when
    /// it had to wait for the configuration directory.
    slot: Arc<Mutex<Slot>>,
    /// Dropped to wake a thread waiting for the configuration directory.
    wake: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    /// Never receives; disconnects when the apply thread's body returns.
    done: Receiver<()>,
}

#[derive(Debug, Default)]
struct Slot {
    guard: Option<WatchGuard>,
    /// Set by `Drop`: the thread handles no further event.
    stopped: bool,
}

impl ConfigWatch {
    /// Watches `store` with `options` and starts the apply thread.
    fn start(live: Live, store: ConfigStore, options: WatchOptions) -> std::io::Result<Self> {
        let (events, guard) = store
            .watch(with_hook(options))
            .map_err(|error| std::io::Error::other(error.to_string()))?
            .into_parts();
        let slot = Arc::new(Mutex::new(Slot {
            guard: Some(guard),
            stopped: false,
        }));
        let (wake, _) = mpsc::channel();
        Self::spawn(slot, wake, move |_| Some((store, events)), live)
    }

    /// Waits for the configuration directory at `paths` (one `stat` every
    /// [`CONFIG_DIR_POLL_INTERVAL`]), then watches it and applies what it
    /// holds, on one thread.
    fn wait_for(live: Live, paths: ConfigPaths, options: WatchOptions) -> std::io::Result<Self> {
        let slot = Arc::new(Mutex::new(Slot::default()));
        let (wake, woken) = mpsc::channel::<()>();
        Self::spawn(
            slot,
            wake,
            move |slot| {
                while matches!(
                    woken.recv_timeout(CONFIG_DIR_POLL_INTERVAL),
                    Err(RecvTimeoutError::Timeout)
                ) {
                    if !paths.root().is_dir() {
                        continue;
                    }
                    let store = ConfigStore::in_memory(paths, Config::default());
                    if let Err(error) = store.reload() {
                        tracing::warn!(target: "tessera_app_core", %error, "config.toml could not be loaded");
                    }
                    let (events, guard) = match store.watch(with_hook(options)) {
                        Ok(watcher) => watcher.into_parts(),
                        Err(error) => {
                            tracing::warn!(target: "tessera_app_core", %error, "cannot watch the configuration");
                            return None;
                        }
                    };
                    let mut slot_guard = lock(slot);
                    if slot_guard.stopped {
                        return None;
                    }
                    slot_guard.guard = Some(guard);
                    drop(slot_guard);
                    tracing::info!(target: "tessera_app_core", "configuration directory appeared; watching it");
                    return Some((store, events));
                }
                None
            },
            live,
        )
    }

    /// Starts the apply thread: `begin` returns the store and events to
    /// work on (waiting first if it has to), or `None` to end.
    fn spawn(
        slot: Arc<Mutex<Slot>>,
        wake: mpsc::Sender<()>,
        begin: impl FnOnce(&Mutex<Slot>) -> Option<(ConfigStore, Receiver<ConfigEvent>)>
        + Send
        + 'static,
        live: Live,
    ) -> std::io::Result<Self> {
        let (done_tx, done) = mpsc::channel::<()>();
        let thread_slot = Arc::clone(&slot);
        let thread = std::thread::Builder::new()
            .name("tessera-config-apply".into())
            .spawn(move || {
                let _done = done_tx;
                run_thread_start_hook();
                let Some((store, events)) = begin(&thread_slot) else {
                    return;
                };
                // Edits made before the watcher's first sample, and a
                // configuration other than the one the `App` started with.
                live.reconcile(&store);
                // Ends when the guard is dropped: the watcher thread exits
                // and its channel disconnects. Events still queued when the
                // watch is stopped (or replaced) are dropped, so an older
                // change never lands after the newer watch's.
                for event in &events {
                    if lock(&thread_slot).stopped {
                        return;
                    }
                    live.handle(&store, event);
                }
            })?;
        Ok(Self {
            slot,
            wake: Some(wake),
            thread: Some(thread),
            done,
        })
    }

    /// Whether the watcher runs (not still waiting for the directory).
    fn is_active(&self) -> bool {
        lock(&self.slot).guard.is_some()
    }
}

/// `options` with the core's thread start hook for the watcher's thread.
fn with_hook(mut options: WatchOptions) -> WatchOptions {
    options.on_thread_start.get_or_insert(run_thread_start_hook);
    options
}

impl Drop for ConfigWatch {
    /// Stops the watcher (which closes the apply thread's channel) or the
    /// wait for the directory, and joins the apply thread, waiting at most
    /// [`CONFIG_WATCH_STOP_CAP`]. Dropped on the apply thread itself (an
    /// observer call that stops or replaces the watch), it does not wait
    /// for itself: the thread ends once that call returns.
    fn drop(&mut self) {
        let guard = {
            let mut slot = lock(&self.slot);
            slot.stopped = true;
            slot.guard.take()
        };
        drop(guard);
        drop(self.wake.take());
        let Some(thread) = self.thread.take() else {
            return;
        };
        if thread.thread().id() == std::thread::current().id() {
            return;
        }
        match self.done.recv_timeout(CONFIG_WATCH_STOP_CAP) {
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if thread.join().is_err() {
                    tracing::error!("the config apply thread panicked");
                }
            }
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::error!(
                    "the config apply thread did not stop within {CONFIG_WATCH_STOP_CAP:?}; detaching it"
                );
            }
        }
    }
}

impl App {
    /// An application started with the configuration in `paths` and, when
    /// that directory exists, watching it ([`App::start_config_watch`] with
    /// `options`).
    ///
    /// Starting creates nothing on disk: `config.toml` is read if it is
    /// there, and a missing or invalid file gives the defaults (logged; an
    /// invalid one is picked up once repaired). A configuration directory
    /// that does not exist yet is looked for every
    /// [`CONFIG_DIR_POLL_INTERVAL`] (one `stat`) and watched, its
    /// configuration applied, once it appears: created by hand, by
    /// [`App::open_config`] (which also starts the watch at once) or by the
    /// first session's shell integration. When its parent directory does
    /// not exist either, nothing is waited for (only `open_config` would
    /// create it, and starts the watch).
    pub fn with_config_dir(paths: ConfigPaths, options: WatchOptions) -> Self {
        let store = ConfigStore::in_memory(paths, Config::default());
        let paths = store.paths().clone();
        if let Err(error) = store.reload() {
            tracing::warn!(
                target: "tessera_app_core",
                %error,
                path = %paths.config_file().display(),
                "config.toml could not be loaded; using the defaults until it is fixed"
            );
        }
        let app = Self::new((*store.config()).clone(), paths.clone());
        let parent_exists = paths.root().parent().is_none_or(Path::is_dir);
        let started = if paths.root().is_dir() {
            app.start_config_watch(store, options)
        } else if !parent_exists {
            // `~/Library/Application Support` always exists; a missing
            // parent is a `TESSERA_CONFIG_DIR` that names nothing (the
            // app's test runs do so on purpose). Nothing will create the
            // directory but `open_config`, which starts the watch itself,
            // so no thread waits for it.
            tracing::info!(
                target: "tessera_app_core",
                path = %paths.root().display(),
                "no configuration directory and no parent directory; using the defaults"
            );
            Ok(())
        } else {
            tracing::info!(
                target: "tessera_app_core",
                path = %paths.root().display(),
                "no configuration directory; using the defaults until it appears"
            );
            ConfigWatch::wait_for(app.live(), paths, options)
                .map(|watch| *lock(&app.config_watch) = Some(watch))
        };
        if let Err(error) = started {
            tracing::warn!(
                target: "tessera_app_core",
                %error,
                "cannot watch the configuration; edits apply after a restart"
            );
        }
        app
    }

    /// Makes sure the configuration can be edited: creates the
    /// configuration directory (with `profiles/` and `themes/`) and a
    /// commented default `config.toml` where they are missing
    /// ([`ConfigStore::open`]), and starts watching it unless a watch runs
    /// already. Returns the path of `config.toml`. For a frontend about to
    /// open the file for editing (Preferences, ⌘,); idempotent.
    ///
    /// # Errors
    ///
    /// When the directory or the default file cannot be created, or the
    /// watch cannot be started; what was created stays.
    pub fn open_config(
        &self,
        options: WatchOptions,
    ) -> Result<std::path::PathBuf, tessera_config::Error> {
        let store = ConfigStore::open(self.paths.clone())?;
        if !self.is_watching_config() {
            self.start_config_watch(store, options)?;
        }
        Ok(self.paths.config_file())
    }

    /// Watches `store` (its `config.toml` and `themes/`) and applies every
    /// change to this application's views with [`App::apply_config`], on a
    /// thread of its own, until the `App` is dropped. One watch per `App`:
    /// a second call replaces the first.
    ///
    /// `store`'s configuration is applied when the watch starts if it
    /// differs from the one in effect, and so are edits made since `store`
    /// was opened. An invalid `config.toml` is logged and the last good
    /// configuration stays applied.
    ///
    /// The observer's [`SessionObserver::on_appearance_changed`] is called
    /// on the watch's thread after each change that alters what the views
    /// draw.
    ///
    /// # Errors
    ///
    /// When the watcher or its thread cannot be started (no kernel queue,
    /// no thread); the `App` keeps running with its configuration.
    ///
    /// [`SessionObserver::on_appearance_changed`]: crate::SessionObserver::on_appearance_changed
    pub fn start_config_watch(
        &self,
        store: ConfigStore,
        options: WatchOptions,
    ) -> std::io::Result<()> {
        // Stop the previous watch first, so two never apply at once, and
        // outside the lock: its thread may be in an observer call that
        // stops the watch itself (and so takes the lock).
        let previous = lock(&self.config_watch).take();
        drop(previous);
        let watch = ConfigWatch::start(self.live(), store, options)?;
        let previous = lock(&self.config_watch).replace(watch);
        drop(previous);
        Ok(())
    }

    /// Stops the configuration watch, if one runs, waiting for a change
    /// being applied to finish (at most [`CONFIG_WATCH_STOP_CAP`]).
    pub fn stop_config_watch(&self) {
        let watch = lock(&self.config_watch).take();
        drop(watch);
    }

    /// Whether a configuration watch is running (not merely waiting for
    /// the configuration directory to appear).
    pub fn is_watching_config(&self) -> bool {
        lock(&self.config_watch)
            .as_ref()
            .is_some_and(ConfigWatch::is_active)
    }

    /// The shared state the apply thread works on.
    pub(crate) fn live(&self) -> Live {
        Live {
            sessions: Arc::clone(&self.sessions),
            views: Arc::clone(&self.views),
            theme: Arc::clone(&self.theme),
            applied: Arc::clone(&self.applied),
            observers: Arc::clone(&self.observers),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::SyncSender;

    use tessera_pty::{ExitStatus, Pty, PtyError, PtyEvent};
    use tessera_render::{Fonts, Gpu, PhysicalSize, Rgba};

    use tessera_grid::DamageRows;

    use super::*;
    use crate::view::Snapshot;
    use crate::{ConfigPaths, SessionEvent, SessionObserver};

    /// A PTY that records its sizes and whether the views lock was free
    /// while it was resized.
    struct FakePty {
        resizes: Arc<Mutex<Vec<PtySize>>>,
        views: Arc<Mutex<HashMap<SessionId, View>>>,
        views_locked_during_resize: Arc<AtomicBool>,
        events: Option<SyncSender<PtyEvent>>,
    }

    impl Pty for FakePty {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn try_write(&mut self, _bytes: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn resize(&mut self, size: PtySize) -> Result<(), PtyError> {
            if self.views.try_lock().is_err() {
                self.views_locked_during_resize
                    .store(true, Ordering::SeqCst);
            }
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
            if let Some(events) = self.events.take() {
                let _ = events.send(PtyEvent::Exited(ExitStatus::Signaled(1)));
            }
            Ok(())
        }
        fn kill(&mut self) -> io::Result<()> {
            self.hangup()
        }
    }

    /// Records every `on_appearance_changed`.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<bool>>>);

    impl SessionObserver for Recorder {
        fn on_session_event(&self, _id: SessionId, _event: SessionEvent) {}
        fn on_appearance_changed(&self, opaque: bool) {
            lock(&self.0).push(opaque);
        }
    }

    impl Recorder {
        fn take(&self) -> Vec<bool> {
            std::mem::take(&mut *lock(&self.0))
        }
    }

    struct Session1 {
        id: SessionId,
        resizes: Arc<Mutex<Vec<PtySize>>>,
        views_locked_during_resize: Arc<AtomicBool>,
    }

    fn attach_fake(app: &App) -> Session1 {
        let (tx, rx) = mpsc::sync_channel(4);
        let resizes = Arc::new(Mutex::new(Vec::new()));
        let flag = Arc::new(AtomicBool::new(false));
        let pty = FakePty {
            resizes: Arc::clone(&resizes),
            views: Arc::clone(&app.views),
            views_locked_during_resize: Arc::clone(&flag),
            events: Some(tx),
        };
        let id = app.allocate_id();
        app.attach(id, Box::new(pty), rx, PtySize::default())
            .expect("attach fake session");
        Session1 {
            id,
            resizes,
            views_locked_during_resize: flag,
        }
    }

    /// A headless view drawing `theme` at 1x into a 200 x 100 texture, or
    /// `None` when GPU tests are skipped.
    fn offscreen(theme: Theme) -> Option<View> {
        if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
            eprintln!("TESSERA_SKIP_GPU_TESTS is set; skipping");
            return None;
        }
        let gpu = Gpu::headless().expect("a Metal adapter");
        let fonts = Fonts::new(&tessera_render::FontConfig {
            family: theme.font_family.clone(),
            size_pt: theme.font_size,
            scale: 1.0,
            fallback: Vec::new(),
        })
        .expect("fonts");
        Some(View::offscreen(gpu, fonts, theme, PhysicalSize::new(200, 100), 1.0).expect("view"))
    }

    fn app() -> App {
        App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"))
    }

    fn config(theme: &str, font_size: f32, opacity: f32) -> Config {
        let mut config = Config::default();
        config.appearance.theme = theme.into();
        config.appearance.font_size = font_size;
        config.appearance.opacity = opacity;
        config
    }

    fn bundled(name: &str) -> tessera_config::Theme {
        tessera_config::Theme::bundled(name).unwrap().clone()
    }

    fn view_theme_of(app: &App, id: SessionId) -> Theme {
        lock(&app.views).get(&id).unwrap().theme().clone()
    }

    fn font_size_of(app: &App, id: SessionId) -> f32 {
        lock(&app.views).get(&id).unwrap().font_size()
    }

    #[test]
    fn apply_config_uses_the_new_configuration_and_tells_the_observer() {
        let app = app();
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));

        // No views: the App-level theme and configuration still change.
        let dracula = bundled("Dracula");
        app.apply_config(&config("Dracula", 13.0, 0.5), &dracula);
        assert_eq!(app.config().appearance.theme, "Dracula");
        let theme = app.view_theme();
        assert_eq!(
            theme.background.with_alpha(1.0),
            Rgba::from_srgb8(0x28, 0x2a, 0x36)
        );
        assert!(
            (theme.background.a - 0.5).abs() < 1e-6,
            "opacity from the new config"
        );
        assert_eq!(recorder.take(), [false], "translucent now");

        // The same again changes nothing and tells nobody.
        app.apply_config(&config("Dracula", 13.0, 0.5), &dracula);
        assert_eq!(recorder.take(), Vec::<bool>::new());

        // `apply_theme` keeps the configuration in effect (opacity 0.5).
        app.apply_theme(&bundled("Nord"));
        assert!((app.view_theme().background.a - 0.5).abs() < 1e-6);
        assert_eq!(app.config().appearance.theme, "Dracula");
        assert_eq!(recorder.take(), [false]);

        app.apply_config(&config("Nord", 13.0, 1.0), &bundled("Nord"));
        assert_eq!(recorder.take(), [true], "opaque again");

        // A change the views do not draw (the theme *name*, same colours)
        // is applied but not reported.
        app.apply_config(&config("nord", 13.0, 1.0), &bundled("Nord"));
        assert_eq!(app.config().appearance.theme, "nord");
        assert_eq!(recorder.take(), Vec::<bool>::new());
    }

    #[test]
    fn a_font_size_change_resizes_outside_the_views_lock_and_resets_zoom() {
        let app = app();
        let session = attach_fake(&app);
        let Some(view) = offscreen(app.view_theme()) else {
            return;
        };
        lock(&app.views).insert(session.id, view);
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));
        let rows_at_13 = app.view_size(session.id).unwrap().rows;

        app.apply_config(&config("Tessera Dark", 26.0, 1.0), &bundled("Tessera Dark"));
        assert_eq!(font_size_of(&app, session.id), 26.0);
        let resizes = lock(&session.resizes).clone();
        let last = *resizes.last().expect("the session follows its view");
        let size = app.view_size(session.id).unwrap();
        assert!(size.rows < rows_at_13, "bigger cells, fewer rows");
        assert_eq!((last.rows, last.cols), (size.rows, size.cols));
        assert!(
            !session.views_locked_during_resize.load(Ordering::SeqCst),
            "the PTY is resized after the views lock is released"
        );
        assert_eq!(recorder.take(), [true]);

        // A zoom (⌘+) survives a change that leaves the configured size
        // alone, such as a theme change...
        app.set_view_font_size(session.id, 40.0).unwrap();
        app.apply_config(&config("Dracula", 26.0, 1.0), &bundled("Dracula"));
        assert_eq!(font_size_of(&app, session.id), 40.0);
        assert_eq!(
            view_theme_of(&app, session.id).background,
            Rgba::from_srgb8(0x28, 0x2a, 0x36)
        );
        // ... but a new configured size replaces it (lead decision D6).
        let before = lock(&session.resizes).len();
        app.apply_config(&config("Dracula", 15.0, 1.0), &bundled("Dracula"));
        assert_eq!(font_size_of(&app, session.id), 15.0);
        assert!(lock(&session.resizes).len() > before);
        assert!(!session.views_locked_during_resize.load(Ordering::SeqCst));
        app.close_session(session.id).unwrap();
    }

    /// The #41 adversarial repro: Dracula's `#282a36` at alpha `0x80`
    /// applied live to a view attached opaque reads back the same as a view
    /// attached translucent from the start. (An offscreen texture has no
    /// compositing mode; the surface switch itself is tested with a real
    /// layer in `tessera-ffi/tests/surface.rs`.)
    #[test]
    fn a_live_translucent_retheme_draws_like_a_translucent_start() {
        let app = app();
        let session = attach_fake(&app);
        let Some(view) = offscreen(app.view_theme()) else {
            return;
        };
        lock(&app.views).insert(session.id, view);
        let mut translucent = bundled("Dracula");
        translucent.background = "#282a3680".parse().unwrap();
        let config = config("Dracula", 13.0, 1.0);
        app.apply_config(&config, &translucent);

        let Some(mut fresh) = offscreen(view::theme_for(&config, &translucent)) else {
            return;
        };
        let term = tessera_term::Terminal::new(4, 10);
        let mut views = lock(&app.views);
        let live = views.get_mut(&session.id).unwrap();
        for view in [&mut *live, &mut fresh] {
            view.capture(&term);
            assert!(view.render(true).is_ok());
        }
        let (live, fresh) = (live.read_back(), fresh.read_back());
        assert_eq!(live[..4], fresh[..4], "the corner pixel");
        assert!(live == fresh, "every pixel");
        // Premultiplied: each channel times alpha (0x80 / 0xff), as the
        // #41 review read back on an opaque surface.
        assert!(live[3].abs_diff(0x80) <= 1, "alpha {:#x}", live[3]);
        drop(views);
        app.close_session(session.id).unwrap();
    }

    /// Calls back into the `App` from `on_appearance_changed`: the views
    /// (`view_ids`, `view_size`, `view_metrics`), sessions, configuration
    /// and the watch itself. No lock is held
    /// across the call but the observer slot's read lock (which only
    /// `set_observer` waits for, as for session events).
    struct Reentrant {
        app: std::sync::Weak<App>,
        session: SessionId,
        /// Also stop the watch from inside the call (it runs on the watch's
        /// own thread).
        stop_watch: bool,
        /// What the calls returned, and how long stopping the watch took.
        done: std::sync::mpsc::Sender<(String, Duration)>,
    }

    impl SessionObserver for Reentrant {
        fn on_session_event(&self, _id: SessionId, _event: SessionEvent) {}
        fn on_appearance_changed(&self, _opaque: bool) {
            let Some(app) = self.app.upgrade() else {
                return;
            };
            let ids = app.view_ids();
            let size = app.view_size(self.session).map(|_| ()).err();
            let metrics = app.view_metrics(self.session).map(|_| ()).err();
            let theme = app.view_theme();
            let config = app.config();
            // Re-entrant apply of what is in effect: changes nothing, so
            // it does not call back again.
            let colours = lock(&app.applied).colours.clone();
            app.apply_config(&config, &colours);
            let closed = app.close_session(self.session);
            let stopping = std::time::Instant::now();
            if self.stop_watch {
                app.stop_config_watch();
            }
            let stop_took = stopping.elapsed();
            let _ = self.done.send((
                format!(
                    "{ids:?} {size:?} {metrics:?} {:?} {closed:?}",
                    theme.background
                ),
                stop_took,
            ));
        }
    }

    /// Runs `f` on a helper thread and fails (instead of hanging) if it
    /// does not finish within five seconds.
    fn within_bound(what: &str, f: impl FnOnce() + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            f();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "{what}: deadlocked"
        );
    }

    #[test]
    fn an_observer_may_call_back_into_the_app() {
        // From `apply_config` on the caller's thread.
        let app = Arc::new(app());
        let session = attach_fake(&app);
        let (done, calls) = mpsc::channel();
        app.set_observer(Some(Box::new(Reentrant {
            app: Arc::downgrade(&app),
            session: session.id,
            stop_watch: false,
            done,
        })));
        let applying = Arc::clone(&app);
        within_bound("apply_config", move || {
            applying.apply_config(&config("Dracula", 13.0, 1.0), &bundled("Dracula"));
        });
        let (call, _) = calls.recv_timeout(Duration::from_secs(5)).expect("called");
        assert!(call.ends_with("Ok(())"), "{call}");
        assert!(app.session_ids().is_empty(), "closed from the callback");
        assert!(calls.try_recv().is_err(), "called once");

        // From the watch's thread, which the callback also stops.
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        let app = Arc::new(App::with_config_dir(paths.clone(), WatchOptions::default()));
        assert!(app.is_watching_config());
        let session = attach_fake(&app);
        let (done, calls) = mpsc::channel();
        app.set_observer(Some(Box::new(Reentrant {
            app: Arc::downgrade(&app),
            session: session.id,
            stop_watch: true,
            done,
        })));
        std::fs::write(paths.config_file(), "[appearance]\ntheme = \"Nord\"\n").unwrap();
        let (call, stop_took) = calls
            .recv_timeout(Duration::from_secs(5))
            .expect("called from the watch");
        assert!(call.ends_with("Ok(())"), "{call}");
        assert!(
            stop_took < Duration::from_millis(500),
            "the watch's thread does not wait for itself: {stop_took:?}"
        );
        assert!(!app.is_watching_config(), "stopped from its own thread");
        assert_eq!(app.config().appearance.theme, "Nord");
        let started = std::time::Instant::now();
        drop(app);
        assert!(
            started.elapsed() < CONFIG_WATCH_STOP_CAP,
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn starting_creates_nothing_and_open_config_creates_and_watches() {
        let base = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(base.path().join("Tessera"));
        let app = App::with_config_dir(paths.clone(), WatchOptions::default());
        assert!(!paths.root().exists());
        assert!(!app.is_watching_config());
        assert_eq!(
            app.open_config(WatchOptions::default()).unwrap(),
            paths.config_file()
        );
        assert!(paths.config_file().is_file());
        assert!(app.is_watching_config());
    }

    fn theme_doc(name: &str, background: &str) -> String {
        let mut theme = bundled("Dracula");
        theme.name = name.into();
        theme.background = background.parse().unwrap();
        theme.to_toml()
    }

    /// Waits up to `bound` for `done`, polling every 20 ms.
    fn eventually(bound: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + bound;
        while std::time::Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        done()
    }

    fn beam(blink: bool) -> tessera_term::CursorStyle {
        tessera_term::CursorStyle {
            shape: tessera_term::CursorShape::Beam,
            blink,
        }
    }

    fn cursor_config(shape: tessera_config::CursorShape, blink: bool) -> Config {
        let mut config = Config::default();
        config.appearance.cursor_shape = shape;
        config.appearance.cursor_blink = blink;
        config
    }

    fn cursor_style_of(app: &App, id: SessionId) -> tessera_term::CursorStyle {
        app.with_terminal(id, tessera_term::Terminal::cursor_style)
            .expect("live session")
    }

    /// A6: a new session starts with the configured cursor style; a
    /// changed default reaches the sessions that follow it (also one back
    /// on the default with DECSCUSR 0) and not one whose program set a
    /// style; an unchanged configuration damages nothing and tells nobody.
    #[test]
    fn apply_config_changes_the_default_cursor_of_live_sessions() {
        use tessera_config::CursorShape;

        let app = App::new(
            cursor_config(CursorShape::Beam, false),
            ConfigPaths::new("/nonexistent/tessera"),
        );
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));
        let [plain, reset, styled] = [(); 3].map(|()| attach_fake(&app).id);
        for id in [plain, reset, styled] {
            assert_eq!(cursor_style_of(&app, id), beam(false), "before any output");
        }
        let feed = |id, bytes: &[u8]| {
            app.session(id)
                .unwrap()
                .with_terminal_mut(|term| term.advance(bytes));
        };
        feed(reset, b"\x1b[3 q\x1b[0 q");
        feed(styled, b"\x1b[5 q");
        let mut snapshots: Vec<Snapshot> = [plain, reset, styled]
            .iter()
            .map(|&id| {
                let mut snapshot = Snapshot::default();
                app.with_terminal(id, |term| snapshot.capture(term))
                    .unwrap();
                snapshot
            })
            .collect();
        let damage = |snapshots: &mut [Snapshot]| -> Vec<DamageRows> {
            [plain, reset, styled]
                .iter()
                .zip(snapshots.iter_mut())
                .map(|(&id, snapshot)| {
                    app.with_terminal(id, |term| snapshot.capture(term))
                        .unwrap()
                })
                .collect()
        };

        let underline = cursor_config(CursorShape::Underline, true);
        app.apply_config(&underline, &tessera_config::Theme::default());
        let expected = tessera_term::CursorStyle {
            shape: tessera_term::CursorShape::Underline,
            blink: true,
        };
        assert_eq!(cursor_style_of(&app, plain), expected);
        assert_eq!(cursor_style_of(&app, reset), expected, "Ps 0 follows");
        assert_eq!(cursor_style_of(&app, styled), beam(true), "the program's");
        assert_eq!(recorder.take(), [true], "the views redraw");
        assert_eq!(
            damage(&mut snapshots),
            [
                DamageRows::Rows(0..=0),
                DamageRows::Rows(0..=0),
                DamageRows::None
            ]
        );

        // The same configuration again: nothing changes, nobody is told.
        app.apply_config(&underline, &tessera_config::Theme::default());
        assert_eq!(recorder.take(), Vec::<bool>::new());
        assert_eq!(damage(&mut snapshots), vec![DamageRows::None; 3]);

        // A session started now gets the new default.
        let fresh = attach_fake(&app).id;
        assert_eq!(cursor_style_of(&app, fresh), expected);
    }

    /// C5: an edit of `config.toml` that changes only the cursor keys is
    /// applied by the watch to the running sessions.
    #[test]
    fn a_reload_of_only_the_cursor_keys_reaches_the_sessions() {
        let base = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(base.path().join("Tessera"));
        std::fs::create_dir_all(paths.root()).unwrap();
        std::fs::write(paths.config_file(), "[appearance]\nfont_size = 14.0\n").unwrap();
        let app = App::with_config_dir(paths.clone(), WatchOptions::default());
        assert!(app.is_watching_config());
        let id = attach_fake(&app).id;
        assert_eq!(
            cursor_style_of(&app, id),
            tessera_term::CursorStyle::default()
        );
        std::fs::write(
            paths.config_file(),
            "[appearance]\nfont_size = 14.0\ncursor_shape = \"beam\"\ncursor_blink = true\n",
        )
        .unwrap();
        assert!(
            eventually(Duration::from_secs(5), || cursor_style_of(&app, id)
                == beam(true)),
            "{:?}",
            cursor_style_of(&app, id)
        );
        assert_eq!(app.config().appearance.font_size, 14.0);
    }

    /// A configuration directory made by hand after launch is picked up:
    /// by `open_config` at once (its `config.toml` applied, not just
    /// watched), and without it within the directory poll.
    #[test]
    fn a_configuration_directory_made_after_launch_applies() {
        for via_open_config in [true, false] {
            let base = tempfile::tempdir().unwrap();
            let paths = ConfigPaths::new(base.path().join("Tessera"));
            let app = App::with_config_dir(paths.clone(), WatchOptions::default());
            assert!(!app.is_watching_config());
            std::fs::create_dir_all(paths.root()).unwrap();
            std::fs::write(
                paths.config_file(),
                "[appearance]\ntheme = \"Nord\"\nfont_size = 20.0\nopacity = 0.5\n",
            )
            .unwrap();
            if via_open_config {
                app.open_config(WatchOptions::default()).unwrap();
            }
            let bound = if via_open_config {
                Duration::from_secs(2)
            } else {
                CONFIG_DIR_POLL_INTERVAL + Duration::from_secs(3)
            };
            assert!(
                eventually(bound, || {
                    (app.config().appearance.font_size - 20.0).abs() < f32::EPSILON
                }),
                "open_config: {via_open_config}: {:?}",
                app.config().appearance
            );
            assert!(app.is_watching_config());
            let theme = app.view_theme();
            assert_eq!(
                theme.background.with_alpha(1.0),
                Rgba::from_srgb8(0x2e, 0x34, 0x40)
            );
            assert!((theme.background.a - 0.5).abs() < 1e-6);
            assert_eq!(
                std::fs::read_to_string(paths.config_file()).unwrap(),
                "[appearance]\ntheme = \"Nord\"\nfont_size = 20.0\nopacity = 0.5\n",
                "never replaced by the default file"
            );
        }
    }

    /// An unrelated broken file whose stem matches the name does not block
    /// edits of the theme file in use (found by its `name` key).
    #[test]
    fn a_broken_stem_match_does_not_block_the_theme_in_use() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(root.path());
        std::fs::create_dir_all(paths.themes_dir()).unwrap();
        let in_use = paths.themes_dir().join("aaa.toml");
        std::fs::write(&in_use, theme_doc("My Theme", "#101010")).unwrap();
        std::fs::write(paths.config_file(), "[appearance]\ntheme = \"My Theme\"\n").unwrap();
        let app = App::with_config_dir(paths.clone(), WatchOptions::default());
        assert_eq!(
            app.view_theme().background,
            Rgba::from_srgb8(0x10, 0x10, 0x10)
        );
        std::fs::write(paths.themes_dir().join("my-theme.toml"), "garbage = ").unwrap();
        std::thread::sleep(Duration::from_millis(400));
        std::fs::write(&in_use, theme_doc("My Theme", "#202020")).unwrap();
        assert!(
            eventually(Duration::from_secs(3), || app.view_theme().background
                == Rgba::from_srgb8(0x20, 0x20, 0x20)),
            "{:?}",
            app.view_theme().background.to_srgb8()
        );
    }

    /// Stops the watch from inside the observer once released.
    struct StopFromObserver {
        app: std::sync::Weak<App>,
        entered: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl SessionObserver for StopFromObserver {
        fn on_session_event(&self, _id: SessionId, _event: SessionEvent) {}
        fn on_appearance_changed(&self, _opaque: bool) {
            if let Some(entered) = lock(&self.entered).take() {
                let _ = entered.send(());
                let _ = lock(&self.release).recv_timeout(Duration::from_secs(3));
                if let Some(app) = self.app.upgrade() {
                    app.stop_config_watch();
                }
            }
        }
    }

    /// The main thread restarting the watch while the apply thread's
    /// observer stops it neither deadlocks nor waits out the stop cap: the
    /// old watch is joined outside the lock.
    #[test]
    fn restarting_the_watch_while_an_observer_stops_it_is_prompt() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(root.path());
        std::fs::create_dir_all(paths.root()).unwrap();
        let app = Arc::new(App::with_config_dir(paths.clone(), WatchOptions::default()));
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        app.set_observer(Some(Box::new(StopFromObserver {
            app: Arc::downgrade(&app),
            entered: Mutex::new(Some(entered)),
            release: Mutex::new(release_rx),
        })));
        std::fs::write(paths.config_file(), "[appearance]\ntheme = \"Nord\"\n").unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("observer entered");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            release.send(()).unwrap();
        });
        let started = std::time::Instant::now();
        let store = ConfigStore::open(paths).unwrap();
        app.start_config_watch(store, WatchOptions::default())
            .unwrap();
        let took = started.elapsed();
        releaser.join().unwrap();
        assert!(
            took < Duration::from_millis(700),
            "start_config_watch took {took:?}"
        );
        assert!(app.is_watching_config());
    }

    /// Events still queued when the watch is stopped are dropped, so an
    /// older change never lands after the watch was stopped (or replaced).
    #[test]
    fn events_queued_before_a_stop_are_not_applied() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(root.path());
        std::fs::create_dir_all(paths.root()).unwrap();
        let app = Arc::new(App::with_config_dir(paths.clone(), WatchOptions::default()));
        let (entered, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        app.set_observer(Some(Box::new(StopFromObserver {
            app: Arc::downgrade(&app),
            entered: Mutex::new(Some(entered)),
            release: Mutex::new(release_rx),
        })));
        std::fs::write(paths.config_file(), "[appearance]\ntheme = \"Nord\"\n").unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("observer entered");
        // Queued while the apply thread is inside the observer.
        std::fs::write(paths.config_file(), "[appearance]\ntheme = \"Dracula\"\n").unwrap();
        std::thread::sleep(Duration::from_millis(600));
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!app.is_watching_config());
        assert_eq!(
            app.config().appearance.theme,
            "Nord",
            "the queued change was dropped"
        );
    }

    /// An application still waiting for its configuration directory stops
    /// at once: the wait is woken, not left to its next poll (or the stop
    /// cap).
    /// No thread waits for a directory whose parent is missing too; Open
    /// Config still creates it and starts the watch.
    #[test]
    fn a_directory_without_a_parent_is_not_waited_for() {
        let base = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(base.path().join("absent").join("Tessera"));
        let app = App::with_config_dir(paths.clone(), WatchOptions::default());
        assert!(lock(&app.config_watch).is_none(), "no waiting thread");
        app.open_config(WatchOptions::default()).unwrap();
        assert!(app.is_watching_config());
        assert!(paths.config_file().is_file());
    }

    #[test]
    fn an_app_waiting_for_its_directory_stops_promptly() {
        let base = tempfile::tempdir().unwrap();
        for drop_it in [false, true] {
            let paths = ConfigPaths::new(base.path().join("Missing"));
            let app = App::with_config_dir(paths, WatchOptions::default());
            assert!(lock(&app.config_watch).is_some(), "waiting");
            let started = std::time::Instant::now();
            if drop_it {
                drop(app);
            } else {
                app.stop_config_watch();
                assert!(lock(&app.config_watch).is_none());
            }
            let took = started.elapsed();
            assert!(
                took < Duration::from_millis(100),
                "drop: {drop_it}: took {took:?}"
            );
        }
    }

    #[test]
    fn dropping_the_app_stops_the_watch_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(ConfigPaths::new(dir.path())).unwrap();
        let app = App::new((*store.config()).clone(), store.paths().clone());
        app.start_config_watch(store.clone(), WatchOptions::default())
            .unwrap();
        assert!(app.is_watching_config());
        // A second start replaces the first.
        app.start_config_watch(store, WatchOptions::default())
            .unwrap();
        let started = std::time::Instant::now();
        drop(app);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "took {:?}",
            started.elapsed()
        );
    }
}
