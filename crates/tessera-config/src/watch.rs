//! Live reload: watches `config.toml` and the `themes/` directory and applies
//! external edits.
//!
//! Two backends share one debounce-and-apply path (ADR 0006):
//!
//! - [`WatchBackend::Native`] registers the configuration directory and the
//!   file with `kqueue(2)` on macOS and the BSDs. The watcher thread sleeps
//!   in the kernel until something changes, so an idle application never
//!   wakes up. The directory is watched as well as the file so an editor
//!   that saves by writing a temporary file and renaming it over
//!   `config.toml` is seen, and a deleted or recreated file is followed.
//!   When `config.toml` is a symlink (into a dotfiles checkout, say) the
//!   directory holding the link's target is registered too, so a target
//!   that is deleted and recreated by `git checkout` or `rm` plus rewrite is
//!   followed as well. If the configuration directory or that target
//!   directory is removed, or `config.toml` exists but cannot be opened
//!   (permission denied, a symlink cycle), the backend polls at
//!   [`WatchOptions::poll_interval`] until it can register everything
//!   again, re-reads the file, and goes back to sleeping on kernel events.
//!   It also registers the `themes/` directory (a theme file created,
//!   deleted or renamed, an editor's rename-save) and the theme file
//!   `appearance.theme` currently resolves to (an in-place write to it),
//!   re-registered whenever the resolved file changes. A theme file that
//!   stops loading (a half-written save) stays registered while it exists,
//!   so the save that fixes it is seen. An in-place write to any *other*
//!   theme file is not seen: it cannot change the theme in use.
//! - [`WatchBackend::Poll`] samples the metadata of the file, of `themes/`
//!   and of the theme file in use every [`WatchOptions::poll_interval`],
//!   and of every `*.toml` file in `themes/` every twentieth interval (about
//!   once a second by default). It is the fallback on platforms without
//!   `kqueue` and stays available everywhere for tests.
//!
//! Either way the files must be quiet for [`WatchOptions::debounce`] before
//! they are re-read, so rapid successive writes coalesce into one event; a
//! writer that never goes quiet is read anyway after
//! [`WatchOptions::max_wait`]. A change to `config.toml` is reported as
//! [`ConfigEvent::Changed`] (or [`ConfigEvent::Invalid`]); a change in
//! `themes/` as [`ConfigEvent::ThemesChanged`], after which the application
//! resolves the theme again ([`ConfigStore::theme`]).
//!
//! Events are applied to the [`ConfigStore`] and then sent to every watcher
//! of that store, so one change is reported once to each of them. If the
//! watcher thread ever dies unexpectedly its channel is closed, so a
//! consumer sees [`RecvTimeoutError::Disconnected`] rather than silence,
//! and the failure is logged at error level.
//!
//! **Create one watcher per process.** Each one costs a thread plus, on
//! the native backend, a kernel queue and about seven file descriptors (the
//! queue, the stop pipe's two ends, the directory, the file, `themes/` and
//! the theme file in use). The
//! application should hold a single [`Watcher`] and fan its events out to
//! every window rather than watching once per window.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crate::load::Warning;
use crate::store::ConfigStore;
use crate::theme::{MAX_THEME_DIR_ENTRIES, Theme, ThemeSource};
use crate::{Config, Error};

/// Lower bound for [`WatchOptions::poll_interval`]; smaller values are
/// raised to it so the polling backend can never spin.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Upper bound for every duration in [`WatchOptions`]; larger values
/// (`Duration::MAX`, say) are lowered to it. An hour is far beyond any
/// useful setting and keeps every deadline and kernel timeout the watcher
/// computes representable, so no option can make the thread panic or sleep
/// forever.
pub const MAX_WATCH_DURATION: Duration = Duration::from_hours(1);

/// How a [`Watcher`] learns that `config.toml` changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum WatchBackend {
    /// Kernel file events (`kqueue(2)`); no wake-ups while idle. On a
    /// platform without `kqueue` this falls back to [`WatchBackend::Poll`]
    /// with a logged warning.
    #[default]
    Native,
    /// Sample the file's metadata every [`WatchOptions::poll_interval`].
    ///
    /// A change is detected from the metadata alone: modification time,
    /// size and, on Unix, inode number and change time. A rewrite of the
    /// same length that lands within the file system's timestamp
    /// granularity (1-2 s on SMB or `ExFAT` volumes, say) and reuses the
    /// inode is therefore missed. APFS and HFS+ timestamps are fine-grained
    /// enough, and an editor's rename-save changes the inode, so this only
    /// matters for in-place rewrites on coarse volumes.
    Poll,
}

/// Tuning for a [`Watcher`].
#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Which backend detects changes.
    pub backend: WatchBackend,
    /// How often the files' metadata is sampled by [`WatchBackend::Poll`],
    /// and how often [`WatchBackend::Native`] looks for the configuration
    /// directory while it is missing. Values below [`MIN_POLL_INTERVAL`]
    /// are raised to it and values above [`MAX_WATCH_DURATION`] lowered.
    pub poll_interval: Duration,
    /// How long the file must be unchanged before it is re-read and applied.
    /// Values above [`MAX_WATCH_DURATION`] are lowered to it.
    pub debounce: Duration,
    /// Upper bound on how long a change can be held back by further
    /// changes: once the file has been changing for this long without a
    /// quiet period of `debounce`, it is re-read anyway. Values below
    /// `debounce` are raised to it and values above [`MAX_WATCH_DURATION`]
    /// lowered. The forced read may see a document that
    /// a slow, non-atomic writer has not finished, in which case an
    /// [`Invalid`](ConfigEvent::Invalid) event precedes the eventual
    /// [`Changed`](ConfigEvent::Changed) one.
    pub max_wait: Duration,
    /// Runs first on the watcher's thread, for thread attributes the
    /// caller sets on every thread it owns (`tessera-app-core` passes its
    /// thread start hook, which gives threads a quality-of-service class on
    /// macOS). `None` by default. A hook that panics is logged and the
    /// watcher runs on.
    pub on_thread_start: Option<fn()>,
}

