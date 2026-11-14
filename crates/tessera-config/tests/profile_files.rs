//! Profile files (#312, ADR 0037): `profiles/<stem>.toml` next to
//! `config.toml` holds one profile each, after the `[[profiles]]` entries,
//! by stem in byte order, within the profile and file caps; a file that
//! does not load is skipped with one warning that names it escaped and
//! quotes none of it; the editor writes `config.toml` only and refuses an
//! edit of a file's profile (ADR 0040).

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tessera_config::edit::{ConfigEditor, EditError, FILE_PROFILE_REFUSED};
use tessera_config::{
    Config, ConfigEvent, ConfigPaths, ConfigStore, MAX_PROFILE_FILE_LEN, MAX_PROFILE_FILES,
    MAX_PROFILES, ProfileSource, Warning, WatchBackend, WatchOptions, Watcher, write_atomic,
};

/// A configuration directory with `config` as `config.toml` and `files`
/// in `profiles/` (name, text).
fn setup(config: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, ConfigPaths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::new(dir.path());
    std::fs::create_dir_all(paths.profiles_dir()).unwrap();
    std::fs::write(paths.config_file(), config).unwrap();
    for (name, text) in files {
        std::fs::write(paths.profiles_dir().join(name), text).unwrap();
    }
    (dir, paths)
}

fn names(store: &ConfigStore) -> Vec<String> {
    store
        .config()
        .profiles
        .iter()
        .map(|profile| profile.name.clone())
        .collect()
}

fn shown(store: &ConfigStore) -> Vec<String> {
    store.warnings().iter().map(ToString::to_string).collect()
}

/// `count` `[[profiles]]` entries named `c0`, `c1`, ...
fn entries(count: usize) -> String {
    (0..count).fold(String::new(), |mut text, i| {
        writeln!(text, "[[profiles]]\nname = \"c{i}\"").unwrap();
        text
    })
}

/// F1: a file without `name` is named after its stem; its keys are those
/// of a `[[profiles]]` table at the top level; its source is the file.
#[test]
fn a_file_without_a_name_is_named_after_its_stem() {
    let (_dir, paths) = setup(
        "",
        &[(
            "work.toml",
            "command = \"/bin/zsh -l\"\n[appearance]\nfont_size = 15.0\n",
        )],
    );
    let store = ConfigStore::open(paths).unwrap();
    let config = store.config();
    let [profile] = &config.profiles[..] else {
        panic!("{:?}", config.profiles);
    };
    assert_eq!(profile.name, "work");
    assert_eq!(profile.command.as_deref(), Some("/bin/zsh -l"));
    assert_eq!(profile.source, ProfileSource::File("work".into()));
    assert_eq!(profile.source.label(), "profiles/work.toml");
    let resolved = config.resolve_profile(Some("work"));
    assert!((resolved.appearance.font_size - 15.0).abs() < f32::EPSILON);
    assert!(store.warnings().is_empty(), "{:?}", shown(&store));
}

/// F2: the `[[profiles]]` entries come first, in file order, then the
/// files by stem in byte order (digits, uppercase, `_`, lowercase; no
/// `B.toml` beside `b.toml`, which APFS takes for one name); a `name` key
/// wins over the stem.
#[test]
fn files_follow_the_entries_sorted_by_stem_in_byte_order() {
    let (_dir, paths) = setup(
        "[[profiles]]\nname = \"z\"\n[[profiles]]\nname = \"y\"\n",
        &[
            ("b.toml", ""),
            ("a.toml", "name = \"Alpha\"\n"),
            ("_x.toml", ""),
            ("C.toml", ""),
            ("10.toml", ""),
            ("9.toml", ""),
        ],
    );
    let store = ConfigStore::open(paths).unwrap();
    assert_eq!(
        names(&store),
        ["z", "y", "10", "9", "C", "_x", "Alpha", "b"]
    );
    let config = store.config();
    assert_eq!(config.profiles[0].source, ProfileSource::Config);
    assert_eq!(config.profiles[6].source, ProfileSource::File("a".into()));
}

