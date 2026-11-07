//! Settings write-back for the Preferences window (#40, ADR 0040): the
//! configuration in effect as JSON, edits of `config.toml` by key path,
//! and the theme list and import.
//!
//! An edit goes through [`ConfigEditor`]: comments and layout survive, a
//! value the loader would warn about is refused, the write is atomic and
//! an external edit since the file was read is never overwritten. The
//! running configuration watch then applies the saved file as it applies a
//! hand edit (`on_config_applied`), so a frontend shows the result when
//! that callback arrives, not when the call returns.
//!
//! Logs name key paths only, never a value (S8).

use std::path::{Path, PathBuf};
use std::slice;

use tessera_app_core::WatchOptions;
use tessera_app_core::command::Keymap;
use tessera_app_core::edit::{
    ConfigEditor, EditError, MAX_KEY_PATH_LEN, ThemeOrigin, TomlValue, import_theme, theme_entries,
};

use crate::app::App;
use crate::error::Error;
use crate::panic::ffi_guard;
use crate::selection::copy_out;

/// Longest value JSON `tessera_config_set` takes, in bytes (64 KiB): far
/// more than any setting needs, small enough to parse on the UI thread.
pub const MAX_CONFIG_VALUE_LEN: usize = 64 * 1024;

/// Longest path `tessera_theme_import` takes, in bytes (macOS `PATH_MAX`).
pub const MAX_THEME_PATH_LEN: usize = 1024;

/// Smallest `id_cap` `tessera_theme_import` takes: the longest theme id
/// (a slug) in bytes.
pub const THEME_ID_LEN: usize = 64;

const _: () = assert!(THEME_ID_LEN == tessera_app_core::edit::MAX_THEME_ID_LEN);

/// Borrows `len` bytes at `ptr` (`NULL` allowed when `len` is 0).
///
/// # Safety
///
/// `ptr` must be `NULL` or valid for `len` bytes of reads.
unsafe fn input<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if ptr.is_null() {
        return None;
    }
    // SAFETY: non-null and valid for `len` bytes per the contract.
    Some(unsafe { slice::from_raw_parts(ptr, len) })
}

/// Writes as much of `text` as fits in `cap` bytes to `buf`, cut on a
/// character boundary, and its length to `*len`; nothing when `len` is
/// `NULL` (the message is optional), and length 0 when `buf` is `NULL`.
///
/// # Safety
///
/// `len` must be `NULL` or writable; `buf` must be `NULL` or valid for
/// `cap` bytes of writes.
unsafe fn message_out(text: &str, buf: *mut u8, cap: usize, len: *mut usize) {
    if len.is_null() {
        return;
    }
    let mut end = if buf.is_null() {
        0
    } else {
        text.len().min(cap)
    };
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end > 0 {
        // SAFETY: `buf` holds `cap >= end` bytes per the contract.
        unsafe { slice::from_raw_parts_mut(buf, end) }.copy_from_slice(&text.as_bytes()[..end]);
    }
    // SAFETY: non-null and writable per the contract.
    unsafe { len.write(end) };
}

/// The directory of `config.toml`'s themes.
fn themes_dir(config_file: &Path) -> PathBuf {
    config_file
        .parent()
        .map_or_else(|| PathBuf::from("themes"), |dir| dir.join("themes"))
}

/// A failed edit: its status code and the message for the caller.
struct Failure {
    code: Error,
    message: String,
}

