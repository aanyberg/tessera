//! The built-in plugin host (ADR 0047).
//!
//! First-party plugins are linked into the binary behind this crate's cargo
//! features (`plugin-hello`, on by default) and listed by [`builtin`]. The
//! configuration's `[plugins] enabled` decides which of them run; it is
//! applied when the [`App`](crate::App) is created and again on every
//! configuration applied, so a newly listed plugin is activated and a
//! removed one deactivated without a restart.
//!
//! # Trust
//!
//! A built-in plugin is first-party code, but the host treats what crosses
//! the [`Host`] boundary like untrusted input all the same (ADR 0047): it
//! grants exactly the capabilities a plugin's manifest declares, and only
//! those this version implements ([`GRANTABLE`]); a plugin that declares
//! any other is refused and never activated. Every value a plugin hands
//! the host has a named cap (security rule S5), its text is sanitised
//! like a title (S3), and the log names a plugin by its id and an error by
//! its kind, never by what the plugin wrote (S8).
//!
//! # Threads
//!
//! Every plugin call runs on one thread, `tessera-plugins`, started on the
//! first activation; it runs the thread start hook first (ADR 0018). The
//! session readers and the applying thread hand it events through a queue
//! of at most [`MAX_PLUGIN_EVENTS`] and never wait for a plugin: when the
//! queue is full the newest event is dropped and counted (S6). A session's
//! creation or close dropped that way is made good on the thread's next
//! turn: each plugin granted [`Capability::Sessions`] is told of the
//! sessions opened and closed since, so its count of the open sessions
//! matches the `App`'s once it has the events queued. A plugin
//! that panics, or whose `activate` returns an error, is put in
//! [`PluginState::Failed`] and gets no further calls until a configuration
//! that does not enable it is applied and one that does follows. Dropping
//! the `App` asks the thread to deactivate every active plugin and end; it
//! is not waited for.

use std::collections::{BTreeSet, VecDeque};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;

pub use tessera_plugin_api::{Capability, Plugin, StatusItem};
use tessera_plugin_api::{Error as PluginError, Event, Host, Manifest};

use crate::config_watch::Applied;
use crate::remote::shown_in_title;
use crate::session::{SessionEvent, SessionId, SessionObserver, lock, run_thread_start_hook};

/// Plugin ids read from `[plugins] enabled`; later ones are ignored with
/// one warning that gives their count.
pub const MAX_ENABLED_PLUGINS: usize = 64;
/// Longest plugin id, in bytes, read from `[plugins] enabled`; a longer
/// one is ignored with a warning that gives its length only.
pub const MAX_PLUGIN_ID_LEN: usize = 128;
/// Events queued for the plugin thread; when the queue is full the newest
/// event is dropped and counted (a session's created or closed event
/// takes the place of the oldest other event instead, while there is
/// one; a close whose creation is still queued takes that creation out;
/// any other is dropped, and the plugin thread's next turn tells the
/// plugins of the sessions opened and closed since).
pub const MAX_PLUGIN_EVENTS: usize = 1024;
/// Status items kept per plugin; later ones are dropped.
pub const MAX_STATUS_ITEMS: usize = 8;
/// Longest status item id, in bytes, after sanitising.
pub const MAX_STATUS_ITEM_ID_LEN: usize = 64;
/// Longest status item text, in bytes, after sanitising.
pub const MAX_STATUS_TEXT_LEN: usize = 256;
/// Longest status item tooltip, in bytes, after sanitising.
pub const MAX_STATUS_TOOLTIP_LEN: usize = 1024;
/// Keys a plugin may store.
pub const MAX_STORAGE_KEYS: usize = 64;
/// Longest storage key, in bytes.
pub const MAX_STORAGE_KEY_LEN: usize = 128;
/// Largest stored value, in bytes.
pub const MAX_STORAGE_VALUE_LEN: usize = 64 * 1024;
/// Bytes of keys and values one plugin may store in all.
pub const MAX_STORAGE_BYTES: usize = 1024 * 1024;
/// Longest manifest name or version shown, in bytes, after sanitising.
const MAX_MANIFEST_TEXT_LEN: usize = 128;

/// The capabilities this version grants. A plugin whose manifest declares
/// any other is refused.
pub const GRANTABLE: [Capability; 3] = [
    Capability::Sessions,
    Capability::StatusBar,
    Capability::Storage,
];

/// Makes a fresh instance of a built-in plugin. Called once when the host
/// is created (for the manifest) and once per activation.
pub type PluginFactory = Box<dyn Fn() -> Box<dyn Plugin> + Send + Sync>;