/// F2: a file whose name an earlier profile has (an entry's or an earlier
/// file's) gets the duplicate-name warning, naming the one used; the first
/// wins.
#[test]
fn a_duplicate_name_across_sources_warns_and_the_first_wins() {
    let (_dir, paths) = setup(
        "[[profiles]]\nname = \"work\"\ncommand = \"/bin/first\"\n",
        &[
            ("a.toml", "name = \"work\"\ncommand = \"/bin/second\"\n"),
            ("b.toml", "name = \"c\"\n"),
            ("c.toml", "command = \"/bin/third\"\n"),
        ],
    );
    let store = ConfigStore::open(paths).unwrap();
    assert_eq!(
        store.warnings(),
        [
            Warning::ProfileFile {
                file: "profiles/a.toml".into(),
                warning: Box::new(Warning::OutOfRange {
                    path: "name".into(),
                    detail: "profiles[0] has this name too and is the one used".into(),
                }),
            },
            Warning::ProfileFile {
                file: "profiles/c.toml".into(),
                warning: Box::new(Warning::OutOfRange {
                    path: "name".into(),
                    detail: "profiles/b.toml has this name too and is the one used".into(),
                }),
            },
        ]
    );
    let config = store.config();
    assert_eq!(
        config.resolve_profile(Some("work")).command.as_deref(),
        Some("/bin/first")
    );
    assert_eq!(config.resolve_profile(Some("c")).command, None);
}

/// F2: `[general] default_profile` may name a file's profile.
#[test]
fn default_profile_may_name_a_file_profile() {
    let (_dir, paths) = setup(
        "[general]\ndefault_profile = \"dev\"\n",
        &[("dev.toml", "command = \"/bin/dev\"\n")],
    );
    let store = ConfigStore::open(paths).unwrap();
    let resolved = store.config().resolve_profile(None);
    assert_eq!(resolved.name.as_deref(), Some("dev"));
    assert_eq!(resolved.command.as_deref(), Some("/bin/dev"));
    assert_eq!(resolved.warning, None);
    assert!(store.warnings().is_empty(), "{:?}", shown(&store));
}

/// F3, S5: [`MAX_PROFILES`] counts both sources together, with one
/// warning for all that are left out, the entries first; a file left out
/// takes its own warnings with it.
#[test]
fn the_profile_cap_counts_both_sources_with_one_warning() {
    let files: Vec<(String, String)> = (0..10)
        .map(|i| (format!("f{i}.toml"), "unknown_key = 1\n".to_owned()))
        .collect();
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(name, text)| (name.as_str(), text.as_str()))
        .collect();

    let (_dir, paths) = setup(&entries(MAX_PROFILES - 6), &files);
    let store = ConfigStore::open(paths).unwrap();
    assert_eq!(store.config().profiles.len(), MAX_PROFILES);
    assert_eq!(names(&store).last().unwrap(), "f5");
    let warnings = store.warnings();
    let caps: Vec<&Warning> = warnings
        .iter()
        .filter(|warning| matches!(warning, Warning::TooManyProfiles { .. }))
        .collect();
    assert_eq!(caps, [&Warning::TooManyProfiles { dropped: 4 }]);
    // The six files read warn about their unknown key; the four left out
    // do not.
    assert_eq!(warnings.len(), 1 + 6, "{warnings:?}");

    // `config.toml` past the cap on its own: one warning counts both.
    let (_dir, paths) = setup(&entries(MAX_PROFILES + 3), &files);
    let store = ConfigStore::open(paths).unwrap();
    assert_eq!(store.config().profiles.len(), MAX_PROFILES);
    assert_eq!(names(&store).last().unwrap(), "c255");
    assert_eq!(
        store.warnings(),
        [Warning::TooManyProfiles { dropped: 3 + 10 }]
    );
}

/// F3, S5: a file of [`MAX_PROFILE_FILE_LEN`] bytes loads; one byte more
/// is skipped unread with one warning, and the others still load.
#[test]
fn a_file_one_byte_past_the_cap_is_refused() {
    let at_cap = |len: u64| {
        let mut text = "name = \"big\"\n#".to_owned();
        text.push_str(&"x".repeat(usize::try_from(len).unwrap() - text.len() - 1));
        text.push('\n');
        assert_eq!(text.len() as u64, len);
        text
    };
    let (_dir, paths) = setup(
        "",
        &[
            ("a.toml", &at_cap(MAX_PROFILE_FILE_LEN)),
            ("b.toml", &at_cap(MAX_PROFILE_FILE_LEN + 1)),
            ("c.toml", ""),
        ],
    );
    let store = ConfigStore::open(paths).unwrap();
    assert_eq!(names(&store), ["big", "c"]);
    assert_eq!(
        store.warnings(),
        [Warning::ProfileFileIgnored {
            file: "profiles/b.toml".into(),
            reason: format!("larger than {MAX_PROFILE_FILE_LEN} bytes, or not a regular file"),
        }]
    );
}