impl PartialEq for WatchOptions {
    /// Field by field; the hooks by address ([`std::ptr::fn_addr_eq`]).
    fn eq(&self, other: &Self) -> bool {
        let hooks = match (self.on_thread_start, other.on_thread_start) {
            (Some(a), Some(b)) => std::ptr::fn_addr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        self.backend == other.backend
            && self.poll_interval == other.poll_interval
            && self.debounce == other.debounce
            && self.max_wait == other.max_wait
            && hooks
    }
}

impl Eq for WatchOptions {}

impl Default for WatchOptions {
    /// Native events, 50 ms polling as the fallback, 100 ms debounce (an edit
    /// lands in well under 500 ms with either backend) and a 1 s cap on how
    /// long a stream of writes can postpone a reload.
    fn default() -> Self {
        Self {
            backend: WatchBackend::default(),
            poll_interval: Duration::from_millis(50),
            debounce: Duration::from_millis(100),
            max_wait: Duration::from_secs(1),
            on_thread_start: None,
        }
    }
}

impl WatchOptions {
    /// The options with every duration inside its documented bounds.
    fn clamped(mut self) -> Self {
        self.poll_interval = self
            .poll_interval
            .clamp(MIN_POLL_INTERVAL, MAX_WATCH_DURATION);
        self.debounce = self.debounce.min(MAX_WATCH_DURATION);
        self.max_wait = self.max_wait.clamp(self.debounce, MAX_WATCH_DURATION);
        self
    }
}

/// The configuration on disk changed and was applied to the store.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigChanged {
    /// The newly applied configuration (the same `Arc` the store now holds).
    pub config: Arc<Config>,
    /// Warnings raised while loading it.
    pub warnings: Vec<Warning>,
}

/// What a [`Watcher`] reports.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ConfigEvent {
    /// The file changed and the new configuration is applied. Also sent when
    /// a load succeeds after an [`Invalid`](ConfigEvent::Invalid) one, even
    /// if the resulting configuration is unchanged, so the application can
    /// clear whatever it shows for the error.
    Changed(ConfigChanged),
    /// The file changed but could not be loaded; the previous configuration
    /// stays applied until the file is fixed. The same error is available
    /// from [`ConfigStore::last_error`].
    ///
    /// A writer that truncates the file and then fills it slowly (slower
    /// than [`WatchOptions::debounce`]) is seen as an invalid document
    /// followed by the finished one, so an `Invalid` is often followed by a
    /// `Changed` within a debounce period or so. An application that shows
    /// errors prominently may want to delay the display by that much and
    /// drop it when the `Changed` arrives.
    Invalid(Error),
    /// A `*.toml` file in the themes directory was created, deleted,
    /// renamed or rewritten (or the directory itself was), so
    /// [`ConfigStore::theme`] may now resolve `appearance.theme` to other
    /// colours. The watcher does not resolve the theme itself: the
    /// application calls [`ConfigStore::theme`] and applies the result if
    /// it differs from the theme in use, since a change to a theme file
    /// that is not in use leaves it the same.
    ///
    /// Sent at most once per settled change (debounced like `config.toml`),
    /// and independently of [`Changed`](ConfigEvent::Changed): an edit of
    /// `config.toml` alone never produces it, and an edit in `themes/`
    /// alone never produces a `Changed`. The native backend sees an
    /// in-place write only to the theme file in use (see the module
    /// documentation); the polling backend sees writes to every theme file,
    /// to one not in use within about twenty polling intervals.
    ThemesChanged,
}

/// A background watcher over the store's `config.toml` and `themes/`.
///
/// Hold **one per process** and fan its events out to every window: each
/// watcher costs a thread and, natively, a kernel queue and about seven
/// file descriptors. Several watchers on one store do work (each sees every
/// event once) but gain nothing over a single one.
///
/// Dropping the watcher stops the thread. Events are delivered on a channel
/// so any frontend loop can consume them: block with [`Watcher::recv_timeout`],
/// poll with [`Watcher::try_recv`], or take the receiver with
/// [`Watcher::receiver`] and select on it.
///
/// The channel closes when the watcher thread exits, so a receive that
/// fails with [`RecvTimeoutError::Disconnected`] (or
/// [`TryRecvError::Disconnected`]) while the `Watcher` is still alive means
/// the thread died; the cause is logged at error level. A terminal emulator
/// can keep running on the last applied configuration and offer a restart
/// of the watcher via [`ConfigStore::watch`].
#[derive(Debug)]
pub struct Watcher {
    events: Receiver<ConfigEvent>,
    guard: WatchGuard,
}

