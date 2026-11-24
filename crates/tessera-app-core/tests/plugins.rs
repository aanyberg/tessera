//! The built-in plugin host (issue #48, ADR 0047) through an `App` and real
//! sessions: Hello's status item follows the open sessions and
//! `[plugins] enabled`; a plugin gets exactly the capabilities its manifest
//! declares; a slow, failing or panicking plugin never holds up a session;
//! every buffer a plugin fills is capped; and the log never carries text a
//! plugin or a session wrote.
//!
//! Waits are bounded by attempts (each sleeps [`POLL`]), not by a clock, so
//! a loaded machine stretches them instead of failing them.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use tessera_app_core::plugins::{
    self, Capability, MAX_ENABLED_PLUGINS, MAX_PLUGIN_EVENTS, MAX_PLUGIN_ID_LEN,
    MAX_STATUS_ITEM_ID_LEN, MAX_STATUS_ITEMS, MAX_STATUS_TEXT_LEN, MAX_STATUS_TOOLTIP_LEN,
    MAX_STORAGE_BYTES, MAX_STORAGE_KEY_LEN, MAX_STORAGE_KEYS, MAX_STORAGE_VALUE_LEN, Plugin,
    PluginFactory, StatusItem,
};
use tessera_app_core::{
    App, Config, ConfigPaths, PluginInfo, PluginState, PtySize, SessionId, SpawnOptions,
    WatchOptions,
};
use tessera_plugin_api::{Error, Event, Host, Manifest, SessionId as PluginSession};

/// Pause between two polls.
const POLL: Duration = Duration::from_millis(10);
/// Polls before a wait gives up: 60 s on an idle machine, where every wait
/// here ends within the first few dozen.
const ATTEMPTS: usize = 6000;

/// Hello's manifest id.
const HELLO: &str = "dev.tessera.hello";

// --- a test plugin ---------------------------------------------------------

/// What [`Probe::activate`] does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OnActivate {
    Succeed,
    Fail,
    Panic,
}

/// How a [`Probe`] behaves.
#[derive(Clone)]
struct Spec {
    id: String,
    capabilities: Vec<Capability>,
    on_activate: OnActivate,
    panic_on_event: bool,
    /// Whether `on_event` returns an error (after recording the event).
    error_on_event: bool,
}

impl Spec {
    fn new(id: &str, capabilities: &[Capability]) -> Self {
        Self {
            id: id.to_owned(),
            capabilities: capabilities.to_vec(),
            on_activate: OnActivate::Succeed,
            panic_on_event: false,
            error_on_event: false,
        }
    }
}

/// What every instance of one [`Probe`] saw.
#[derive(Default)]
struct Seen {
    activations: usize,
    deactivations: usize,
    events: Vec<Event>,
    /// Whether a storage write from `deactivate` succeeded, once called.
    stored_on_deactivate: Option<bool>,
    /// The events of the latest activation only.
    current: Vec<Event>,
    host: Option<Arc<dyn Host>>,
}

/// A plugin that records its calls, shared by every instance its factory
/// makes, and sleeps `delay` milliseconds on each event.
#[derive(Clone)]
struct Probe {
    spec: Spec,
    seen: Arc<Mutex<Seen>>,
    delay: Arc<AtomicU64>,
    /// While set, each event waits (after it is recorded) until it clears.
    hold: Arc<AtomicBool>,
    /// [`Probe::hold`] applies to `ConfigChanged` only.
    hold_reloads_only: Arc<AtomicBool>,
    /// Held events let through one at a time ([`Probe::step`]).
    steps: Arc<AtomicUsize>,
    /// While set, the next event panics (and clears it).
    panic_next: Arc<AtomicBool>,
}

impl Probe {
    fn new(spec: Spec) -> Self {
        Self {
            spec,
            seen: Arc::default(),
            delay: Arc::default(),
            hold: Arc::default(),
            hold_reloads_only: Arc::default(),
            steps: Arc::default(),
            panic_next: Arc::default(),
        }
    }

    fn factory(&self) -> PluginFactory {
        let probe = self.clone();
        Box::new(move || Box::new(probe.clone()) as Box<dyn Plugin>)
    }

    fn events(&self) -> Vec<Event> {
        self.seen.lock().unwrap().events.clone()
    }

    fn activations(&self) -> usize {
        self.seen.lock().unwrap().activations
    }

    fn deactivations(&self) -> usize {
        self.seen.lock().unwrap().deactivations
    }

    /// The sessions its latest activation was told are open: created
    /// minus closed.
    fn open_sessions(&self) -> i64 {
        self.seen
            .lock()
            .unwrap()
            .current
            .iter()
            .map(|event| match event {
                Event::SessionCreated { .. } => 1,
                Event::SessionClosed { .. } => -1,
                _ => 0,
            })
            .sum()
    }

    /// The sessions its latest activation was told are open, by id.
    fn open_set(&self) -> std::collections::BTreeSet<u64> {
        let mut open = std::collections::BTreeSet::new();
        for event in &self.seen.lock().unwrap().current {
            match event {
                Event::SessionCreated { session } => assert!(open.insert(session.0), "{event:?}"),
                Event::SessionClosed { session } => assert!(open.remove(&session.0), "{event:?}"),
                _ => {}
            }
        }
        open
    }

    /// Lets one held event through.
    fn step(&self) {
        self.steps.fetch_add(1, Ordering::Relaxed);
    }

    /// How many `ConfigChanged` its latest activation was handed.
    fn reloads_seen(&self) -> usize {
        let seen = self.seen.lock().unwrap();
        seen.current
            .iter()
            .filter(|event| **event == Event::ConfigChanged)
            .count()
    }

    /// Whether its latest activation was handed `event`.
    fn current_holds(&self, event: &Event) -> bool {
        self.seen.lock().unwrap().current.contains(event)
    }

    /// Holds the plugin thread inside this probe's `on_event`: sets
    /// `hold` and has `app` announce a reload of `enabled` (the list in
    /// effect), then waits until the probe is handed that
    /// `ConfigChanged`. Only that event is held: an earlier one the plugin
    /// thread has recorded but not finished (a preempted thread can sit
    /// there while the test moves on) would otherwise park the thread
    /// short of the reload, and the wait would never end (#525).
    fn hold_thread(&self, app: &App, enabled: &[&str]) {
        self.hold_reloads_only.store(true, Ordering::Relaxed);
        self.hold.store(true, Ordering::Relaxed);
        let before = self.events().len();
        enable(app, enabled);
        wait("the held ConfigChanged", || {
            self.events().len() > before && self.events().last() == Some(&Event::ConfigChanged)
        });
    }

    /// The host of its latest activation.
    fn host(&self) -> Arc<dyn Host> {
        self.seen.lock().unwrap().host.clone().expect("activated")
    }
}

impl Plugin for Probe {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: self.spec.id.clone(),
            name: format!("Probe {}", self.spec.id),
            version: "1.0.0".into(),
            capabilities: self.spec.capabilities.clone(),
        }
    }

    fn activate(&mut self, host: Arc<dyn Host>) -> tessera_plugin_api::Result<()> {
        let mut seen = self.seen.lock().unwrap();
        seen.activations += 1;
        seen.current.clear();
        seen.host = Some(host);
        drop(seen);
        match self.spec.on_activate {
            OnActivate::Succeed => Ok(()),
            OnActivate::Fail => Err(Error::Plugin("secret-activate-error".into())),
            OnActivate::Panic => panic!("secret-activate-panic"),
        }
    }

    fn on_event(&mut self, event: &Event) -> tessera_plugin_api::Result<()> {
        let mut seen = self.seen.lock().unwrap();
        seen.events.push(event.clone());
        seen.current.push(event.clone());
        drop(seen);
        thread::sleep(Duration::from_millis(self.delay.load(Ordering::Relaxed)));
        let holdable =
            !self.hold_reloads_only.load(Ordering::Relaxed) || *event == Event::ConfigChanged;
        while holdable && self.hold.load(Ordering::Relaxed) {
            let left = self.steps.load(Ordering::Relaxed);
            if left > 0
                && self
                    .steps
                    .compare_exchange(left, left - 1, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!self.spec.panic_on_event, "secret-event-panic");
        assert!(
            !self.panic_next.swap(false, Ordering::Relaxed),
            "secret-event-panic"
        );
        if self.spec.error_on_event {
            return Err(Error::Plugin("secret-event-error".into()));
        }
        Ok(())
    }

    fn deactivate(&mut self) -> tessera_plugin_api::Result<()> {
        let mut seen = self.seen.lock().unwrap();
        seen.deactivations += 1;
        let host = seen.host.clone().expect("activated");
        seen.stored_on_deactivate = Some(host.storage_set("bye", b"1").is_ok());
        Ok(())
    }
}