/// F3, S8: a file that is not TOML, not UTF-8 or of the wrong shape is
/// skipped with one warning naming the file (escaped) and where the
/// problem is, never the text; the others load.
#[test]
fn a_file_that_does_not_load_is_skipped_with_one_warning() {
    let (_dir, paths) = setup(
        "",
        &[
            ("a.toml", "name = \"a\"\n"),
            ("broken.toml", "x = 1\nsecret = hunter2\n"),
            ("evil\u{1b}[31m.toml", "secret = = \"hunter2\"\n"),
            ("shape.toml", "appearance = \"hunter2\"\n"),
            ("z.toml", ""),
        ],
    );
    std::fs::write(
        paths.profiles_dir().join("latin1.toml"),
        b"name = \"caf\xe9\"\n",
    )
    .unwrap();
    let store = ConfigStore::open(paths).unwrap();
    assert_eq!(names(&store), ["a", "z"]);
    let warnings = store.warnings();
    assert_eq!(
        warnings,
        [
            Warning::ProfileFileIgnored {
                file: "profiles/broken.toml".into(),
                reason: "not TOML (line 2, column 10)".into(),
            },
            Warning::ProfileFileIgnored {
                file: "profiles/evil\\u{1b}[31m.toml".into(),
                reason: "not TOML (line 1, column 10)".into(),
            },
            Warning::ProfileFileIgnored {
                file: "profiles/latin1.toml".into(),
                reason: "not UTF-8 (at byte 11)".into(),
            },
            Warning::ProfileFileIgnored {
                file: "profiles/shape.toml".into(),
                reason: "a value has the wrong type (line 1, column 14)".into(),
            },
        ]
    );
    for warning in shown(&store) {
        assert!(!warning.contains("hunter2"), "{warning}");
        assert!(!warning.contains('\u{1b}'), "{warning}");
    }
}

