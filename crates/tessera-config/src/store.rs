//! The configuration store: the single source of the applied configuration.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, RwLock};

use crate::load::{Loaded, Warning, load_file, write_default_if_missing};
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
}

impl ConfigStore {
    /// Opens the store rooted at `paths`.
    ///
    /// Creates the root, `profiles/` and `themes/` directories, writes the
    /// commented default `config.toml` when none exists, then loads it.
    ///
    /// A `config.toml` that cannot be loaded (it does not parse, is not
    /// UTF-8, is unreadable, or is a directory) does not fail the open: the
    /// defaults are applied, the error is logged and kept in
    /// [`ConfigStore::last_error`], and a watcher or [`ConfigStore::reload`]
    /// picks up the repaired file later. A `profiles/` or `themes/`
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
                tracing::error!(
                    target: "tessera_config",
                    path = %file.display(),
                    %error,
                    "config could not be loaded; using defaults until it is fixed"
                );
                store.record_error(error);
            }
        }
        Ok(store)
    }

    /// A store that never touches disk, holding `config` as the applied
    /// value. `paths` is still used by [`ConfigStore::watch`] and
    /// [`ConfigStore::reload`].
    pub fn in_memory(paths: ConfigPaths, config: Config) -> Self {
        Self {
            paths,
            state: Arc::new(RwLock::new(State {
                config: Arc::new(config),
                warnings: Vec::new(),
                theme_warnings: Vec::new(),
                error: None,
                subscribers: Vec::new(),
                next_subscription: 0,
                themes: None,
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
    /// applied), by [`ConfigStore::reload`] and by a [`Watcher`] (the last
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
    /// error is both returned and remembered in [`ConfigStore::last_error`].
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
    /// descriptors, and gains nothing over a single one.
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
    }

    pub(crate) fn load_from_disk(&self) -> Result<Loaded, Error> {
        match load_file(&self.paths.config_file()) {
            Ok(loaded) => Ok(loaded),
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(Loaded {
                config: Config::default(),
                warnings: Vec::new(),
            }),
            Err(e) => Err(e),
        }
    }

    /// Replaces the applied configuration and clears any remembered error.
    ///
    /// Returns `None` when the configuration is unchanged and no error was
    /// pending; `Some` when it changed or when this load recovers from a
    /// previous failure.
    pub(crate) fn apply(&self, loaded: Loaded) -> Option<Arc<Config>> {
        let mut state = self.write();
        let recovered = state.error.take().is_some();
        if !recovered && *state.config == loaded.config && state.warnings == loaded.warnings {
            return None;
        }
        let config = Arc::new(loaded.config);
        state.config = Arc::clone(&config);
        state.warnings = loaded.warnings;
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

    /// Remembers a failed load; the applied configuration is untouched.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::DEFAULT_CONFIG_TOML;

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
        assert_eq!(*store.config(), Config::default());
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

        // Repairing the file to something equal to the applied defaults still
        // counts as a change, so the app can clear its error state.
        std::fs::write(paths.config_file(), "").unwrap();
        let applied = store.reload().unwrap().expect("recovery is reported");
        assert_eq!(*applied, Config::default());
        assert!(store.last_error().is_none());
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
        assert_eq!(*store.config(), Config::default());
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
        assert_eq!(*store.config(), Config::default());
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
                assert_eq!(*store.config(), Config::default());
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
}
