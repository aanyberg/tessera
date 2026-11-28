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
    // A key nested in the profile's tables is the file's too (#489 21.4
    // r2 M2).
    for (path, value) in [
        ("profiles[1].command", toml::Value::String("/bin/x".into())),
        ("profiles[2].name", toml::Value::String("x".into())),
        ("profiles[1].appearance.font_size", toml::Value::Float(16.0)),
        ("profiles[2].appearance.opacity", toml::Value::Float(0.5)),
    ] {
        match editor.set(path, value) {
            Err(EditError::Unsupported { reason, .. }) => {
                assert_eq!(reason, FILE_PROFILE_REFUSED, "{path}");
            }
            other => panic!("{path}: {other:?}"),
        }
    }
    for path in ["profiles[1]", "profiles[2].appearance.opacity"] {
        match editor.remove(path) {
            Err(EditError::Unsupported { reason, .. }) => {
                assert_eq!(reason, FILE_PROFILE_REFUSED, "{path}");
            }
            other => panic!("{path}: {other:?}"),
        }
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

/// `(key, value)` pairs for [`ConfigEditor::add_profile_with`].
fn keys(pairs: &[(&str, toml::Value)]) -> Vec<(String, toml::Value)> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

/// `n` profile files `p000.toml`, ... named `p000`, ... (by their stem).
fn files(n: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| (format!("p{i:03}.toml"), String::new()))
        .collect()
}

fn as_refs(files: &[(String, String)]) -> Vec<(&str, &str)> {
    files
        .iter()
        .map(|(name, text)| (name.as_str(), text.as_str()))
        .collect()
}

/// #312 app half: a profile added with its keys while profile files exist
/// is a new `[[profiles]]` entry, before the files' profiles (which move
/// up one), at the index the add returns; its name is the first of
/// `base`, `base 2`, ... that no profile of either source has (two files
/// with the same name, one named like the next number), so it shadows
/// none; `profiles/` is not written.
#[test]
fn add_profile_with_names_it_unused_across_both_sources() {
    let (_dir, paths) = setup(
        "[[profiles]]\nname = \"entry\"\n",
        &[
            ("a.toml", "name = \"Work\"\n"),
            ("b.toml", "name = \"Work\"\n"),
            ("c.toml", "name = \"Work 2\"\n"),
        ],
    );
    let before = snapshot(&paths.profiles_dir());
    let mut editor = ConfigEditor::open(&paths.config_file()).unwrap();
    assert_eq!(editor.unused_profile_name("Work"), "Work 3");
    assert_eq!(editor.unused_profile_name("entry"), "entry 2");
    assert_eq!(editor.unused_profile_name("free"), "free");
    let added = editor
        .add_profile_with(
            "Work",
            &keys(&[
                ("command", toml::Value::String("/bin/copy".into())),
                ("appearance.opacity", toml::Value::Float(0.5)),
            ]),
        )
        .unwrap();
    assert_eq!((added.index, added.name.as_str()), (1, "Work 3"));
    assert!(editor.save().unwrap());
    let store = ConfigStore::open(paths.clone()).unwrap();
    assert_eq!(names(&store), ["entry", "Work 3", "Work", "Work", "Work 2"]);
    let profile = &store.config().profiles[added.index];
    assert_eq!(profile.source, ProfileSource::Config);
    assert_eq!(profile.command.as_deref(), Some("/bin/copy"));
    assert_eq!(
        profile.appearance.as_ref().and_then(|a| a.opacity),
        Some(0.5)
    );
    assert_eq!(
        snapshot(&paths.profiles_dir()),
        before,
        "profiles/ untouched"
    );

    // The next add sees the entry just added, too.
    let mut editor = ConfigEditor::open(&paths.config_file()).unwrap();
    assert_eq!(editor.unused_profile_name("Work"), "Work 4");
    assert_eq!(editor.add_profile_with("Work", &[]).unwrap().index, 2);
}