/// The plugins linked into this build, in a fixed order.
pub fn builtin() -> Vec<PluginFactory> {
    #[allow(unused_mut, reason = "empty without the plugin features")]
    let mut factories: Vec<PluginFactory> = Vec::new();
    #[cfg(feature = "plugin-hello")]
    factories.push(Box::new(|| {
        Box::new(tessera_plugin_hello::Hello::default()) as Box<dyn Plugin>
    }));
    factories
}

/// Where a plugin stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PluginState {
    /// Activated and receiving events.
    Active,
    /// Enabled, but its manifest declares a capability this version does
    /// not grant; never activated.
    Refused,
    /// Panicked, or its `activate` returned an error; gets no calls until
    /// the configuration enables it again.
    Failed,
    /// Not enabled (or being deactivated).
    Disabled,
}

impl PluginState {
    /// The state's name in the plugins JSON: `active`, `refused`, `failed`
    /// or `disabled`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Refused => "refused",
            Self::Failed => "failed",
            Self::Disabled => "disabled",
        }
    }
}

/// One linked plugin as [`App::plugins`](crate::App::plugins) reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PluginInfo {
    /// The manifest's id (at most [`MAX_PLUGIN_ID_LEN`] bytes, sanitised).
    pub id: String,
    /// The manifest's name, sanitised.
    pub name: String,
    /// The manifest's version, sanitised.
    pub version: String,
    /// The capabilities the manifest declares.
    pub capabilities: Vec<Capability>,
    /// Whether `[plugins] enabled` lists it.
    pub enabled: bool,
    /// Where it stands.
    pub state: PluginState,
    /// Why it is refused or failed, written by the host (never text the
    /// plugin wrote); `None` when active or disabled.
    pub message: Option<String>,
    /// Its status items, sanitised and capped; empty unless active.
    pub status_items: Vec<StatusItem>,
}

/// Counters of the event queue, for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct QueueStats {
    /// Events queued now.
    pub queued: usize,
    /// The most events ever queued at once.
    pub high_water: usize,
    /// Events dropped because the queue was full.
    pub dropped: u64,
}

/// Removes the characters a title drops (control, bidi-control and line
/// separator characters, S3) and cuts the rest at a character boundary
/// to at most `cap` bytes.
fn sanitise(text: &str, cap: usize) -> String {
    let mut out = String::new();
    for c in text.chars().filter(|&c| shown_in_title(c)) {
        if out.len() + c.len_utf8() > cap {
            break;
        }
        out.push(c);
    }
    out
}

/// `items` sanitised and capped (P6).
fn sanitise_items(items: Vec<StatusItem>) -> Vec<StatusItem> {
    items
        .into_iter()
        .take(MAX_STATUS_ITEMS)
        .map(|item| StatusItem {
            id: sanitise(&item.id, MAX_STATUS_ITEM_ID_LEN),
            text: sanitise(&item.text, MAX_STATUS_TEXT_LEN),
            tooltip: item
                .tooltip
                .map(|tip| sanitise(&tip, MAX_STATUS_TOOLTIP_LEN)),
        })
        .collect()
}

/// A plugin id for the log: sanitised, capped and escaped (S3, S8).
fn id_for_log(id: &str) -> String {
    sanitise(id, MAX_PLUGIN_ID_LEN).escape_debug().to_string()
}

/// An error's kind for the log and the report, never its text (S8).
fn error_kind(error: &PluginError) -> &'static str {
    match error {
        PluginError::NotGranted(_) => "capability not granted",
        PluginError::Plugin(_) => "plugin error",
    }
}

/// Whether `event` is only for plugins granted [`Capability::Sessions`].
fn needs_sessions(event: &Event) -> bool {
    !matches!(event, Event::ConfigChanged)
}

/// Whether `event` is a session's creation or end, which the host keeps
/// even when the queue is full (it evicts another event for it).
fn is_lifecycle(event: &Event) -> bool {
    matches!(
        event,
        Event::SessionCreated { .. } | Event::SessionClosed { .. }
    )
}

fn plugin_session(id: SessionId) -> tessera_plugin_api::SessionId {
    tessera_plugin_api::SessionId(id.get())
}

/// What the host knows about one linked plugin, shown by the report.
struct Entry {
    /// Sanitised manifest fields.
    id: String,
    name: String,
    version: String,
    capabilities: Vec<Capability>,
    /// The first declared capability outside [`GRANTABLE`], if any.
    unsupported: Option<Capability>,
    enabled: bool,
    state: PluginState,
    message: Option<String>,
    status_items: Vec<StatusItem>,
    /// The handle of the activation in progress or in effect; its `live`
    /// flag is set and cleared under the board lock.
    handle: Option<Arc<Handle>>,
}