// --- log capture -----------------------------------------------------------

/// One logged event: its level, the thread that logged it and its fields
/// as `name=value`.
#[derive(Clone, Debug)]
struct Line {
    level: tracing::Level,
    thread: String,
    text: String,
}

/// Every event logged in this process, at every level. The plugin thread
/// logs too, so the subscriber is global; tests tell their lines apart by
/// thread or by plugin id.
fn log() -> &'static Mutex<Vec<Line>> {
    static LOG: OnceLock<Mutex<Vec<Line>>> = OnceLock::new();
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        tracing::subscriber::set_global_default(Capture).expect("the only global subscriber");
    });
    LOG.get_or_init(Mutex::default)
}

struct Capture;

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        write!(self.0, "{}={value:?} ", field.name()).unwrap();
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
        let mut text = String::new();
        event.record(&mut Fields(&mut text));
        let line = Line {
            level: *event.metadata().level(),
            thread: thread::current().name().unwrap_or_default().to_owned(),
            text,
        };
        log().lock().unwrap().push(line);
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// The warnings logged so far on this thread.
fn warnings_here() -> Vec<String> {
    let me = thread::current().name().unwrap_or_default().to_owned();
    log()
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.level == tracing::Level::WARN && line.thread == me)
        .map(|line| line.text.clone())
        .collect()
}

/// The warnings logged so far, on any thread, that name `id`.
fn warnings_naming(id: &str) -> Vec<String> {
    log()
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.level == tracing::Level::WARN && line.text.contains(id))
        .map(|line| line.text.clone())
        .collect()
}

// --- helpers ---------------------------------------------------------------

fn config(enabled: &[&str]) -> Config {
    let mut config = Config::default();
    config.plugins.enabled = enabled.iter().map(|&id| id.to_owned()).collect();
    config
}

fn app(root: &tempfile::TempDir, enabled: &[&str], factories: Vec<PluginFactory>) -> App {
    // Installs the subscriber before the host can log.
    log();
    App::with_plugins(
        config(enabled),
        ConfigPaths::new(root.path().join("config")),
        factories,
    )
}

/// Enables exactly `enabled` from now on, as a reload would.
fn enable(app: &App, enabled: &[&str]) {
    app.apply_config(&config(enabled), &tessera_config::Theme::default());
}

/// A session running `script` under `/bin/sh -c`.
fn spawn(app: &App, script: &str) -> SessionId {
    let options = SpawnOptions::command("/bin/sh", "")
        .arg("-c")
        .arg(script)
        .size(PtySize::new(24, 80));
    app.spawn_session(options).expect("spawn /bin/sh -c")
}

/// A session that only waits to be closed.
fn idle_session(app: &App) -> SessionId {
    spawn(app, "sleep 600")
}

fn info(app: &App, id: &str) -> PluginInfo {
    app.plugins()
        .into_iter()
        .find(|info| info.id == id)
        .unwrap_or_else(|| panic!("no plugin {id}"))
}

/// Polls until `done` holds, panicking with `what` once the attempts run
/// out.
fn wait(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..ATTEMPTS {
        if done() {
            return;
        }
        thread::sleep(POLL);
    }
    panic!("timed out waiting for {what}");
}

fn wait_for_state(app: &App, id: &str, state: PluginState) {
    wait(&format!("{id} {state:?}"), || info(app, id).state == state);
}

/// The text of `id`'s status items.
fn status_texts(app: &App, id: &str) -> Vec<String> {
    info(app, id)
        .status_items
        .into_iter()
        .map(|item| item.text)
        .collect()
}

fn wait_for_status(app: &App, id: &str, text: &str) {
    wait(&format!("{id} status {text:?}"), || {
        status_texts(app, id) == [text]
    });
}

/// Whether any visible row of session `id` holds `needle`.
fn screen_holds(app: &App, id: SessionId, needle: &str) -> bool {
    app.with_terminal(id, |term| {
        term.grid()
            .visible_lines()
            .iter()
            .any(|line| line.text().contains(needle))
    })
    .unwrap_or(false)
}

fn plugin_session(id: SessionId) -> PluginSession {
    PluginSession(id.get())
}

// --- Done when 1: Hello through the App ------------------------------------

/// Hello counts the open sessions; removing it from `[plugins] enabled`
/// deactivates it and leaves it disabled; listing it again activates a
/// fresh instance that counts the sessions still open.
#[cfg(feature = "plugin-hello")]
#[test]
fn hello_counts_sessions_and_follows_enablement() {
    let root = tempfile::tempdir().unwrap();
    let watcher = Probe::new(Spec::new("test.hello.watcher", &[Capability::Sessions]));
    let mut factories = plugins::builtin();
    factories.push(watcher.factory());
    let app = app(&root, &[HELLO, "test.hello.watcher"], factories);

    let hello = info(&app, HELLO);
    assert_eq!(hello.name, "Hello");
    assert!(hello.enabled);
    assert_eq!(
        hello.capabilities,
        [Capability::Sessions, Capability::StatusBar]
    );

    let first = idle_session(&app);
    let second = idle_session(&app);
    wait_for_status(&app, HELLO, "2 sessions");
    assert_eq!(info(&app, HELLO).state, PluginState::Active);

    app.close_session(first).unwrap();
    wait_for_status(&app, HELLO, "1 sessions");

    enable(&app, &["test.hello.watcher"]);
    wait_for_state(&app, HELLO, PluginState::Disabled);
    let hello = info(&app, HELLO);
    assert!(!hello.enabled);
    assert!(hello.status_items.is_empty(), "{hello:?}");
    assert_eq!(hello.message, None);

    // The watcher stays active and sees the reload; removing it deactivates it.
    wait("the watcher's ConfigChanged", || {
        watcher.events().contains(&Event::ConfigChanged)
    });
    enable(&app, &[]);
    wait_for_state(&app, "test.hello.watcher", PluginState::Disabled);
    assert_eq!(watcher.deactivations(), 1);

    enable(&app, &[HELLO]);
    wait_for_status(&app, HELLO, "1 sessions");
    assert_eq!(info(&app, HELLO).state, PluginState::Active);

    app.close_session(second).unwrap();
    wait_for_status(&app, HELLO, "0 sessions");
}

/// Hello is in the default build; the App starts with no plugin enabled.
#[cfg(feature = "plugin-hello")]
#[test]
fn hello_is_built_in_and_off_by_default() {
    let root = tempfile::tempdir().unwrap();
    let app = App::new(
        Config::default(),
        ConfigPaths::new(root.path().join("config")),
    );
    let all = app.plugins();
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].id, HELLO);
    assert_eq!(all[0].state, PluginState::Disabled);
    assert!(!all[0].enabled);
}

// --- Done when 2: capabilities -----------------------------------------------

/// A session that sets its title, runs one command that exits 3 and then
/// waits to be closed.
const MARKED: &str = "printf '\\033]0;secret-title\\007'; \
    printf '\\033]133;A\\007$ \\033]133;B\\007\\033]133;C\\007\\r\\n'; sleep 0.05; \
    printf '\\033]133;D;3\\007marks-done\\r\\n'; sleep 600";

/// Each plugin gets the capabilities its manifest declares and no more:
/// session, title and command events only with `Sessions`, `ConfigChanged`
/// for all, and every host call it was not granted refused.
#[test]
fn a_plugin_gets_exactly_what_its_manifest_declares() {
    let root = tempfile::tempdir().unwrap();
    let sessions = Probe::new(Spec::new("test.caps.sessions", &[Capability::Sessions]));
    let status = Probe::new(Spec::new("test.caps.status", &[Capability::StatusBar]));
    let storage = Probe::new(Spec::new("test.caps.storage", &[Capability::Storage]));
    let app = app(
        &root,
        &[
            "test.caps.sessions",
            "test.caps.status",
            "test.caps.storage",
        ],
        vec![sessions.factory(), status.factory(), storage.factory()],
    );
    for id in [
        "test.caps.sessions",
        "test.caps.status",
        "test.caps.storage",
    ] {
        wait_for_state(&app, id, PluginState::Active);
    }

    let id = spawn(&app, MARKED);
    let session = plugin_session(id);
    wait("the command's end", || {
        sessions
            .events()
            .iter()
            .any(|event| matches!(event, Event::CommandFinished { .. }))
    });
    enable(
        &app,
        &[
            "test.caps.sessions",
            "test.caps.status",
            "test.caps.storage",
        ],
    );
    app.close_session(id).unwrap();
    wait("the session's close", || {
        sessions
            .events()
            .contains(&Event::SessionClosed { session })
    });
    wait("ConfigChanged for all", || {
        [&sessions, &status, &storage]
            .iter()
            .all(|probe| probe.events().contains(&Event::ConfigChanged))
    });

    let events = sessions.events();
    assert_eq!(events[0], Event::SessionCreated { session }, "{events:?}");
    assert!(
        events.contains(&Event::TitleChanged {
            session,
            title: "secret-title".into()
        }),
        "{events:?}"
    );
    assert!(
        events.contains(&Event::CommandStarted {
            session,
            command: None,
            cwd: None
        }),
        "{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::CommandFinished { session: s, exit_code: Some(3), .. } if *s == session
        )),
        "{events:?}"
    );
    // Without `Sessions`, `ConfigChanged` is all there is.
    assert_eq!(status.events(), [Event::ConfigChanged]);
    assert_eq!(storage.events(), [Event::ConfigChanged]);
    assert_host_calls(&app, &sessions, &status, &storage);
}