/// #312, S5: [`MAX_PROFILES`] counts both sources for an add: 255
/// profiles in all (split three ways between `config.toml` and
/// `profiles/`) take one more, at the index past the entries; 256 take
/// none, and nothing changes. Kills the mutant that drops the file count
/// from the cap (#430 21.7 M5, `Xaddcap`).
#[test]
fn add_profile_with_counts_both_sources_toward_the_cap() {
    for entry_count in [0, 100, MAX_PROFILES - 1] {
        let file_count = MAX_PROFILES - 1 - entry_count;
        let files = files(file_count);
        let (_dir, paths) = setup(&entries(entry_count), &as_refs(&files));
        let mut editor = ConfigEditor::open(&paths.config_file()).unwrap();
        let added = editor.add_profile_with("new", &[]).unwrap();
        assert_eq!(added.index, entry_count, "{entry_count} entries");
        assert!(editor.save().unwrap());
        let store = ConfigStore::open(paths.clone()).unwrap();
        assert_eq!(store.config().profiles.len(), MAX_PROFILES);
        assert_eq!(names(&store)[entry_count], "new");
        assert!(
            !store
                .warnings()
                .iter()
                .any(|w| matches!(w, Warning::TooManyProfiles { .. })),
            "{:?}",
            store.warnings()
        );

        // Now at the cap: refused, through both adds, nothing changed.
        let mut editor = ConfigEditor::open(&paths.config_file()).unwrap();
        let refused = editor
            .add_profile_with("one more", &[])
            .expect_err("at the cap");
        assert!(
            matches!(refused, EditError::Unsupported { .. }) && refused.to_string().contains("256"),
            "{entry_count} entries: {refused}"
        );
        assert!(editor.add_profile("one more").is_err());
        assert!(!editor.is_modified());
    }
    // All 256 in profiles/: none.
    let files = files(MAX_PROFILES);
    let (_dir, paths) = setup("", &as_refs(&files));
    let mut editor = ConfigEditor::open(&paths.config_file()).unwrap();
    assert!(editor.add_profile_with("new", &[]).is_err());
    assert!(!editor.is_modified());
}

/// #312, S5: a name at the byte cap is added; one byte more is refused
/// before anything is set, and so is a name the unused-name number would
/// push past the cap (126 bytes taken: `… 2` is 128, added; 127 taken:
/// 129, refused).
#[test]
fn add_profile_with_keeps_the_name_within_the_byte_cap() {
    let cap = tessera_config::MAX_PROFILE_NAME_LEN;
    let name = |len: usize| "n".repeat(len);
    let (_dir, paths) = setup(
        &format!(
            "[[profiles]]\nname = \"{}\"\n[[profiles]]\nname = \"{}\"\n",
            name(126),
            name(127)
        ),
        &[("f.toml", &format!("name = \"{}\"\n", name(cap)))],
    );
    let mut editor = ConfigEditor::open(&paths.config_file()).unwrap();
    for (base, why) in [
        (name(cap + 1), "a profile name is at most 128 bytes"),
        (name(127), "the name is taken"),
        (name(cap), "the name is taken"),
    ] {
        let refused = editor
            .add_profile_with(&base, &[])
            .expect_err("past the cap");
        assert!(
            matches!(&refused, EditError::Unsupported { reason, .. }
                if reason.contains("128 bytes") && reason.starts_with(why)),
            "{} bytes: {refused}",
            base.len()
        );
        assert!(!editor.is_modified());
    }
    let added = editor.add_profile_with(&name(126), &[]).unwrap();
    assert_eq!(added.name, format!("{} 2", name(126)));
    assert_eq!(added.name.len(), cap);
    // A multi-byte name exactly at the cap (64 two-byte characters).
    let wide = "é".repeat(cap / 2);
    let added = editor.add_profile_with(&wide, &[]).unwrap();
    assert_eq!(added.name, wide);
    assert!(editor.save().unwrap());
    let store = ConfigStore::open(paths).unwrap();
    assert!(store.warnings().is_empty(), "{:?}", store.warnings());
    assert_eq!(names(&store)[2..4], [format!("{} 2", name(126)), wide]);
}

