//! Watching `themes/`: an edit of the theme in use is reported as
//! `ThemesChanged` within a debounce plus 500 ms, by both backends; an edit
//! of an unrelated theme file produces at most one event and never a
//! `Changed`; the native backend follows the theme file when
//! `appearance.theme` names another one.
//!
//! Load-tolerant (#525): every event a test expects is waited for until it
//! arrives (up to [`EVENTUALLY`]), never for a fixed window that a starved
//! watcher can miss; the files a test saves never pass through a partial
//! state ([`save`]); the one latency bar is the fastest of up to
//! [`BOUND_ROUNDS`] edits of each kind; and every window that asserts *no*
//! event is a lower bound: a watcher starved past it can only make the test
//! miss a wrong event, never fail on a right one.

use std::io::Write as _;
use std::path::Path;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tessera_config::{
    ConfigEvent, ConfigPaths, ConfigStore, Theme, WatchBackend, WatchOptions, Watcher,
};

const BACKENDS: [WatchBackend; 2] = [WatchBackend::Native, WatchBackend::Poll];

/// The bound the issue sets: one debounce plus 500 ms.
fn limit() -> Duration {
    WatchOptions::default().debounce + Duration::from_millis(500)
}

/// How long a test waits for an event that is due (the issue's bound,
/// [`limit`], is asserted only by
/// `editing_the_theme_in_use_is_reported_once_within_the_bound`): long
/// enough for a starved process (`taskpolicy -b` on a loaded machine stalls
/// a thread for 1 to 26 s; #253 item 9, #525). `recv_timeout` returns as
/// soon as the event arrives, so only a failing run waits this long.
const EVENTUALLY: Duration = Duration::from_secs(30);

/// How long to wait to be sure no (further) event comes: the polling
/// interval, the debounce and generous scheduling slack. A lower bound: a
/// watcher starved past it reports a wrong event too late to be seen here.
const QUIET: Duration = Duration::from_millis(600);

/// Rounds of edits `editing_the_theme_in_use_is_reported_once_within_the_bound`
/// may take until the fastest edit of each kind is reported within
/// [`limit`].
const BOUND_ROUNDS: usize = 5;

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

/// Every event that arrives within `window`, but the
/// [`ConfigEvent::Unchanged`] of each re-read of the unchanged
/// `config.toml` a change in `themes/` causes (#535): these tests are
/// about the themes events.
fn drain(watcher: &Watcher, window: Duration) -> Vec<ConfigEvent> {
    let deadline = Instant::now() + window;
    let mut events = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match next_event(watcher, left) {
            Ok(event) => events.push(event),
            Err(_) => return events,
        }
    }
}

/// The next event within `bound` that is not a [`ConfigEvent::Unchanged`]
/// (see [`drain`]).
fn next_event(watcher: &Watcher, bound: Duration) -> Result<ConfigEvent, RecvTimeoutError> {
    let deadline = Instant::now() + bound;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match watcher.recv_timeout(left)? {
            ConfigEvent::Unchanged => {}
            event => return Ok(event),
        }
    }
}

/// Waits up to [`EVENTUALLY`] for the next event (but `Unchanged`), which
/// must be a `ThemesChanged`, and returns how long it took.
fn expect_themes_changed(watcher: &Watcher, backend: WatchBackend, what: &str) -> Duration {
    let started = Instant::now();
    let event = next_event(watcher, EVENTUALLY)
        .unwrap_or_else(|e| panic!("{backend:?}: no event for {what} within {EVENTUALLY:?}: {e}"));
    assert!(
        matches!(event, ConfigEvent::ThemesChanged),
        "{backend:?}: {what}: expected ThemesChanged, got {event:?}"
    );
    started.elapsed()
}

/// Asserts that no event (but `Unchanged`) comes within [`QUIET`].
fn expect_quiet(watcher: &Watcher, backend: WatchBackend, what: &str) {
    let rest = drain(watcher, QUIET);
    assert!(rest.is_empty(), "{backend:?}: {what}: then {rest:?}");
}

fn background(store: &ConfigStore) -> String {
    store.theme().background.to_string()
}

