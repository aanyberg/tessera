//! Edits the loader cannot check (#449, ADR 0040): a file the loader fails
//! on keeps what the user wrote, an index cap is checked on the full key
//! path, a NUL in a profile's command is dropped with a warning, and a save
//! removes the stale temporary files a crashed edit left (#324 item 6).
//! The per-cap refusals are `edit::tests::every_loader_cap_refuses_an_edit_past_it`,
//! which iterates the cap table.

use std::path::{Path, PathBuf};

use tessera_config::edit::{ConfigEditor, EditError, KeyPathError, MAX_KEY_PATH_LEN};
use tessera_config::{Config, MAX_ENABLED_PLUGINS, Warning};

fn open(text: &str) -> (tempfile::TempDir, PathBuf, ConfigEditor) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    let editor = ConfigEditor::open(&path).unwrap();
    (dir, path, editor)
}

fn s(text: &str) -> toml::Value {
    toml::Value::String(text.into())
}

fn ids(ids: &[&str]) -> toml::Value {
    toml::Value::Array(ids.iter().map(|id| s(id)).collect())
}

/// #489 48b m2: a wrong-typed `[plugins] enabled` fails the load; the
/// Plugins toggle's set of `plugins.enabled` is refused with the loader's
/// error and the file keeps its bytes, also after a save. A set elsewhere
/// that leaves the file failing is refused too. A replace, for when the
/// user asked to replace the value, writes it.
#[test]
fn a_wrong_typed_plugin_list_is_not_replaced_by_a_toggle() {
    for wrong in ["\"x\"", "3", "[\"a\", 1]"] {
        let text = format!("# mine\n[plugins]\nenabled = {wrong} # by hand\n");
        let (_dir, path, mut editor) = open(&text);
        let tessera_config::Error::Parse(loader) = Config::from_toml_lenient(&text).unwrap_err()
        else {
            panic!("{wrong}: not a parse error");
        };
        for value in [ids(&["hello"]), ids(&[])] {
            let result = editor.set("plugins.enabled", value);
            match &result {
                Err(EditError::Invalid { error, .. }) => {
                    assert_eq!(error, &loader, "{wrong}");
                }
                other => panic!("{wrong}: {other:?}"),
            }
        }
        assert!(
            matches!(
                editor.set("appearance.font_size", toml::Value::Float(14.0)),
                Err(EditError::Invalid { .. })
            ),
            "{wrong}"
        );
        assert_eq!(editor.to_toml(), text, "{wrong}");
        assert!(!editor.save().unwrap(), "{wrong}");
        assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes(), "{wrong}");
        // A wrong-typed replacement is still refused.
        assert!(
            editor.replace("plugins.enabled", s("y")).is_err(),
            "{wrong}"
        );
        editor.replace("plugins.enabled", ids(&["hello"])).unwrap();
        assert!(editor.save().unwrap());
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with("# mine\n[plugins]\n"), "{saved}");
        let config = Config::from_toml_lenient(&saved).unwrap();
        assert_eq!(config.config.plugins.enabled, ["hello"]);
    }
}

