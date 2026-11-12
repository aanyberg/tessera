//! Live reload: an external edit reaches the store within 500 ms, rapid
//! writes coalesce into one event, and both backends behave the same.

use std::time::{Duration, Instant};

use tessera_config::{
    Config, ConfigEvent, ConfigPaths, ConfigStore, WatchBackend, WatchOptions, Watcher,
    write_atomic,
};

/// Bound for an event that follows straight from a kernel event or a poll
/// sample: at most a poll interval plus the debounce (150 ms by default),
/// well inside the 500 ms the issue requires.
const LIMIT: Duration = Duration::from_millis(500);
const BACKENDS: [WatchBackend; 2] = [WatchBackend::Native, WatchBackend::Poll];

fn font_size_doc(size: u32) -> String {
    format!("[appearance]\nfont_size = {size}\n")
}

/// How long a test waits for an event that is due (#253 item 9, #262 N2).
/// Only `font_size_edit_is_observed_within_500ms` asserts a latency
/// ([`LIMIT`]); every other wait only needs the event to arrive, so it is
/// long enough for a starved process (`taskpolicy -b` on a loaded machine).
/// `recv_timeout` returns as soon as the event arrives, so the headroom
/// costs nothing when the backend is on time.
const EVENTUALLY: Duration = Duration::from_secs(5);

fn options(backend: WatchBackend) -> WatchOptions {
    WatchOptions {
        backend,
        ..Default::default()
    }
}

fn open_watched(opts: WatchOptions) -> (tempfile::TempDir, ConfigStore, Watcher) {
    let dir = tempfile::tempdir().unwrap();
    let store = ConfigStore::open(ConfigPaths::new(dir.path())).unwrap();
    let watcher = store.watch(opts).unwrap();
    (dir, store, watcher)
}

fn expect_changed(event: ConfigEvent, backend: WatchBackend) -> tessera_config::ConfigChanged {
    match event {
        ConfigEvent::Changed(changed) => changed,
        other => panic!("{backend:?}: expected Changed, got {other:?}"),
    }
}

fn expect_invalid(event: ConfigEvent, backend: WatchBackend) {
    match event {
        ConfigEvent::Invalid(_) => {}
        other => panic!("{backend:?}: expected Invalid, got {other:?}"),
    }
}

#[test]
fn font_size_edit_is_observed_within_500ms() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = open_watched(options(backend));
        assert_eq!(store.config().appearance.font_size, 13.0);

        let started = Instant::now();
        std::fs::write(store.paths().config_file(), font_size_doc(17)).unwrap();

        let event = watcher
            .recv_timeout(LIMIT)
            .unwrap_or_else(|e| panic!("{backend:?}: no event within 500 ms: {e}"));
        let elapsed = started.elapsed();
        let changed = expect_changed(event, backend);
        assert_eq!(changed.config.appearance.font_size, 17.0);
        assert_eq!(changed.warnings, Vec::new());
        assert_eq!(
            store.config().appearance.font_size,
            17.0,
            "{backend:?}: store applies the change"
        );
        assert!(elapsed < LIMIT, "{backend:?}: took {elapsed:?}");
    }
}

/// Writes closer together than the debounce are applied as one change,
/// the last. Independent of scheduling (#283 item 4): a stall of the
/// writing thread is a quiet period the watcher may rightly settle in, so
/// each gap between two writes of at least half the debounce allows one
/// more event; `max_wait` is out of reach. A watcher that does not
/// coalesce reports a change per write whatever the gaps.
#[test]
fn rapid_writes_are_debounced_into_one_event() {
    // Longer than a write takes under background QoS (about 200 ms, its
    // I/O throttled), so the writes coalesce there too.
    const DEBOUNCE: Duration = Duration::from_millis(600);
    for backend in BACKENDS {
        let (_dir, store, watcher) = open_watched(WatchOptions {
            backend,
            poll_interval: Duration::from_millis(10),
            debounce: DEBOUNCE,
            max_wait: Duration::from_secs(60),
            ..Default::default()
        });
        let path = store.paths().config_file();

        let mut written = Vec::new();
        for size in 14..=20 {
            std::fs::write(&path, font_size_doc(size)).unwrap();
            written.push(Instant::now());
            std::thread::sleep(Duration::from_millis(5));
        }
        let stalls = written
            .windows(2)
            .filter(|pair| pair[1].duration_since(pair[0]) >= DEBOUNCE / 2)
            .count();

        let mut changes = vec![expect_changed(
            watcher.recv_timeout(EVENTUALLY).expect("one event"),
            backend,
        )];
        while let Ok(event) = watcher.recv_timeout(DEBOUNCE * 2 + Duration::from_millis(300)) {
            changes.push(expect_changed(event, backend));
        }
        assert_eq!(
            changes.last().unwrap().config.appearance.font_size,
            20.0,
            "{backend:?}: the final value is applied"
        );
        assert!(
            changes.len() <= 1 + stalls,
            "{backend:?}: {} events for writes with {stalls} stalls between them",
            changes.len()
        );
    }
}