/// Each probe's host refuses the calls its manifest did not ask for and
/// serves the ones it did.
fn assert_host_calls(app: &App, sessions: &Probe, status: &Probe, storage: &Probe) {
    let item = || StatusItem {
        id: "i".into(),
        text: "t".into(),
        tooltip: None,
    };
    let host = sessions.host();
    assert_eq!(host.granted(), [Capability::Sessions]);
    assert!(matches!(
        host.set_status_items(vec![item()]),
        Err(Error::NotGranted(Capability::StatusBar))
    ));
    assert!(matches!(
        host.storage_set("k", b"v"),
        Err(Error::NotGranted(Capability::Storage))
    ));
    assert!(matches!(
        host.storage_get("k"),
        Err(Error::NotGranted(Capability::Storage))
    ));

    let host = status.host();
    assert_eq!(host.granted(), [Capability::StatusBar]);
    host.set_status_items(vec![item()]).unwrap();
    assert_eq!(info(app, "test.caps.status").status_items, [item()]);
    assert!(matches!(
        host.storage_get("k"),
        Err(Error::NotGranted(Capability::Storage))
    ));

    let host = storage.host();
    assert_eq!(host.granted(), [Capability::Storage]);
    host.storage_set("k", b"v").unwrap();
    assert_eq!(host.storage_get("k").unwrap().as_deref(), Some(&b"v"[..]));
    assert_eq!(host.storage_get("missing").unwrap(), None);
    assert!(matches!(
        host.set_status_items(vec![item()]),
        Err(Error::NotGranted(Capability::StatusBar))
    ));
}

/// A manifest that declares a capability this version does not grant is
/// refused: never activated, with one warning naming the capability and a
/// message saying so; the plugins beside it run.
#[test]
fn a_plugin_declaring_an_ungranted_capability_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let output = Probe::new(Spec::new(
        "test.refused.output",
        &[Capability::Sessions, Capability::Output],
    ));
    let fine = Probe::new(Spec::new("test.refused.fine", &[Capability::Sessions]));
    let app = app(
        &root,
        &["test.refused.output", "test.refused.fine"],
        vec![output.factory(), fine.factory()],
    );
    let id = idle_session(&app);
    wait("the fine plugin's SessionCreated", || {
        !fine.events().is_empty()
    });
    // A reload that keeps the list warns no more.
    enable(&app, &["test.refused.output", "test.refused.fine"]);
    app.close_session(id).unwrap();
    wait("the fine plugin's SessionClosed", || {
        fine.events().len() >= 3
    });

    let refused = info(&app, "test.refused.output");
    assert_eq!(refused.state, PluginState::Refused);
    assert!(refused.enabled);
    assert!(
        refused
            .message
            .as_deref()
            .is_some_and(|m| m.contains("output")),
        "{refused:?}"
    );
    assert_eq!(output.activations(), 0);
    assert_eq!(output.events(), Vec::<Event>::new());
    let warned = warnings_naming("test.refused.output");
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].contains("Output"), "{warned:?}");
    assert_eq!(info(&app, "test.refused.fine").state, PluginState::Active);

    // Disabled, it is no longer refused; enabled again, it is.
    enable(&app, &[]);
    assert_eq!(
        info(&app, "test.refused.output").state,
        PluginState::Disabled
    );
    enable(&app, &["test.refused.output"]);
    assert_eq!(
        info(&app, "test.refused.output").state,
        PluginState::Refused
    );
    assert_eq!(warnings_naming("test.refused.output").len(), 2);
}

// --- Done when 3: failures -------------------------------------------------

/// An `activate` that errors, one that panics and an event handler that
/// panics each leave their plugin `failed` with one warning (id and error
/// kind), and the failed plugin gets no further calls until it is disabled
/// and enabled again.
#[test]
fn erroring_and_panicking_plugins_fail_alone() {
    let root = tempfile::tempdir().unwrap();
    let mut errs = Spec::new("test.fail.error", &[Capability::Sessions]);
    errs.on_activate = OnActivate::Fail;
    let errs = Probe::new(errs);
    let mut panics = Spec::new("test.fail.panic", &[Capability::Sessions]);
    panics.on_activate = OnActivate::Panic;
    let panics = Probe::new(panics);
    let mut event_panics = Spec::new("test.fail.event", &[Capability::Sessions]);
    event_panics.panic_on_event = true;
    let event_panics = Probe::new(event_panics);
    let fine = Probe::new(Spec::new("test.fail.fine", &[Capability::Sessions]));
    let all = [
        "test.fail.error",
        "test.fail.panic",
        "test.fail.event",
        "test.fail.fine",
    ];
    let app = app(
        &root,
        &all,
        vec![
            errs.factory(),
            panics.factory(),
            event_panics.factory(),
            fine.factory(),
        ],
    );
    wait_for_state(&app, "test.fail.error", PluginState::Failed);
    wait_for_state(&app, "test.fail.panic", PluginState::Failed);
    wait_for_state(&app, "test.fail.event", PluginState::Active);

    let first = idle_session(&app);
    let second = idle_session(&app);
    wait_for_state(&app, "test.fail.event", PluginState::Failed);
    wait("the fine plugin's events", || fine.events().len() == 2);
    app.close_session(first).unwrap();
    app.close_session(second).unwrap();
    wait("the fine plugin's closes", || fine.events().len() == 4);

    assert_eq!(
        info(&app, "test.fail.error").message.as_deref(),
        Some("failed: plugin error")
    );
    assert_eq!(
        info(&app, "test.fail.panic").message.as_deref(),
        Some("failed: panicked")
    );
    assert_eq!(
        info(&app, "test.fail.event").message.as_deref(),
        Some("failed: panicked")
    );
    // The event that panicked was its last call.
    assert_eq!(event_panics.events().len(), 1);
    assert_eq!(errs.events(), Vec::<Event>::new());
    assert_eq!(panics.events(), Vec::<Event>::new());
    for id in ["test.fail.error", "test.fail.panic", "test.fail.event"] {
        let warned = warnings_naming(id);
        assert_eq!(warned.len(), 1, "{id}: {warned:?}");
    }
    assert_eq!(info(&app, "test.fail.fine").state, PluginState::Active);

    // A failed plugin stays failed through reloads, also those that
    // change which other plugins run...
    enable(&app, &all);
    wait("ConfigChanged", || {
        fine.events().contains(&Event::ConfigChanged)
    });
    enable(&app, &all[..3]);
    wait_for_state(&app, "test.fail.fine", PluginState::Disabled);
    enable(&app, &all);
    wait_for_state(&app, "test.fail.fine", PluginState::Active);
    assert_eq!(errs.activations(), 1);
    assert_eq!(panics.activations(), 1);
    assert_eq!(event_panics.activations(), 1);
    // ...and is tried again once disabled and enabled.
    enable(&app, &[]);
    wait_for_state(&app, "test.fail.error", PluginState::Disabled);
    assert_eq!(info(&app, "test.fail.error").message, None);
    enable(&app, &["test.fail.error"]);
    wait("a second activation", || errs.activations() == 2);
    wait_for_state(&app, "test.fail.error", PluginState::Failed);
}