/// #312: one refused key refuses the whole profile, naming the key's full
/// path, and the document is as before: a theme that does not exist, a
/// key the loader does not know, a value of the wrong type, a table, a
/// `name` key, a malformed key, a path too deep, and more than
/// [`MAX_ADDED_PROFILE_KEYS`] keys.
#[test]
fn add_profile_with_is_all_or_nothing() {
    use tessera_config::edit::MAX_ADDED_PROFILE_KEYS;
    let (_dir, paths) = setup("[[profiles]]\nname = \"entry\"\n", &[("f.toml", "")]);
    let mut editor = ConfigEditor::open(&paths.config_file()).unwrap();
    let good = ("command", toml::Value::String("/bin/sh".into()));
    let cases: Vec<(Vec<(&str, toml::Value)>, &str)> = vec![
        (
            vec![
                good.clone(),
                (
                    "appearance.theme",
                    toml::Value::String("No Such Theme".into()),
                ),
            ],
            "`profiles[1].appearance.theme` refused",
        ),
        (
            vec![good.clone(), ("bogus", toml::Value::Integer(1))],
            "`profiles[1].bogus` refused",
        ),
        (
            vec![
                good.clone(),
                ("appearance.opacity", toml::Value::String("x".into())),
            ],
            "`profiles[1].appearance.opacity` refused",
        ),
        (
            vec![("appearance", toml::Value::Table(toml::Table::new()))],
            "`profiles[1].appearance`",
        ),
        (
            vec![good.clone(), ("name", toml::Value::String("other".into()))],
            "`profiles[1].name`",
        ),
        (
            vec![good.clone(), ("a..b", toml::Value::Integer(1))],
            "malformed",
        ),
        (
            vec![("a.b.c.d.e.f.g.h.i.j.k.l.m.n.o", toml::Value::Integer(1))],
            "steps",
        ),
    ];
    for (pairs, expected) in cases {
        let refused = editor
            .add_profile_with("new", &keys(&pairs))
            .expect_err(expected);
        assert!(
            refused.to_string().contains(expected),
            "{expected}: {refused}"
        );
        assert!(!editor.is_modified(), "{expected}");
        assert_eq!(editor.profile_count(), 1, "{expected}");
    }
    let many: Vec<(String, toml::Value)> = (0..=MAX_ADDED_PROFILE_KEYS)
        .map(|i| (format!("k{i}"), toml::Value::Integer(1)))
        .collect();
    let refused = editor.add_profile_with("new", &many).expect_err("too many");
    assert!(refused.to_string().contains("at most 64 keys"), "{refused}");
    assert!(!editor.is_modified());
    // The cap itself is taken (the keys are checked, not counted, past it).
    let at_cap = &many[..MAX_ADDED_PROFILE_KEYS];
    let refused = editor.add_profile_with("new", at_cap).expect_err("unknown");
    assert!(
        refused.to_string().contains("`profiles[1].k0` refused"),
        "{refused}"
    );
    // A path of exactly 16 steps is taken.
    let deep = keys(&[(
        "appearance.a.b.c.d.e.f.g.h.i.j.k.l.m",
        toml::Value::Integer(1),
    )]);
    let refused = editor.add_profile_with("new", &deep).expect_err("unknown");
    assert!(
        !refused.to_string().contains("steps"),
        "16 steps are a key path: {refused}"
    );
    assert!(!editor.is_modified());
    // And the good keys alone go in.
    assert_eq!(
        editor
            .add_profile_with("new", &keys(&[good]))
            .unwrap()
            .index,
        1
    );
}

const BACKENDS: [WatchBackend; 2] = [WatchBackend::Native, WatchBackend::Poll];

/// The issue's bound (#312, F4): 500 ms from the write to the store, on
/// the native backend (one debounce, 100 ms) and on the polling one (the
/// change is sampled within one poll interval, 50 ms, and applied one
/// debounce after that, at a tick). It is the bar on a quiet machine:
/// [`saving_adding_and_removing_a_profile_file_each_reload_once`] asserts
/// it on the fastest of up to [`BOUND_ROUNDS`] runs of each step (#656).
const LIMIT: Duration = Duration::from_millis(500);

/// Rounds of the five steps that
/// [`saving_adding_and_removing_a_profile_file_each_reload_once`] may take
/// until the fastest run of each step is applied within [`LIMIT`].
const BOUND_ROUNDS: usize = 5;

/// How long a wait for a due event may take when it asserts no bound:
/// enough for a starved process (`taskpolicy -b` on a loaded machine
/// stalls a thread for 1 to 26 s; #253 item 9, #525); `recv_timeout`
/// returns at the event, so only a failing run waits this long.
const EVENTUALLY: Duration = Duration::from_secs(30);

