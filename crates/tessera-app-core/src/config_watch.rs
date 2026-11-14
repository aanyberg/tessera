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
//! - [`ConfigEvent::Invalid`]: the last good configuration stays (the
//!   store logged the error, once).
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
use tessera_render::Theme;

use crate::input::Bindings;
use crate::session::{ObserverSlot, Session, lock, run_thread_start_hook};
use crate::view::{self, DisplayOptions, Look, View};
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

/// How many times a [`ConfigEvent::ThemesChanged`] looks the theme up when
/// the store's configuration keeps changing under the lookup (each lookup
/// reads `themes/` once, at most 256 files).
const THEME_LOOKUP_ATTEMPTS: u32 = 3;

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
    /// Counts the configurations applied, and the display options set: a
    /// session keeps the look of the latest one it was given
    /// ([`Session::set_look`]).
    pub(crate) generation: u64,
    /// The host's display options ([`App::set_display_options`]), applied
    /// to every look.
    pub(crate) display: DisplayOptions,
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
    pub(crate) bindings: Arc<Bindings>,
    /// Where a profile's theme is looked up (`themes/`).
    pub(crate) themes_dir: Arc<Path>,
}

/// Themes a profile names, resolved once per configuration applied:
/// every session of one profile shares them, and a profile not in use is
/// never looked up (each lookup reads `themes/`, at most 256 files).
pub(crate) struct Looks<'a> {
    themes_dir: &'a Path,
    config: &'a Config,
    colours: &'a tessera_config::Theme,
    display: DisplayOptions,
    resolved: HashMap<String, Option<tessera_config::Theme>>,
}

impl<'a> Looks<'a> {
    /// Looks for `config`, whose `[appearance]` theme resolved to
    /// `colours`, drawn under `display`; other themes are looked up in
    /// `themes_dir`.
    pub(crate) fn new(
        themes_dir: &'a Path,
        config: &'a Config,
        colours: &'a tessera_config::Theme,
        display: DisplayOptions,
    ) -> Self {
        Self {
            themes_dir,
            config,
            colours,
            display,
            resolved: HashMap::new(),
        }
    }

    /// The look of a session of `profile` (`None`: the built-in profile,
    /// `[appearance]` alone). A profile no longer in the configuration
    /// falls back as [`Config::profile_appearance`] does. A profile theme
    /// that is the `[appearance]` one uses its colours; another one is
    /// looked up, and when its file is there but does not load, a session
    /// already drawn with that theme (`previous`) keeps its colours, as
    /// `[appearance]`'s do ([`Lookup::KeepBroken`]).
    pub(crate) fn of(&mut self, profile: Option<&str>, previous: Option<&Look>) -> Look {
        let appearance = match profile {
            None => self.config.appearance.clone(),
            Some(_) => self.config.profile_appearance(profile),
        };
        self.with(appearance, previous)
    }

    /// The look of a new session that names no profile: the one
    /// `[general] default_profile` names, else the built-in one.
    pub(crate) fn default_look(&mut self) -> Look {
        let appearance = self.config.profile_appearance(None);
        self.with(appearance, None)
    }

