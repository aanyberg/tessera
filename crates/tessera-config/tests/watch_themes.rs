//! Watching `themes/`: an edit of the theme in use is reported as
//! `ThemesChanged` within a debounce plus 500 ms, by both backends; an edit
//! of an unrelated theme file produces at most one event and never a
//! `Changed`; the native backend follows the theme file when
//! `appearance.theme` names another one.

use std::path::Path;
use std::time::{Duration, Instant};

use tessera_config::{
    ConfigEvent, ConfigPaths, ConfigStore, Theme, WatchBackend, WatchOptions, Watcher,
};

const BACKENDS: [WatchBackend; 2] = [WatchBackend::Native, WatchBackend::Poll];

/// The bound the issue sets: one debounce plus 500 ms.
fn limit() -> Duration {
    WatchOptions::default().debounce + Duration::from_millis(500)
}

/// How long a test waits for an event that is due when it does not assert
/// the issue's bound ([`limit`], asserted only by
/// `editing_the_theme_in_use_is_reported_once_within_the_bound`): long
/// enough for a starved process (`taskpolicy -b` on a loaded machine; #253
/// item 9). `recv_timeout` returns as soon as the event arrives.
const EVENTUALLY: Duration = Duration::from_secs(5);

/// How long to wait to be sure no (further) event comes: the polling
/// interval, the debounce and generous scheduling slack.
const QUIET: Duration = Duration::from_millis(600);

/// A valid theme file named `name` with background `#rrggbb`.
fn theme_doc(name: &str, background: &str) -> String {
    let mut theme = Theme::bundled("Dracula").unwrap().clone();
    theme.name = name.into();
    theme.background = background.parse().unwrap();
    theme.to_toml()
}

fn config_doc(theme: &str) -> String {
    format!("[appearance]\ntheme = \"{theme}\"\n")
}

/// A store whose `config.toml` selects `theme`, with `themes/mine.toml`
/// and `themes/other.toml` present, watched by `backend`.
fn setup(backend: WatchBackend, theme: &str) -> (tempfile::TempDir, ConfigStore, Watcher) {
    let dir = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::new(dir.path());
    std::fs::create_dir_all(paths.themes_dir()).unwrap();
    std::fs::write(paths.config_file(), config_doc(theme)).unwrap();
    std::fs::write(
        paths.themes_dir().join("mine.toml"),
        theme_doc("Mine", "#101010"),
    )
    .unwrap();
    std::fs::write(
        paths.themes_dir().join("other.toml"),
        theme_doc("Other", "#202020"),
    )
    .unwrap();
    let store = ConfigStore::open(paths).unwrap();
    let watcher = store
        .watch(WatchOptions {
            backend,
            ..Default::default()
        })
        .unwrap();
    (dir, store, watcher)
}

/// Every event that arrives within `window`.
fn drain(watcher: &Watcher, window: Duration) -> Vec<ConfigEvent> {
    let deadline = Instant::now() + window;
    let mut events = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match watcher.recv_timeout(left) {
            Ok(event) => events.push(event),
            Err(_) => return events,
        }
    }
}

fn expect_themes_changed(watcher: &Watcher, backend: WatchBackend, what: &str) {
    expect_themes_changed_within(watcher, backend, what, EVENTUALLY);
}

fn expect_themes_changed_within(
    watcher: &Watcher,
    backend: WatchBackend,
    what: &str,
    bound: Duration,
) {
    let started = Instant::now();
    let event = watcher
        .recv_timeout(bound)
        .unwrap_or_else(|e| panic!("{backend:?}: no event for {what} within {bound:?}: {e}"));
    assert!(
        matches!(event, ConfigEvent::ThemesChanged),
        "{backend:?}: {what}: expected ThemesChanged, got {event:?}"
    );
    assert!(
        started.elapsed() < bound,
        "{backend:?}: {what}: took {:?}",
        started.elapsed()
    );
}