/// Keeps a [`Watcher`]'s thread running; dropping it stops the thread and
/// waits for it to exit (it wakes at once), which closes the event
/// channel.
///
/// Obtained from [`Watcher::into_parts`], for a consumer that blocks on the
/// receiver on a thread of its own: dropping the guard elsewhere ends that
/// thread's `recv` with a disconnection instead of leaving it waiting.
#[derive(Debug)]
pub struct WatchGuard {
    stop: Option<Stop>,
    thread: Option<JoinHandle<()>>,
}

/// Lives on the watcher thread for as long as it runs. Dropping it (on a
/// normal return or while unwinding from a panic) removes the watcher's
/// channel from the store, so the [`Watcher`]'s receiver disconnects
/// instead of waiting forever for events that can no longer come; an
/// unexpected exit is logged at error level.
struct ThreadGuard {
    store: ConfigStore,
    subscription: crate::store::SubscriptionId,
    finished: bool,
}

impl ThreadGuard {
    /// Marks the thread as running or finished; the `Drop` reads the flag.
    fn set_finished(&mut self, finished: bool) {
        self.finished = finished;
    }
}

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        if !self.finished {
            tracing::error!(
                target: "tessera_config",
                "config watcher thread exited unexpectedly; live reload is off until the watcher is recreated"
            );
        }
        self.store.unsubscribe(self.subscription);
    }
}

/// Dropping this wakes the watcher thread and makes it return; the fields
/// exist only for their `Drop`.
#[derive(Debug)]
enum Stop {
    Channel {
        _tx: Sender<()>,
    },
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    Pipe {
        _tx: std::io::PipeWriter,
    },
}

impl Watcher {
    pub(crate) fn spawn(store: ConfigStore, options: WatchOptions) -> Result<Self, Error> {
        let options = options.clamped();
        let (subscription, events) = store.subscribe();
        let (backend, stop) = match Backend::new(&store, options.backend) {
            Ok(backend) => backend,
            Err(error) => {
                store.unsubscribe(subscription);
                return Err(error);
            }
        };
        // The backend is armed, so from here on every edit produces an
        // event. Catch up on anything written between `ConfigStore::open`
        // and now; an edit that is seen both here and by the backend loads
        // twice but is reported once, since the second load finds no change.
        // The first watcher of a store records the themes directory's state
        // as its baseline here; a later one reports what changed since.
        settle(&store);
        // `finished` starts true so a closure that is dropped unrun (the
        // spawn failed) unsubscribes without logging a spurious crash.
        let mut guard = ThreadGuard {
            store: store.clone(),
            subscription,
            finished: true,
        };
        let thread = std::thread::Builder::new()
            .name("tessera-config-watch".into())
            .spawn(move || {
                if let Some(hook) = options.on_thread_start
                    && std::panic::catch_unwind(hook).is_err()
                {
                    tracing::error!(target: "tessera_config", "the watcher's thread start hook panicked");
                }
                guard.set_finished(false);
                backend.run(&store, &options);
                guard.set_finished(true);
            })?;
        Ok(Self {
            events,
            guard: WatchGuard {
                stop: Some(stop),
                thread: Some(thread),
            },
        })
    }

    /// Splits the watcher into its event channel and the guard that keeps
    /// its thread running. Once the guard is dropped (the thread stopped),
    /// the receiver reports [`RecvTimeoutError::Disconnected`] after the
    /// events already queued.
    #[must_use]
    pub fn into_parts(self) -> (Receiver<ConfigEvent>, WatchGuard) {
        (self.events, self.guard)
    }

    /// The event channel; `Err` once the watcher thread has exited.
    pub fn receiver(&self) -> &Receiver<ConfigEvent> {
        &self.events
    }

    /// Returns the next event without blocking.
    pub fn try_recv(&self) -> Result<ConfigEvent, TryRecvError> {
        self.events.try_recv()
    }