#[test]
fn rename_save_and_delete_are_observed() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = open_watched(options(backend));
        let path = store.paths().config_file();

        // Editors commonly write a sibling file and rename it over the target.
        let tmp = path.with_extension("toml.new");
        std::fs::write(&tmp, font_size_doc(18)).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        let changed = expect_changed(watcher.recv_timeout(EVENTUALLY).expect("rename"), backend);
        assert_eq!(changed.config.appearance.font_size, 18.0);

        // An in-place write to the renamed-in file is still seen.
        std::fs::write(&path, font_size_doc(19)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(EVENTUALLY).expect("write"), backend);
        assert_eq!(changed.config.appearance.font_size, 19.0);

        // Deleting the file applies the defaults; recreating it is seen too.
        std::fs::remove_file(&path).unwrap();
        let changed = expect_changed(watcher.recv_timeout(EVENTUALLY).expect("delete"), backend);
        assert_eq!(*changed.config, Config::default());
        std::fs::write(&path, font_size_doc(21)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(EVENTUALLY).expect("recreate"), backend);
        assert_eq!(changed.config.appearance.font_size, 21.0);
    }
}

#[test]
fn invalid_edit_reports_error_and_keeps_last_good_config() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = open_watched(options(backend));
        let path = store.paths().config_file();

        std::fs::write(&path, "[appearance\n").unwrap();
        let event = watcher.recv_timeout(EVENTUALLY).expect("event");
        assert!(
            matches!(event, ConfigEvent::Invalid(_)),
            "{backend:?}: got {event:?}"
        );
        assert_eq!(*store.config(), Config::default());
        assert!(store.last_error().is_some());

        std::fs::write(&path, font_size_doc(15)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(EVENTUALLY).expect("event"), backend);
        assert_eq!(changed.config.appearance.font_size, 15.0);
        assert!(store.last_error().is_none());
    }
}

#[test]
fn repairing_file_to_unchanged_config_still_emits_changed() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = open_watched(options(backend));
        let path = store.paths().config_file();

        std::fs::write(&path, "[appearance\n").unwrap();
        let event = watcher.recv_timeout(EVENTUALLY).expect("event");
        assert!(matches!(event, ConfigEvent::Invalid(_)), "{event:?}");

        // Repair to a document equal to what is applied (the defaults).
        std::fs::write(&path, "").unwrap();
        let changed = expect_changed(watcher.recv_timeout(EVENTUALLY).expect("recovery"), backend);
        assert_eq!(*changed.config, Config::default());
        assert!(
            store.last_error().is_none(),
            "{backend:?}: error cleared on recovery"
        );
    }
}

#[test]
fn edit_between_open_and_watch_is_applied() {
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(ConfigPaths::new(dir.path())).unwrap();
        std::fs::write(store.paths().config_file(), font_size_doc(16)).unwrap();

        let watcher = store.watch(options(backend)).unwrap();
        assert_eq!(
            store.config().appearance.font_size,
            16.0,
            "{backend:?}: applied before watch() returns"
        );
        let changed = expect_changed(watcher.try_recv().expect("reported"), backend);
        assert_eq!(changed.config.appearance.font_size, 16.0);
        assert!(
            watcher.recv_timeout(Duration::from_millis(300)).is_err(),
            "{backend:?}: reported once"
        );
    }
}

#[test]
fn comment_only_edit_emits_nothing() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = open_watched(WatchOptions {
            backend,
            poll_interval: Duration::from_millis(10),
            debounce: Duration::from_millis(30),
            ..Default::default()
        });
        let path = store.paths().config_file();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("\n# trailing comment\n");
        std::fs::write(&path, text).unwrap();
        assert!(
            watcher.recv_timeout(Duration::from_millis(300)).is_err(),
            "{backend:?}"
        );
        assert_eq!(*store.config(), Config::default());
    }
}