fn background(store: &ConfigStore) -> String {
    store.theme().background.to_string()
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

#[test]
fn editing_the_theme_in_use_is_reported_once_within_the_bound() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        assert_eq!(background(&store), "#101010");
        let mine = store.paths().themes_dir().join("mine.toml");

        // In place: no directory event; the native backend must watch the
        // file itself.
        write(&mine, &theme_doc("Mine", "#111111"));
        expect_themes_changed_within(&watcher, backend, "in-place write", limit());
        assert_eq!(background(&store), "#111111", "{backend:?}");
        let rest = drain(&watcher, QUIET);
        assert!(
            rest.is_empty(),
            "{backend:?}: one event per edit, then {rest:?}"
        );

        // A rename-save replaces the inode; a later in-place write to the
        // new file is still seen.
        let tmp = mine.with_extension("toml.tmp");
        write(&tmp, &theme_doc("Mine", "#121212"));
        std::fs::rename(&tmp, &mine).unwrap();
        expect_themes_changed_within(&watcher, backend, "rename-save", limit());
        assert_eq!(background(&store), "#121212", "{backend:?}");
        assert!(drain(&watcher, QUIET).is_empty(), "{backend:?}");
        write(&mine, &theme_doc("Mine", "#131313"));
        expect_themes_changed_within(&watcher, backend, "write after rename-save", limit());
        assert_eq!(background(&store), "#131313", "{backend:?}");
    }
}

#[test]
fn editing_an_unrelated_theme_file_is_at_most_one_event_and_no_changed() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        let themes = store.paths().themes_dir();

        write(&themes.join("other.toml"), &theme_doc("Other", "#212121"));
        // Pinned per backend: the native one does not watch a theme file
        // that is not in use; the polling one sees it at its next full
        // listing, every twenty intervals, then debounces. The wait allows
        // four times that for a throttled machine, and at least
        // [`EVENTUALLY`] (#262 N2); it ends at the event.
        let events = if backend == WatchBackend::Poll {
            let defaults = WatchOptions::default();
            let bound = (defaults.poll_interval * 20 * 4 + defaults.debounce).max(EVENTUALLY);
            let first = watcher
                .recv_timeout(bound)
                .unwrap_or_else(|e| panic!("Poll: unused-file write unseen within {bound:?}: {e}"));
            let mut events = vec![first];
            events.extend(drain(&watcher, QUIET));
            events
        } else {
            drain(&watcher, limit() + QUIET)
        };
        let expected = usize::from(backend == WatchBackend::Poll);
        assert_eq!(
            events.len(),
            expected,
            "{backend:?}: in-place write: {events:?}"
        );
        assert!(
            events
                .iter()
                .all(|e| matches!(e, ConfigEvent::ThemesChanged)),
            "{backend:?}: in-place write: {events:?}"
        );

        // Creating and deleting files the theme in use does not depend on.
        write(&themes.join("new.toml"), &theme_doc("New", "#303030"));
        let events = drain(&watcher, limit() + QUIET);
        assert_eq!(events.len(), 1, "{backend:?}: created: {events:?}");
        assert!(
            matches!(events[0], ConfigEvent::ThemesChanged),
            "{events:?}"
        );
        std::fs::remove_file(themes.join("new.toml")).unwrap();
        let events = drain(&watcher, limit() + QUIET);
        assert_eq!(events.len(), 1, "{backend:?}: deleted: {events:?}");
        assert!(
            matches!(events[0], ConfigEvent::ThemesChanged),
            "{events:?}"
        );
        assert_eq!(background(&store), "#101010", "{backend:?}: still Mine");
    }
}

#[test]
fn files_the_theme_lookup_ignores_are_not_reported() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        let themes = store.paths().themes_dir();
        // Hidden files and non-TOML files are never themes. The native
        // backend still wakes for them (a directory event) but finds
        // nothing to report.
        write(
            &themes.join(".hidden.toml"),
            &theme_doc("Hidden", "#404040"),
        );
        write(&themes.join("notes.txt"), "not a theme");
        let events = drain(&watcher, limit() + QUIET);
        assert!(events.is_empty(), "{backend:?}: {events:?}");
    }
}

#[test]
fn an_edit_of_config_toml_alone_is_no_themes_event() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        write(
            &store.paths().config_file(),
            "[appearance]\ntheme = \"mine\"\nfont_size = 17\n",
        );
        let events = drain(&watcher, limit() + QUIET);
        assert_eq!(events.len(), 1, "{backend:?}: {events:?}");
        assert!(matches!(events[0], ConfigEvent::Changed(_)), "{events:?}");
    }
}

