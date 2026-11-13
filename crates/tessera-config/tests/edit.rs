//! Writing settings back to `config.toml` (`tessera_config::edit`, ADR
//! 0040): edits keep every comment, key order and spelling they do not
//! touch, a value the loader would warn about is refused, saving is atomic
//! through symlinks with the mode kept, an external edit since open is a
//! conflict, and hostile files are refused instead of read or replaced.

use std::path::{Path, PathBuf};

use proptest::prelude::*;
use tessera_config::Warning;
use tessera_config::edit::{
    ConfigEditor, EditError, KeyPath, KeyPathError, MAX_DISPLAY_NAME_LEN, Step, ThemeOrigin,
    display_name, import_theme, theme_entries,
};

/// A hand-edited file: comment paragraphs, trailing comments, dotted keys,
/// an inline table, a multi-line array with comments, odd spacing, an
/// unknown key and two profiles.
const HAND_EDITED: &str = r#"# My Tessera setup.
# Synced from dotfiles.

[general]
confirm_close   =   false    # I live dangerously
fancy_unknown = "kept"

[appearance]
# Colours.

# The theme I like.
theme = 'Nord'
font_size = 13.5 # points
# Fully opaque.
opacity = 1.0
# ...and nothing after this.

[terminal]
scrollback_lines = 50_000

# Plugins come last.
[plugins]
enabled = [
  "hello", # the demo
  # nothing else yet
]

[[profiles]]
name = "Work"
appearance.theme = "Dracula"

[[profiles]]
# My shell.
name = "Shell"
command = "/bin/zsh -l"
[profiles.appearance]
font_size = 15.0

[keys]
"cmd+e" = "new_tab"  # muscle memory
"#;

fn write(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn open(text: &str) -> (tempfile::TempDir, PathBuf, ConfigEditor) {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), text);
    let editor = ConfigEditor::open(&path).unwrap();
    (dir, path, editor)
}

fn s(text: &str) -> toml::Value {
    toml::Value::String(text.into())
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// Opening and saving without edits changes nothing, byte for byte, and
/// does not even write.
#[test]
fn an_unedited_file_round_trips_byte_for_byte() {
    for text in [
        HAND_EDITED,
        tessera_config::DEFAULT_CONFIG_TOML,
        "",
        "\n\n# only\n",
    ] {
        let (_dir, path, mut editor) = open(text);
        assert_eq!(editor.to_toml(), text);
        assert!(!editor.is_modified());
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!editor.save().unwrap(), "nothing to write");
        assert_eq!(read(&path), text);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
    }
}

/// Setting existing values replaces only the value: spacing, quotes and
/// trailing comments around it stay.
#[test]
fn set_replaces_values_in_place() {
    let (_dir, path, mut editor) = open(HAND_EDITED);
    editor
        .set("general.confirm_close", toml::Value::Boolean(true))
        .unwrap();
    editor.set("appearance.theme", s("Dracula")).unwrap();
    editor
        .set("appearance.font_size", toml::Value::Float(14.0))
        .unwrap();
    editor
        .set("profiles[0].appearance.theme", s("Nord"))
        .unwrap();
    editor
        .set("profiles[1].appearance.font_size", toml::Value::Float(16.0))
        .unwrap();
    editor.set("keys.\"cmd+e\"", s("new_window")).unwrap();
    assert!(editor.save().unwrap());
    insta::assert_snapshot!("set_in_place", read(&path));
}

/// Setting a whole array keeps the comments between its old brackets,
/// each on a line of its own above the key, indented like it (#324 item
/// 21): in a table, as a dotted key and inside an inline table.
#[test]
fn setting_an_array_moves_its_comments_above_the_key() {
    let (_dir, path, mut editor) = open(HAND_EDITED);
    editor
        .set("plugins.enabled", toml::Value::Array(vec![s("a")]))
        .unwrap();
    assert!(editor.save().unwrap());
    insta::assert_snapshot!("array_set_keeps_comments", read(&path));
    for (text, want) in [
        (
            "# above\n  plugins.enabled = [ # first\n    \"x\", # x\n    # end\n  ]\n",
            "# above\n  # first\n  # x\n  # end\n  plugins.enabled = [\"a\"]\n",
        ),
        (
            "plugins = { enabled = [\"x\", # x\n] } # after\n",
            "# x\nplugins = { enabled = [\"a\"] } # after\n",
        ),
        (
            "[plugins]\nenabled = [\"x\", [\"nested\" # deep\n], # x\n]\n",
            "[plugins]\n# deep\n# x\nenabled = [\"a\"]\n",
        ),
    ] {
        let (_dir, _path, mut editor) = open(text);
        editor
            .set("plugins.enabled", toml::Value::Array(vec![s("a")]))
            .unwrap();
        assert_eq!(editor.to_toml(), want, "{text:?}");
    }
}

/// A profile whose name holds a NUL keeps its place (#333): the loaded
/// `profiles` count is the file's, so `profiles[N].name` (N profiles, as
/// a frontend reads them) adds a profile and never renames the last one.
#[test]
fn a_nul_named_profile_keeps_the_indices_of_the_file() {
    let text = "[[profiles]]\nname = \"a\\u0000b\"\n\n\
                [[profiles]]\nname = \"Work\"\ncommand = \"vim\"\n";
    let loaded = tessera_config::Config::from_toml_lenient(text).unwrap();
    let n = loaded.config.profiles.len();
    assert_eq!(n, 2, "every [[profiles]] entry is listed");
    let (_dir, path, mut editor) = open(text);
    editor
        .set(&format!("profiles[{n}].name"), s("New"))
        .unwrap();
    assert!(editor.save().unwrap());
    let saved = tessera_config::Config::from_toml_lenient(&read(&path)).unwrap();
    let profiles: Vec<(&str, Option<&str>)> = saved
        .config
        .profiles
        .iter()
        .map(|p| (p.name.as_str(), p.command.as_deref()))
        .collect();
    assert_eq!(
        profiles,
        [("a\u{fffd}b", None), ("Work", Some("vim")), ("New", None)]
    );
}

/// An integer set on a float setting is written as a float (#324 item
/// 4), in a value and in a profile; an integer setting keeps its integer.
#[test]
fn an_integer_on_a_float_key_is_written_as_a_float() {
    let (_dir, _path, mut editor) =
        open("[appearance]\nopacity = 0.5\n\n[[profiles]]\nname = \"p\"\n");
    editor
        .set("appearance.opacity", toml::Value::Integer(1))
        .unwrap();
    editor
        .set("appearance.font_size", toml::Value::Integer(14))
        .unwrap();
    editor
        .set("profiles[0].appearance.opacity", toml::Value::Integer(0))
        .unwrap();
    editor
        .set("terminal.scrollback_lines", toml::Value::Integer(10))
        .unwrap();
    let text = editor.to_toml();
    assert!(text.contains("\nopacity = 1.0\n"), "{text}");
    assert!(text.contains("\nfont_size = 14.0\n"), "{text}");
    assert!(
        text.contains("[profiles.appearance]\nopacity = 0.0\n"),
        "{text}"
    );
    assert!(text.contains("\nscrollback_lines = 10\n"), "{text}");
    // Out of range as an integer: refused as before.
    assert!(matches!(
        editor.set("appearance.opacity", toml::Value::Integer(5)),
        Err(EditError::Refused { .. })
    ));
}