impl Failure {
    fn new(code: Error, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Maps an edit error to its code, logging its kind and the key path only
/// (the message may hold the value; it goes to the caller, not the log).
fn failure(function: &'static str, key: &str, error: &EditError) -> Failure {
    let (code, kind) = match error {
        EditError::KeyPath(_) => (Error::InvalidArgument, "malformed key path"),
        EditError::Unsupported { .. } => (Error::InvalidArgument, "unsupported edit"),
        EditError::TooLarge => (Error::InvalidArgument, "edit past the size cap"),
        EditError::Refused { .. } => (Error::Refused, "refused by the loader"),
        EditError::Invalid { .. } => (Error::Refused, "rejected by the loader"),
        EditError::Conflict => (Error::Conflict, "config.toml changed on disk"),
        EditError::ThemeExists { .. } => (Error::Conflict, "the theme file exists"),
        EditError::File(_) => (Error::Io, "file error"),
        _ => (Error::Io, "edit failed"),
    };
    // The key path only when it parses (then it displays escaped).
    if let Ok(path) = key.parse::<tessera_app_core::edit::KeyPath>() {
        tracing::info!(function, %path, kind, "config edit not saved");
    } else {
        tracing::info!(function, kind, "config edit not saved");
    }
    Failure::new(code, error.to_string())
}

/// What an edit does to the opened document.
enum Change {
    Set(TomlValue),
    Remove,
}

/// Opens `config.toml` (creating it and starting the watch when needed),
/// applies `change` at `key`, checks the key map, and saves.
fn edit(function: &'static str, app: &App, key: &str, change: Change) -> Result<(), Failure> {
    let path = app
        .core
        .open_config(WatchOptions::default())
        .map_err(|error| {
            tracing::warn!(function, %error, "cannot create the configuration");
            Failure::new(Error::Io, error.to_string())
        })?;
    let mut editor = ConfigEditor::open(&path).map_err(|e| failure(function, key, &e))?;
    // The key map is compiled quietly: its warnings are compared, not
    // logged (the watch logs them when it applies the file).
    let quiet = |config: Option<&tessera_app_core::Config>| -> Vec<String> {
        tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
            config.map_or_else(Vec::new, |config| {
                Keymap::compile(&config.keys)
                    .warnings()
                    .iter()
                    .map(ToString::to_string)
                    .collect()
            })
        })
    };
    let mut before = quiet(editor.config());
    let result = match change {
        Change::Set(value) => editor.set(key, value),
        Change::Remove => editor.remove(key).map(|_| ()),
    };
    result.map_err(|e| failure(function, key, &e))?;
    let new: Vec<String> = quiet(editor.config())
        .into_iter()
        .filter(|warning| match before.iter().position(|w| w == warning) {
            Some(at) => {
                before.swap_remove(at);
                false
            }
            None => true,
        })
        .collect();
    if !new.is_empty() {
        let path = key.parse::<tessera_app_core::edit::KeyPath>();
        if let Ok(path) = path {
            tracing::info!(function, %path, kind = "refused by the key map", "config edit not saved");
        }
        return Err(Failure::new(
            Error::Refused,
            format!("`{key}` refused: {}", new.join("; ")),
        ));
    }
    editor.save().map_err(|e| failure(function, key, &e))?;
    Ok(())
}

/// Runs `edit` for an export, with its arguments checked, and reports the
/// result.
///
/// # Safety
///
/// As for `tessera_config_set`.
#[expect(clippy::too_many_arguments, reason = "the C signature")]
unsafe fn edit_export(
    function: &'static str,
    app: *mut App,
    key: *const u8,
    key_len: usize,
    value: Option<(*const u8, usize)>,
    msg: *mut u8,
    msg_cap: usize,
    msg_len: *mut usize,
) -> Error {
    // SAFETY: `msg_len` and `msg` per the contract.
    unsafe { message_out("", msg, msg_cap, msg_len) };
    let outcome = (|| {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Err(Failure::new(Error::NullPointer, "app is NULL"));
        };
        if key_len > MAX_KEY_PATH_LEN {
            return Err(Failure::new(Error::InvalidArgument, "key path too long"));
        }
        // SAFETY: `key` is NULL or valid for `key_len` bytes.
        let Some(key) = (unsafe { input(key, key_len) }) else {
            return Err(Failure::new(Error::NullPointer, "key is NULL"));
        };
        let Ok(key) = std::str::from_utf8(key) else {
            return Err(Failure::new(
                Error::InvalidArgument,
                "key path is not UTF-8",
            ));
        };
        let change = match value {
            None => Change::Remove,
            Some((value, value_len)) => {
                if value_len > MAX_CONFIG_VALUE_LEN {
                    return Err(Failure::new(Error::InvalidArgument, "value too long"));
                }
                // SAFETY: `value` is NULL or valid for `value_len` bytes.
                let Some(value) = (unsafe { input(value, value_len) }) else {
                    return Err(Failure::new(Error::NullPointer, "value is NULL"));
                };
                // JSON `null` and numbers past `i64` have no TOML form.
                match serde_json::from_slice::<TomlValue>(value) {
                    Ok(value) => Change::Set(value),
                    Err(error) => {
                        return Err(Failure::new(
                            Error::InvalidArgument,
                            format!("the value is not a TOML value as JSON: {error}"),
                        ));
                    }
                }
            }
        };
        edit(function, app, key, change)
    })();
    match outcome {
        Ok(()) => Error::Ok,
        Err(failure) => {
            // SAFETY: as above.
            unsafe { message_out(&failure.message, msg, msg_cap, msg_len) };
            failure.code
        }
    }
}