#[test]
fn zero_poll_interval_does_not_spin_and_still_works() {
    let (_dir, store, watcher) = open_watched(WatchOptions {
        backend: WatchBackend::Poll,
        poll_interval: Duration::ZERO,
        debounce: Duration::from_millis(30),
        ..Default::default()
    });
    std::fs::write(store.paths().config_file(), font_size_doc(22)).unwrap();
    let changed = expect_changed(
        watcher.recv_timeout(EVENTUALLY).expect("event"),
        WatchBackend::Poll,
    );
    assert_eq!(changed.config.appearance.font_size, 22.0);
}

/// `Duration::MAX` in any option must not kill the thread (an unbounded
/// `max_wait` once overflowed `Instant + Duration`; an unbounded
/// `poll_interval` once tripped a `kqueue` timeout assertion). With the
/// debounce left at its default the edit still arrives in time: for the
/// native backend `poll_interval` only matters while the directory is
/// missing, so it can be huge too; for the polling backend it is the
/// sampling rate, so a huge value is exercised by `huge_debounce_...` below
/// instead.
#[test]
fn huge_max_wait_and_poll_interval_still_observe_an_edit() {
    for backend in BACKENDS {
        let poll_interval = match backend {
            WatchBackend::Native => Duration::MAX,
            _ => Duration::from_millis(10),
        };
        let (_dir, store, watcher) = open_watched(WatchOptions {
            backend,
            poll_interval,
            debounce: Duration::from_millis(50),
            max_wait: Duration::MAX,
            ..Default::default()
        });
        std::fs::write(store.paths().config_file(), font_size_doc(24)).unwrap();
        let event = watcher
            .recv_timeout(EVENTUALLY)
            .unwrap_or_else(|e| panic!("{backend:?}: no event within 500 ms: {e}"));
        let changed = expect_changed(event, backend);
        assert_eq!(changed.config.appearance.font_size, 24.0);

        // A second edit proves the thread survived the first debounce.
        std::fs::write(store.paths().config_file(), font_size_doc(25)).unwrap();
        let event = watcher
            .recv_timeout(EVENTUALLY)
            .unwrap_or_else(|e| panic!("{backend:?}: thread died after the first edit: {e}"));
        assert_eq!(
            expect_changed(event, backend).config.appearance.font_size,
            25.0
        );
    }
}

/// With every option at `Duration::MAX` the (clamped, hour-long) debounce
/// holds the edit back, but the thread stays alive, the channel stays open
/// and dropping the watcher still returns at once.
#[test]
fn huge_debounce_keeps_the_thread_alive_and_stoppable() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = open_watched(WatchOptions {
            backend,
            poll_interval: Duration::MAX,
            debounce: Duration::MAX,
            max_wait: Duration::MAX,
            ..Default::default()
        });
        std::fs::write(store.paths().config_file(), font_size_doc(26)).unwrap();
        assert!(
            matches!(
                watcher.recv_timeout(Duration::from_millis(200)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "{backend:?}: still debouncing, thread alive"
        );
        let started = Instant::now();
        drop(watcher);
        assert!(started.elapsed() < Duration::from_secs(1), "{backend:?}");
    }
}

#[test]
fn dropping_the_watcher_stops_the_thread_promptly() {
    for backend in BACKENDS {
        let (_dir, _store, watcher) = open_watched(WatchOptions {
            backend,
            poll_interval: Duration::from_secs(10),
            debounce: Duration::from_secs(10),
            ..Default::default()
        });
        let started = Instant::now();
        drop(watcher);
        assert!(started.elapsed() < Duration::from_secs(1), "{backend:?}");
    }
}

/// With a 10 s poll interval only kernel events can deliver an edit in time,
/// so this proves the native backend is in use rather than the fallback.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
#[test]
fn native_backend_is_event_driven_not_polled() {
    let (_dir, store, watcher) = open_watched(WatchOptions {
        backend: WatchBackend::Native,
        poll_interval: Duration::from_secs(10),
        debounce: Duration::from_millis(50),
        ..Default::default()
    });
    std::fs::write(store.paths().config_file(), font_size_doc(23)).unwrap();
    let changed = expect_changed(
        watcher.recv_timeout(EVENTUALLY).expect("kernel event"),
        WatchBackend::Native,
    );
    assert_eq!(changed.config.appearance.font_size, 23.0);
}