/// Writes `path` with `std::fs::write`: truncated, then filled. For the
/// states a test means to show the watcher (an empty file, a flood of
/// broken ones); a save uses [`save`].
fn write(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

/// Saves `text` as the whole of `path` in one step, so the watcher, however
/// late it reads, sees the old document or the new one and never a part of
/// either: an existing file is overwritten in place by a single `write(2)`
/// (no truncation; a shorter document is padded with newlines to the old
/// length, which changes nothing it means), and a new one is written under
/// a hidden name, which the theme lookup ignores, and renamed into place.
/// `std::fs::write` truncates first, and a writer starved between the
/// truncation and the write for longer than the debounce is seen as two
/// saves, an empty file and then the document (#525; documented for
/// `config.toml` at `ConfigEvent::Invalid`), so a test counting events per
/// save would fail on a right answer.
fn save(path: &Path, text: &str) {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let old = usize::try_from(meta.len()).unwrap();
            let mut doc = text.to_owned();
            if doc.len() < old {
                doc.push_str(&"\n".repeat(old - doc.len()));
            }
            let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let written = file.write(doc.as_bytes()).unwrap();
            assert_eq!(written, doc.len(), "{}: one write", path.display());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let name = path.file_name().unwrap().to_str().unwrap();
            let part = path.with_file_name(format!(".{name}.part"));
            write(&part, text);
            std::fs::rename(&part, path).unwrap();
        }
        Err(error) => panic!("{}: {error}", path.display()),
    }
}

/// One edit of the theme in use in a round of
/// `editing_the_theme_in_use_is_reported_once_within_the_bound`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    /// In place: no directory event; the native backend must watch the
    /// file itself.
    InPlace,
    /// A rename-save replaces the inode; the next in-place write, to the
    /// new file, must still be seen.
    RenameSave,
}

/// Each edit is reported once and applied, and the fastest edit of each
/// kind in up to [`BOUND_ROUNDS`] rounds is reported within the issue's
/// bound ([`limit`]). Every edit's event is waited for until it arrives, so
/// a loaded machine only adds rounds: at a load of 99 on 10 cores under
/// `taskpolicy -b` single edits took 1 to 9 s (#525), so a bar on every
/// edit, or on every edit of one round, cannot hold there. A watcher that
/// is late at every edit (a delay before the event) fails every round. A
/// late *phase* is not caught here: a polling backend that sampled the
/// theme file in use only at its full listing would meet the bound by
/// chance at about half its edits. The unit tests
/// `watch::tests::a_theme_switch_caught_up_at_start_up_is_followed_before_it_is_published`
/// and `watch::tests::a_theme_switch_while_running_is_followed_before_it_is_published`
/// catch it: they poll at 500 ms, so the full listing comes every 10 s,
/// and wait 5 s for an in-place fix of the theme file in use.
#[test]
fn editing_the_theme_in_use_is_reported_once_within_the_bound() {
    use Edit::{InPlace, RenameSave};
    const ROUND: [Edit; 3] = [InPlace, RenameSave, InPlace];
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        assert_eq!(background(&store), "#101010");
        let mine = store.paths().themes_dir().join("mine.toml");
        let tmp = mine.with_extension("toml.tmp");
        let mut colour = 0x11_0000_u32;
        let mut took: Vec<(Edit, Duration)> = Vec::new();
        let fastest = |took: &[(Edit, Duration)], kind: Edit| {
            took.iter()
                .filter(|(edit, _)| *edit == kind)
                .map(|(_, took)| *took)
                .min()
        };
        let within = |took: &[(Edit, Duration)]| {
            [InPlace, RenameSave]
                .into_iter()
                .all(|kind| fastest(took, kind).is_some_and(|best| best < limit()))
        };
        for _ in 0..BOUND_ROUNDS {
            for edit in ROUND {
                colour += 1;
                let hex = format!("#{colour:06x}");
                let doc = theme_doc("Mine", &hex);
                match edit {
                    InPlace => save(&mine, &doc),
                    RenameSave => {
                        write(&tmp, &doc);
                        std::fs::rename(&tmp, &mine).unwrap();
                    }
                }
                let elapsed = expect_themes_changed(&watcher, backend, &format!("{edit:?} {hex}"));
                assert_eq!(background(&store), hex, "{backend:?}: {edit:?}");
                expect_quiet(
                    &watcher,
                    backend,
                    &format!("one event per edit, {edit:?} {hex}"),
                );
                took.push((edit, elapsed));
            }
            if within(&took) {
                break;
            }
        }
        assert!(
            within(&took),
            "{backend:?}: the fastest edit of a kind not reported within {:?}: {took:?}",
            limit()
        );
    }
}