/// Nothing a plugin or a session wrote reaches the log, at any level: not a
/// plugin's error text, not its panic message, not a title, not a stored
/// value.
/// S8: plugins that fail to activate, panic in an event, store values and
/// set status items, next to a session that sets a title, log none of
/// that text (every value here is marked `secret`): the log names a plugin
/// by its id and an error by its kind, and the panicking plugin's failure
/// is one warning.
#[test]
fn the_log_carries_no_text_a_plugin_or_session_wrote() {
    let root = tempfile::tempdir().unwrap();
    let mut errs = Spec::new("test.quiet.error", &[Capability::Sessions]);
    errs.on_activate = OnActivate::Fail;
    let errs = Probe::new(errs);
    let mut panics = Spec::new("test.quiet.event", &[Capability::Sessions]);
    panics.panic_on_event = true;
    let panics = Probe::new(panics);
    let store = Probe::new(Spec::new(
        "test.quiet.store",
        &[
            Capability::Sessions,
            Capability::Storage,
            Capability::StatusBar,
        ],
    ));
    let app = app(
        &root,
        &["test.quiet.error", "test.quiet.event", "test.quiet.store"],
        vec![errs.factory(), panics.factory(), store.factory()],
    );
    wait_for_state(&app, "test.quiet.store", PluginState::Active);
    let host = store.host();
    host.storage_set("secret-key", b"secret-value").unwrap();
    // Refused past the cap: the refusal is not logged with the value.
    let _ = host.storage_set("k", &vec![b's'; MAX_STORAGE_VALUE_LEN + 1]);
    host.set_status_items(vec![StatusItem {
        id: "secret-item".into(),
        text: "secret-text".into(),
        tooltip: Some("secret-tooltip".into()),
    }])
    .unwrap();
    let id = spawn(&app, MARKED);
    wait("the title", || {
        store
            .events()
            .iter()
            .any(|event| matches!(event, Event::TitleChanged { .. }))
    });
    wait_for_state(&app, "test.quiet.event", PluginState::Failed);
    wait_for_state(&app, "test.quiet.error", PluginState::Failed);
    app.close_session(id).unwrap();
    drop(app);

    let leaked: Vec<Line> = log()
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.text.contains("secret"))
        .cloned()
        .collect();
    assert!(leaked.is_empty(), "{leaked:?}");
    assert_eq!(warnings_naming("test.quiet.event").len(), 1);
}

// --- Done when 3: the event queue under a flood ------------------------------

/// A file of `count` lines, each setting the title and then 1 KiB of text:
/// about one title per PTY read.
fn titled_flood(path: &std::path::Path, count: usize) {
    let mut text = String::with_capacity(count * 1100);
    let filler = "x".repeat(1000);
    for i in 0..count {
        write!(text, "\x1b]0;t{i}\x07{filler}\r\n").unwrap();
    }
    std::fs::write(path, text).unwrap();
}

/// A plugin stuck in its first event under a flood of titles: the queue
/// fills to its cap and holds there, the overflow is counted, the session
/// reads on, and a session's close still goes in (evicting a title) and
/// arrives once the plugin moves again.
#[test]
fn a_stuck_plugin_under_a_title_flood_drops_events_not_output() {
    let root = tempfile::tempdir().unwrap();
    let flood = root.path().join("flood");
    titled_flood(&flood, 6000);
    let stuck = Probe::new(Spec::new("test.flood.stuck", &[Capability::Sessions]));
    stuck.hold.store(true, Ordering::Relaxed);
    let app = app(&root, &["test.flood.stuck"], vec![stuck.factory()]);
    wait_for_state(&app, "test.flood.stuck", PluginState::Active);

    let id = spawn(
        &app,
        &format!("cat '{}'; echo flood-done; sleep 600", flood.display()),
    );
    wait("the end of the flood", || {
        screen_holds(&app, id, "flood-done")
    });
    let stats = app.plugin_queue_stats();
    // Full: the plugin has taken only the session's creation.
    assert_eq!(stuck.events().len(), 1);
    assert_eq!(stats.queued, MAX_PLUGIN_EVENTS, "{stats:?}");
    assert_eq!(stats.high_water, MAX_PLUGIN_EVENTS, "{stats:?}");
    assert!(stats.dropped > 0, "{stats:?}");

    app.close_session(id).unwrap();
    let after = app.plugin_queue_stats();
    assert_eq!(after.queued, MAX_PLUGIN_EVENTS, "{after:?}");
    assert_eq!(after.dropped, stats.dropped + 1, "{after:?}");
    stuck.hold.store(false, Ordering::Relaxed);
    let session = plugin_session(id);
    wait("the close after the flood", || {
        stuck.events().last() == Some(&Event::SessionClosed { session })
    });
    let events = stuck.events();
    assert_eq!(events[0], Event::SessionCreated { session });
    assert_eq!(events.len(), 1 + MAX_PLUGIN_EVENTS, "{}", events.len());
    assert_eq!(app.plugin_queue_stats().queued, 0);
}

// --- enablement changes the plugin thread has not seen yet --------------

/// Disabled and enabled again while the plugin thread is busy, with a
/// session opened in between: the old instance is deactivated and a fresh
/// one is told of both open sessions (B1 of the #48 review).
#[test]
fn a_coalesced_disable_and_enable_reactivates_and_replays_an_opened_session() {
    let root = tempfile::tempdir().unwrap();
    let probe = Probe::new(Spec::new("test.epoch.open", &[Capability::Sessions]));
    let app = app(&root, &["test.epoch.open"], vec![probe.factory()]);
    wait_for_state(&app, "test.epoch.open", PluginState::Active);
    let _first = idle_session(&app);
    wait("the first creation", || probe.open_sessions() == 1);

    probe.hold_thread(&app, &["test.epoch.open"]);
    enable(&app, &[]);
    let _second = idle_session(&app);
    enable(&app, &["test.epoch.open"]);
    probe.hold.store(false, Ordering::Relaxed);

    wait("the second activation", || probe.activations() == 2);
    // The replay comes before the reload's ConfigChanged.
    wait("the reload", || probe.current_holds(&Event::ConfigChanged));
    assert_eq!(probe.open_sessions(), 2);
    assert_eq!(probe.deactivations(), 1);
    wait_for_state(&app, "test.epoch.open", PluginState::Active);
}

/// The same with a session closed in between: the fresh instance counts
/// only the session still open.
#[test]
fn a_coalesced_disable_and_enable_replays_without_a_closed_session() {
    let root = tempfile::tempdir().unwrap();
    let probe = Probe::new(Spec::new("test.epoch.close", &[Capability::Sessions]));
    let app = app(&root, &["test.epoch.close"], vec![probe.factory()]);
    wait_for_state(&app, "test.epoch.close", PluginState::Active);
    let _kept = idle_session(&app);
    let closed = idle_session(&app);
    wait("both creations", || probe.open_sessions() == 2);

    probe.hold_thread(&app, &["test.epoch.close"]);
    enable(&app, &[]);
    app.close_session(closed).unwrap();
    enable(&app, &["test.epoch.close"]);
    probe.hold.store(false, Ordering::Relaxed);

    wait("the second activation", || probe.activations() == 2);
    // The replay comes before the reload's ConfigChanged.
    wait("the reload", || probe.current_holds(&Event::ConfigChanged));
    assert_eq!(probe.open_sessions(), 1);
    assert_eq!(probe.deactivations(), 1);
}

/// A failed plugin disabled and enabled again while the plugin thread is
/// busy with another plugin is still retried.
#[test]
fn a_coalesced_disable_and_enable_retries_a_failed_plugin() {
    let root = tempfile::tempdir().unwrap();
    let busy = Probe::new(Spec::new("test.epoch.busy", &[Capability::Sessions]));
    let mut spec = Spec::new("test.epoch.failing", &[]);
    spec.on_activate = OnActivate::Fail;
    let failing = Probe::new(spec);
    let both = ["test.epoch.busy", "test.epoch.failing"];
    let app = app(&root, &both, vec![busy.factory(), failing.factory()]);
    wait_for_state(&app, "test.epoch.failing", PluginState::Failed);
    wait_for_state(&app, "test.epoch.busy", PluginState::Active);
    assert_eq!(failing.activations(), 1);

    busy.hold_thread(&app, &both);
    enable(&app, &["test.epoch.busy"]);
    enable(&app, &both);
    busy.hold.store(false, Ordering::Relaxed);

    wait("the retry", || failing.activations() == 2);
    wait_for_state(&app, "test.epoch.failing", PluginState::Failed);
}

/// A session opened while the plugin thread is busy (its `SessionCreated`
/// still queued) and closed while the counting plugin is disabled, then
/// the plugin enabled again before the thread's next turn: the fresh
/// instance is handed the creation and the close, and counts none open
/// (adversarial round 1 of #48).
#[test]
fn a_session_opened_and_closed_across_a_coalesced_toggle_is_not_counted() {
    let root = tempfile::tempdir().unwrap();
    let count = Probe::new(Spec::new("test.stale.count", &[Capability::Sessions]));
    let busy = Probe::new(Spec::new("test.stale.busy", &[]));
    let both = ["test.stale.count", "test.stale.busy"];
    let app = app(&root, &both, vec![count.factory(), busy.factory()]);
    wait_for_state(&app, "test.stale.count", PluginState::Active);
    wait_for_state(&app, "test.stale.busy", PluginState::Active);

    busy.hold_thread(&app, &both);
    let session = idle_session(&app);
    enable(&app, &["test.stale.busy"]);
    app.close_session(session).unwrap();
    enable(&app, &both);
    busy.hold.store(false, Ordering::Relaxed);

    wait("the second activation", || count.activations() == 2);
    wait("both reloads", || count.reloads_seen() == 2);
    assert_eq!(count.open_sessions(), 0, "{:?}", count.events());
}