/// How long to wait to be sure no further reload comes: a poll interval,
/// the debounce and slack. A lower bound: a watcher starved past it sends
/// a wrong second reload too late to be seen here, so load can make the
/// test miss one but never fail a watcher that reloads once.
const QUIET: Duration = Duration::from_millis(600);

fn options(backend: WatchBackend) -> WatchOptions {
    WatchOptions {
        backend,
        ..WatchOptions::default()
    }
}

/// The next `Changed`, within `within`, skipping `ThemesChanged` and the
/// `Unchanged` reports that may come before it (#578, PR #586's run).
///
/// With one watcher, an `Unchanged` was a re-read that ran while a save
/// was half done, so it found the files as already applied: the native
/// backend sees `profiles/` change when [`write_atomic`] creates its
/// temporary file (`.a.toml.<pid>.<n>.tmp`, which the loader skips: it
/// is not a `*.toml` file), and if writing and syncing it takes longer than the
/// debounce (a loaded machine) the re-read runs before the rename and
/// finds nothing new (the files are quiet meanwhile: the temporary file
/// itself is not watched). Since #731 that re-read reports nothing, as
/// the files the load reads are as they were; the skip stays for a
/// re-read of a save another re-read already applied. The `Changed`
/// follows at the rename. Only `Changed` counts as a reload, so a save
/// that reloads twice still fails [`more_changes`].
fn next_change(watcher: &Watcher, within: Duration, what: &str) -> Arc<Config> {
    let started = Instant::now();
    loop {
        let left = within.saturating_sub(started.elapsed());
        match watcher.recv_timeout(left) {
            Ok(ConfigEvent::Changed(changed)) => return changed.config,
            Ok(ConfigEvent::ThemesChanged | ConfigEvent::Unchanged) => {}
            Ok(other) => panic!("{what}: {other:?}"),
            Err(error) => panic!("{what}: no reload within {within:?}: {error}"),
        }
    }
}