/// New keys go at the end of their table, new tables after the existing
/// ones, a new profile after the last; nothing else moves.
#[test]
fn set_adds_keys_tables_and_profiles() {
    let (_dir, path, mut editor) = open(HAND_EDITED);
    editor
        .set("appearance.blur", toml::Value::Boolean(true))
        .unwrap();
    editor
        .set("terminal.clipboard_write", toml::Value::Boolean(true))
        .unwrap();
    editor.set("keys.\"cmd+d\"", s("split_right")).unwrap();
    editor
        .set("profiles[0].command", s("/usr/bin/fish"))
        .unwrap();
    assert_eq!(editor.add_profile("Remote").unwrap(), 2);
    editor
        .set("profiles[2].appearance.theme", s("Nord"))
        .unwrap();
    editor.set("plugins.enabled[1]", s("other")).unwrap();
    assert!(editor.save().unwrap());
    insta::assert_snapshot!("set_added", read(&path));
}

/// An element appended to an array written over several lines goes on a
/// line of its own, after the comment on the last element's line.
#[test]
fn elements_append_on_their_own_line() {
    let cases = [
        ("x = []\n", "x = [\"b\"]\n"),
        ("x = [\"a\"]\n", "x = [\"a\", \"b\"]\n"),
        ("x = [\n  \"a\",\n]\n", "x = [\n  \"a\",\n  \"b\",\n]\n"),
        ("x = [\n  \"a\"\n]\n", "x = [\n  \"a\",\n  \"b\"\n]\n"),
        (
            "x = [\n    \"a\", # one\n    # end\n]\n",
            "x = [\n    \"a\", # one\n    \"b\",\n    # end\n]\n",
        ),
        (
            "x = [\n  \"a\" # one\n]\n",
            "x = [\n  \"a\", # one\n  \"b\"\n]\n",
        ),
    ];
    for (before, after) in cases {
        let text = format!("[plugins]\n{}", before.replace("x =", "enabled ="));
        let (_dir, _path, mut editor) = open(&text);
        let key = if before.contains('a') {
            "plugins.enabled[1]"
        } else {
            "plugins.enabled[0]"
        };
        editor.set(key, s("b")).unwrap();
        assert_eq!(
            editor.to_toml(),
            format!("[plugins]\n{}", after.replace("x =", "enabled =")),
            "{before:?}"
        );
    }
}

/// The default file gains a `[keys]` table and keeps every comment.
#[test]
fn the_default_file_takes_edits_and_keeps_its_comments() {
    let (_dir, path, mut editor) = open(tessera_config::DEFAULT_CONFIG_TOML);
    editor
        .set("appearance.opacity", toml::Value::Float(0.9))
        .unwrap();
    editor.set("keys.\"cmd+e\"", s("new_tab")).unwrap();
    editor.save().unwrap();
    let text = read(&path);
    insta::assert_snapshot!("default_edited", text);
    // What the Restore Defaults button does: remove what it wrote.
    let mut editor = ConfigEditor::open(&path).unwrap();
    assert!(editor.remove("appearance.opacity").unwrap());
    assert!(editor.remove("keys.\"cmd+e\"").unwrap());
    editor.save().unwrap();
    let restored = read(&path);
    let comments = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|l| l.trim_start().starts_with('#'))
            .map(str::to_owned)
            .collect()
    };
    let lost: Vec<String> = comments(tessera_config::DEFAULT_CONFIG_TOML)
        .into_iter()
        .filter(|c| !restored.contains(c.as_str()))
        .collect();
    assert_eq!(lost, Vec::<String>::new());
    insta::assert_snapshot!("default_restored", restored);
}

/// Removing a value loses no comment: those above it stay where they
/// were, one on its line stays as a line of its own.
#[test]
fn remove_keeps_comments_that_are_not_attached() {
    let (_dir, path, mut editor) = open(HAND_EDITED);
    assert!(editor.remove("appearance.theme").unwrap());
    assert!(editor.remove("general.fancy_unknown").unwrap());
    assert!(editor.remove("appearance.opacity").unwrap());
    assert!(editor.remove("terminal.scrollback_lines").unwrap());
    assert!(editor.remove("plugins.enabled[0]").unwrap());
    assert!(editor.remove("profiles[1].appearance").unwrap());
    assert!(editor.remove("keys.\"cmd+e\"").unwrap());
    assert!(!editor.remove("keys.\"cmd+e\"").unwrap(), "gone already");
    assert!(
        !editor.remove("appearance.cursor_shape").unwrap(),
        "never there"
    );
    assert!(
        !editor.remove("profiles[7].name").unwrap(),
        "no such profile"
    );
    assert!(editor.save().unwrap());
    insta::assert_snapshot!("removed", read(&path));
}

/// A profile is removed with its sub-tables; the later ones move up.
#[test]
fn remove_profile_entries() {
    let (_dir, path, mut editor) = open(HAND_EDITED);
    assert!(editor.remove("profiles[0]").unwrap());
    assert_eq!(editor.profile_count(), 1);
    assert_eq!(editor.config().unwrap().profiles[0].name, "Shell");
    editor.save().unwrap();
    insta::assert_snapshot!("profile_removed", read(&path));
    let mut editor = ConfigEditor::open(&path).unwrap();
    assert!(editor.remove("profiles[0]").unwrap());
    assert_eq!(editor.profile_count(), 0);
    editor.save().unwrap();
    insta::assert_snapshot!("profiles_removed", read(&path));
}

/// Keys the loader does not know stay through every edit, and their
/// warning (there before) does not block one.
#[test]
fn unknown_keys_are_kept() {
    let text = "[general]\nfancy_unknown = 1\n[appearance]\nshiny = true\nopacity = 1.0\n[future]\nx = 1\n";
    let (_dir, path, mut editor) = open(text);
    editor
        .set("appearance.opacity", toml::Value::Float(0.5))
        .unwrap();
    editor.remove("general.confirm_close").unwrap();
    editor.save().unwrap();
    assert_eq!(
        read(&path),
        "[general]\nfancy_unknown = 1\n[appearance]\nshiny = true\nopacity = 0.5\n[future]\nx = 1\n"
    );
}