/// The same with the thread taking the disable first: it deactivates the
/// counting plugin while the session's `SessionCreated` is still queued
/// behind another event, and the enable comes after.
#[test]
fn a_session_opened_and_closed_across_a_toggle_is_not_counted() {
    let root = tempfile::tempdir().unwrap();
    let count = Probe::new(Spec::new("test.stale2.count", &[Capability::Sessions]));
    let busy = Probe::new(Spec::new("test.stale2.busy", &[]));
    let both = ["test.stale2.count", "test.stale2.busy"];
    let app = app(&root, &both, vec![count.factory(), busy.factory()]);
    wait_for_state(&app, "test.stale2.count", PluginState::Active);
    wait_for_state(&app, "test.stale2.busy", PluginState::Active);

    busy.hold_thread(&app, &both);
    // A reload queued ahead of the session's creation, for the disable's
    // turn to take.
    enable(&app, &both);
    let session = idle_session(&app);
    enable(&app, &["test.stale2.busy"]);
    app.close_session(session).unwrap();
    let held = busy.events().len();
    busy.step();
    wait_for_state(&app, "test.stale2.count", PluginState::Disabled);
    wait("the disable's reload, held", || {
        busy.events().len() == held + 1
    });
    enable(&app, &both);
    busy.hold.store(false, Ordering::Relaxed);

    wait("the second activation", || count.activations() == 2);
    wait("both reloads", || count.reloads_seen() == 2);
    assert_eq!(count.open_sessions(), 0, "{:?}", count.events());
}

/// The frontend's `on_config_applied` sees the plugins already following
/// that configuration: a plugin it enables reads as enabled there.
#[test]
fn the_frontend_reads_the_plugins_of_the_applied_configuration() {
    struct Frontend {
        app: Arc<OnceLock<std::sync::Weak<App>>>,
        seen: Arc<Mutex<Vec<bool>>>,
    }
    impl tessera_app_core::SessionObserver for Frontend {
        fn on_session_event(&self, _: SessionId, _: tessera_app_core::SessionEvent) {}
        fn on_config_applied(&self, _: u64) {
            if let Some(app) = self.app.get().and_then(std::sync::Weak::upgrade) {
                self.seen
                    .lock()
                    .unwrap()
                    .push(info(&app, "test.frontend.probe").enabled);
            }
        }
    }
    let root = tempfile::tempdir().unwrap();
    let probe = Probe::new(Spec::new("test.frontend.probe", &[]));
    let app = Arc::new(app(&root, &[], vec![probe.factory()]));
    let cell = Arc::new(OnceLock::new());
    cell.set(Arc::downgrade(&app)).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    app.set_observer(Some(Box::new(Frontend {
        app: Arc::clone(&cell),
        seen: Arc::clone(&seen),
    })));
    enable(&app, &["test.frontend.probe"]);
    enable(&app, &[]);
    assert_eq!(*seen.lock().unwrap(), [true, false]);
}

/// A plugin that sleeps 100 ms per event under the same flood: the queue
/// never holds more than its cap and the session reads on.
#[test]
fn a_slow_plugin_under_a_title_flood_keeps_the_queue_capped() {
    let root = tempfile::tempdir().unwrap();
    let flood = root.path().join("flood");
    titled_flood(&flood, 6000);
    let slow = Probe::new(Spec::new("test.flood.slow", &[Capability::Sessions]));
    slow.delay.store(100, Ordering::Relaxed);
    let app = app(&root, &["test.flood.slow"], vec![slow.factory()]);
    wait_for_state(&app, "test.flood.slow", PluginState::Active);
    let id = spawn(
        &app,
        &format!("cat '{}'; echo flood-done; sleep 600", flood.display()),
    );
    wait("the end of the flood", || {
        screen_holds(&app, id, "flood-done")
    });
    let stats = app.plugin_queue_stats();
    assert!(stats.high_water <= MAX_PLUGIN_EVENTS, "{stats:?}");
    assert!(stats.dropped > 0, "{stats:?}");
    assert!(slow.events().len() < 100, "{}", slow.events().len());
}

/// Reads `path` through a session and returns how long the read took, or
/// `None` once it has taken longer than `deadline`.
fn time_cat(
    app: &App,
    path: &std::path::Path,
    marker: &str,
    deadline: Duration,
) -> Option<Duration> {
    let start = Instant::now();
    let id = spawn(
        app,
        &format!("cat '{}'; echo {marker}; sleep 600", path.display()),
    );
    let took = loop {
        if screen_holds(app, id, marker) {
            break Some(start.elapsed());
        }
        if start.elapsed() > deadline {
            break None;
        }
        thread::sleep(Duration::from_millis(2));
    };
    app.close_session(id).unwrap();
    took
}

/// S6: 100 MB through a session, a title every KiB, with a plugin that
/// sleeps 100 ms per event, reads within 1.05x of the same with no plugin
/// enabled.
///
/// The bar is the best of up to ten pairs of reads, each pair one read with
/// the plugin and one without taken back to back (the order alternates), so
/// both reads of a pair see the same load. The best read with the plugin
/// over the best without, taken minutes apart, failed under load (#603:
/// 1.39 at load 55-82). It stays a wall-clock bar: a plugin that blocks the
/// session sleeps, so the reader's CPU time would not see it. That mutant
/// (the reader waits for room in the queue instead of dropping the event,
/// or runs the plugin itself) reads 100 MB in hours, so a read with the
/// plugin that takes 20 times its pair's read without, plus 10 s, fails at
/// once.
#[test]
#[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
fn a_slow_plugin_keeps_session_throughput() {
    const PAIRS: usize = 10;
    const MAX_RATIO: f64 = 1.05;
    let root = tempfile::tempdir().unwrap();
    let flood = root.path().join("flood");
    titled_flood(&flood, 100 * 1024 * 1024 / 1010);
    let slow = Probe::new(Spec::new("test.speed.slow", &[Capability::Sessions]));
    slow.delay.store(100, Ordering::Relaxed);
    let with = app(&root, &["test.speed.slow"], vec![slow.factory()]);
    let idle = Probe::new(Spec::new("test.speed.idle", &[Capability::Sessions]));
    let without = app(&root, &[], vec![idle.factory()]);
    wait_for_state(&with, "test.speed.slow", PluginState::Active);

    let unbounded = Duration::from_secs(3600);
    // The longest a read with the plugin may take: 20 times a read without
    // it plus 10 s.
    let deadline = |without: Duration| without * 20 + Duration::from_secs(10);
    let mut ratios = Vec::new();
    let mut fastest_without = unbounded;
    let passed = (0..PAIRS).any(|pair| {
        let read_without = || {
            time_cat(&without, &flood, &format!("end-{pair}-a"), unbounded)
                .expect("a read without a plugin ends")
        };
        let read_with = |without| {
            time_cat(&with, &flood, &format!("end-{pair}-b"), deadline(without)).unwrap_or_else(
                || {
                    panic!(
                        "the slow plugin blocked the session: a read with it took over \
                         {:?}, 20x a read without it plus 10 s",
                        deadline(without)
                    )
                },
            )
        };
        // The first pair reads without the plugin first, so a read with it
        // always has a deadline.
        let (took_with, took_without) = if pair % 2 == 0 {
            let took_without = read_without();
            (read_with(took_without), took_without)
        } else {
            (read_with(fastest_without), read_without())
        };
        fastest_without = fastest_without.min(took_without);
        let ratio = took_with.as_secs_f64() / took_without.as_secs_f64();
        eprintln!(
            "pair {pair}: with {took_with:?}, without {took_without:?}, ratio {ratio:.3}, {:?}",
            with.plugin_queue_stats()
        );
        ratios.push(ratio);
        ratio <= MAX_RATIO
    });
    assert!(
        passed,
        "every pair read more than {MAX_RATIO}x slower with the plugin: {ratios:.3?}"
    );
    assert!(with.plugin_queue_stats().dropped > 0);
    assert_eq!(idle.events(), Vec::<Event>::new());
}

// --- Done when 4: caps -------------------------------------------------------

