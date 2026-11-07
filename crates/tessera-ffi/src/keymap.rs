//! The key map, for the frontend's menus (#43, ADR 0036).

use tessera_app_core::command::{CommandId, CommandOwner, menu_shortcut};

use crate::app::App;
use crate::error::Error;
use crate::panic::ffi_guard;
use crate::selection::copy_out;

/// The key map in effect, as JSON (`App::keymap`): what the menus show.
fn keymap_json(app: &App) -> String {
    let map = app.core.keymap();
    let generation = app.core.config_generation();
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
///   (`on_config_applied`'s count; 0 for the one the app started with).
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
/// the two calls (a configuration applied); compare `generation`, or ask
/// with room to spare.
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