/// A byte order mark stays.
#[test]
fn a_byte_order_mark_is_kept() {
    let (_dir, path, mut editor) = open("\u{feff}[appearance]\nopacity = 1.0\n");
    editor
        .set("appearance.opacity", toml::Value::Float(0.5))
        .unwrap();
    editor.save().unwrap();
    assert_eq!(read(&path), "\u{feff}[appearance]\nopacity = 0.5\n");
}

/// The edits of the line-ending tests below.
fn edit_endings(editor: &mut ConfigEditor) {
    editor
        .set("appearance.opacity", toml::Value::Float(0.5))
        .unwrap();
    editor.set("general.default_profile", s("Shell")).unwrap();
    editor.remove("profiles[0]").unwrap();
}

/// A file without a final newline (an editor or `printf`) opens,
/// round-trips and saves without one.
#[test]
fn a_missing_final_newline_stays_missing() {
    for text in [
        "a = 1",
        "[x]",
        "a = 1 # c",
        "a = [1,\n2]",
        "[appearance]\nopacity = 1.0",
    ] {
        let (_dir, path, mut editor) = open(text);
        assert_eq!(editor.to_toml(), text);
        assert!(!editor.is_modified(), "{text:?}");
        assert!(!editor.save().unwrap(), "{text:?}: nothing to write");
        editor.remove("appearance.opacity").unwrap();
        editor
            .set("general.confirm_close", toml::Value::Boolean(false))
            .unwrap();
        editor.save().unwrap();
        let saved = read(&path);
        assert!(!saved.ends_with('\n'), "{text:?} saved as {saved:?}");
        assert_eq!(ConfigEditor::open(&path).unwrap().to_toml(), saved);
    }
    let text = HAND_EDITED.trim_end_matches('\n');
    let (_dir, path, mut editor) = open(text);
    edit_endings(&mut editor);
    editor.save().unwrap();
    let saved = read(&path);
    assert!(!saved.ends_with('\n'), "{saved:?}");
    insta::assert_snapshot!("no_final_newline_edited", saved);
}

/// A CRLF file is edited as LF and written back with CRLF on every line:
/// the same edits give the same file as on the LF original, line endings
/// apart. Mixed line endings are refused.
#[test]
fn crlf_line_endings_are_kept() {
    let (_dir, lf_path, mut editor) = open(HAND_EDITED);
    edit_endings(&mut editor);
    editor.save().unwrap();
    let lf_saved = read(&lf_path);
    for (crlf, final_newline) in [
        (HAND_EDITED.replace('\n', "\r\n"), true),
        (
            HAND_EDITED.trim_end_matches('\n').replace('\n', "\r\n"),
            false,
        ),
    ] {
        let (_dir, path, mut editor) = open(&crlf);
        assert_eq!(editor.to_toml(), crlf);
        assert!(!editor.is_modified());
        assert!(!editor.save().unwrap(), "nothing to write");
        edit_endings(&mut editor);
        editor.save().unwrap();
        let saved = read(&path);
        assert_eq!(
            saved.matches('\n').count(),
            saved.matches("\r\n").count(),
            "a bare LF in {saved:?}"
        );
        assert_eq!(saved.ends_with('\n'), final_newline, "{saved:?}");
        let expected = if final_newline {
            lf_saved.replace('\n', "\r\n")
        } else {
            lf_saved.trim_end_matches('\n').replace('\n', "\r\n")
        };
        assert_eq!(saved, expected);
        if final_newline {
            // `␍` marks each CR (insta would normalise CRLF away).
            insta::assert_snapshot!("crlf_edited", saved.replace("\r\n", "\u{240d}\n"));
        }
        assert_eq!(ConfigEditor::open(&path).unwrap().to_toml(), saved);
    }
    for mixed in ["[appearance]\r\nopacity = 1.0\n", "a = 1\nb = 2\r\n"] {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), mixed);
        // Refused at open, with no key path (#324 item 1).
        assert!(
            matches!(
                ConfigEditor::open(&path),
                Err(EditError::NotEditable { .. })
            ),
            "{mixed:?}"
        );
    }
}

/// Removing a profile moves the warnings of the later ones down an index;
/// that is not a new warning.
#[test]
fn removing_a_profile_renumbers_the_later_warnings() {
    let text = "[[profiles]]\nname = \"ok\"\n\n[[profiles]]\nname = \"bad\"\nbogus = 1\n\
                appearance.opacity = 7.0\n\n[[profiles]]\nname = \"worse\"\nbogus = 2\n";
    let (_dir, _path, mut editor) = open(text);
    assert!(editor.remove("profiles[0]").unwrap());
    assert_eq!(editor.profile_count(), 2);
    // The warnings of the removed profile go with it.
    assert!(editor.remove("profiles[0]").unwrap());
    assert_eq!(editor.config().unwrap().profiles[0].name, "worse");
    // A new warning on a renumbered profile is still refused.
    let (_dir, _path, mut editor) = open(text);
    editor.remove("profiles[0]").unwrap();
    let warnings = refused(editor.set("profiles[1].appearance.opacity", toml::Value::Float(9.0)));
    assert!(
        matches!(&warnings[..], [Warning::OutOfRange { path, .. }] if path == "profiles[1].appearance.opacity"),
        "{warnings:?}"
    );
}

fn refused(result: Result<(), EditError>) -> Vec<Warning> {
    match result {
        Err(EditError::Refused { warnings, .. }) => warnings,
        other => panic!("not refused: {other:?}"),
    }
}

/// A set the loader would warn about is refused with that warning, and
/// neither the document nor the file changes.
#[test]
fn a_set_the_loader_warns_about_is_refused() {
    let (_dir, path, mut editor) = open(HAND_EDITED);
    let cases: Vec<(&str, toml::Value, &str)> = vec![
        (
            "appearance.opacity",
            toml::Value::Float(5.0),
            "appearance.opacity",
        ),
        (
            "appearance.font_size",
            toml::Value::Float(1.0),
            "appearance.font_size",
        ),
        (
            "appearance.opacityy",
            toml::Value::Float(0.5),
            "appearance.opacityy",
        ),
        ("appearance.theme", s("No Such Theme"), "No Such Theme"),
        ("profiles[1].appearance.theme", s("Nope"), "Nope"),
        ("keys.\"cmd+nokey\"", s("new_tab"), "cmd+nokey"),
        (
            "terminal.scrollback_lines",
            toml::Value::Integer(20_000_000),
            "scrollback",
        ),
    ];
    for (key, value, mentioned) in cases {
        let warnings = refused(editor.set(key, value));
        assert!(
            warnings.iter().any(|w| w.to_string().contains(mentioned)),
            "{key}: {warnings:?}"
        );
        assert_eq!(editor.to_toml(), HAND_EDITED, "{key}");
        assert!(!editor.is_modified());
    }
    // A value of the wrong type fails the loader: refused as invalid.
    let invalid = editor.set("appearance.opacity", s("opaque"));
    assert!(
        matches!(invalid, Err(EditError::Invalid { .. })),
        "{invalid:?}"
    );
    assert!(!editor.save().unwrap());
    assert_eq!(read(&path), HAND_EDITED);
}