#[test]
fn continuous_writes_are_applied_within_max_wait() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = open_watched(WatchOptions {
            backend,
            poll_interval: Duration::from_millis(10),
            debounce: Duration::from_millis(100),
            max_wait: Duration::from_millis(300),
            ..Default::default()
        });
        let path = store.paths().config_file();

        // A writer that never goes quiet for `debounce`: atomic writes every
        // 30 ms for a second, so each read sees a complete document.
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let mut size = 30;
                while started.elapsed() < Duration::from_secs(1) {
                    write_atomic(&path, &font_size_doc(size)).unwrap();
                    size += 1;
                    std::thread::sleep(Duration::from_millis(30));
                }
            })
        };

        let started = Instant::now();
        let event = watcher
            .recv_timeout(Duration::from_millis(800))
            .unwrap_or_else(|e| panic!("{backend:?}: no event while writes continue: {e}"));
        let changed = expect_changed(event, backend);
        assert!(
            changed.config.appearance.font_size >= 30.0,
            "{backend:?}: {changed:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "{backend:?}: took {:?}",
            started.elapsed()
        );
        writer.join().unwrap();
    }
}

#[test]
fn every_watcher_on_a_store_receives_every_event() {
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(ConfigPaths::new(dir.path())).unwrap();
        let first = store.watch(options(backend)).unwrap();
        let second = store.watch(options(backend)).unwrap();

        std::fs::write(store.paths().config_file(), font_size_doc(17)).unwrap();
        for (name, watcher) in [("first", &first), ("second", &second)] {
            let event = watcher
                .recv_timeout(EVENTUALLY)
                .unwrap_or_else(|e| panic!("{backend:?}: {name} watcher saw nothing: {e}"));
            let changed = expect_changed(event, backend);
            assert_eq!(
                changed.config.appearance.font_size, 17.0,
                "{backend:?}: {name}"
            );
            assert!(
                watcher.recv_timeout(Duration::from_millis(200)).is_err(),
                "{backend:?}: {name} watcher reported the change twice"
            );
        }

        // Dropping one watcher does not affect the other.
        drop(first);
        std::fs::write(store.paths().config_file(), font_size_doc(18)).unwrap();
        let changed = expect_changed(second.recv_timeout(EVENTUALLY).expect("second"), backend);
        assert_eq!(changed.config.appearance.font_size, 18.0);
    }
}

#[test]
fn invalid_edit_is_reported_once_to_each_of_two_watchers() {
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(ConfigPaths::new(dir.path())).unwrap();
        let first = store.watch(options(backend)).unwrap();
        let second = store.watch(options(backend)).unwrap();

        std::fs::write(store.paths().config_file(), "[appearance\n").unwrap();
        for (name, watcher) in [("first", &first), ("second", &second)] {
            let event = watcher
                .recv_timeout(EVENTUALLY)
                .unwrap_or_else(|e| panic!("{backend:?}: {name} watcher saw nothing: {e}"));
            assert!(
                matches!(event, ConfigEvent::Invalid(_)),
                "{backend:?}: {name}: {event:?}"
            );
            assert!(
                watcher.recv_timeout(Duration::from_millis(300)).is_err(),
                "{backend:?}: {name} watcher reported the error twice"
            );
        }

        // A different error after the first is still news.
        std::fs::write(store.paths().config_file(), "[x\n").unwrap();
        for watcher in [&first, &second] {
            let event = watcher.recv_timeout(EVENTUALLY).expect("second error");
            assert!(matches!(event, ConfigEvent::Invalid(_)), "{event:?}");
        }
    }
}

/// `config.toml` symlinked into a dotfiles checkout whose target is deleted
/// and recreated (`git checkout`, `rm` plus rewrite): the recreate must be
/// observed even though nothing in the configuration directory changes.
#[cfg(unix)]
#[test]
fn symlink_target_deleted_and_recreated_is_followed() {
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        let target = dotfiles.join("tessera.toml");
        std::fs::write(&target, font_size_doc(10)).unwrap();
        let root = dir.path().join("Tessera");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&target, root.join("config.toml")).unwrap();

        let store = ConfigStore::open(ConfigPaths::new(&root)).unwrap();
        assert_eq!(store.config().appearance.font_size, 10.0);
        let watcher = store.watch(options(backend)).unwrap();

        // A dangling link is an error, not a missing file (#262 item 1):
        // the last good configuration stays.
        std::fs::remove_file(&target).unwrap();
        expect_invalid(watcher.recv_timeout(EVENTUALLY).expect("delete"), backend);
        assert_eq!(store.config().appearance.font_size, 10.0, "{backend:?}");

        std::fs::write(&target, font_size_doc(14)).unwrap();
        let changed = expect_changed(
            watcher
                .recv_timeout(EVENTUALLY)
                .unwrap_or_else(|e| panic!("{backend:?}: recreated target not seen: {e}")),
            backend,
        );
        assert_eq!(changed.config.appearance.font_size, 14.0, "{backend:?}");

        // An in-place edit of the recreated target is still seen too.
        std::fs::write(&target, font_size_doc(15)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(EVENTUALLY).expect("edit"), backend);
        assert_eq!(changed.config.appearance.font_size, 15.0, "{backend:?}");

        // The link itself survived all of this.
        assert!(
            std::fs::symlink_metadata(root.join("config.toml"))
                .unwrap()
                .is_symlink()
        );
    }
}