/// `mkfifo` (the crate forbids `unsafe`, so no `libc::mkfifo`).
fn mkfifo(path: &Path) {
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

/// F3: hidden entries, entries not ending in `.toml` (or only `.toml`),
/// and entries that are not regular files through their links (a FIFO, a
/// directory, a dangling link) are skipped without a warning, and a FIFO
/// is never opened (the load would block). A link to a regular file is a
/// profile file.
#[test]
fn entries_that_are_not_profile_files_are_skipped() {
    let (dir, paths) = setup(
        "",
        &[
            (".hidden.toml", ""),
            (".toml", ""),
            ("notes.txt", ""),
            ("work.toml.bak", ""),
            ("WORK.TOML", ""),
            ("plain.toml", ""),
        ],
    );
    let profiles = paths.profiles_dir();
    mkfifo(&profiles.join("fifo.toml"));
    std::fs::create_dir(profiles.join("dir.toml")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("missing"), profiles.join("dangling.toml")).unwrap();
    std::fs::write(dir.path().join("target"), "command = \"/bin/linked\"\n").unwrap();
    std::os::unix::fs::symlink(dir.path().join("target"), profiles.join("linked.toml")).unwrap();
    std::os::unix::fs::symlink(profiles.join("fifo.toml"), profiles.join("to-fifo.toml")).unwrap();

    let store = ConfigStore::open(paths).unwrap();
    assert_eq!(names(&store), ["linked", "plain"]);
    assert_eq!(
        store.config().profiles[0].command.as_deref(),
        Some("/bin/linked")
    );
    assert!(store.warnings().is_empty(), "{:?}", shown(&store));
}

/// F3, S5: of 1000 files at most [`MAX_PROFILE_FILES`] are read, the
/// first by stem, with one warning for the rest.
#[test]
fn a_thousand_files_read_at_most_the_cap_with_one_warning() {
    let (_dir, paths) = setup("", &[]);
    for i in 0..1000 {
        std::fs::write(paths.profiles_dir().join(format!("p{i:04}.toml")), "").unwrap();
    }
    let store = ConfigStore::open(paths).unwrap();
    let names = names(&store);
    assert_eq!(names.len(), MAX_PROFILE_FILES);
    assert_eq!(names.first().unwrap(), "p0000");
    assert_eq!(names.last().unwrap(), "p0255");
    assert_eq!(
        store.warnings(),
        [Warning::TooManyProfileFiles {
            dropped: 1000 - MAX_PROFILE_FILES
        }]
    );
}

/// S8: the warnings about a file's keys are those of a `[[profiles]]`
/// entry, named relative to the file; the unknown keys are capped per
/// file.
#[test]
fn a_file_warns_as_an_entry_would() {
    let (_dir, paths) = setup(
        "",
        &[(
            "w.toml",
            "colour = 1\n[appearance]\nfont_size = 900.0\nblur_radius = 3\n",
        )],
    );
    let store = ConfigStore::open(paths).unwrap();
    let shown = shown(&store);
    assert_eq!(shown.len(), 3, "{shown:?}");
    assert!(
        shown[0].starts_with("profiles/w.toml: unknown key `colour`"),
        "{shown:?}"
    );
    assert!(
        shown[1].starts_with("profiles/w.toml: `appearance.font_size`"),
        "{shown:?}"
    );
    assert!(
        shown[2].starts_with("profiles/w.toml: `appearance.blur_radius`"),
        "{shown:?}"
    );
}

/// Every file under `dir` with its contents and modification time.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
    let mut all: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let meta = std::fs::metadata(&path).unwrap();
            (
                path.clone(),
                std::fs::read(&path).unwrap(),
                meta.modified().unwrap(),
            )
        })
        .collect();
    all.sort();
    all
}

/// F5, ADR 0040: an edit of a file's profile (`profiles[N]` and on, `N`
/// the number of entries) is refused with [`FILE_PROFILE_REFUSED`], a set
/// and a remove alike; an edit of an entry still works and is saved; a
/// new entry goes before the files' profiles. Nothing in `profiles/` is
/// written.
#[test]
fn the_editor_refuses_an_edit_of_a_file_profile() {
    let (_dir, paths) = setup(
        "[[profiles]]\nname = \"entry\"\n",
        &[("a.toml", "command = \"/bin/a\"\n"), ("b.toml", "")],
    );
    let before = snapshot(&paths.profiles_dir());
    let store = ConfigStore::open(paths.clone()).unwrap();
    assert_eq!(names(&store), ["entry", "a", "b"]);

    let mut editor = ConfigEditor::open(&paths.config_file()).unwrap();
    for path in ["profiles[1].command", "profiles[2].name"] {
        match editor.set(path, toml::Value::String("/bin/x".into())) {
            Err(EditError::Unsupported { reason, .. }) => {
                assert_eq!(reason, FILE_PROFILE_REFUSED, "{path}");
            }
            other => panic!("{path}: {other:?}"),
        }
    }
    match editor.remove("profiles[1]") {
        Err(EditError::Unsupported { reason, .. }) => assert_eq!(reason, FILE_PROFILE_REFUSED),
        other => panic!("{other:?}"),
    }
    assert!(!editor.is_modified());

    editor
        .set(
            "profiles[0].command",
            toml::Value::String("/bin/entry".into()),
        )
        .unwrap();
    assert_eq!(editor.add_profile("new").unwrap(), 1);
    assert!(editor.save().unwrap());
    store.reload().unwrap();
    assert_eq!(names(&store), ["entry", "new", "a", "b"]);
    assert_eq!(
        store.config().profiles[0].command.as_deref(),
        Some("/bin/entry")
    );
    assert_eq!(
        snapshot(&paths.profiles_dir()),
        before,
        "profiles/ untouched"
    );
}

