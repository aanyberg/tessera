//! The configuration store: the single source of the applied configuration.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};

use crate::load::{Loaded, Warning, load_file_noting_empty, write_default_if_missing};
use crate::paths::ConfigPaths;
use crate::theme::{ResolvedTheme, Theme};
use crate::watch::{ConfigEvent, ThemesFingerprint, WatchOptions, Watcher};
use crate::{Config, Error};

/// Holds the configuration currently applied to the application and keeps it
/// up to date from disk.
///
/// The store is cheap to clone; every clone observes the same configuration.
/// `config()` returns a snapshot, so callers hold an `Arc<Config>` that stays
/// consistent while a reload replaces the store's current value.
///
/// The store is resilient to a broken file: [`ConfigStore::open`] falls back
/// to [`Config::default()`] when `config.toml` cannot be loaded, and a reload
/// that fails keeps the last good configuration. In both cases
/// [`ConfigStore::last_error`] holds the error until a later load succeeds.
///
/// A failed load fails closed for clipboard writes while the store has
/// never loaded successfully (D8; docs/security.md): `config.toml` exists
/// but cannot be applied at launch, so the user's
/// `terminal.clipboard_write` setting is unknown, and the store holds its
/// configuration with `clipboard_write = false` until a load succeeds.
/// Once a load has succeeded (a missing file counts: it applies the
/// defaults), a failed load keeps the last good configuration unchanged.
/// A `config.toml` symlink whose target is missing is a failed load, not a
/// missing file (#262 item 1). While writes are refused this way, a file
/// that is empty (or only whitespace, after an optional byte order mark)
/// applies the defaults but keeps them
/// refused: it is what a save that truncates before it writes leaves for a
/// moment, so only a load of a non-empty file turns them on (#262 item 9).
#[derive(Debug, Clone)]
pub struct ConfigStore {
    paths: ConfigPaths,
    state: Arc<RwLock<State>>,
}

/// Identifies one [`ConfigStore::subscribe`] call so its channel can be
/// removed again with [`ConfigStore::unsubscribe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubscriptionId(u64);

#[derive(Debug)]
pub(crate) struct State {
    pub(crate) config: Arc<Config>,
    pub(crate) warnings: Vec<Warning>,
    /// Warnings of the most recent [`ConfigStore::theme`] lookup.
    theme_warnings: Vec<Warning>,
    /// Why the most recent load failed; `None` when it succeeded.
    pub(crate) error: Option<Error>,
    /// Whether a load has succeeded on this store ([`ConfigStore::apply`]
    /// ran, other than for an empty file while `refusing`). Until one has,
    /// [`ConfigStore::record_error`] fails closed.
    loaded: bool,
    /// Whether clipboard writes are refused because a load failed before
    /// any succeeded; cleared by the first load of a non-empty file.
    refusing: bool,
    /// Every live [`Watcher`]'s event channel. A change applied by any
    /// watcher thread is sent to all of them, so several watchers on one
    /// store each see every event. A watcher thread removes its own entry
    /// when it exits, so its receiver disconnects rather than waiting for
    /// events that can no longer come.
    subscribers: Vec<(SubscriptionId, Sender<ConfigEvent>)>,
    next_subscription: u64,
    /// The themes directory's state when a watcher last sampled it, `None`
    /// before the first sample; see [`ConfigStore::record_themes`].
    themes: Option<ThemesFingerprint>,
    /// Called after each [`ConfigStore::broadcast`], on the broadcasting
    /// thread: a subscriber that reacts to an event before the watcher
    /// thread goes on, for tests of what the watcher does after reporting.
    #[cfg(test)]
    on_broadcast: Option<fn(&ConfigPaths, &ConfigEvent)>,
    /// Called by [`ConfigStore::apply`] once the new configuration is in
    /// place, on the applying thread with the store unlocked: a reader of
    /// [`ConfigStore::config`] that reacts before the event is sent.
    #[cfg(test)]
    on_apply: Option<fn(&ConfigPaths)>,
}

/// A load of `config.toml` from disk, before it is applied.
#[derive(Debug)]
pub(crate) struct FromDisk {
    pub(crate) loaded: Loaded,
    /// Whether the file held nothing but whitespace, after an optional
    /// byte order mark (a missing file is not
    /// empty: it is a successful load of the defaults).
    pub(crate) empty: bool,
}