/// The symlink target's whole directory going away and coming back (a
/// dotfiles checkout moved aside and restored) is followed as well.
#[cfg(unix)]
#[test]
fn symlink_target_directory_removed_and_recreated_is_followed() {
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        let target = dotfiles.join("tessera.toml");
        std::fs::write(&target, font_size_doc(10)).unwrap();
        let root = dir.path().join("Tessera");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&target, root.join("config.toml")).unwrap();

        let store = ConfigStore::open(ConfigPaths::new(&root)).unwrap();
        let watcher = store.watch(options(backend)).unwrap();

        std::fs::remove_dir_all(&dotfiles).unwrap();
        expect_invalid(watcher.recv_timeout(EVENTUALLY).expect("removed"), backend);
        assert_eq!(store.config().appearance.font_size, 10.0, "{backend:?}");

        std::fs::create_dir(&dotfiles).unwrap();
        std::fs::write(&target, font_size_doc(16)).unwrap();
        let changed = expect_changed(
            watcher
                .recv_timeout(EVENTUALLY)
                .unwrap_or_else(|e| panic!("{backend:?}: restored checkout not seen: {e}")),
            backend,
        );
        assert_eq!(changed.config.appearance.font_size, 16.0, "{backend:?}");

        std::fs::write(&target, font_size_doc(17)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(EVENTUALLY).expect("after"), backend);
        assert_eq!(changed.config.appearance.font_size, 17.0, "{backend:?}");
    }
}

/// A symlinked `config.toml` whose target is saved by rename (editor
/// writes a temporary file beside it and renames it over the target), then
/// edited in place: the in-place write must reach the new inode's watch.
/// Repeated because the race it guards (the rename landing while the watch
/// re-registers) is narrow.
#[cfg(unix)]
#[test]
fn symlink_target_rename_save_then_in_place_write_is_observed() {
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        let target = dotfiles.join("tessera.toml");
        std::fs::write(&target, font_size_doc(10)).unwrap();
        let root = dir.path().join("Tessera");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&target, root.join("config.toml")).unwrap();

        let store = ConfigStore::open(ConfigPaths::new(&root)).unwrap();
        let watcher = store.watch(options(backend)).unwrap();
        let tmp = dotfiles.join("tessera.toml.tmp");
        for i in 0..20_u16 {
            let saved = 11 + 2 * i;
            std::fs::write(&tmp, font_size_doc(saved.into())).unwrap();
            std::fs::rename(&tmp, &target).unwrap();
            let changed = expect_changed(
                watcher
                    .recv_timeout(EVENTUALLY)
                    .unwrap_or_else(|e| panic!("{backend:?} #{i}: rename-save not seen: {e}")),
                backend,
            );
            assert_eq!(
                changed.config.appearance.font_size,
                f32::from(saved),
                "{backend:?}"
            );

            std::fs::write(&target, font_size_doc((saved + 1).into())).unwrap();
            let changed = expect_changed(
                watcher
                    .recv_timeout(EVENTUALLY)
                    .unwrap_or_else(|e| panic!("{backend:?} #{i}: in-place write not seen: {e}")),
                backend,
            );
            assert_eq!(
                changed.config.appearance.font_size,
                f32::from(saved + 1),
                "{backend:?} #{i}"
            );
        }
    }
}