/// A file the loader fails on because of a scalar of the wrong type is
/// repaired by a set of that key, or reset by its remove, as before #449:
/// only a list or table the loader rejected is kept from a toggle. A
/// wrong-typed `[plugins] enabled` stays byte for byte while a scalar
/// elsewhere is repaired, and is still refused after.
#[test]
fn a_wrong_typed_scalar_is_repaired_and_a_wrong_typed_list_is_kept() {
    let scalars = [
        (
            "[appearance]\nopacity = \"x\"\n",
            "appearance.opacity",
            toml::Value::Float(0.5),
        ),
        (
            "[appearance]\nfont_size = \"14\"\n",
            "appearance.font_size",
            toml::Value::Float(14.0),
        ),
        (
            "[terminal]\nscrollback_lines = \"x\"\n",
            "terminal.scrollback_lines",
            toml::Value::Integer(1000),
        ),
        (
            "[general]\nconfirm_close = \"yes\"\n",
            "general.confirm_close",
            toml::Value::Boolean(true),
        ),
        ("[appearance]\ntheme = 3\n", "appearance.theme", s("Nord")),
        (
            "appearance = { opacity = \"x\" }\n",
            "appearance.opacity",
            toml::Value::Float(0.5),
        ),
        (
            "[[profiles]]\nname = \"p\"\nappearance.blur = \"y\"\n",
            "profiles[0].appearance.blur",
            toml::Value::Boolean(true),
        ),
        // A scalar in an inline table inside an inline array is the
        // rejected value, not the array that holds it (adversarial B1).
        (
            "profiles = [\n  { name = \"Work\", command = 3 },\n]\n",
            "profiles[0].command",
            s("/bin/zsh"),
        ),
        (
            "profiles = [\n  { name = \"Work\", working_directory = 3 },\n]\n",
            "profiles[0].working_directory",
            s("/tmp"),
        ),
        (
            "[selection]\nsmart_rules = [\n  { regex = \"a+\", precision = 7 },\n]\n",
            "selection.smart_rules[0].precision",
            s("high"),
        ),
    ];
    for (text, key, value) in scalars {
        assert!(Config::from_toml_lenient(text).is_err(), "{text}");
        let (_dir, path, mut editor) = open(text);
        editor
            .set(key, value)
            .unwrap_or_else(|e| panic!("{key}: {e}"));
        assert!(editor.save().unwrap());
        Config::from_toml_lenient(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let (_dir, path, mut editor) = open(text);
        assert!(editor.remove(key).unwrap_or_else(|e| panic!("{key}: {e}")));
        assert!(editor.save().unwrap());
        Config::from_toml_lenient(&std::fs::read_to_string(&path).unwrap()).unwrap();
    }
    // The list (or a table where one goes) is not set, removed, removed
    // with its table or edited inside: each is refused, nothing written.
    for wrong in [
        "\"x\"",
        "3",
        "[\"a\", 1]",
        "[\"x\", [\"nested\"]]",
        "{ a = 1 }",
    ] {
        let text = format!("[plugins]\nenabled = {wrong} # mine\n");
        let (_dir, path, mut editor) = open(&text);
        for result in [
            editor.set("plugins.enabled", ids(&["hello"])),
            editor.remove("plugins.enabled").map(drop),
            editor.remove("plugins").map(drop),
        ] {
            // A table where the list goes is not replaced by a value at all.
            assert!(
                matches!(
                    result,
                    Err(EditError::Invalid { .. } | EditError::Unsupported { .. })
                ),
                "{wrong}: {result:?}"
            );
        }
        if wrong.starts_with('[') {
            let result = editor.set("plugins.enabled[1]", s("b"));
            assert!(
                matches!(result, Err(EditError::Invalid { .. })),
                "{wrong}: {result:?}"
            );
        }
        assert!(!editor.save().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), text.as_bytes(), "{wrong}");
    }
}

/// #312 m3: `add_profile_with` checks the key path cap on the full path:
/// a key of [`MAX_KEY_PATH_LEN`] bytes parses alone and is too long after
/// `profiles[N].`.
#[test]
fn a_profile_key_too_long_with_its_prefix_is_refused() {
    let (_dir, _path, mut editor) = open("");
    let key = format!("a{}", "b".repeat(MAX_KEY_PATH_LEN - 1));
    assert!(key.parse::<tessera_config::edit::KeyPath>().is_ok());
    let result = editor.add_profile_with("p", &[(key, s("x"))]);
    assert!(
        matches!(result, Err(EditError::KeyPath(KeyPathError::TooLong))),
        "{result:?}"
    );
    assert_eq!(editor.to_toml(), "");
}

/// A NUL in a profile's `command` or `working_directory` cannot reach a
/// spawn: the loader drops the value with a warning naming the key.
#[test]
fn a_nul_in_a_profile_command_is_dropped_with_a_warning() {
    for key in ["command", "working_directory"] {
        let text = format!("[[profiles]]\nname = \"p\"\n{key} = \"/bin/sh\\u0000-x\"\n");
        let config = Config::from_toml_lenient(&text).unwrap();
        let profile = &config.config.profiles[0];
        assert_eq!(profile.command, None, "{key}");
        assert_eq!(profile.working_directory, None, "{key}");
        let path = format!("profiles[0].{key}");
        assert!(
            matches!(&config.warnings[..], [Warning::OutOfRange { path: p, .. }] if *p == path),
            "{key}: {:?}",
            config.warnings
        );
        // An edit that would write one is refused with that warning.
        let (_dir, _path, mut editor) = open("[[profiles]]\nname = \"p\"\n");
        assert!(
            matches!(editor.set(&path, s("a\0b")), Err(EditError::Refused { .. })),
            "{key}"
        );
    }
}

