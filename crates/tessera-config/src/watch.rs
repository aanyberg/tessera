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
//!   re-registered at every settle. A theme file that stops loading (a
//!   half-written save) stays registered while it exists, so the save that
//!   fixes it is seen. While the configured theme is not found as a
//!   loadable user file, the theme files in `themes/` that do not load (an
//!   empty file an editor just created, one being saved in chunks) are
//!   registered too, newest first and at most eight
//!   (`MAX_UNLOADABLE_FOLLOWED`), since one of them may turn out to be the
//!   configured theme by its `name` key once it is complete (#253 item 8). An in-place write to any *other* theme
//!   file is not seen: it cannot change the theme in use.
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
//! Each re-read settles in one order: `config.toml` is loaded, the theme
//! file it names is followed (registered, or recorded as the poll
//! baseline), and only then is the change applied to the [`ConfigStore`]
//! and sent to every watcher of that store, so one change is reported once
//! to each of them, and a theme save made in reaction to the event is seen
//! (#243, #253 items 1 to 3). The same holds for the catch-up the watcher
//! does when it starts. If the
//! watcher thread ever dies unexpectedly its channel is closed, so a
//! consumer sees [`RecvTimeoutError::Disconnected`] rather than silence,
//! and the failure is logged at error level.
//!
//! **Create one watcher per process.** Each one costs a thread plus, on
//! the native backend, a kernel queue and about seven file descriptors (the
//! queue, the stop pipe's two ends, the directory, the file, `themes/` and
//! the theme file in use), plus at most eight
//! (`MAX_UNLOADABLE_FOLLOWED`) while the configured theme's file does not load. The
//! application should hold a single [`Watcher`] and fan its events out to
//! every window rather than watching once per window.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crate::load::Warning;
use crate::store::{ConfigStore, FromDisk};
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
        let (mut backend, stop) = match Backend::new(&store, options.backend) {
            Ok(backend) => backend,
            Err(error) => {
                store.unsubscribe(subscription);
                return Err(error);
            }
        };
        // The backend is armed, so from here on every edit produces an
        // event. Catch up on anything written between `ConfigStore::open`
        // and now, following the theme file the catch-up leaves in use
        // before it is published (#253 item 1); an edit that is seen both
        // here and by the backend loads twice but is reported once, since
        // the second load finds no change. The first watcher of a store
        // records the themes directory's state as its baseline here; a
        // later one reports what changed since.
        backend.catch_up(&store);
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

/// Re-reads `config.toml`, has `follow` start watching the theme file the
/// outcome leaves in use, then applies the outcome to the store, reports it
/// to every watcher of the store and reports a change of `themes/`
/// ([`report_themes`]).
///
/// `follow` gets the theme name the outcome leaves in use and runs before
/// the outcome is applied or sent (#243, #253 items 1-3). A subscriber may
/// react to the change at once by saving that theme file, and so may any
/// thread that reads [`ConfigStore::config`] once it is applied; a save
/// that leaves a file found by its `name` key unloadable hides it from the
/// lookup, which would then miss the save that fixes it. Following first
/// means such a save lands on a watched file, and the themes directory is
/// sampled after that, so a write between the two is reported here.
fn settle(store: &ConfigStore, follow: impl FnOnce(&str)) {
    let outcome = store.load_from_disk();
    let name = match &outcome {
        Ok(from) => from.loaded.config.appearance.theme.clone(),
        Err(_) => store.config().appearance.theme.clone(),
    };
    follow(&name);
    if let Some(event) = apply(store, outcome) {
        store.broadcast(&event);
    }
    report_themes(store);
}

/// Samples the themes directory and sends [`ConfigEvent::ThemesChanged`]
/// to every watcher of the store when it differs from the state the store
/// last recorded. The store keeps that state, so several watchers of one
/// store report one change once, as with [`settle`]; the first sample of
/// a store records a baseline and reports nothing.
fn report_themes(store: &ConfigStore) {
    let themes = ThemesFingerprint::of(&store.paths().themes_dir());
    if store.record_themes(themes) {
        store.broadcast(&ConfigEvent::ThemesChanged);
    }
}