/// The event queue and what the plugin thread is asked to do.
#[derive(Default)]
struct Inbox {
    events: VecDeque<Event>,
    /// Per linked plugin: whether the configuration enables it (and its
    /// manifest is acceptable).
    desired: Vec<bool>,
    /// Per linked plugin: bumped each time the configuration enables it
    /// again, so a disable and an enable that both land before the plugin
    /// thread's next turn still end the old activation and start a fresh
    /// one.
    epochs: Vec<u64>,
    /// Set when `desired` or `epochs` changed since the thread last read
    /// them.
    reconcile: bool,
    /// Set when the `App` is dropped.
    shutdown: bool,
    /// Whether the plugin thread was started.
    started: bool,
    /// Events dropped since the queue was last empty (one debug log per
    /// burst).
    burst_dropped: u64,
    /// Sessions' created and closed events dropped since the plugin
    /// thread last took the sessions open (saturating; S5): while it is
    /// not 0 the thread's next turn hands the plugins what they missed.
    lifecycle_dropped: u64,
    /// Hash of the last `[plugins] enabled` list applied, so its warnings
    /// are logged once per change, not once per apply.
    last_enabled: Option<u64>,
    /// The `App`'s open sessions, kept whether or not their events are
    /// queued: a plugin activated later is told of them first. As many as
    /// the `App` has sessions (not filled from untrusted input).
    live: BTreeSet<u64>,
}

impl Inbox {
    /// The sessions open as of the queue's head: [`Inbox::live`] with the
    /// created and closed events still queued undone, newest first, so a
    /// plugin told of these and then handed the queued events counts each
    /// session once.
    fn live_at_head(&self) -> BTreeSet<u64> {
        let mut live = self.live.clone();
        for event in self.events.iter().rev() {
            match event {
                Event::SessionCreated { session } => {
                    live.remove(&session.0);
                }
                Event::SessionClosed { session } => {
                    live.insert(session.0);
                }
                _ => {}
            }
        }
        live
    }
}

/// State shared by the host, the plugin thread and every [`Handle`].
struct Shared {
    factories: Vec<PluginFactory>,
    /// The report, one entry per linked plugin. Never held while `inbox`
    /// is taken.
    board: Mutex<Vec<Entry>>,
    inbox: Mutex<Inbox>,
    wake: Condvar,
    /// Whether any enabled plugin may be granted [`Capability::Sessions`]:
    /// session events are queued only then.
    wants_sessions: AtomicBool,
    high_water: AtomicUsize,
    dropped: AtomicU64,
}

impl Shared {
    fn board(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        lock(&self.board)
    }
}

/// The plugin host of one [`App`](crate::App).
pub(crate) struct PluginHost {
    shared: Arc<Shared>,
    applied: Arc<Mutex<Applied>>,
}

impl PluginHost {
    /// A host for the plugins `factories` make, reading the configuration
    /// in effect from `applied`. Each factory is called once here, for its
    /// plugin's manifest; one that panics leaves its plugin out, with a
    /// warning that gives its index.
    pub(crate) fn new(factories: Vec<PluginFactory>, applied: Arc<Mutex<Applied>>) -> Self {
        let mut kept = Vec::new();
        let mut board = Vec::new();
        for (index, factory) in factories.into_iter().enumerate() {
            let manifest = panic::catch_unwind(AssertUnwindSafe(|| factory().manifest()));
            let Ok(manifest) = manifest else {
                tracing::warn!(
                    index,
                    "a built-in plugin panicked before its manifest; left out"
                );
                continue;
            };
            board.push(Entry::new(&manifest));
            kept.push(factory);
        }
        let inbox = Inbox {
            desired: vec![false; kept.len()],
            epochs: vec![0; kept.len()],
            ..Inbox::default()
        };
        Self {
            shared: Arc::new(Shared {
                factories: kept,
                board: Mutex::new(board),
                inbox: Mutex::new(inbox),
                wake: Condvar::new(),
                wants_sessions: AtomicBool::new(false),
                high_water: AtomicUsize::new(0),
                dropped: AtomicU64::new(0),
            }),
            applied,
        }
    }