/// `[plugins] enabled` honours 64 ids of up to 128 bytes: an id at the
/// length cap enables its plugin, one past it is ignored with a warning,
/// an unknown id gets one warning, and ids past the 64th are ignored
/// without one from the host (the loader gives it, #560:
/// `a_load_past_the_cap_logs_one_warning`). A reload with the same list
/// warns no more.
#[test]
fn enabled_ids_are_capped_in_count_and_length() {
    let root = tempfile::tempdir().unwrap();
    let long_id = "l".repeat(MAX_PLUGIN_ID_LEN);
    let too_long = "t".repeat(MAX_PLUGIN_ID_LEN + 1);
    let long = Probe::new(Spec::new(&long_id, &[]));
    let late = Probe::new(Spec::new("test.enabled.late", &[]));
    let unknown: Vec<String> = (0..MAX_ENABLED_PLUGINS - 3)
        .map(|i| format!("test.enabled.unknown-{i}"))
        .collect();

    // At the cap: 64 ids, the last of them a real plugin.
    let mut at_cap: Vec<&str> = vec![&long_id, &too_long];
    at_cap.extend(unknown.iter().map(String::as_str));
    at_cap.push("test.enabled.late");
    assert_eq!(at_cap.len(), MAX_ENABLED_PLUGINS);
    let app = app(&root, &at_cap, vec![long.factory(), late.factory()]);
    wait_for_state(&app, &long_id, PluginState::Active);
    wait_for_state(&app, "test.enabled.late", PluginState::Active);
    let warned = warnings_here();
    assert_eq!(warned.len(), 1 + unknown.len(), "{warned:?}");
    assert_eq!(
        warned.iter().filter(|w| w.contains("longer than")).count(),
        1
    );
    assert!(warned.iter().all(|w| !w.contains(&too_long)), "{warned:?}");

    // Past the cap: one more unknown id pushes the real plugin out.
    let mut past_cap = at_cap.clone();
    past_cap.insert(2, "test.enabled.extra");
    enable(&app, &past_cap);
    wait_for_state(&app, "test.enabled.late", PluginState::Disabled);
    assert!(!info(&app, "test.enabled.late").enabled);
    assert_eq!(late.deactivations(), 1);
    let warned = warnings_here();
    // The first 64 of `past_cap` warn again (the list changed): the
    // too-long id, the extra unknown id and the 61 unknown ids.
    assert_eq!(warned.len(), 2 * (1 + unknown.len()) + 1, "{warned:?}");
    assert!(
        warned.iter().all(|w| !w.contains("more than")),
        "{warned:?}"
    );

    // The same list again: no new warnings.
    enable(&app, &past_cap);
    assert_eq!(warnings_here().len(), warned.len());
}

/// The warnings logged so far, on any thread, about ids past
/// [`MAX_ENABLED_PLUGINS`]: the loader's and any other that names the cap.
fn warnings_past_the_cap() -> Vec<String> {
    let cap = format!("more than {MAX_ENABLED_PLUGINS} ids");
    log()
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.level == tracing::Level::WARN && line.text.contains(&cap))
        .map(|line| line.text.clone())
        .collect()
}

/// #560: a `config.toml` whose `[plugins] enabled` lists more than 64 ids
/// is loaded with exactly one warning about them, the loader's, which
/// gives no id (S8); the host applies the first 64 without warning again.
/// A reload with another list still past the cap logs no other.
#[test]
fn a_load_past_the_cap_logs_one_warning() {
    log();
    let root = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::new(root.path().join("config"));
    std::fs::create_dir_all(paths.root()).unwrap();
    let listed = |count: usize| {
        let ids: Vec<String> = (0..count).map(|_| format!("\"{HELLO}\"")).collect();
        format!("[plugins]\nenabled = [{}]\n", ids.join(", "))
    };
    std::fs::write(paths.config_file(), listed(MAX_ENABLED_PLUGINS + 1)).unwrap();
    let app = App::with_config_dir(paths.clone(), WatchOptions::default());
    assert_eq!(app.config().plugins.enabled.len(), MAX_ENABLED_PLUGINS + 1);
    wait_for_state(&app, HELLO, PluginState::Active);
    let warned = warnings_past_the_cap();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].contains("plugins.enabled"), "{warned:?}");
    assert!(!warned[0].contains(HELLO), "{warned:?}");

    let generation = app.config_generation();
    std::fs::write(paths.config_file(), listed(MAX_ENABLED_PLUGINS + 3)).unwrap();
    wait("the longer list to apply", || {
        app.config_generation() != generation
    });
    assert_eq!(app.config().plugins.enabled.len(), MAX_ENABLED_PLUGINS + 3);
    assert_eq!(warnings_past_the_cap(), warned);
    drop(app);
}

/// A string of `len` bytes: `x` and then two-byte `é`, so a cut at an odd
/// byte count lands inside a character.
fn text_of(len: usize) -> String {
    let mut text = "x".repeat(len % 2);
    text.push_str(&"é".repeat(len / 2));
    assert_eq!(text.len(), len);
    text
}

/// Status items: at most 8; id, text and tooltip cut to 64, 256 and 1024
/// bytes at a character boundary; control, bidi-control and line-separator
/// characters removed. Values at the caps pass unchanged.
#[test]
fn status_items_are_capped_and_cleaned() {
    let root = tempfile::tempdir().unwrap();
    let probe = Probe::new(Spec::new("test.items", &[Capability::StatusBar]));
    let app = app(&root, &["test.items"], vec![probe.factory()]);
    wait_for_state(&app, "test.items", PluginState::Active);
    let host = probe.host();

    let at_cap: Vec<StatusItem> = (0..MAX_STATUS_ITEMS)
        .map(|i| StatusItem {
            id: format!("{i}{}", "i".repeat(MAX_STATUS_ITEM_ID_LEN - 1)),
            text: "a".repeat(MAX_STATUS_TEXT_LEN),
            tooltip: Some("b".repeat(MAX_STATUS_TOOLTIP_LEN)),
        })
        .collect();
    host.set_status_items(at_cap.clone()).unwrap();
    assert_eq!(info(&app, "test.items").status_items, at_cap);

    let mut past_cap = at_cap.clone();
    past_cap.push(StatusItem {
        id: "ninth".into(),
        text: "ninth".into(),
        tooltip: None,
    });
    past_cap[0] = StatusItem {
        id: text_of(MAX_STATUS_ITEM_ID_LEN + 1),
        text: text_of(MAX_STATUS_TEXT_LEN + 1),
        tooltip: Some(text_of(MAX_STATUS_TOOLTIP_LEN + 1)),
    };
    past_cap[1] = StatusItem {
        id: "ctl\u{1b}[31m\u{7}".into(),
        text: "a\u{202e}b\u{2066}c\u{200f}d\u{2028}e\u{2029}f\r\ng\u{9b}h\u{0}".into(),
        tooltip: Some("tip\u{85}\u{7f}".into()),
    };
    host.set_status_items(past_cap).unwrap();
    let items = info(&app, "test.items").status_items;
    assert_eq!(items.len(), MAX_STATUS_ITEMS);
    assert!(items.iter().all(|item| item.id != "ninth"));
    // Cut below the cap where the cap falls inside `é`.
    assert_eq!(items[0].id, text_of(MAX_STATUS_ITEM_ID_LEN - 1));
    assert_eq!(items[0].text, text_of(MAX_STATUS_TEXT_LEN - 1));
    assert_eq!(
        items[0].tooltip.as_deref(),
        Some(&*text_of(MAX_STATUS_TOOLTIP_LEN - 1))
    );
    assert_eq!(items[1].id, "ctl[31m");
    assert_eq!(items[1].text, "abcdefgh");
    assert_eq!(items[1].tooltip.as_deref(), Some("tip"));
    assert_eq!(items[2..], at_cap[2..MAX_STATUS_ITEMS]);

    // An empty list clears them.
    host.set_status_items(Vec::new()).unwrap();
    assert_eq!(info(&app, "test.items").status_items, []);
}

fn storage_full(result: tessera_plugin_api::Result<()>) -> bool {
    matches!(result, Err(Error::Plugin(message)) if message == "storage full")
}

/// A plugin can still use its storage in `deactivate`, and finds what it
/// stored there when it is enabled again.
#[test]
fn deactivate_may_still_store() {
    let root = tempfile::tempdir().unwrap();
    let probe = Probe::new(Spec::new("test.bye.store", &[Capability::Storage]));
    let app = app(&root, &["test.bye.store"], vec![probe.factory()]);
    wait_for_state(&app, "test.bye.store", PluginState::Active);
    enable(&app, &[]);
    wait_for_state(&app, "test.bye.store", PluginState::Disabled);
    assert_eq!(probe.seen.lock().unwrap().stored_on_deactivate, Some(true));
    enable(&app, &["test.bye.store"]);
    wait("the second activation", || probe.activations() == 2);
    let stored = probe.host().storage_get("bye").unwrap();
    assert_eq!(stored.as_deref(), Some(&b"1"[..]));
}