impl ConfigStore {
    /// Opens the store rooted at `paths`.
    ///
    /// Creates the root, `profiles/` and `themes/` directories, writes the
    /// commented default `config.toml` when none exists, then loads it.
    ///
    /// A `config.toml` that cannot be loaded (it does not parse, is not
    /// UTF-8, is unreadable, or is a directory) does not fail the open: the
    /// defaults are applied with `terminal.clipboard_write = false` (writes
    /// are refused until the file is fixed, since the file may turn them
    /// off), the error is logged and kept in [`ConfigStore::last_error`],
    /// and a watcher or [`ConfigStore::reload`] applies the repaired file
    /// later. A `profiles/` or `themes/`
    /// directory that cannot be created (a read-only root, say) is logged
    /// and otherwise ignored. Only a failure to create the root or to write
    /// the default file is returned as an error, since without them nothing
    /// can be saved or watched.
    pub fn open(paths: ConfigPaths) -> Result<Self, Error> {
        let file = paths.config_file();
        if write_default_if_missing(&file)? {
            tracing::info!(target: "tessera_config", path = %file.display(), "wrote default config");
        }
        for dir in [paths.profiles_dir(), paths.themes_dir()] {
            if let Err(error) = std::fs::create_dir_all(&dir) {
                tracing::warn!(
                    target: "tessera_config",
                    path = %dir.display(),
                    %error,
                    "could not create config subdirectory"
                );
            }
        }
        let store = Self::in_memory(paths, Config::default());
        match store.load_from_disk() {
            Ok(loaded) => {
                store.apply(loaded);
            }
            Err(error) => {
                // Logged there, once (#262 item 6).
                store.record_error(error);
            }
        }
        Ok(store)
    }

    /// A store that never touches disk, holding `config` as the applied
    /// value. `paths` is still used by [`ConfigStore::watch`] and
    /// [`ConfigStore::reload`].
    ///
    /// `config` does not count as a successful load: if the first load
    /// ([`ConfigStore::reload`] or a watcher's) fails, the store fails
    /// closed and holds `config` with `terminal.clipboard_write = false`,
    /// as [`ConfigStore::open`] does with the defaults.
    pub fn in_memory(paths: ConfigPaths, config: Config) -> Self {
        Self {
            paths,
            state: Arc::new(RwLock::new(State {
                config: Arc::new(config),
                warnings: Vec::new(),
                theme_warnings: Vec::new(),
                error: None,
                loaded: false,
                refusing: false,
                subscribers: Vec::new(),
                next_subscription: 0,
                themes: None,
                #[cfg(test)]
                on_broadcast: None,
                #[cfg(test)]
                on_apply: None,
            })),
        }
    }

