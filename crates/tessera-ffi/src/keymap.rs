//! The key map, for the frontend's menus (#43, ADR 0036).

use tessera_app_core::command::{CommandId, CommandOwner, menu_shortcut};

use crate::app::App;
use crate::error::Error;
use crate::panic::ffi_guard;
use crate::selection::copy_out;

/// The key map in effect, as JSON (`App::keymap`): what the menus show.
/// The map and its generation are read under one lock
/// (`App::keymap_and_generation`), so `generation` always names the
/// configuration the chords came from (#317).
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
    let warnings: Vec<String> = map.warnings().iter().map(ToString::to_string).collect();
    serde_json::json!({
        "generation": generation,
        "commands": commands,
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
/// * `warnings`: why `[keys]` entries were left out or honoured with a
///   caveat (escaped, bounded), for a settings or log view; the core has
///   logged them already.
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tessera_app_core::{App as CoreApp, Config, ConfigPaths};

    use super::*;

    /// #348 m1, staged: an apply started between the chords and the
    /// generation cannot finish there (the combined read holds the key
    /// map's lock), so the JSON is the old chords with the old generation.
    /// Read as `keymap()` and then `config_generation()`, the apply lands
    /// in between: the old chords with the new generation.
    #[test]
    fn an_apply_cannot_land_between_the_chords_and_the_generation() {
        let app = Arc::new(App {
            core: CoreApp::new(Config::default(), ConfigPaths::new("/nonexistent/tessera")),
        });
        let (start, started) = std::sync::mpsc::channel::<()>();
        let (finish, finished) = std::sync::mpsc::channel::<()>();
        let applier = {
            let app = Arc::clone(&app);
            std::thread::spawn(move || {
                let bound = Config::from_toml_lenient("[keys]\n'cmd+u' = 'new_tab'\n")
                    .unwrap()
                    .config;
                started.recv().unwrap();
                app.core
                    .apply_config(&bound, &tessera_config::Theme::default());
                // The reader stopped listening if the apply had to wait.
                let _ = finish.send(());
            })
        };
        CoreApp::pause_between_reads_for_test(move || {
            start.send(()).unwrap();
            // Long enough for an unblocked apply even under load; a
            // blocked one never finishes here.
            let _ = finished.recv_timeout(std::time::Duration::from_millis(500));
        });
        let json: serde_json::Value = serde_json::from_str(&keymap_json(&app)).unwrap();
        applier.join().unwrap();
        let new_tab = json["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["command"] == "new_tab")
            .unwrap();
        assert_eq!(
            (json["generation"].as_u64().unwrap(), &new_tab["chords"]),
            (0, &serde_json::json!(["cmd+t"])),
            "an apply landed between the reads"
        );
        assert_eq!(app.core.config_generation(), 1);
    }

    /// M4 (#317): under a storm of applies alternating between two
    /// `[keys]` tables, every JSON's chords are those of its generation
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
            let expected = if generation % 2 == 1 {
                seen_odd = true;
                serde_json::json!(["cmd+u", "cmd+t"])
            } else {
                seen_even = true;
                serde_json::json!(["cmd+t"])
            };
            torn += u64::from(new_tab["chords"] != expected);
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