/// The native backend registers the theme file `appearance.theme` resolves
/// to; after the configuration switches to another theme it must follow
/// the new file, or an in-place write to it goes unseen.
#[test]
fn the_watched_theme_file_follows_appearance_theme() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        write(&store.paths().config_file(), &config_doc("other"));
        let event = watcher.recv_timeout(EVENTUALLY).expect("config change");
        assert!(matches!(event, ConfigEvent::Changed(_)), "{event:?}");
        assert_eq!(background(&store), "#202020");
        assert!(drain(&watcher, QUIET).is_empty(), "{backend:?}");

        // Several in-place writes at delays that fall at different points
        // of the polling interval: each is seen through the newly followed
        // file within the bound, not by the slower full listing.
        for (n, delay_ms) in [0_u64, 130, 370].into_iter().enumerate() {
            std::thread::sleep(Duration::from_millis(delay_ms));
            let colour = format!("#2{n}2{n}2{n}");
            write(
                &store.paths().themes_dir().join("other.toml"),
                &theme_doc("Other", &colour),
            );
            expect_themes_changed(
                &watcher,
                backend,
                &format!("write {n} to the newly selected theme"),
            );
            assert_eq!(background(&store), colour, "{backend:?}");
            assert!(drain(&watcher, QUIET).is_empty(), "{backend:?}");
        }
    }
}

/// A theme that is not in `themes/` yet (a bundled one, or a name nothing
/// matches) becomes a user file when one is created for it.
#[test]
fn creating_the_file_of_the_theme_in_use_is_reported() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "Dracula");
        assert_eq!(background(&store), "#282a36");
        write(
            &store.paths().themes_dir().join("dracula.toml"),
            &theme_doc("Dracula", "#000001"),
        );
        expect_themes_changed(&watcher, backend, "user file shadowing a bundled theme");
        assert_eq!(background(&store), "#000001");
        // And that file is now followed for in-place writes.
        assert!(drain(&watcher, QUIET).is_empty(), "{backend:?}");
        write(
            &store.paths().themes_dir().join("dracula.toml"),
            &theme_doc("Dracula", "#000002"),
        );
        expect_themes_changed(&watcher, backend, "write to the new user file");
    }
}

#[test]
fn a_removed_and_recreated_themes_directory_is_followed() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        let themes = store.paths().themes_dir();
        std::fs::remove_dir_all(&themes).unwrap();
        expect_themes_changed(&watcher, backend, "directory removed");
        assert_eq!(background(&store), "#000000", "{backend:?}: default theme");
        std::fs::create_dir(&themes).unwrap();
        write(&themes.join("mine.toml"), &theme_doc("Mine", "#141414"));
        expect_themes_changed(&watcher, backend, "directory recreated");
        assert!(drain(&watcher, QUIET).is_empty(), "{backend:?}");
        write(&themes.join("mine.toml"), &theme_doc("Mine", "#151515"));
        expect_themes_changed(&watcher, backend, "write in the recreated directory");
        assert_eq!(background(&store), "#151515", "{backend:?}");
    }
}

#[test]
fn every_watcher_of_a_store_sees_a_themes_change_once() {
    for backend in BACKENDS {
        let (_dir, store, first) = setup(backend, "mine");
        let second = store
            .watch(WatchOptions {
                backend,
                ..Default::default()
            })
            .unwrap();
        write(
            &store.paths().themes_dir().join("mine.toml"),
            &theme_doc("Mine", "#161616"),
        );
        for (n, watcher) in [&first, &second].into_iter().enumerate() {
            let events = drain(watcher, limit() + QUIET);
            assert_eq!(events.len(), 1, "{backend:?}: watcher {n}: {events:?}");
            assert!(
                matches!(events[0], ConfigEvent::ThemesChanged),
                "{events:?}"
            );
        }
    }
}

/// An edit of the theme in use right after the watcher starts, while its
/// thread is still setting up, is seen. (It caught the native backend
/// re-registering the theme file at thread start when the lookup named a
/// *different* file; re-registering the same file is an equivalent change
/// here, since the settle samples the directory afterwards.) Repeated, as
/// the window is a few microseconds wide.
#[test]
fn an_edit_right_after_the_watch_starts_is_seen() {
    for backend in BACKENDS {
        for round in 0..15 {
            let (_dir, store, watcher) = setup(backend, "mine");
            write(
                &store.paths().themes_dir().join("mine.toml"),
                &theme_doc("Mine", "#171717"),
            );
            let event = watcher.recv_timeout(EVENTUALLY).unwrap_or_else(|e| {
                panic!("{backend:?}: round {round}: no event within {EVENTUALLY:?}: {e}")
            });
            assert!(
                matches!(event, ConfigEvent::ThemesChanged),
                "{backend:?}: round {round}: {event:?}"
            );
        }
    }
}