/// The theme file `name` resolves to in `themes`, or `None` when it
/// resolves to a bundled theme or the default.
///
/// A theme file that is there but does not load (an editor's half-written
/// save, a typo) is still the one to follow, or the save that fixes it would
/// go unseen: a file whose stem matches the name, else `previous` (the file
/// followed so far, which may match by its `name` key) while it exists.
/// Files that may match by a `name` key no one can read yet are in
/// [`theme_files_to_follow`].
fn active_theme_file(
    themes: &Path,
    name: &str,
    previous: Option<&Path>,
) -> Option<std::path::PathBuf> {
    match Theme::resolve(themes, name).source {
        ThemeSource::User(path) => Some(path),
        ThemeSource::Bundled | ThemeSource::Default => fallback_theme_file(themes, name, previous),
    }
}

/// [`active_theme_file`] for a name that does not resolve to a user file.
fn fallback_theme_file(
    themes: &Path,
    name: &str,
    previous: Option<&Path>,
) -> Option<std::path::PathBuf> {
    crate::theme::files_named(themes, name)
        .into_iter()
        .next()
        .or_else(|| {
            previous
                .filter(|path| path.is_file())
                .map(Path::to_path_buf)
        })
}

/// What the native backend follows for the theme `name`: the file of
/// [`active_theme_file`] and, while that is not a user file the lookup
/// loaded, the theme files it could not load
/// ([`crate::theme::unloadable_to_follow`], at most
/// [`crate::theme::MAX_UNLOADABLE_FOLLOWED`]), without the first.
///
/// One of those may be the theme asked for, half-written: created empty
/// and then filled, deleted and recreated, or saved in chunks while the
/// switch settled. Its `name` key cannot be read yet, and the in-place
/// write that completes it raises no event on the directory, so without a
/// watch on the file itself it would be lost for good (#253 item 8). The
/// event on it starts a settle, which looks the theme up again.
#[cfg_attr(
    not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )),
    allow(dead_code)
)]
fn theme_files_to_follow(
    themes: &Path,
    name: &str,
    previous: Option<&Path>,
) -> (Option<std::path::PathBuf>, Vec<std::path::PathBuf>) {
    let (user, unloadable) = crate::theme::unloadable_to_follow(themes, name);
    if user.is_some() {
        return (user, Vec::new());
    }
    let file = fallback_theme_file(themes, name, previous);
    let unloadable = unloadable
        .into_iter()
        .filter(|path| Some(path) != file.as_ref())
        .collect();
    (file, unloadable)
}

/// Applies a load's outcome: the event that reports it, for the caller to
/// send to every watcher of the store, or `None` when the file is
/// unchanged (or the same error was reported already).
fn apply(store: &ConfigStore, outcome: Result<FromDisk, Error>) -> Option<ConfigEvent> {
    match outcome {
        Ok(from) => {
            let warnings = from.loaded.warnings.clone();
            store
                .apply(from)
                .map(|config| ConfigEvent::Changed(ConfigChanged { config, warnings }))
        }
        Err(error) => {
            // Two watchers of one store both load the same broken file; only
            // the first to record the error reports (and logs) it, as with
            // `apply`.
            store
                .record_error(error.clone())
                .then_some(ConfigEvent::Invalid(error))
        }
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
                    let (native, stop) = native::Native::open(store.paths(), &store.config())?;
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
        let themes = store.paths().themes_dir();
        let name = store.config().appearance.theme.clone();
        let baseline = PollSample::of(store, active_theme_file(&themes, &name, None));
        (
            Self::Poll { stop, baseline },
            Stop::Channel { _tx: stop_tx },
        )
    }

    /// The catch-up in [`Watcher::spawn`]: a [`settle`] that follows the
    /// theme file it leaves in use, from the baseline the backend took when
    /// it was armed.
    fn catch_up(&mut self, store: &ConfigStore) {
        match self {
            Self::Poll { baseline, .. } => {
                settle(store, |name| follow_polled_theme(store, baseline, name));
            }
            #[cfg(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "dragonfly"
            ))]
            Self::Native(native) => native.settle(store),
        }
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
        settle(store, |name| {
            follow_polled_theme(store, &mut last_seen, name);
        });
    }
}