    /// Waits up to `timeout` for the next event.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<ConfigEvent, RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        // Closing the stop handle wakes the thread immediately.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Re-reads `config.toml` and the themes directory's state and reports what
/// changed to every watcher of the store ([`reload_and_report`], then
/// [`report_themes`]).
fn settle(store: &ConfigStore) {
    reload_and_report(store);
    report_themes(store);
}

/// Samples the themes directory and sends [`ConfigEvent::ThemesChanged`]
/// to every watcher of the store when it differs from the state the store
/// last recorded. The store keeps that state, so several watchers of one
/// store report one change once, as with [`reload_and_report`]; the first
/// sample of a store records a baseline and reports nothing.
fn report_themes(store: &ConfigStore) {
    let themes = ThemesFingerprint::of(&store.paths().themes_dir());
    if store.record_themes(themes) {
        store.broadcast(&ConfigEvent::ThemesChanged);
    }
}

/// The theme file `appearance.theme` resolves to now, or `None` when it
/// resolves to a bundled theme or the default.
///
/// A theme file that is there but does not load (an editor's half-written
/// save, a typo) is still the one to follow, or the save that fixes it would
/// go unseen: a file whose stem matches the name, else `previous` (the file
/// followed so far, which may match by its `name` key) while it exists.
fn active_theme_file(store: &ConfigStore, previous: Option<&Path>) -> Option<std::path::PathBuf> {
    let name = store.config().appearance.theme.clone();
    let themes = store.paths().themes_dir();
    match Theme::resolve(&themes, &name).source {
        ThemeSource::User(path) => Some(path),
        ThemeSource::Bundled | ThemeSource::Default => crate::theme::files_named(&themes, &name)
            .into_iter()
            .next()
            .or_else(|| {
                previous
                    .filter(|path| path.is_file())
                    .map(Path::to_path_buf)
            }),
    }
}

/// Re-reads the file, applies it and reports the outcome to every watcher of
/// the store. Nothing is sent when the file is unchanged.
fn reload_and_report(store: &ConfigStore) {
    let event = match store.load_from_disk() {
        Ok(loaded) => {
            let warnings = loaded.warnings.clone();
            store
                .apply(loaded)
                .map(|config| ConfigEvent::Changed(ConfigChanged { config, warnings }))
        }
        Err(error) => {
            tracing::warn!(target: "tessera_config", %error, "config changed but failed to load");
            // Two watchers of one store both load the same broken file; only
            // the first to record the error reports it, as with `apply`.
            store
                .record_error(error.clone())
                .then_some(ConfigEvent::Invalid(error))
        }
    };
    if let Some(event) = event {
        store.broadcast(&event);
    }
}

enum Backend {
    Poll {
        stop: Receiver<()>,
        baseline: PollSample,
    },
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    Native(native::Native),
}

impl Backend {
    fn new(store: &ConfigStore, backend: WatchBackend) -> Result<(Self, Stop), Error> {
        match backend {
            WatchBackend::Native => {
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "netbsd",
                    target_os = "dragonfly"
                ))]
                {
                    let (native, stop) =
                        native::Native::open(store.paths(), active_theme_file(store, None))?;
                    Ok((Self::Native(native), Stop::Pipe { _tx: stop }))
                }
                #[cfg(not(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "netbsd",
                    target_os = "dragonfly"
                )))]
                {
                    tracing::warn!(
                        target: "tessera_config",
                        "native file watching is unavailable on this platform; polling instead"
                    );
                    Ok(Self::poll(store))
                }
            }
            WatchBackend::Poll => Ok(Self::poll(store)),
        }
    }

    fn poll(store: &ConfigStore) -> (Self, Stop) {
        let (stop_tx, stop) = mpsc::channel();
        // Fingerprint first, reload (in `spawn`) second: an edit in between
        // is applied by the reload and seen as a change on the first poll,
        // where the second load finds nothing new.
        let baseline = PollSample::of(store, active_theme_file(store, None));
        (
            Self::Poll { stop, baseline },
            Stop::Channel { _tx: stop_tx },
        )
    }

    fn run(self, store: &ConfigStore, options: &WatchOptions) {
        match self {
            Self::Poll { stop, baseline } => run_polling(store, options, baseline, &stop),
            #[cfg(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "dragonfly"
            ))]
            Self::Native(native) => native.run(store, options),
        }
    }
}

/// A cheap identity of the file's on-disk state, from metadata alone; see
/// [`WatchBackend::Poll`] for what it cannot tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fingerprint {
    Missing,
    Present {
        modified: Option<SystemTime>,
        len: u64,
        /// Inode number and change time (seconds, nanoseconds) on Unix, so
        /// a rename-save (new inode) or a same-length rewrite that only
        /// bumps `ctime` is seen; zero elsewhere.
        identity: (u64, i64, i64),
    },
}

impl Fingerprint {
    fn of(path: &Path) -> Self {
        match std::fs::metadata(path) {
            Ok(meta) => Self::Present {
                modified: meta.modified().ok(),
                len: meta.len(),
                identity: identity_of(&meta),
            },
            Err(_) => Self::Missing,
        }
    }
}

/// A cheap identity of the themes directory's state: the name and
/// [`Fingerprint`] of every `*.toml` entry the theme lookup would consider
/// (not hidden, UTF-8 name; at most [`MAX_THEME_DIR_ENTRIES`] listed), in
/// name order. Following symlinks, so editing the target of a linked theme
/// file changes it too. A missing or unreadable directory has no entries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ThemesFingerprint(Vec<(String, Fingerprint)>);

impl ThemesFingerprint {
    fn of(dir: &Path) -> Self {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Self::default();
        };
        let mut files: Vec<(String, Fingerprint)> = entries
            .take(MAX_THEME_DIR_ENTRIES)
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let stem = name.strip_suffix(".toml")?;
                if stem.is_empty() || stem.starts_with('.') {
                    return None;
                }
                let fingerprint = Fingerprint::of(&entry.path());
                Some((name, fingerprint))
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        Self(files)
    }
}

#[cfg(unix)]
fn identity_of(meta: &std::fs::Metadata) -> (u64, i64, i64) {
    use std::os::unix::fs::MetadataExt as _;
    (meta.ino(), meta.ctime(), meta.ctime_nsec())
}

#[cfg(not(unix))]
fn identity_of(_meta: &std::fs::Metadata) -> (u64, i64, i64) {
    (0, 0, 0)
}

/// How many polling ticks pass between two samples of every theme file;
/// the ticks in between sample only `config.toml`, the themes directory
/// itself (an entry created, deleted or renamed) and the theme file in use.
/// A themes directory of a few thousand files then costs a full listing
/// about once a second at the default interval, not twenty times.
const POLL_FULL_THEMES_EVERY: u32 = 20;

/// What the polling backend compares from one tick to the next.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PollSample {
    config: Fingerprint,
    /// The themes directory's own metadata.
    themes_dir: Fingerprint,
    /// The theme file in use, and its metadata.
    theme_file: Option<(std::path::PathBuf, Fingerprint)>,
    /// Every theme file, refreshed every [`POLL_FULL_THEMES_EVERY`] ticks.
    themes: ThemesFingerprint,
}