/// A save of the theme in use that does not parse (an editor's partial
/// write, a typo) is reported, and the in-place save that fixes it is seen
/// too: the native backend keeps watching the file although the lookup no
/// longer resolves to it. Both for a file found by its stem and one found
/// by its `name` key.
#[test]
fn a_broken_theme_file_stays_watched_until_it_is_fixed() {
    for backend in BACKENDS {
        for (theme, file) in [("mine", "mine.toml"), ("Keyed Theme", "keyed.toml")] {
            let (_dir, store, watcher) = setup(backend, theme);
            let path = store.paths().themes_dir().join(file);
            if file == "keyed.toml" {
                // The configuration names it already; found by `name` key
                // once it exists.
                write(&path, &theme_doc("Keyed Theme", "#181818"));
                expect_themes_changed(&watcher, backend, "keyed theme created");
                drain(&watcher, QUIET);
                assert_eq!(background(&store), "#181818");
            }
            write(&path, "name = \"half a theme\"\nforeground = ");
            expect_themes_changed(&watcher, backend, &format!("{file}: broken save"));
            assert!(drain(&watcher, QUIET).is_empty(), "{backend:?}: {file}");
            write(
                &path,
                &theme_doc(
                    if file == "mine.toml" {
                        "Mine"
                    } else {
                        "Keyed Theme"
                    },
                    "#191919",
                ),
            );
            expect_themes_changed(&watcher, backend, &format!("{file}: in-place fix"));
            assert_eq!(background(&store), "#191919", "{backend:?}: {file}");
        }
    }
}

/// A theme file that is already broken when the watch starts (or when the
/// configuration first names it) is followed by its stem, so the in-place
/// save that fixes it is seen.
#[test]
fn a_theme_file_broken_from_the_start_is_followed_by_its_stem() {
    for backend in BACKENDS {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(dir.path());
        std::fs::create_dir_all(paths.themes_dir()).unwrap();
        let mine = paths.themes_dir().join("mine.toml");
        write(&mine, "name = \"Mine\"\nforeground = ");
        std::fs::write(paths.config_file(), config_doc("mine")).unwrap();
        let store = ConfigStore::open(paths).unwrap();
        let watcher = store
            .watch(WatchOptions {
                backend,
                ..Default::default()
            })
            .unwrap();
        assert!(drain(&watcher, QUIET).is_empty(), "{backend:?}");
        write(&mine, &theme_doc("Mine", "#1e1e1e"));
        expect_themes_changed(
            &watcher,
            backend,
            "in-place fix of a file broken from the start",
        );
        assert_eq!(background(&store), "#1e1e1e", "{backend:?}");
    }
}

/// Waits up to [`EVENTUALLY`] for a `ThemesChanged` after which the store
/// resolves background `background`.
fn expect_theme_applied(watcher: &Watcher, store: &ConfigStore, background_hex: &str, what: &str) {
    let started = Instant::now();
    loop {
        let left = EVENTUALLY.saturating_sub(started.elapsed());
        match watcher.recv_timeout(left) {
            Ok(ConfigEvent::ThemesChanged) if background(store) == background_hex => return,
            Ok(_) => {}
            Err(error) => panic!(
                "{what}: {background_hex} not applied within {EVENTUALLY:?} (now {}): {error}",
                background(store)
            ),
        }
    }
}

/// Waits for the `Changed` of switching `config.toml` to `theme`, then
/// for the watcher to go quiet.
fn switch_theme(store: &ConfigStore, watcher: &Watcher, theme: &str, what: &str) {
    tessera_config::write_atomic(&store.paths().config_file(), &config_doc(theme)).unwrap();
    let started = Instant::now();
    loop {
        let left = EVENTUALLY.saturating_sub(started.elapsed());
        match watcher.recv_timeout(left) {
            Ok(ConfigEvent::Changed(_)) => break,
            Ok(_) => {}
            Err(error) => panic!("{what}: no Changed for the switch: {error}"),
        }
    }
    drain(watcher, QUIET);
}