/// Points the polling backend's `sample` at the theme file `name` resolves
/// to now, before the settle publishes it (see [`settle`]). A file that
/// did not change keeps its fingerprint, so a write to it since the last
/// tick is still seen; a new one is sampled now, after which every write
/// to it shows on a tick.
fn follow_polled_theme(store: &ConfigStore, sample: &mut PollSample, name: &str) {
    let previous = sample.theme_file.as_ref().map(|(file, _)| file.as_path());
    let theme_file = active_theme_file(&store.paths().themes_dir(), name, previous);
    if theme_file.as_deref() != previous {
        sample.theme_file = theme_file.map(|file| {
            let fingerprint = Fingerprint::of(&file);
            (file, fingerprint)
        });
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

    use super::{WatchOptions, theme_files_to_follow};
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
        /// While `theme_file` is not a user file the lookup loaded: theme
        /// files that do not load, one of which may be the theme asked for
        /// half-written (see [`theme_files_to_follow`]), and the handles of
        /// those registered. At most
        /// [`crate::theme::MAX_UNLOADABLE_FOLLOWED`]. Best effort: one that
        /// cannot be registered (unreadable, say) is skipped rather than
        /// making the backend poll, since it may well be unrelated.
        unloadable: Vec<PathBuf>,
        unloadable_handles: Vec<File>,
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
    /// The type is checked with `stat` before opening, and again on the
    /// open handle: the open is non-blocking (`O_NONBLOCK`), so a FIFO
    /// renamed over the path between the two (a hostile local process
    /// racing a broken `themes/*.toml`, #287) opens at once instead of
    /// blocking the watcher thread, and the drop that joins it, until a
    /// writer appears; the check on the handle then rejects it. Anything
    /// but the expected kind counts as missing; the directory watch still
    /// reports when it is replaced.
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
                // S8: the unloadable follow reaches here with arbitrary
                // `themes/` entry names.
                tracing::debug!(
                    target: "tessera_config",
                    path = %crate::load::escaped(
                        &path.display().to_string(),
                        crate::load::MAX_WARNING_PATH_LEN
                    ),
                    "not a regular {kind:?}; not watched"
                );
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        #[cfg(test)]
        tests::before_open(path);
        let handle = match open_nonblocking(path) {
            Ok(handle) => handle,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if !kind.matches(handle.metadata()?.file_type()) {
            // Replaced since the `stat`; the directory watch reports the
            // next change.
            return Ok(());
        }
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

    /// Opens `path` read-only without blocking on a FIFO (`O_NONBLOCK`,
    /// which a regular file or directory ignores). Symlinks are followed:
    /// a symlinked `config.toml` is watched through its target.
    fn open_nonblocking(path: &Path) -> std::io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
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
        /// directory, the theme files `config` follows (see
        /// [`theme_files_to_follow`]) and a stop pipe. Dropping the
        /// returned writer wakes the thread. A path that cannot be
        /// registered yet (an unreadable `config.toml`, say) is not a
        /// failure: the thread polls for it, as [`ConfigStore::open`]
        /// tolerates the same conditions.
        pub(super) fn open(
            paths: &ConfigPaths,
            config: &crate::Config,
        ) -> std::io::Result<(Self, PipeWriter)> {
            let (theme_file, unloadable) =
                theme_files_to_follow(&paths.themes_dir(), &config.appearance.theme, None);
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
                unloadable,
                unloadable_handles: Vec::new(),
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

        /// Registers the theme file in use and the unloadable ones followed
        /// with it, or drops the registrations of the previous ones. Touches
        /// no other slot, so pending directory events survive it.
        fn register_theme_file(&mut self) -> std::io::Result<()> {
            for old in self.unloadable_handles.drain(..) {
                let _ = self.queue.remove_file(&old, EventFilter::EVFILT_VNODE);
            }
            for path in &self.unloadable {
                let mut slot = None;
                match register(&mut self.queue, &mut slot, path, Kind::File) {
                    Ok(()) => self.unloadable_handles.extend(slot),
                    Err(error) => tracing::debug!(
                        target: "tessera_config",
                        path = %crate::load::escaped(
                            &path.display().to_string(),
                            crate::load::MAX_WARNING_PATH_LEN
                        ),
                        %error,
                        "unloadable theme file not watched"
                    ),
                }
            }
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

        /// [`super::settle`]: reloads `config.toml`, follows the theme files
        /// the outcome's `appearance.theme` resolves to before the outcome
        /// is applied and reported (#243, #253 items 1-3), and reports a
        /// change of `themes/`.
        ///
        /// Runs with the directories armed and does not re-register them
        /// (that would drop their pending events), only the theme files,
        /// and that before the directory is sampled. So a theme file created
        /// after the lookup leaves a directory event for the next round, a
        /// write after the registration raises an event on the armed file,
        /// and nothing lands unseen between the sample and the next wait.
        /// `Watcher::spawn` runs it once before the thread starts, to catch
        /// up on what changed since `ConfigStore::open` (#253 item 1).
        pub(super) fn settle(&mut self, store: &ConfigStore) {
            super::settle(store, |name| self.follow(name));
        }

        /// Follows the theme files `name` resolves to now.
        fn follow(&mut self, name: &str) {
            let (file, unloadable) =
                theme_files_to_follow(&self.themes, name, self.theme_file.as_deref());
            self.theme_file = file;
            self.unloadable = unloadable;
            let result = self.register_theme_file();
            if result.is_err() {
                self.note_registration(result);
            }
        }

        pub(super) fn run(mut self, store: &ConfigStore, options: &WatchOptions) {
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
        use crate::theme::Theme;
        use crate::watch::ConfigEvent;

        thread_local! {
            /// Run by [`register`] on this thread between its `stat` and
            /// its open: a test that swaps the path in that window.
            static BEFORE_OPEN: std::cell::Cell<Option<fn(&Path)>> =
                const { std::cell::Cell::new(None) };
        }

        /// The hook point in [`register`]; see [`BEFORE_OPEN`].
        pub(super) fn before_open(path: &Path) {
            if let Some(hook) = BEFORE_OPEN.with(std::cell::Cell::get) {
                hook(path);
            }
        }

        /// Creates a FIFO at `path`.
        fn mkfifo(path: &Path) {
            let status = std::process::Command::new("mkfifo")
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success(), "mkfifo {}", path.display());
        }

        /// Renames the FIFO made beforehand at `path.fifo` over `path`
        /// (no process is started in the window), keeping that name so a
        /// test that fails can still release a reader blocked on it.
        fn swap_in_fifo(path: &Path) {
            let fifo = path.with_extension("fifo");
            let staged = path.with_extension("staged");
            std::fs::hard_link(&fifo, &staged).unwrap();
            std::fs::rename(&staged, path).unwrap();
        }

        /// #287 (M4): a FIFO renamed over a broken theme file between
        /// [`register`]'s `stat` and its open must not block the watcher
        /// thread; it counts as missing. Deterministic: the swap runs in
        /// the window, on the registering thread.
        #[test]
        fn a_fifo_raced_in_after_the_stat_does_not_block_the_registration() {
            let base = tempfile::tempdir().unwrap();
            let paths = ConfigPaths::new(base.path());
            std::fs::create_dir_all(paths.themes_dir()).unwrap();
            let broken = paths.themes_dir().join("broken.toml");
            std::fs::write(&broken, "name = \"Broken\"\nforeground = ").unwrap();
            mkfifo(&broken.with_extension("fifo"));
            let (done, finished) = std::sync::mpsc::channel();
            let path = broken.clone();
            let registering = std::thread::spawn(move || {
                BEFORE_OPEN.with(|hook| hook.set(Some(swap_in_fifo)));
                let mut queue = kqueue::Watcher::new().unwrap();
                let mut slot = None;
                let result = register(&mut queue, &mut slot, &path, Kind::File);
                done.send(()).unwrap();
                (result.map_err(|e| e.to_string()), slot.is_some())
            });
            if finished
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_err()
            {
                // Release the blocked open so the process can exit.
                let _writer = std::fs::OpenOptions::new()
                    .write(true)
                    .open(broken.with_extension("fifo"));
                let _ = registering.join();
                panic!("register blocked opening a FIFO raced over the path");
            }
            let (result, armed) = registering.join().unwrap();
            assert_eq!(result, Ok(()), "a FIFO counts as missing, not as an error");
            assert!(!armed, "a FIFO is not watched as the file");
            assert!(
                std::os::unix::fs::FileTypeExt::is_fifo(
                    &std::fs::metadata(&broken).unwrap().file_type()
                ),
                "the hook swapped the FIFO in"
            );
        }

        /// A theme document named `name`.
        fn theme_named(name: &str) -> String {
            let mut theme = Theme::bundled("Nord").unwrap().clone();
            theme.name = name.into();
            theme.to_toml()
        }

        /// Saves `keyed.toml` half-written (it no longer loads, so its
        /// `name` key cannot be read) the moment a `Changed` is reported.
        fn break_keyed_on_change(paths: &ConfigPaths, event: &ConfigEvent) {
            if matches!(event, ConfigEvent::Changed(_)) {
                std::fs::write(
                    paths.themes_dir().join("keyed.toml"),
                    "name = \"Keyed Theme\"\nforeground = ",
                )
                .unwrap();
            }
        }

        /// #243: a configuration that switches to a theme found by its
        /// `name` key (file `keyed.toml`, name "Keyed Theme") is reported
        /// only once that file is followed. A subscriber that saves the file
        /// half-written as soon as it hears of the change (it no longer
        /// loads, so the lookup cannot find it by name) must not leave the
        /// watch on the previous theme's file: the save that fixes the
        /// theme raises an event. Deterministic: the save runs inside the
        /// broadcast, on the watcher's own thread.
        #[test]
        fn a_new_theme_file_is_followed_before_the_change_is_reported() {
            let base = tempfile::tempdir().unwrap();
            let paths = ConfigPaths::new(base.path());
            let mine = paths.themes_dir().join("mine.toml");
            let keyed = paths.themes_dir().join("keyed.toml");
            std::fs::create_dir_all(paths.themes_dir()).unwrap();
            std::fs::write(&mine, theme_named("Mine")).unwrap();
            std::fs::write(&keyed, theme_named("Keyed Theme")).unwrap();
            std::fs::write(paths.config_file(), "[appearance]\ntheme = \"mine\"\n").unwrap();
            let store = ConfigStore::open(paths.clone()).unwrap();
            let (mut native, _stop) = Native::open(&paths, &store.config()).unwrap();
            assert_eq!(native.theme_file.as_deref(), Some(mine.as_path()));

            store.set_on_broadcast(Some(break_keyed_on_change));
            crate::write_atomic(
                &paths.config_file(),
                "[appearance]\ntheme = \"Keyed Theme\"\n",
            )
            .unwrap();
            native.settle(&store);
            assert_eq!(store.config().appearance.theme, "Keyed Theme");
            assert_eq!(
                std::fs::read_to_string(&keyed).unwrap(),
                "name = \"Keyed Theme\"\nforeground = ",
                "the subscriber saved the file"
            );
            assert_eq!(
                native.theme_file.as_deref(),
                Some(keyed.as_path()),
                "the theme in use is followed"
            );

            // The fix, saved in place, raises an event on the followed file.
            while native.queue.poll(None).is_some() {}
            std::fs::write(&keyed, theme_named("Keyed Theme")).unwrap();
            let fd = native.theme_file_handle.as_ref().map(AsRawFd::as_raw_fd);
            let event = native.queue.poll(Some(std::time::Duration::from_secs(5)));
            assert!(
                matches!(&event, Some(Event { ident: Ident::Fd(got), .. }) if Some(*got) == fd),
                "no event for the fix on the theme file ({fd:?}): {event:?}"
            );
        }

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

        /// Records every event's fields, `name=value`, at every level.
        struct Capture(std::sync::Mutex<String>);

        struct Fields<'a>(&'a mut String);

        impl tracing::field::Visit for Fields<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write as _;
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
                event.record(&mut Fields(&mut self.0.lock().unwrap()));
            }

            fn enter(&self, _: &tracing::span::Id) {}

            fn exit(&self, _: &tracing::span::Id) {}
        }

        /// S8: a `themes/` entry that is not a regular file (a directory
        /// named like a theme, which the unloadable follow hands to
        /// `register`) is logged with its name escaped, so a raw ESC or a
        /// bidi override in it never reaches the log.
        #[test]
        fn a_hostile_entry_name_is_escaped_in_the_not_watched_log() {
            let dir = tempfile::tempdir().unwrap();
            let entry = dir.path().join("x\u{1b}[31m\u{202e}evil.toml");
            std::fs::create_dir(&entry).unwrap();
            let mut queue = kqueue::Watcher::new().unwrap();
            let mut slot = None;

            let capture = std::sync::Arc::new(Capture(std::sync::Mutex::new(String::new())));
            tracing::subscriber::with_default(std::sync::Arc::clone(&capture), || {
                register(&mut queue, &mut slot, &entry, Kind::File).unwrap();
            });
            assert!(slot.is_none(), "a directory is not registered as a file");

            let logged = capture.0.lock().unwrap().clone();
            assert!(logged.contains("not watched"), "{logged:?}");
            assert!(logged.contains("evil.toml"), "{logged:?}");
            assert!(!logged.contains('\u{1b}'), "raw ESC logged: {logged:?}");
            assert!(
                !logged.contains('\u{202e}'),
                "raw U+202E logged: {logged:?}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::ConfigPaths;

    /// A theme document named `name` with background `background`.
    fn theme_doc(name: &str, background: &str) -> String {
        let mut theme = Theme::bundled("Nord").unwrap().clone();
        theme.name = name.into();
        let doc = theme.to_toml();
        let line = doc
            .lines()
            .find(|line| line.starts_with("background"))
            .unwrap()
            .to_owned();
        doc.replace(&line, &format!("background = \"{background}\""))
    }

    /// `themes/keyed.toml` half-written: it no longer loads, so its `name`
    /// key cannot be read.
    const HALF_WRITTEN: &str = "name = \"Keyed Theme\"\nforeground = ";

    /// How a reactor saves `keyed.toml` so that the lookup no longer finds
    /// it: half-written ([`HALF_WRITTEN`]), or whole under an interim
    /// `name`. Only the first is a file the lookup cannot load, which the
    /// native backend also follows for #253 item 8; the second shows the
    /// order alone.
    fn broken_keyed(half_written: bool) -> String {
        if half_written {
            HALF_WRITTEN.to_owned()
        } else {
            theme_doc("Interim", "#1c1c1c")
        }
    }

    /// Saves `keyed.toml` as [`arm_break`] said, once: the first call after
    /// it consumes its marker file, which lives in `profiles/` since no
    /// watcher watches that (an event in the root would start a settle that
    /// finds the fix anyway).
    fn break_keyed_once(paths: &ConfigPaths) {
        let marker = paths.profiles_dir().join("break-keyed-once");
        if let Ok(half_written) = std::fs::read_to_string(&marker)
            && std::fs::remove_file(&marker).is_ok()
        {
            std::fs::write(
                paths.themes_dir().join("keyed.toml"),
                broken_keyed(half_written == "half"),
            )
            .unwrap();
        }
    }

    fn arm_break(paths: &ConfigPaths, half_written: bool) {
        let mode = if half_written { "half" } else { "renamed" };
        std::fs::write(paths.profiles_dir().join("break-keyed-once"), mode).unwrap();
    }

    /// Where the configuration's change is seen first: by a subscriber of
    /// the watcher's `Changed`, or by a reader of [`ConfigStore::config`]
    /// as soon as the store applies it (#253 item 3).
    #[derive(Debug, Clone, Copy)]
    enum Reactor {
        Subscriber,
        Reader,
    }

    /// A store using theme `mine` (themes `mine.toml` and `keyed.toml`,
    /// the latter found only by its `name` key "Keyed Theme"), with
    /// `reactor` saving `keyed.toml` half-written the first time it sees
    /// a change once [`arm_break`] ran.
    fn keyed_store(reactor: Reactor) -> (tempfile::TempDir, ConfigPaths, ConfigStore) {
        let base = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(base.path());
        std::fs::create_dir_all(paths.themes_dir()).unwrap();
        std::fs::write(
            paths.themes_dir().join("mine.toml"),
            theme_doc("Mine", "#101010"),
        )
        .unwrap();
        std::fs::write(
            paths.themes_dir().join("keyed.toml"),
            theme_doc("Keyed Theme", "#1c1c1c"),
        )
        .unwrap();
        std::fs::write(paths.config_file(), "[appearance]\ntheme = \"mine\"\n").unwrap();
        let store = ConfigStore::open(paths.clone()).unwrap();
        match reactor {
            Reactor::Subscriber => store.set_on_broadcast(Some(|paths, event| {
                if matches!(event, ConfigEvent::Changed(_)) {
                    break_keyed_once(paths);
                }
            })),
            Reactor::Reader => store.set_on_apply(Some(break_keyed_once)),
        }
        (base, paths, store)
    }

    /// Generous for scheduling stalls on a loaded machine, and far below
    /// the poll backend's full themes scan at the interval the tests use
    /// (see [`options`]), so only a followed theme file passes.
    const EVENTUALLY: Duration = Duration::from_secs(5);

    /// A poll interval of 500 ms puts the full themes scan
    /// ([`POLL_FULL_THEMES_EVERY`] ticks) at 10 s, beyond [`EVENTUALLY`].
    fn options(backend: WatchBackend) -> WatchOptions {
        WatchOptions {
            backend,
            poll_interval: Duration::from_millis(500),
            ..WatchOptions::default()
        }
    }

    /// Fixes `keyed.toml` in place and waits for a `ThemesChanged` after
    /// which the store resolves the fixed theme: the fix was seen. The
    /// caller has consumed the events of the settle the reactor ran in, so
    /// a `ThemesChanged` from here on reports the fix (the store resolves
    /// from disk, so it alone cannot tell).
    fn assert_fix_is_seen(
        paths: &ConfigPaths,
        store: &ConfigStore,
        watcher: &Watcher,
        half_written: bool,
        what: &str,
    ) {
        let keyed = paths.themes_dir().join("keyed.toml");
        assert_eq!(
            std::fs::read_to_string(&keyed).unwrap(),
            broken_keyed(half_written),
            "{what}: the reactor ran"
        );
        std::fs::write(&keyed, theme_doc("Keyed Theme", "#1d1d1d")).unwrap();
        let fixed = Theme::load(&keyed).unwrap();
        let started = Instant::now();
        loop {
            let left = EVENTUALLY.saturating_sub(started.elapsed());
            match watcher.recv_timeout(left) {
                Ok(ConfigEvent::ThemesChanged) if store.theme() == fixed => return,
                Ok(_) => {}
                Err(error) => panic!("{what}: the fix was not seen within {EVENTUALLY:?}: {error}"),
            }
        }
    }

    /// Every backend, reactor and kind of broken save.
    fn cases() -> impl Iterator<Item = (WatchBackend, Reactor, bool)> {
        [WatchBackend::Native, WatchBackend::Poll]
            .into_iter()
            .flat_map(|backend| {
                [Reactor::Subscriber, Reactor::Reader]
                    .into_iter()
                    .flat_map(move |reactor| [false, true].map(|half| (backend, reactor, half)))
            })
    }

    /// #253 items 1 and 3, at start-up: a configuration switched to the
    /// keyed theme between `ConfigStore::open` and `watch` is caught up on
    /// by `Watcher::spawn`, whose subscriber (or reader) breaks the theme
    /// file at once; the in-place save that fixes it must still be seen,
    /// on both backends. The critic-243 probe, made a test.
    #[test]
    fn a_theme_switch_caught_up_at_start_up_is_followed_before_it_is_published() {
        for (backend, reactor, half_written) in cases() {
            let what = format!("{backend:?}, {reactor:?}, half-written {half_written}");
            let (_base, paths, store) = keyed_store(reactor);
            crate::write_atomic(
                &paths.config_file(),
                "[appearance]\ntheme = \"Keyed Theme\"\n",
            )
            .unwrap();
            arm_break(&paths, half_written);
            let watcher = store.watch(options(backend)).unwrap();
            assert_eq!(store.config().appearance.theme, "Keyed Theme", "{what}");
            // The catch-up ran in `watch`; its events are queued.
            while watcher.try_recv().is_ok() {}
            assert_fix_is_seen(&paths, &store, &watcher, half_written, &what);
        }
    }

    /// #253 items 2 and 3, while running: the same switch made after the
    /// watcher started.
    #[test]
    fn a_theme_switch_while_running_is_followed_before_it_is_published() {
        for (backend, reactor, half_written) in cases() {
            let what = format!("{backend:?}, {reactor:?}, half-written {half_written}");
            let (_base, paths, store) = keyed_store(reactor);
            let watcher = store.watch(options(backend)).unwrap();
            arm_break(&paths, half_written);
            crate::write_atomic(
                &paths.config_file(),
                "[appearance]\ntheme = \"Keyed Theme\"\n",
            )
            .unwrap();
            // The settle that applies the switch sends `Changed` and then,
            // since the reactor changed `keyed.toml` before the themes
            // directory was sampled, `ThemesChanged`.
            let mut changed = false;
            let started = Instant::now();
            loop {
                let left = EVENTUALLY.saturating_sub(started.elapsed());
                match watcher.recv_timeout(left) {
                    Ok(ConfigEvent::Changed(_)) => changed = true,
                    Ok(ConfigEvent::ThemesChanged) if changed => break,
                    Ok(_) => {}
                    Err(error) => panic!("{what}: no Changed and ThemesChanged: {error}"),
                }
            }
            assert_fix_is_seen(&paths, &store, &watcher, half_written, &what);
        }
    }

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