/// Edits the editor does not make are refused without a change.
#[test]
fn unsupported_edits_are_refused() {
    let (_dir, _path, mut editor) = open(HAND_EDITED);
    let mut table = toml::Table::new();
    table.insert("theme".into(), s("Nord"));
    for (key, value) in [
        ("appearance", s("x")),
        ("profiles", s("x")),
        ("profiles[0]", s("x")),
        ("appearance.theme.x", s("x")),
        ("plugins.enabled[5]", s("x")),
        ("profiles[1].appearance", toml::Value::Table(table)),
    ] {
        let result = editor.set(key, value);
        assert!(
            matches!(result, Err(EditError::Unsupported { .. })),
            "{key}: {result:?}"
        );
    }
    let deep = (0..40).fold(s("x"), |v, _| toml::Value::Array(vec![v]));
    assert!(matches!(
        editor.set("plugins.enabled", deep),
        Err(EditError::Unsupported { .. })
    ));
    for key in [
        "",
        ".a",
        "a.",
        "a..b",
        "a[x]",
        "a[1",
        "\"a",
        "a b",
        "a[1234567]",
    ] {
        assert!(
            matches!(editor.set(key, s("x")), Err(EditError::KeyPath(_))),
            "{key:?}"
        );
    }
    assert!(!editor.is_modified());
}

#[test]
fn key_paths_parse_and_display() {
    let path: KeyPath = "profiles[1].appearance.theme".parse().unwrap();
    assert_eq!(
        path.steps(),
        [
            Step::Key("profiles".into()),
            Step::Index(1),
            Step::Key("appearance".into()),
            Step::Key("theme".into()),
        ]
    );
    let path: KeyPath = r#"keys."cmd+\"x\\".a"#.parse().unwrap();
    assert_eq!(path.steps()[1], Step::Key("cmd+\"x\\".into()));
    assert_eq!(path.to_string(), r#"keys."cmd+\"x\\".a"#);
    assert_eq!("".parse::<KeyPath>(), Err(KeyPathError::Empty));
    assert_eq!(
        "a".repeat(2000).parse::<KeyPath>(),
        Err(KeyPathError::TooLong)
    );
    assert_eq!(
        vec!["a"; 17].join(".").parse::<KeyPath>(),
        Err(KeyPathError::TooManySteps)
    );
    assert_eq!(
        "a[70000]".parse::<KeyPath>(),
        Err(KeyPathError::IndexTooLarge)
    );
    assert!(matches!(
        "keys.\"a\u{7}\"".parse::<KeyPath>(),
        Err(KeyPathError::Malformed { .. })
    ));
}

/// The file on disk changed between open and save: the save is refused
/// and the external edit stays (no data loss), and no temporary file is
/// left.
#[test]
fn an_external_edit_since_open_is_a_conflict() {
    let (dir, path, mut editor) = open(HAND_EDITED);
    editor
        .set("appearance.opacity", toml::Value::Float(0.5))
        .unwrap();
    let theirs = format!("{HAND_EDITED}# added in vim\n");
    std::fs::write(&path, &theirs).unwrap();
    assert!(matches!(editor.save(), Err(EditError::Conflict)));
    assert_eq!(read(&path), theirs);
    // Removed, or replaced by a directory: a conflict too.
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(editor.save(), Err(EditError::Conflict)));
    assert!(!path.exists());
    std::fs::create_dir(&path).unwrap();
    assert!(matches!(editor.save(), Err(EditError::Conflict)));
    assert!(path.is_dir());
    std::fs::remove_dir(&path).unwrap();
    assert_eq!(names(dir.path()), Vec::<String>::new());
    // Reopened, the new version is what is edited.
    std::fs::write(&path, &theirs).unwrap();
    let mut editor = ConfigEditor::open(&path).unwrap();
    editor
        .set("appearance.opacity", toml::Value::Float(0.5))
        .unwrap();
    assert!(editor.save().unwrap());
    assert!(read(&path).ends_with("# added in vim\n"));
    assert!(read(&path).contains("opacity = 0.5\n"));
}

/// Two saves from one editor: the second sees its own first write, not a
/// conflict.
#[test]
fn saving_twice_is_not_a_conflict() {
    let (_dir, path, mut editor) = open(HAND_EDITED);
    editor
        .set("appearance.opacity", toml::Value::Float(0.5))
        .unwrap();
    assert!(editor.save().unwrap());
    editor
        .set("appearance.opacity", toml::Value::Float(0.75))
        .unwrap();
    assert!(editor.save().unwrap());
    assert!(read(&path).contains("opacity = 0.75\n"));
    assert!(!editor.is_modified());
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A symlinked `config.toml` (a dotfiles checkout) stays a symlink; its
/// target changes, keeps its mode, and no temporary file is left.
#[cfg(unix)]
#[test]
fn a_symlink_stays_a_symlink_and_the_mode_is_kept() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let dotfiles = dir.path().join("dotfiles");
    std::fs::create_dir(&dotfiles).unwrap();
    let target = dotfiles.join("tessera.toml");
    std::fs::write(&target, HAND_EDITED).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
    let config_dir = dir.path().join("Tessera");
    std::fs::create_dir(&config_dir).unwrap();
    let link = config_dir.join("config.toml");
    std::os::unix::fs::symlink("../dotfiles/tessera.toml", &link).unwrap();

    let mut editor = ConfigEditor::open(&link).unwrap();
    editor
        .set("appearance.opacity", toml::Value::Float(0.5))
        .unwrap();
    assert!(editor.save().unwrap());
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        Path::new("../dotfiles/tessera.toml")
    );
    assert!(read(&target).contains("opacity = 0.5\n"));
    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o640);
    assert_eq!(names(&dotfiles), ["tessera.toml"]);
    assert_eq!(names(&config_dir), ["config.toml"]);

    // Re-pointed between open and save: a conflict, both files kept.
    let other = dotfiles.join("other.toml");
    std::fs::write(&other, "# other\n").unwrap();
    let mut editor = ConfigEditor::open(&link).unwrap();
    editor
        .set("appearance.opacity", toml::Value::Float(0.25))
        .unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&other, &link).unwrap();
    assert!(matches!(editor.save(), Err(EditError::Conflict)));
    assert_eq!(read(&other), "# other\n");
    assert!(read(&target).contains("opacity = 0.5\n"));
}