/// 21.0b m1: the probe for a new key in an unknown table compares paths
/// with every index zeroed, so a table in the second profile is found as
/// in the first.
#[test]
fn a_new_key_in_an_unknown_table_of_a_later_profile_is_refused() {
    let text = "[[profiles]]\nname = \"a\"\n\n[[profiles]]\nname = \"b\"\n[profiles.zzz]\nk = 1\n";
    let (_dir, _path, mut editor) = open(text);
    let result = editor.set("profiles[1].zzz.new", toml::Value::Integer(1));
    assert!(
        matches!(&result, Err(EditError::Refused { warnings, .. })
            if matches!(&warnings[..], [Warning::UnknownKey { path }] if path == "profiles[1].zzz")),
        "{result:?}"
    );
    assert_eq!(editor.to_toml(), text);
    editor
        .set("profiles[1].zzz.k", toml::Value::Integer(2))
        .unwrap();
}

/// The pid of a process that has exited and been reaped.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn temporary(dir: &Path, pid: u32, n: usize) -> PathBuf {
    let path = dir.join(format!(".config.toml.{pid}.{n}.edit"));
    std::fs::write(&path, "half an edit").unwrap();
    path
}

/// #324 item 6: a save removes the temporary files of this target that an
/// edit of a process no longer running left, and keeps one of a running
/// process (pid 1, which is always running) and its own target's file.
#[test]
fn a_save_removes_the_stale_temporary_files_of_a_crashed_edit() {
    let (dir, path, mut editor) = open("[general]\n");
    let stale = temporary(dir.path(), dead_pid(), 0);
    let running = temporary(dir.path(), 1, 0);
    editor
        .set("general.confirm_close", toml::Value::Boolean(false))
        .unwrap();
    assert!(editor.save().unwrap());
    assert!(!stale.exists());
    assert!(running.exists());
    assert!(path.exists());
    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [running.file_name().unwrap(), path.file_name().unwrap()]
    );
}