#[test]
fn removing_and_recreating_the_root_directory_is_followed() {
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path().join("Tessera"));
        let store = ConfigStore::open(paths.clone()).unwrap();
        std::fs::write(paths.config_file(), font_size_doc(25)).unwrap();
        let watcher = store.watch(options(backend)).unwrap();
        let changed = expect_changed(watcher.try_recv().expect("catch-up"), backend);
        assert_eq!(changed.config.appearance.font_size, 25.0);

        std::fs::remove_dir_all(paths.root()).unwrap();
        let changed = expect_changed(watcher.recv_timeout(EVENTUALLY).expect("removed"), backend);
        assert_eq!(*changed.config, Config::default());

        std::fs::create_dir_all(paths.root()).unwrap();
        std::fs::write(paths.config_file(), font_size_doc(30)).unwrap();
        let changed = expect_changed(
            watcher
                .recv_timeout(EVENTUALLY)
                .unwrap_or_else(|e| panic!("{backend:?}: recreated: {e}")),
            backend,
        );
        assert_eq!(changed.config.appearance.font_size, 30.0);

        // And kernel events are back in use afterwards: a further write is
        // still seen.
        std::fs::write(paths.config_file(), font_size_doc(31)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(EVENTUALLY).expect("after"), backend);
        assert_eq!(changed.config.appearance.font_size, 31.0);
    }
}