fn open_error(path: &Path) -> String {
    match ConfigEditor::open(path) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("{} opened", path.display()),
    }
}

/// Files the editor must not read or replace are refused when opened.
#[cfg(unix)]
#[test]
fn hostile_files_are_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    // Missing, or a dangling symlink: create the file first.
    assert!(
        open_error(&path).contains("No such file"),
        "{}",
        open_error(&path)
    );
    std::os::unix::fs::symlink(dir.path().join("nowhere.toml"), &path).unwrap();
    assert!(open_error(&path).contains("No such file"));
    std::fs::remove_file(&path).unwrap();

    // A symlink cycle.
    std::os::unix::fs::symlink(dir.path().join("b"), &path).unwrap();
    std::os::unix::fs::symlink(&path, dir.path().join("b")).unwrap();
    assert!(open_error(&path).contains("symlink cycle"));
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(dir.path().join("b")).unwrap();

    // A FIFO (never blocks), a directory.
    let fifo = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(fifo.success());
    assert!(open_error(&path).contains("not a regular file"));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(open_error(&path).contains("not a regular file"));
    std::fs::remove_dir(&path).unwrap();

    // Over the 16 MiB cap (sparse, so cheap).
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(tessera_config::MAX_CONFIG_FILE_LEN + 1)
        .unwrap();
    drop(file);
    assert!(open_error(&path).contains("limit"), "{}", open_error(&path));

    // Read-only, or with a second hard link the rename would split.
    std::fs::write(&path, HAND_EDITED).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    assert!(open_error(&path).contains("read-only"));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::hard_link(&path, dir.path().join("twin.toml")).unwrap();
    assert!(open_error(&path).contains("hard link"));
    std::fs::remove_file(dir.path().join("twin.toml")).unwrap();

    // Not UTF-8, not TOML: left for the user to fix by hand.
    std::fs::write(&path, b"opacity = \xff\n").unwrap();
    assert!(open_error(&path).contains("encoding"));
    std::fs::write(&path, "[appearance\nopacity = 1\n").unwrap();
    assert!(open_error(&path).contains("parse"));
    assert_eq!(read(&path), "[appearance\nopacity = 1\n");
}

/// An edit that would make the file larger than the cap is refused.
#[test]
fn an_edit_past_the_size_cap_is_refused() {
    let filler = format!("# {}\n", "x".repeat(1022));
    let lines = usize::try_from(tessera_config::MAX_CONFIG_FILE_LEN).unwrap() / filler.len();
    let text = filler.repeat(lines);
    let (_dir, _path, mut editor) = open(&text);
    let long = "y".repeat(filler.len());
    assert!(matches!(
        editor.set("general.default_profile", s(&long)),
        Err(EditError::TooLarge)
    ));
}

/// A file that loads with a warning already can still be edited
/// elsewhere; the old warning is not "new".
#[test]
fn an_existing_warning_does_not_block_other_edits() {
    let (_dir, _path, mut editor) = open("[appearance]\nopacity = 7.0\nfont_size = 12.0\n");
    editor
        .set("appearance.font_size", toml::Value::Float(13.0))
        .unwrap();
    // Fixing the warned value is allowed too.
    editor
        .set("appearance.opacity", toml::Value::Float(1.0))
        .unwrap();
}

/// A NaN anywhere in the file does not make every edit look like a
/// change of it.
#[test]
fn a_nan_does_not_block_edits() {
    let (_dir, _path, mut editor) = open("[future]\nx = nan\n[appearance]\nopacity = 1.0\n");
    editor
        .set("appearance.opacity", toml::Value::Float(0.5))
        .unwrap();
    assert!(editor.to_toml().contains("x = nan"));
}

/// Inline tables and dotted keys are edited where they are.
#[test]
fn inline_tables_and_dotted_keys_are_edited_in_place() {
    let text = "appearance.theme = \"Nord\" # dotted\nappearance.font_size = 12.0\n\n[[profiles]]\nname = \"a\"\nappearance = { theme = \"Nord\", font_size = 12.0 }\n";
    let (_dir, path, mut editor) = open(text);
    editor
        .set("appearance.opacity", toml::Value::Float(0.5))
        .unwrap();
    editor.set("appearance.theme", s("Dracula")).unwrap();
    editor
        .set("profiles[0].appearance.theme", s("Dracula"))
        .unwrap();
    editor
        .set("profiles[0].appearance.blur", toml::Value::Boolean(true))
        .unwrap();
    editor.remove("profiles[0].appearance.font_size").unwrap();
    editor.remove("appearance.font_size").unwrap();
    editor.save().unwrap();
    insta::assert_snapshot!("inline_and_dotted", read(&path));
}

#[test]
fn display_names_are_sanitised() {
    assert_eq!(display_name("Plain Name"), "Plain Name");
    assert_eq!(
        display_name("a\u{0}b\u{1b}[31mc\u{7f}d\u{85}e\u{9b}f"),
        "ab[31mcdef"
    );
    assert_eq!(
        display_name("x\u{202e}y\u{2066}z\u{2069}\u{200f}\u{061c}w"),
        "xyzw"
    );
    let long = "é".repeat(100);
    let cut = display_name(&long);
    assert!(cut.len() <= MAX_DISPLAY_NAME_LEN);
    assert_eq!(cut.len(), 128);
    assert!(long.starts_with(&cut));
}

const BASE16: &str = "scheme: \"Ocean\"\nauthor: \"x\"\nbase00: \"2b303b\"\nbase01: \"343d46\"\nbase02: \"4f5b66\"\nbase03: \"65737e\"\nbase04: \"a7adba\"\nbase05: \"c0c5ce\"\nbase06: \"dfe1e8\"\nbase07: \"eff1f5\"\nbase08: \"bf616a\"\nbase09: \"d08770\"\nbase0A: \"ebcb8b\"\nbase0B: \"a3be8c\"\nbase0C: \"96b5b4\"\nbase0D: \"8fa1b3\"\nbase0E: \"b48ead\"\nbase0F: \"ab7967\"\n";