    fn with(&mut self, appearance: tessera_config::Appearance, previous: Option<&Look>) -> Look {
        if appearance.theme == self.config.appearance.theme {
            return Look::new(appearance, self.colours.clone(), self.display);
        }
        let themes_dir = self.themes_dir;
        let resolved = self
            .resolved
            .entry(appearance.theme.clone())
            .or_insert_with(|| {
                let resolved = tessera_config::Theme::resolve(themes_dir, &appearance.theme);
                for warning in &resolved.warnings {
                    tracing::warn!(target: "tessera_app_core", "profile theme: {warning}");
                }
                let broken = resolved
                    .warnings
                    .iter()
                    .any(|warning| matches!(warning, Warning::ThemeInvalid { .. }));
                (!broken).then_some(resolved.theme)
            });
        let colours = match (resolved, previous) {
            (Some(colours), _) => colours.clone(),
            (None, Some(previous)) if previous.appearance.theme == appearance.theme => {
                previous.colours.clone()
            }
            // A broken file and nothing to keep: what the lookup fell back
            // to (`Theme::resolve` warned).
            (None, _) => tessera_config::Theme::resolve(themes_dir, &appearance.theme).theme,
        };
        Look::new(appearance, colours, self.display)
    }
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
        let changed = self.restyle(config, colours, source);
        self.config_applied();
        changed
    }

    /// Records `config` and `colours` as in effect and gives every session
    /// the look of its profile under them (#39): the views their theme and,
    /// where the session's configured size changed, its font size, the
    /// terminals their default cursor style and theme colours. Tells the
    /// observer when anything drawn changed; returns whether it did.
    ///
    /// Each session's look is resolved outside every lock (a profile with
    /// a theme of its own reads `themes/`), set on the session before the
    /// views are updated under the views lock: a view attached meanwhile
    /// reads its session's look under that lock, so it either gets the new
    /// one or is updated here. Under that lock each view takes its
    /// session's look as it is then, not the one this apply resolved, so
    /// of two applies racing the older one cannot put its look back.
    fn restyle(
        &self,
        config: &Config,
        colours: &tessera_config::Theme,
        source: Option<PathBuf>,
    ) -> bool {
        let config = Arc::new(config.clone());
        let (generation, display, radius_changed) = {
            let mut applied = lock(&self.applied);
            let generation = applied.generation + 1;
            let display = applied.display;
            let radius_changed =
                applied.config.appearance.blur_radius != config.appearance.blur_radius;
            *applied = Applied {
                config: Arc::clone(&config),
                colours: colours.clone(),
                source,
                generation,
                display,
            };
            (generation, display, radius_changed)
        };
        self.restyle_at(&config, colours, generation, display, radius_changed)
    }

    /// The part of [`Live::restyle`] after the configuration is recorded:
    /// gives every session its look under `config`, `colours` and
    /// `display` at `generation`, updates the views and tells the
    /// observer, also when nothing a session draws changed but
    /// `window_changed` (the window's blur radius, [`App::blur_radius`]).
    fn restyle_at(
        &self,
        config: &Config,
        colours: &tessera_config::Theme,
        generation: u64,
        display: DisplayOptions,
        window_changed: bool,
    ) -> bool {
        // Listed after the configuration is recorded: a session that starts
        // meanwhile and is not listed reads it once listed (`App::attach`).
        let sessions: Vec<(SessionId, Arc<Session>)> = lock(&self.sessions)
            .iter()
            .map(|(id, session)| (*id, Arc::clone(session)))
            .collect();
        let mut looks = Looks::new(&self.themes_dir, config, colours, display);
        let default_look = looks.default_look();
        let mut restyled: HashMap<SessionId, (Arc<Look>, &Arc<Session>)> = HashMap::new();
        let mut cursor_changed = false;
        // Blur is the host's to draw, not the views': a change of it alone
        // (`blur` toggled at an unchanged translucent opacity) still tells
        // the host, which reads it per session (`App::session_appearance`).
        // So is the background colour under the title bar and tab bar
        // (#388), also of a session with no view, whose look changes
        // nothing a view draws.
        let mut host_changed = false;
        for (id, session) in &sessions {
            let previous = session.look();
            let look = Arc::new(looks.of(session.profile(), Some(&previous)));
            let Some(old) = session.set_look(generation, Arc::clone(&look)) else {
                continue; // a later configuration got there first
            };
            let cursor = old.cursor() != look.cursor();
            cursor_changed |= cursor;
            host_changed |= old.blur != look.blur || old.theme.background != look.theme.background;
            if cursor || old.colours != look.colours {
                // Each terminal's lock, with no other lock held (ADR 0014).
                // A session the program gave a shape of its own keeps it
                // until it asks for the default (DECSCUSR 0) or resets; a
                // blink of its own goes when the configured blink changes,
                // which the terminal decides against the default it holds
                // (ADR 0020, #169). The theme's colours become what the
                // program's colours (OSC 4, 10, 11, 12) fall back to; the
                // program's stay (D-THEME-KEEP). The look is read again
                // under the terminal lock (`Session::push_look`), so of two
                // applies racing here the newer one's stays (#168).
                session.push_look();
            }
            restyled.insert(*id, (old, session));
        }
        let mut views = lock(&self.views);
        let mut current = lock(&self.theme);
        let mut theme_changed = *current != default_look.theme;
        let mut font_changed = false;
        let mut resizes: Vec<SessionId> = Vec::new();
        for (id, view) in views.iter_mut() {
            let Some((old, session)) = restyled.get(id) else {
                continue;
            };
            // A later apply may have set a newer look meanwhile (views,
            // then the session's look: the lock order).
            let look = session.look();
            if *view.base_theme() != look.theme {
                view.set_appearance(look.theme.clone());
                theme_changed = true;
            }
            // A configured size that changed resets the view's size, zoomed
            // or not (lead decision D6, as Terminal.app does on a profile
            // change); any other change keeps its own size. Per session, so
            // editing `[appearance] font_size` leaves the views of a profile
            // with a size of its own alone.
            let size = look.font_size();
            if old.font_size().to_bits() != size.to_bits() {
                font_changed = true;
                if view.font_size().to_bits() != size.to_bits() {
                    view.set_font_size(size);
                    resizes.push(*id);
                }
            }
        }
        *current = default_look.theme.clone();
        // Opacity is per tab window (ADR 0043): the host keeps a window
        // opaque only while every view in it is, so a translucent view is
        // never drawn over an opaque window's backing. This aggregate is
        // what the host reports across all views; with no view, what a
        // new one would be.
        let opaque = if views.is_empty() {
            !view::is_translucent(&default_look.theme)
        } else {
            views
                .values()
                .all(|view| !view::is_translucent(view.base_theme()))
        };
        drop(current);
        // Resizing takes each session's terminal lock and re-wraps its
        // text: never under the views lock (ADR 0014's lock order).
        drop(views);
        for id in resizes {
            // The size is read again now rather than kept from under the
            // lock, so a resize of the view in between (the main thread's
            // `resize_view`) is not undone by a stale size. The window left
            // is the one `resize_view` itself has between its view update
            // and its session resize.
            let (size, cell): (Option<PtySize>, _) = match lock(&self.views).get(&id) {
                Some(view) => (view.pty_size(), Some(view.cell_metrics())),
                None => (None, None),
            };
            let session = lock(&self.sessions).get(&id).cloned();
            if let (Some(cell), Some(session)) = (cell, &session) {
                session.set_cell_pixels(cell.width_px, cell.height_px);
            }
            if let (Some(size), Some(session)) = (size, session)
                && let Err(error) = session.resize(size)
            {
                tracing::warn!(session = %id, %error, ?size, "resizing a session to its view failed");
            }
        }
        let changed =
            theme_changed || font_changed || cursor_changed || host_changed || window_changed;
        if changed {
            self.notify(opaque);
        }
        changed
    }

    /// Gives `session`, just listed, the look of the configuration in
    /// effect if one was applied after it resolved its own, and its
    /// terminal that look's defaults (`App::attach`).
    pub(crate) fn catch_up(&self, session: &Session) {
        let (config, colours, generation, display) = {
            let applied = lock(&self.applied);
            (
                Arc::clone(&applied.config),
                applied.colours.clone(),
                applied.generation,
                applied.display,
            )
        };
        if session.look_generation() < generation {
            let previous = session.look();
            let look = Looks::new(&self.themes_dir, &config, &colours, display)
                .of(session.profile(), Some(&previous));
            session.set_look(generation, Arc::new(look));
        }
        session.push_look();
    }

    /// Records the host's display options and, when they changed, gives
    /// every session its look under them, with the configuration and the
    /// colours in effect ([`App::set_display_options`]). Returns whether
    /// anything drawn changed (and so whether the observer was told).
    ///
    /// The options, the configuration they apply with and the generation
    /// are taken under one hold of the `applied` lock, so a configuration
    /// the watch applies meanwhile is never replaced by the older one, and
    /// whichever restyle comes second carries both.
    pub(crate) fn set_display(&self, display: DisplayOptions) -> bool {
        let (config, colours, generation) = {
            let mut applied = lock(&self.applied);
            if applied.display == display {
                return false;
            }
            applied.display = display;
            applied.generation += 1;
            (
                Arc::clone(&applied.config),
                applied.colours.clone(),
                applied.generation,
            )
        };
        self.restyle_at(&config, &colours, generation, display, false)
    }

    /// Compiles the key map of the configuration in effect when its
    /// `[keys]` changed, counts the configuration applied, and tells the
    /// observer
    /// ([`SessionObserver::on_config_applied`](crate::SessionObserver::on_config_applied)).
    ///
    /// The key map is compiled from the configuration recorded now, under
    /// the lock that records it, not from the one this apply recorded: of
    /// two racing applies the one that records its configuration last
    /// also has its key map published last, so [`App::config`] and
    /// [`App::keymap`] never stay from different applies (#348 M1). The
    /// generation counts under that lock too, so
    /// [`Live::config_and_generation`] reads a matching pair.
    fn config_applied(&self) {
        let generation = {
            let applied = lock(&self.applied);
            self.bindings.applied(&applied.config)
        };
        let observers = self
            .observers
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(observer) = observers.as_ref()
            && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observer.on_config_applied(generation);
            }))
            .is_err()
        {
            tracing::error!("session observer panicked in on_config_applied");
        }
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
        let config = Arc::clone(&lock(&self.applied).config);
        #[cfg(any(test, feature = "test-hooks"))]
        crate::input::read_pause::between_reads();
        config
    }

    /// [`Live::config`] and the configuration generation, read under the
    /// lock [`Live::config_applied`] counts the generation under: the
    /// generation is that of the last apply whose key map was published,
    /// never one newer than the configuration (#348).
    pub(crate) fn config_and_generation(&self) -> (Arc<Config>, u64) {
        let applied = lock(&self.applied);
        let config = Arc::clone(&applied.config);
        #[cfg(any(test, feature = "test-hooks"))]
        crate::input::read_pause::between_reads();
        (config, self.bindings.generation())
    }

    /// Handles one watcher event.
    fn handle(&self, store: &ConfigStore, event: ConfigEvent) {
        match event {
            ConfigEvent::Changed(changed) => {
                // The store logged the warnings it had not logged before
                // when it applied the file; logging all of them here again
                // would repeat each on every save (#324 item 9).
                // The store's configuration, not the event's: an older
                // `Changed` still queued behind a `ThemesChanged` that
                // applied a newer one must not bring the older back (#283).
                drop(changed);
                self.apply_changed(store, &store.config());
            }
            ConfigEvent::ThemesChanged => {
                self.apply_theme_if_changed(store);
            }
            ConfigEvent::Invalid(error) => {
                // The store logged it as a warning when it recorded it.
                tracing::debug!(
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
            // S8: the path is a `themes/` entry name, and an I/O error
            // (too large, not a regular file) quotes it.
            tracing::warn!(
                target: "tessera_app_core",
                path = %tessera_config::escaped(
                    &previous.display().to_string(),
                    tessera_config::MAX_WARNING_PATH_LEN
                ),
                error = %tessera_config::escaped(
                    &error.to_string(),
                    tessera_config::MAX_PARSE_ERROR_LEN
                ),
                "the theme file in use does not load; keeping its colours"
            );
            return Lookup::KeepBroken;
        }
        Lookup::Use(resolved.theme, source)
    }

    /// Looks the theme up again and applies it if it differs from the
    /// colours in use (lead decision D3). Returns `false` when the theme
    /// file in use does not load (the colours in use are kept).
    ///
    /// The theme is applied with the store's configuration, the one the
    /// lookup resolved the name of, not the one in effect: when the store
    /// reloaded `config.toml` before this event was handled (its `Changed`
    /// still queued), the newly named theme is applied at the new opacity
    /// at once, not at the old one first (#253 item 4). A configuration
    /// that changes during the lookup is looked up again, at most
    /// [`THEME_LOOKUP_ATTEMPTS`] times; past that the last lookup is
    /// applied with the configuration read just before it, and the queued
    /// `Changed` corrects it.
    fn apply_theme_if_changed(&self, store: &ConfigStore) -> bool {
        let mut attempts = 0;
        let (config, lookup) = loop {
            attempts += 1;
            let config = store.config();
            let lookup = self.look_up(store, true);
            if attempts == THEME_LOOKUP_ATTEMPTS || Arc::ptr_eq(&config, &store.config()) {
                break (config, lookup);
            }
        };
        let Lookup::Use(colours, source) = lookup else {
            return false;
        };
        let same = {
            let applied = lock(&self.applied);
            applied.colours == colours && *applied.config == *config
        };
        if same {
            // The `[appearance]` theme is unchanged, but a profile's may
            // not be: its sessions are restyled, without announcing a new
            // configuration.
            self.restyle(&config, &colours, source);
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
    /// While waiting for the directory, the store the wait watches once it
    /// appears when that store's load failed (it refuses clipboard writes):
    /// [`App::open_config`] watches it too rather than a fresh store, so
    /// the refusal carries over (#393 F1).
    refusing: Option<ConfigStore>,
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
        Self::spawn(slot, wake, move |_| Some((store, events)), live, None)
    }

    /// Waits for the configuration directory of `store` (one `stat` every
    /// [`CONFIG_DIR_POLL_INTERVAL`]), then watches it and applies what it
    /// holds, on one thread.
    ///
    /// A `store` whose load failed (a dangling symlink at or above the
    /// directory, one that cannot be searched) is watched itself, so its
    /// refusal of clipboard writes carries over: an empty `config.toml` in
    /// the appeared directory keeps them off (#382 m2), and so it does when
    /// `App::open_config` runs before the next poll sees the directory,
    /// since that watches the same store (#393 F1). Otherwise a fresh store
    /// is, as if the application started now: a missing directory is a
    /// successful load that a later broken file must not inherit.
    ///
    /// Xcode's Thread Performance Checker reports a priority inversion
    /// here: the `tessera-config-apply` thread, raised by the thread start
    /// hook, parks in `woken.recv_timeout` (a `dispatch_semaphore_wait`),
    /// which the Checker charges to the default-QoS thread that will drop
    /// the `ConfigWatch`, the only one that ends the wait early. Accepted
    /// (lead decision D7-1, #417): the wait is a timed sleep between two
    /// `stat`s, nothing waits on its result, and avoiding the park needs
    /// tick polling (idle wake-ups) or a stop latency above
    /// [`CONFIG_WATCH_STOP_CAP`]. See `docs/debugging.md`.
    fn wait_for(live: Live, store: ConfigStore, options: WatchOptions) -> std::io::Result<Self> {
        let paths = store.paths().clone();
        let refusing = store.last_error().is_some().then(|| store.clone());
        let failed = refusing.is_some();
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
                    let store = if failed {
                        store
                    } else {
                        ConfigStore::in_memory(paths, Config::default())
                    };
                    // A file that does not load is logged by the store, once
                    // (#262 item 6); the defaults stay, writes refused.
                    let _ = store.reload();
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
            refusing,
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
        refusing: Option<ConfigStore>,
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
            refusing,
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
    /// there. A missing file gives the defaults. A file that does not load
    /// (invalid TOML or values, unreadable, a symlink loop or a dangling
    /// symlink, or missing because the configuration directory or one
    /// above it is a dangling symlink) gives the defaults with clipboard writes (OSC 52) refused,
    /// logged once, until a version of it loads; an empty file does not
    /// turn them back on (ADR 0006). A configuration directory
    /// that does not exist yet is looked for every
    /// [`CONFIG_DIR_POLL_INTERVAL`] (one `stat`) and watched, its
    /// configuration applied, once it appears: created by hand, by
    /// [`App::open_config`] (which also starts the watch at once) or by the
    /// first session's shell integration; so is one whose load failed
    /// (behind a dangling symlink, at the directory or above it, or below
    /// a directory that cannot be searched), until the directory appears,
    /// with clipboard writes refused meanwhile. When its parent directory
    /// does not exist either and the load did not fail, nothing is waited
    /// for (only `open_config` would create it, and starts the watch).
    pub fn with_config_dir(paths: ConfigPaths, options: WatchOptions) -> Self {
        let store = ConfigStore::in_memory(paths, Config::default());
        let paths = store.paths().clone();
        // A file that does not load is logged by the store, once (#262
        // item 6).
        let _ = store.reload();
        let app = Self::new((*store.config()).clone(), paths.clone());
        let parent_exists = paths.root().parent().is_none_or(Path::is_dir);
        // A missing directory whose load failed is behind a dangling
        // symlink, at it or above it (#382 item 1), whose target may
        // appear, or below one that cannot be searched, which may change.
        let behind_link = store.last_error().is_some();
        let started = if paths.root().is_dir() {
            app.start_config_watch(store, options)
        } else if !parent_exists && !behind_link {
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
            ConfigWatch::wait_for(app.live(), store, options)
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
            // A wait whose store refuses clipboard writes hands that store
            // over, so an empty `config.toml` keeps them off (#393 F1).
            let refusing = lock(&self.config_watch)
                .as_ref()
                .and_then(|watch| watch.refusing.clone());
            self.start_config_watch(refusing.unwrap_or(store), options)?;
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
            bindings: Arc::clone(&self.bindings),
            themes_dir: Arc::from(self.paths.themes_dir()),
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

    use tessera_config::WatchBackend;
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
        attach_fake_with_output(app, &[])
    }

    /// [`attach_fake`] whose reader finds `output` waiting, one event per
    /// chunk, as soon as it starts.
    fn attach_fake_with_output(app: &App, output: &[&[u8]]) -> Session1 {
        let (tx, rx) = mpsc::sync_channel(4.max(output.len()));
        for chunk in output {
            tx.send(PtyEvent::Output(chunk.to_vec())).unwrap();
        }
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
            view.capture(&term, 0);
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

    /// #253 item 4 (critic OC2), staged without a watcher: a
    /// `ThemesChanged` handled after the store reloaded a `config.toml`
    /// that names another theme and a translucent opacity (its `Changed`
    /// still queued) applies the new theme at the new opacity, one
    /// translucent report; the `Changed` that follows changes nothing.
    #[test]
    fn themes_changed_after_a_reload_applies_the_new_theme_at_the_new_opacity() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(root.path());
        std::fs::create_dir_all(paths.themes_dir()).unwrap();
        std::fs::write(
            paths.themes_dir().join("mine.toml"),
            theme_doc("Mine", "#101010"),
        )
        .unwrap();
        std::fs::write(
            paths.themes_dir().join("other.toml"),
            theme_doc("Other", "#212121"),
        )
        .unwrap();
        std::fs::write(paths.config_file(), "[appearance]\ntheme = \"mine\"\n").unwrap();
        let store = ConfigStore::open(paths.clone()).unwrap();
        let app = App::new((*store.config()).clone(), paths.clone());
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));
        let live = app.live();
        live.reconcile(&store);
        assert_eq!(recorder.take(), Vec::<bool>::new(), "nothing to reconcile");

        std::fs::write(
            paths.config_file(),
            "[appearance]\ntheme = \"other\"\nopacity = 0.5\n",
        )
        .unwrap();
        store.reload().unwrap();
        live.handle(&store, ConfigEvent::ThemesChanged);
        assert_eq!(recorder.take(), [false], "one translucent report");
        let theme = app.view_theme();
        assert_eq!(theme.background.to_srgb8(), [0x21, 0x21, 0x21]);
        assert_eq!(theme.background.a, 0.5, "the new opacity, exactly");
        assert_eq!(app.config().appearance.opacity, 0.5);

        live.apply_changed(&store, &store.config());
        assert_eq!(recorder.take(), Vec::<bool>::new(), "the Changed after it");
    }

    /// Records the fields of every warning (and error) logged on the
    /// thread it is the default subscriber of, `name=value`.
    struct WarningText(Mutex<String>);

    struct Fields<'a>(&'a mut String);

    impl tracing::field::Visit for Fields<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }

    impl tracing::Subscriber for WarningText {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            // More verbose levels compare greater.
            if *event.metadata().level() <= tracing::Level::WARN {
                event.record(&mut Fields(&mut lock(&self.0)));
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// S8: the warning that the theme file in use no longer loads escapes
    /// its path and its error (an oversized file's error quotes the path),
    /// so a file name carrying an OSC 52 request, an SGR sequence and a
    /// bidi override cannot reach the log raw.
    #[test]
    fn a_hostile_theme_file_name_is_escaped_when_it_stops_loading() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(root.path());
        std::fs::create_dir_all(paths.themes_dir()).unwrap();
        let hostile = paths
            .themes_dir()
            .join("\u{1b}]52;c;aGk=\u{7} \u{1b}[31m\u{9b}\u{202e}\u{2028}evil.toml");
        std::fs::write(&hostile, theme_doc("Keyed", "#101010")).unwrap();
        std::fs::write(paths.config_file(), "[appearance]\ntheme = \"Keyed\"\n").unwrap();
        let store = ConfigStore::open(paths.clone()).unwrap();
        let app = App::new((*store.config()).clone(), paths.clone());
        let live = app.live();
        live.reconcile(&store);
        assert_eq!(
            app.view_theme().background,
            Rgba::from_srgb8(0x10, 0x10, 0x10),
            "the hostile file is the theme in use"
        );

        let oversized =
            "#".repeat(usize::try_from(tessera_config::MAX_THEME_FILE_LEN).unwrap() + 1);
        for (case, broken) in [
            ("half-written", "name = \"Keyed\"\nforeground = "),
            ("oversized", &oversized),
        ] {
            std::fs::write(&hostile, broken).unwrap();
            let capture = Arc::new(WarningText(Mutex::new(String::new())));
            tracing::subscriber::with_default(Arc::clone(&capture), || {
                live.handle(&store, ConfigEvent::ThemesChanged);
            });
            let logged = lock(&capture.0).clone();
            assert!(logged.contains("does not load"), "{case}: {logged:?}");
            assert!(logged.contains("evil.toml"), "{case}: {logged:?}");
            for c in logged.chars() {
                let raw = c.is_control()
                    || matches!(
                        c,
                        '\u{2028}' | '\u{2029}' | '\u{061c}' | '\u{200e}' | '\u{200f}'
                    )
                    || ('\u{202a}'..='\u{202e}').contains(&c)
                    || ('\u{2066}'..='\u{2069}').contains(&c);
                assert!(!raw, "{case}: raw {c:?} logged: {logged:?}");
            }
            assert_eq!(
                app.view_theme().background,
                Rgba::from_srgb8(0x10, 0x10, 0x10),
                "{case}: its colours are kept"
            );
        }
    }

    /// Counts the warnings (and errors) logged on the thread it is the
    /// default subscriber of.
    struct WarningCount(std::sync::atomic::AtomicUsize);

    impl tracing::Subscriber for WarningCount {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            // More verbose levels compare greater.
            if *event.metadata().level() <= tracing::Level::WARN {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// #262 item 6 at the application's start: a `config.toml` that does
    /// not load is one warning (the store's), not one from the store and
    /// one from `App::with_config_dir`; and clipboard writes are refused.
    #[test]
    fn a_broken_file_at_start_is_one_warning() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(root.path());
        std::fs::write(paths.config_file(), "[terminal\nclipboard_write = true\n").unwrap();
        let count = Arc::new(WarningCount(std::sync::atomic::AtomicUsize::new(0)));
        let app = tracing::subscriber::with_default(Arc::clone(&count), || {
            App::with_config_dir(paths.clone(), WatchOptions::default())
        });
        assert_eq!(count.0.load(Ordering::SeqCst), 1, "one warning at start");
        assert!(!app.config().terminal.clipboard_write, "writes refused");
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
                app.with_terminal(id, |term| snapshot.capture(term, 0))
                    .unwrap();
                snapshot
            })
            .collect();
        let damage = |snapshots: &mut [Snapshot]| -> Vec<DamageRows> {
            [plain, reset, styled]
                .iter()
                .zip(snapshots.iter_mut())
                .map(|(&id, snapshot)| {
                    app.with_terminal(id, |term| snapshot.capture(term, 0))
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

    /// #169 (L05 and D1 option C through `apply_config`): a tab whose
    /// program sent `CSI ? 12 h CSI ? 12 l` (`vim -u NONE`; tmux's cnorm
    /// sends the `?12 l`) still follows a shape edit with its blink off,
    /// and follows an edit of `cursor_blink` too.
    #[test]
    fn apply_config_reaches_a_tab_that_ran_vim_or_tmux() {
        use tessera_config::CursorShape;
        use tessera_term::CursorShape as Shape;

        let style = |shape, blink| tessera_term::CursorStyle { shape, blink };
        let app = App::new(
            cursor_config(CursorShape::Beam, true),
            ConfigPaths::new("/nonexistent/tessera"),
        );
        let id = attach_fake(&app).id;
        app.session(id)
            .unwrap()
            .with_terminal_mut(|term| term.advance(b"\x1b[?12h\x1b[?12l"));
        assert_eq!(cursor_style_of(&app, id), beam(false), "?12 l stops it");

        app.apply_config(
            &cursor_config(CursorShape::Underline, true),
            &tessera_config::Theme::default(),
        );
        assert_eq!(
            cursor_style_of(&app, id),
            style(Shape::Underline, false),
            "a shape edit: the shape follows, the program's blink stays"
        );

        app.apply_config(
            &cursor_config(CursorShape::Underline, false),
            &tessera_config::Theme::default(),
        );
        app.apply_config(
            &cursor_config(CursorShape::Underline, true),
            &tessera_config::Theme::default(),
        );
        assert_eq!(
            cursor_style_of(&app, id),
            style(Shape::Underline, true),
            "a blink edit: the blink follows too"
        );
    }

    /// #168 (D-APPLY-ORDER): four applies of different cursor styles race
    /// while every terminal's lock is busy with output, and a session is
    /// attached meanwhile (its reader busy from the start). Each terminal
    /// reads the style under its own lock, so once both return every
    /// session, the new one too, has the style of the configuration in
    /// effect: an apply that recorded its configuration first but reaches a
    /// terminal last does not leave it on its older style. The same holds
    /// for the theme's colours each apply pushes (#10).
    ///
    /// Runs: `ADV_RUNS` (default 200, about 0.4 s in debug, so the seven
    /// threads do not load the timing tests beside it for long; #231). Both
    /// mutants it was written for fail all 20 of 20 tries at 50 runs:
    /// `Live::apply` pushing the `config` and colours it was given instead
    /// of `push_defaults`, and `App::attach` without its re-read once the
    /// session is listed.
    #[test]
    fn racing_applies_leave_every_session_on_the_style_in_effect() {
        use tessera_config::CursorShape;

        let runs = std::env::var("ADV_RUNS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(200_usize);
        let theme = tessera_config::Theme::default();
        let block = cursor_config(CursorShape::Block, false);
        let racing = [
            cursor_config(CursorShape::Beam, false),
            cursor_config(CursorShape::Underline, false),
            cursor_config(CursorShape::Beam, true),
            cursor_config(CursorShape::Underline, true),
        ];
        let themes = ["dracula", "nord", "gruvbox-dark", "one-dark"].map(bundled);
        let app = App::new(block.clone(), ConfigPaths::new("/nonexistent/tessera"));
        let busy: Vec<SessionId> = (0..3).map(|_| attach_fake(&app).id).collect();
        let stop = AtomicBool::new(false);
        let output = b"busy\r\n".repeat(2_000);
        let mut disagreeing = Vec::new();
        std::thread::scope(|scope| {
            // Busy readers: each holds its terminal's lock for a slice of
            // output at a time and lets a waiting thread go first between
            // slices, as a session's reader thread does.
            for &id in &busy {
                let session = app.session(id).unwrap();
                let stop = &stop;
                scope.spawn(move || {
                    let chunk = b"busy\r\n".repeat(16);
                    while !stop.load(Ordering::Relaxed) {
                        session.with_terminal_mut(|term| term.advance(&chunk));
                        std::thread::yield_now();
                    }
                });
            }
            for run in 0..runs {
                app.apply_config(&block, &theme);
                let barrier = std::sync::Barrier::new(racing.len() + 1);
                let fresh = std::thread::scope(|race| {
                    for (config, theme) in racing.iter().zip(&themes) {
                        let barrier = &barrier;
                        let app = &app;
                        race.spawn(move || {
                            barrier.wait();
                            app.apply_config(config, theme);
                        });
                    }
                    barrier.wait();
                    // Its reader starts on output, so the attach's re-read
                    // of the style waits for the terminal lock too.
                    attach_fake_with_output(&app, &[&output, &output]).id
                });
                let expected = view::cursor_style_for(&app.config());
                let colours = view::theme_colors_for(&lock(&app.applied).colours);
                for &id in busy.iter().chain([&fresh]) {
                    let style = cursor_style_of(&app, id);
                    let same = app.with_terminal(id, |t| t.colors() == colours);
                    if style != expected || !matches!(same, Ok(true)) {
                        disagreeing.push((run, id == fresh, style));
                    }
                }
                app.close_session(fresh).unwrap();
            }
            stop.store(true, Ordering::Relaxed);
        });
        assert_eq!(
            disagreeing,
            [],
            "(run, the session attached meanwhile, its shape) over {runs} runs"
        );
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

    /// #43 (R3): an edit that only reorders `[keys]` changes the chord a
    /// command's menu item shows (the first), so the watch applies it and
    /// tells the observer.
    #[test]
    fn a_reload_that_only_reorders_the_keys_is_applied() {
        struct Generations(Arc<Mutex<Vec<u64>>>);
        impl SessionObserver for Generations {
            fn on_session_event(&self, _: SessionId, _: SessionEvent) {}
            fn on_config_applied(&self, generation: u64) {
                lock(&self.0).push(generation);
            }
        }
        let base = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(base.path().join("Tessera"));
        std::fs::create_dir_all(paths.root()).unwrap();
        std::fs::write(
            paths.config_file(),
            "[keys]\n'cmd+e' = 'new_tab'\n'cmd+y' = 'new_tab'\n",
        )
        .unwrap();
        let app = App::with_config_dir(paths.clone(), WatchOptions::default());
        let seen = Arc::new(Mutex::new(Vec::new()));
        app.set_observer(Some(Box::new(Generations(Arc::clone(&seen)))));
        let new_tab = crate::command::CommandId::find("new_tab", None).unwrap();
        let first = |app: &App| {
            app.keymap()
                .chords(new_tab)
                .first()
                .map(ToString::to_string)
        };
        assert_eq!(first(&app).as_deref(), Some("cmd+e"));
        std::fs::write(
            paths.config_file(),
            "[keys]\n'cmd+y' = 'new_tab'\n'cmd+e' = 'new_tab'\n",
        )
        .unwrap();
        assert!(
            eventually(Duration::from_secs(5), || first(&app).as_deref()
                == Some("cmd+y")),
            "{:?}",
            first(&app)
        );
        assert!(!lock(&seen).is_empty(), "on_config_applied");
    }

    /// #287 (M1): a configuration directory that is a symlink whose target
    /// is missing (a dotfiles volume not mounted) starts with clipboard
    /// writes refused; once the target appears it is watched and the usual
    /// rules apply (no `config.toml`: the defaults, writes on).
    #[cfg(unix)]
    #[test]
    fn a_dangling_configuration_directory_refuses_writes_until_it_appears() {
        let base = tempfile::tempdir().unwrap();
        let target = base.path().join("volume").join("tessera");
        let paths = ConfigPaths::new(base.path().join("Tessera"));
        std::os::unix::fs::symlink(&target, paths.root()).unwrap();
        let app = App::with_config_dir(paths.clone(), WatchOptions::default());
        assert!(!app.config().terminal.clipboard_write, "writes refused");
        std::fs::create_dir_all(&target).unwrap();
        assert!(
            eventually(CONFIG_DIR_POLL_INTERVAL + Duration::from_secs(3), || {
                app.config().terminal.clipboard_write
            }),
            "the appeared directory applies"
        );
        assert!(app.is_watching_config());
    }

    /// Moves `dir.staged` (made beforehand) to `dir`, so `dir` appears at
    /// once with what it holds.
    #[cfg(unix)]
    fn appear(dir: &Path) {
        std::fs::rename(dir.with_extension("staged"), dir).unwrap();
    }

    /// #382 item 1: the configuration directory sits below a dangling
    /// symlink (`dotfiles -> volume`, not mounted; the directory is
    /// `dotfiles/tessera`), so its parent does not exist either. It is
    /// still waited for, with writes refused, and its `config.toml`
    /// applies once it appears, without ⌘, or a restart.
    #[cfg(unix)]
    #[test]
    fn a_directory_below_a_dangling_link_is_waited_for() {
        let base = tempfile::tempdir().unwrap();
        let volume = base.path().join("volume");
        let dotfiles = base.path().join("dotfiles");
        std::os::unix::fs::symlink(&volume, &dotfiles).unwrap();
        let paths = ConfigPaths::new(dotfiles.join("tessera"));
        let app = App::with_config_dir(paths.clone(), WatchOptions::default());
        assert!(!app.config().terminal.clipboard_write, "writes refused");
        assert!(lock(&app.config_watch).is_some(), "waiting");

        let staged = volume.with_extension("staged").join("tessera");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(
            staged.join("config.toml"),
            "[appearance]\nfont_size = 20.0\n[terminal]\nclipboard_write = true\n",
        )
        .unwrap();
        appear(&volume);
        assert!(
            eventually(CONFIG_DIR_POLL_INTERVAL + Duration::from_secs(3), || {
                (app.config().appearance.font_size - 20.0).abs() < f32::EPSILON
            }),
            "the appeared directory applies: {:?}",
            app.config().appearance
        );
        assert!(app.config().terminal.clipboard_write);
        assert!(app.is_watching_config());
        std::fs::write(paths.config_file(), "[appearance]\nfont_size = 21.0\n").unwrap();
        assert!(
            eventually(Duration::from_secs(3), || {
                (app.config().appearance.font_size - 21.0).abs() < f32::EPSILON
            }),
            "and is watched"
        );
    }

    /// #382 m2: started behind a dangling symlink (writes refused), the
    /// directory appears holding an empty or byte-order-mark-only
    /// `config.toml`. Writes stay off on both backends, whether the link
    /// is the directory itself or one above it: the refusal is carried
    /// into the watch, also when `open_config` (⌘,) starts it before the
    /// directory poll sees the directory (#393 F1). A version of the file
    /// with something in it that loads turns them on.
    #[cfg(unix)]
    #[test]
    fn an_empty_file_appearing_behind_a_dangling_link_keeps_writes_off() {
        let mut cases = Vec::new();
        for above in [false, true] {
            for backend in [WatchBackend::Native, WatchBackend::Poll] {
                for content in ["", "\u{feff}"] {
                    for opened in [false, true] {
                        cases.push(std::thread::spawn(move || {
                            // A case the directory poll won proves nothing
                            // about `open_config`: run it again.
                            assert!(
                                (0..5).any(|_| {
                                    empty_file_behind_a_dangling_link(
                                        above, backend, content, opened,
                                    )
                                }),
                                "the directory poll beat open_config five times"
                            );
                        }));
                    }
                }
            }
        }
        for case in cases {
            case.join().unwrap();
        }
    }

    /// One case of
    /// [`an_empty_file_appearing_behind_a_dangling_link_keeps_writes_off`];
    /// `opened`: `open_config` runs as soon as the directory appears.
    /// `false` when `opened` but the directory poll started the watch
    /// before `open_config` did, so the case did not test `open_config`
    /// (#417 item 7).
    #[cfg(unix)]
    #[must_use]
    fn empty_file_behind_a_dangling_link(
        above: bool,
        backend: WatchBackend,
        content: &str,
        opened: bool,
    ) -> bool {
        let what = format!("above: {above}, {backend:?}, {content:?}, opened: {opened}");
        let base = tempfile::tempdir().unwrap();
        let volume = base.path().join("volume");
        let link = base.path().join("link");
        std::os::unix::fs::symlink(&volume, &link).unwrap();
        let (root, staged_root) = if above {
            (
                link.join("tessera"),
                volume.with_extension("staged").join("tessera"),
            )
        } else {
            (link, volume.with_extension("staged"))
        };
        std::fs::create_dir_all(&staged_root).unwrap();
        std::fs::write(staged_root.join("config.toml"), content).unwrap();
        let paths = ConfigPaths::new(&root);
        let options = WatchOptions {
            backend,
            ..WatchOptions::default()
        };
        let app = App::with_config_dir(paths.clone(), options);
        assert!(!app.config().terminal.clipboard_write, "{what}");
        appear(&volume);
        if opened {
            app.open_config(WatchOptions {
                backend,
                ..WatchOptions::default()
            })
            .unwrap();
            assert!(app.is_watching_config(), "{what}: open_config watches");
            // `open_config` starts a watch of its own, which hands nothing
            // on; the wait's watch keeps the store it refuses with.
            let by_open_config = lock(&app.config_watch)
                .as_ref()
                .is_some_and(|watch| watch.refusing.is_none());
            if !by_open_config {
                return false;
            }
        }
        assert!(
            eventually(CONFIG_DIR_POLL_INTERVAL + Duration::from_secs(3), || {
                app.is_watching_config()
            }),
            "{what}: watched"
        );
        assert!(
            !eventually(Duration::from_millis(600), || {
                app.config().terminal.clipboard_write
            }),
            "{what}: the empty file turned writes on"
        );
        std::fs::write(paths.config_file(), "[terminal]\nclipboard_write = true\n").unwrap();
        assert!(
            eventually(Duration::from_secs(3), || {
                app.config().terminal.clipboard_write
            }),
            "{what}: a file that loads turns them on"
        );
        true
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
            // Waits on the view theme too: `restyle` records the config
            // before `restyle_at` updates the view theme, so a read of the
            // theme right after the config shows the new font size can
            // still see the old one under load (#343).
            let applied = || {
                let background = app.view_theme().background;
                (app.config().appearance.font_size - 20.0).abs() < f32::EPSILON
                    && background.with_alpha(1.0) == Rgba::from_srgb8(0x2e, 0x34, 0x40)
                    && (background.a - 0.5).abs() < 1e-6
            };
            assert!(
                eventually(bound, applied),
                "open_config: {via_open_config}: {:?}, background {:?}",
                app.config().appearance,
                app.view_theme().background
            );
            assert!(app.is_watching_config());
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

    /// Also when the directory is waited for below a dangling link
    /// (#382 item 1).
    #[test]
    fn an_app_waiting_for_its_directory_stops_promptly() {
        let base = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.path().join("volume"), base.path().join("dotfiles"))
            .unwrap();
        for (drop_it, root) in [
            (false, "Missing"),
            (true, "Missing"),
            #[cfg(unix)]
            (false, "dotfiles/tessera"),
            #[cfg(unix)]
            (true, "dotfiles/tessera"),
        ] {
            let paths = ConfigPaths::new(base.path().join(root));
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
                "{root}: drop: {drop_it}: took {took:?}"
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

    /// A session of `profile` (`None`: the built-in one) on a fake PTY.
    fn attach_fake_profile(app: &App, profile: Option<&str>) -> SessionId {
        let (tx, rx) = mpsc::sync_channel(4);
        let pty = FakePty {
            resizes: Arc::new(Mutex::new(Vec::new())),
            views: Arc::clone(&app.views),
            views_locked_during_resize: Arc::new(AtomicBool::new(false)),
            events: Some(tx),
        };
        let id = app.allocate_id();
        app.attach_with_profile(
            id,
            Box::new(pty),
            rx,
            PtySize::default(),
            profile.map(Into::into),
            &[],
        )
        .expect("attach fake session");
        id
    }

    /// Attaches a headless 200 x 100 view to session `id` the way
    /// `App::attach_view` does (the session's look, under the views lock);
    /// `false` when GPU tests are skipped.
    fn attach_offscreen(app: &App, id: SessionId) -> bool {
        if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
            eprintln!("TESSERA_SKIP_GPU_TESTS is set; skipping");
            return false;
        }
        app.attach_with(
            id,
            Gpu::new_instance(),
            (),
            |()| None,
            |gpu, (), fonts, theme| {
                View::offscreen(gpu, fonts, theme, PhysicalSize::new(200, 100), 1.0)
            },
        )
        .expect("attach an offscreen view");
        true
    }

    /// The top left pixel of session `id`'s next frame: padding, so the
    /// theme's background.
    fn corner(app: &App, id: SessionId) -> [u8; 4] {
        lock(&app.views).get_mut(&id).unwrap().redraw_now();
        assert_eq!(
            app.render_view(id).unwrap(),
            tessera_render::FrameStatus::Presented
        );
        let image = lock(&app.views).get(&id).unwrap().read_back();
        [image[0], image[1], image[2], image[3]]
    }

    fn srgb(colour: tessera_config::Color) -> [u8; 3] {
        [colour.r, colour.g, colour.b]
    }

    /// `[appearance]` in Dracula at 13 pt, a profile `big` that sets only
    /// its font size and a profile `nord` with a theme and size of its own.
    fn profiles_config() -> Config {
        Config::from_toml_lenient(
            "[appearance]\n\
             theme = \"Dracula\"\n\
             font_size = 13\n\
             [[profiles]]\n\
             name = \"big\"\n\
             [profiles.appearance]\n\
             font_size = 20\n\
             [[profiles]]\n\
             name = \"nord\"\n\
             [profiles.appearance]\n\
             theme = \"Nord\"\n\
             font_size = 18\n",
        )
        .unwrap()
        .config
    }

    /// Done-when 3 (#39): sessions of two profiles draw with their own
    /// theme and font size, and editing `[appearance]` restyles only the
    /// sessions that inherit what changed.
    #[test]
    fn sessions_of_two_profiles_draw_with_their_own_theme_and_font_size() {
        let config = profiles_config();
        let app = App::new(config.clone(), ConfigPaths::new("/nonexistent/tessera"));
        let plain = attach_fake_profile(&app, None);
        let nord = attach_fake_profile(&app, Some("nord"));
        let big = attach_fake_profile(&app, Some("big"));
        if !attach_offscreen(&app, plain) {
            return;
        }
        assert!(attach_offscreen(&app, nord));
        assert!(attach_offscreen(&app, big));
        let dracula = bundled("Dracula");
        let nord_colours = bundled("Nord");
        assert_ne!(dracula.background, nord_colours.background);
        assert_eq!(corner(&app, plain)[..3], srgb(dracula.background));
        assert_eq!(corner(&app, nord)[..3], srgb(nord_colours.background));
        assert_eq!(corner(&app, big)[..3], srgb(dracula.background));
        assert_eq!(font_size_of(&app, plain), 13.0);
        assert_eq!(font_size_of(&app, nord), 18.0);
        assert_eq!(font_size_of(&app, big), 20.0);
        // The terminals' default colours are their looks' too (OSC 11 ?).
        let bg = |id| app.with_terminal(id, |t| t.colors().bg).unwrap();
        assert_eq!(
            bg(nord),
            tessera_term::Rgb::new(
                nord_colours.background.r,
                nord_colours.background.g,
                nord_colours.background.b
            )
        );

        // Edit `[appearance]`: theme and size.
        let mut edited = config;
        edited.appearance.theme = "Gruvbox Dark".into();
        edited.appearance.font_size = 15.0;
        let gruvbox = bundled("Gruvbox Dark");
        app.apply_config(&edited, &gruvbox);
        assert_eq!(corner(&app, plain)[..3], srgb(gruvbox.background));
        assert_eq!(font_size_of(&app, plain), 15.0);
        // `big` inherits the theme, keeps its size.
        assert_eq!(corner(&app, big)[..3], srgb(gruvbox.background));
        assert_eq!(font_size_of(&app, big), 20.0);
        // `nord` sets both: untouched.
        assert_eq!(corner(&app, nord)[..3], srgb(nord_colours.background));
        assert_eq!(font_size_of(&app, nord), 18.0);
        // A zoom on `nord` survives an `[appearance]` size change (D6 is
        // per session: its configured size did not change).
        app.set_view_font_size(nord, 24.0).unwrap();
        edited.appearance.font_size = 11.0;
        app.apply_config(&edited, &gruvbox);
        assert_eq!(font_size_of(&app, nord), 24.0);
        assert_eq!(font_size_of(&app, plain), 11.0);
        // Editing the profile restyles its session.
        edited.profiles[1].appearance.as_mut().unwrap().theme = None;
        app.apply_config(&edited, &gruvbox);
        assert_eq!(corner(&app, nord)[..3], srgb(gruvbox.background));
        // Its size did not change, so its zoom stays (D6).
        assert_eq!(font_size_of(&app, nord), 24.0);
        // Its size changed: back to the configured one.
        edited.profiles[1].appearance.as_mut().unwrap().font_size = Some(16.0);
        app.apply_config(&edited, &gruvbox);
        assert_eq!(font_size_of(&app, nord), 16.0);
    }

    /// Two applies racing leave every view drawing its session's look
    /// (#39 critic B1): the older one never puts its look back over the
    /// newer one's under the views lock. Staged: the older apply sets the
    /// first session's look and then waits for that session's terminal
    /// (held here) while the newer one, which changes only the font size
    /// and so never needs that terminal, restyles every view; the older
    /// one then takes the views lock last.
    #[test]
    fn racing_applies_leave_every_view_on_its_sessions_look() {
        let mut dracula = Config::default();
        dracula.appearance.theme = "Dracula".into();
        dracula.appearance.font_size = 13.0;
        let app = App::new(dracula.clone(), ConfigPaths::new("/nonexistent/tessera"));
        let ids = [
            attach_fake_profile(&app, None),
            attach_fake_profile(&app, None),
        ];
        if !attach_offscreen(&app, ids[0]) {
            return;
        }
        assert!(attach_offscreen(&app, ids[1]));
        let mut older = dracula.clone();
        older.appearance.theme = "Nord".into();
        let mut newer = older.clone();
        newer.appearance.font_size = 14.0;
        let nord = bundled("Nord");
        app.apply_config(&dracula, &bundled("Dracula"));
        // The session `restyle` comes to first: the map is not changed.
        let first = *lock(&app.sessions).keys().next().unwrap();
        let session = Arc::clone(&lock(&app.sessions)[&first]);
        let start = lock(&app.applied).generation;
        let wait = |what: &str, done: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !done() {
                assert!(std::time::Instant::now() < deadline, "timed out: {what}");
                std::thread::yield_now();
            }
        };
        let (held_tx, held) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let app = &app;
            scope.spawn(move || {
                app.with_terminal(first, |_| {
                    held_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                })
                .unwrap();
            });
            held.recv().unwrap();
            scope.spawn(|| app.apply_config(&older, &nord));
            wait("the older look", &|| session.look_generation() == start + 1);
            scope.spawn(|| app.apply_config(&newer, &nord));
            wait("the newer apply's views", &|| {
                lock(&app.views)[&first].font_size().to_bits() == 14.0_f32.to_bits()
            });
            release.send(()).unwrap();
        });
        let sessions = lock(&app.sessions).clone();
        let views = lock(&app.views);
        for id in ids {
            let look = sessions[&id].look();
            assert_eq!(look.font_size(), 14.0, "session {id} has the newer look");
            let theme = views[&id].base_theme();
            assert!(
                *theme == look.theme,
                "view of {id} draws {} {} pt, its session's look {} {} pt",
                theme.font_family,
                theme.font_size,
                look.theme.font_family,
                look.theme.font_size
            );
            assert_eq!(views[&id].font_size(), 14.0, "view of {id}");
        }
    }

    /// The process's font is `[appearance]`'s family, never a profile's
    /// (#39 critic B2, ADR 0037), even when the first view is of a profile
    /// with a family of its own (one `default_profile` may name, so every
    /// new tab's).
    #[test]
    fn a_profiles_font_family_never_becomes_the_process_font() {
        let config = Config::from_toml_lenient(
            "[general]\n\
             default_profile = \"work\"\n\
             [appearance]\n\
             font_family = \"Menlo\"\n\
             [[profiles]]\n\
             name = \"work\"\n\
             [profiles.appearance]\n\
             font_family = \"Courier\"\n",
        )
        .unwrap()
        .config;
        let app = App::new(config, ConfigPaths::new("/nonexistent/tessera"));
        let id = attach_fake_profile(&app, Some("work"));
        assert_eq!(
            lock(&app.sessions)[&id].look().theme.font_family,
            "Courier",
            "the session's look keeps the profile's family"
        );
        if !attach_offscreen(&app, id) {
            return;
        }
        let fonts = lock(&app.fonts).clone().expect("fonts loaded by the view");
        assert!(
            fonts.has_family("Courier"),
            "the probe needs Courier installed"
        );
        assert_eq!(fonts.family(), "Menlo");
    }

    /// A profile whose theme file stops loading keeps the colours its
    /// sessions draw with, as `[appearance]`'s theme does
    /// ([`Lookup::KeepBroken`]); a session with none to keep gets what the
    /// lookup fell back to.
    #[test]
    fn a_profile_whose_theme_file_breaks_keeps_its_colours() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Mine.toml"), "this = = is not a theme").unwrap();
        let config = Config::from_toml_lenient(
            "[appearance]\n\
             theme = \"Dracula\"\n\
             [[profiles]]\n\
             name = \"p\"\n\
             [profiles.appearance]\n\
             theme = \"Mine\"\n",
        )
        .unwrap()
        .config;
        let colours = bundled("Dracula");
        let previous = Look::new(
            config.profile_appearance(Some("p")),
            bundled("Nord"),
            DisplayOptions::default(),
        );
        let mut looks = Looks::new(dir.path(), &config, &colours, DisplayOptions::default());
        assert_eq!(
            looks.of(Some("p"), Some(&previous)).colours,
            previous.colours
        );
        assert_ne!(looks.of(Some("p"), None).colours, previous.colours);
    }

    /// The observer hears "opaque" only while every view is (#39): one
    /// translucent profile makes the host draw translucent.
    #[test]
    fn appearance_reports_opaque_only_while_every_view_is_opaque() {
        let mut config = profiles_config();
        config.profiles[1].appearance.as_mut().unwrap().opacity = Some(1.0);
        let app = App::new(config.clone(), ConfigPaths::new("/nonexistent/tessera"));
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));
        let plain = attach_fake_profile(&app, None);
        let nord = attach_fake_profile(&app, Some("nord"));
        if !attach_offscreen(&app, plain) {
            return;
        }
        assert!(attach_offscreen(&app, nord));
        let dracula = bundled("Dracula");
        // `[appearance]` translucent, `nord` opaque: translucent.
        config.appearance.opacity = 0.5;
        app.apply_config(&config, &dracula);
        assert_eq!(recorder.take(), [false]);
        assert_eq!(corner(&app, plain)[3], 128, "the plain view is translucent");
        assert_eq!(corner(&app, nord)[3], 255, "the nord view is opaque");
        // Only `nord` translucent: still translucent.
        config.appearance.opacity = 1.0;
        config.profiles[1].appearance.as_mut().unwrap().opacity = Some(0.5);
        app.apply_config(&config, &dracula);
        assert_eq!(recorder.take(), [false]);
        // Both opaque: opaque.
        config.profiles[1].appearance.as_mut().unwrap().opacity = Some(1.0);
        app.apply_config(&config, &dracula);
        assert_eq!(recorder.take(), [true]);
    }

    /// `[general] default_profile` styles the sessions that name no
    /// profile, and with no session the App-level theme follows it.
    #[test]
    fn the_default_profile_styles_new_sessions_and_the_app_theme() {
        let mut config = profiles_config();
        config.general.default_profile = "nord".into();
        let app = App::new(config, ConfigPaths::new("/nonexistent/tessera"));
        let nord = bundled("Nord");
        assert_eq!(
            app.view_theme().background.to_srgb8()[..3],
            srgb(nord.background)
        );
        assert_eq!(app.view_theme().font_size, 18.0);
    }

    /// A session that starts while a configuration is applied ends on the
    /// newer look: the apply lists the sessions after recording it, and a
    /// session not yet listed catches up (`Live::catch_up`).
    #[test]
    fn a_session_started_with_an_older_configuration_catches_up() {
        let app = App::new(profiles_config(), ConfigPaths::new("/nonexistent/tessera"));
        let id = attach_fake_profile(&app, Some("big"));
        let session = app.session(id).unwrap();
        // As if the look had been resolved before an apply that missed it.
        let mut newer = profiles_config();
        newer.appearance.theme = "Nord".into();
        {
            let mut applied = lock(&app.applied);
            applied.config = Arc::new(newer);
            applied.colours = bundled("Nord");
            applied.generation += 1;
        }
        app.live().catch_up(&session);
        assert_eq!(session.look().appearance.theme, "Nord");
        assert_eq!(session.look().appearance.font_size, 20.0);
        assert_eq!(
            app.with_terminal(id, |t| t.colors().bg).unwrap(),
            tessera_term::Rgb::new(
                bundled("Nord").background.r,
                bundled("Nord").background.g,
                bundled("Nord").background.b
            )
        );
        // An older look never replaces a newer one.
        let stale = Arc::new(Look::new(
            tessera_config::Appearance::default(),
            bundled("Dracula"),
            DisplayOptions::default(),
        ));
        assert!(session.set_look(0, stale).is_none());
        assert_eq!(session.look().appearance.theme, "Nord");
    }

    /// #283 item 1: a `Changed` event applies the store's configuration,
    /// not the one it carries: an older `Changed` still queued behind a
    /// newer apply must not bring the older configuration back.
    #[test]
    fn a_stale_changed_event_applies_the_stores_configuration() {
        let paths = ConfigPaths::new("/nonexistent/tessera");
        let current = config("Nord", 13.0, 1.0);
        let store = ConfigStore::in_memory(paths.clone(), current.clone());
        let app = App::new(current.clone(), paths);
        let live = app.live();
        let stale = config("Dracula", 20.0, 0.5);
        live.handle(
            &store,
            ConfigEvent::Changed(tessera_config::ConfigChanged {
                config: Arc::new(stale),
                warnings: Vec::new(),
            }),
        );
        assert_eq!(*app.config(), current);
        assert_eq!(
            app.view_theme().background.to_srgb8()[..3],
            srgb(bundled("Nord").background)
        );
    }

    /// A configuration whose `[appearance]` is translucent and blurred,
    /// with an opaque profile `solid`, an unblurred translucent profile
    /// `clear` and a blurred translucent one `frosted`.
    fn translucency_config() -> Config {
        Config::from_toml_lenient(
            "[appearance]\n\
             theme = \"Dracula\"\n\
             opacity = 0.5\n\
             blur = true\n\
             [[profiles]]\n\
             name = \"solid\"\n\
             [profiles.appearance]\n\
             opacity = 1.0\n\
             [[profiles]]\n\
             name = \"clear\"\n\
             [profiles.appearance]\n\
             blur = false\n\
             [[profiles]]\n\
             name = \"frosted\"\n\
             [profiles.appearance]\n\
             opacity = 0.8\n\
             blur = true\n",
        )
        .unwrap()
        .config
    }

    const REDUCE: DisplayOptions = DisplayOptions {
        reduce_transparency: true,
        increase_contrast: false,
    };

    const CONTRAST: DisplayOptions = DisplayOptions {
        reduce_transparency: false,
        increase_contrast: true,
    };

    /// Increase Contrast (ADR 0041, fixture E13/E14) draws the cursor and
    /// the selection opaque in every view theme, tells the observer once,
    /// turns `dim_inactive_panes` off while it is on (the configured amount
    /// comes back after), and keeps the background as translucent as the
    /// configuration says.
    #[test]
    fn increase_contrast_draws_opaque_cursor_and_selection_and_stops_dimming() {
        let mut config = translucency_config();
        config.appearance.dim_inactive_panes = 0.4;
        let app = App::new(config.clone(), ConfigPaths::new("/nonexistent/tessera"));
        let mut colours = bundled("Dracula");
        colours.selection = colours.selection.with_alpha(0x99);
        colours.cursor = colours.cursor.with_alpha(0x40);
        app.apply_config(&config, &colours);
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));
        let plain = attach_fake_profile(&app, None);
        assert!((app.dim_inactive_panes() - 0.4).abs() < f32::EPSILON);
        let before = app.view_theme();
        assert!(
            before.selection.a < 1.0,
            "the theme's selection is translucent"
        );

        app.set_display_options(CONTRAST);
        assert_eq!(app.display_options(), CONTRAST);
        assert_eq!(recorder.take(), [false], "told once, still translucent");
        let theme = app.view_theme();
        assert!((theme.selection.a - 1.0).abs() < f32::EPSILON);
        assert!((theme.cursor.a - 1.0).abs() < f32::EPSILON);
        assert_eq!(theme.background, before.background);
        assert_eq!(
            flags(app.session_appearance(plain).unwrap()),
            appearance(false, true)
        );
        assert!(app.dim_inactive_panes().abs() < f32::EPSILON, "no dimming");

        app.set_display_options(DisplayOptions::default());
        assert_eq!(recorder.take(), [false]);
        assert_eq!(app.view_theme(), before);
        assert!((app.dim_inactive_panes() - 0.4).abs() < f32::EPSILON);
    }

    /// The flags `flags` compares a [`crate::SessionAppearance`] by.
    fn appearance(opaque: bool, blur: bool) -> (bool, bool) {
        (opaque, blur)
    }

    /// Whether `look` is opaque and blurred (its colour aside).
    fn flags(look: crate::SessionAppearance) -> (bool, bool) {
        (look.opaque, look.blur)
    }

    /// Each session reports its own profile's translucency and blur
    /// (ADR 0043): blur only where the profile asks for it and its
    /// background is translucent.
    #[test]
    fn session_appearance_follows_each_profile() {
        let app = App::new(
            translucency_config(),
            ConfigPaths::new("/nonexistent/tessera"),
        );
        let plain = attach_fake_profile(&app, None);
        let solid = attach_fake_profile(&app, Some("solid"));
        let clear = attach_fake_profile(&app, Some("clear"));
        let frosted = attach_fake_profile(&app, Some("frosted"));
        assert_eq!(
            flags(app.session_appearance(plain).unwrap()),
            appearance(false, true)
        );
        assert_eq!(
            flags(app.session_appearance(solid).unwrap()),
            appearance(true, false)
        );
        assert_eq!(
            flags(app.session_appearance(clear).unwrap()),
            appearance(false, false)
        );
        assert_eq!(
            flags(app.session_appearance(frosted).unwrap()),
            appearance(false, true)
        );
        assert!(matches!(
            app.session_appearance(SessionId::from_raw(9999)),
            Err(crate::Error::SessionNotFound(_))
        ));
    }

    /// Reduce Transparency (ADR 0043) makes every session opaque and
    /// unblurred, its views' backgrounds drawn opaque, and tells the
    /// observer once; the same options again change nothing; turning it
    /// off brings each profile's translucency and blur back. The options
    /// last across a configuration apply and reach sessions started later.
    #[test]
    fn reduce_transparency_makes_every_session_opaque_until_turned_off() {
        let config = translucency_config();
        let app = App::new(config.clone(), ConfigPaths::new("/nonexistent/tessera"));
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));
        let plain = attach_fake_profile(&app, None);
        let frosted = attach_fake_profile(&app, Some("frosted"));
        let views = attach_offscreen(&app, plain);
        if views {
            assert_eq!(corner(&app, plain)[3], 128, "translucent before");
        }
        let generation = app.config_generation();

        app.set_display_options(REDUCE);
        assert_eq!(app.display_options(), REDUCE);
        assert_eq!(recorder.take(), [true], "told once, opaque");
        assert_eq!(app.config_generation(), generation, "no new configuration");
        for id in [plain, frosted] {
            assert_eq!(
                flags(app.session_appearance(id).unwrap()),
                appearance(true, false)
            );
        }
        assert!(!view::is_translucent(&app.view_theme()));
        if views {
            assert_eq!(corner(&app, plain)[3], 255, "the view draws opaque");
        }

        app.set_display_options(REDUCE);
        assert_eq!(recorder.take(), Vec::<bool>::new(), "unchanged: not told");

        // A configuration applied meanwhile keeps the options.
        let mut edited = config.clone();
        edited.appearance.opacity = 0.6;
        app.apply_config(&edited, &bundled("Dracula"));
        assert_eq!(
            flags(app.session_appearance(plain).unwrap()),
            appearance(true, false)
        );
        let late = attach_fake_profile(&app, None);
        assert_eq!(
            flags(app.session_appearance(late).unwrap()),
            appearance(true, false)
        );
        recorder.take();

        app.set_display_options(DisplayOptions::default());
        assert_eq!(recorder.take(), [false], "told once, translucent");
        for id in [plain, frosted, late] {
            assert_eq!(
                flags(app.session_appearance(id).unwrap()),
                appearance(false, true)
            );
        }
        if views {
            assert_eq!(corner(&app, plain)[3], 153, "0.6 again, the edited opacity");
        }
    }

    /// With no session, Reduce Transparency still turns the App's theme
    /// (what a new view would draw) opaque, and a session started under
    /// it starts opaque.
    #[test]
    fn reduce_transparency_set_before_any_session_applies_to_the_first() {
        let app = App::new(
            translucency_config(),
            ConfigPaths::new("/nonexistent/tessera"),
        );
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));
        assert!(view::is_translucent(&app.view_theme()));
        app.set_display_options(REDUCE);
        assert_eq!(recorder.take(), [true]);
        assert!(!view::is_translucent(&app.view_theme()));
        let id = attach_fake_profile(&app, Some("frosted"));
        assert_eq!(
            flags(app.session_appearance(id).unwrap()),
            appearance(true, false)
        );
    }

    /// Toggling `blur` alone, at an unchanged translucent opacity, changes
    /// nothing the views draw but still tells the observer, which draws
    /// the blur (ADR 0043).
    #[test]
    fn toggling_blur_alone_tells_the_observer() {
        let mut config = translucency_config();
        let app = App::new(config.clone(), ConfigPaths::new("/nonexistent/tessera"));
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));
        let id = attach_fake_profile(&app, None);
        let dracula = bundled("Dracula");
        config.appearance.blur = false;
        app.apply_config(&config, &dracula);
        assert_eq!(recorder.take(), [false]);
        assert_eq!(
            flags(app.session_appearance(id).unwrap()),
            appearance(false, false)
        );
        config.appearance.blur = true;
        app.apply_config(&config, &dracula);
        assert_eq!(recorder.take(), [false]);
        assert_eq!(
            flags(app.session_appearance(id).unwrap()),
            appearance(false, true)
        );
    }

    /// A configuration applied after the display options changed is the
    /// one in effect, with the options; options set after a configuration
    /// was applied never bring an older configuration back (they restyle
    /// with the one in effect).
    #[test]
    fn display_options_restyle_with_the_configuration_in_effect() {
        let config = translucency_config();
        let app = App::new(config.clone(), ConfigPaths::new("/nonexistent/tessera"));
        let id = attach_fake_profile(&app, None);
        let mut nord = config.clone();
        nord.appearance.theme = "Nord".into();
        app.apply_config(&nord, &bundled("Nord"));
        app.set_display_options(REDUCE);
        assert_eq!(app.config().appearance.theme, "Nord");
        let session = app.session(id).unwrap();
        assert_eq!(session.look().appearance.theme, "Nord");
        assert_eq!(session.look().colours, bundled("Nord"));
        assert!(session.look().is_opaque());
    }

    /// A restyle of the configuration thread that recorded its generation
    /// before the options were set, and runs after them, is older than
    /// theirs: it cannot undo them.
    #[test]
    fn a_restyle_older_than_the_display_options_cannot_undo_them() {
        let config = translucency_config();
        let app = App::new(config.clone(), ConfigPaths::new("/nonexistent/tessera"));
        let id = attach_fake_profile(&app, None);
        let (before, colours) = {
            let applied = lock(&app.applied);
            (applied.generation, applied.colours.clone())
        };
        app.set_display_options(REDUCE);
        app.live()
            .restyle_at(&config, &colours, before, DisplayOptions::default(), false);
        assert_eq!(
            flags(app.session_appearance(id).unwrap()),
            appearance(true, false),
            "the late restyle was refused"
        );
    }

    /// The configured background as `SessionAppearance::background` should
    /// report it: `colour`'s sRGB bytes over 255, and `alpha`.
    fn background_of(colour: tessera_config::Color, alpha: f32) -> [f32; 4] {
        let unit = |byte: u8| f32::from(byte) / 255.0;
        [unit(colour.r), unit(colour.g), unit(colour.b), alpha]
    }

    /// Three profiles for the background tests: `[appearance]` Dracula at
    /// 0.5, `nord` Nord at 0.8, `solid` Dracula at 1.0.
    fn background_config() -> Config {
        Config::from_toml_lenient(
            "[appearance]\n\
             theme = \"Dracula\"\n\
             opacity = 0.5\n\
             [[profiles]]\n\
             name = \"nord\"\n\
             [profiles.appearance]\n\
             theme = \"Nord\"\n\
             opacity = 0.8\n\
             [[profiles]]\n\
             name = \"solid\"\n\
             [profiles.appearance]\n\
             opacity = 1.0\n",
        )
        .unwrap()
        .config
    }

    /// #388 (ADR 0043 amendment): each session reports its profile's
    /// theme background, sRGB bytes over 255, with the profile's opacity
    /// as straight alpha; Reduce Transparency makes every alpha 1.0 and
    /// keeps the colours; Increase Contrast changes nothing here.
    #[test]
    fn session_appearance_reports_each_profiles_configured_background() {
        let app = App::new(
            background_config(),
            ConfigPaths::new("/nonexistent/tessera"),
        );
        let plain = attach_fake_profile(&app, None);
        let nord = attach_fake_profile(&app, Some("nord"));
        let solid = attach_fake_profile(&app, Some("solid"));
        let dracula = bundled("Dracula").background;
        let nord_bg = bundled("Nord").background;
        let expected = [
            (plain, background_of(dracula, 0.5)),
            (nord, background_of(nord_bg, 0.8)),
            (solid, background_of(dracula, 1.0)),
        ];
        for (id, background) in expected {
            assert_eq!(app.session_appearance(id).unwrap().background, background);
        }
        assert!(app.session_appearance(solid).unwrap().opaque);
        assert_ne!(dracula, nord_bg, "the two themes differ");

        app.set_display_options(REDUCE);
        for (id, [r, g, b, _]) in expected {
            let look = app.session_appearance(id).unwrap();
            assert_eq!(look.background, [r, g, b, 1.0], "opaque, same colour");
            assert!(look.opaque);
        }
        app.set_display_options(CONTRAST);
        for (id, background) in expected {
            assert_eq!(app.session_appearance(id).unwrap().background, background);
        }
    }

    /// A theme whose background has an alpha of its own: the reported
    /// alpha is the theme's times the opacity, as the view draws it.
    #[test]
    fn session_background_alpha_includes_the_themes_alpha() {
        let config = background_config();
        let app = App::new(config.clone(), ConfigPaths::new("/nonexistent/tessera"));
        let plain = attach_fake_profile(&app, None);
        let mut colours = bundled("Dracula");
        colours.background = colours.background.with_alpha(0x80);
        app.apply_config(&config, &colours);
        let look = app.session_appearance(plain).unwrap();
        let alpha = 128.0 / 255.0 * 0.5;
        assert!((look.background[3] - alpha).abs() < 1e-6, "{look:?}");
        assert_eq!(
            look.background[..3],
            background_of(colours.background, 0.0)[..3]
        );
    }

    /// D3 (ADR 0043 amendment): a background the program sets (OSC 11)
    /// changes what the view draws but not the reported background, which
    /// is the configured one; the program's reset leaves it as it was.
    #[test]
    fn an_osc_11_background_is_not_followed() {
        let app = App::new(
            background_config(),
            ConfigPaths::new("/nonexistent/tessera"),
        );
        let plain = attach_fake_profile(&app, None);
        let before = app.session_appearance(plain).unwrap();
        app.feed_for_test(plain, b"\x1b]11;#ff0000\x07").unwrap();
        assert_eq!(
            app.with_terminal(plain, |term| term.color_overrides().bg)
                .unwrap(),
            Some(tessera_term::Rgb::new(0xff, 0, 0)),
            "the program's colour took"
        );
        assert_eq!(app.session_appearance(plain).unwrap(), before);
        app.feed_for_test(plain, b"\x1b]111\x07").unwrap();
        assert_eq!(app.session_appearance(plain).unwrap(), before);
    }

    /// #388 done-when 2: an apply that changes a session's background (its
    /// profile's theme or opacity, or `[appearance]`'s) tells the observer
    /// exactly once, however many sessions it changes, also for sessions
    /// with no view; an apply that changes nothing tells it nothing.
    #[test]
    fn a_background_change_tells_the_observer_once_and_no_change_never() {
        let mut config = background_config();
        let app = App::new(config.clone(), ConfigPaths::new("/nonexistent/tessera"));
        let dracula = bundled("Dracula");
        app.apply_config(&config, &dracula);
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));
        let plain = attach_fake_profile(&app, None);
        let other = attach_fake_profile(&app, None);
        let nord = attach_fake_profile(&app, Some("nord"));

        app.apply_config(&config, &dracula);
        assert_eq!(recorder.take(), Vec::<bool>::new(), "unchanged");

        // The `nord` profile's theme: only a session with no view changes.
        config.profiles[0].appearance.as_mut().unwrap().theme = Some("Gruvbox Dark".into());
        app.apply_config(&config, &dracula);
        assert_eq!(recorder.take(), [false], "told once");
        assert_eq!(
            app.session_appearance(nord).unwrap().background,
            background_of(bundled("Gruvbox Dark").background, 0.8)
        );
        app.apply_config(&config, &dracula);
        assert_eq!(recorder.take(), Vec::<bool>::new(), "unchanged again");

        // The profile's opacity alone.
        config.profiles[0].appearance.as_mut().unwrap().opacity = Some(0.6);
        app.apply_config(&config, &dracula);
        assert_eq!(recorder.take(), [false], "told once");
        assert!((app.session_appearance(nord).unwrap().background[3] - 0.6).abs() < 1e-6);

        // `[appearance]`'s opacity: two sessions change, one call.
        config.appearance.opacity = 0.7;
        app.apply_config(&config, &dracula);
        assert_eq!(recorder.take(), [false], "one call for both sessions");
        for id in [plain, other] {
            assert!((app.session_appearance(id).unwrap().background[3] - 0.7).abs() < 1e-6);
        }

        // `[appearance]`'s theme.
        let nord_colours = bundled("Nord");
        config.appearance.theme = "Nord".into();
        app.apply_config(&config, &nord_colours);
        assert_eq!(recorder.take(), [false], "told once");
        assert_eq!(
            app.session_appearance(plain).unwrap().background,
            background_of(nord_colours.background, 0.7)
        );
        app.apply_config(&config, &nord_colours);
        assert_eq!(recorder.take(), Vec::<bool>::new(), "unchanged");
    }

    /// The same through the watch: a profile's theme file edited on disk
    /// (`ThemesChanged`, the `[appearance]` theme unchanged) tells the
    /// observer once and reports the new colour, for a session with no
    /// view; looking again with nothing changed tells it nothing.
    #[test]
    fn a_profile_theme_file_edit_tells_the_observer_once() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(root.path());
        std::fs::create_dir_all(paths.themes_dir()).unwrap();
        let file = paths.themes_dir().join("ocean.toml");
        std::fs::write(&file, theme_doc("Ocean", "#102030")).unwrap();
        std::fs::write(
            paths.config_file(),
            "[[profiles]]\nname = \"sea\"\n[profiles.appearance]\ntheme = \"ocean\"\n",
        )
        .unwrap();
        let store = ConfigStore::open(paths.clone()).unwrap();
        let app = App::new((*store.config()).clone(), paths.clone());
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));
        let sea = attach_fake_profile(&app, Some("sea"));
        let colour = |hex: &str| background_of(hex.parse().unwrap(), 1.0);
        assert_eq!(
            app.session_appearance(sea).unwrap().background,
            colour("#102030")
        );
        let live = app.live();
        live.handle(&store, ConfigEvent::ThemesChanged);
        assert_eq!(recorder.take(), Vec::<bool>::new(), "nothing changed");

        std::fs::write(&file, theme_doc("Ocean", "#405060")).unwrap();
        live.handle(&store, ConfigEvent::ThemesChanged);
        assert_eq!(recorder.take(), [true], "told once");
        assert_eq!(
            app.session_appearance(sea).unwrap().background,
            colour("#405060")
        );
    }

    /// #397 m2: a configuration that did not come through the loader (so
    /// nothing clamped it) still reports a blur radius of at most
    /// `Appearance::MAX_BLUR_RADIUS`.
    #[test]
    fn the_blur_radius_is_clamped_whatever_built_the_configuration() {
        let mut config = Config::default();
        config.appearance.blur_radius = tessera_config::Appearance::MAX_BLUR_RADIUS + 1;
        let app = App::new(config.clone(), ConfigPaths::new("/nonexistent/tessera"));
        assert_eq!(
            app.blur_radius(),
            tessera_config::Appearance::MAX_BLUR_RADIUS
        );
        config.appearance.blur_radius = u32::MAX;
        app.apply_config(&config, &bundled("Dracula"));
        assert_eq!(
            app.blur_radius(),
            tessera_config::Appearance::MAX_BLUR_RADIUS
        );
        config.appearance.blur_radius = 7;
        app.apply_config(&config, &bundled("Dracula"));
        assert_eq!(app.blur_radius(), 7);
    }

    /// #397: the same through a running watch, on each backend: a
    /// profile's theme file saved in place (not renamed, so `themes/`
    /// itself does not change) restyles the profile's session, whose view
    /// draws the new colours, and tells the observer. The native backend
    /// watches the profiles' theme files for it; the poll backend sees it
    /// in its full scan of `themes/`.
    #[test]
    fn a_profile_theme_saved_in_place_restyles_through_the_watch() {
        for backend in [WatchBackend::Native, WatchBackend::Poll] {
            let root = tempfile::tempdir().unwrap();
            let paths = ConfigPaths::new(root.path());
            std::fs::create_dir_all(paths.themes_dir()).unwrap();
            let file = paths.themes_dir().join("ocean.toml");
            std::fs::write(&file, theme_doc("Ocean", "#102030")).unwrap();
            std::fs::write(
                paths.config_file(),
                "[[profiles]]\nname = \"sea\"\n[profiles.appearance]\ntheme = \"ocean\"\n",
            )
            .unwrap();
            let app = App::with_config_dir(
                paths.clone(),
                WatchOptions {
                    backend,
                    ..WatchOptions::default()
                },
            );
            assert!(app.is_watching_config(), "{backend:?}");
            let sea = attach_fake_profile(&app, Some("sea"));
            // The watch needs no GPU: without one (`TESSERA_SKIP_GPU_TESTS`)
            // only the view's part is left out (#417 item 6).
            let viewed = attach_offscreen(&app, sea);
            let recorder = Recorder::default();
            app.set_observer(Some(Box::new(recorder.clone())));
            let before = viewed.then(|| view_theme_of(&app, sea).background);
            let saved = background_of("#405060".parse().unwrap(), 1.0);
            assert_ne!(
                app.session_appearance(sea).unwrap().background,
                saved,
                "{backend:?}"
            );

            std::fs::write(&file, theme_doc("Ocean", "#405060")).unwrap();
            // Generous: the poll backend sees an in-place save at its full
            // theme scan, seconds late under background QoS (#393 B2).
            assert!(
                eventually(Duration::from_secs(30), || {
                    app.session_appearance(sea)
                        .unwrap()
                        .background
                        .iter()
                        .zip(saved)
                        .all(|(now, saved)| (now - saved).abs() < 1e-6)
                        && before.is_none_or(|before| view_theme_of(&app, sea).background != before)
                }),
                "{backend:?}: the session (and its view) take the saved theme"
            );
            assert_eq!(
                app.session_appearance(sea).unwrap().background,
                saved,
                "{backend:?}"
            );
            assert!(
                eventually(Duration::from_secs(1), || !lock(&recorder.0).is_empty()),
                "{backend:?}: the observer was told"
            );
        }
    }

    /// #365: `App::app_appearance` follows an edit of `config.toml` that
    /// the watch applies, and is already the new value when the host
    /// hears `on_config_applied` (its cue to read it again, lead decision
    /// D5-1): the observer reads the value in effect at each call. A
    /// profile's value and the display options never change it.
    #[test]
    fn app_appearance_follows_a_live_reload() {
        use tessera_config::AppAppearance;

        struct Seen(Live, Arc<Mutex<Vec<AppAppearance>>>);
        impl SessionObserver for Seen {
            fn on_session_event(&self, _: SessionId, _: SessionEvent) {}
            fn on_config_applied(&self, _generation: u64) {
                let now = lock(&self.0.applied).config.appearance.app_appearance;
                lock(&self.1).push(now);
            }
        }
        let base = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(base.path().join("Tessera"));
        std::fs::create_dir_all(paths.root()).unwrap();
        std::fs::write(
            paths.config_file(),
            "[appearance]
font_size = 14.0
",
        )
        .unwrap();
        let app = App::with_config_dir(paths.clone(), WatchOptions::default());
        assert_eq!(app.app_appearance(), AppAppearance::Dark, "missing key");
        let seen = Arc::new(Mutex::new(Vec::new()));
        app.set_observer(Some(Box::new(Seen(app.live(), Arc::clone(&seen)))));

        // Each step's own font size marks when its file is in effect.
        for (size, text, want) in [
            (15u8, "app_appearance = \"light\"\n", AppAppearance::Light),
            (16, "app_appearance = \"system\"\n", AppAppearance::System),
            (
                17,
                "app_appearance = \"system\"\n[[profiles]]\nname = \"p\"\n\
                 [profiles.appearance]\napp_appearance = \"light\"\n",
                AppAppearance::System,
            ),
            (18, "app_appearance = \"nope\"\n", AppAppearance::Dark),
        ] {
            let text = format!("[appearance]\nfont_size = {size}.0\n{text}");
            std::fs::write(paths.config_file(), &text).unwrap();
            assert!(
                eventually(Duration::from_secs(10), || {
                    #[allow(clippy::float_cmp)]
                    let applied = app.config().appearance.font_size == f32::from(size);
                    applied && lock(&seen).last() == Some(&want)
                }),
                "{text}: {:?}",
                lock(&seen)
            );
            assert_eq!(app.app_appearance(), want, "{text}");
            assert_eq!(
                app.config()
                    .resolve_profile(Some("p"))
                    .appearance
                    .app_appearance,
                want,
                "{text}"
            );
        }
        app.set_display_options(REDUCE);
        assert_eq!(app.app_appearance(), AppAppearance::Dark);
    }

    /// #390 (ADR 0045): `App::blur_radius` is `[appearance] blur_radius`
    /// in effect; a reload that changes it alone tells the observer once
    /// (`on_appearance_changed`, the host's cue to blur its windows
    /// again), one that keeps it tells it nothing, and display options
    /// leave it alone.
    #[test]
    fn a_blur_radius_change_tells_the_observer_once() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(root.path());
        std::fs::write(
            paths.config_file(),
            "[appearance]\nopacity = 0.5\nblur = true\n",
        )
        .unwrap();
        let store = ConfigStore::open(paths.clone()).unwrap();
        let app = App::new((*store.config()).clone(), paths.clone());
        assert_eq!(app.blur_radius(), 0);
        let recorder = Recorder::default();
        app.set_observer(Some(Box::new(recorder.clone())));
        let id = attach_fake_profile(&app, None);
        let before = app.session_appearance(id).unwrap();
        let live = app.live();

        let reload = |text: &str| {
            std::fs::write(paths.config_file(), text).unwrap();
            store.reload().unwrap();
            live.handle(
                &store,
                ConfigEvent::Changed(tessera_config::ConfigChanged {
                    config: store.config(),
                    warnings: Vec::new(),
                }),
            );
        };
        reload("[appearance]\nopacity = 0.5\nblur = true\nblur_radius = 20\n");
        assert_eq!(app.blur_radius(), 20);
        assert_eq!(recorder.take(), [false], "told once");
        assert_eq!(
            app.session_appearance(id).unwrap(),
            before,
            "no session changed"
        );
        reload("[appearance]\nopacity = 0.5\nblur = true\nblur_radius = 20\n# edited\n");
        assert_eq!(recorder.take(), Vec::<bool>::new(), "unchanged");
        reload("[appearance]\nopacity = 0.5\nblur = true\nblur_radius = 900\n");
        assert_eq!(app.blur_radius(), 64, "clamped");
        assert_eq!(recorder.take(), [false]);
        app.set_display_options(REDUCE);
        assert_eq!(app.blur_radius(), 64, "the configured radius");
        recorder.take();
        reload("[appearance]\nopacity = 0.5\nblur = true\n");
        assert_eq!(app.blur_radius(), 0);
        assert_eq!(recorder.take(), [true], "told once, opaque under REDUCE");
        // A profile cannot change it.
        reload(
            "[appearance]\nblur_radius = 8\n[[profiles]]\nname = \"p\"\n\
             [profiles.appearance]\nblur_radius = 30\n",
        );
        assert_eq!(app.blur_radius(), 8);
    }
}