/// Copies the configuration in effect (the one the views use: the file as
/// last applied, defaults filled in) as UTF-8 JSON, for a settings window:
///
/// ```json
/// {"generation": 3,
///  "config": {"general": {...}, "appearance": {"theme": "Nord",
///             "opacity": 1.0, ...}, "terminal": {...}, "keys": {...},
///             "profiles": [...], ...}}
/// ```
///
/// `generation` is `on_config_applied`'s count for this configuration (0
/// for the one the app started with); `config` has every key the
/// configuration file documents, under the same names. Read it at start
/// and again on every `on_config_applied`.
///
/// Buffer handling as for `tessera_app_keymap_json`: `*len` gets the
/// length; `TESSERA_ERROR_INVALID_ARGUMENT` when `cap` is too small, with
/// nothing written; `TESSERA_ERROR_NULL_POINTER` for a `NULL` `app` or
/// `len`.
///
/// Any thread.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `len` must be `NULL` or point to
/// a writable `size_t`. `buf` must be `NULL` or valid for `cap` bytes of
/// writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_config_json(
    app: *const App,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> Error {
    ffi_guard!("tessera_config_json", {
        if len.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { len.write(0) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        let generation = app.core.config_generation();
        let config = app.core.config();
        let json = serde_json::json!({
            "generation": generation,
            "config": &*config,
        })
        .to_string();
        // SAFETY: `len` and `buf` per the contract.
        unsafe { copy_out(&json, buf, cap, len) }
    })
}

/// Sets the setting at the key path `key` (UTF-8, `key_len` bytes, at
/// most 1024) of `config.toml` to the JSON value `value_json` (`value_len`
/// bytes, at most `TESSERA_MAX_CONFIG_VALUE_LEN`) and saves the file.
///
/// The key path is keys joined by `.`, bare (`A-Za-z0-9_-`) or in double
/// quotes, each optionally followed by `[N]`: `appearance.opacity`,
/// `profiles[1].appearance.theme`, `keys."cmd+d"`. An index one past the
/// last profile adds a profile (`profiles[2].name` with two profiles).
/// The value is a JSON string, number, boolean or array (`"Nord"`, `0.9`,
/// `true`); `null` and objects are refused.
///
/// The configuration directory and a default `config.toml` are created
/// when missing, and the watch started (`tessera_app_create_config`).
/// Every comment and the layout of the rest of the file stay; the write is
/// atomic, a symlinked file stays a symlink and the file mode is kept. The
/// configuration watch applies the saved file (`on_config_applied`).
///
/// Returns:
///
/// * `TESSERA_ERROR_REFUSED` when the configuration loader would warn
///   about the result (an unknown key, a value out of range or of the
///   wrong type, a theme that does not exist, a key binding it would drop
///   or honour with a caveat): nothing is written;
/// * `TESSERA_ERROR_CONFLICT` when `config.toml` changed on disk while the
///   edit was made (an editor saved it): nothing is written, the other
///   change stays; read the configuration again and retry;
/// * `TESSERA_ERROR_INVALID_ARGUMENT` for a key path or value that does
///   not parse or is too long, or an edit the editor does not make (a
///   value in place of a table);
/// * `TESSERA_ERROR_IO` when the file cannot be read or written, or is not
///   one the editor writes (not UTF-8 or TOML, read-only, a second hard
///   link): it is left for the user to fix by hand;
/// * `TESSERA_ERROR_NULL_POINTER` for a `NULL` `app`, or a `NULL` `key` or
///   `value_json` with a non-zero length.
///
/// On every failure a message for the user (UTF-8, no NUL terminator, cut
/// to `msg_cap` bytes on a character boundary) goes to `msg` and its
/// length to `*msg_len`; on success `*msg_len` is 0. `msg_len` may be
/// `NULL` to skip the message.
///
/// The UI thread; it reads and writes the file (a few milliseconds).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `key` must be `NULL` or valid
/// for `key_len` bytes of reads, `value_json` for `value_len`. `msg_len`
/// must be `NULL` or point to a writable `size_t`; `msg` must be `NULL` or
/// valid for `msg_cap` bytes of writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_config_set(
    app: *mut App,
    key: *const u8,
    key_len: usize,
    value_json: *const u8,
    value_len: usize,
    msg: *mut u8,
    msg_cap: usize,
    msg_len: *mut usize,
) -> Error {
    ffi_guard!("tessera_config_set", {
        // SAFETY: forwarded under the same contract.
        unsafe {
            edit_export(
                "tessera_config_set",
                app,
                key,
                key_len,
                Some((value_json, value_len)),
                msg,
                msg_cap,
                msg_len,
            )
        }
    })
}