/// An imported scheme lands in `themes/` once, is listed, and selects
/// with its id; a second import of the same name is refused, not
/// replaced.
#[test]
fn themes_import_and_list() {
    let dir = tempfile::tempdir().unwrap();
    let themes = dir.path().join("themes");
    let source = dir.path().join("ocean.yaml");
    std::fs::write(&source, BASE16).unwrap();
    let imported = import_theme(&themes, &source).unwrap();
    assert_eq!(imported.name, "Ocean");
    assert_eq!(imported.id, "ocean");
    assert_eq!(imported.path, themes.join("ocean.toml"));
    let written = read(&imported.path);
    assert!(written.starts_with("# Imported by Tessera from ocean.yaml.\n"));
    assert_eq!(names(&themes), ["ocean.toml"]);
    assert!(matches!(
        import_theme(&themes, &source),
        Err(EditError::ThemeExists { .. })
    ));
    assert_eq!(read(&imported.path), written);

    // A user theme that shadows a bundled one replaces it in the list.
    std::fs::write(
        themes.join("nord.toml"),
        tessera_config::Theme::bundled("Dracula").unwrap().to_toml(),
    )
    .unwrap();
    // A file whose name would not show as written is left out.
    std::fs::write(
        themes.join("evil\u{202e}.toml"),
        tessera_config::Theme::bundled("Nord").unwrap().to_toml(),
    )
    .unwrap();
    std::fs::write(themes.join("broken.toml"), "nope").unwrap();
    let entries = theme_entries(&themes);
    let listed: Vec<(&str, &str, ThemeOrigin)> = entries
        .iter()
        .map(|e| (e.name.as_str(), e.id.as_str(), e.origin))
        .collect();
    assert!(!listed.iter().any(|(_, id, _)| *id == "Nord"), "{listed:?}");
    assert!(listed.contains(&("Tessera Dark", "Tessera Dark", ThemeOrigin::Bundled)));
    assert!(listed.contains(&("Ocean", "ocean", ThemeOrigin::User)));
    assert!(listed.contains(&("Dracula", "nord", ThemeOrigin::User)));
    assert!(
        !listed
            .iter()
            .any(|(_, id, _)| id.contains("evil") || *id == "broken")
    );
    // The id selects it.
    let path = write(dir.path(), "[appearance]\ntheme = \"Nord\"\n");
    let mut editor = ConfigEditor::open(&path).unwrap();
    editor.set("appearance.theme", s(&imported.id)).unwrap();
}

/// S5: a crowded `themes/` lists at most `MAX_THEME_FILES` user themes,
/// the first in file-name order, and every name within the display cap.
#[test]
fn the_theme_list_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let themes = dir.path().join("themes");
    std::fs::create_dir(&themes).unwrap();
    let mut theme = tessera_config::Theme::bundled("Nord").unwrap().clone();
    for i in 0..tessera_config::MAX_THEME_FILES + 40 {
        theme.name = format!("{i:04} {}", "n".repeat(300));
        std::fs::write(themes.join(format!("t{i:04}.toml")), theme.to_toml()).unwrap();
    }
    let entries = theme_entries(&themes);
    let user: Vec<_> = entries
        .iter()
        .filter(|e| e.origin == ThemeOrigin::User)
        .collect();
    assert_eq!(user.len(), tessera_config::MAX_THEME_FILES);
    assert_eq!(user[0].id, "t0000");
    assert_eq!(user.last().unwrap().id, "t0255");
    assert!(entries.iter().all(|e| e.name.len() <= MAX_DISPLAY_NAME_LEN));
}

/// Import refusals: unknown extension, a FIFO, an oversized scheme, a
/// scheme that does not import, a name with nothing to slug.
#[cfg(unix)]
#[test]
fn hostile_theme_imports_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let themes = dir.path().join("themes");
    let txt = dir.path().join("ocean.txt");
    std::fs::write(&txt, BASE16).unwrap();
    assert!(matches!(
        import_theme(&themes, &txt),
        Err(EditError::Unsupported { .. })
    ));
    let fifo = dir.path().join("fifo.yaml");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    assert!(matches!(
        import_theme(&themes, &fifo),
        Err(EditError::File(_))
    ));
    let big = dir.path().join("big.yaml");
    let file = std::fs::File::create(&big).unwrap();
    file.set_len(2 * 1024 * 1024).unwrap();
    drop(file);
    assert!(matches!(
        import_theme(&themes, &big),
        Err(EditError::File(_))
    ));
    let bad = dir.path().join("bad.itermcolors");
    std::fs::write(&bad, "not a plist").unwrap();
    assert!(matches!(
        import_theme(&themes, &bad),
        Err(EditError::Unsupported { .. })
    ));
    let nameless = dir.path().join("---.yaml");
    std::fs::write(
        &nameless,
        BASE16.replace("scheme: \"Ocean\"\n", "scheme: \"!!!\"\n"),
    )
    .unwrap();
    assert!(matches!(
        import_theme(&themes, &nameless),
        Err(EditError::Unsupported { .. })
    ));
    assert!(!themes.exists() || names(&themes).is_empty());
}

/// Records every event's message and fields.
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

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
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write as _;
                let _ = write!(self.0, "{}={value:?} ", field.name());
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// S8: checking an edit logs nothing, so a refused value never reaches
/// the log.
#[test]
fn checking_an_edit_logs_no_values() {
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = Capture(events.clone());
    tracing::subscriber::with_default(capture, || {
        let (_dir, _path, mut editor) = open("[appearance]\nopacity = 1.0\n");
        let _ = editor.set("appearance.opacity", toml::Value::Float(123.456));
        let _ = editor.set("appearance.secret_unknown", s("hunter2"));
        let _ = editor.set("keys.\"cmd+hunter3\"", s("new_tab"));
        editor
            .set("appearance.opacity", toml::Value::Float(0.5))
            .unwrap();
        editor.save().unwrap();
    });
    let events = events.lock().unwrap();
    for event in events.iter() {
        for secret in ["123.456", "hunter2", "hunter3", "0.5"] {
            assert!(!event.contains(secret), "{event}");
        }
    }
}