impl PollSample {
    fn of(store: &ConfigStore, theme_file: Option<std::path::PathBuf>) -> Self {
        Self {
            config: Fingerprint::of(&store.paths().config_file()),
            themes_dir: Fingerprint::of(&store.paths().themes_dir()),
            theme_file: theme_file.map(|path| {
                let fingerprint = Fingerprint::of(&path);
                (path, fingerprint)
            }),
            themes: ThemesFingerprint::of(&store.paths().themes_dir()),
        }
    }
}

fn run_polling(
    store: &ConfigStore,
    options: &WatchOptions,
    baseline: PollSample,
    stop: &Receiver<()>,
) {
    let path = store.paths().config_file();
    let themes_dir = store.paths().themes_dir();
    let mut last_seen = baseline;
    let mut tick: u32 = 0;
    // When the files last changed, and when the current burst of changes
    // began; both `None` while nothing is pending.
    let mut quiet_since: Option<Instant> = None;
    let mut dirty_since: Option<Instant> = None;

    loop {
        match stop.recv_timeout(options.poll_interval) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }

        let now = Instant::now();
        tick = tick.wrapping_add(1);
        let mut current = PollSample {
            config: Fingerprint::of(&path),
            themes_dir: Fingerprint::of(&themes_dir),
            theme_file: last_seen
                .theme_file
                .as_ref()
                .map(|(file, _)| (file.clone(), Fingerprint::of(file))),
            themes: last_seen.themes.clone(),
        };
        if tick.is_multiple_of(POLL_FULL_THEMES_EVERY) {
            current.themes = ThemesFingerprint::of(&themes_dir);
        }
        if current != last_seen {
            last_seen = current;
            quiet_since = Some(now);
            dirty_since.get_or_insert(now);
        }
        let (Some(quiet), Some(dirty)) = (quiet_since, dirty_since) else {
            continue;
        };
        let settled = now.duration_since(quiet) >= options.debounce;
        let overdue = now.duration_since(dirty) >= options.max_wait;
        if !settled && !overdue {
            continue;
        }
        quiet_since = None;
        dirty_since = None;
        settle(store);
        // The configuration (or a new file) may have changed the theme in
        // use: follow its file from now on.
        let previous = last_seen
            .theme_file
            .as_ref()
            .map(|(file, _)| file.as_path());
        let theme_file = active_theme_file(store, previous);
        if theme_file.as_deref() != previous {
            last_seen.theme_file = theme_file.map(|file| {
                let fingerprint = Fingerprint::of(&file);
                (file, fingerprint)
            });
        }
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
mod native {
    //! `kqueue(2)` backend.

    use std::fs::File;
    use std::io::{PipeReader, PipeWriter};
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::fs::MetadataExt as _;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use kqueue::{Event, EventData, EventFilter, FilterFlag, Ident};

    use super::{WatchOptions, active_theme_file, report_themes};
    use crate::paths::ConfigPaths;

    /// Longest pause after a `kqueue` error, so a stop request is never
    /// delayed by more than this.
    const ERROR_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(1);
    use crate::store::ConfigStore;

    pub(super) struct Native {
        queue: kqueue::Watcher,
        /// Read end of the stop pipe; kept open for the thread's lifetime so
        /// the descriptor registered with the queue stays valid. The queue
        /// does not own it.
        _wake: PipeReader,
        wake_fd: RawFd,
        /// The configuration directory and its open, registered handle.
        /// `None` while the directory does not exist: the thread then polls
        /// for its return instead of sleeping on the queue, since nothing
        /// would wake it.
        root: PathBuf,
        root_handle: Option<File>,
        /// `config.toml` and its handle, `None` while the file is missing.
        /// Opening follows symlinks, so the handle is the link's target.
        file: PathBuf,
        file_handle: Option<File>,
        /// When `config.toml` is a symlink: the directory holding its final
        /// target, and that directory's registered handle. A target that is
        /// deleted and recreated (a dotfiles checkout being updated) is
        /// seen through this watch; the root directory alone would not
        /// notice, since the target lives elsewhere. `link_dir` is `None`
        /// when the file is not a link; `link_dir_handle` is `None` while
        /// the directory is missing, in which case the thread polls.
        link_dir: Option<PathBuf>,
        link_dir_handle: Option<File>,
        /// The themes directory and its handle, `None` while it is missing.
        /// Its creation is seen through the root directory's watch, so a
        /// missing `themes/` does not make the thread poll.
        themes: PathBuf,
        themes_handle: Option<File>,
        /// The theme file `appearance.theme` resolved to at the last
        /// settle, and its handle, so an in-place write to the theme in use
        /// (which raises no directory event) is seen. `None` while the
        /// theme is a bundled one or the file is missing. A theme file that
        /// is a symlink is followed to its target, but a target replaced in
        /// its own directory is seen only on the next change of the
        /// configuration or of `themes/`.
        theme_file: Option<PathBuf>,
        theme_file_handle: Option<File>,
        /// The error from the most recent [`Native::register_all`] that
        /// could not register every slot, or `None` when it registered
        /// them all. While `Some` the backend is not armed and polls, since
        /// a path it cannot open (an unreadable file, a symlink cycle)
        /// could change without any registered watch noticing. Only a
        /// change is logged, so a file that stays unreadable produces one
        /// warning rather than one per poll.
        register_error: Option<String>,
    }

    /// What a registration slot expects to find at its path.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Kind {
        File,
        Dir,
    }

    impl Kind {
        fn flags(self) -> FilterFlag {
            let common = FilterFlag::NOTE_WRITE
                | FilterFlag::NOTE_EXTEND
                | FilterFlag::NOTE_DELETE
                | FilterFlag::NOTE_RENAME;
            match self {
                Self::File => common | FilterFlag::NOTE_ATTRIB,
                Self::Dir => common,
            }
        }

        fn matches(self, file_type: std::fs::FileType) -> bool {
            match self {
                Self::File => file_type.is_file(),
                Self::Dir => file_type.is_dir(),
            }
        }
    }

    /// Opens `path` and registers it with `queue`, replacing whatever
    /// `slot` held before. On return `slot` is `Some` exactly when the path
    /// exists, is of the expected `kind` and is armed. A missing path, or
    /// one of another type, is not an error; a path that exists but cannot
    /// be inspected or opened (permission denied, a symlink cycle) or
    /// cannot be registered is, and the caller then polls until it can.
    ///
    /// `kqueue` watches an inode, not a name, so after a rename-save, a
    /// delete and recreate, or a removed directory this must run again to
    /// follow the current inode. A path removed or replaced between the
    /// open and the registration never produces `NOTE_DELETE` for the armed
    /// handle, so after arming the path must still name the handle's inode
    /// (see [`still_names`]); otherwise it counts as missing.
    ///
    /// The type is checked with `stat` before opening: a FIFO where
    /// `config.toml` should be would otherwise block the open until a
    /// writer appeared. Anything but the expected kind counts as missing;
    /// the directory watch still reports when it is replaced.
    fn register(
        queue: &mut kqueue::Watcher,
        slot: &mut Option<File>,
        path: &Path,
        kind: Kind,
    ) -> std::io::Result<()> {
        if let Some(old) = slot.take() {
            // Closing the descriptor (on drop) also removes its kernel
            // registration; `remove_file` drops the queue's bookkeeping.
            let _ = queue.remove_file(&old, EventFilter::EVFILT_VNODE);
        }
        match std::fs::metadata(path) {
            Ok(meta) if kind.matches(meta.file_type()) => {}
            Ok(_) => {
                tracing::debug!(
                    target: "tessera_config",
                    path = %path.display(),
                    "not a regular {kind:?}; not watched"
                );
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        let handle = match File::open(path) {
            Ok(handle) => handle,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        queue.add_file(&handle, EventFilter::EVFILT_VNODE, kind.flags())?;
        match queue.watch().and_then(|()| still_names(path, &handle)) {
            Ok(true) => {
                *slot = Some(handle);
                Ok(())
            }
            named => {
                // Not armed after all: drop the queue's entry before the
                // handle closes, so no later `watch()` re-submits a closed
                // (or reused) descriptor.
                let _ = queue.remove_file(&handle, EventFilter::EVFILT_VNODE);
                named.map(|_| ())
            }
        }
    }

    /// Whether `path` still names the inode `handle` has open. `false` when
    /// the handle's inode has been unlinked, or the path is missing or now
    /// names another inode. The link count alone is not enough: APFS
    /// reports two links for an open directory after `rmdir`, so a
    /// directory removed between the open and the registration would
    /// otherwise look armed while nothing watches the path.
    fn still_names(path: &Path, handle: &File) -> std::io::Result<bool> {
        let opened = handle.metadata()?;
        if opened.nlink() == 0 {
            return Ok(false);
        }
        match std::fs::metadata(path) {
            Ok(current) => Ok(current.dev() == opened.dev() && current.ino() == opened.ino()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The directory holding the final target of `file` when `file` is a
    /// symlink, `None` when it is not (or is unreadable or a cycle).
    fn link_dir_of(file: &Path) -> Option<PathBuf> {
        let is_link = std::fs::symlink_metadata(file).ok()?.is_symlink();
        if !is_link {
            return None;
        }
        let target = crate::load::follow_links(file).ok().flatten()?;
        target.parent().map(Path::to_path_buf)
    }

    impl Native {
        /// Registers the directory, the file (if present), the themes
        /// directory, `theme_file` (the theme in use, if it is a user file)
        /// and a stop pipe. Dropping the returned writer wakes the thread.
        /// A path that cannot be registered yet (an unreadable
        /// `config.toml`, say) is not a failure: the thread polls for it,
        /// as [`ConfigStore::open`] tolerates the same conditions.
        pub(super) fn open(
            paths: &ConfigPaths,
            theme_file: Option<PathBuf>,
        ) -> std::io::Result<(Self, PipeWriter)> {
            let (wake, stop) = std::io::pipe()?;
            let mut queue = kqueue::Watcher::new()?;
            queue.add_fd(
                wake.as_raw_fd(),
                EventFilter::EVFILT_READ,
                FilterFlag::empty(),
            )?;
            queue.watch()?;
            let mut native = Self {
                queue,
                wake_fd: wake.as_raw_fd(),
                _wake: wake,
                root: paths.root().to_path_buf(),
                root_handle: None,
                file: paths.config_file(),
                file_handle: None,
                link_dir: None,
                link_dir_handle: None,
                themes: paths.themes_dir(),
                themes_handle: None,
                theme_file,
                theme_file_handle: None,
                register_error: None,
            };
            native.rearm();
            Ok((native, stop))
        }

        /// Re-registers the directory, (for a symlinked file) the target's
        /// directory, the themes directory, the theme file in use and then
        /// the file, so all follow their current inodes.
        /// Every slot is attempted even when an earlier one fails, so the
        /// directory watch stays armed while the file is unreadable; the
        /// first failure is returned.
        fn register_all(&mut self) -> std::io::Result<()> {
            let mut failed = None;
            let mut note = |result: std::io::Result<()>| {
                if let Err(error) = result {
                    failed.get_or_insert(error);
                }
            };
            note(register(
                &mut self.queue,
                &mut self.root_handle,
                &self.root,
                Kind::Dir,
            ));
            self.link_dir = link_dir_of(&self.file);
            match &self.link_dir {
                Some(dir) if *dir != self.root => {
                    let dir = dir.clone();
                    note(register(
                        &mut self.queue,
                        &mut self.link_dir_handle,
                        &dir,
                        Kind::Dir,
                    ));
                }
                _ => {
                    // Not a link, or one whose target sits in the root
                    // directory, which is watched already.
                    self.link_dir = None;
                    if let Some(old) = self.link_dir_handle.take() {
                        let _ = self.queue.remove_file(&old, EventFilter::EVFILT_VNODE);
                    }
                }
            }
            note(register(
                &mut self.queue,
                &mut self.themes_handle,
                &self.themes,
                Kind::Dir,
            ));
            note(self.register_theme_file());
            // The file goes last: re-registering a directory drops its
            // pending events, so a rename-save landing between opening the
            // file and re-arming the directory it was replaced in would go
            // unseen, and the file handle would miss the new inode's writes.
            // With every directory armed first, such a rename either
            // precedes the open (which then sees the new inode) or raises an
            // event on an armed directory.
            note(register(
                &mut self.queue,
                &mut self.file_handle,
                &self.file,
                Kind::File,
            ));
            failed.map_or(Ok(()), Err)
        }

        /// Registers the theme file in use, or drops the registration of the
        /// previous one when the theme is no longer a user file. Touches no
        /// other slot, so pending directory events survive it.
        fn register_theme_file(&mut self) -> std::io::Result<()> {
            if let Some(path) = self.theme_file.clone() {
                return register(
                    &mut self.queue,
                    &mut self.theme_file_handle,
                    &path,
                    Kind::File,
                );
            }
            if let Some(old) = self.theme_file_handle.take() {
                let _ = self.queue.remove_file(&old, EventFilter::EVFILT_VNODE);
            }
            Ok(())
        }

        /// Whether everything that could produce the next change is
        /// registered, so sleeping on the queue cannot miss anything. While
        /// the root or a symlink target's directory is missing, or a path
        /// exists but could not be registered (an unreadable `config.toml`
        /// is rewritten in place without any directory event), the thread
        /// polls instead and re-reads the file once it is armed again.
        fn armed(&self) -> bool {
            self.root_handle.is_some()
                && (self.link_dir.is_none() || self.link_dir_handle.is_some())
                && self.register_error.is_none()
        }

        /// Refreshes the registrations after an event. A failure is logged
        /// once when it first appears (or changes) and the recovery once,
        /// since while it lasts this runs every `poll_interval`.
        fn rearm(&mut self) {
            let result = self.register_all();
            self.note_registration(result);
        }

        /// Logs a registration failure once when it first appears (or
        /// changes) and the recovery once, and keeps the failure so the
        /// thread polls while it lasts.
        fn note_registration(&mut self, result: std::io::Result<()>) {
            match result {
                Ok(()) => {
                    if self.register_error.take().is_some() {
                        tracing::info!(target: "tessera_config", "config watch registered again");
                    }
                }
                Err(error) => {
                    let error = error.to_string();
                    if self.register_error.as_deref() != Some(&error) {
                        tracing::warn!(
                            target: "tessera_config",
                            %error,
                            "could not register config watch; polling until it succeeds"
                        );
                        self.register_error = Some(error);
                    }
                }
            }
        }

        fn is_stop(&self, event: &Event) -> bool {
            matches!(event.ident, Ident::Fd(fd) if fd == self.wake_fd)
        }

        /// Waits for the next event: in the kernel while every watched
        /// directory exists, otherwise polling for its return. Returns
        /// `None` to stop.
        fn next_event(&mut self, store: &ConfigStore, options: &WatchOptions) -> Option<Event> {
            loop {
                if self.armed() {
                    return self.queue.poll_forever(None);
                }
                if let Some(event) = self.queue.poll(Some(options.poll_interval)) {
                    return Some(event);
                }
                self.rearm();
                if self.armed() {
                    // The directory is back (it may already hold a file),
                    // or the file became readable; either way it may have
                    // changed unseen.
                    self.settle(store);
                }
            }
        }

        /// Reloads `config.toml`, follows the theme file `appearance.theme`
        /// now resolves to and reports a change of `themes/`.
        ///
        /// Runs with the directories armed and does not re-register them
        /// (that would drop their pending events), only the theme file, and
        /// that before the directory is sampled. So a theme file created
        /// after the lookup leaves a directory event for the next round, a
        /// write after the registration raises an event on the armed file,
        /// and nothing lands unseen between the sample and the next wait.
        fn settle(&mut self, store: &ConfigStore) {
            super::reload_and_report(store);
            self.theme_file = active_theme_file(store, self.theme_file.as_deref());
            let result = self.register_theme_file();
            if result.is_err() {
                self.note_registration(result);
            }
            report_themes(store);
        }

        pub(super) fn run(mut self, store: &ConfigStore, options: &WatchOptions) {
            // `Watcher::spawn` reloaded the file after `open`, so the theme
            // in use may have changed. Only then is its file registered
            // again: re-registering the same file would drop a write
            // already pending on it. A file that did change is sampled
            // after it is armed, so a write to it since `spawn` sampled
            // the directory is reported here rather than lost. Anything
            // else since then raises an event on the armed directories or
            // file, debounced as usual.
            let theme_file = active_theme_file(store, self.theme_file.as_deref());
            if theme_file != self.theme_file {
                self.theme_file = theme_file;
                let result = self.register_theme_file();
                if result.is_err() {
                    self.note_registration(result);
                }
                report_themes(store);
            }
            loop {
                let Some(event) = self.next_event(store, options) else {
                    return;
                };
                if self.is_stop(&event) {
                    return;
                }
                if let EventData::Error(error) = &event.data {
                    tracing::warn!(target: "tessera_config", %error, "kqueue error");
                    // Pause so a persistent error cannot spin, but briefly,
                    // since a stop request cannot interrupt the sleep.
                    std::thread::sleep(
                        options
                            .debounce
                            .clamp(super::MIN_POLL_INTERVAL, ERROR_BACKOFF_CAP),
                    );
                    continue;
                }
                self.rearm();

                // Debounce: wait until the file has been quiet, but no
                // longer than `max_wait` in all. `max_wait` is clamped to
                // `MAX_WATCH_DURATION`, so the deadline is representable; a
                // failure to represent it just skips the debounce.
                let Some(deadline) = Instant::now().checked_add(options.max_wait) else {
                    self.settle(store);
                    continue;
                };
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    match self.queue.poll(Some(options.debounce.min(remaining))) {
                        None => break,
                        Some(event) if self.is_stop(&event) => return,
                        Some(_) => self.rearm(),
                    }
                }

                self.settle(store);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_directory_removed_after_opening_is_no_longer_named() {
            let base = tempfile::tempdir().unwrap();
            let dir = base.path().join("Tessera");
            std::fs::create_dir(&dir).unwrap();
            let handle = File::open(&dir).unwrap();
            assert!(still_names(&dir, &handle).unwrap());

            std::fs::remove_dir(&dir).unwrap();
            assert!(!still_names(&dir, &handle).unwrap(), "removed");

            std::fs::create_dir(&dir).unwrap();
            assert!(!still_names(&dir, &handle).unwrap(), "recreated");
        }

        #[test]
        fn a_file_unlinked_or_replaced_after_opening_is_no_longer_named() {
            let base = tempfile::tempdir().unwrap();
            let path = base.path().join("config.toml");
            std::fs::write(&path, "").unwrap();
            let handle = File::open(&path).unwrap();
            assert!(still_names(&path, &handle).unwrap());

            let tmp = base.path().join("config.toml.new");
            std::fs::write(&tmp, "").unwrap();
            std::fs::rename(&tmp, &path).unwrap();
            assert!(!still_names(&path, &handle).unwrap(), "replaced");

            let handle = File::open(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            assert!(!still_names(&path, &handle).unwrap(), "unlinked");
        }

        #[test]
        fn a_symlink_target_replaced_after_opening_is_no_longer_named() {
            let base = tempfile::tempdir().unwrap();
            let target_dir = base.path().join("dotfiles");
            std::fs::create_dir(&target_dir).unwrap();
            let target = target_dir.join("config.toml");
            std::fs::write(&target, "").unwrap();
            let link = base.path().join("config.toml");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let handle = File::open(&link).unwrap();
            assert!(
                still_names(&link, &handle).unwrap(),
                "opened through the link"
            );

            let tmp = target_dir.join("config.toml.new");
            std::fs::write(&tmp, "").unwrap();
            std::fs::rename(&tmp, &target).unwrap();
            assert!(!still_names(&link, &handle).unwrap(), "target replaced");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_clamped() {
        let opts = WatchOptions {
            poll_interval: Duration::ZERO,
            debounce: Duration::from_millis(100),
            max_wait: Duration::ZERO,
            ..Default::default()
        }
        .clamped();
        assert_eq!(opts.poll_interval, MIN_POLL_INTERVAL);
        assert_eq!(opts.max_wait, Duration::from_millis(100));
        let opts = WatchOptions::default().clamped();
        assert_eq!(opts.poll_interval, Duration::from_millis(50));
        assert_eq!(opts.max_wait, Duration::from_secs(1));
    }

    #[test]
    fn huge_durations_are_lowered_to_the_maximum() {
        let opts = WatchOptions {
            poll_interval: Duration::MAX,
            debounce: Duration::MAX,
            max_wait: Duration::MAX,
            ..Default::default()
        }
        .clamped();
        assert_eq!(opts.poll_interval, MAX_WATCH_DURATION);
        assert_eq!(opts.debounce, MAX_WATCH_DURATION);
        assert_eq!(opts.max_wait, MAX_WATCH_DURATION);
        // Every deadline the thread computes from these stays representable.
        assert!(Instant::now().checked_add(opts.max_wait).is_some());
        assert!(
            Instant::now()
                .checked_add(opts.poll_interval + opts.debounce)
                .is_some()
        );

        // A huge `max_wait` alone keeps the (bounded) debounce.
        let opts = WatchOptions {
            max_wait: Duration::MAX,
            ..Default::default()
        }
        .clamped();
        assert_eq!(opts.debounce, Duration::from_millis(100));
        assert_eq!(opts.max_wait, MAX_WATCH_DURATION);
    }
}