/// Removes the setting, table or profile at the key path `key` from
/// `config.toml` (so the default applies again) and saves the file; `Ok`
/// when there was nothing to remove (then nothing is written). Removing a
/// profile (`profiles[1]`) moves the later ones down.
///
/// The comments above a removed setting stay, and a comment on its line
/// stays as a line of its own; a removed table or profile takes the
/// comments inside it. Everything else as for `tessera_config_set`,
/// including the codes and the message.
///
/// The UI thread.
///
/// # Safety
///
/// As for `tessera_config_set`, without `value_json`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_config_remove(
    app: *mut App,
    key: *const u8,
    key_len: usize,
    msg: *mut u8,
    msg_cap: usize,
    msg_len: *mut usize,
) -> Error {
    ffi_guard!("tessera_config_remove", {
        // SAFETY: forwarded under the same contract.
        unsafe {
            edit_export(
                "tessera_config_remove",
                app,
                key,
                key_len,
                None,
                msg,
                msg_cap,
                msg_len,
            )
        }
    })
}

/// Copies the themes `appearance.theme` can name, as UTF-8 JSON, for a
/// theme picker:
///
/// ```json
/// {"themes": [{"name": "Tessera Dark", "id": "Tessera Dark",
///              "source": "bundled"},
///             {"name": "Ocean", "id": "ocean", "source": "user"}]}
/// ```
///
/// The bundled themes come first (one a user theme of the same name
/// replaces is left out), then the `themes/` files in file-name order (at
/// most 256 of them). `id` is what to write to
/// `appearance.theme`; `name` is for display, with control and
/// bidirectional formatting characters removed and at most 128 bytes. A
/// file whose stem would not show as written, or that does not parse, is
/// left out.
///
/// Buffer handling as for `tessera_config_json`.
///
/// Any thread; it reads `themes/`.
///
/// # Safety
///
/// As for `tessera_config_json`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_theme_list(
    app: *const App,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> Error {
    ffi_guard!("tessera_theme_list", {
        if len.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { len.write(0) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        let themes: Vec<serde_json::Value> = theme_entries(&themes_dir(&app.core.config_path()))
            .into_iter()
            .map(|entry| {
                let source = match entry.origin {
                    ThemeOrigin::Bundled => "bundled",
                    _ => "user",
                };
                serde_json::json!({"name": entry.name, "id": entry.id, "source": source})
            })
            .collect();
        let json = serde_json::json!({ "themes": themes }).to_string();
        // SAFETY: `len` and `buf` per the contract.
        unsafe { copy_out(&json, buf, cap, len) }
    })
}

/// Imports the colour scheme file at `path` (UTF-8, `path_len` bytes, at
/// most `TESSERA_MAX_THEME_PATH_LEN`): an iTerm2 `.itermcolors` or a
/// base16 `.yaml`/`.yml`, at most 1 MiB, as a new file in `themes/`.
/// Writes the new theme's id (what `appearance.theme` names it by; at most
/// `TESSERA_THEME_ID_LEN` bytes) to `id_buf` and its length to `*id_len`.
/// The theme is not selected; set `appearance.theme` to the id for that.
///
/// Returns `TESSERA_ERROR_INVALID_ARGUMENT` when `id_cap` is below
/// `TESSERA_THEME_ID_LEN` (checked first; nothing is imported), the path
/// is too long or not UTF-8, or the file is not a scheme the importers
/// read; `TESSERA_ERROR_CONFLICT` when `themes/` has a file of that name
/// already (it is kept); `TESSERA_ERROR_IO` when the file cannot be read
/// (missing, not a regular file, larger than 1 MiB) or the theme not
/// written; `TESSERA_ERROR_NULL_POINTER` for a `NULL` `app`, `id_buf` or
/// `id_len`, or a `NULL` `path` with a non-zero length. A message goes to
/// `msg` as for `tessera_config_set`.
///
/// The UI thread.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `path` must be `NULL` or valid
/// for `path_len` bytes of reads. `id_len` and `msg_len` must be `NULL` or
/// point to a writable `size_t`; `id_buf` must be `NULL` or valid for
/// `id_cap` bytes of writes, `msg` for `msg_cap`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_theme_import(
    app: *mut App,
    path: *const u8,
    path_len: usize,
    id_buf: *mut u8,
    id_cap: usize,
    id_len: *mut usize,
    msg: *mut u8,
    msg_cap: usize,
    msg_len: *mut usize,
) -> Error {
    ffi_guard!("tessera_theme_import", {
        // SAFETY: `msg_len` and `msg` per the contract.
        unsafe { message_out("", msg, msg_cap, msg_len) };
        let outcome = (|| {
            if id_len.is_null() || id_buf.is_null() {
                return Err(Failure::new(Error::NullPointer, "id_buf or id_len is NULL"));
            }
            // SAFETY: non-null and writable per the contract.
            unsafe { id_len.write(0) };
            if id_cap < THEME_ID_LEN {
                return Err(Failure::new(Error::InvalidArgument, "id_cap too small"));
            }
            // SAFETY: the caller passes NULL or a live handle.
            let Some(app) = (unsafe { App::from_ptr(app) }) else {
                return Err(Failure::new(Error::NullPointer, "app is NULL"));
            };
            if path_len > MAX_THEME_PATH_LEN {
                return Err(Failure::new(Error::InvalidArgument, "path too long"));
            }
            // SAFETY: `path` is NULL or valid for `path_len` bytes.
            let Some(path) = (unsafe { input(path, path_len) }) else {
                return Err(Failure::new(Error::NullPointer, "path is NULL"));
            };
            let Ok(path) = std::str::from_utf8(path) else {
                return Err(Failure::new(Error::InvalidArgument, "path is not UTF-8"));
            };
            let themes = themes_dir(&app.core.config_path());
            let imported = import_theme(&themes, Path::new(path)).map_err(|error| {
                let code = match &error {
                    EditError::Unsupported { .. } => Error::InvalidArgument,
                    EditError::ThemeExists { .. } => Error::Conflict,
                    _ => Error::Io,
                };
                tracing::info!(kind = ?code, "theme not imported");
                Failure::new(code, error.to_string())
            })?;
            tracing::info!(id = %imported.id, "theme imported");
            // SAFETY: `id_buf` holds `id_cap >= THEME_ID_LEN` bytes, and an
            // id is at most that long.
            Ok(unsafe { copy_out(&imported.id, id_buf, id_cap, id_len) })
        })();
        match outcome {
            Ok(code) => code,
            Err(failure) => {
                // SAFETY: as above.
                unsafe { message_out(&failure.message, msg, msg_cap, msg_len) };
                failure.code
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_cut_on_a_character_boundary() {
        let mut buf = [0u8; 8];
        let mut len = usize::MAX;
        // SAFETY: valid buffer and length.
        unsafe { message_out("abcdefgé", buf.as_mut_ptr(), buf.len(), &raw mut len) };
        assert_eq!(&buf[..len], b"abcdefg");
        // SAFETY: as above.
        unsafe { message_out("ab", buf.as_mut_ptr(), buf.len(), &raw mut len) };
        assert_eq!(&buf[..len], b"ab");
        // SAFETY: a NULL buffer writes nothing.
        unsafe { message_out("ab", std::ptr::null_mut(), 8, &raw mut len) };
        assert_eq!(len, 0);
        // SAFETY: a NULL length skips the message.
        unsafe { message_out("ab", buf.as_mut_ptr(), buf.len(), std::ptr::null_mut()) };
    }

    #[test]
    fn values_come_from_json() {
        let parse = |json: &str| serde_json::from_str::<TomlValue>(json).ok();
        assert_eq!(parse("0.5"), Some(TomlValue::Float(0.5)));
        assert_eq!(parse("3"), Some(TomlValue::Integer(3)));
        assert_eq!(parse("\"Nord\""), Some(TomlValue::String("Nord".into())));
        assert_eq!(parse("true"), Some(TomlValue::Boolean(true)));
        assert_eq!(parse("null"), None);
        assert_eq!(parse("18446744073709551615"), None);
    }
}