/// F5: past the files' profiles a path is an entry's again (one past the
/// end is refused as before, by the cap or as a gap, not as a file's).
#[test]
fn without_profile_files_the_editor_is_unchanged() {
    let (_dir, paths) = setup("[[profiles]]\nname = \"entry\"\n", &[]);
    let mut editor = ConfigEditor::open(&paths.config_file()).unwrap();
    editor
        .set("profiles[1].name", toml::Value::String("second".into()))
        .unwrap();
    assert!(editor.save().unwrap());
    let store = ConfigStore::open(paths).unwrap();
    assert_eq!(names(&store), ["entry", "second"]);
}

const BACKENDS: [WatchBackend; 2] = [WatchBackend::Native, WatchBackend::Poll];

/// The issue's bound: 500 ms from the write to the store, on the native
/// backend (one debounce, 100 ms) and on the polling one (the change is
/// sampled within one poll interval, 50 ms, and applied one debounce
/// after that, at a tick).
const LIMIT: Duration = Duration::from_millis(500);

/// How long a wait for a due event may take when it asserts no bound:
/// enough for a starved process; `recv_timeout` returns at the event.
const EVENTUALLY: Duration = Duration::from_secs(5);

/// How long to wait to be sure no further reload comes: a poll interval,
/// the debounce and slack.
const QUIET: Duration = Duration::from_millis(600);

fn options(backend: WatchBackend) -> WatchOptions {
    WatchOptions {
        backend,
        ..WatchOptions::default()
    }
}

/// The next `Changed`, within `within`, skipping `ThemesChanged`.
fn next_change(watcher: &Watcher, within: Duration, what: &str) -> Arc<Config> {
    let started = Instant::now();
    loop {
        let left = within.saturating_sub(started.elapsed());
        match watcher.recv_timeout(left) {
            Ok(ConfigEvent::Changed(changed)) => return changed.config,
            Ok(ConfigEvent::ThemesChanged) => {}
            Ok(other) => panic!("{what}: {other:?}"),
            Err(error) => panic!("{what}: no reload within {within:?}: {error}"),
        }
    }
}

/// How many `Changed` arrive until none has for [`QUIET`].
fn more_changes(watcher: &Watcher) -> usize {
    let mut count = 0;
    loop {
        match watcher.recv_timeout(QUIET) {
            Ok(ConfigEvent::Changed(_)) => count += 1,
            Ok(_) => {}
            Err(_) => return count,
        }
    }
}

fn commands(config: &Config) -> Vec<(String, Option<String>)> {
    config
        .profiles
        .iter()
        .map(|profile| (profile.name.clone(), profile.command.clone()))
        .collect()
}

fn command(text: &str) -> String {
    format!("command = \"{text}\"\n")
}

/// `step` (a write, an add, a remove of a profile file) is applied by
/// exactly one reload within [`LIMIT`], that reload shows `expected`, and
/// no other follows.
fn reloads_once(
    watcher: &Watcher,
    what: &str,
    within: Duration,
    step: impl FnOnce(),
    expected: &[(&str, Option<&str>)],
) {
    let started = Instant::now();
    step();
    let config = next_change(watcher, EVENTUALLY, what);
    let elapsed = started.elapsed();
    let expected: Vec<(String, Option<String>)> = expected
        .iter()
        .map(|(name, command)| ((*name).to_owned(), command.map(str::to_owned)))
        .collect();
    assert_eq!(commands(&config), expected, "{what}");
    assert!(elapsed < within, "{what}: took {elapsed:?}");
    assert_eq!(more_changes(watcher), 0, "{what}: more than one reload");
}