    /// Paths this store reads from.
    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    /// Snapshot of the applied configuration.
    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.read().config)
    }

    /// Warnings from the most recent successful load.
    pub fn warnings(&self) -> Vec<Warning> {
        self.read().warnings.clone()
    }

    /// Every warning the store currently knows of: those of the most recent
    /// successful load ([`ConfigStore::warnings`]) followed by those of the
    /// most recent [`ConfigStore::theme`] lookup
    /// ([`Warning::ThemeNotFound`], [`Warning::ThemeInvalid`]).
    pub fn last_warnings(&self) -> Vec<Warning> {
        let state = self.read();
        state
            .warnings
            .iter()
            .chain(&state.theme_warnings)
            .cloned()
            .collect()
    }

    /// The colour theme `appearance.theme` names, resolved now from the
    /// themes directory and the bundled themes (see [`Theme::resolve`]
    /// for the order). Never fails: a name that matches nothing gives
    /// [`Theme::DEFAULT_NAME`]. The lookup's warnings are logged and
    /// replace the previous lookup's in [`ConfigStore::last_warnings`].
    ///
    /// Reads the disk on every call, so call it when the configuration
    /// or the themes change (on [`ConfigEvent::Changed`] and
    /// [`ConfigEvent::ThemesChanged`]), not per frame.
    pub fn theme(&self) -> Theme {
        self.resolve_theme().theme
    }

    /// [`ConfigStore::theme`] with where the theme came from and the
    /// lookup's warnings (also logged and kept for
    /// [`ConfigStore::last_warnings`]), so a caller can tell a theme file
    /// that failed to load ([`Warning::ThemeInvalid`]) from a theme that
    /// changed.
    pub fn resolve_theme(&self) -> ResolvedTheme {
        let name = self.config().appearance.theme.clone();
        let resolved = Theme::resolve(&self.paths.themes_dir(), &name);
        for warning in &resolved.warnings {
            tracing::warn!(target: "tessera_config", "{warning}");
        }
        self.write().theme_warnings.clone_from(&resolved.warnings);
        resolved
    }

    /// Why the most recent load failed, or `None` when it succeeded.
    ///
    /// Set by [`ConfigStore::open`] (the file could not be loaded; defaults are
    /// applied, clipboard writes refused), by [`ConfigStore::reload`] and by a [`Watcher`] (the last
    /// good configuration stays applied). Cleared by the next successful
    /// load, which also reports a `Changed` event or a `Some` reload result
    /// even when the configuration itself did not change, so the application
    /// can clear whatever it shows for the error.
    pub fn last_error(&self) -> Option<Error> {
        self.read().error.clone()
    }

    /// Re-reads `config.toml` now and applies it.
    ///
    /// A missing file applies [`Config::default()`]. Returns the newly
    /// applied configuration when it differs from the current one or when
    /// this load recovered from an earlier failure, `None` when nothing
    /// changed. On a parse error the current configuration is kept and the
    /// error is both returned and remembered in [`ConfigStore::last_error`];
    /// when no load has succeeded on this store yet, the kept configuration
    /// has `terminal.clipboard_write` set to `false` (see [`ConfigStore`]).
    pub fn reload(&self) -> Result<Option<Arc<Config>>, Error> {
        match self.load_from_disk() {
            Ok(loaded) => Ok(self.apply(loaded)),
            Err(error) => {
                self.record_error(error.clone());
                Err(error)
            }
        }
    }

    /// Starts a background watcher on `config.toml` and `themes/` that
    /// applies external edits to this store and reports each one as a
    /// [`ConfigEvent`] (a change in `themes/` as
    /// [`ConfigEvent::ThemesChanged`]).
    ///
    /// Any edit made between [`ConfigStore::open`] and this call is applied
    /// before the watcher returns and reported as its first event.
    ///
    /// **Create one watcher per process** and fan its events out to every
    /// window. Several watchers on the same store each receive every event
    /// too, but each costs a thread, a kernel queue and about seven file
    /// descriptors (up to eight more while the configured theme's file does
    /// not load, and up to eight for the directories holding symbolic links
    /// on the way to the configuration directory), and gains nothing over a
    /// single one.
    pub fn watch(&self, options: WatchOptions) -> Result<Watcher, Error> {
        Watcher::spawn(self.clone(), options)
    }

    /// Registers a new event channel that [`ConfigStore::broadcast`] feeds.
    pub(crate) fn subscribe(&self) -> (SubscriptionId, Receiver<ConfigEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut state = self.write();
        let id = SubscriptionId(state.next_subscription);
        state.next_subscription += 1;
        state.subscribers.push((id, tx));
        (id, rx)
    }

    /// Drops the channel registered under `id`, closing its receiver.
    pub(crate) fn unsubscribe(&self, id: SubscriptionId) {
        self.write().subscribers.retain(|(other, _)| *other != id);
    }

    /// Number of registered event channels (live watcher threads).
    #[cfg(test)]
    pub(crate) fn subscriber_count(&self) -> usize {
        self.read().subscribers.len()
    }

    /// Sends `event` to every live subscriber, forgetting the dead ones.
    pub(crate) fn broadcast(&self, event: &ConfigEvent) {
        self.write()
            .subscribers
            .retain(|(_, tx)| tx.send(event.clone()).is_ok());
        #[cfg(test)]
        {
            let hook = self.read().on_broadcast;
            if let Some(hook) = hook {
                hook(&self.paths, event);
            }
        }
    }

    /// Sets the hook [`ConfigStore::broadcast`] calls after each event.
    #[cfg(test)]
    pub(crate) fn set_on_broadcast(&self, hook: Option<fn(&ConfigPaths, &ConfigEvent)>) {
        self.write().on_broadcast = hook;
    }

    /// Sets the hook [`ConfigStore::apply`] calls once a new configuration
    /// is in place.
    #[cfg(test)]
    pub(crate) fn set_on_apply(&self, hook: Option<fn(&ConfigPaths)>) {
        self.write().on_apply = hook;
    }

    /// Reads `config.toml`. A missing file is the defaults; a symlink whose
    /// target is missing is an error, like a symlink cycle, since the file
    /// the user keeps their settings in is somewhere and could not be read
    /// (#262 item 1). So is a file missing because the configuration
    /// directory, or a directory above it, is such a symlink (GNU stow's
    /// layout, a dotfiles volume that is not mounted; #287).
    ///
    /// The loader's warnings are not logged here: [`ConfigStore::apply`]
    /// logs those the store did not hold already.
    pub(crate) fn load_from_disk(&self) -> Result<FromDisk, Error> {
        let file = self.paths.config_file();
        let loaded =
            tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
                load_file_noting_empty(&file)
            });
        match loaded {
            Ok((loaded, empty)) => Ok(FromDisk { loaded, empty }),
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(&file).is_ok() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "config.toml is a symlink whose target is missing",
                    )
                    .into());
                }
                if behind_dangling_link(&file) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "the configuration directory (or one above it) is a symlink whose target is missing",
                    )
                    .into());
                }
                Ok(FromDisk {
                    loaded: Loaded {
                        config: Config::default(),
                        warnings: Vec::new(),
                    },
                    empty: false,
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Replaces the applied configuration and clears any remembered error.
    ///
    /// While clipboard writes are refused after a failed load, an empty
    /// file is applied with them still refused and does not count as a
    /// successful load (see [`ConfigStore`]).
    ///
    /// Logs each warning of the load that the previous one did not have:
    /// a warning about a line that stays is logged once, not again on
    /// every save that reloads the file (#324 item 9).
    ///
    /// Returns `None` when the configuration is unchanged and no error was
    /// pending; `Some` when it changed or when this load recovers from a
    /// previous failure.
    pub(crate) fn apply(&self, from: FromDisk) -> Option<Arc<Config>> {
        let FromDisk {
            loaded: Loaded {
                mut config,
                warnings,
            },
            empty,
        } = from;
        let mut state = self.write();
        // Warnings are few (the loader bounds them); a list will do.
        let mut known: Vec<&Warning> = state.warnings.iter().collect();
        for warning in &warnings {
            if let Some(at) = known.iter().position(|w| *w == warning) {
                known.swap_remove(at);
            } else {
                tracing::warn!(target: "tessera_config", "{warning}");
            }
        }
        if empty && state.refusing {
            config.terminal.clipboard_write = false;
        } else {
            state.loaded = true;
            state.refusing = false;
        }
        let recovered = state.error.take().is_some();
        if !recovered && *state.config == config && state.warnings == warnings {
            return None;
        }
        let config = Arc::new(config);
        state.config = Arc::clone(&config);
        state.warnings = warnings;
        #[cfg(test)]
        {
            let hook = state.on_apply;
            drop(state);
            if let Some(hook) = hook {
                hook(&self.paths);
            }
        }
        Some(config)
    }

    /// Records the themes directory's state as a watcher sampled it.
    ///
    /// Returns whether it is news: `true` when it differs from the state
    /// recorded before, `false` for the first sample (the baseline) and
    /// when it is unchanged, so two watchers that sample the same change
    /// report it once.
    pub(crate) fn record_themes(&self, themes: ThemesFingerprint) -> bool {
        let mut state = self.write();
        let news = state
            .themes
            .as_ref()
            .is_some_and(|previous| *previous != themes);
        state.themes = Some(themes);
        news
    }

    /// Remembers a failed load and logs it, once per error: a file that
    /// stays broken across several loads (two watchers, a reload and a
    /// watcher) is one warning (#262 item 6). The applied configuration is
    /// untouched, except that while no load has succeeded on this store its
    /// `terminal.clipboard_write` is set to `false` (fail closed; see
    /// [`ConfigStore`]), which the warning says.
    ///
    /// Turning writes off sends no [`ConfigEvent::Changed`]: the caller
    /// reports the error as [`ConfigEvent::Invalid`], and a consumer reads
    /// the setting from [`ConfigStore::config`] when it acts (#262 item 7).
    ///
    /// Returns whether the error is news: `false` when the store already
    /// holds an error with the same text, so two watchers that both load
    /// the same broken file report it to the subscribers once, the way
    /// [`ConfigStore::apply`] reports an unchanged configuration once.
    pub(crate) fn record_error(&self, error: Error) -> bool {
        let mut state = self.write();
        let same = state
            .error
            .as_ref()
            .is_some_and(|previous| previous.to_string() == error.to_string());
        if !state.loaded {
            state.refusing = true;
            if state.config.terminal.clipboard_write {
                let mut config = (*state.config).clone();
                config.terminal.clipboard_write = false;
                state.config = Arc::new(config);
            }
        }
        // The error's text is escaped where it quotes the file (a parse
        // error; S8), and an I/O error's names only the system's reason.
        if !same {
            if state.loaded {
                tracing::warn!(
                    target: "tessera_config",
                    path = %self.paths.config_file().display(),
                    %error,
                    "config.toml could not be loaded; keeping the last good configuration"
                );
            } else {
                tracing::warn!(
                    target: "tessera_config",
                    path = %self.paths.config_file().display(),
                    %error,
                    "config.toml could not be loaded; using the defaults, with clipboard writes (OSC 52) refused until it loads"
                );
            }
        }
        state.error = Some(error);
        !same
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Whether `file` is missing because a directory above it is a symlink
/// whose target is missing (or cannot be followed): the deepest ancestor
/// that exists is such a symlink. `false` when it is a directory, so the
/// file is simply missing. A few `lstat`s, only for a missing file.
fn behind_dangling_link(file: &std::path::Path) -> bool {
    for dir in file.ancestors().skip(1) {
        if dir.as_os_str().is_empty() {
            return false;
        }
        match std::fs::symlink_metadata(dir) {
            Ok(meta) => return meta.is_symlink() && std::fs::metadata(dir).is_err(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::DEFAULT_CONFIG_TOML;

    /// [`Config::default()`] with clipboard writes refused: what a store
    /// holds after its first load failed (D8).
    fn refusing_defaults() -> Config {
        let mut config = Config::default();
        config.terminal.clipboard_write = false;
        config
    }

    /// Files that exist but cannot be applied, by what is wrong with them.
    /// The first five are the ways a user turning clipboard writes off can
    /// get the syntax wrong (#233 item 7); every one is a load error, not a
    /// lenient-key warning.
    const BROKEN: &[(&str, &[u8])] = &[
        ("False", b"[terminal]\nclipboard_write = False\n"),
        ("no", b"[terminal]\nclipboard_write = no\n"),
        ("off", b"[terminal]\nclipboard_write = off\n"),
        ("empty value", b"[terminal]\nclipboard_write =\n"),
        (
            "duplicate key",
            b"[terminal]\nclipboard_write = false\nclipboard_write = false\n",
        ),
        (
            "not UTF-8",
            b"[terminal]\nclipboard_write = false\n# caf\xE9\n",
        ),
        (
            "unrelated syntax error",
            b"[terminal]\nclipboard_write = false\n[appearance\nfont_size = 20\n",
        ),
    ];

    /// D8: a file that exists but cannot be loaded at launch refuses
    /// clipboard writes, through [`ConfigStore::open`] and through the
    /// app-core path (an in-memory store with the defaults, then
    /// [`ConfigStore::reload`]); the error is reported, and the repaired
    /// file applies normally.
    #[test]
    fn a_broken_file_at_launch_refuses_clipboard_writes_until_repaired() {
        for &(what, bytes) in BROKEN {
            for via_reload in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let paths = ConfigPaths::new(dir.path());
                std::fs::write(paths.config_file(), bytes).unwrap();
                let store = if via_reload {
                    let store = ConfigStore::in_memory(paths.clone(), Config::default());
                    assert!(store.reload().is_err(), "{what}: reload reports the error");
                    store
                } else {
                    ConfigStore::open(paths.clone()).unwrap()
                };
                assert!(
                    !store.config().terminal.clipboard_write,
                    "{what} (reload: {via_reload}): writes refused"
                );
                assert_eq!(*store.config(), refusing_defaults(), "{what}");
                assert!(
                    matches!(store.last_error(), Some(Error::Parse(_) | Error::Utf8(_))),
                    "{what}: {:?}",
                    store.last_error()
                );
                // Still refused on a second failure.
                assert!(store.reload().is_err());
                assert!(!store.config().terminal.clipboard_write, "{what}");

                std::fs::write(paths.config_file(), "[appearance]\nfont_size = 20\n").unwrap();
                let applied = store.reload().unwrap().expect("the repair is reported");
                assert!(applied.terminal.clipboard_write, "{what}: repaired");
                assert_eq!(applied.appearance.font_size, 20.0);
                assert!(store.last_error().is_none());
            }
        }
    }

    /// A missing file is a successful load of the defaults: writes on, at
    /// launch and after a later broken file (that is a failed reload, which
    /// keeps the last good configuration).
    #[test]
    fn a_missing_file_allows_clipboard_writes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        let store = ConfigStore::in_memory(paths.clone(), Config::default());
        assert!(store.reload().unwrap().is_none(), "nothing changed");
        assert!(store.config().terminal.clipboard_write);
        assert!(store.last_error().is_none());
        std::fs::write(paths.config_file(), BROKEN[0].1).unwrap();
        assert!(store.reload().is_err());
        assert!(store.config().terminal.clipboard_write, "last good kept");

        // `open` writes the commented default file, which allows writes.
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(ConfigPaths::new(dir.path())).unwrap();
        assert!(store.config().terminal.clipboard_write);
    }

    /// A reload to a broken file keeps the last good value of the switch,
    /// `true` or `false`: only a store that never loaded fails closed.
    #[test]
    fn a_broken_reload_keeps_the_last_good_clipboard_switch() {
        for good in [true, false] {
            for &(what, bytes) in BROKEN {
                let dir = tempfile::tempdir().unwrap();
                let paths = ConfigPaths::new(dir.path());
                std::fs::write(
                    paths.config_file(),
                    format!("[terminal]\nclipboard_write = {good}\n[appearance]\nfont_size = 20\n"),
                )
                .unwrap();
                let store = ConfigStore::open(paths.clone()).unwrap();
                assert_eq!(store.config().terminal.clipboard_write, good);
                std::fs::write(paths.config_file(), bytes).unwrap();
                assert!(store.reload().is_err(), "{what}");
                assert_eq!(store.config().terminal.clipboard_write, good, "{what}");
                assert_eq!(store.config().appearance.font_size, 20.0, "{what}");
                assert!(store.last_error().is_some());
            }
        }
    }

    /// A watcher's first load fails closed too when nothing loaded before
    /// it (an in-memory store watched without a reload).
    #[test]
    fn a_watchers_first_failed_load_refuses_clipboard_writes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        std::fs::write(paths.config_file(), BROKEN[1].1).unwrap();
        let store = ConfigStore::in_memory(paths, Config::default());
        let _watcher = store
            .watch(WatchOptions {
                backend: crate::WatchBackend::Poll,
                ..Default::default()
            })
            .unwrap();
        assert!(store.last_error().is_some());
        assert!(!store.config().terminal.clipboard_write);
    }

    #[test]
    fn open_writes_defaults_and_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path().join("Tessera"));
        let store = ConfigStore::open(paths.clone()).unwrap();
        assert_eq!(*store.config(), Config::default());
        assert_eq!(store.warnings(), Vec::new());
        assert!(store.last_error().is_none());
        assert_eq!(
            std::fs::read_to_string(paths.config_file()).unwrap(),
            DEFAULT_CONFIG_TOML
        );
        assert!(paths.profiles_dir().is_dir());
        assert!(paths.themes_dir().is_dir());
    }

    #[test]
    fn open_keeps_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        std::fs::write(
            paths.config_file(),
            "[appearance]\nfont_size = 20\nbogus = 1\n",
        )
        .unwrap();
        let store = ConfigStore::open(paths).unwrap();
        assert_eq!(store.config().appearance.font_size, 20.0);
        assert_eq!(store.warnings().len(), 1);
    }

    #[test]
    fn open_with_broken_file_starts_with_defaults_and_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        std::fs::write(paths.config_file(), "[appearance\n").unwrap();

        let store = ConfigStore::open(paths.clone()).unwrap();
        assert_eq!(*store.config(), refusing_defaults());
        assert!(
            matches!(store.last_error(), Some(Error::Parse(_))),
            "{:?}",
            store.last_error()
        );
        assert_eq!(
            std::fs::read_to_string(paths.config_file()).unwrap(),
            "[appearance\n",
            "the broken file is left for the user to fix"
        );

        // Emptying the file clears the error (reported, so the app can
        // clear its error state) but keeps clipboard writes refused: an
        // empty file is what a truncating save leaves for a moment (#262
        // item 9).
        std::fs::write(paths.config_file(), "").unwrap();
        let applied = store.reload().unwrap().expect("recovery is reported");
        assert_eq!(*applied, refusing_defaults());
        assert!(store.last_error().is_none());
        assert!(store.reload().unwrap().is_none(), "and only once");

        // Repairing the file to something equal to the defaults turns them
        // on.
        std::fs::write(paths.config_file(), "# nothing set\n").unwrap();
        let applied = store.reload().unwrap().expect("repair is reported");
        assert_eq!(*applied, Config::default());
        assert!(store.reload().unwrap().is_none(), "and only once");
    }

    #[test]
    fn open_with_non_utf8_file_starts_with_defaults_and_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        std::fs::write(
            paths.config_file(),
            b"[appearance]\nfont_family = \"Men\xE9lo\"\n",
        )
        .unwrap();
        let store = ConfigStore::open(paths).unwrap();
        assert_eq!(*store.config(), refusing_defaults());
        assert!(
            matches!(store.last_error(), Some(Error::Utf8(_))),
            "{:?}",
            store.last_error()
        );
    }

    #[test]
    fn open_with_unloadable_file_starts_with_defaults_and_reports_error() {
        // A directory where the file should be.
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        std::fs::create_dir(paths.config_file()).unwrap();
        let store = ConfigStore::open(paths).unwrap();
        assert_eq!(*store.config(), refusing_defaults());
        assert!(
            matches!(store.last_error(), Some(Error::Io(_))),
            "{:?}",
            store.last_error()
        );

        // An unreadable file (skipped when running as root, who can read it).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = tempfile::tempdir().unwrap();
            let paths = ConfigPaths::new(dir.path());
            std::fs::write(paths.config_file(), "[appearance]\nfont_size = 20\n").unwrap();
            std::fs::set_permissions(paths.config_file(), std::fs::Permissions::from_mode(0o000))
                .unwrap();
            if std::fs::read(paths.config_file()).is_err() {
                let store = ConfigStore::open(paths.clone()).unwrap();
                assert_eq!(*store.config(), refusing_defaults());
                assert!(matches!(store.last_error(), Some(Error::Io(_))));
                std::fs::set_permissions(
                    paths.config_file(),
                    std::fs::Permissions::from_mode(0o644),
                )
                .unwrap();
                let applied = store.reload().unwrap().expect("recovery is reported");
                assert_eq!(applied.appearance.font_size, 20.0);
                assert!(store.last_error().is_none());
            }
        }
    }

    /// A read-only root with a valid `config.toml` must not prevent launch
    /// even though `profiles/` and `themes/` cannot be created (ADR 0004).
    /// Skipped when running as root, who can write anywhere.
    #[cfg(unix)]
    #[test]
    fn open_tolerates_read_only_root() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        std::fs::write(paths.config_file(), "[appearance]\nfont_size = 20\n").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let read_only = std::fs::create_dir(dir.path().join("probe")).is_err();
        let result = ConfigStore::open(paths.clone());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        if read_only {
            let store = result.unwrap();
            assert_eq!(store.config().appearance.font_size, 20.0);
            assert!(store.last_error().is_none());
            assert!(!paths.profiles_dir().exists());
        }
    }

    #[test]
    fn same_error_is_recorded_but_not_news_twice() {
        let paths = ConfigPaths::new("/nonexistent");
        let store = ConfigStore::in_memory(paths, Config::default());
        let err = Config::from_toml_lenient("[appearance\n").unwrap_err();
        assert!(store.record_error(err.clone()));
        assert!(!store.record_error(err.clone()));
        let other = Config::from_toml_lenient("[x\n").unwrap_err();
        assert!(store.record_error(other));
        assert!(store.record_error(err), "a different error in between");
    }

    #[test]
    fn watcher_thread_removes_its_channel_when_it_exits() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(ConfigPaths::new(dir.path())).unwrap();
        assert_eq!(store.subscriber_count(), 0);
        let watcher = store
            .watch(WatchOptions {
                backend: crate::WatchBackend::Poll,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(store.subscriber_count(), 1);
        drop(watcher);
        assert_eq!(
            store.subscriber_count(),
            0,
            "the thread unsubscribed itself"
        );
    }

    #[test]
    fn open_fails_on_unreadable_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let err = ConfigStore::open(ConfigPaths::new(file)).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "{err}");
    }

    #[test]
    fn reload_applies_changes_and_keeps_last_good_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        let store = ConfigStore::open(paths.clone()).unwrap();
        let before = store.config();
        assert!(store.reload().unwrap().is_none());

        std::fs::write(paths.config_file(), "[appearance]\nfont_size = 21\n").unwrap();
        let after = store.reload().unwrap().expect("changed");
        assert_eq!(after.appearance.font_size, 21.0);
        assert_eq!(before.appearance.font_size, 13.0, "snapshot is unaffected");

        std::fs::write(paths.config_file(), "[appearance\n").unwrap();
        assert!(store.reload().is_err());
        assert!(store.last_error().is_some());
        assert_eq!(store.config().appearance.font_size, 21.0);

        std::fs::remove_file(paths.config_file()).unwrap();
        assert_eq!(
            *store.reload().unwrap().expect("changed"),
            Config::default()
        );
        assert!(store.last_error().is_none());
    }

    /// What a subscriber records of an event at `WARN` or above: every
    /// field, `name=value`, the values as they would be formatted.
    struct Capture(std::sync::Mutex<Vec<String>>);

    struct Fields<'a>(&'a mut String);

    use std::fmt::Write as _;

    impl tracing::field::Visit for Fields<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            let _ = write!(self.0, "{}={value} ", field.name());
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }

    impl tracing::Subscriber for Capture {
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
            if *event.metadata().level() > tracing::Level::WARN {
                return;
            }
            let mut line = String::new();
            event.record(&mut Fields(&mut line));
            self.0.lock().unwrap().push(line);
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// The warnings and errors `f` logs on this thread.
    fn logged_warnings(f: impl FnOnce()) -> Vec<String> {
        let capture = std::sync::Arc::new(Capture(std::sync::Mutex::new(Vec::new())));
        tracing::subscriber::with_default(std::sync::Arc::clone(&capture), f);
        capture.0.lock().unwrap().clone()
    }

    /// #262 item 6: a broken `config.toml` at open is one warning, which
    /// says writes are refused and why, and loading the same broken file
    /// again logs nothing more.
    #[test]
    fn a_broken_file_at_open_is_one_warning() {
        for (case, bytes) in BROKEN {
            let dir = tempfile::tempdir().unwrap();
            let paths = ConfigPaths::new(dir.path());
            std::fs::write(paths.config_file(), bytes).unwrap();
            let mut store = None;
            let lines = logged_warnings(|| store = Some(ConfigStore::open(paths.clone()).unwrap()));
            let [line] = &lines[..] else {
                panic!("{case}: {lines:#?}");
            };
            assert!(
                line.contains("clipboard writes (OSC 52) refused"),
                "{case}: {line}"
            );
            let error = store.as_ref().unwrap().last_error().unwrap().to_string();
            assert!(line.contains(&error), "{case}: {line}");
            let again = logged_warnings(|| {
                assert!(store.as_ref().unwrap().reload().is_err());
            });
            assert_eq!(again, Vec::<String>::new(), "{case}");
        }
    }

    /// #324 item 9: a load warning is logged when the store first sees
    /// it, not again on each reload of a file that still has it; a new
    /// one is logged when it appears, and again if it comes back.
    #[test]
    fn a_load_warning_is_logged_once_per_appearance() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        let file = paths.config_file();
        std::fs::write(&file, "[general]\nbogus = 1\n").unwrap();
        let mut store = None;
        let lines = logged_warnings(|| store = Some(ConfigStore::open(paths.clone()).unwrap()));
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert!(lines[0].contains("general.bogus"), "{lines:#?}");
        let store = store.unwrap();
        let reload = |text: &str| {
            std::fs::write(&file, text).unwrap();
            logged_warnings(|| drop(store.reload().unwrap()))
        };
        for i in 0..20 {
            let lines = reload(&format!(
                "[general]\nbogus = 1\n[terminal]\nscrollback_lines = {i}\n"
            ));
            assert_eq!(lines, Vec::<String>::new(), "reload {i}");
        }
        let lines = reload("[general]\nbogus = 1\nother = 2\n");
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert!(lines[0].contains("general.other"), "{lines:#?}");
        assert_eq!(reload("[general]\n"), Vec::<String>::new());
        let lines = reload("[general]\nbogus = 1\n");
        assert_eq!(lines.len(), 1, "it came back: {lines:#?}");
    }

    /// #262 items 4 and 10 at the log: a parse error quoting a line with a
    /// raw ESC, a line separator and a right-to-left override reaches the
    /// warning escaped, on one line.
    #[test]
    fn a_parse_error_is_logged_escaped() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        std::fs::write(
            paths.config_file(),
            "[terminal]\nclipboard_write = off # \u{1b}]52;c;x\u{7}\u{2028}\u{202e}\n",
        )
        .unwrap();
        let lines = logged_warnings(|| drop(ConfigStore::open(paths.clone()).unwrap()));
        let [line] = &lines[..] else {
            panic!("{lines:#?}");
        };
        assert!(
            !line.chars().any(|c| c.is_control()
                || matches!(c, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}')),
            "{line:?}"
        );
        assert!(
            line.contains("\\u{1b}]52") && line.contains("\\u{202e}"),
            "{line}"
        );
    }

    /// #262 item 1: a `config.toml` symlink whose target is missing fails
    /// closed at launch, like a symlink cycle, rather than counting as a
    /// missing file; no default file is written through the link, and a
    /// target that appears later applies normally.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_at_launch_refuses_clipboard_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Tessera");
        std::fs::create_dir(&root).unwrap();
        let target = dir.path().join("dotfiles").join("tessera.toml");
        std::os::unix::fs::symlink(&target, root.join("config.toml")).unwrap();
        let store = ConfigStore::open(ConfigPaths::new(&root)).unwrap();
        assert_eq!(*store.config(), refusing_defaults());
        assert!(
            matches!(store.last_error(), Some(Error::Io(_))),
            "{:?}",
            store.last_error()
        );
        assert!(!target.exists(), "nothing written through the link");
        assert!(store.reload().is_err(), "still dangling");
        assert_eq!(*store.config(), refusing_defaults());

        std::fs::create_dir(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "[appearance]\nfont_size = 15\n").unwrap();
        let applied = store.reload().unwrap().expect("the target appeared");
        assert_eq!(applied.appearance.font_size, 15.0);
        assert!(applied.terminal.clipboard_write);
    }

    /// #287 (M1): a configuration directory that is a symlink whose target
    /// is missing (GNU stow's layout with the checkout gone, a dotfiles
    /// volume that is not mounted), or that sits below one, fails closed
    /// like a dangling `config.toml` (#262 item 1), for an absolute and a
    /// relative link; once the target directory appears the usual rules
    /// apply (no file: the defaults, writes on).
    #[cfg(unix)]
    #[test]
    fn a_dangling_config_directory_refuses_clipboard_writes() {
        for (what, link, target, root) in [
            ("absolute", "Tessera", "volume/tessera", "Tessera"),
            ("relative", "Tessera", "./volume/tessera", "Tessera"),
            ("above the root", "dotfiles", "volume", "dotfiles/tessera"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let target_dir = if target.starts_with("./") {
                std::os::unix::fs::symlink(target, dir.path().join(link)).unwrap();
                dir.path().join(target)
            } else {
                let target_dir = dir.path().join(target);
                std::os::unix::fs::symlink(&target_dir, dir.path().join(link)).unwrap();
                target_dir
            };
            let paths = ConfigPaths::new(dir.path().join(root));
            // How the application starts (`App::with_config_dir`).
            let store = ConfigStore::in_memory(paths.clone(), Config::default());
            assert!(store.reload().is_err(), "{what}");
            assert_eq!(*store.config(), refusing_defaults(), "{what}");
            assert!(store.last_error().is_some(), "{what}");

            std::fs::create_dir_all(&target_dir).unwrap();
            std::fs::create_dir_all(paths.root()).unwrap();
            let applied = store.reload().unwrap().expect("recovered");
            assert_eq!(*applied, Config::default(), "{what}: no file, writes on");
            assert!(store.last_error().is_none(), "{what}");
        }

        // A real directory with no `config.toml` is still the defaults.
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::in_memory(
            ConfigPaths::new(dir.path().join("Tessera")),
            Config::default(),
        );
        assert_eq!(store.reload().unwrap(), None);
        assert!(store.config().terminal.clipboard_write);
    }

    /// #262 item 9: while writes are refused after a failed load, an empty
    /// or whitespace-only file (after an optional byte order mark, #287)
    /// keeps them refused, however often it loads;
    /// only a non-empty file turns them on. Without a failure first, an
    /// empty file is the defaults with writes on.
    #[test]
    fn an_empty_file_turns_clipboard_writes_on_only_without_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        std::fs::write(paths.config_file(), "").unwrap();
        let store = ConfigStore::open(paths.clone()).unwrap();
        assert_eq!(*store.config(), Config::default(), "a clean launch");

        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        std::fs::write(paths.config_file(), "[terminal\n").unwrap();
        let store = ConfigStore::open(paths.clone()).unwrap();
        // A byte order mark alone loads as an empty document too (#287).
        for empty in ["", " \n\t\r\n", "\u{feff}", "\u{feff} \r\n", ""] {
            std::fs::write(paths.config_file(), empty).unwrap();
            let _ = store.reload().unwrap();
            assert_eq!(*store.config(), refusing_defaults(), "{empty:?}");
            assert!(store.last_error().is_none());
        }
        // Broken again: still refused (the store never loaded).
        std::fs::write(paths.config_file(), "[terminal\n").unwrap();
        assert!(store.reload().is_err());
        assert_eq!(*store.config(), refusing_defaults());
        std::fs::write(paths.config_file(), "[terminal]\n").unwrap();
        assert_eq!(*store.reload().unwrap().unwrap(), Config::default());
        // Once a non-empty file loaded, an empty one is just the defaults.
        std::fs::write(paths.config_file(), "").unwrap();
        assert_eq!(store.reload().unwrap(), None);
        assert_eq!(*store.config(), Config::default());
    }
}