#[test]
fn editing_an_unrelated_theme_file_is_at_most_one_event_and_no_changed() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        let themes = store.paths().themes_dir();

        save(&themes.join("other.toml"), &theme_doc("Other", "#212121"));
        // Pinned per backend: the native one does not watch a theme file
        // that is not in use (a no-event window: a lower bound); the
        // polling one sees it at its next full listing, every twenty
        // intervals, then debounces. The wait allows
        // four times that for a throttled machine, and at least
        // [`EVENTUALLY`] (#262 N2); it ends at the event.
        let events = if backend == WatchBackend::Poll {
            let defaults = WatchOptions::default();
            let bound = (defaults.poll_interval * 20 * 4 + defaults.debounce).max(EVENTUALLY);
            let first = next_event(&watcher, bound)
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

        // Creating and deleting files the theme in use does not depend on:
        // one event each.
        save(&themes.join("new.toml"), &theme_doc("New", "#303030"));
        expect_themes_changed(&watcher, backend, "created");
        expect_quiet(&watcher, backend, "created");
        std::fs::remove_file(themes.join("new.toml")).unwrap();
        expect_themes_changed(&watcher, backend, "deleted");
        expect_quiet(&watcher, backend, "deleted");
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
        // nothing to report. A no-event window: a lower bound.
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
        save(
            &store.paths().config_file(),
            "[appearance]\ntheme = \"mine\"\nfont_size = 17\n",
        );
        let event = next_event(&watcher, EVENTUALLY)
            .unwrap_or_else(|e| panic!("{backend:?}: no event within {EVENTUALLY:?}: {e}"));
        assert!(matches!(event, ConfigEvent::Changed(_)), "{event:?}");
        // And no ThemesChanged after it (a lower bound).
        expect_quiet(&watcher, backend, "config.toml edit");
    }
}