/// F4: an in-place save, a rename-save, an add and a remove of a profile
/// file each give exactly one reload that shows it, within 500 ms, on
/// both backends.
#[test]
fn saving_adding_and_removing_a_profile_file_each_reload_once() {
    for backend in BACKENDS {
        let what = |step: &str| format!("{backend:?}: {step}");
        let (_dir, paths) = setup("", &[("a.toml", &command("/bin/a1"))]);
        let store = ConfigStore::open(paths.clone()).unwrap();
        let watcher = store.watch(options(backend)).unwrap();
        while watcher.try_recv().is_ok() {}
        let a = paths.profiles_dir().join("a.toml");
        let b = paths.profiles_dir().join("b.toml");
        reloads_once(
            &watcher,
            &what("in-place save"),
            LIMIT,
            || {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&a)
                    .unwrap();
                file.write_all(command("/bin/a2").as_bytes()).unwrap();
            },
            &[("a", Some("/bin/a2"))],
        );
        reloads_once(
            &watcher,
            &what("rename-save"),
            LIMIT,
            || write_atomic(&a, &command("/bin/a3")).unwrap(),
            &[("a", Some("/bin/a3"))],
        );
        reloads_once(
            &watcher,
            &what("add"),
            LIMIT,
            || std::fs::write(&b, "").unwrap(),
            &[("a", Some("/bin/a3")), ("b", None)],
        );
        reloads_once(
            &watcher,
            &what("in-place save of the added file"),
            LIMIT,
            || std::fs::write(&b, command("/bin/b1")).unwrap(),
            &[("a", Some("/bin/a3")), ("b", Some("/bin/b1"))],
        );
        reloads_once(
            &watcher,
            &what("remove"),
            LIMIT,
            || std::fs::remove_file(&a).unwrap(),
            &[("b", Some("/bin/b1"))],
        );
        assert_eq!(
            commands(&store.config()),
            [("b".to_owned(), Some("/bin/b1".to_owned()))]
        );
    }
}

/// Replaces the symbolic link `link` with one to `target` atomically (a
/// new link renamed over it, as `ln -sfn` does on Linux).
fn retarget(link: &Path, target: &Path) {
    let tmp = link.with_extension("retarget");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).unwrap();
    std::fs::rename(&tmp, link).unwrap();
}

/// F3, F4: a linked `profiles/` (a dotfiles checkout) is followed: an
/// in-place save and an add in its target, a retarget of the link, and the
/// target directory removed and created again each reload once.
#[test]
fn a_linked_profiles_directory_is_followed() {
    for backend in BACKENDS {
        let what = |step: &str| format!("{backend:?}: {step}");
        let dir = tempfile::tempdir().unwrap();
        let dots = dir.path().join("dots");
        for generation in ["gen1", "gen2"] {
            std::fs::create_dir_all(dots.join(generation)).unwrap();
        }
        std::fs::write(dots.join("gen1").join("w.toml"), command("/bin/w1")).unwrap();
        std::fs::write(dots.join("gen2").join("w.toml"), command("/bin/g2")).unwrap();
        let root = dir.path().join("Tessera");
        std::fs::create_dir(&root).unwrap();
        let paths = ConfigPaths::new(&root);
        std::os::unix::fs::symlink(dots.join("gen1"), paths.profiles_dir()).unwrap();
        let store = ConfigStore::open(paths.clone()).unwrap();
        assert_eq!(names(&store), ["w"]);
        let watcher = store.watch(options(backend)).unwrap();
        while watcher.try_recv().is_ok() {}

        let w = dots.join("gen1").join("w.toml");
        reloads_once(
            &watcher,
            &what("in-place save in the target"),
            EVENTUALLY,
            || std::fs::write(&w, command("/bin/w2")).unwrap(),
            &[("w", Some("/bin/w2"))],
        );
        reloads_once(
            &watcher,
            &what("add in the target"),
            EVENTUALLY,
            || std::fs::write(dots.join("gen1").join("x.toml"), "").unwrap(),
            &[("w", Some("/bin/w2")), ("x", None)],
        );
        reloads_once(
            &watcher,
            &what("retarget"),
            EVENTUALLY,
            || retarget(&paths.profiles_dir(), &dots.join("gen2")),
            &[("w", Some("/bin/g2"))],
        );
        reloads_once(
            &watcher,
            &what("target removed"),
            EVENTUALLY,
            || std::fs::remove_dir_all(dots.join("gen2")).unwrap(),
            &[],
        );
        reloads_once(
            &watcher,
            &what("target created again"),
            EVENTUALLY,
            || {
                let tmp = dots.join("gen2.tmp");
                std::fs::create_dir(&tmp).unwrap();
                std::fs::write(tmp.join("y.toml"), "").unwrap();
                std::fs::rename(&tmp, dots.join("gen2")).unwrap();
            },
            &[("y", None)],
        );
    }
}