// #253 item 8: a name-keyed theme (`keyed.toml`, name "Keyed Theme") whose
// file could not be loaded when the watcher last settled is still seen when
// an in-place write completes it, which raises no event on the directory.
// Before the fix the native backend lost each of these for good.

/// Created empty after the switch (an editor's "New File"), then filled.
#[test]
fn a_keyed_theme_created_empty_then_filled_is_applied() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        let keyed = store.paths().themes_dir().join("keyed.toml");
        switch_theme(&store, &watcher, "Keyed Theme", "create-empty");
        write(&keyed, "");
        drain(&watcher, QUIET);
        write(&keyed, &theme_doc("Keyed Theme", "#1d1d1d"));
        expect_theme_applied(
            &watcher,
            &store,
            "#1d1d1d",
            &format!("{backend:?}: create-empty-then-fill"),
        );
    }
}

/// In use, deleted, recreated empty, then filled.
#[test]
fn a_keyed_theme_removed_then_recreated_is_applied() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        let keyed = store.paths().themes_dir().join("keyed.toml");
        write(&keyed, &theme_doc("Keyed Theme", "#1c1c1c"));
        drain(&watcher, QUIET);
        switch_theme(&store, &watcher, "Keyed Theme", "rm-then-recreate");
        assert_eq!(background(&store), "#1c1c1c", "{backend:?}");
        std::fs::remove_file(&keyed).unwrap();
        drain(&watcher, QUIET);
        write(&keyed, "");
        drain(&watcher, QUIET);
        write(&keyed, &theme_doc("Keyed Theme", "#1e1e1e"));
        expect_theme_applied(
            &watcher,
            &store,
            "#1e1e1e",
            &format!("{backend:?}: rm-then-recreate"),
        );
    }
}

/// Saved in chunks around a switch: truncated with the first chunk written
/// while another theme is in use, the switch settles on the unloadable
/// file, and the rest is appended in place.
#[test]
fn a_keyed_theme_saved_in_chunks_around_the_switch_is_applied() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        let keyed = store.paths().themes_dir().join("keyed.toml");
        let doc = theme_doc("Keyed Theme", "#1f1f1f");
        let (first, rest) = doc.split_at(doc.len() / 2);
        write(&keyed, first);
        drain(&watcher, QUIET);
        switch_theme(&store, &watcher, "Keyed Theme", "chunked");
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&keyed)
                .unwrap();
            file.write_all(rest.as_bytes()).unwrap();
        }
        expect_theme_applied(
            &watcher,
            &store,
            "#1f1f1f",
            &format!("{backend:?}: chunked save"),
        );
    }
}

/// S5 for #253 item 8: however many theme files fail to load, the native
/// backend follows at most `MAX_UNLOADABLE_FOLLOWED` of them (one file
/// descriptor each), and still sees the newest being completed.
#[test]
fn a_flood_of_unloadable_theme_files_is_followed_up_to_the_cap() {
    let (_dir, store, watcher) = setup(WatchBackend::Native, "mine");
    let themes = store.paths().themes_dir();
    // The lookup lists at most `MAX_THEME_DIR_ENTRIES` (256) entries, cut
    // in `readdir` order (name-hash order on APFS) before it sorts them,
    // so which of these 301 files it sees depends on their names. For
    // these names `keyed.toml` is among them on every run; other names or
    // more files could leave it out.
    for i in 0..300 {
        write(&themes.join(format!("zz-broken-{i:03}.toml")), "name = ");
    }
    drain(&watcher, QUIET);
    let before = open_descriptors();
    switch_theme(&store, &watcher, "Keyed Theme", "flood");
    let keyed = themes.join("keyed.toml");
    write(&keyed, "");
    drain(&watcher, QUIET);
    let after = open_descriptors();
    // `mine.toml` is no longer followed, up to eight unloadable files are,
    // and nothing else changed.
    assert!(
        after <= before + 8,
        "{after} descriptors open, {before} before the switch"
    );
    write(&keyed, &theme_doc("Keyed Theme", "#1c1c1c"));
    expect_theme_applied(&watcher, &store, "#1c1c1c", "the newest of a flood");
}