/// The configurations of the `Changed` that arrive until none has for
/// [`QUIET`].
fn more_changes(watcher: &Watcher) -> Vec<Arc<Config>> {
    let mut reloads = Vec::new();
    loop {
        match watcher.recv_timeout(QUIET) {
            Ok(ConfigEvent::Changed(changed)) => reloads.push(changed.config),
            Ok(_) => {}
            Err(_) => return reloads,
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
/// exactly one reload, that reload shows `expected`, and no other
/// follows; returns how long the reload took to arrive.
fn reloads_once(
    watcher: &Watcher,
    what: &str,
    step: impl FnOnce(),
    expected: &[(&str, Option<&str>)],
) -> Duration {
    let started = Instant::now();
    step();
    let config = next_change(watcher, EVENTUALLY, what);
    let elapsed = started.elapsed();
    let expected: Vec<(String, Option<String>)> = expected
        .iter()
        .map(|(name, command)| ((*name).to_owned(), command.map(str::to_owned)))
        .collect();
    assert_eq!(commands(&config), expected, "{what}");
    assert_eq!(
        more_changes(watcher).len(),
        0,
        "{what}: more than one reload"
    );
    elapsed
}

/// Saves `text` as the whole of the existing file `path` in place, in one
/// step: a single `write(2)` over the old bytes, without truncation (a
/// shorter document is padded with newlines to the old length, which
/// changes nothing it means), so the watcher, however late it reads, sees
/// the old document or the new one and never an empty file between them.
/// `std::fs::write` truncates first, and a writer starved between the
/// truncation and the write for longer than the debounce is rightly seen
/// as two saves, which a test counting reloads per save would fail (as
/// `save` in `tests/watch_themes.rs`, #525).
fn overwrite(path: &Path, text: &str) {
    let old = usize::try_from(std::fs::metadata(path).unwrap().len()).unwrap();
    let mut doc = text.to_owned();
    if doc.len() < old {
        doc.push_str(&"\n".repeat(old - doc.len()));
    }
    let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    let written = file.write(doc.as_bytes()).unwrap();
    assert_eq!(written, doc.len(), "{}: one write", path.display());
}

/// The steps of one round of
/// [`saving_adding_and_removing_a_profile_file_each_reload_once`], in
/// order.
const STEPS: [&str; 5] = [
    "in-place save",
    "rename-save",
    "add",
    "in-place save of the added file",
    "remove",
];

/// One round on a fresh directory: each of [`STEPS`] is applied by exactly
/// one reload that shows it, and how long each took to reach the watcher.
fn reload_round(backend: WatchBackend) -> [Duration; STEPS.len()] {
    let what = |step: usize| format!("{backend:?}: {}", STEPS[step]);
    let (_dir, paths) = setup("", &[("a.toml", &command("/bin/a1"))]);
    let store = ConfigStore::open(paths.clone()).unwrap();
    let watcher = store.watch(options(backend)).unwrap();
    while watcher.try_recv().is_ok() {}
    let a = paths.profiles_dir().join("a.toml");
    let b = paths.profiles_dir().join("b.toml");
    let took = [
        reloads_once(
            &watcher,
            &what(0),
            || overwrite(&a, &command("/bin/a2")),
            &[("a", Some("/bin/a2"))],
        ),
        reloads_once(
            &watcher,
            &what(1),
            || write_atomic(&a, &command("/bin/a3")).unwrap(),
            &[("a", Some("/bin/a3"))],
        ),
        reloads_once(
            &watcher,
            &what(2),
            || std::fs::write(&b, "").unwrap(),
            &[("a", Some("/bin/a3")), ("b", None)],
        ),
        reloads_once(
            &watcher,
            &what(3),
            || overwrite(&b, &command("/bin/b1")),
            &[("a", Some("/bin/a3")), ("b", Some("/bin/b1"))],
        ),
        reloads_once(
            &watcher,
            &what(4),
            || std::fs::remove_file(&a).unwrap(),
            &[("b", Some("/bin/b1"))],
        ),
    ];
    assert_eq!(
        commands(&store.config()),
        [("b".to_owned(), Some("/bin/b1".to_owned()))]
    );
    took
}

/// F4: an in-place save, a rename-save, an add and a remove of a profile
/// file each give exactly one reload that shows it, on both backends, and
/// the fastest run of each step in up to [`BOUND_ROUNDS`] rounds is within
/// the issue's 500 ms ([`LIMIT`]).
///
/// Load-tolerant (#656, as the theme bound of `tests/watch_themes.rs`,
/// #525): every step's reload is waited for until it arrives, and the
/// count and the configuration are asserted at every step of every round,
/// so a loaded machine only adds rounds. On CI a single step took 529 and
/// 645 ms, so a bar on every step cannot hold there. A watcher that is late
/// at every save of a kind (a delay before the reload) fails every round;
/// one late at only some saves (a late poll phase) is not caught. In the
/// background service class (`taskpolicy -b`, which no script uses) at a
/// load of 50 to 110 on 10 cores the fastest of five runs still took 516
/// to 720 ms, so the bar does not hold there.
#[test]
fn saving_adding_and_removing_a_profile_file_each_reload_once() {
    for backend in BACKENDS {
        let mut rounds: Vec<[Duration; STEPS.len()]> = Vec::new();
        let fastest = |rounds: &[[Duration; STEPS.len()]], step: usize| {
            rounds.iter().map(|took| took[step]).min()
        };
        let within = |rounds: &[[Duration; STEPS.len()]]| {
            (0..STEPS.len()).all(|step| fastest(rounds, step).is_some_and(|best| best < LIMIT))
        };
        while rounds.len() < BOUND_ROUNDS && !within(&rounds) {
            rounds.push(reload_round(backend));
        }
        let late: Vec<(&str, Option<Duration>)> = (0..STEPS.len())
            .filter(|&step| fastest(&rounds, step).is_none_or(|best| best >= LIMIT))
            .map(|step| (STEPS[step], fastest(&rounds, step)))
            .collect();
        assert!(
            late.is_empty(),
            "{backend:?}: the fastest run of a step not applied within {LIMIT:?} in {} rounds: {late:?}",
            rounds.len()
        );
    }
}

/// F4: an in-place save that truncates and then writes (`std::fs::write`,
/// as many editors save in place) gives one reload that shows the new
/// document, on both backends. The count depends on the writer as well as
/// the watcher: a writer stalled between the truncation and the write for
/// a debounce has left an empty file on disk for that long, which the
/// watcher rightly applies (an empty profile), so the count cannot be
/// asserted alone without failing on a loaded machine. It is asserted by
/// the writer's measured gap, as in `rapid_writes_are_debounced_into_one_event`
/// of `tests/reload.rs` (#283 item 4): a save that took under half a
/// debounce, from before the truncation to after the write, gets exactly
/// one reload; a slower one at most one more, before it, that shows the
/// empty file. The measured time includes all of the `open` that
/// truncates, not only the time from the truncation on, so it reads a
/// little long: a save near the line gets the looser check, never a wrong
/// one. Every save's last reload shows the new document. A watcher
/// that reloads twice per save fails both cases.
#[test]
fn a_truncating_in_place_save_reloads_once_unless_the_writer_stalls() {
    const SAVES: usize = 3;
    for backend in BACKENDS {
        let (_dir, paths) = setup("", &[("t.toml", &command("/bin/t0"))]);
        let store = ConfigStore::open(paths.clone()).unwrap();
        let opts = options(backend);
        let half = opts.debounce / 2;
        let watcher = store.watch(opts).unwrap();
        while watcher.try_recv().is_ok() {}
        let file = paths.profiles_dir().join("t.toml");
        let empty = vec![("t".to_owned(), None)];
        for i in 1..=SAVES {
            let what = format!("{backend:?}: truncating save {i}");
            let started = Instant::now();
            std::fs::write(&file, command(&format!("/bin/t{i}"))).unwrap();
            let gap = started.elapsed();
            let mut seen = vec![commands(&next_change(&watcher, EVENTUALLY, &what))];
            seen.extend(more_changes(&watcher).iter().map(|config| commands(config)));
            let want = vec![("t".to_owned(), Some(format!("/bin/t{i}")))];
            assert_eq!(seen.last(), Some(&want), "{what}: {seen:?}");
            if gap < half {
                assert_eq!(seen.len(), 1, "{what}: saved in {gap:?}: {seen:?}");
            } else {
                assert!(
                    seen.len() == 1 || (seen.len() == 2 && seen[0] == empty),
                    "{what}: saved in {gap:?}, more than the empty file before it: {seen:?}"
                );
            }
        }
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
            || overwrite(&w, &command("/bin/w2")),
            &[("w", Some("/bin/w2"))],
        );
        reloads_once(
            &watcher,
            &what("add in the target"),
            || std::fs::write(dots.join("gen1").join("x.toml"), "").unwrap(),
            &[("w", Some("/bin/w2")), ("x", None)],
        );
        reloads_once(
            &watcher,
            &what("retarget"),
            || retarget(&paths.profiles_dir(), &dots.join("gen2")),
            &[("w", Some("/bin/g2"))],
        );
        reloads_once(
            &watcher,
            &what("target removed"),
            || std::fs::remove_dir_all(dots.join("gen2")).unwrap(),
            &[],
        );
        reloads_once(
            &watcher,
            &what("target created again"),
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
/// saved by rename, deleted, and created again each reload once. The
/// target comes back by a rename, and the in-place saves here and in
/// [`a_linked_profiles_directory_is_followed`] write without truncating
/// ([`overwrite`]), so no step leaves an empty file on disk that a
/// starved writer could show the watcher as one more save; the truncating
/// save has its own test,
/// [`a_truncating_in_place_save_reloads_once_unless_the_writer_stalls`].
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
            || overwrite(&target, &command("/bin/w2")),
            &[("work", Some("/bin/w2"))],
        );
        reloads_once(
            &watcher,
            &what("rename-save of the target"),
            || write_atomic(&target, &command("/bin/w3")).unwrap(),
            &[("work", Some("/bin/w3"))],
        );
        reloads_once(
            &watcher,
            &what("target deleted"),
            || std::fs::remove_file(&target).unwrap(),
            &[],
        );
        reloads_once(
            &watcher,
            &what("target created again"),
            || write_atomic(&target, &command("/bin/w4")).unwrap(),
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
            overwrite(&file, &command(&format!("/bin/{i}")));
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
        overwrite(&file, &command("/bin/x"));
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
        overwrite(&paths.config_file(), "[appearance]\nfont_size = 15\n");
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
