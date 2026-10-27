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

/// Bound for a step the native backend recovers through its poll fallback
/// (a removed directory coming back, a file becoming readable again). The
/// fallback samples every `poll_interval` and reloads straight after a
/// successful re-arm, or within one debounce after a directory event it
/// sees on the way; `max_wait` only matters for a continuous writer. That is
/// at most `poll_interval + debounce`, times four for scheduling stalls on
/// loaded CI runners (600 ms with the defaults). The poll backend has no
/// separate recovery path, so it keeps [`LIMIT`]. `recv_timeout` returns as
/// soon as the event arrives, so the headroom costs nothing when the
/// backend is on time.
fn recovery_limit(backend: WatchBackend) -> Duration {
    if backend == WatchBackend::Poll {
        return LIMIT;
    }
    let defaults = WatchOptions::default();
    (defaults.poll_interval + defaults.debounce) * 4
}

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

#[test]
fn rapid_writes_are_debounced_into_one_event() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = open_watched(WatchOptions {
            backend,
            poll_interval: Duration::from_millis(10),
            debounce: Duration::from_millis(150),
            ..Default::default()
        });
        let path = store.paths().config_file();

        for size in 14..=20 {
            std::fs::write(&path, font_size_doc(size)).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }

        let event = watcher.recv_timeout(LIMIT).expect("one event");
        let changed = expect_changed(event, backend);
        assert_eq!(
            changed.config.appearance.font_size, 20.0,
            "{backend:?}: only the final value is applied"
        );
        assert!(
            watcher.recv_timeout(Duration::from_millis(300)).is_err(),
            "{backend:?}: intermediate writes must not produce extra events"
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
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("rename"), backend);
        assert_eq!(changed.config.appearance.font_size, 18.0);

        // An in-place write to the renamed-in file is still seen.
        std::fs::write(&path, font_size_doc(19)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("write"), backend);
        assert_eq!(changed.config.appearance.font_size, 19.0);

        // Deleting the file applies the defaults; recreating it is seen too.
        std::fs::remove_file(&path).unwrap();
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("delete"), backend);
        assert_eq!(*changed.config, Config::default());
        std::fs::write(&path, font_size_doc(21)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("recreate"), backend);
        assert_eq!(changed.config.appearance.font_size, 21.0);
    }
}

#[test]
fn invalid_edit_reports_error_and_keeps_last_good_config() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = open_watched(options(backend));
        let path = store.paths().config_file();

        std::fs::write(&path, "[appearance\n").unwrap();
        let event = watcher.recv_timeout(LIMIT).expect("event");
        assert!(
            matches!(event, ConfigEvent::Invalid(_)),
            "{backend:?}: got {event:?}"
        );
        assert_eq!(*store.config(), Config::default());
        assert!(store.last_error().is_some());

        std::fs::write(&path, font_size_doc(15)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("event"), backend);
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
        let event = watcher.recv_timeout(LIMIT).expect("event");
        assert!(matches!(event, ConfigEvent::Invalid(_)), "{event:?}");

        // Repair to a document equal to what is applied (the defaults).
        std::fs::write(&path, "").unwrap();
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("recovery"), backend);
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
        watcher.recv_timeout(LIMIT).expect("event"),
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
        });
        std::fs::write(store.paths().config_file(), font_size_doc(24)).unwrap();
        let event = watcher
            .recv_timeout(LIMIT)
            .unwrap_or_else(|e| panic!("{backend:?}: no event within 500 ms: {e}"));
        let changed = expect_changed(event, backend);
        assert_eq!(changed.config.appearance.font_size, 24.0);

        // A second edit proves the thread survived the first debounce.
        std::fs::write(store.paths().config_file(), font_size_doc(25)).unwrap();
        let event = watcher
            .recv_timeout(LIMIT)
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
        watcher.recv_timeout(LIMIT).expect("kernel event"),
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
                .recv_timeout(LIMIT)
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
        let changed = expect_changed(second.recv_timeout(LIMIT).expect("second"), backend);
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
                .recv_timeout(LIMIT)
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
            let event = watcher.recv_timeout(LIMIT).expect("second error");
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

        std::fs::remove_file(&target).unwrap();
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("delete"), backend);
        assert_eq!(*changed.config, Config::default(), "{backend:?}");

        std::fs::write(&target, font_size_doc(14)).unwrap();
        let changed = expect_changed(
            watcher
                .recv_timeout(LIMIT)
                .unwrap_or_else(|e| panic!("{backend:?}: recreated target not seen: {e}")),
            backend,
        );
        assert_eq!(changed.config.appearance.font_size, 14.0, "{backend:?}");

        // An in-place edit of the recreated target is still seen too.
        std::fs::write(&target, font_size_doc(15)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("edit"), backend);
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
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("removed"), backend);
        assert_eq!(*changed.config, Config::default(), "{backend:?}");

        std::fs::create_dir(&dotfiles).unwrap();
        std::fs::write(&target, font_size_doc(16)).unwrap();
        let changed = expect_changed(
            watcher
                .recv_timeout(recovery_limit(backend))
                .unwrap_or_else(|e| panic!("{backend:?}: restored checkout not seen: {e}")),
            backend,
        );
        assert_eq!(changed.config.appearance.font_size, 16.0, "{backend:?}");

        std::fs::write(&target, font_size_doc(17)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("after"), backend);
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
                    .recv_timeout(LIMIT)
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
                    .recv_timeout(LIMIT)
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
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("removed"), backend);
        assert_eq!(*changed.config, Config::default());

        std::fs::create_dir_all(paths.root()).unwrap();
        std::fs::write(paths.config_file(), font_size_doc(30)).unwrap();
        let changed = expect_changed(
            watcher
                .recv_timeout(recovery_limit(backend))
                .unwrap_or_else(|e| panic!("{backend:?}: recreated: {e}")),
            backend,
        );
        assert_eq!(changed.config.appearance.font_size, 30.0);

        // And kernel events are back in use afterwards: a further write is
        // still seen.
        std::fs::write(paths.config_file(), font_size_doc(31)).unwrap();
        let changed = expect_changed(watcher.recv_timeout(LIMIT).expect("after"), backend);
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
        match watcher.recv_timeout(LIMIT) {
            Ok(ConfigEvent::Invalid(_)) => {}
            other => panic!("{backend:?}: expected Invalid, got {other:?}"),
        }
        assert!(store.last_error().is_some(), "{backend:?}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        // Readable again: the stale error is cleared by a Changed event even
        // though the content is the same.
        let changed = expect_changed(
            watcher
                .recv_timeout(recovery_limit(backend))
                .unwrap_or_else(|e| panic!("{backend:?}: recovery not seen: {e}")),
            backend,
        );
        assert_eq!(*changed.config, Config::default(), "{backend:?}");
        assert!(store.last_error().is_none(), "{backend:?}");

        // An in-place rewrite afterwards is seen as before.
        std::fs::write(&path, font_size_doc(45)).unwrap();
        let changed = expect_changed(
            watcher
                .recv_timeout(LIMIT)
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
                .recv_timeout(recovery_limit(backend))
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
                .recv_timeout(recovery_limit(backend))
                .unwrap_or_else(|e| panic!("{backend:?}: resolved cycle not seen: {e}")),
            backend,
        );
        assert_eq!(changed.config.appearance.font_size, 22.0, "{backend:?}");
    }
}

/// Permission bits do not restrict root, so the permission tests are
/// meaningless (and would fail) when run as root.
#[cfg(unix)]
fn unsafe_root() -> bool {
    std::env::var_os("USER").is_some_and(|u| u == "root")
}