/// A `config.toml` that becomes unreadable and then readable again is
/// followed by both backends: the native one cannot register an unreadable
/// file, so it polls until it can, re-reads it, and sees later in-place
/// writes (which produce no directory event) through its restored watch.
#[cfg(unix)]
#[test]
fn unreadable_then_restored_file_is_followed() {
    use std::os::unix::fs::PermissionsExt as _;
    if unsafe_root() {
        return;
    }
    for backend in BACKENDS {
        let (_dir, store, watcher) = open_watched(options(backend));
        let path = store.paths().config_file();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        match watcher.recv_timeout(EVENTUALLY) {
            Ok(ConfigEvent::Invalid(_)) => {}
            other => panic!("{backend:?}: expected Invalid, got {other:?}"),
        }
        assert!(store.last_error().is_some(), "{backend:?}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        // Readable again: the stale error is cleared by a Changed event even
        // though the content is the same.
        let changed = expect_changed(
            watcher
                .recv_timeout(EVENTUALLY)
                .unwrap_or_else(|e| panic!("{backend:?}: recovery not seen: {e}")),
            backend,
        );
        assert_eq!(*changed.config, Config::default(), "{backend:?}");
        assert!(store.last_error().is_none(), "{backend:?}");

        // An in-place rewrite afterwards is seen as before.
        std::fs::write(&path, font_size_doc(45)).unwrap();
        let changed = expect_changed(
            watcher
                .recv_timeout(EVENTUALLY)
                .unwrap_or_else(|e| panic!("{backend:?}: edit after recovery not seen: {e}")),
            backend,
        );
        assert_eq!(changed.config.appearance.font_size, 45.0, "{backend:?}");
        assert_eq!(store.config().appearance.font_size, 45.0, "{backend:?}");
    }
}

/// Starting a watcher over a file that cannot be opened (unreadable, or a
/// symlink cycle) succeeds with either backend, as `ConfigStore::open`
/// does, and the file is read once it can be.
#[cfg(unix)]
#[test]
fn watch_starts_over_an_unopenable_file_and_recovers() {
    use std::os::unix::fs::PermissionsExt as _;
    if unsafe_root() {
        return;
    }
    for backend in BACKENDS {
        // Unreadable.
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        let path = paths.config_file();
        std::fs::write(&path, font_size_doc(21)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let store = ConfigStore::open(paths).unwrap();
        assert!(store.last_error().is_some(), "{backend:?}");
        let watcher = store
            .watch(options(backend))
            .unwrap_or_else(|e| panic!("{backend:?}: watch failed: {e}"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let changed = expect_changed(
            watcher
                .recv_timeout(EVENTUALLY)
                .unwrap_or_else(|e| panic!("{backend:?}: recovery not seen: {e}")),
            backend,
        );
        assert_eq!(changed.config.appearance.font_size, 21.0, "{backend:?}");
        drop(watcher);

        // A symlink cycle where config.toml should be.
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        let path = paths.config_file();
        let other = dir.path().join("other.toml");
        std::os::unix::fs::symlink(&other, &path).unwrap();
        std::os::unix::fs::symlink(&path, &other).unwrap();
        let store = ConfigStore::open(paths).unwrap();
        let watcher = store
            .watch(options(backend))
            .unwrap_or_else(|e| panic!("{backend:?}: watch failed on a cycle: {e}"));
        std::fs::remove_file(&other).unwrap();
        std::fs::write(&other, font_size_doc(22)).unwrap();
        let changed = expect_changed(
            watcher
                .recv_timeout(EVENTUALLY)
                .unwrap_or_else(|e| panic!("{backend:?}: resolved cycle not seen: {e}")),
            backend,
        );
        assert_eq!(changed.config.appearance.font_size, 22.0, "{backend:?}");
    }
}

/// #262 item 9, the adversarial e2e case `inplace:150,10,300,0`: launched
/// with a `config.toml` that does not load (so clipboard writes are
/// refused), the same broken file is saved in place ten times by an editor
/// that truncates it, leaves it empty long enough for the watcher to read
/// the empty file, and then writes it. Writes stay refused throughout,
/// sampled continuously, on both backends.
///
/// Each round waits for the watcher to read the empty file (the read
/// clears the recorded error, so it is reported as `Changed`) and then the
/// broken one (`Invalid`, news again). A fixed 150 ms empty window is not
/// enough: the polling backend reads a change one tick plus a debounce
/// after it, 100 to 200 ms, so on a loaded machine it can miss every
/// window, see only the broken file, which it rightly does not report, and
/// leave the empty file untested (#296).
#[test]
fn in_place_saves_from_a_broken_launch_never_turn_clipboard_writes_on() {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const BROKEN: &[u8] =
        b"# my config\n[appearance]\nfont_size = 14\n[terminal]\nclipboard_write = False\n";
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        std::fs::write(paths.config_file(), BROKEN).unwrap();
        // As `App::with_config_dir` opens it.
        let store = ConfigStore::in_memory(paths.clone(), Config::default());
        assert!(store.reload().is_err(), "{backend:?}");
        assert!(!store.config().terminal.clipboard_write, "{backend:?}");
        let watcher = store.watch(options(backend)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let on = Arc::new(AtomicUsize::new(0));
        let sampler = {
            let (store, stop, on) = (store.clone(), Arc::clone(&stop), Arc::clone(&on));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if store.config().terminal.clipboard_write {
                        on.fetch_add(1, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };
        for round in 0..10 {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(paths.config_file())
                .unwrap();
            let empty = expect_changed(
                watcher
                    .recv_timeout(EVENTUALLY)
                    .unwrap_or_else(|e| panic!("{backend:?} {round}: empty file unread: {e}")),
                backend,
            );
            assert!(
                !empty.config.terminal.clipboard_write,
                "{backend:?} {round}: the empty file turned writes on"
            );
            file.write_all(BROKEN).unwrap();
            drop(file);
            match watcher.recv_timeout(EVENTUALLY) {
                Ok(ConfigEvent::Invalid(_)) => {}
                other => panic!("{backend:?} {round}: expected Invalid, got {other:?}"),
            }
        }
        stop.store(true, Ordering::Relaxed);
        sampler.join().unwrap();
        assert_eq!(
            on.load(Ordering::Relaxed),
            0,
            "{backend:?}: writes turned on"
        );
        assert!(!store.config().terminal.clipboard_write, "{backend:?}");
    }
}

/// Permission bits do not restrict root, so the permission tests are
/// meaningless (and would fail) when run as root.
#[cfg(unix)]
fn unsafe_root() -> bool {
    std::env::var_os("USER").is_some_and(|u| u == "root")
}

/// A configuration in `dir/config.toml` with `font_size` and
/// `clipboard_write`.
#[cfg(unix)]
fn write_config(dir: &std::path::Path, font_size: u32, clipboard_write: bool) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[appearance]\nfont_size = {font_size}\n[terminal]\nclipboard_write = {clipboard_write}\n"
        ),
    )
    .unwrap();
}

/// Points the symlink `link` at `target` atomically (a new link renamed
/// over it, as `ln -sfn` does on GNU and home-manager does on a switch).
#[cfg(unix)]
fn retarget(link: &std::path::Path, target: &std::path::Path) {
    let staged = link.with_extension("newlink");
    std::os::unix::fs::symlink(target, &staged).unwrap();
    std::fs::rename(&staged, link).unwrap();
}

/// Waits for a `Changed` event whose configuration has `font_size`;
/// panics after [`EVENTUALLY`]. Other events on the way are skipped.
#[cfg(unix)]
fn expect_font_size(watcher: &Watcher, font_size: f32, what: &str) -> Config {
    let deadline = Instant::now() + EVENTUALLY;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match watcher.recv_timeout(remaining) {
            Ok(ConfigEvent::Changed(changed))
                if (changed.config.appearance.font_size - font_size).abs() < f32::EPSILON =>
            {
                return (*changed.config).clone();
            }
            Ok(_) => {}
            Err(e) => panic!("{what}: font_size {font_size} not applied: {e}"),
        }
    }
}

/// #384: the configuration directory is a symlink (GNU stow, a dotfiles
/// generation) retargeted while watched. The new target's `config.toml`
/// applies, including `clipboard_write = false`, and so does a later
/// in-place edit of it, on both backends. Kernel watches follow inodes, so
/// the native backend sees the retarget only through the directory holding
/// the link.
#[cfg(unix)]
#[test]
fn retargeting_a_symlinked_configuration_directory_is_followed() {
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let (gen1, gen2) = (dir.path().join("gen1"), dir.path().join("gen2"));
        write_config(&gen1.join("tessera"), 10, true);
        write_config(&gen2.join("tessera"), 20, false);
        let root = dir.path().join("Tessera");
        std::os::unix::fs::symlink(gen1.join("tessera"), &root).unwrap();
        let store = ConfigStore::open(ConfigPaths::new(&root)).unwrap();
        assert!(store.config().terminal.clipboard_write, "{backend:?}");
        let watcher = store.watch(options(backend)).unwrap();

        retarget(&root, &gen2.join("tessera"));
        let config = expect_font_size(&watcher, 20.0, &format!("{backend:?} retarget"));
        assert!(!config.terminal.clipboard_write, "{backend:?}: writes off");
        assert!(!store.config().terminal.clipboard_write, "{backend:?}");

        write_config(&gen2.join("tessera"), 21, false);
        expect_font_size(
            &watcher,
            21.0,
            &format!("{backend:?} edit of the new target"),
        );

        // Back, the way BSD `ln -sfn` does it: removed and made again.
        std::fs::remove_file(&root).unwrap();
        std::os::unix::fs::symlink(gen1.join("tessera"), &root).unwrap();
        let config = expect_font_size(&watcher, 10.0, &format!("{backend:?} back"));
        assert!(config.terminal.clipboard_write, "{backend:?}");
    }
}

/// #384: a link above the configuration directory (a dotfiles checkout
/// switched with `ln -sfn`) retargeted while watched is followed too.
#[cfg(unix)]
#[test]
fn retargeting_a_link_above_the_configuration_directory_is_followed() {
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let (gen1, gen2) = (dir.path().join("gen1"), dir.path().join("gen2"));
        write_config(&gen1.join("tessera"), 10, true);
        write_config(&gen2.join("tessera"), 20, false);
        let dotfiles = dir.path().join("dotfiles");
        std::os::unix::fs::symlink(&gen1, &dotfiles).unwrap();
        let store = ConfigStore::open(ConfigPaths::new(dotfiles.join("tessera"))).unwrap();
        let watcher = store.watch(options(backend)).unwrap();

        std::fs::remove_file(&dotfiles).unwrap();
        std::os::unix::fs::symlink(&gen2, &dotfiles).unwrap();
        let config = expect_font_size(&watcher, 20.0, &format!("{backend:?} retarget"));
        assert!(!config.terminal.clipboard_write, "{backend:?}: writes off");

        write_config(&gen2.join("tessera"), 21, false);
        expect_font_size(
            &watcher,
            21.0,
            &format!("{backend:?} edit of the new target"),
        );
    }
}