/// F3, F4: a profile file that is a symbolic link (into a dotfiles
/// checkout) is followed: an in-place save of its target, the target
/// saved by rename, deleted, and created again each reload once.
#[test]
fn a_linked_profile_file_is_followed() {
    for backend in BACKENDS {
        let what = |step: &str| format!("{backend:?}: {step}");
        let dir = tempfile::tempdir().unwrap();
        let dots = dir.path().join("dots");
        std::fs::create_dir(&dots).unwrap();
        let target = dots.join("work.toml");
        std::fs::write(&target, command("/bin/w1")).unwrap();
        let (_dir, paths) = setup("", &[]);
        std::os::unix::fs::symlink(&target, paths.profiles_dir().join("work.toml")).unwrap();
        let store = ConfigStore::open(paths.clone()).unwrap();
        let watcher = store.watch(options(backend)).unwrap();
        while watcher.try_recv().is_ok() {}

        reloads_once(
            &watcher,
            &what("in-place save of the target"),
            EVENTUALLY,
            || std::fs::write(&target, command("/bin/w2")).unwrap(),
            &[("work", Some("/bin/w2"))],
        );
        reloads_once(
            &watcher,
            &what("rename-save of the target"),
            EVENTUALLY,
            || write_atomic(&target, &command("/bin/w3")).unwrap(),
            &[("work", Some("/bin/w3"))],
        );
        reloads_once(
            &watcher,
            &what("target deleted"),
            EVENTUALLY,
            || std::fs::remove_file(&target).unwrap(),
            &[],
        );
        reloads_once(
            &watcher,
            &what("target created again"),
            EVENTUALLY,
            || std::fs::write(&target, command("/bin/w4")).unwrap(),
            &[("work", Some("/bin/w4"))],
        );
    }
}