/// The native backend registers the theme file `appearance.theme` resolves
/// to; after the configuration switches to another theme it must follow
/// the new file, or an in-place write to it goes unseen.
#[test]
fn the_watched_theme_file_follows_appearance_theme() {
    for backend in BACKENDS {
        let (_dir, store, watcher) = setup(backend, "mine");
        save(&store.paths().config_file(), &config_doc("other"));
        let event = next_event(&watcher, EVENTUALLY).expect("config change");
        assert!(matches!(event, ConfigEvent::Changed(_)), "{event:?}");
        assert_eq!(background(&store), "#202020");
        assert!(drain(&watcher, QUIET).is_empty(), "{backend:?}");

        // Several in-place writes at delays that fall at different points
        // of the polling interval: each is seen eventually (within
        // [`EVENTUALLY`]). No latency is asserted, so this does not tell
        // the newly followed file from the polling backend's full listing.
        // `editing_the_theme_in_use_is_reported_once_within_the_bound`
        // bounds an edit of the theme already in use; no test bounds the
        // latency of a file newly followed after `appearance.theme`
        // switches.
        for (n, delay_ms) in [0_u64, 130, 370].into_iter().enumerate() {
            std::thread::sleep(Duration::from_millis(delay_ms));
            let colour = format!("#2{n}2{n}2{n}");
            save(
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
        save(
            &store.paths().themes_dir().join("dracula.toml"),
            &theme_doc("Dracula", "#000001"),
        );
        expect_themes_changed(&watcher, backend, "user file shadowing a bundled theme");
        assert_eq!(background(&store), "#000001");
        // And that file is now followed for in-place writes.
        assert!(drain(&watcher, QUIET).is_empty(), "{backend:?}");
        save(
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
        save(&themes.join("mine.toml"), &theme_doc("Mine", "#141414"));
        expect_themes_changed(&watcher, backend, "directory recreated");
        assert!(drain(&watcher, QUIET).is_empty(), "{backend:?}");
        save(&themes.join("mine.toml"), &theme_doc("Mine", "#151515"));
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
        save(
            &store.paths().themes_dir().join("mine.toml"),
            &theme_doc("Mine", "#161616"),
        );
        for (n, watcher) in [&first, &second].into_iter().enumerate() {
            expect_themes_changed(watcher, backend, &format!("watcher {n}"));
            expect_quiet(watcher, backend, &format!("watcher {n}"));
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
            save(
                &store.paths().themes_dir().join("mine.toml"),
                &theme_doc("Mine", "#171717"),
            );
            let event = next_event(&watcher, EVENTUALLY).unwrap_or_else(|e| {
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
                save(&path, &theme_doc("Keyed Theme", "#181818"));
                expect_themes_changed(&watcher, backend, "keyed theme created");
                drain(&watcher, QUIET);
                assert_eq!(background(&store), "#181818");
            }
            save(&path, "name = \"half a theme\"\nforeground = ");
            expect_themes_changed(&watcher, backend, &format!("{file}: broken save"));
            assert!(drain(&watcher, QUIET).is_empty(), "{backend:?}: {file}");
            save(
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
        save(&mine, &theme_doc("Mine", "#1e1e1e"));
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
        save(&keyed, &theme_doc("Keyed Theme", "#1d1d1d"));
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
        save(&keyed, &theme_doc("Keyed Theme", "#1c1c1c"));
        drain(&watcher, QUIET);
        switch_theme(&store, &watcher, "Keyed Theme", "rm-then-recreate");
        assert_eq!(background(&store), "#1c1c1c", "{backend:?}");
        std::fs::remove_file(&keyed).unwrap();
        drain(&watcher, QUIET);
        write(&keyed, "");
        drain(&watcher, QUIET);
        save(&keyed, &theme_doc("Keyed Theme", "#1e1e1e"));
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
    // Each count is taken once the watcher has settled on state, not after
    // a fixed wait that a starved watcher can still be settling in (#525:
    // one count was 28 against 19). The save of the theme in use comes
    // after every file of the flood, so the settle that applies it has
    // listed them all.
    save(&themes.join("mine.toml"), &theme_doc("Mine", "#1a1a1a"));
    expect_theme_applied(&watcher, &store, "#1a1a1a", "the save after the flood");
    drain(&watcher, QUIET);
    let before = settled_descriptors();
    switch_theme(&store, &watcher, "Keyed Theme", "flood");
    let keyed = themes.join("keyed.toml");
    write(&keyed, "");
    // Sent once the settle has registered what it follows.
    expect_themes_changed(&watcher, WatchBackend::Native, "keyed.toml created empty");
    drain(&watcher, QUIET);
    let after = settled_descriptors();
    // `mine.toml` stays followed as the fallback `previous` (no file
    // stem matches `Keyed Theme`, and an empty `keyed.toml` does not load),
    // up to eight unloadable files are followed beside it, and nothing
    // else changed.
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

/// The fewest [`open_descriptors`] of five samples 20 ms apart: a settle
/// that the `$TMPDIR` traffic of other tests starts may hold a new
/// registration open beside the old one for a moment.
fn settled_descriptors() -> usize {
    (0..5)
        .map(|n| {
            if n > 0 {
                std::thread::sleep(Duration::from_millis(20));
            }
            open_descriptors()
        })
        .min()
        .unwrap()
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
        let deadline = Instant::now() + EVENTUALLY;
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
/// 20 rounds (or as many as `SWAP_BUDGET` allows) the watcher still
/// reports the fix, and dropping it (which joins the thread) returns within
/// [`EVENTUALLY`] (a hung `open` never returns; a starved thread can take
/// seconds, #525). The FIFO and the regular file are one inode each,
/// hard-linked into place, so a failing run can release a reader blocked
/// on the FIFO before it panics. Only the native
/// backend opens the files it watches. The race is real, so without the
/// fix a run catches it about three times in five; the deterministic test
/// is `a_fifo_raced_in_after_the_stat_does_not_block_the_registration`
/// in `watch.rs`.
///
/// The swaps stop at `SWAP_BUDGET`: unloaded, the 20 rounds take about
/// 5 s, but under `taskpolicy -b` the 16 000 links and renames are
/// throttled I/O and one run took more than 180 s (#525). A loaded run
/// races fewer times, so it can only miss the hang, never fail on a
/// watcher that has none.
#[cfg(unix)]
#[test]
fn a_fifo_renamed_over_a_broken_theme_file_does_not_hang_the_watcher() {
    const ROUNDS: usize = 20;
    /// Swaps per round: each one gives the watcher a directory event,
    /// so it registers the file again while the next swap lands.
    const SWAPS: usize = 200;
    const DROP_BOUND: Duration = EVENTUALLY;
    /// Wall time after which no further round of swaps starts.
    const SWAP_BUDGET: Duration = Duration::from_secs(20);
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
    let swapping = Instant::now();
    for round in 0..ROUNDS {
        if round > 0 && swapping.elapsed() >= SWAP_BUDGET {
            break;
        }
        for _ in 0..SWAPS {
            for source in [&fifo, &regular] {
                std::fs::hard_link(source, &staged).unwrap();
                std::fs::rename(&staged, &broken).unwrap();
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drain(&watcher, QUIET);
    save(&broken, &theme_doc("Broken", "#1d1d1d"));
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
        release("dropping the watcher did not return within EVENTUALLY");
    }
}