    /// Applies the configuration in effect: activates the plugins it
    /// newly enables and deactivates those it no longer does, then queues
    /// [`Event::ConfigChanged`] when `announce` is set.
    ///
    /// The configuration is read under the queue's lock, so of two applies
    /// racing on two threads the one that reads last, which sees the
    /// newer configuration, decides.
    pub(crate) fn apply(&self, announce: bool) {
        let mut inbox = lock(&self.shared.inbox);
        if inbox.shutdown {
            return;
        }
        let config = Arc::clone(&lock(&self.applied).config);
        let ids = &config.plugins.enabled;
        let mut hasher = DefaultHasher::new();
        ids.hash(&mut hasher);
        let hash = hasher.finish();
        let warn = inbox.last_enabled != Some(hash);
        inbox.last_enabled = Some(hash);
        let listed = self.listed(ids, warn);

        let mut desired = vec![false; listed.len()];
        let (mut sessions, mut any) = (false, false);
        {
            let mut board = self.shared.board();
            for (index, entry) in board.iter_mut().enumerate() {
                let was_enabled = entry.enabled;
                entry.enabled = listed[index];
                if let Some(capability) = entry.unsupported {
                    if entry.enabled {
                        if !was_enabled {
                            tracing::warn!(
                                plugin = %id_for_log(&entry.id),
                                capability = ?capability,
                                "plugin refused: it declares a capability this version does not grant"
                            );
                        }
                        entry.state = PluginState::Refused;
                        entry.message = Some(format!(
                            "refused: needs the {} capability, which this version does not grant",
                            capability_name(capability)
                        ));
                    } else {
                        entry.state = PluginState::Disabled;
                        entry.message = None;
                    }
                    continue;
                }
                desired[index] = entry.enabled;
                any |= entry.enabled;
                sessions |= entry.enabled && entry.capabilities.contains(&Capability::Sessions);
            }
        }
        self.shared
            .wants_sessions
            .store(sessions, Ordering::Release);
        if inbox.desired != desired {
            let inbox = &mut *inbox;
            for ((epoch, was), &now) in inbox.epochs.iter_mut().zip(&inbox.desired).zip(&desired) {
                if now && !was {
                    *epoch += 1;
                }
            }
            inbox.desired = desired;
            inbox.reconcile = true;
        }
        if any && !inbox.started {
            self.start(&mut inbox);
        }
        if announce && any {
            self.push_locked(&mut inbox, Event::ConfigChanged);
        }
        drop(inbox);
        self.shared.wake.notify_one();
    }

    /// Which linked plugins `ids` lists, logging (when `warn`) one escaped
    /// warning per id past a cap or unknown, and one for the ids past
    /// [`MAX_ENABLED_PLUGINS`] (S3, S5, S8).
    fn listed(&self, ids: &[String], warn: bool) -> Vec<bool> {
        let board = self.shared.board();
        let mut listed = vec![false; board.len()];
        for id in ids.iter().take(MAX_ENABLED_PLUGINS) {
            if id.len() > MAX_PLUGIN_ID_LEN {
                if warn {
                    tracing::warn!(
                        len = id.len(),
                        "a [plugins] enabled id is longer than {MAX_PLUGIN_ID_LEN} bytes; ignored"
                    );
                }
                continue;
            }
            match board.iter().position(|entry| entry.id == *id) {
                Some(index) => listed[index] = true,
                None if warn => tracing::warn!(
                    plugin = %id_for_log(id),
                    "[plugins] enabled names a plugin this build does not have; ignored"
                ),
                None => {}
            }
        }
        if warn && ids.len() > MAX_ENABLED_PLUGINS {
            tracing::warn!(
                ignored = ids.len() - MAX_ENABLED_PLUGINS,
                "[plugins] enabled lists more than {MAX_ENABLED_PLUGINS} ids; the rest are ignored"
            );
        }
        listed
    }

    /// Starts the plugin thread; on failure every plugin it would run is
    /// marked failed.
    fn start(&self, inbox: &mut Inbox) {
        let runner = Runner {
            shared: Arc::clone(&self.shared),
            slots: (0..self.shared.factories.len())
                .map(|_| Slot::default())
                .collect(),
        };
        match thread::Builder::new()
            .name("tessera-plugins".into())
            .spawn(move || runner.run())
        {
            Ok(_) => inbox.started = true,
            Err(error) => {
                tracing::warn!(error = ?error.kind(), "the plugin thread could not start");
                let mut board = self.shared.board();
                for (entry, desired) in board.iter_mut().zip(&inbox.desired) {
                    if *desired {
                        entry.state = PluginState::Failed;
                        entry.message = Some("failed: the plugin thread could not start".into());
                    }
                }
            }
        }
    }

    /// Queues `event` for the plugin thread (see [`MAX_PLUGIN_EVENTS`]).
    fn push(&self, event: Event) {
        let mut inbox = lock(&self.shared.inbox);
        self.push_locked(&mut inbox, event);
        drop(inbox);
        self.shared.wake.notify_one();
    }

    fn push_locked(&self, inbox: &mut Inbox, event: Event) {
        if inbox.shutdown || !inbox.started {
            return;
        }
        if inbox.events.len() >= MAX_PLUGIN_EVENTS {
            let evicted = is_lifecycle(&event)
                .then(|| inbox.events.iter().position(|e| !is_lifecycle(e)))
                .flatten();
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            inbox.burst_dropped += 1;
            if let Some(at) = evicted {
                inbox.events.remove(at);
            } else {
                // Only sessions' creations and closes are queued. A close
                // whose creation is still queued undoes it; any other is
                // made good on the thread's next turn.
                let created = match &event {
                    Event::SessionClosed { session } => inbox.events.iter().position(
                        |e| matches!(e, Event::SessionCreated { session: s } if s == session),
                    ),
                    _ => None,
                };
                match created {
                    Some(at) => {
                        inbox.events.remove(at);
                    }
                    None if is_lifecycle(&event) => {
                        inbox.lifecycle_dropped = inbox.lifecycle_dropped.saturating_add(1);
                    }
                    None => {}
                }
                return;
            }
        }
        inbox.events.push_back(event);
        self.shared
            .high_water
            .fetch_max(inbox.events.len(), Ordering::Relaxed);
    }

