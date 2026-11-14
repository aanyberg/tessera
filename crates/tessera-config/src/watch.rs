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
//!   configured theme by its `name` key once it is complete (#253 item 8).
//!   The theme files the profiles name are registered as well, at most
//!   eight (`MAX_PROFILE_THEMES_FOLLOWED`; a bundled theme has no file
//!   and takes no slot), so a profile's theme saved in place restyles that
//!   profile's sessions (#397). An in-place write to
//!   any *other* theme file is not seen: it cannot change a theme in use.
//!   Every directory met on the way to `config.toml` (those above the
//!   configuration directory, those holding a symbolic link on the way,
//!   including a link to the file itself, and the one where a link is
//!   missing) is registered too, so a retargeted link, one deleted and
//!   created again, or a directory renamed on the way is followed (#384,
//!   #393 F3, #417); an event on one that changes
//!   nothing on the way (a file written in `~`) costs a few `stat`s and
//!   no reload.
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
//! (`MAX_UNLOADABLE_FOLLOWED`) while the configured theme's file does not
//! load, at most eight for the profiles' themes, and one per directory on
//! the way to `config.toml` (at most 32, `MAX_CHAIN_DIRS`, through at most
//! eight symbolic links, `MAX_DIR_LINKS_FOLLOWED`; past either, or in a
//! cycle, it polls `config.toml` instead, every two seconds). The
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
/// file descriptors, plus one per directory on the way to `config.toml`
/// and per profile theme (see the module documentation). Several watchers
/// on one store do work (each sees every event once) but gain nothing over
/// a single one.
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
/// `follow` gets the configuration the outcome leaves in use (whose
/// `appearance.theme` and profile themes are followed) and runs before the
/// outcome is applied or sent (#243, #253 items 1-3). A subscriber may
/// react to the change at once by saving that theme file, and so may any
/// thread that reads [`ConfigStore::config`] once it is applied; a save
/// that leaves a file found by its `name` key unloadable hides it from the
/// lookup, which would then miss the save that fixes it. Following first
/// means such a save lands on a watched file, and the themes directory is
/// sampled after that, so a write between the two is reported here.
fn settle(store: &ConfigStore, follow: impl FnOnce(&Config)) {
    let outcome = store.load_from_disk();
    match &outcome {
        Ok(from) => follow(&from.loaded.config),
        Err(_) => follow(&store.config()),
    }
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

/// Most profile themes whose files the native backend follows for
/// in-place saves ([`profile_theme_files`]): one file descriptor each
/// (S5). `[[profiles]]` holds up to [`crate::MAX_PROFILES`] entries; the
/// themes past the first eight distinct ones are seen only when their
/// file is created, deleted or renamed, or the configuration changes.
pub(crate) const MAX_PROFILE_THEMES_FOLLOWED: usize = 8;

/// The user theme files the profiles of `config` name, for the native
/// backend to follow as it follows `appearance.theme`'s (#397): an
/// in-place save of a profile's theme raises no event on `themes/`, so
/// without a watch on the file the sessions of that profile would keep
/// the old colours. The files of the first [`MAX_PROFILE_THEMES_FOLLOWED`]
/// distinct theme names other than `appearance.theme` that have one, in
/// profile order, each resolved as [`Theme::resolve`] does (else its first
/// stem match, which may be a file that does not load yet). A bundled
/// theme has no file and takes no slot; the themes directory is listed
/// once for all names, and a name that is a bundled theme's reads no file
/// (#417 item 4).
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
fn profile_theme_files(themes: &Path, config: &Config) -> Vec<std::path::PathBuf> {
    // At most `MAX_PROFILES` distinct names (S5).
    let mut names: Vec<&str> = Vec::new();
    for profile in &config.profiles {
        let Some(name) = profile
            .appearance
            .as_ref()
            .and_then(|appearance| appearance.theme.as_deref())
        else {
            continue;
        };
        if name != config.appearance.theme && !names.contains(&name) {
            names.push(name);
        }
    }
    crate::theme::files_for_names(themes, &names, MAX_PROFILE_THEMES_FOLLOWED)
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
    Native(Box<native::Native>),
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
                    let (native, stop) = native::Native::open(store.paths())?;
                    Ok((Self::Native(Box::new(native)), Stop::Pipe { _tx: stop }))
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
                settle(store, |config| {
                    follow_polled_theme(store, baseline, &config.appearance.theme);
                });
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
        settle(store, |config| {
            follow_polled_theme(store, &mut last_seen, &config.appearance.theme);
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

    use super::{WatchOptions, profile_theme_files, theme_files_to_follow};
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
        /// The directories met on the way to `config.toml` ([`chain_dirs`]:
        /// those above the root, those holding a symbolic link on the way,
        /// for a linked `config.toml` those on the way to its target; at
        /// most [`MAX_CHAIN_DIRS`]), each with its registration. Opening the
        /// root follows its links, so its watch is on the current target's
        /// inode; a retargeted link (`ln -sfn`, a home-manager switch), a
        /// directory above the root swapped by a rename, or a linked
        /// `config.toml` whose way to its target changed is seen only here,
        /// and the re-registration it starts follows the new chain (#384,
        /// #393 F3). Empty while the chain cannot be followed (more links or
        /// directories than the caps, a cycle), in which case the thread
        /// polls, slowly ([`CAPPED_CHAIN_POLL_INTERVAL`]).
        chain: Vec<(PathBuf, ChainSlot)>,
        /// Whether the last [`Native::register_chain`] found the chain past
        /// a cap or in a cycle: polling then waits
        /// [`CAPPED_CHAIN_POLL_INTERVAL`] rather than `poll_interval`, since
        /// nothing will arm the backend until the links change (#393 m3).
        chain_capped: bool,
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
        /// The theme files the profiles name ([`profile_theme_files`], at
        /// most [`super::MAX_PROFILE_THEMES_FOLLOWED`]), so an in-place save
        /// of one restyles that profile's sessions (#397), and the handles
        /// of those registered. Best effort, as `unloadable`.
        profile_themes: Vec<PathBuf>,
        profile_theme_handles: Vec<File>,
        /// The error from the most recent [`Native::register_all`] that
        /// could not register every slot, or `None` when it registered
        /// them all. While `Some` the backend is not armed and polls, since
        /// a path it cannot open (an unreadable file, a symlink cycle)
        /// could change without any registered watch noticing. Only a
        /// change is logged, so a file that stays unreadable produces one
        /// warning rather than one per poll.
        register_error: Option<String>,
        /// `config.toml`'s metadata when the last settle began. While the
        /// backend is not armed it compares this with the file on every
        /// poll and settles on a change, as the polling backend does, so a
        /// change that no registered watch can see (a link retargeted
        /// past [`MAX_DIR_LINKS_FOLLOWED`]) is still applied.
        config_seen: super::Fingerprint,
        /// Counts what the thread did, for tests.
        #[cfg(test)]
        probe: std::sync::Arc<tests::Probe>,
    }

    /// The registration of one directory of the chain to `config.toml`.
    #[derive(Debug)]
    pub(crate) enum ChainSlot {
        /// Registered; its events arrive on this handle.
        Armed(File),
        /// Missing, of another type, or not registered after an error: the
        /// backend is not armed and polls.
        Missing,
        /// There but not readable (a directory that can be searched but
        /// not listed): it cannot be watched, and the backend stays armed
        /// without it, as it was before directories above the root were
        /// watched (#393 F3).
        Unreadable,
    }

    impl ChainSlot {
        fn handle(&self) -> Option<&File> {
            match self {
                Self::Armed(handle) => Some(handle),
                Self::Missing | Self::Unreadable => None,
            }
        }
    }

    /// How often the backend polls while the chain to `config.toml` is
    /// past [`MAX_DIR_LINKS_FOLLOWED`] or [`MAX_CHAIN_DIRS`], or in a cycle
    /// (or `poll_interval`, when that is longer): the 2 s at which the
    /// application looks for a missing configuration directory
    /// (`tessera_app_core::CONFIG_DIR_POLL_INTERVAL`). Such a chain stays
    /// so until someone changes the links, so a fast poll would cost CPU
    /// for nothing (#393 m3). An edit in the configuration directory still
    /// applies at once, from the directory's own watch; a retarget of a
    /// link on the way, within about 2 s.
    pub(crate) const CAPPED_CHAIN_POLL_INTERVAL: std::time::Duration =
        std::time::Duration::from_secs(2);

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
        let handle = match crate::load::open_nonblocking(path) {
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

    /// Registers each of `paths` (theme files followed in case they
    /// change in place), skipping, with a debug log, one that cannot be
    /// registered (unreadable, say) rather than making the backend poll,
    /// since it may well be unrelated. Returns the handles registered.
    fn best_effort(queue: &mut kqueue::Watcher, paths: &[PathBuf], what: &str) -> Vec<File> {
        let mut handles = Vec::new();
        for path in paths {
            let mut slot = None;
            match register(queue, &mut slot, path, Kind::File) {
                Ok(()) => handles.extend(slot),
                Err(error) => tracing::debug!(
                    target: "tessera_config",
                    path = %crate::load::escaped(
                        &path.display().to_string(),
                        crate::load::MAX_WARNING_PATH_LEN
                    ),
                    %error,
                    "{what} not watched"
                ),
            }
        }
        handles
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

    /// How many symbolic links [`chain_dirs`] follows on the way to
    /// `config.toml` (the configuration directory's and, for a linked
    /// `config.toml`, its own). A path that needs more, or runs in a
    /// cycle, is not watched through its chain: the backend polls
    /// `config.toml` instead, slowly (see [`Native::armed`],
    /// [`CAPPED_CHAIN_POLL_INTERVAL`]).
    pub(crate) const MAX_DIR_LINKS_FOLLOWED: usize = 8;

    /// How many directories [`chain_dirs`] lists (S5: each costs one
    /// watched descriptor). Past it the backend polls, as past
    /// [`MAX_DIR_LINKS_FOLLOWED`]. Far deeper than any configuration
    /// directory: `~/Library/Application Support/Tessera` has four.
    pub(crate) const MAX_CHAIN_DIRS: usize = 32;

    /// The error [`chain_dirs`] returns past one of its caps.
    #[derive(Debug)]
    pub(crate) struct ChainCapped(String);

    impl ChainCapped {
        fn error(message: String) -> std::io::Error {
            std::io::Error::other(Self(message))
        }

        /// Whether `error` is a [`ChainCapped`].
        pub(crate) fn is(error: &std::io::Error) -> bool {
            matches!(error.get_ref(), Some(inner) if inner.is::<Self>())
        }
    }

    impl std::fmt::Display for ChainCapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl std::error::Error for ChainCapped {}

    /// The directories met while resolving `file` (`config.toml`)
    /// component by component, as the kernel does, each once, in the order
    /// met: every directory that holds a component on the way, except the
    /// one holding the last (the configuration directory, or for a linked
    /// `config.toml` its target's directory; those have watches of their
    /// own). So the directories above the root, the ones holding a
    /// symbolic link on the way (the configuration directory itself a
    /// link, GNU stow's layout, a dotfiles checkout switched with
    /// `ln -sfn`) and, for a linked `config.toml`, the ones on the way to
    /// its target, including one whose link is the last component of a
    /// link's target (`config.toml -> x/current -> y/a.toml` lists `x`).
    /// Retargeting a link, or renaming a directory on the way, changes one
    /// of those directories and nothing the root's own watch sees, since
    /// that follows the old inode (#384, #393 F3, #417).
    ///
    /// Each directory is a resolved path, with no link left in it. A
    /// component that is missing ends the walk (nothing past it exists
    /// yet), and the directory that would hold it is listed, unless it is
    /// the configuration directory holding a missing `config.toml`: its
    /// creation (a link unlinked and created again, as macOS `ln -sfn`
    /// does) is then seen (#417 M-A). A [`ChainCapped`] error for more than
    /// [`MAX_DIR_LINKS_FOLLOWED`] links, which includes every cycle, or
    /// more than [`MAX_CHAIN_DIRS`] directories, and the error of any
    /// other I/O failure but "not found".
    pub(crate) fn chain_dirs(file: &Path) -> std::io::Result<Vec<PathBuf>> {
        Ok(walk_chain(file)?.into_iter().map(|(dir, _)| dir).collect())
    }

    /// [`chain_dirs`], each directory with whether it holds a link the
    /// walk follows.
    fn walk_chain(file: &Path) -> std::io::Result<Vec<(PathBuf, bool)>> {
        use std::collections::VecDeque;
        use std::path::Component;

        let file = std::path::absolute(file)?;
        let mut pending: VecDeque<PathBuf> = file
            .components()
            .map(|component| PathBuf::from(component.as_os_str()))
            .collect();
        let mut resolved = PathBuf::from("/");
        let mut links = 0_usize;
        let mut dirs: Vec<(PathBuf, bool)> = Vec::new();
        // The configuration directory, resolved: where the walk first
        // reaches `config.toml`. It has a watch of its own, so it is listed
        // only as a directory on the way (a chain through it).
        let mut root: Option<PathBuf> = None;
        while let Some(part) = pending.pop_front() {
            let Some(component) = part.components().next() else {
                continue;
            };
            let name = match component {
                Component::RootDir | Component::Prefix(_) => {
                    resolved = PathBuf::from("/");
                    continue;
                }
                Component::CurDir => continue,
                Component::ParentDir => {
                    resolved.pop();
                    continue;
                }
                Component::Normal(name) => name,
            };
            let last = pending.is_empty();
            if last && root.is_none() {
                root = Some(resolved.clone());
            }
            let candidate = resolved.join(name);
            let meta = match std::fs::symlink_metadata(&candidate) {
                Ok(meta) => Some(meta),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => break,
                Err(e) => return Err(e),
            };
            let is_link = meta.as_ref().is_some_and(std::fs::Metadata::is_symlink);
            // Listed: a directory passed through, and one whose entry is a
            // link or missing even when that entry is the last (the link
            // expanded first: `config.toml -> x/current -> y/a.toml` lists
            // `x`), since retargeting or creating that entry changes
            // nothing else watched (#417). Not the configuration directory
            // for its last entry, nor the directory holding the final
            // target (both have watches of their own).
            let listed = !last || ((is_link || meta.is_none()) && root.as_ref() != Some(&resolved));
            if listed && !dirs.iter().any(|(dir, _)| *dir == resolved) {
                if dirs.len() == MAX_CHAIN_DIRS {
                    return Err(ChainCapped::error(format!(
                        "more than {MAX_CHAIN_DIRS} directories on the way to config.toml"
                    )));
                }
                dirs.push((resolved.clone(), false));
            }
            if meta.is_none() {
                // Nothing past a missing component exists yet; the
                // directory that would hold it is listed above, so its
                // creation (the second half of an unlink-and-symlink
                // retarget, `ln -sfn` on macOS) raises an event (#417 M-A).
                break;
            }
            if !is_link {
                resolved = candidate;
                continue;
            }
            if let Some((_, holds_link)) = dirs.iter_mut().find(|(dir, _)| *dir == resolved) {
                *holds_link = true;
            }
            links += 1;
            if links > MAX_DIR_LINKS_FOLLOWED {
                return Err(ChainCapped::error(format!(
                    "more than {MAX_DIR_LINKS_FOLLOWED} symbolic links (or a cycle) on the way to config.toml"
                )));
            }
            let target = std::fs::read_link(&candidate)?;
            for part in target.components().rev() {
                pending.push_front(PathBuf::from(part.as_os_str()));
            }
        }
        Ok(dirs)
    }

    /// Whether `handle` (the registration of `path`) still names what
    /// `path` names now: `None` while `path` is missing.
    fn names_now(path: &Path, handle: Option<&File>) -> bool {
        match handle {
            Some(handle) => still_names(path, handle).unwrap_or(false),
            None => matches!(
                std::fs::metadata(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound
            ),
        }
    }

    impl Native {
        /// Registers the directory, the file (if present), the themes
        /// directory and a stop pipe. Dropping the returned writer wakes
        /// the thread. A path that cannot be registered yet (an unreadable
        /// `config.toml`, say) is not a failure: the thread polls for it,
        /// as [`ConfigStore::open`] tolerates the same conditions.
        ///
        /// The theme files are not looked up here: the catch-up settle
        /// that `Watcher::spawn` runs next follows them before anything is
        /// published, so a lookup here (up to 256 files, on the caller's
        /// thread, the main thread at launch) would be done twice (#283
        /// item 2).
        pub(super) fn open(paths: &ConfigPaths) -> std::io::Result<(Self, PipeWriter)> {
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
                chain: Vec::new(),
                chain_capped: false,
                file: paths.config_file(),
                file_handle: None,
                link_dir: None,
                link_dir_handle: None,
                themes: paths.themes_dir(),
                themes_handle: None,
                theme_file: None,
                theme_file_handle: None,
                unloadable: Vec::new(),
                unloadable_handles: Vec::new(),
                profile_themes: Vec::new(),
                profile_theme_handles: Vec::new(),
                register_error: None,
                config_seen: super::Fingerprint::Missing,
                #[cfg(test)]
                probe: std::sync::Arc::default(),
            };
            native.rearm();
            Ok((native, stop))
        }

        /// Re-registers the directories on the way to `config.toml`
        /// ([`chain_dirs`]), the directory, (for a symlinked file) the target's
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
            // The chain first: a retarget landing while the root is
            // re-registered below then either precedes the root's open
            // (which follows the new target) or raises an event on an
            // armed directory of the chain. That holds for a link unlinked
            // and created again too: while it is missing, the walk still
            // lists the directory that holds it (#417 M-A).
            note(self.register_chain());
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

        /// Registers the directories on the way to `config.toml`
        /// ([`chain_dirs`]), replacing the previous registrations.
        ///
        /// The chain is resolved again once they are registered, and
        /// registered again (keeping the handles of directories still in
        /// it, so their pending events survive) until two resolutions agree.
        /// A retarget that lands after a directory's registration raises an
        /// event on it; one that lands before (an atomic `ln -sfn`, whose
        /// temporary link woke the thread just before its rename) changes
        /// the next resolution, so a directory the new chain brings in is
        /// not left unwatched. More than [`MAX_DIR_LINKS_FOLLOWED`] + 1
        /// resolutions that keep changing, a cap or cycle error, or an I/O
        /// error leave the backend unarmed, polling (an error, see
        /// [`Native::armed`]). After changing resolutions it gives up and
        /// polls at `poll_interval`, not at the slower
        /// [`CAPPED_CHAIN_POLL_INTERVAL`] of a chain past the caps: a
        /// retarget storm is transient, and each poll rearms, so the watch
        /// is armed again at the first poll after it ends (#417 item 8). A
        /// directory that cannot be read is not watched and does not unarm
        /// it ([`ChainSlot::Unreadable`]).
        fn register_chain(&mut self) -> std::io::Result<()> {
            // After an event every directory is registered anew: one may
            // have been replaced, and its handle would be stale.
            let stale = std::mem::take(&mut self.chain);
            self.drop_chain(stale);
            self.chain_capped = false;
            let mut registered = Vec::new();
            let mut failed = None;
            let mut resolved = walk_chain(&self.file);
            for _ in 0..=MAX_DIR_LINKS_FOLLOWED {
                let dirs = match resolved {
                    Ok(dirs) => dirs,
                    Err(error) => {
                        self.chain_capped = ChainCapped::is(&error);
                        self.drop_chain(registered);
                        return Err(error);
                    }
                };
                for (dir, holds_link) in dirs {
                    let slot = if let Some(at) =
                        registered.iter().position(|(held, _)| *held == dir)
                    {
                        registered.swap_remove(at).1
                    } else {
                        let mut slot = None;
                        match register(&mut self.queue, &mut slot, &dir, Kind::Dir) {
                            Ok(()) => slot.map_or(ChainSlot::Missing, ChainSlot::Armed),
                            // Best effort for a directory only passed
                            // through. One holding a link must be watched
                            // (a retarget changes nothing below it), so
                            // that fails the registration and the backend
                            // polls (#393 B1).
                            Err(error)
                                if error.kind() == std::io::ErrorKind::PermissionDenied
                                    && !holds_link =>
                            {
                                tracing::debug!(
                                    target: "tessera_config",
                                    path = %crate::load::escaped(
                                        &dir.display().to_string(),
                                        crate::load::MAX_WARNING_PATH_LEN
                                    ),
                                    "directory on the way to config.toml not readable; not watched"
                                );
                                ChainSlot::Unreadable
                            }
                            Err(error) => {
                                failed.get_or_insert(error);
                                ChainSlot::Missing
                            }
                        }
                    };
                    self.chain.push((dir, slot));
                }
                self.drop_chain(std::mem::take(&mut registered));
                resolved = walk_chain(&self.file);
                if resolved.as_ref().is_ok_and(|now| {
                    now.iter()
                        .map(|(dir, _)| dir)
                        .eq(self.chain.iter().map(|(dir, _)| dir))
                }) {
                    return failed.map_or(Ok(()), Err);
                }
                registered = std::mem::take(&mut self.chain);
            }
            self.drop_chain(registered);
            Err(std::io::Error::other(
                "the symbolic links on the way to config.toml kept changing",
            ))
        }

        /// Removes the registrations of `chain`, closing their handles.
        fn drop_chain(&mut self, chain: Vec<(PathBuf, ChainSlot)>) {
            for (_, slot) in chain {
                if let ChainSlot::Armed(handle) = slot {
                    let _ = self.queue.remove_file(&handle, EventFilter::EVFILT_VNODE);
                }
            }
        }

        /// Whether `event` is on a directory of the chain and changed
        /// nothing that leads to `config.toml`: the chain resolves to the
        /// same directories, and every registered handle on the way (the
        /// chain's, the root's, a linked file's target directory's and the
        /// file's) still names what its path names. Such an event (a file
        /// written next to a link, in `~` or a dotfiles checkout, say) needs
        /// neither a re-registration nor a reload (#393 m2): a few `stat`s
        /// instead of a dozen opens and a load.
        #[cfg(test)]
        fn changes_nothing(&self, event: &Event) -> bool {
            self.on_chain(event) && self.chain_unchanged()
        }

        /// Whether `event` is on a registered directory of the chain.
        fn on_chain(&self, event: &Event) -> bool {
            let Ident::Fd(fd) = event.ident else {
                return false;
            };
            self.chain
                .iter()
                .any(|(_, slot)| slot.handle().is_some_and(|handle| handle.as_raw_fd() == fd))
        }

        /// Whether nothing that leads to `config.toml` changed (the test
        /// `changes_nothing` makes after its event): walks the chain once.
        fn chain_unchanged(&self) -> bool {
            let Ok(now) = chain_dirs(&self.file) else {
                return false;
            };
            now.iter().eq(self.chain.iter().map(|(dir, _)| dir))
                && self.chain.iter().all(|(dir, slot)| match slot {
                    ChainSlot::Armed(handle) => still_names(dir, handle).unwrap_or(false),
                    ChainSlot::Missing => false,
                    ChainSlot::Unreadable => true,
                })
                && names_now(&self.root, self.root_handle.as_ref())
                && link_dir_of(&self.file) == self.link_dir
                && self
                    .link_dir
                    .as_deref()
                    .is_none_or(|dir| names_now(dir, self.link_dir_handle.as_ref()))
                && names_now(&self.file, self.file_handle.as_ref())
        }

        /// Whether a registration is incomplete: one failed
        /// ([`Native::register_error`]), or `themes/` is a directory the
        /// backend holds no watch on. A chain event cannot repair either
        /// without a rearm, though the chain itself is unchanged (#417 M-B).
        fn registration_incomplete(&self) -> bool {
            self.register_error.is_some()
                || (self.themes_handle.is_none()
                    && std::fs::metadata(&self.themes).is_ok_and(|meta| meta.is_dir()))
        }

        /// Registers the theme file in use, the unloadable ones followed
        /// with it and the profiles' theme files, or drops the
        /// registrations of the previous ones. Touches no other slot, so
        /// pending directory events survive it.
        fn register_theme_file(&mut self) -> std::io::Result<()> {
            for old in self
                .unloadable_handles
                .drain(..)
                .chain(self.profile_theme_handles.drain(..))
            {
                let _ = self.queue.remove_file(&old, EventFilter::EVFILT_VNODE);
            }
            let unloadable =
                best_effort(&mut self.queue, &self.unloadable, "unloadable theme file");
            self.unloadable_handles = unloadable;
            let profiles = best_effort(&mut self.queue, &self.profile_themes, "profile theme file");
            self.profile_theme_handles = profiles;
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
        /// the root, a directory on the way to `config.toml` or a symlink
        /// target's directory is missing, or a path exists but could not be
        /// registered (an unreadable `config.toml` is rewritten in place
        /// without any directory event; the chain is past
        /// [`MAX_DIR_LINKS_FOLLOWED`] or [`MAX_CHAIN_DIRS`], or a cycle;
        /// a directory on the way that cannot be read is skipped, see
        /// [`ChainSlot::Unreadable`]), the thread polls
        /// instead (past a cap, every [`CAPPED_CHAIN_POLL_INTERVAL`]): it
        /// re-reads the file once it is armed again, or as soon as the
        /// file's metadata (followed through every link) changes.
        fn armed(&self) -> bool {
            self.root_handle.is_some()
                && self
                    .chain
                    .iter()
                    .all(|(_, slot)| !matches!(slot, ChainSlot::Missing))
                && (self.link_dir.is_none() || self.link_dir_handle.is_some())
                && self.register_error.is_none()
        }

        /// Refreshes the registrations after an event: every slot of
        /// [`Native::register_all`], the directories on the way to
        /// `config.toml` included ([`Native::register_chain`]), so a chain
        /// that changed is followed too. A failure is logged once when it
        /// first appears (or changes) and the recovery once, since while it
        /// lasts this runs every `poll_interval`.
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
                let interval = if self.chain_capped {
                    options.poll_interval.max(CAPPED_CHAIN_POLL_INTERVAL)
                } else {
                    options.poll_interval
                };
                #[cfg(test)]
                self.probe.note_poll();
                if let Some(event) = self.queue.poll(Some(interval)) {
                    return Some(event);
                }
                self.rearm();
                if self.armed() || super::Fingerprint::of(&self.file) != self.config_seen {
                    // The directory is back (it may already hold a file),
                    // or the file became readable; either way it may have
                    // changed unseen. Or it changed while nothing could
                    // see it (a link past the cap retargeted).
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
            // Before the load: a change after this sample is either read
            // by the load or seen as a difference on the next poll.
            self.config_seen = super::Fingerprint::of(&self.file);
            #[cfg(test)]
            self.probe.note_settle();
            super::settle(store, |config| self.follow(config));
        }

        /// Follows the theme files `config`'s `appearance.theme` and its
        /// profiles' themes resolve to now.
        fn follow(&mut self, config: &crate::Config) {
            let (file, unloadable) = theme_files_to_follow(
                &self.themes,
                &config.appearance.theme,
                self.theme_file.as_deref(),
            );
            self.profile_themes = profile_theme_files(&self.themes, config)
                .into_iter()
                .filter(|path| Some(path) != file.as_ref() && !unloadable.contains(path))
                .collect();
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
                // An event on a directory on the way to `config.toml` (`~`,
                // a dotfiles checkout) is mostly unrelated: such events are
                // debounced like the others and the chain is walked once at
                // the end, re-registered and reloaded only if something on
                // the way changed (`changes_nothing`, #393 m2). They never
                // extend the wait, so a stream of them costs one walk per
                // `debounce` and holds back neither an edit of the file nor
                // a change on the way (a busy `$TMPDIR` or `~` is common).
                let mut changed = !self.on_chain(&event);
                let mut chain_seen = !changed;
                if changed {
                    self.rearm();
                    #[cfg(test)]
                    self.probe.note_rearm();
                }

                // Debounce: wait until the file has been quiet, but no
                // longer than `max_wait` in all. `max_wait` is clamped to
                // `MAX_WATCH_DURATION`, so the deadline is representable; a
                // failure to represent it just skips the debounce.
                let deadline = Instant::now()
                    .checked_add(options.max_wait)
                    .unwrap_or_else(Instant::now);
                let quiet_after =
                    |now: Instant| now.checked_add(options.debounce).unwrap_or(deadline);
                let mut quiet_at = quiet_after(Instant::now());
                loop {
                    let remaining = quiet_at
                        .min(deadline)
                        .saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    match self.queue.poll(Some(remaining)) {
                        None => {}
                        Some(event) if self.is_stop(&event) => return,
                        Some(event) if self.on_chain(&event) => chain_seen = true,
                        Some(_) => {
                            changed = true;
                            self.rearm();
                            #[cfg(test)]
                            self.probe.note_rearm();
                            quiet_at = quiet_after(Instant::now());
                        }
                    }
                }
                if chain_seen && !self.chain_unchanged() {
                    changed = true;
                    self.rearm();
                    #[cfg(test)]
                    self.probe.note_rearm();
                } else if chain_seen && !changed && self.registration_incomplete() {
                    // A registration failed (an unreadable `themes/` or
                    // theme file, say), so `next_event` polls, but chain
                    // events keep it from timing out and rearming: recover
                    // here as that poll would (#417 M-B).
                    self.rearm();
                    #[cfg(test)]
                    self.probe.note_rearm();
                    changed =
                        self.armed() || super::Fingerprint::of(&self.file) != self.config_seen;
                }
                if changed {
                    self.settle(store);
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::theme::Theme;
        use crate::watch::ConfigEvent;
        use crate::watch::MAX_PROFILE_THEMES_FOLLOWED;
        use std::fmt::Write as _;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

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
            let (mut native, _stop) = Native::open(&paths).unwrap();
            native.settle(&store);
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
            let fd = native.theme_file_handle.as_ref().unwrap().as_raw_fd();
            assert!(
                event_on(&mut native, fd).is_some(),
                "no event for the fix on the theme file ({fd})"
            );
        }

        /// The next event on `fd` within 5 s, skipping others (the
        /// directories above a temporary directory are on the chain, and
        /// other tests write in them).
        fn event_on(native: &mut Native, fd: RawFd) -> Option<Event> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let event = native.queue.poll(Some(left))?;
                if matches!(event.ident, Ident::Fd(got) if got == fd) {
                    return Some(event);
                }
            }
        }

        /// The handle's descriptor of `dir`'s chain slot.
        fn chain_fd(native: &Native, dir: &Path) -> RawFd {
            native
                .chain
                .iter()
                .find(|(held, _)| held == dir)
                .and_then(|(_, slot)| slot.handle())
                .unwrap_or_else(|| panic!("{dir:?} is not armed on the chain"))
                .as_raw_fd()
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

        /// A temporary directory by its resolved path (`/var` is a link on
        /// macOS, which would add `/private` to the [`chain_dirs`] tests'
        /// paths).
        fn resolved_tempdir() -> (tempfile::TempDir, PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let base = std::fs::canonicalize(dir.path()).unwrap();
            (dir, base)
        }

        /// A chain of `links` symbolic links, each in a directory of its
        /// own (`d0/l -> d1/l -> ... -> gen/tessera`); returns the first.
        fn link_chain(base: &Path, links: usize) -> PathBuf {
            std::fs::create_dir_all(base.join("gen").join("tessera")).unwrap();
            let mut target = base.join("gen").join("tessera");
            for i in (0..links).rev() {
                let dir = base.join(format!("d{i}"));
                std::fs::create_dir(&dir).unwrap();
                std::os::unix::fs::symlink(&target, dir.join("l")).unwrap();
                target = dir.join("l");
            }
            target
        }

        /// [`chain_dirs`] of `root`'s `config.toml`, the entries under
        /// `base` only (the ones above it are the temporary directory's).
        fn chain_under(base: &Path, root: &Path) -> Vec<PathBuf> {
            let all = chain_dirs(&root.join("config.toml")).unwrap();
            let above: Vec<&PathBuf> = all.iter().filter(|dir| !dir.starts_with(base)).collect();
            assert!(
                base.ancestors()
                    .skip(1)
                    .all(|dir| above.iter().any(|seen| *seen == dir)),
                "every directory above {base:?} is listed: {all:?}"
            );
            all.iter()
                .filter(|dir| dir.starts_with(base))
                .cloned()
                .collect()
        }

        /// The directories of a [`Native`]'s chain under `base`.
        fn watched_under(native: &Native, base: &Path) -> Vec<PathBuf> {
            native
                .chain
                .iter()
                .map(|(dir, _)| dir.clone())
                .filter(|dir| dir.starts_with(base))
                .collect()
        }

        /// #384, #393 F3: every directory met while resolving
        /// `config.toml`, each once, except the one holding its last
        /// component: those above the root, those holding a link on the
        /// way (relative targets resolved against the link's directory)
        /// and, for a linked `config.toml`, those on the way to its target.
        #[test]
        fn chain_dirs_lists_each_directory_on_the_way_to_config_toml() {
            let (_dir, base) = resolved_tempdir();
            std::fs::create_dir_all(base.join("gen1").join("tessera")).unwrap();
            std::fs::create_dir_all(base.join("plain")).unwrap();
            assert_eq!(chain_under(&base, &base.join("plain")), vec![base.clone()]);
            std::fs::write(base.join("plain").join("config.toml"), "").unwrap();
            assert_eq!(
                chain_under(&base, &base.join("plain")),
                vec![base.clone()],
                "the root holds config.toml and has a watch of its own"
            );

            std::os::unix::fs::symlink(base.join("gen1").join("tessera"), base.join("Tessera"))
                .unwrap();
            assert_eq!(
                chain_under(&base, &base.join("Tessera")),
                vec![base.clone(), base.join("gen1")]
            );

            // An ancestor that is a relative link into a directory holding
            // another (absolute) link.
            std::fs::create_dir_all(base.join("home")).unwrap();
            std::fs::create_dir_all(base.join("store").join("dot")).unwrap();
            std::os::unix::fs::symlink("../store/dot", base.join("home").join("dotfiles")).unwrap();
            std::os::unix::fs::symlink(
                base.join("gen1").join("tessera"),
                base.join("store").join("dot").join("tessera"),
            )
            .unwrap();
            let root = base.join("home").join("dotfiles").join("tessera");
            assert_eq!(
                chain_under(&base, &root),
                vec![
                    base.clone(),
                    base.join("home"),
                    base.join("store"),
                    base.join("store").join("dot"),
                    base.join("gen1"),
                ]
            );

            // Two links in one directory are one entry.
            std::os::unix::fs::symlink(base.join("Tessera"), base.join("again")).unwrap();
            assert_eq!(
                chain_under(&base, &base.join("again")),
                vec![base.clone(), base.join("gen1")]
            );

            // A dangling link ends the walk; its directory is still listed.
            std::os::unix::fs::symlink(base.join("missing"), base.join("dangling")).unwrap();
            assert_eq!(
                chain_under(&base, &base.join("dangling").join("tessera")),
                vec![base.clone()]
            );

            // A linked config.toml: the directories on the way to its
            // target, but not the target's own (the link directory's watch).
            std::fs::create_dir_all(base.join("dots").join("gen1")).unwrap();
            std::fs::write(base.join("dots").join("gen1").join("config.toml"), "").unwrap();
            std::os::unix::fs::symlink("gen1", base.join("dots").join("current")).unwrap();
            std::fs::create_dir_all(base.join("linked")).unwrap();
            std::os::unix::fs::symlink(
                "../dots/current/config.toml",
                base.join("linked").join("config.toml"),
            )
            .unwrap();
            assert_eq!(
                chain_under(&base, &base.join("linked")),
                vec![base.clone(), base.join("dots")]
            );

            // #417: `config.toml -> x/current -> y/a.toml`: `x` holds a
            // link that is the last component of a link's target; it is
            // listed (expanded first), `y` (the target's own) is not.
            let xc = base.join("xc");
            let root = x_current_chain(&xc);
            assert_eq!(
                chain_under(&base, &root),
                vec![base.clone(), xc.clone(), xc.join("x")]
            );

            // #417 M-A: a link on the way that is missing (between the
            // unlink and the symlink of macOS `ln -sfn`) still lists the
            // directory that held it, and so does a missing `config.toml`
            // target; a missing `config.toml` in the root lists nothing
            // more.
            std::fs::create_dir_all(base.join("swap").join("gen1").join("tessera")).unwrap();
            let swapped = base.join("swap").join("cfg").join("tessera");
            assert_eq!(
                chain_under(&base, &swapped),
                vec![base.clone(), base.join("swap")],
                "the link is missing"
            );
            std::os::unix::fs::symlink("gen1", base.join("swap").join("cfg")).unwrap();
            assert_eq!(
                chain_under(&base, &swapped),
                vec![
                    base.clone(),
                    base.join("swap"),
                    base.join("swap").join("gen1")
                ],
            );
            std::fs::remove_file(xc.join("y").join("a.toml")).unwrap();
            assert_eq!(
                chain_under(&base, &root),
                vec![base.clone(), xc.clone(), xc.join("x"), xc.join("y")],
                "a missing target lists its directory"
            );
        }

        /// `base/Tessera/config.toml -> ../x/current -> ../y/a.toml`, with
        /// `y/b.toml` beside it; returns the root, `base/Tessera`.
        fn x_current_chain(base: &Path) -> PathBuf {
            let root = base.join("Tessera");
            for dir in [&root, &base.join("x"), &base.join("y")] {
                std::fs::create_dir_all(dir).unwrap();
            }
            std::fs::write(base.join("y").join("a.toml"), sized(11)).unwrap();
            std::fs::write(base.join("y").join("b.toml"), sized(22)).unwrap();
            std::os::unix::fs::symlink("../y/a.toml", base.join("x").join("current")).unwrap();
            std::os::unix::fs::symlink("../x/current", root.join("config.toml")).unwrap();
            root
        }

        /// `links` symbolic links to `config.toml`'s target, each the last
        /// component of the one before and each in a directory of its own
        /// (`root/config.toml -> f0/c -> f1/c -> ... -> gen/config.toml`);
        /// returns the root.
        fn file_link_chain(base: &Path, links: usize) -> PathBuf {
            let root = base.join("root");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(base.join("gen")).unwrap();
            std::fs::write(base.join("gen").join("config.toml"), sized(11)).unwrap();
            let mut target = base.join("gen").join("config.toml");
            for i in (1..links).rev() {
                let dir = base.join(format!("f{i}"));
                std::fs::create_dir(&dir).unwrap();
                std::os::unix::fs::symlink(&target, dir.join("c")).unwrap();
                target = dir.join("c");
            }
            std::os::unix::fs::symlink(&target, root.join("config.toml")).unwrap();
            root
        }

        /// S5: at most [`MAX_DIR_LINKS_FOLLOWED`] links and
        /// [`MAX_CHAIN_DIRS`] directories; one more, or a cycle, is a
        /// [`ChainCapped`] error rather than an endless walk.
        #[test]
        fn chain_dirs_refuses_more_than_the_caps_and_cycles() {
            let (_dir, base) = resolved_tempdir();
            let at_cap = link_chain(&base.join("at"), MAX_DIR_LINKS_FOLLOWED);
            assert!(chain_dirs(&at_cap.join("config.toml")).is_ok());
            let past = link_chain(&base.join("past"), MAX_DIR_LINKS_FOLLOWED + 1);
            let error = chain_dirs(&past.join("config.toml")).unwrap_err();
            assert!(ChainCapped::is(&error), "{error}");
            assert!(error.to_string().contains("symbolic links"), "{error}");

            std::os::unix::fs::symlink(base.join("c2"), base.join("c1")).unwrap();
            std::os::unix::fs::symlink(base.join("c1"), base.join("c2")).unwrap();
            for root in [base.join("c1").join("tessera"), base.join("c1")] {
                let error = chain_dirs(&root.join("config.toml")).unwrap_err();
                assert!(ChainCapped::is(&error), "{error}");
            }

            let above = base.ancestors().count();
            let mut deep = base.join("deep");
            for i in 0..MAX_CHAIN_DIRS - above {
                deep.push(format!("{i}"));
            }
            std::fs::create_dir_all(&deep).unwrap();
            assert_eq!(
                chain_dirs(&deep.join("config.toml")).unwrap().len(),
                MAX_CHAIN_DIRS
            );
            std::fs::create_dir(deep.join("more")).unwrap();
            let error = chain_dirs(&deep.join("more").join("config.toml")).unwrap_err();
            assert!(ChainCapped::is(&error), "{error}");
            assert!(error.to_string().contains("directories"), "{error}");

            // #417: the directories now listed (a link that is a target's
            // last component, the directory of a missing component) count
            // against the same caps.
            let at_cap = file_link_chain(&base.join("files-at"), MAX_DIR_LINKS_FOLLOWED);
            let listed = chain_dirs(&at_cap.join("config.toml")).unwrap();
            assert!(listed.len() <= MAX_CHAIN_DIRS, "{listed:?}");
            assert_eq!(
                listed
                    .iter()
                    .filter(|dir| dir.starts_with(base.join("files-at")))
                    .count(),
                MAX_DIR_LINKS_FOLLOWED,
                "the root's parent and every link's directory but the root: {listed:?}"
            );
            let past = file_link_chain(&base.join("files-past"), MAX_DIR_LINKS_FOLLOWED + 1);
            let error = chain_dirs(&past.join("config.toml")).unwrap_err();
            assert!(ChainCapped::is(&error), "{error}");
            assert!(error.to_string().contains("symbolic links"), "{error}");
            let error = chain_dirs(&deep.join("gone").join("config.toml")).unwrap_err();
            assert!(
                ChainCapped::is(&error),
                "the directory of a missing component counts: {error}"
            );
        }

        thread_local! {
            /// For [`retarget_current_once`]: the `dots` directory whose
            /// `current` link it switches to `gen2`, once.
            static SWITCH_IN: std::cell::RefCell<Option<PathBuf>> =
                const { std::cell::RefCell::new(None) };
        }

        /// A [`BEFORE_OPEN`] hook: the first time any path is about to be
        /// registered, switches `dots/current` to `gen2` atomically (a new
        /// link renamed over it, as GNU `ln -sfn` does).
        fn retarget_current_once(_: &Path) {
            if let Some(dots) = SWITCH_IN.with(|dots| dots.borrow_mut().take()) {
                retarget(&dots.join("current"), "gen2");
            }
        }

        /// Points the link `link` at `target` atomically (a new link
        /// renamed over it, as GNU `ln -sfn` does).
        fn retarget(link: &Path, target: &str) {
            let tmp = link.with_extension("tmp");
            std::os::unix::fs::symlink(target, &tmp).unwrap();
            std::fs::rename(&tmp, link).unwrap();
        }

        /// #393 m1: a switch landing after the chain was resolved and
        /// before its first directory is registered (GNU `ln -sfn`: the
        /// temporary link wakes the thread, the rename follows at once)
        /// brings in directories the first resolution did not list. They
        /// are registered too: the chain is resolved again until the
        /// registered directories are the current ones. Deterministic: the
        /// switch runs in the window, on the registering thread.
        #[test]
        fn a_switch_during_the_registration_watches_the_new_chain() {
            let (_dir, base) = resolved_tempdir();
            let (home, dots, store) = (base.join("home"), base.join("dots"), base.join("store"));
            for generation in ["gen1", "gen2"] {
                std::fs::create_dir_all(dots.join(generation).join("config")).unwrap();
                std::fs::create_dir_all(store.join(generation)).unwrap();
                std::os::unix::fs::symlink(
                    format!("../../../store/{generation}"),
                    dots.join(generation).join("config").join("tessera"),
                )
                .unwrap();
            }
            std::fs::create_dir_all(&home).unwrap();
            std::os::unix::fs::symlink("gen1", dots.join("current")).unwrap();
            std::os::unix::fs::symlink("../dots/current/config", home.join("cfg")).unwrap();
            let root = home.join("cfg").join("tessera");
            let chain = |generation: &str| {
                vec![
                    base.clone(),
                    home.clone(),
                    dots.clone(),
                    dots.join(generation),
                    dots.join(generation).join("config"),
                    store.clone(),
                ]
            };
            assert_eq!(chain_under(&base, &root), chain("gen1"));

            SWITCH_IN.with(|switch| *switch.borrow_mut() = Some(dots.clone()));
            BEFORE_OPEN.with(|hook| hook.set(Some(retarget_current_once)));
            let opened = Native::open(&ConfigPaths::new(&root));
            BEFORE_OPEN.with(|hook| hook.set(None));
            let (native, _stop) = opened.unwrap();
            assert!(
                SWITCH_IN.with(|switch| switch.borrow().is_none()),
                "the hook switched"
            );
            assert_eq!(
                watched_under(&native, &base),
                chain("gen2"),
                "the new generation's directories are watched"
            );
            assert!(native.armed());
        }

        /// S5: the watcher holds one descriptor per directory of the chain
        /// up to the cap, and none past it (the backend polls instead,
        /// slowly).
        #[test]
        fn the_chain_watch_is_capped() {
            let (_dir, base) = resolved_tempdir();
            let at_cap = link_chain(&base.join("at"), MAX_DIR_LINKS_FOLLOWED);
            let (native, _stop) = Native::open(&ConfigPaths::new(&at_cap)).unwrap();
            assert!(native.chain.len() <= MAX_CHAIN_DIRS);
            assert!(
                native
                    .chain
                    .iter()
                    .all(|(_, slot)| matches!(slot, ChainSlot::Armed(_)))
            );
            assert!(!native.chain_capped);
            assert!(native.armed());

            let past = link_chain(&base.join("past"), MAX_DIR_LINKS_FOLLOWED + 1);
            let (native, _stop) = Native::open(&ConfigPaths::new(&past)).unwrap();
            assert!(native.chain.is_empty());
            assert!(native.chain_capped);
            assert!(native.root_handle.is_some(), "the root itself is watched");
            assert!(!native.armed(), "polls past the cap");
        }

        /// S5 after #417: a chain of file links at the link cap (each
        /// link's directory now watched) still holds at most
        /// [`MAX_CHAIN_DIRS`] directories and stays armed; one link more
        /// polls.
        #[test]
        fn the_chain_watch_of_file_links_is_capped() {
            let (_dir, base) = resolved_tempdir();
            let at_cap = file_link_chain(&base.join("at"), MAX_DIR_LINKS_FOLLOWED);
            let (native, _stop) = Native::open(&ConfigPaths::new(&at_cap)).unwrap();
            assert!(native.chain.len() <= MAX_CHAIN_DIRS);
            assert_eq!(
                watched_under(&native, &base.join("at")).len(),
                MAX_DIR_LINKS_FOLLOWED
            );
            assert!(!native.chain_capped);
            assert!(native.armed());

            let past = file_link_chain(&base.join("past"), MAX_DIR_LINKS_FOLLOWED + 1);
            let (native, _stop) = Native::open(&ConfigPaths::new(&past)).unwrap();
            assert!(native.chain.is_empty());
            assert!(native.chain_capped);
            assert!(!native.armed(), "polls past the cap");
        }

        /// What a running [`Native`] did, counted on its own thread.
        #[derive(Debug, Default)]
        pub(crate) struct Probe {
            polls: AtomicUsize,
            settles: AtomicUsize,
            rearms: AtomicUsize,
        }

        impl Probe {
            pub(super) fn note_poll(&self) {
                self.polls.fetch_add(1, Ordering::Relaxed);
            }

            pub(super) fn note_settle(&self) {
                self.settles.fetch_add(1, Ordering::Relaxed);
            }

            pub(super) fn note_rearm(&self) {
                self.rearms.fetch_add(1, Ordering::Relaxed);
            }

            fn polls(&self) -> usize {
                self.polls.load(Ordering::Relaxed)
            }

            fn settles(&self) -> usize {
                self.settles.load(Ordering::Relaxed)
            }

            fn rearms(&self) -> usize {
                self.rearms.load(Ordering::Relaxed)
            }
        }

        /// A [`Native`] watching `root` on a thread of its own, as
        /// `Watcher::spawn` runs it (catch-up settle first), with its probe.
        /// Dropping the writer stops it.
        struct Running {
            store: ConfigStore,
            probe: std::sync::Arc<Probe>,
            stop: Option<PipeWriter>,
            thread: Option<std::thread::JoinHandle<()>>,
        }

        impl Drop for Running {
            fn drop(&mut self) {
                drop(self.stop.take());
                if let Some(thread) = self.thread.take() {
                    thread.join().unwrap();
                }
            }
        }

        /// Starts a [`Running`] on `root` with a 50 ms poll interval.
        fn run_native(root: &Path, check: impl FnOnce(&Native)) -> Running {
            run_native_waiting(root, WatchOptions::default().max_wait, check)
        }

        /// [`run_native`] with `max_wait`.
        fn run_native_waiting(
            root: &Path,
            max_wait: Duration,
            check: impl FnOnce(&Native),
        ) -> Running {
            let paths = ConfigPaths::new(root);
            let store = ConfigStore::open(paths.clone()).unwrap();
            let (mut native, stop) = Native::open(&paths).unwrap();
            native.settle(&store);
            check(&native);
            let probe = std::sync::Arc::clone(&native.probe);
            let options = WatchOptions {
                backend: crate::WatchBackend::Native,
                poll_interval: Duration::from_millis(50),
                max_wait,
                ..WatchOptions::default()
            }
            .clamped();
            let thread_store = store.clone();
            let thread = std::thread::spawn(move || native.run(&thread_store, &options));
            Running {
                store,
                probe,
                stop: Some(stop),
                thread: Some(thread),
            }
        }

        /// A configuration whose font size is `size`.
        fn sized(size: u8) -> String {
            format!("[appearance]\nfont_size = {size}\n")
        }

        /// Waits up to `within` for the store to apply font size `size`.
        fn applies(running: &Running, size: u8, within: Duration) -> bool {
            let started = Instant::now();
            loop {
                if (running.store.config().appearance.font_size - f32::from(size)).abs()
                    < f32::EPSILON
                {
                    return true;
                }
                if started.elapsed() > within {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// Far below [`CAPPED_CHAIN_POLL_INTERVAL`], so only an event
        /// applies a change in time, and generous for a loaded machine.
        const APPLIED_WITHIN: Duration = Duration::from_secs(1);

        /// #393 F3: `config.toml` a link through a directory link that is
        /// retargeted (a dotfiles checkout switched with `ln -sfn`) is
        /// applied from an event, not a poll: the directory holding the
        /// retargeted link is on the chain.
        #[test]
        fn a_config_toml_linked_through_a_retargeted_directory_link_is_applied() {
            let (_dir, base) = resolved_tempdir();
            let dots = base.join("dots");
            for (generation, size) in [("gen1", 11), ("gen2", 22)] {
                std::fs::create_dir_all(dots.join(generation)).unwrap();
                std::fs::write(dots.join(generation).join("config.toml"), sized(size)).unwrap();
            }
            std::os::unix::fs::symlink("gen1", dots.join("current")).unwrap();
            let root = base.join("Tessera");
            std::fs::create_dir(&root).unwrap();
            std::os::unix::fs::symlink("../dots/current/config.toml", root.join("config.toml"))
                .unwrap();
            let running = run_native(&root, |native| assert!(native.armed()));
            assert!(applies(&running, 11, Duration::ZERO));

            retarget(&dots.join("current"), "gen2");
            assert!(
                applies(&running, 22, APPLIED_WITHIN),
                "the retargeted link was not applied"
            );
            assert_eq!(running.probe.polls(), 0, "armed throughout: no poll");
        }

        /// #417 item 1: `config.toml -> x/current -> y/a.toml`, with
        /// `x/current` retargeted to `y/b.toml`, is applied from an event
        /// on `x` (which holds the link the walk reaches last), not a poll.
        #[test]
        fn a_config_toml_linked_through_a_retargeted_file_link_is_applied() {
            let (_dir, base) = resolved_tempdir();
            let root = x_current_chain(&base);
            let running = run_native(&root, |native| {
                assert!(native.armed());
                assert!(watched_under(native, &base).contains(&base.join("x")));
            });
            assert!(applies(&running, 11, Duration::ZERO));

            retarget(&base.join("x").join("current"), "../y/b.toml");
            assert!(
                applies(&running, 22, APPLIED_WITHIN),
                "the retargeted file link was not applied"
            );
            assert_eq!(running.probe.polls(), 0, "armed throughout: no poll");
        }

        /// #417 M-A: while a directory link on the way is missing (the
        /// unlink half of macOS `ln -sfn`), the directory that held it is
        /// watched, so the symlink half raises an event there. Without it
        /// a rearm caught in between leaves the backend armed on a chain
        /// that misses every later retarget. Deterministic: the events are
        /// read off the queue here.
        #[test]
        fn the_directory_of_a_missing_link_is_watched_for_its_return() {
            let (_dir, base) = resolved_tempdir();
            let home = base.join("home");
            std::fs::create_dir_all(home.join("gen1").join("tessera")).unwrap();
            let root = home.join("cfg").join("tessera");
            let (mut native, _stop) = Native::open(&ConfigPaths::new(&root)).unwrap();
            assert!(
                watched_under(&native, &base).contains(&home),
                "{:?}",
                native.chain
            );
            std::os::unix::fs::symlink("gen1", home.join("cfg")).unwrap();
            let fd = chain_fd(&native, &home);
            let event = event_on(&mut native, fd).expect("no event for the link's return");
            assert!(!native.changes_nothing(&event), "{event:?}");
        }

        /// #417 M-A, the T4 probe (`adv retarget head native 0 ln`): a
        /// directory link retargeted by unlink and symlink in a tight loop
        /// (bounded storms), then once more: the last target applies, and
        /// so does an edit in it and a further retarget, from events.
        #[test]
        fn an_unlink_and_symlink_retarget_storm_never_leaves_the_watch_deaf() {
            let (_dir, base) = resolved_tempdir();
            let home = base.join("home");
            for generation in 0..8_u8 {
                let tree = home.join(format!("gen{generation}")).join("tessera");
                std::fs::create_dir_all(&tree).unwrap();
                std::fs::write(tree.join("config.toml"), sized(10 + generation)).unwrap();
            }
            let link = home.join("cfg");
            std::os::unix::fs::symlink("gen0", &link).unwrap();
            let running = run_native(&link.join("tessera"), |native| assert!(native.armed()));
            assert!(applies(&running, 10, Duration::ZERO));
            let switch = |generation: u8| {
                let _ = std::fs::remove_file(&link);
                std::os::unix::fs::symlink(format!("gen{generation}"), &link).unwrap();
            };
            for storm in 0..5_u8 {
                let started = Instant::now();
                let mut generation = 0_u8;
                while started.elapsed() < Duration::from_millis(150) {
                    generation = (generation + 1) % 6;
                    switch(generation);
                }
                // Let the watcher settle on the storm's end, then a quiet
                // retarget it must see from an event.
                let last = 6 + storm % 2;
                assert!(
                    applies(&running, 10 + generation, Duration::from_secs(5)),
                    "storm {storm}: the storm's last target was not applied"
                );
                switch(last);
                assert!(
                    applies(&running, 10 + last, Duration::from_secs(5)),
                    "storm {storm}: deaf after the storm"
                );
            }
            crate::write_atomic(&link.join("tessera").join("config.toml"), &sized(40)).unwrap();
            assert!(
                applies(&running, 40, Duration::from_secs(5)),
                "deaf to an edit"
            );
        }

        /// #393 F3: a real directory above the root swapped by rename (a
        /// new tree moved into place, the old one moved aside) is applied
        /// from an event on the directory holding it.
        #[test]
        fn a_directory_above_the_root_swapped_by_rename_is_applied() {
            let (_dir, base) = resolved_tempdir();
            for (tree, size) in [("outer", 11), ("outer.new", 22)] {
                let root = base.join(tree).join("Tessera");
                std::fs::create_dir_all(&root).unwrap();
                std::fs::write(root.join("config.toml"), sized(size)).unwrap();
            }
            let running = run_native(&base.join("outer").join("Tessera"), |native| {
                assert!(native.armed());
            });
            assert!(applies(&running, 11, Duration::ZERO));

            std::fs::rename(base.join("outer"), base.join("outer.old")).unwrap();
            std::fs::rename(base.join("outer.new"), base.join("outer")).unwrap();
            assert!(
                applies(&running, 22, APPLIED_WITHIN),
                "the swapped directory was not applied"
            );
        }

        /// #393 m2: an event on a directory of the chain that changes
        /// nothing on the way to `config.toml` (a file written next to the
        /// root, in `~`, say) is recognised as such; a retarget, a rename
        /// on the way or a change of `config.toml`'s own link is not.
        /// Deterministic: the events are read off the queue here.
        #[test]
        fn an_unrelated_change_in_a_chain_directory_changes_nothing() {
            let (_dir, base) = resolved_tempdir();
            let dots = base.join("dots");
            for generation in ["gen1", "gen2"] {
                std::fs::create_dir_all(dots.join(generation)).unwrap();
                std::fs::write(dots.join(generation).join("config.toml"), sized(11)).unwrap();
            }
            std::os::unix::fs::symlink("gen1", dots.join("current")).unwrap();
            let root = base.join("outer").join("Tessera");
            std::fs::create_dir_all(&root).unwrap();
            std::os::unix::fs::symlink("../../dots/current/config.toml", root.join("config.toml"))
                .unwrap();
            let paths = ConfigPaths::new(&root);
            let store = ConfigStore::open(paths.clone()).unwrap();
            let (mut native, _stop) = Native::open(&paths).unwrap();
            native.settle(&store);
            assert!(native.armed());
            let next = |native: &mut Native, dir: &Path| {
                let fd = chain_fd(native, dir);
                event_on(native, fd).unwrap_or_else(|| panic!("no event on {dir:?}"))
            };

            for dir in [&base, &dots, &base.join("outer")] {
                std::fs::write(dir.join("noise"), "x").unwrap();
                let event = next(&mut native, dir);
                assert!(native.changes_nothing(&event), "{dir:?}: {event:?}");
            }

            retarget(&dots.join("current"), "gen2");
            let event = next(&mut native, &dots);
            assert!(!native.changes_nothing(&event), "a retarget: {event:?}");
            native.rearm();

            std::fs::rename(base.join("outer"), base.join("outer.old")).unwrap();
            std::fs::create_dir_all(&root).unwrap();
            let event = next(&mut native, &base);
            assert!(!native.changes_nothing(&event), "a swap: {event:?}");
        }

        /// Gives `dirs` mode 0311 (search and write, no read) and back
        /// to 0755 when dropped, so the temporary directory can go.
        struct Locked(Vec<PathBuf>);

        impl Locked {
            /// Locks `dirs`; `None` when they stay readable (as root).
            fn lock(dirs: &[PathBuf]) -> Option<Self> {
                use std::os::unix::fs::PermissionsExt as _;
                let locked = Self(dirs.to_vec());
                for dir in dirs {
                    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o311)).unwrap();
                }
                dirs.iter()
                    .all(|dir| std::fs::read_dir(dir).is_err())
                    .then_some(locked)
            }
        }

        impl Drop for Locked {
            fn drop(&mut self) {
                use std::os::unix::fs::PermissionsExt as _;
                for dir in &self.0 {
                    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
                }
            }
        }

        /// #393 B1: a directory on the way that cannot be read (mode 0311)
        /// cannot be registered. One the walk only passes through is left
        /// unwatched: the backend stays armed and an event elsewhere on
        /// the way still changes nothing. One that holds a link on the way
        /// fails the registration, so the backend polls and a retarget of
        /// the link is applied, as by the poll backend.
        #[test]
        fn an_unreadable_directory_on_the_way_is_watched_as_far_as_it_can_be() {
            let (_dir, base) = resolved_tempdir();
            let plain_root = base.join("plain").join("Tessera");
            std::fs::create_dir_all(&plain_root).unwrap();
            std::fs::write(plain_root.join("config.toml"), sized(11)).unwrap();
            for backend in ["native", "poll"] {
                let held = base.join(backend).join("held");
                for (generation, size) in [("gen1", 11), ("gen2", 22)] {
                    let tree = base.join(backend).join(generation).join("tessera");
                    std::fs::create_dir_all(&tree).unwrap();
                    std::fs::write(tree.join("config.toml"), sized(size)).unwrap();
                }
                std::fs::create_dir(&held).unwrap();
                std::os::unix::fs::symlink("../gen1", held.join("cur")).unwrap();
            }
            let Some(_locked) = Locked::lock(&[
                base.join("plain"),
                base.join("native").join("held"),
                base.join("poll").join("held"),
            ]) else {
                return;
            };

            let paths = ConfigPaths::new(&plain_root);
            let store = ConfigStore::open(paths.clone()).unwrap();
            let (mut native, _stop) = Native::open(&paths).unwrap();
            native.settle(&store);
            assert!(
                native.armed(),
                "an unreadable directory passed through unarms"
            );
            std::fs::write(base.join("noise"), "x").unwrap();
            let fd = chain_fd(&native, &base);
            let event = event_on(&mut native, fd).expect("no event on the base");
            assert!(native.changes_nothing(&event), "{event:?}");

            let running = run_native(
                &base.join("native").join("held").join("cur").join("tessera"),
                |native| {
                    assert!(
                        !native.armed(),
                        "an unreadable link holder must not stay armed"
                    );
                },
            );
            let poll_store = ConfigStore::open(ConfigPaths::new(
                base.join("poll").join("held").join("cur").join("tessera"),
            ))
            .unwrap();
            let _poll = poll_store
                .watch(WatchOptions {
                    backend: crate::WatchBackend::Poll,
                    poll_interval: Duration::from_millis(50),
                    ..WatchOptions::default()
                })
                .unwrap();
            for backend in ["native", "poll"] {
                retarget(&base.join(backend).join("held").join("cur"), "../gen2");
            }
            assert!(
                applies(&running, 22, APPLIED_WITHIN),
                "native: the retarget in an unreadable directory was missed"
            );
            let deadline = Instant::now() + APPLIED_WITHIN;
            while (poll_store.config().appearance.font_size - 22.0).abs() >= f32::EPSILON {
                assert!(Instant::now() < deadline, "poll: the retarget was missed");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        /// #417 M-B: `themes/` unreadable at start (not registered, so the
        /// backend polls), then restored while a directory of the chain
        /// gets an event every 60 ms (faster than the poll could time out
        /// and rearm): a theme file created in it is still reported within
        /// the normal latency, since a chain event rearms while a
        /// registration is incomplete.
        #[test]
        fn an_unreadable_themes_directory_recovers_while_the_chain_is_busy() {
            use std::sync::atomic::{AtomicBool, Ordering};
            let (_dir, base) = resolved_tempdir();
            let root = base.join("Tessera");
            let themes = root.join("themes");
            std::fs::create_dir_all(&themes).unwrap();
            std::fs::write(root.join("config.toml"), sized(11)).unwrap();
            let Some(locked) = Locked::lock(std::slice::from_ref(&themes)) else {
                return;
            };
            let store = ConfigStore::open(ConfigPaths::new(&root)).unwrap();
            let watcher = store
                .watch(WatchOptions {
                    backend: crate::WatchBackend::Native,
                    ..WatchOptions::default()
                })
                .unwrap();
            let flooding = std::sync::Arc::new(AtomicBool::new(true));
            let flood = std::thread::spawn({
                let (flooding, base) = (std::sync::Arc::clone(&flooding), base.clone());
                move || {
                    while flooding.load(Ordering::Relaxed) {
                        std::fs::write(base.join("flood.0"), "x").unwrap();
                        std::fs::rename(base.join("flood.0"), base.join("flood.r")).unwrap();
                        std::thread::sleep(Duration::from_millis(60));
                    }
                }
            });
            std::thread::sleep(Duration::from_millis(400));
            drop(locked);
            std::thread::sleep(Duration::from_millis(100));
            while watcher.try_recv().is_ok() {}
            std::fs::write(themes.join("new.toml"), theme_named("New")).unwrap();
            let written = Instant::now();
            let mut reported = None;
            while let Some(left) = Duration::from_millis(500).checked_sub(written.elapsed()) {
                match watcher.recv_timeout(left) {
                    Ok(ConfigEvent::ThemesChanged) => {
                        reported = Some(written.elapsed());
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            flooding.store(false, Ordering::Relaxed);
            flood.join().unwrap();
            assert!(
                reported.is_some(),
                "no ThemesChanged within 500 ms while the chain was busy"
            );
        }

        /// #393 m2: a flood of unrelated writes in a chain directory
        /// re-registers nothing and reloads nothing; an edit of
        /// `config.toml` during it still applies.
        #[test]
        fn a_flood_in_a_chain_directory_neither_rearms_nor_reloads() {
            let (_dir, base) = resolved_tempdir();
            let root = base.join("Tessera");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("config.toml"), sized(11)).unwrap();
            let running = run_native(&root, |native| assert!(native.armed()));
            let settled = running.probe.settles();
            // Bursts further apart than the debounce, so each is a
            // debounce of its own: each would re-register and reload if a
            // change there counted.
            for _ in 0..10 {
                for i in 0..20 {
                    std::fs::write(base.join(format!("noise-{}", i % 7)), "x").unwrap();
                    if i % 7 == 6 {
                        for j in 0..7 {
                            std::fs::remove_file(base.join(format!("noise-{j}"))).unwrap();
                        }
                    }
                }
                std::thread::sleep(WatchOptions::default().debounce * 2);
            }
            // Something the thread must have read past the flood to see.
            crate::write_atomic(&root.join("config.toml"), &sized(22)).unwrap();
            assert!(applies(&running, 22, Duration::from_secs(5)));
            assert!(
                running.probe.settles() - settled <= 2,
                "settled {} times",
                running.probe.settles() - settled
            );
            assert!(
                running.probe.rearms() <= 4,
                "re-registered {} times",
                running.probe.rearms()
            );
        }

        /// #393 m2: changes in a chain directory that go on (a busy
        /// `$TMPDIR` or `~`) hold back neither an edit of `config.toml` nor
        /// a directory on the way swapped meanwhile, however long
        /// `max_wait` is: they never extend the debounce.
        #[test]
        fn a_flood_in_a_chain_directory_holds_nothing_back() {
            use std::sync::atomic::{AtomicBool, Ordering};
            let (_dir, base) = resolved_tempdir();
            for (tree, size) in [("outer", 11), ("outer.new", 33)] {
                let root = base.join(tree).join("Tessera");
                std::fs::create_dir_all(&root).unwrap();
                std::fs::write(root.join("config.toml"), sized(size)).unwrap();
            }
            let root = base.join("outer").join("Tessera");
            let running = run_native_waiting(&root, Duration::from_secs(60), |native| {
                assert!(native.armed());
            });
            let flooding = std::sync::Arc::new(AtomicBool::new(true));
            let flood = std::thread::spawn({
                let (flooding, noise) = (std::sync::Arc::clone(&flooding), base.join("noise"));
                move || {
                    while flooding.load(Ordering::Relaxed) {
                        std::fs::write(&noise, "x").unwrap();
                        std::fs::remove_file(&noise).unwrap();
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
            });
            std::thread::sleep(Duration::from_millis(300));
            crate::write_atomic(&root.join("config.toml"), &sized(22)).unwrap();
            let edited = applies(&running, 22, Duration::from_secs(10));
            std::fs::rename(base.join("outer"), base.join("outer.old")).unwrap();
            std::fs::rename(base.join("outer.new"), base.join("outer")).unwrap();
            let swapped = applies(&running, 33, Duration::from_secs(10));
            flooding.store(false, Ordering::Relaxed);
            flood.join().unwrap();
            assert!(edited, "the edit waited for the flood to end");
            assert!(swapped, "the swap waited for the flood to end");
        }

        /// #393 m3, m4: past the chain cap the backend polls every
        /// [`CAPPED_CHAIN_POLL_INTERVAL`] (not every `poll_interval`), and
        /// a poll that finds `config.toml` unchanged reloads nothing (the
        /// settle refreshes what the poll compares with). An edit still
        /// applies, from the root's own watch.
        #[test]
        fn past_the_chain_cap_the_backend_polls_slowly_and_reloads_nothing() {
            let (_dir, base) = resolved_tempdir();
            let root = link_chain(&base, MAX_DIR_LINKS_FOLLOWED + 1);
            std::fs::write(root.join("config.toml"), sized(11)).unwrap();
            let running = run_native(&root, |native| {
                assert!(native.chain_capped);
                assert!(!native.armed());
            });
            let settled = running.probe.settles();
            let watched = Duration::from_secs(5);
            std::thread::sleep(watched);
            let polls = running.probe.polls();
            let most =
                usize::try_from(watched.as_millis() / CAPPED_CHAIN_POLL_INTERVAL.as_millis() + 1)
                    .unwrap();
            assert!(polls >= 1, "it polls");
            assert!(
                polls <= most,
                "{polls} polls in {watched:?}, at most {most}"
            );
            assert_eq!(
                running.probe.settles(),
                settled,
                "a poll finding config.toml unchanged reloaded it"
            );

            crate::write_atomic(&root.join("config.toml"), &sized(22)).unwrap();
            assert!(applies(&running, 22, Duration::from_secs(5)));
        }

        /// #397: the profiles' theme files are watched, so one saved in
        /// place raises an event; at most [`MAX_PROFILE_THEMES_FOLLOWED`]
        /// however many profiles name themes (S5: 256 profiles here).
        #[test]
        fn profile_theme_files_are_watched_up_to_the_cap() {
            let base = tempfile::tempdir().unwrap();
            let paths = ConfigPaths::new(base.path());
            std::fs::create_dir_all(paths.themes_dir()).unwrap();
            let mut config = String::from("[appearance]\ntheme = \"main\"\n");
            std::fs::write(paths.themes_dir().join("main.toml"), theme_named("Main")).unwrap();
            for i in 0..crate::profile::MAX_PROFILES {
                std::fs::write(
                    paths.themes_dir().join(format!("t{i}.toml")),
                    theme_named(&format!("T{i}")),
                )
                .unwrap();
                writeln!(
                    config,
                    "[[profiles]]\nname = \"p{i}\"\n[profiles.appearance]\ntheme = \"t{i}\""
                )
                .unwrap();
            }
            std::fs::write(paths.config_file(), config).unwrap();
            let store = ConfigStore::open(paths.clone()).unwrap();
            assert_eq!(store.config().profiles.len(), crate::profile::MAX_PROFILES);
            let (mut native, _stop) = Native::open(&paths).unwrap();
            native.settle(&store);
            assert_eq!(native.profile_themes.len(), MAX_PROFILE_THEMES_FOLLOWED);
            assert_eq!(
                native.profile_theme_handles.len(),
                MAX_PROFILE_THEMES_FOLLOWED
            );

            let first = paths.themes_dir().join("t0.toml");
            std::fs::write(&first, theme_named("T0 saved")).unwrap();
            let fd = native.profile_theme_handles[0].as_raw_fd();
            assert!(
                event_on(&mut native, fd).is_some(),
                "no event for the save of a profile's theme ({fd})"
            );
        }

        /// #283 item 2: the theme files are looked up once on the thread
        /// that starts the watch (the catch-up settle's), not twice.
        #[test]
        fn starting_the_watch_looks_the_theme_up_once() {
            let base = tempfile::tempdir().unwrap();
            let paths = ConfigPaths::new(base.path());
            std::fs::create_dir_all(paths.themes_dir()).unwrap();
            std::fs::write(paths.themes_dir().join("mine.toml"), theme_named("Mine")).unwrap();
            std::fs::write(paths.config_file(), "[appearance]\ntheme = \"mine\"\n").unwrap();
            let store = ConfigStore::open(paths.clone()).unwrap();
            let before = crate::theme::lookups_on_this_thread();
            let watcher = store
                .watch(WatchOptions {
                    backend: crate::WatchBackend::Native,
                    ..WatchOptions::default()
                })
                .unwrap();
            assert_eq!(crate::theme::lookups_on_this_thread() - before, 1);
            drop(watcher);
        }

        /// `paths` with `count` theme files `f{i}.toml` (`appearance.theme`
        /// is `f0`) and one profile per name in `profile_themes`.
        fn profiled_paths(base: &Path, count: usize, profile_themes: &[&str]) -> ConfigPaths {
            let paths = ConfigPaths::new(base);
            std::fs::create_dir_all(paths.themes_dir()).unwrap();
            for i in 0..count {
                std::fs::write(
                    paths.themes_dir().join(format!("f{i:03}.toml")),
                    theme_named(&format!("F{i}")),
                )
                .unwrap();
            }
            let mut config = String::from("[appearance]\ntheme = \"f000\"\n");
            for (i, theme) in profile_themes.iter().enumerate() {
                writeln!(
                    config,
                    "[[profiles]]\nname = \"p{i}\"\n[profiles.appearance]\ntheme = \"{theme}\""
                )
                .unwrap();
            }
            std::fs::write(paths.config_file(), config).unwrap();
            paths
        }

        /// Eight profiles naming bundled themes, next to 256 theme files,
        /// read no theme file when the watch starts and take no slot
        /// (#417 item 4: it was a lookup per name, loading every file).
        #[test]
        fn profiles_naming_bundled_themes_read_no_theme_file() {
            let bundled: Vec<&str> =
                Theme::bundled_names()[1..=MAX_PROFILE_THEMES_FOLLOWED].to_vec();
            let base = tempfile::tempdir().unwrap();
            let paths = profiled_paths(base.path(), crate::theme::MAX_THEME_FILES, &bundled);
            let store = ConfigStore::open(paths.clone()).unwrap();
            assert_eq!(store.config().profiles.len(), bundled.len());
            let loads = crate::theme::loads_on_this_thread();
            assert_eq!(
                profile_theme_files(&paths.themes_dir(), &store.config()),
                Vec::<std::path::PathBuf>::new()
            );
            assert_eq!(crate::theme::loads_on_this_thread() - loads, 0);

            // Starting the watch costs what it costs without the profiles.
            let unprofiled = tempfile::tempdir().unwrap();
            let bare_paths = profiled_paths(unprofiled.path(), crate::theme::MAX_THEME_FILES, &[]);
            let bare_store = ConfigStore::open(bare_paths).unwrap();
            let start = |store: &ConfigStore| {
                let (lookups, loads) = (
                    crate::theme::lookups_on_this_thread(),
                    crate::theme::loads_on_this_thread(),
                );
                let watcher = store
                    .watch(WatchOptions {
                        backend: crate::WatchBackend::Native,
                        ..WatchOptions::default()
                    })
                    .unwrap();
                let cost = (
                    crate::theme::lookups_on_this_thread() - lookups,
                    crate::theme::loads_on_this_thread() - loads,
                );
                drop(watcher);
                cost
            };
            let (lookups, loads) = start(&store);
            assert_eq!(lookups, 1);
            assert_eq!((lookups, loads), start(&bare_store));
        }

        /// Bundled names do not use up the profile-theme slots: eight of
        /// them before eight user themes still follow all eight files.
        /// A name found only by its `name` key is followed too, and a name
        /// whose stem match does not load follows that file.
        #[test]
        fn bundled_profile_themes_take_no_slot() {
            let mut names: Vec<String> = Theme::bundled_names()[1..=MAX_PROFILE_THEMES_FOLLOWED]
                .iter()
                .map(|name| (*name).to_owned())
                .collect();
            names.extend((1..=6).map(|i| format!("f{i:03}")));
            names.push("Keyed".into());
            names.push("broken".into());
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let base = tempfile::tempdir().unwrap();
            let paths = profiled_paths(base.path(), 8, &refs);
            let themes = paths.themes_dir();
            std::fs::write(themes.join("other.toml"), theme_named("Keyed")).unwrap();
            std::fs::write(themes.join("broken.toml"), "foreground = ").unwrap();
            let store = ConfigStore::open(paths.clone()).unwrap();
            let mut want: Vec<std::path::PathBuf> = (1..=6)
                .map(|i| themes.join(format!("f{i:03}.toml")))
                .collect();
            want.push(themes.join("other.toml"));
            want.push(themes.join("broken.toml"));
            assert_eq!(profile_theme_files(&themes, &store.config()), want);
            let (mut native, _stop) = Native::open(&paths).unwrap();
            native.settle(&store);
            assert_eq!(native.profile_themes, want);
            assert_eq!(
                native.profile_theme_handles.len(),
                MAX_PROFILE_THEMES_FOLLOWED
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