proptest! {
    /// Any key path that parses displays as a path that parses to it.
    #[test]
    fn key_paths_round_trip(text in r#"([a-z_]{1,8}|"[^"\\\x00-\x1f\x7f-\x9f]{0,8}")(\[[0-9]{1,3}\])?(\.([a-z_]{1,8}|"[^"\\\x00-\x1f\x7f-\x9f]{0,8}")(\[[0-9]{1,3}\])?){0,4}"#) {
        let path: KeyPath = text.parse().unwrap();
        let again: KeyPath = path.to_string().parse().unwrap();
        prop_assert_eq!(again, path);
    }

    /// Arbitrary text never panics the parser.
    #[test]
    fn key_path_parsing_never_panics(text in "\\PC{0,64}|[\\[\\]\".a-z0-9\\\\]{0,64}") {
        let _ = text.parse::<KeyPath>();
    }

    /// Hostile documents never panic the editor; one that opens either
    /// takes an edit, keeping every comment line (a profile removed takes
    /// at most those in it, #324 item 8), or refuses it and stays as it
    /// was. Setting an array keeps the comments between its brackets
    /// (#324 item 21).
    #[test]
    fn hostile_documents_never_lose_lines(
        lines in proptest::collection::vec(
            prop_oneof![
                Just("# comment".to_owned()),
                Just(String::new()),
                Just("[appearance]".to_owned()),
                Just("[terminal]".to_owned()),
                Just("[plugins]".to_owned()),
                Just("[[profiles]]".to_owned()),
                Just("name = \"p\"".to_owned()),
                Just("opacity = 0.5 # trailing".to_owned()),
                Just("appearance.opacity = 1.0".to_owned()),
                Just("x = [ # open\n  1, # one\n  # two\n  2,\n]".to_owned()),
                Just("enabled = [ # first\n  \"a\", # elem\n  # end\n]".to_owned()),
                Just("plugins.enabled = [\"a\", # dotted\n]".to_owned()),
                Just("y = { a = 1, b = \"#\" }".to_owned()),
                Just("s = \"\"\"\n# not a comment\n\"\"\"".to_owned()),
                Just("font_size = nan".to_owned()),
                Just("\"quoted key\" = 1".to_owned()),
                "[ -~]{0,20}",
            ],
            0..24,
        ),
        // Half the documents start with a commented array that op 4
        // replaces, so the comments it moves are checked on most runs.
        lead in proptest::bool::ANY,
        op in 0..6u8,
    ) {
        /// The comments the generator writes, each counted on its own.
        const COMMENTS: [&str; 9] = [
            "# comment", "# trailing", "# open", "# one", "# two", "# first", "# elem",
            "# end", "# dotted",
        ];
        let lead = if lead { "[plugins]\nenabled = [ # first\n  \"a\", # elem\n  # end\n]\n" } else { "" };
        let text = format!("{lead}{}", lines.join("\n"));
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), &text);
        let Ok(mut editor) = ConfigEditor::open(&path) else { return Ok(()) };
        let result = match op {
            0 => editor.set("appearance.opacity", toml::Value::Float(0.25)),
            1 => editor.remove("appearance.opacity").map(|_| ()),
            2 => editor.remove("profiles[0]").map(|_| ()),
            3 => editor.set("terminal.scrollback_lines", toml::Value::Integer(10)),
            _ => editor.set("plugins.enabled", toml::Value::Array(vec![s("b")])),
        };
        let after = editor.to_toml();
        if result.is_err() {
            prop_assert_eq!(&after, &text);
        }
        let reparsed: Result<toml::Table, _> = toml::from_str(&after);
        prop_assert!(reparsed.is_ok(), "{}", after);
        // The lines of the first profile: its header up to the next one.
        let profile: String = text
            .lines()
            .skip_while(|line| *line != "[[profiles]]")
            .enumerate()
            .take_while(|(i, line)| *i == 0 || !line.starts_with('['))
            .flat_map(|(_, line)| [line, "\n"])
            .collect();
        for comment in COMMENTS {
            let before = text.matches(comment).count();
            let now = after.matches(comment).count();
            prop_assert!(now <= before, "{comment} added: {text:?} -> {after:?}");
            let may_go = if op == 2 && result.is_ok() { profile.matches(comment).count() } else { 0 };
            prop_assert!(now + may_go >= before, "{comment} lost: {text:?} -> {after:?}");
        }
        editor.save().unwrap();
        prop_assert_eq!(read(&path), after);
    }
}

/// A line break inside a multi-line string is part of its value (TOML
/// keeps a CRLF there), so a CRLF file with one is refused, and so is a
/// value with a line break (`toml_edit` writes it as a multi-line string)
/// set in a CRLF file.
#[test]
fn crlf_in_a_multi_line_string_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let text = "[[profiles]]\r\nname = \"a\"\r\ncommand = \"\"\"x\r\ny\"\"\"\r\n";
    let path = write(dir.path(), text);
    assert!(matches!(
        ConfigEditor::open(&path),
        Err(EditError::NotEditable { .. })
    ));
    let text = "[[profiles]]\r\nname = \"a\"\r\n";
    let path = write(dir.path(), text);
    let mut editor = ConfigEditor::open(&path).unwrap();
    assert!(matches!(
        editor.set("profiles[0].command", s("x\ny")),
        Err(EditError::Unsupported { .. })
    ));
    assert_eq!(editor.to_toml(), text);
    editor.set("profiles[0].command", s("x y")).unwrap();
}

/// #324 item 2: another copy of a warning the file has already is
/// refused. A new key in an unknown table adds no warning (the loader
/// names the table once), and a second use of a missing theme the root
/// uses added none either; what the file has already may stay and change.
#[test]
fn another_copy_of_an_existing_warning_is_refused() {
    let text = "[bogus]\na = 1\n\n[general.extra]\nx = 1\n\n[appearance]\ntheme = \"Missing\"\n\n\
                [[profiles]]\nname = \"p\"\n";
    let (_dir, path, mut editor) = open(text);
    for (key, unknown) in [("bogus.b", "bogus"), ("general.extra.y", "general.extra")] {
        let warnings = refused(editor.set(key, toml::Value::Integer(2)));
        assert!(
            matches!(&warnings[..], [Warning::UnknownKey { path }] if path == unknown),
            "{key}: {warnings:?}"
        );
    }
    let warnings = refused(editor.set("profiles[0].appearance.theme", s("Missing")));
    assert!(
        matches!(&warnings[..], [Warning::ThemeNotFound { .. }]),
        "{warnings:?}"
    );
    assert_eq!(editor.to_toml(), text);
    editor.set("bogus.a", toml::Value::Integer(3)).unwrap();
    editor
        .set("appearance.font_size", toml::Value::Float(14.0))
        .unwrap();
    editor.save().unwrap();
    assert!(read(&path).contains("a = 3\n"));
}

/// #324 items 14 and 18: past `MAX_UNKNOWN_KEY_WARNINGS` the loader only
/// counts the unknown keys. Removing a listed one (a hidden one becomes
/// listed) or a profile with one (the count drops) adds nothing; one more
/// unknown key past the cap is still new.
#[test]
fn hidden_unknown_keys_count_as_present() {
    use std::fmt::Write as _;

    let mut text = String::from("[general]\n");
    for i in 0..tessera_config::MAX_UNKNOWN_KEY_WARNINGS + 6 {
        writeln!(text, "bogus{i} = 1").unwrap();
    }
    text.push_str("\n[[profiles]]\nname = \"a\"\nbogus = 1\n\n[[profiles]]\nname = \"b\"\n");
    let (_dir, _path, mut editor) = open(&text);
    assert!(editor.remove("general.bogus0").unwrap());
    assert!(editor.remove("general.bogus69").unwrap());
    assert!(editor.remove("profiles[0]").unwrap());
    for key in ["general.bogus_new", "profiles[0].bogus_new"] {
        let warnings = refused(editor.set(key, toml::Value::Integer(1)));
        assert!(
            matches!(&warnings[..], [Warning::UnknownKeysOmitted { .. }]),
            "{key}: {warnings:?}"
        );
    }
}