    /// A session was registered (before its reader starts).
    pub(crate) fn session_created(&self, id: SessionId) {
        let mut inbox = lock(&self.shared.inbox);
        inbox.live.insert(id.get());
        if self.shared.wants_sessions.load(Ordering::Acquire) {
            let session = plugin_session(id);
            self.push_locked(&mut inbox, Event::SessionCreated { session });
        }
        drop(inbox);
        self.shared.wake.notify_one();
    }

    /// A session was closed (after its last event was delivered). Its
    /// close is queued also while no plugin wants sessions if its creation
    /// is still queued, so a plugin enabled again before the thread takes
    /// that creation is told of both, not of the creation alone.
    pub(crate) fn session_closed(&self, id: SessionId) {
        let mut inbox = lock(&self.shared.inbox);
        inbox.live.remove(&id.get());
        let created_queued = || {
            inbox.events.iter().any(
                |event| matches!(event, Event::SessionCreated { session } if session.0 == id.get()),
            )
        };
        if self.shared.wants_sessions.load(Ordering::Acquire) || created_queued() {
            let session = plugin_session(id);
            self.push_locked(&mut inbox, Event::SessionClosed { session });
        }
        drop(inbox);
        self.shared.wake.notify_one();
    }

    /// Queues the plugin event `event` of session `id` stands for, if any.
    /// Never called for [`SessionEvent::Output`] ([`Tee`] hands it to the
    /// frontend alone, P11).
    #[inline(never)]
    fn session_event(&self, id: SessionId, event: &SessionEvent) {
        debug_assert!(
            !matches!(event, SessionEvent::Output),
            "output never reaches the plugin host (P11)"
        );
        if !self.shared.wants_sessions.load(Ordering::Acquire) {
            return;
        }
        let session = plugin_session(id);
        let event = match event {
            SessionEvent::Title(title) => Event::TitleChanged {
                session,
                title: title.clone(),
            },
            // Metadata only, never the command's text (ADR 0039).
            SessionEvent::CommandStarted => Event::CommandStarted {
                session,
                command: None,
                cwd: None,
            },
            SessionEvent::CommandFinished {
                exit_code,
                duration,
            } => Event::CommandFinished {
                session,
                exit_code: *exit_code,
                duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            },
            _ => return,
        };
        self.push(event);
    }

    /// The linked plugins as they stand.
    pub(crate) fn report(&self) -> Vec<PluginInfo> {
        self.shared
            .board()
            .iter()
            .map(|entry| PluginInfo {
                id: entry.id.clone(),
                name: entry.name.clone(),
                version: entry.version.clone(),
                capabilities: entry.capabilities.clone(),
                enabled: entry.enabled,
                state: entry.state,
                message: entry.message.clone(),
                status_items: entry.status_items.clone(),
            })
            .collect()
    }

    /// The queue's counters.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn stats(&self) -> QueueStats {
        QueueStats {
            queued: lock(&self.shared.inbox).events.len(),
            high_water: self.shared.high_water.load(Ordering::Relaxed),
            dropped: self.shared.dropped.load(Ordering::Relaxed),
        }
    }

    /// Asks the plugin thread to deactivate every plugin and end, dropping
    /// the events still queued. Does not wait.
    pub(crate) fn shut_down(&self) {
        let mut inbox = lock(&self.shared.inbox);
        inbox.shutdown = true;
        inbox.events.clear();
        drop(inbox);
        self.shared.wake.notify_one();
    }
}

fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::Sessions => "sessions",
        Capability::Output => "output",
        Capability::StatusBar => "status_bar",
        Capability::Panels => "panels",
        Capability::Commands => "commands",
        Capability::Storage => "storage",
        Capability::Fs => "fs",
        Capability::Http => "http",
        _ => "unknown",
    }
}

impl Entry {
    fn new(manifest: &Manifest) -> Self {
        let mut capabilities = Vec::new();
        for capability in &manifest.capabilities {
            if !capabilities.contains(capability) {
                capabilities.push(*capability);
            }
        }
        Self {
            id: sanitise(&manifest.id, MAX_PLUGIN_ID_LEN),
            name: sanitise(&manifest.name, MAX_MANIFEST_TEXT_LEN),
            version: sanitise(&manifest.version, MAX_MANIFEST_TEXT_LEN),
            unsupported: capabilities
                .iter()
                .copied()
                .find(|c| !GRANTABLE.contains(c)),
            capabilities,
            enabled: false,
            state: PluginState::Disabled,
            message: None,
            status_items: Vec::new(),
            handle: None,
        }
    }
}