/// #384, S5: past the cap on links followed (here nine in a chain), the
/// native backend watches none of them and polls `config.toml` instead, so
/// a retarget of the last link still applies, and so does a later edit.
#[cfg(unix)]
#[test]
fn a_retarget_past_the_link_cap_is_applied_by_polling() {
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let (gen1, gen2) = (dir.path().join("gen1"), dir.path().join("gen2"));
        write_config(&gen1.join("tessera"), 10, true);
        write_config(&gen2.join("tessera"), 20, false);
        // l0 -> l1 -> ... -> l8 -> gen1/tessera: nine links.
        let last = dir.path().join("l8");
        std::os::unix::fs::symlink(gen1.join("tessera"), &last).unwrap();
        for i in (0..8).rev() {
            std::os::unix::fs::symlink(
                dir.path().join(format!("l{}", i + 1)),
                dir.path().join(format!("l{i}")),
            )
            .unwrap();
        }
        let store = ConfigStore::open(ConfigPaths::new(dir.path().join("l0"))).unwrap();
        assert_eq!(store.config().appearance.font_size, 10.0, "{backend:?}");
        let watcher = store.watch(options(backend)).unwrap();

        retarget(&last, &gen2.join("tessera"));
        let config = expect_font_size(&watcher, 20.0, &format!("{backend:?} retarget"));
        assert!(!config.terminal.clipboard_write, "{backend:?}: writes off");

        write_config(&gen2.join("tessera"), 21, false);
        expect_font_size(
            &watcher,
            21.0,
            &format!("{backend:?} edit of the new target"),
        );
    }
}