/// Descriptors open in this process.
fn open_descriptors() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

/// `WatchOptions::on_thread_start` runs on the watcher's thread (the core
/// passes its quality-of-service hook through it).
#[test]
fn the_thread_start_hook_runs_on_the_watcher_thread() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RAN_ON_WATCHER: AtomicBool = AtomicBool::new(false);
    fn hook() {
        let name = std::thread::current().name().map(str::to_owned);
        RAN_ON_WATCHER.store(
            name.as_deref() == Some("tessera-config-watch"),
            Ordering::SeqCst,
        );
    }
    for backend in BACKENDS {
        RAN_ON_WATCHER.store(false, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(ConfigPaths::new(dir.path())).unwrap();
        let watcher = store
            .watch(WatchOptions {
                backend,
                on_thread_start: Some(hook),
                ..Default::default()
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !RAN_ON_WATCHER.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(RAN_ON_WATCHER.load(Ordering::SeqCst), "{backend:?}");
        drop(watcher);
    }
}

/// #287 (M4): a hostile local process that keeps renaming a FIFO over a
/// broken theme file (followed while it does not load) and a regular file
/// back must not hang the watcher thread in an `open` of the FIFO: after
/// 20 rounds the watcher still reports the fix, and dropping it (which
/// joins the thread) returns within 5 s. The FIFO and the regular file
/// are one inode each, hard-linked into place, so a failing run can
/// release a reader blocked on the FIFO before it panics. Only the native
/// backend opens the files it watches. The race is real, so without the
/// fix a run catches it about three times in five; the deterministic test
/// is `a_fifo_raced_in_after_the_stat_does_not_block_the_registration`
/// in `watch.rs`.
#[cfg(unix)]
#[test]
fn a_fifo_renamed_over_a_broken_theme_file_does_not_hang_the_watcher() {
    const ROUNDS: usize = 20;
    /// Swaps per round: each one gives the watcher a directory event,
    /// so it registers the file again while the next swap lands.
    const SWAPS: usize = 200;
    const DROP_BOUND: Duration = Duration::from_secs(5);
    let backend = WatchBackend::Native;
    let dir = tempfile::tempdir().unwrap();
    let side = tempfile::tempdir_in(dir.path()).unwrap();
    let paths = ConfigPaths::new(dir.path().join("config"));
    std::fs::create_dir_all(paths.themes_dir()).unwrap();
    let broken = paths.themes_dir().join("broken.toml");
    let staged = paths.themes_dir().join(".staged");
    let regular = side.path().join("regular");
    let fifo = side.path().join("fifo");
    write(&regular, "name = \"Broken\"\nforeground = ");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::hard_link(&regular, &broken).unwrap();
    std::fs::write(paths.config_file(), config_doc("broken")).unwrap();
    let store = ConfigStore::open(paths).unwrap();
    let watcher = store
        .watch(WatchOptions {
            backend,
            debounce: Duration::from_millis(20),
            max_wait: Duration::from_millis(50),
            ..Default::default()
        })
        .unwrap();
    // Releases a reader blocked opening the FIFO, from a thread of its
    // own since the open blocks when no reader is.
    let release = |what: &str| -> ! {
        let fifo = fifo.clone();
        std::thread::spawn(move || {
            let _ = std::fs::OpenOptions::new().write(true).open(fifo);
        });
        panic!("{backend:?}: {what}");
    };
    for _ in 0..ROUNDS {
        for _ in 0..SWAPS {
            for source in [&fifo, &regular] {
                std::fs::hard_link(source, &staged).unwrap();
                std::fs::rename(&staged, &broken).unwrap();
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drain(&watcher, QUIET);
    write(&broken, &theme_doc("Broken", "#1d1d1d"));
    let deadline = Instant::now() + EVENTUALLY;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match watcher.recv_timeout(left) {
            Ok(ConfigEvent::ThemesChanged) if background(&store) == "#1d1d1d" => break,
            Ok(_) => {}
            Err(_) => release("the watcher no longer reports the fix"),
        }
    }
    let (dropped, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(watcher);
        let _ = dropped.send(());
    });
    if done.recv_timeout(DROP_BOUND).is_err() {
        release("dropping the watcher did not return within 5 s");
    }
}