/// The observer the `App` installs: the frontend's observer, with the
/// plugin host looking on.
pub(crate) struct Tee {
    pub(crate) inner: Option<Box<dyn SessionObserver>>,
    pub(crate) plugins: Arc<PluginHost>,
}

impl SessionObserver for Tee {
    fn on_session_event(&self, id: SessionId, event: SessionEvent) {
        // Output is per chunk: it goes to the frontend before, and
        // without, any plugin work (no lock, no allocation; P11).
        if matches!(event, SessionEvent::Output) {
            if let Some(inner) = &self.inner {
                inner.on_session_event(id, event);
            }
            return;
        }
        // Queued first: a frontend that closes the session from this call
        // queues its close after it.
        self.plugins.session_event(id, &event);
        if let Some(inner) = &self.inner {
            inner.on_session_event(id, event);
        }
    }

    fn on_appearance_changed(&self, opaque: bool) {
        if let Some(inner) = &self.inner {
            inner.on_appearance_changed(opaque);
        }
    }

    /// The plugins follow the configuration first, then the frontend is
    /// called: it reads the new `enabled` in the report here (and a
    /// frontend that panics cannot keep the plugins from following it).
    /// Only the report and the queue change on this thread; the plugins'
    /// activations and deactivations follow on the plugin thread, later and
    /// with no callback of their own.
    fn on_config_applied(&self, generation: u64) {
        self.plugins.apply(true);
        if let Some(inner) = &self.inner {
            inner.on_config_applied(generation);
        }
    }
}

/// A plugin's storage, in memory only (ADR 0047).
#[derive(Default)]
struct Storage {
    values: std::collections::HashMap<String, Vec<u8>>,
    /// Bytes of keys and values stored.
    bytes: usize,
}

/// What one activation of a plugin calls: its grant, its storage and its
/// status items. A handle outlives its activation only as a dead one: once
/// the plugin is deactivated or failed every call is refused.
struct Handle {
    index: usize,
    granted: Vec<Capability>,
    /// Set while the activation is in effect; written under the board
    /// lock.
    live: AtomicBool,
    shared: Arc<Shared>,
    storage: Arc<Mutex<Storage>>,
}

impl Handle {
    fn check(&self, capability: Capability) -> tessera_plugin_api::Result<()> {
        if !self.granted.contains(&capability) {
            return Err(PluginError::NotGranted(capability));
        }
        if !self.live.load(Ordering::Acquire) {
            return Err(PluginError::Plugin("plugin is not active".into()));
        }
        Ok(())
    }
}

fn storage_full() -> PluginError {
    PluginError::Plugin("storage full".into())
}

impl Host for Handle {
    fn granted(&self) -> &[Capability] {
        &self.granted
    }

    fn set_status_items(&self, items: Vec<StatusItem>) -> tessera_plugin_api::Result<()> {
        self.check(Capability::StatusBar)?;
        let items = sanitise_items(items);
        let mut board = self.shared.board();
        // Checked again under the lock that clears it.
        if !self.live.load(Ordering::Acquire) {
            return Err(PluginError::Plugin("plugin is not active".into()));
        }
        board[self.index].status_items = items;
        Ok(())
    }

    fn storage_set(&self, key: &str, value: &[u8]) -> tessera_plugin_api::Result<()> {
        self.check(Capability::Storage)?;
        if key.len() > MAX_STORAGE_KEY_LEN || value.len() > MAX_STORAGE_VALUE_LEN {
            return Err(storage_full());
        }
        let mut storage = lock(&self.storage);
        let old = storage.values.get(key).map(|v| key.len() + v.len());
        if old.is_none() && storage.values.len() >= MAX_STORAGE_KEYS {
            return Err(storage_full());
        }
        let bytes = storage.bytes - old.unwrap_or(0) + key.len() + value.len();
        if bytes > MAX_STORAGE_BYTES {
            return Err(storage_full());
        }
        storage.values.insert(key.to_owned(), value.to_vec());
        storage.bytes = bytes;
        Ok(())
    }

    fn storage_get(&self, key: &str) -> tessera_plugin_api::Result<Option<Vec<u8>>> {
        self.check(Capability::Storage)?;
        Ok(lock(&self.storage).values.get(key).cloned())
    }
}