/// Storage: 64 keys, keys of 128 bytes, values of 64 KiB and 1 MiB in all
/// per plugin, each accepted at its cap and refused one past it with
/// `storage full`, leaving what was stored untouched.
#[test]
fn storage_is_capped_per_plugin() {
    let root = tempfile::tempdir().unwrap();
    let probe = Probe::new(Spec::new("test.storage", &[Capability::Storage]));
    let other = Probe::new(Spec::new("test.storage.other", &[Capability::Storage]));
    let app = app(
        &root,
        &["test.storage", "test.storage.other"],
        vec![probe.factory(), other.factory()],
    );
    wait_for_state(&app, "test.storage", PluginState::Active);
    wait_for_state(&app, "test.storage.other", PluginState::Active);
    let host = probe.host();

    // Key length.
    let key = "k".repeat(MAX_STORAGE_KEY_LEN);
    host.storage_set(&key, b"v").unwrap();
    assert!(storage_full(
        host.storage_set(&"k".repeat(MAX_STORAGE_KEY_LEN + 1), b"v")
    ));
    assert_eq!(host.storage_get(&key).unwrap().as_deref(), Some(&b"v"[..]));
    // Value length.
    let value = vec![7; MAX_STORAGE_VALUE_LEN];
    host.storage_set("big", &value).unwrap();
    assert!(storage_full(
        host.storage_set("bigger", &vec![7; MAX_STORAGE_VALUE_LEN + 1])
    ));
    // An overwrite past the value cap leaves the old value.
    assert!(storage_full(
        host.storage_set("big", &vec![8; MAX_STORAGE_VALUE_LEN + 1])
    ));
    assert_eq!(host.storage_get("big").unwrap(), Some(value));
    assert_eq!(host.storage_get("bigger").unwrap(), None);

    // Key count: 2 stored, 62 more fill it.
    for i in 0..MAX_STORAGE_KEYS - 2 {
        host.storage_set(&format!("n{i}"), b"").unwrap();
    }
    assert!(storage_full(host.storage_set("one-more", b"")));
    // An overwrite of a stored key is not a new key.
    host.storage_set("n0", b"again").unwrap();
    assert_eq!(
        host.storage_get("n0").unwrap().as_deref(),
        Some(&b"again"[..])
    );

    // Total bytes, keys included, in the other plugin: 15 values of 64 KiB
    // under one-byte keys, then exactly the rest.
    let host = other.host();
    for i in 0..15u8 {
        host.storage_set(
            &char::from(b'a' + i).to_string(),
            &vec![0; MAX_STORAGE_VALUE_LEN],
        )
        .unwrap();
    }
    let used = 15 * (1 + MAX_STORAGE_VALUE_LEN);
    let rest = MAX_STORAGE_BYTES - used - 1;
    assert!(storage_full(host.storage_set("z", &vec![0; rest + 1])));
    host.storage_set("z", &vec![0; rest]).unwrap();
    assert!(storage_full(host.storage_set("y", b"")));
    // Shrinking a value frees room.
    host.storage_set("z", &[]).unwrap();
    host.storage_set("y", b"").unwrap();
}

/// A host kept past deactivation refuses every call, and the next
/// activation's storage still holds what the plugin stored.
#[test]
fn a_host_outliving_its_activation_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let probe = Probe::new(Spec::new(
        "test.stale",
        &[Capability::StatusBar, Capability::Storage],
    ));
    let app = app(&root, &["test.stale"], vec![probe.factory()]);
    wait_for_state(&app, "test.stale", PluginState::Active);
    let stale = probe.host();
    stale.storage_set("kept", b"1").unwrap();

    enable(&app, &[]);
    wait_for_state(&app, "test.stale", PluginState::Disabled);
    assert!(matches!(stale.storage_get("kept"), Err(Error::Plugin(_))));
    assert!(matches!(
        stale.set_status_items(vec![StatusItem {
            id: "late".into(),
            text: "late".into(),
            tooltip: None,
        }]),
        Err(Error::Plugin(_))
    ));
    assert_eq!(info(&app, "test.stale").status_items, []);

    enable(&app, &["test.stale"]);
    wait("a second activation", || probe.activations() == 2);
    wait_for_state(&app, "test.stale", PluginState::Active);
    assert_eq!(
        probe.host().storage_get("kept").unwrap().as_deref(),
        Some(&b"1"[..])
    );
}

// --- #489: lifecycle events past the cap, the queued head, errors, output ---

/// A plugin held while 1024 sessions' creations and closes fill the queue
/// (21.1 m3, adversarial cycle 2): the creations and closes that find it
/// full are dropped, a close whose creation is still queued takes that
/// creation out, and once the plugin moves again it is told of what it
/// missed. The sessions it counts open are then exactly the `App`'s.
#[test]
fn lifecycle_events_past_the_cap_leave_the_session_count_right() {
    let root = tempfile::tempdir().unwrap();
    let probe = Probe::new(Spec::new("test.cap.count", &[Capability::Sessions]));
    let app = app(&root, &["test.cap.count"], vec![probe.factory()]);
    wait_for_state(&app, "test.cap.count", PluginState::Active);
    let open = || app.spawn_test_session(PtySize::new(4, 10)).unwrap();
    let raw = |id: SessionId| id.get();

    let first = open();
    wait("the first creation", || probe.open_sessions() == 1);
    probe.hold_thread(&app, &["test.cap.count"]);
    // 1 + 2 × 511 + 1 = 1024 lifecycle events: the queue is full of them.
    let queued = open();
    for _ in 0..511 {
        let id = open();
        app.close_session(id).unwrap();
    }
    let last = open();
    let full = app.plugin_queue_stats();
    assert_eq!(full.queued, MAX_PLUGIN_EVENTS, "{full:?}");
    // Dropped: `late`'s creation, `first`'s close (its creation was
    // delivered).
    let late = open();
    app.close_session(first).unwrap();
    // `queued`'s close takes its queued creation out, which leaves room
    // for `after_room`.
    app.close_session(queued).unwrap();
    let after_room = open();
    // Both of `brief`'s events are dropped.
    let brief = open();
    app.close_session(brief).unwrap();
    let after = app.plugin_queue_stats();
    assert_eq!(after.queued, MAX_PLUGIN_EVENTS, "{after:?}");
    assert_eq!(after.dropped, full.dropped + 5, "{after:?}");

    probe.hold.store(false, Ordering::Relaxed);
    let want: std::collections::BTreeSet<u64> = [raw(late), raw(last), raw(after_room)].into();
    wait("the queue to drain", || {
        app.plugin_queue_stats().queued == 0
    });
    wait("the counts to settle", || probe.open_set() == want);
    // A later turn changes nothing.
    let check = open();
    app.close_session(check).unwrap();
    wait("the check session's close", || {
        probe.current_holds(&Event::SessionClosed {
            session: plugin_session(check),
        })
    });
    assert_eq!(probe.open_set(), want);
    assert_eq!(probe.open_sessions(), 3);
    for id in [late, last, after_room] {
        app.close_session(id).unwrap();
    }
}

/// Fills the plugin queue with lifecycle events while the plugin thread is
/// held: 1 + 2 × 511 + 1 = 1024. Returns the two sessions left open.
fn fill_the_queue(app: &App) -> (SessionId, SessionId) {
    let open = || app.spawn_test_session(PtySize::new(4, 10)).unwrap();
    let first = open();
    for _ in 0..511 {
        let id = open();
        app.close_session(id).unwrap();
    }
    let last = open();
    let full = app.plugin_queue_stats();
    assert_eq!(full.queued, MAX_PLUGIN_EVENTS, "{full:?}");
    (first, last)
}

/// A close that finds the queue full, and no creation of its session there,
/// is the only event dropped: the plugin is still told of it on the
/// thread's next turn (24.13 m2).
#[test]
fn a_close_dropped_alone_is_made_good() {
    let root = tempfile::tempdir().unwrap();
    let probe = Probe::new(Spec::new("test.cap.close", &[Capability::Sessions]));
    let app = app(&root, &["test.cap.close"], vec![probe.factory()]);
    wait_for_state(&app, "test.cap.close", PluginState::Active);
    let delivered = app.spawn_test_session(PtySize::new(4, 10)).unwrap();
    wait("the creation", || probe.open_sessions() == 1);
    probe.hold_thread(&app, &["test.cap.close"]);
    let (first, last) = fill_the_queue(&app);
    let full = app.plugin_queue_stats();
    app.close_session(delivered).unwrap();
    let after = app.plugin_queue_stats();
    assert_eq!(after.dropped, full.dropped + 1, "{after:?}");

    probe.hold.store(false, Ordering::Relaxed);
    let want: std::collections::BTreeSet<u64> = [first.get(), last.get()].into();
    wait("the queue to drain", || {
        app.plugin_queue_stats().queued == 0
    });
    wait("the close to be made good", || probe.open_set() == want);
    for id in [first, last] {
        app.close_session(id).unwrap();
    }
}