/// S5, S6: 10 000 writes to one profile file within 5 s coalesce into a
/// bounded number of reloads: at most one per `max_wait` while the writes
/// last, plus the final one, which shows the last write.
#[test]
fn a_flood_of_writes_to_a_profile_file_coalesces() {
    const WRITES: u32 = 10_000;
    const SPAN: Duration = Duration::from_secs(5);
    for backend in BACKENDS {
        let (_dir, paths) = setup("", &[("f.toml", &command("/bin/0"))]);
        let store = ConfigStore::open(paths.clone()).unwrap();
        let opts = options(backend);
        let max_wait = opts.max_wait;
        // A gap this long between two writes (the machine stalled the
        // writer) may end a debounce and earn one more reload.
        let stall = opts.debounce / 2;
        let watcher = store.watch(opts).unwrap();
        while watcher.try_recv().is_ok() {}
        let file = paths.profiles_dir().join("f.toml");
        let started = Instant::now();
        let mut stalls = 0_usize;
        let mut wrote = started;
        for i in 1..=WRITES {
            std::fs::write(&file, command(&format!("/bin/{i}"))).unwrap();
            if wrote.elapsed() >= stall {
                stalls += 1;
            }
            wrote = Instant::now();
            // Spread over `SPAN`, as a writer that never goes quiet.
            let due = SPAN * i / WRITES;
            if let Some(early) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(early);
            }
        }
        let span = started.elapsed();
        let mut reloads = 0_usize;
        let mut last = None;
        loop {
            match watcher.recv_timeout(EVENTUALLY.min(QUIET * 2)) {
                Ok(ConfigEvent::Changed(changed)) => {
                    reloads += 1;
                    last = Some(changed.config);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let bound = usize::try_from(span.as_millis() / max_wait.as_millis()).unwrap() + 2 + stalls;
        eprintln!(
            "{backend:?}: {WRITES} writes in {span:?}: {reloads} reloads \
             ({stalls} stalls, bound {bound})"
        );
        assert!(reloads >= 1, "{backend:?}: no reload");
        assert!(
            reloads <= bound,
            "{backend:?}: {reloads} reloads, bound {bound}"
        );
        let last = last.unwrap();
        assert_eq!(
            last.profiles[0].command.as_deref(),
            Some(format!("/bin/{WRITES}").as_str()),
            "{backend:?}: the last write is applied"
        );
    }
}

/// F4, polling: a profile file added is followed from the poll that sees
/// the directory change, so an in-place save of it right after is seen
/// within a poll or two, not only after the full listing every twentieth
/// poll (8 s at this interval). Three files in a row: a save caught at a
/// full listing leaves the next one a whole period away.
#[test]
fn polling_follows_an_added_profile_file_before_the_full_listing() {
    const WITHIN: Duration = Duration::from_millis(2500);
    let (_dir, paths) = setup("", &[]);
    let store = ConfigStore::open(paths.clone()).unwrap();
    let watcher = store
        .watch(WatchOptions {
            backend: WatchBackend::Poll,
            poll_interval: Duration::from_millis(400),
            ..WatchOptions::default()
        })
        .unwrap();
    while watcher.try_recv().is_ok() {}
    for (i, name) in ["a", "b", "c"].into_iter().enumerate() {
        let file = paths.profiles_dir().join(format!("{name}.toml"));
        std::fs::write(&file, "").unwrap();
        let config = next_change(&watcher, EVENTUALLY * 2, name);
        assert_eq!(config.profiles.len(), i + 1, "{name}");
        let started = Instant::now();
        std::fs::write(&file, command("/bin/x")).unwrap();
        let config = next_change(&watcher, EVENTUALLY * 2, name);
        let elapsed = started.elapsed();
        assert_eq!(
            config.profiles[i].command.as_deref(),
            Some("/bin/x"),
            "{name}"
        );
        assert!(elapsed < WITHIN, "{name}: saved in place, took {elapsed:?}");
    }
}

/// S7: the watcher never writes `profiles/`, whatever it reloads.
#[test]
fn watching_never_writes_profile_files() {
    for backend in BACKENDS {
        let (_dir, paths) = setup("", &[("a.toml", ""), ("b.toml", "bogus = = 1\n")]);
        let before = snapshot(&paths.profiles_dir());
        let store = ConfigStore::open(paths.clone()).unwrap();
        let watcher = store.watch(options(backend)).unwrap();
        std::fs::write(paths.config_file(), "[appearance]\nfont_size = 15\n").unwrap();
        let _ = next_change(&watcher, EVENTUALLY, &format!("{backend:?}"));
        drop(watcher);
        assert_eq!(snapshot(&paths.profiles_dir()), before, "{backend:?}");
    }
}

/// #312 "Done when" 3: the start-up cost of `watch()` (backend armed and
/// the catch-up settle, which loads every profile file) with 256 profile
/// files is at most twice the cost with 256 theme files. Release only,
/// best of five, each backend: `cargo nextest run --release -p
/// tessera-config --run-ignored only -E 'test(start_up)'`.
#[test]
#[ignore = "a release-mode timing measurement"]
fn start_up_with_256_profile_files_costs_at_most_twice_256_themes() {
    fn best(dir: &Path, backend: WatchBackend) -> Duration {
        (0..5)
            .map(|_| {
                let store = ConfigStore::open(ConfigPaths::new(dir)).unwrap();
                let started = Instant::now();
                let watcher = store.watch(options(backend)).unwrap();
                let took = started.elapsed();
                drop(watcher);
                took
            })
            .min()
            .unwrap()
    }
    let theme = "background = \"#101010\"\nforeground = \"#e0e0e0\"\n";
    let profile = "command = \"/bin/zsh\"\nworking_directory = \"~\"\n\
                   [appearance]\nfont_size = 13\n";
    let profiles = tempfile::tempdir().unwrap();
    std::fs::create_dir(profiles.path().join("profiles")).unwrap();
    let themes = tempfile::tempdir().unwrap();
    std::fs::create_dir(themes.path().join("themes")).unwrap();
    for i in 0..256 {
        std::fs::write(
            profiles.path().join(format!("profiles/p{i:03}.toml")),
            profile,
        )
        .unwrap();
        std::fs::write(themes.path().join(format!("themes/t{i:03}.toml")), theme).unwrap();
    }
    for dir in [profiles.path(), themes.path()] {
        std::fs::write(dir.join("config.toml"), "").unwrap();
    }
    for backend in BACKENDS {
        let with_profiles = best(profiles.path(), backend);
        let with_themes = best(themes.path(), backend);
        let ratio = with_profiles.as_secs_f64() / with_themes.as_secs_f64();
        eprintln!(
            "{backend:?}: watch() with 256 profile files {with_profiles:?}, \
             with 256 theme files {with_themes:?}: {ratio:.2}x"
        );
        assert!(ratio <= 2.0, "{backend:?}: {ratio:.2}x");
    }
}