/// One linked plugin on the plugin thread.
#[derive(Default)]
struct Slot {
    /// The instance of the activation in effect.
    plugin: Option<Box<dyn Plugin>>,
    /// Whether the activation in effect was granted
    /// [`Capability::Sessions`].
    sessions: bool,
    /// The sessions the activation in effect was told are open: created
    /// minus closed. As many as the `App` has sessions.
    open: BTreeSet<u64>,
    /// The [`Inbox::epochs`] entry its latest activation was made for.
    epoch: u64,
    /// Kept across activations for the `App`'s life.
    storage: Arc<Mutex<Storage>>,
}

/// The plugin thread's state.
struct Runner {
    shared: Arc<Shared>,
    slots: Vec<Slot>,
}

/// The configuration's plugins as the plugin thread takes them.
struct Desired {
    /// [`Inbox::desired`].
    wanted: Vec<bool>,
    /// [`Inbox::epochs`].
    epochs: Vec<u64>,
    /// [`Inbox::live_at_head`].
    live: BTreeSet<u64>,
}

/// What the plugin thread takes from the inbox in one turn.
struct Turn {
    /// The plugins to run and the sessions open as of the queue's head
    /// ([`Inbox::live_at_head`]), when the configuration changed.
    desired: Option<Desired>,
    /// The sessions open as of the queue's head, when a session's created
    /// or closed event was dropped ([`Inbox::lifecycle_dropped`]).
    resync: Option<BTreeSet<u64>>,
    event: Option<Event>,
    shutdown: bool,
}

impl Runner {
    fn run(mut self) {
        run_thread_start_hook();
        loop {
            let turn = self.next();
            if turn.shutdown {
                for index in 0..self.slots.len() {
                    self.deactivate(index);
                }
                return;
            }
            if let Some(desired) = turn.desired {
                self.reconcile(&desired);
            }
            if let Some(live) = turn.resync {
                self.resync(&live);
            }
            if let Some(event) = turn.event {
                self.fan_out(&event);
            }
        }
    }