/// Overflows the queue while `busy` holds the plugin thread, so that the
/// next turn tells `count` what it missed, and checks that `count` then
/// holds exactly the sessions open. [`Probe::open_set`] fails on a close of
/// a session this activation was never told of.
fn overflow_and_check_the_count(app: &App, busy: &Probe, count: &Probe, enabled: &[&str]) {
    busy.hold_thread(app, enabled);
    let (first, last) = fill_the_queue(app);
    let late = app.spawn_test_session(PtySize::new(4, 10)).unwrap();
    busy.hold.store(false, Ordering::Relaxed);
    let want: std::collections::BTreeSet<u64> = [first.get(), last.get(), late.get()].into();
    wait("the queue to drain", || {
        app.plugin_queue_stats().queued == 0
    });
    wait("the counts to settle", || count.open_set() == want);
    for id in [first, last, late] {
        app.close_session(id).unwrap();
    }
}

/// A plugin disabled with a session open forgets it: the session closed
/// meanwhile is not reported closed to the next activation when an overflow
/// later makes the thread compare what it told the plugin (24.13 m3).
#[test]
fn a_disabled_plugin_forgets_the_sessions_it_was_told_of() {
    let root = tempfile::tempdir().unwrap();
    let count = Probe::new(Spec::new("test.forget.count", &[Capability::Sessions]));
    let busy = Probe::new(Spec::new("test.forget.busy", &[]));
    let both = ["test.forget.count", "test.forget.busy"];
    let app = app(&root, &both, vec![count.factory(), busy.factory()]);
    wait_for_state(&app, "test.forget.count", PluginState::Active);
    wait_for_state(&app, "test.forget.busy", PluginState::Active);
    let session = app.spawn_test_session(PtySize::new(4, 10)).unwrap();
    wait("the creation", || count.open_sessions() == 1);

    enable(&app, &["test.forget.busy"]);
    wait_for_state(&app, "test.forget.count", PluginState::Disabled);
    app.close_session(session).unwrap();
    enable(&app, &both);
    wait_for_state(&app, "test.forget.count", PluginState::Active);
    overflow_and_check_the_count(&app, &busy, &count, &both);
}

/// The same for a plugin that failed with sessions open and was retried:
/// the failed activation's sessions are forgotten (24.13 m3).
#[test]
fn a_failed_plugin_forgets_the_sessions_it_was_told_of() {
    let root = tempfile::tempdir().unwrap();
    let count = Probe::new(Spec::new("test.failed.count", &[Capability::Sessions]));
    let busy = Probe::new(Spec::new("test.failed.busy", &[]));
    let both = ["test.failed.count", "test.failed.busy"];
    let app = app(&root, &both, vec![count.factory(), busy.factory()]);
    wait_for_state(&app, "test.failed.count", PluginState::Active);
    wait_for_state(&app, "test.failed.busy", PluginState::Active);
    let session = app.spawn_test_session(PtySize::new(4, 10)).unwrap();
    wait("the creation", || count.open_sessions() == 1);

    count.panic_next.store(true, Ordering::Relaxed);
    let last_straw = app.spawn_test_session(PtySize::new(4, 10)).unwrap();
    wait_for_state(&app, "test.failed.count", PluginState::Failed);
    app.close_session(session).unwrap();
    app.close_session(last_straw).unwrap();
    enable(&app, &["test.failed.busy"]);
    wait_for_state(&app, "test.failed.count", PluginState::Disabled);
    enable(&app, &both);
    wait_for_state(&app, "test.failed.count", PluginState::Active);
    overflow_and_check_the_count(&app, &busy, &count, &both);
}

/// 21.1 m4 MX2: a plugin enabled again while a session's creation is still
/// queued is told of the sessions open as of the queue's head, so it counts
/// that session once (from the queued creation), not twice.
#[test]
fn a_reenabled_plugin_counts_a_session_whose_creation_is_queued_once() {
    let root = tempfile::tempdir().unwrap();
    let busy = Probe::new(Spec::new("test.head.busy", &[Capability::Sessions]));
    let count = Probe::new(Spec::new("test.head.count", &[Capability::Sessions]));
    let both = ["test.head.busy", "test.head.count"];
    let app = app(&root, &both, vec![busy.factory(), count.factory()]);
    wait_for_state(&app, "test.head.busy", PluginState::Active);
    wait_for_state(&app, "test.head.count", PluginState::Active);

    busy.hold_thread(&app, &both);
    enable(&app, &["test.head.busy"]);
    // Queued: `busy` still wants sessions.
    let session = app.spawn_test_session(PtySize::new(4, 10)).unwrap();
    enable(&app, &both);
    busy.hold.store(false, Ordering::Relaxed);

    wait("the second activation", || count.activations() == 2);
    wait("the creation", || {
        count.current_holds(&Event::SessionCreated {
            session: plugin_session(session),
        })
    });
    wait("the queue to drain", || {
        app.plugin_queue_stats().queued == 0
    });
    assert_eq!(count.open_sessions(), 1, "{:?}", count.events());
    app.close_session(session).unwrap();
}

/// 21.1 m4 MX13 (S3, S8): an id in `[plugins] enabled` that no linked
/// plugin has is logged sanitised and escaped, never as written: no escape
/// sequence or bidi control reaches the log.
#[test]
fn an_unknown_enabled_id_is_logged_escaped() {
    let root = tempfile::tempdir().unwrap();
    let id = "test.unknown.\u{1b}[2Jmx13\u{202e}\u{7}";
    let app = app(&root, &[id], vec![]);
    let logged = warnings_naming("mx13");
    assert_eq!(logged.len(), 1, "{logged:?}");
    let line = &logged[0];
    for raw in ['\u{1b}', '\u{202e}', '\u{7}'] {
        assert!(!line.contains(raw), "{raw:?} in {line:?}");
    }
    assert!(
        line.contains(r"test.unknown.\u{1b}[2Jmx13\u{202e}\u{7}"),
        "{line:?}"
    );
    drop(app);
}

/// 21.1 m4 MX22: a plugin whose `on_event` returns an error keeps running
/// and gets the events after it; only a panic fails a plugin.
#[test]
fn an_event_error_does_not_fail_the_plugin() {
    let root = tempfile::tempdir().unwrap();
    let mut spec = Spec::new("test.event.error", &[Capability::Sessions]);
    spec.error_on_event = true;
    let probe = Probe::new(spec);
    let app = app(&root, &["test.event.error"], vec![probe.factory()]);
    wait_for_state(&app, "test.event.error", PluginState::Active);
    let first = app.spawn_test_session(PtySize::new(4, 10)).unwrap();
    let second = app.spawn_test_session(PtySize::new(4, 10)).unwrap();
    wait("both creations", || probe.open_sessions() == 2);
    assert_eq!(info(&app, "test.event.error").state, PluginState::Active);
    assert_eq!(warnings_naming("test.event.error"), Vec::<String>::new());
    for id in [first, second] {
        app.close_session(id).unwrap();
    }
    wait("both closes", || probe.open_sessions() == 0);
    assert_eq!(probe.activations(), 1);
}

/// P11: a session's output reaches the frontend's observer while a plugin
/// granted sessions is active, and never the plugin host: the plugin is
/// handed the session's creation and title, no event for its output.
#[test]
fn output_reaches_the_frontend_and_not_the_plugins() {
    log();
    let root = tempfile::tempdir().unwrap();
    let probe = Probe::new(Spec::new("test.output", &[Capability::Sessions]));
    let app = app(&root, &["test.output"], vec![probe.factory()]);
    wait_for_state(&app, "test.output", PluginState::Active);
    let outputs = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&outputs);
    app.set_observer(Some(Box::new(move |_, event| {
        if event == tessera_app_core::SessionEvent::Output {
            seen.fetch_add(1, Ordering::Relaxed);
        }
    })));
    let id = spawn(
        &app,
        "printf 'out-1\\n'; printf '\\033]0;p11\\007'; sleep 600",
    );
    wait("the title", || {
        probe
            .events()
            .iter()
            .any(|event| matches!(event, Event::TitleChanged { .. }))
    });
    wait("the output", || outputs.load(Ordering::Relaxed) > 0);
    let session = plugin_session(id);
    assert_eq!(
        probe.events(),
        [
            Event::SessionCreated { session },
            Event::TitleChanged {
                session,
                title: "p11".into()
            }
        ]
    );
    app.close_session(id).unwrap();
    app.set_observer(None);
    let panicked: Vec<_> = log()
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.text.contains("session observer panicked"))
        .cloned()
        .collect();
    assert!(panicked.is_empty(), "{panicked:?}");
}
