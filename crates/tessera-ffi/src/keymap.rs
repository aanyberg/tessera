//! The key map, for the frontend's menus (#43, ADR 0036).

use tessera_app_core::command::{CommandId, CommandOwner, EntryValue, menu_shortcut};

use crate::app::App;
use crate::error::Error;
use crate::panic::ffi_guard;
use crate::selection::copy_out;

/// The key map in effect, as JSON (`App::keymap`): what the menus show,
/// and every `[keys]` entry for the Keys pane. The map and its generation
/// are read under one lock (`App::keymap_and_generation`), so `generation`
/// always names the configuration the chords and entries came from (#317);
/// the entries are the map's own, never read from the configuration again
/// (#355 K2).
fn keymap_json(app: &App) -> String {
    let (map, generation) = app.core.keymap_and_generation();
    let commands: Vec<serde_json::Value> = CommandId::all()
        .map(|id| {
            let spec = id.spec();
            let chords = map.chords(id);
            let menu: Vec<serde_json::Value> = chords
                .iter()
                .filter_map(|&chord| menu_shortcut(chord))
                .map(|shortcut| {
                    serde_json::json!({
                        "key_equivalent": shortcut.key_equivalent.to_string(),
                        "modifiers": shortcut.modifiers.bits(),
                    })
                })
                .collect();
            let owner = match spec.owner {
                CommandOwner::System => "system",
                CommandOwner::App => "app",
                _ => "planned",
            };
            serde_json::json!({
                "id": id.index(),
                "command": id.to_string(),
                "name": spec.name,
                "parameter": spec.parameter,
                "owner": owner,
                "chords": chords.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "menu": menu,
            })
        })
        .collect();
    let entries: Vec<serde_json::Value> = map
        .entries()
        .map(|entry| {
            let value = match entry.value {
                EntryValue::Text(text) => serde_json::Value::String(text),
                EntryValue::SendText(text) => serde_json::json!({ "send_text": text }),
                _ => serde_json::Value::Null,
            };
            serde_json::json!({
                "index": entry.index,
                "key": entry.key,
                "chord": entry.chord.map(|chord| chord.to_string()),
                "value": value,
                "effect": entry.effect.as_str(),
                "warning": entry.warning.map(ToString::to_string),
            })
        })
        .collect();
    let warnings: Vec<String> = map
        .warnings()
        .iter()
        .chain(map.overflow())
        .map(ToString::to_string)
        .collect();
    serde_json::json!({
        "generation": generation,
        "commands": commands,
        "entries": entries,
        "warnings": warnings,
    })
    .to_string()
}