/// #526 24.10 M2: a cap belongs to its own array. Index 70 of a list with
/// no cap of its own is above the 64 of smart rules and is edited; index
/// 300 of another key is above the 256 of profiles and is not refused for
/// that cap.
#[test]
fn an_index_past_another_arrays_cap_is_edited() {
    let names: Vec<String> = (0..72).map(|i| format!("p{i}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let text = format!(
        "[plugins]\nenabled = {}\n",
        toml::Value::try_from(&names).unwrap()
    );
    let (_dir, path, mut editor) = open(&text);
    editor.set("plugins.enabled[70]", s("changed")).unwrap();
    assert!(editor.save().unwrap());
    let loaded = Config::from_toml_lenient(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(loaded.config.plugins.enabled[70], "changed");
    assert_eq!(loaded.config.plugins.enabled[71], "p71");

    // The same at a depth of one key, where the profile cap's row looks.
    let values: Vec<i64> = (0..301).collect();
    let text = format!("zzz = {}\n", toml::Value::try_from(&values).unwrap());
    let (_dir, _path, mut editor) = open(&text);
    let result = editor.set("zzz[300]", toml::Value::Integer(7));
    assert!(
        !matches!(&result, Err(EditError::Unsupported { reason, .. }) if reason.contains("cap")),
        "{result:?}"
    );
}

/// #526 24.10 M4: `replace` writes a rejected value's replacement as a
/// float where the setting is one, also when that replaces a list the
/// loader rejected, which `set` refuses.
#[test]
fn a_replace_writes_an_integer_as_the_float_the_setting_is() {
    let text = "[appearance]\nopacity = [\"x\"]\n";
    let (_dir, path, mut editor) = open(text);
    let refused = editor.set("appearance.opacity", toml::Value::Integer(1));
    assert!(
        matches!(&refused, Err(EditError::Invalid { .. })),
        "{refused:?}"
    );
    assert_eq!(editor.to_toml(), text);
    editor
        .replace("appearance.opacity", toml::Value::Integer(1))
        .unwrap();
    assert!(editor.save().unwrap());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "[appearance]\nopacity = 1.0\n"
    );
}

/// #526 24.10 M4: the file `replace` makes load must load without any
/// warning, since the failing file had none to keep: an unknown key
/// elsewhere in it refuses the replace, naming that key, and nothing
/// changes. (ADR 0040: pinned as it is, not endorsed by this test.)
#[test]
fn a_replace_is_refused_while_an_unrelated_key_is_unknown() {
    let text = "[appearance]\nopacity = [\"x\"]\nzzz = 1\n";
    let (_dir, path, mut editor) = open(text);
    let result = editor.replace("appearance.opacity", toml::Value::Float(0.5));
    assert!(
        matches!(&result, Err(EditError::Refused { warnings, .. })
            if matches!(&warnings[..], [Warning::UnknownKey { path }] if path == "appearance.zzz")),
        "{result:?}"
    );
    assert_eq!(editor.to_toml(), text);
    assert!(!editor.save().unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

/// #526 24.10 r2 m1: a key with no default holding a scalar is a rejected
/// value that the edit would make a list. `set` refuses that like any list
/// it would replace, with the loader's error; `replace` writes it. This
/// pins what the editor does; it does not make the scalar a repair.
#[test]
fn a_scalar_at_a_key_with_no_default_is_not_turned_into_a_list_by_a_set() {
    let text = "[selection]\nsmart_rules = \"x\"\n";
    let no_rules = || toml::Value::Array(Vec::new());
    let (_dir, path, mut editor) = open(text);
    let refused = editor.set("selection.smart_rules", no_rules());
    assert!(
        matches!(&refused, Err(EditError::Invalid { .. })),
        "{refused:?}"
    );
    assert_eq!(editor.to_toml(), text);
    assert!(!editor.save().unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    editor.replace("selection.smart_rules", no_rules()).unwrap();
    assert!(editor.save().unwrap());
    let loaded = Config::from_toml_lenient(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(loaded.warnings, []);
    assert_eq!(loaded.config.selection.smart_rules, Some(Vec::new()));
}

/// #526 24.10 M5: a new profile's name differs from the names as written
/// and as loaded. `"A\0"` loads as `"A\u{FFFD}"`, so that name takes a
/// number; so does `"A\0"` itself, and a name a failing file holds, which
/// only the raw half can see.
#[test]
fn a_new_profile_name_avoids_written_and_loaded_names() {
    let text = "[[profiles]]\nname = \"A\\u0000\"\n";
    let (_dir, path, mut editor) = open(text);
    assert_eq!(editor.unused_profile_name("A\u{FFFD}"), "A\u{FFFD} 2");
    assert_eq!(editor.unused_profile_name("A\0"), "A\0 2");
    assert_eq!(editor.unused_profile_name("A"), "A", "not a taken name");
    let added = editor.add_profile_with("A\u{FFFD}", &[]).unwrap();
    assert_eq!((added.index, added.name.as_str()), (1, "A\u{FFFD} 2"));
    assert!(editor.save().unwrap());
    let loaded = Config::from_toml_lenient(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let names: Vec<&str> = loaded.config.profiles.iter().map(|p| &*p.name).collect();
    assert_eq!(names, ["A\u{FFFD}", "A\u{FFFD} 2"]);

    // Beside no such name it is taken as given.
    let (_dir, _path, mut editor) = open("[[profiles]]\nname = \"B\"\n");
    let added = editor.add_profile_with("A\u{FFFD}", &[]).unwrap();
    assert_eq!(added.name, "A\u{FFFD}");

    // A file the loader fails on has only its written names.
    let failing = "[appearance]\nopacity = \"x\"\n[[profiles]]\nname = \"C\"\n";
    let (_dir, _path, editor) = open(failing);
    assert_eq!(editor.unused_profile_name("C"), "C 2");
}

/// `[plugins] enabled` as TOML with `count` ids.
fn plugin_list(count: usize) -> String {
    let ids: Vec<String> = (0..count).map(|i| format!("\"p.{i}\"")).collect();
    format!("[plugins]\nenabled = [{}]\n", ids.join(", "))
}

/// The loader's warnings about `plugins.enabled`.
fn plugin_warnings(warnings: &[Warning]) -> Vec<&Warning> {
    warnings
        .iter()
        .filter(|w| matches!(w, Warning::OutOfRange { path, .. } if path == "plugins.enabled"))
        .collect()
}

/// #560 (S5, S8): a `[plugins] enabled` past [`MAX_ENABLED_PLUGINS`] gets
/// one warning that names the cap and no id, however long the list; the
/// list is kept whole for the host and the editor. At the cap, none.
#[test]
fn a_plugin_list_past_the_cap_is_one_warning_and_kept_whole() {
    for count in [0, 1, MAX_ENABLED_PLUGINS - 1, MAX_ENABLED_PLUGINS] {
        let loaded = Config::from_toml_lenient(&plugin_list(count)).unwrap();
        assert_eq!(loaded.warnings, [], "{count}");
    }
    let mut seen = Vec::new();
    for count in [MAX_ENABLED_PLUGINS + 1, MAX_ENABLED_PLUGINS + 2, 100_000] {
        let loaded = Config::from_toml_lenient(&plugin_list(count)).unwrap();
        assert_eq!(loaded.config.plugins.enabled.len(), count);
        assert_eq!(loaded.warnings.len(), 1, "{count}: {:?}", loaded.warnings);
        let [warning] = plugin_warnings(&loaded.warnings)[..] else {
            panic!("{count}: {:?}", loaded.warnings);
        };
        let text = warning.to_string();
        assert!(
            text.contains(&format!("more than {MAX_ENABLED_PLUGINS} ids")),
            "{text}"
        );
        assert!(!text.contains("p."), "no id: {text}");
        seen.push(warning.clone());
    }
    // The same warning for any list past the cap, so an edit that keeps a
    // list past it adds no new one.
    assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
}

/// #560: the Plugins toggle's set of `plugins.enabled` that takes a list
/// at the cap past it is refused with the loader's warning; one that
/// shortens a list past the cap (a toggle turned off while the file lists
/// more than the host reads) is written, also when it stays past the cap.
#[test]
fn a_plugin_list_edit_is_refused_only_when_it_crosses_the_cap() {
    let names = |count: usize| -> Vec<String> { (0..count).map(|i| format!("p.{i}")).collect() };

    let (_dir, _path, mut editor) = open(&plugin_list(MAX_ENABLED_PLUGINS));
    let past = names(MAX_ENABLED_PLUGINS + 1);
    match editor.set("plugins.enabled", ids_value(&past)) {
        Err(EditError::Refused { warnings, .. }) => {
            assert_eq!(plugin_warnings(&warnings).len(), 1, "{warnings:?}");
        }
        other => panic!("{other:?}"),
    }

    for (from, to) in [
        (MAX_ENABLED_PLUGINS + 3, MAX_ENABLED_PLUGINS + 2),
        (MAX_ENABLED_PLUGINS + 1, MAX_ENABLED_PLUGINS),
    ] {
        let (_dir, path, mut editor) = open(&plugin_list(from));
        let shorter = names(from)[1..].to_vec();
        assert_eq!(shorter.len(), to);
        editor
            .set("plugins.enabled", ids_value(&shorter))
            .unwrap_or_else(|e| panic!("{from} -> {to}: {e:?}"));
        assert!(editor.save().unwrap());
        let saved = std::fs::read_to_string(&path).unwrap();
        let loaded = Config::from_toml_lenient(&saved).unwrap();
        assert_eq!(loaded.config.plugins.enabled, shorter);
    }
}

/// `ids` as a TOML array of strings.
fn ids_value(ids: &[String]) -> toml::Value {
    toml::Value::Array(ids.iter().map(|id| s(id)).collect())
}