    /// Waits for work and takes it.
    fn next(&self) -> Turn {
        let mut inbox = lock(&self.shared.inbox);
        while !inbox.shutdown && !inbox.reconcile && inbox.events.is_empty() {
            inbox = self
                .shared
                .wake
                .wait(inbox)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if inbox.shutdown {
            return Turn {
                desired: None,
                resync: None,
                event: None,
                shutdown: true,
            };
        }
        let resync = (inbox.lifecycle_dropped > 0).then(|| {
            inbox.lifecycle_dropped = 0;
            inbox.live_at_head()
        });
        let desired = std::mem::take(&mut inbox.reconcile).then(|| Desired {
            wanted: inbox.desired.clone(),
            epochs: inbox.epochs.clone(),
            live: resync.clone().unwrap_or_else(|| inbox.live_at_head()),
        });
        let event = inbox.events.pop_front();
        if inbox.events.is_empty() && inbox.burst_dropped > 0 {
            tracing::debug!(
                dropped = inbox.burst_dropped,
                "plugin event queue was full; events dropped"
            );
            inbox.burst_dropped = 0;
        }
        Turn {
            desired,
            resync,
            event,
            shutdown: false,
        }
    }

    /// Activates and deactivates plugins to match `desired`; one activated
    /// is told of the sessions open first. A plugin enabled again since its
    /// activation (its epoch moved on) is deactivated, or cleared of its
    /// failure, and activated afresh.
    fn reconcile(&mut self, desired: &Desired) {
        for (index, &wanted) in desired.wanted.iter().enumerate() {
            let epoch = desired.epochs[index];
            if wanted && self.slots[index].epoch != epoch {
                self.deactivate(index);
                self.slots[index].epoch = epoch;
                self.activate(index, &desired.live);
            } else if !wanted {
                self.deactivate(index);
                let mut board = self.shared.board();
                if board[index].state == PluginState::Failed {
                    board[index].state = PluginState::Disabled;
                    board[index].message = None;
                }
            }
        }
    }

    /// Tells every active plugin granted [`Capability::Sessions`] of the
    /// sessions closed and opened that it was not told of, so the sessions
    /// it counts open are `live`, the sessions open as of the queue's head.
    fn resync(&mut self, live: &BTreeSet<u64>) {
        for index in 0..self.slots.len() {
            let slot = &self.slots[index];
            if slot.plugin.is_none() || !slot.sessions {
                continue;
            }
            let closed: Vec<u64> = slot.open.difference(live).copied().collect();
            let opened: Vec<u64> = live.difference(&slot.open).copied().collect();
            let events = closed
                .into_iter()
                .map(|session| Event::SessionClosed {
                    session: tessera_plugin_api::SessionId(session),
                })
                .chain(opened.into_iter().map(|session| Event::SessionCreated {
                    session: tessera_plugin_api::SessionId(session),
                }));
            for event in events {
                if !self.deliver(index, &event) {
                    break;
                }
            }
        }
    }

    fn activate(&mut self, index: usize, live: &BTreeSet<u64>) {
        let made = panic::catch_unwind(AssertUnwindSafe(|| (self.shared.factories[index])()));
        let Ok(mut plugin) = made else {
            self.fail(index, "panicked");
            return;
        };
        let handle = {
            let mut board = self.shared.board();
            let entry = &mut board[index];
            let handle = Arc::new(Handle {
                index,
                granted: entry.capabilities.clone(),
                live: AtomicBool::new(true),
                shared: Arc::clone(&self.shared),
                storage: Arc::clone(&self.slots[index].storage),
            });
            entry.handle = Some(Arc::clone(&handle));
            handle
        };
        let sessions = handle.granted.contains(&Capability::Sessions);
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| plugin.activate(handle)));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                drop_quietly(plugin);
                self.fail(index, error_kind(&error));
                return;
            }
            Err(_) => {
                drop_quietly(plugin);
                self.fail(index, "panicked");
                return;
            }
        }
        self.slots[index].plugin = Some(plugin);
        self.slots[index].sessions = sessions;
        {
            let mut board = self.shared.board();
            board[index].state = PluginState::Active;
            board[index].message = None;
        }
        // Told of the sessions already open, as if each was just created.
        if sessions {
            for &session in live {
                let event = Event::SessionCreated {
                    session: tessera_plugin_api::SessionId(session),
                };
                if !self.deliver(index, &event) {
                    return;
                }
            }
        }
    }

    /// Calls `deactivate` on plugin `index` if it is active, and marks it
    /// disabled.
    fn deactivate(&mut self, index: usize) {
        let Some(mut plugin) = self.slots[index].plugin.take() else {
            return;
        };
        self.slots[index].open.clear();
        // Its handle still serves storage and status items here; retired
        // (and the items cleared) right after.
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| plugin.deactivate()));
        self.retire(index, PluginState::Disabled, None);
        let kind = match &outcome {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error_kind(error)),
            Err(_) => Some("panicked"),
        };
        if let Some(kind) = kind {
            let id = id_for_log(&self.shared.board()[index].id);
            tracing::warn!(plugin = %id, error = kind, "plugin deactivate failed");
        }
        drop_quietly(plugin);
    }

    /// Ends plugin `index`'s activation in the report: its handle refuses
    /// every call from now on and its status items are cleared.
    fn retire(&self, index: usize, state: PluginState, message: Option<String>) {
        let mut board = self.shared.board();
        let entry = &mut board[index];
        if let Some(handle) = entry.handle.take() {
            handle.live.store(false, Ordering::Release);
        }
        entry.status_items.clear();
        entry.state = state;
        entry.message = message;
    }

    /// Puts plugin `index` in [`PluginState::Failed`] with one warning
    /// (its id and `kind` only, S8); its instance, if any, is dropped.
    fn fail(&mut self, index: usize, kind: &'static str) {
        if let Some(plugin) = self.slots[index].plugin.take() {
            drop_quietly(plugin);
        }
        self.slots[index].open.clear();
        self.retire(index, PluginState::Failed, Some(format!("failed: {kind}")));
        let id = id_for_log(&self.shared.board()[index].id);
        tracing::warn!(plugin = %id, error = kind, "plugin failed; it gets no further calls");
    }

    /// Hands `event` to every active plugin allowed to see it.
    fn fan_out(&mut self, event: &Event) {
        for index in 0..self.slots.len() {
            if self.slots[index].plugin.is_some() {
                self.deliver(index, event);
            }
        }
    }

    /// Hands `event` to plugin `index` if its grant allows; returns
    /// whether the plugin is still active afterwards.
    fn deliver(&mut self, index: usize, event: &Event) -> bool {
        let slot = &mut self.slots[index];
        let Some(plugin) = slot.plugin.as_mut() else {
            return false;
        };
        if needs_sessions(event) && !slot.sessions {
            return true;
        }
        match event {
            Event::SessionCreated { session } => {
                slot.open.insert(session.0);
            }
            Event::SessionClosed { session } => {
                slot.open.remove(&session.0);
            }
            _ => {}
        }
        match panic::catch_unwind(AssertUnwindSafe(|| plugin.on_event(event))) {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                tracing::debug!(
                    error = error_kind(&error),
                    "plugin returned an error from an event"
                );
                true
            }
            Err(_) => {
                self.fail(index, "panicked");
                false
            }
        }
    }
}

/// Drops a plugin instance, catching a panic in its destructor.
fn drop_quietly(plugin: Box<dyn Plugin>) {
    if panic::catch_unwind(AssertUnwindSafe(move || drop(plugin))).is_err() {
        tracing::warn!("a plugin panicked while it was dropped");
    }
}