/// #324 items 15 and 17: the loader cuts a warning path at
/// `MAX_WARNING_PATH_LEN`; renumbered from `profiles[10]` to
/// `profiles[9]`, a 300-byte key's path is cut one character later and a
/// 244-byte key's path is not cut at all. Removing an earlier profile adds
/// no warning, but a new long key in that profile is still new.
#[test]
fn renumbered_cut_paths_are_not_new() {
    use std::fmt::Write as _;

    for len in [300, 244] {
        let mut text = String::new();
        for i in 0..11 {
            writeln!(text, "[[profiles]]\nname = \"p{i}\"").unwrap();
        }
        writeln!(text, "{} = 1", "k".repeat(len)).unwrap();
        let (_dir, _path, mut editor) = open(&text);
        assert!(editor.remove("profiles[0]").unwrap(), "{len}");
        let warnings = refused(editor.set(
            &format!("profiles[9].{}", "j".repeat(len)),
            toml::Value::Integer(1),
        ));
        assert!(
            matches!(&warnings[..], [Warning::UnknownKey { path }] if path.starts_with("profiles[9].jjj")),
            "{len}: {warnings:?}"
        );
    }
}

/// #324 item 19: the size cap applies to the file as saved, CRLF line
/// endings and all. Here the edit fits with LF endings and does not with
/// CRLF.
#[test]
fn the_size_cap_counts_crlf_line_endings() {
    let cap = usize::try_from(tessera_config::MAX_CONFIG_FILE_LEN).unwrap();
    let filler = format!("# {}\r\n", "x".repeat(1020));
    let text = filler.repeat(cap / filler.len() - 1);
    let lines = text.matches('\n').count();
    let (_dir, _path, mut editor) = open(&text);
    let fits = 500;
    let crlf_too_large = 2000;
    assert!(crlf_too_large < lines, "the LF text would fit");
    let mut small = editor.clone();
    small
        .set("general.default_profile", s(&"y".repeat(fits)))
        .unwrap();
    assert!(small.to_toml().len() <= cap);
    assert!(matches!(
        editor.set("general.default_profile", s(&"y".repeat(crlf_too_large))),
        Err(EditError::TooLarge)
    ));
}

/// #324 item 22: a file without a final newline keeps none when its last
/// profile goes and the comment above it stays (with the blank line
/// between them, which became last).
#[test]
fn removing_the_last_profile_keeps_no_final_newline() {
    for (text, edited) in [
        (
            "a = 1\n# about x\n\n[[profiles]]\nname = \"x\"",
            "a = 1\n# about x",
        ),
        (
            "a = 1\r\n# about x\r\n\r\n[[profiles]]\r\nname = \"x\"",
            "a = 1\r\n# about x",
        ),
        (
            "a = 1\n# about x\n\n[[profiles]]\nname = \"x\"\n",
            "a = 1\n# about x\n\n",
        ),
    ] {
        let (_dir, path, mut editor) = open(text);
        assert!(editor.remove("profiles[0]").unwrap());
        assert_eq!(editor.to_toml(), edited, "{text:?}");
        editor.save().unwrap();
        assert_eq!(read(&path), edited);
    }
}

/// #342 M7: the loader reads the first `MAX_PROFILES` profiles only, so an
/// edit of a later one would go unseen by every check (the Settings
/// window, adding at the loaded count, would rename the 257th entry).
/// Such a set and an add at the cap are refused; removing one works.
#[test]
fn no_profile_past_the_cap_is_edited() {
    use std::fmt::Write as _;

    use tessera_config::MAX_PROFILES;

    let mut text = String::new();
    for i in 0..=MAX_PROFILES {
        writeln!(text, "[[profiles]]\nname = \"p{i}\"").unwrap();
    }
    let (_dir, _path, mut editor) = open(&text);
    let unsupported = |result: Result<(), EditError>| {
        assert!(
            matches!(result, Err(EditError::Unsupported { .. })),
            "{result:?}"
        );
    };
    unsupported(editor.set(&format!("profiles[{MAX_PROFILES}].name"), s("New")));
    unsupported(editor.set(
        &format!("profiles[{MAX_PROFILES}].appearance.opacity"),
        toml::Value::Float(0.5),
    ));
    unsupported(editor.add_profile("New").map(drop));
    assert_eq!(editor.to_toml(), text);
    assert!(editor.remove(&format!("profiles[{MAX_PROFILES}]")).unwrap());
    unsupported(editor.add_profile("New").map(drop));
    assert!(editor.remove("profiles[0]").unwrap());
    assert_eq!(editor.add_profile("New").unwrap(), MAX_PROFILES - 1);
}

/// #324 item 7 (ADR 0040 amendment): the saved file keeps the old file's
/// group when the owner is in it, not the directory's, which a new file
/// gets. Skipped when the user is in no second group.
#[cfg(unix)]
#[test]
fn saving_keeps_the_files_group() {
    use std::os::unix::fs::MetadataExt as _;

    let (dir, path, _) = open("[appearance]\nopacity = 1.0\n");
    let dir_gid = std::fs::metadata(dir.path()).unwrap().gid();
    let Some(gid) = [12, 20, 80, 61, 33, 98, 100, 701, 702]
        .into_iter()
        .filter(|&gid| gid != dir_gid)
        .find(|&gid| std::os::unix::fs::chown(&path, None, Some(gid)).is_ok())
    else {
        eprintln!("skipped: the user is in no group but the directory's");
        return;
    };
    let mut editor = ConfigEditor::open(&path).unwrap();
    editor
        .set("appearance.opacity", toml::Value::Float(0.5))
        .unwrap();
    assert!(editor.save().unwrap());
    assert_eq!(std::fs::metadata(&path).unwrap().gid(), gid);
}

/// #324 item 3: a file with 20 000 lines that look like the editor's
/// marks (`#tessera-edit-<n>`) is edited in well under 100 ms: the mark
/// search is one pass, not one search per candidate (8 s before).
#[test]
#[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
fn an_edit_among_many_mark_like_lines_is_fast() {
    use std::fmt::Write as _;

    let mut text = String::new();
    for n in 0..20_000u32 {
        writeln!(text, "#tessera-edit-{n:x}").unwrap();
    }
    text.push_str("[appearance]\n# kept\nopacity = 1.0\n");
    let (_dir, _path, editor) = open(&text);
    let best = (0..5)
        .map(|_| {
            let mut editor = editor.clone();
            let start = std::time::Instant::now();
            assert!(editor.remove("appearance.opacity").unwrap());
            let took = start.elapsed();
            assert!(editor.to_toml().ends_with("[appearance]\n# kept\n"));
            took
        })
        .min()
        .unwrap();
    assert!(
        best < std::time::Duration::from_millis(100),
        "best of 5: {best:?}"
    );
}