/// Copies the key map in effect as UTF-8 JSON, for the menus' key
/// equivalents: the default chords with `[keys]` laid over them. Read it
/// at start and again on every `on_config_applied`.
///
/// ```json
/// {"generation": 0,
///  "commands": [{"id": 7, "command": "new_tab", "name": "new_tab",
///                "parameter": null, "owner": "app", "chords": ["cmd+t"],
///                "menu": [{"key_equivalent": "t", "modifiers": 8}]}, ...],
///  "entries": [{"index": 0, "key": "shift+cmd+u", "chord": "cmd+shift+u",
///               "value": "new_tab", "effect": "bound", "warning": null},
///              {"index": 1, "key": "cmd+e", "chord": "cmd+e",
///               "value": {"send_text": "\\e[A"}, "effect": "bound",
///               "warning": null},
///              {"index": 2, "key": "d+cmd", "chord": null, "value": "copy",
///               "effect": "refused",
///               "warning": "`keys.\"d+cmd\"`: unknown key `d`; ignored"}],
///  "warnings": ["`keys.\"cmd+q\"`: shadows the menu item of `quit`, ..."]}
/// ```
///
/// * `generation`: the configurations applied before this map
///   (`on_config_applied`'s count; 0 for the one the app started with),
///   read with the map: the chords are always those of this generation.
/// * `commands`: every command, in id order (the id
///   `tessera_session_handle_key` reports); `command` is the name as
///   written in `[keys]` (`select_tab:3`); `owner` is `system` (an `AppKit`
///   menu item: Quit, Hide, Minimize), `app`, or `planned` (a command a
///   later version runs; bindable, does nothing yet).
/// * `chords`: the chords bound to it, canonical, the user's first.
/// * `menu`: the chords a menu item can show, as `keyEquivalent` (one
///   character; a function key is its `NS…FunctionKey` character) and
///   modifiers (bits: Shift 1, Option 2, Control 4, Command 8). The first
///   is the item's own; the others are hidden alternates. Chords without
///   Command never go on a menu (they would take the key from the shell),
///   nor keypad keys.
/// * `entries`: the first 512 `[keys]` entries in file order, refused ones
///   included (the Keys pane's list). `index` is the entry's place in the
///   table from 0; `key` the table key as written; `chord` its canonical
///   form (as in `commands[].chords`), `null` when the key does not parse;
///   `value` the value as written: a string (a command such as
///   `select_tab:3`, `unbound`, `none`, or a value that does not parse),
///   `{"send_text": "<the text after send_text:, escapes as written>"}`,
///   or `null` when it is not a string. `effect` is `bound` (it binds its
///   chord), `unbound` (it removed the chord's defaults), `shadowed` (an
///   earlier entry for the same chord took effect: the first wins) or
///   `refused` (a chord or value that does not parse, an unknown command,
///   an invisible character); `warning` is the entry's warning (why it was
///   left out, or the caveat it is honoured with), or `null`. Every text
///   is escaped as warnings are (control, bidi, zero-width and line
///   separator characters as `\u{..}`); `key` is cut at 256 bytes and
///   `value` at 4106, with a marker. Chords are canonicalised here, never
///   by the frontend.
/// * `warnings`: why `[keys]` entries were left out or honoured with a
///   caveat (escaped, bounded), for a settings or log view, and last the
///   one for the entries past the first 512, if any; the core has logged
///   them already.
///
/// Stores the JSON's length in bytes in `*len` and writes it (no NUL
/// terminator) to `buf` when `cap` holds all of it. Returns
/// `TESSERA_ERROR_INVALID_ARGUMENT` when it is longer than `cap`: nothing
/// is written, and `*len` holds the size to call again with (`buf` may be
/// `NULL` with `cap` 0 to ask for it). Returns `TESSERA_ERROR_NULL_POINTER`
/// if `app` or `len` is `NULL`, or `buf` is `NULL` with `cap` too small;
/// `*len` is then 0 (when `len` is not `NULL`). The map can change between
/// the two calls (a configuration applied), so the second may need more
/// room: ask with room to spare. Each JSON is consistent in itself.
///
/// Any thread.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `len` must be `NULL` or point to
/// a writable `size_t`. `buf` must be `NULL` or valid for `cap` bytes of
/// writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_app_keymap_json(
    app: *const App,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> Error {
    ffi_guard!("tessera_app_keymap_json", {
        if len.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { len.write(0) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        // SAFETY: `len` and `buf` per the contract.
        unsafe { copy_out(&keymap_json(app), buf, cap, len) }
    })
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tessera_app_core::{App as CoreApp, Config, ConfigPaths};

    use super::*;

    /// An app whose configuration is `toml`.
    fn app_with(toml: &str) -> App {
        let loaded = Config::from_toml_lenient(toml).unwrap();
        App {
            core: CoreApp::new(loaded.config, ConfigPaths::new("/nonexistent/tessera")),
        }
    }

    /// The key map JSON of `app`, parsed.
    fn json_of(app: &App) -> serde_json::Value {
        serde_json::from_str(&keymap_json(app)).unwrap()
    }

    /// The chords of `command` in `json`.
    fn chords_of<'a>(json: &'a serde_json::Value, command: &str) -> &'a serde_json::Value {
        &json["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["command"] == command)
            .unwrap()["chords"]
    }

    /// #355 Done when 1: every entry in file order, refused ones included,
    /// with its canonical chord, value, effect and warning; a bound
    /// command's chord is the one `commands[].chords` lists.
    #[test]
    #[expect(clippy::too_many_lines, reason = "one row per kind of entry")]
    fn entries_list_every_key_in_file_order_with_its_effect() {
        let long = format!("cmd+{}", "a".repeat(70));
        let toml = format!(
            "[keys]\n\
             'shift+cmd+u' = 'new_tab'\n\
             'opt+cmd+j' = 'new_window'\n\
             'command+K' = 'copy'\n\
             'control+c' = 'copy'\n\
             'cmd+d' = 'split_down'\n\
             'super+d' = 'split_right'\n\
             'd+cmd' = 'copy'\n\
             'cmd+j' = 'bogus'\n\
             '{long}' = 'copy'\n\
             \"cmd+\\u200D\" = 'copy'\n\
             'cmd+e' = \"send_text:\\\\e[A\\u0007\"\n\
             'cmd+i' = 'none'\n\
             'command+shift+u' = 'unbound'\n\
             'cmd+b' = 'unbound'\n"
        );
        let json = json_of(&app_with(&toml));
        let row = |index: usize,
                   key: &str,
                   chord: Option<&str>,
                   value: serde_json::Value,
                   effect: &str,
                   warning: Option<&str>| {
            serde_json::json!({
                "index": index, "key": key, "chord": chord, "value": value,
                "effect": effect,
                "warning": warning.map(|reason| format!("`keys.\"{key}\"`: {reason}")),
            })
        };
        let text = |text: &str| serde_json::json!(text);
        let expected = serde_json::json!([
            row(
                0,
                "shift+cmd+u",
                Some("cmd+shift+u"),
                text("new_tab"),
                "bound",
                None
            ),
            row(
                1,
                "opt+cmd+j",
                Some("cmd+alt+j"),
                text("new_window"),
                "bound",
                None
            ),
            row(
                2,
                "command+K",
                None,
                text("copy"),
                "refused",
                Some("uppercase key `K` (write shift+ and the lowercase key); ignored")
            ),
            row(3, "control+c", Some("ctrl+c"), text("copy"), "bound", None),
            row(4, "cmd+d", Some("cmd+d"), text("split_down"), "bound", None),
            row(
                5,
                "super+d",
                Some("cmd+d"),
                text("split_right"),
                "shadowed",
                Some("same chord as `cmd+d` (the first entry wins); ignored")
            ),
            row(
                6,
                "d+cmd",
                None,
                text("copy"),
                "refused",
                Some("unknown key `d`; ignored")
            ),
            row(
                7,
                "cmd+j",
                Some("cmd+j"),
                text("bogus"),
                "refused",
                Some("unknown command `bogus`; ignored")
            ),
            row(
                8,
                &long,
                None,
                text("copy"),
                "refused",
                Some("chord longer than 64 bytes; ignored")
            ),
            row(
                9,
                "cmd+\\u{200d}",
                None,
                text("copy"),
                "refused",
                Some(
                    "invisible key `\\u{200d}` (a bidi, zero-width or other default-ignorable character); ignored"
                )
            ),
            row(
                10,
                "cmd+e",
                Some("cmd+e"),
                serde_json::json!({"send_text": "\\e[A\\u{7}"}),
                "bound",
                Some(
                    "shadows the menu item of `use_selection_for_find`, which loses this chord; honoured"
                )
            ),
            row(11, "cmd+i", Some("cmd+i"), text("none"), "bound", None),
            row(
                12,
                "command+shift+u",
                Some("cmd+shift+u"),
                text("unbound"),
                "shadowed",
                Some("same chord as `shift+cmd+u` (the first entry wins); ignored")
            ),
            row(13, "cmd+b", Some("cmd+b"), text("unbound"), "unbound", None),
        ]);
        let entries = &json["entries"];
        for (index, (got, want)) in entries
            .as_array()
            .unwrap()
            .iter()
            .zip(expected.as_array().unwrap())
            .enumerate()
        {
            assert_eq!(got, want, "entry {index}");
        }
        assert_eq!(entries, &expected);
        for (command, chord) in [
            ("new_tab", "cmd+shift+u"),
            ("new_window", "cmd+alt+j"),
            ("copy", "ctrl+c"),
            ("split_down", "cmd+d"),
        ] {
            assert_eq!(chords_of(&json, command)[0], chord, "{command}");
        }
        assert!(
            !chords_of(&json, "split_right")
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("cmd+d")),
            "the shadowed entry claimed no chord"
        );
    }

    /// #355 Done when 3 (S3, S5): 600 entries export the first 512 and one
    /// overflow warning; a 4096-byte `send_text` and a key with bidi
    /// controls and U+2028 come out escaped and capped; and 512 entries of
    /// the largest kind stay bounded (the size is recorded in ADR 0036).
    #[test]
    fn entries_are_capped_and_escaped() {
        let mut toml = String::from("[keys]\n");
        for i in 0..600 {
            let _ = writeln!(toml, "'k{i}' = 'copy'");
        }
        let json = json_of(&app_with(&toml));
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 512);
        assert_eq!(entries[511]["key"], "k511");
        assert_eq!(entries[511]["index"], 511);
        let warnings = json["warnings"].as_array().unwrap();
        assert_eq!(
            warnings.last().unwrap(),
            "88 `[keys]` entries past the first 512 ignored"
        );
        assert_eq!(
            warnings
                .iter()
                .filter(|w| w.as_str().unwrap().contains("past the first"))
                .count(),
            1
        );

        let plain = "x".repeat(4096);
        let bells = "\\u0007".repeat(4096);
        let toml = format!(
            "[keys]\n'cmd+e' = 'send_text:{plain}'\n'cmd+g' = \"send_text:{bells}\"\n\
             \"cmd+\\u202Ex\\u2028\" = 'copy'\n'cmd+k' = \"\\u202Ecopy\\u2029\"\n"
        );
        let json = json_of(&app_with(&toml));
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries[0]["effect"], "bound");
        assert_eq!(
            entries[0]["value"]["send_text"],
            plain.as_str(),
            "fits whole"
        );
        assert_eq!(entries[1]["effect"], "bound", "4096 bytes after unescaping");
        let bells_out = entries[1]["value"]["send_text"].as_str().unwrap();
        assert!(
            bells_out.starts_with("\\u{7}\\u{7}")
                && !bells_out.contains('\u{7}')
                && bells_out.ends_with("… (truncated)")
                && bells_out.len() <= tessera_config::MAX_ENTRY_VALUE_TEXT_LEN,
            "{} bytes",
            bells_out.len()
        );
        assert_eq!(entries[2]["key"], "cmd+\\u{202e}x\\u{2028}");
        assert_eq!(entries[2]["effect"], "refused");
        assert_eq!(entries[3]["value"], "\\u{202e}copy\\u{2029}");
        assert_eq!(entries[3]["chord"], "cmd+k");
        let text = json.to_string();
        for c in ['\u{202E}', '\u{2028}', '\u{2029}', '\u{7}'] {
            assert!(!text.contains(c), "U+{:04X} in the JSON", u32::from(c));
        }

        // The largest entries: refused, so key and value are kept escaped
        // at their caps, of backslashes, which JSON doubles, with reasons
        // that quote the key.
        let key = "\\".repeat(1000);
        let value = "\\".repeat(10_000);
        let mut toml = String::from("[keys]\n");
        for i in 0..512 {
            let _ = writeln!(toml, "'{i}+{key}' = '{value}'");
        }
        let app = app_with(&toml);
        let json = keymap_json(&app);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["entries"].as_array().unwrap().len(), 512);
        let entry = &parsed["entries"][0];
        assert!(entry["key"].as_str().unwrap().len() <= tessera_config::MAX_WARNING_PATH_LEN);
        assert!(entry["value"].as_str().unwrap().len() <= tessera_config::MAX_ENTRY_VALUE_TEXT_LEN);
        let defaults = keymap_json(&app_with(""));
        // 4.78 MB measured (ADR 0036, #355 amendment).
        assert!(json.len() < 6 << 20, "{} bytes", json.len());
        eprintln!(
            "key map JSON: {} bytes for 512 entries at their caps, {} without [keys]",
            json.len(),
            defaults.len()
        );
    }

    /// An apply that changes only refused entries recompiles the map, so
    /// the entries are the new file's (`Bindings::applied`).
    #[test]
    fn a_change_of_refused_entries_alone_reaches_the_json() {
        // Entries the loader refuses (an unknown key, a value that is not
        // a string) are no binding: only `Keys::entries` changes.
        let app = app_with("[keys]\n'cmd+u' = 'new_tab'\n'cmd+nokey' = 'copy'\n");
        let theme = tessera_config::Theme::default();
        for (toml, value) in [
            (
                "[keys]\n'cmd+u' = 'new_tab'\n'cmd+other' = 'copy'\n",
                serde_json::json!("copy"),
            ),
            (
                "[keys]\n'cmd+u' = 'new_tab'\n'cmd+other' = 3\n",
                serde_json::Value::Null,
            ),
        ] {
            let config = Config::from_toml_lenient(toml).unwrap().config;
            app.core.apply_config(&config, &theme);
            let json = json_of(&app);
            assert_eq!(entry_keys(&json), ["cmd+u", "cmd+other"]);
            assert_eq!(json["entries"][1]["effect"], "refused");
            assert_eq!(json["entries"][1]["value"], value);
        }
    }

    /// The `entries` keys of `json`.
    fn entry_keys(json: &serde_json::Value) -> Vec<&str> {
        json["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["key"].as_str().unwrap())
            .collect()
    }

    /// #348 m1, staged, and #355 K2: an apply started between the map and
    /// the generation cannot finish there (the combined read holds the key
    /// map's lock), so each JSON's chords and entries are those of its
    /// `generation`: one apply, never two. Read as `keymap()` and then
    /// `config_generation()`, or with the entries from a second read of
    /// the configuration, the apply lands in between. 1000 rounds, each
    /// with an apply racing the read; odd generations bind `cmd+u`.
    #[test]
    fn an_apply_cannot_land_between_the_chords_and_the_generation() {
        const ROUNDS: u64 = 1000;
        let app = Arc::new(App {
            core: CoreApp::new(Config::default(), ConfigPaths::new("/nonexistent/tessera")),
        });
        let (start, started) = std::sync::mpsc::channel::<()>();
        let applier = {
            let app = Arc::clone(&app);
            std::thread::spawn(move || {
                let bound = Config::from_toml_lenient("[keys]\n'cmd+u' = 'new_tab'\n")
                    .unwrap()
                    .config;
                let defaults = Config::default();
                let theme = tessera_config::Theme::default();
                for generation in 1..=ROUNDS {
                    started.recv().unwrap();
                    let config = if generation % 2 == 1 {
                        &bound
                    } else {
                        &defaults
                    };
                    app.core.apply_config(config, &theme);
                }
            })
        };
        for round in 0..ROUNDS {
            let start = start.clone();
            let watched = Arc::clone(&app);
            CoreApp::pause_between_reads_for_test(move || {
                start.send(()).unwrap();
                // Long enough for an unblocked apply in most rounds even
                // under load; a blocked one never finishes here.
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2);
                while watched.core.config_generation() == round
                    && std::time::Instant::now() < deadline
                {
                    std::thread::yield_now();
                }
            });
            let json = json_of(&app);
            let generation = json["generation"].as_u64().unwrap();
            let (chords, keys) = if generation % 2 == 1 {
                (serde_json::json!(["cmd+u", "cmd+t"]), vec!["cmd+u"])
            } else {
                (serde_json::json!(["cmd+t"]), vec![])
            };
            assert_eq!(
                (generation, chords_of(&json, "new_tab"), entry_keys(&json)),
                (round, &chords, keys),
                "an apply landed between the reads in round {round}"
            );
            // This round's apply, which the read held off, lands now.
            while app.core.config_generation() == round {
                std::thread::yield_now();
            }
        }
        applier.join().unwrap();
        assert_eq!(app.core.config_generation(), ROUNDS);
    }

    /// M4 (#317): under a storm of applies alternating between two
    /// `[keys]` tables, every JSON's chords and entries are those of its generation
    /// (odd: `cmd+u` opens a tab too; even: the defaults). Reading the map
    /// and the generation one after the other tore 2 reads in 357,861, too
    /// rarely to catch that revert: the staged test above does (#348 m1).
    #[test]
    fn the_generation_and_the_chords_never_disagree() {
        const APPLIES: u64 = 20_000;
        let app = Arc::new(App {
            core: CoreApp::new(Config::default(), ConfigPaths::new("/nonexistent/tessera")),
        });
        let bound = Config::from_toml_lenient("[keys]\n'cmd+u' = 'new_tab'\n")
            .unwrap()
            .config;
        let defaults = Config::default();
        let theme = tessera_config::Theme::default();
        let done = Arc::new(AtomicBool::new(false));
        let applier = {
            let app = Arc::clone(&app);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                for generation in 1..=APPLIES {
                    let config = if generation % 2 == 1 {
                        &bound
                    } else {
                        &defaults
                    };
                    app.core.apply_config(config, &theme);
                }
                done.store(true, Ordering::Release);
            })
        };
        let (mut reads, mut torn, mut seen_odd, mut seen_even) = (0_u64, 0_u64, false, false);
        while !done.load(Ordering::Acquire) {
            let json: serde_json::Value = serde_json::from_str(&keymap_json(&app)).unwrap();
            let generation = json["generation"].as_u64().unwrap();
            let new_tab = json["commands"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["command"] == "new_tab")
                .unwrap();
            let (expected, keys) = if generation % 2 == 1 {
                seen_odd = true;
                (serde_json::json!(["cmd+u", "cmd+t"]), vec!["cmd+u"])
            } else {
                seen_even = true;
                (serde_json::json!(["cmd+t"]), vec![])
            };
            torn += u64::from(new_tab["chords"] != expected || entry_keys(&json) != keys);
            reads += 1;
        }
        applier.join().unwrap();
        assert_eq!(app.core.config_generation(), APPLIES);
        assert_eq!(torn, 0, "{torn} torn reads in {reads}");
        assert!(
            reads > 1 && seen_odd && seen_even,
            "{reads} reads overlapped the applies"
        );
    }
}
