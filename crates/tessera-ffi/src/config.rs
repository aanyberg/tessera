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
use std::sync::Mutex;

use tessera_app_core::WatchOptions;
use tessera_app_core::command::Keymap;
use tessera_app_core::edit::{
    ConfigEditor, EditError, FILE_PROFILE_REFUSED, KeyPath, MAX_KEY_PATH_LEN, Step, ThemeOrigin,
    TomlValue, import_theme, theme_entries,
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
        EditError::NotEditable { .. } => {
            // Refused at open: no key path is involved (#324 item 1).
            tracing::info!(
                function,
                kind = "file not editable",
                "config edit not saved"
            );
            return Failure::new(Error::Io, error.to_string());
        }
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

/// Held across every edit of `config.toml` from this process: two edits
/// on two threads (the write queue and any other caller) would both read the file
/// and one could replace the other's save (#324 item 5). An edit that
/// panicked left nothing half done in memory, so a poisoned lock is
/// taken over.
static EDIT_LOCK: Mutex<()> = Mutex::new(());

/// Held across [`open_config`]'s check and its start of the watch.
static OPEN_LOCK: Mutex<()> = Mutex::new(());

/// The apps (by address) [`open_config`] started a watch for, in order:
/// what the test of two callers at once counts.
#[cfg(test)]
static WATCH_STARTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Makes `config.toml` editable: creates the configuration directory and a
/// commented default `config.toml` where they are missing and starts the
/// watch unless it runs already (`App::open_config`); returns the file's
/// path. Nothing is opened again when the file exists and is watched: that
/// would reload and log it (#324 item 9). The check and the start happen
/// under one lock, so two callers at once (the Open Config menu item's
/// `tessera_app_create_config` and a Settings edit on the write queue)
/// start one watch, never two (#344). A Settings edit
/// (`tessera_config_set`, `tessera_config_remove`) creates the file too.
pub(crate) fn open_config(app: &App) -> Result<PathBuf, String> {
    let _open = OPEN_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let path = app.core.config_path();
    let watching = app.core.is_watching_config();
    if path.is_file() && watching {
        return Ok(path);
    }
    #[cfg(test)]
    if !watching {
        WATCH_STARTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(std::ptr::from_ref(app) as usize);
    }
    app.core
        .open_config(WatchOptions::default())
        .map_err(|error| error.to_string())
}

/// Opens `config.toml` (creating it and starting the watch when needed),
/// applies `change` at `key`, checks the key map, and saves, holding
/// [`EDIT_LOCK`].
fn edit(function: &'static str, app: &App, key: &str, change: Change) -> Result<(), Failure> {
    let _edit = EDIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // The editor counts the profile files it reads now; the caller's index
    // comes from the configuration applied before this call, the one
    // `tessera_config_json` reported (read before `open_config`, whose
    // first watch may apply a newer one). A file deleted, moved or broken
    // since then would leave the index one past the editor's entries, and
    // the edit an append to `config.toml` (#312 B1): an index that names a
    // file's profile in the applied configuration is refused too.
    if let Ok(path) = key.parse::<KeyPath>()
        && let [Step::Key(first), Step::Index(index), ..] = path.steps()
        && first == "profiles"
        && app
            .core
            .config()
            .profiles
            .get(*index)
            .is_some_and(|profile| profile.source.is_file())
    {
        let error = EditError::Unsupported {
            path,
            reason: FILE_PROFILE_REFUSED,
        };
        return Err(failure(function, key, &error));
    }
    let path = open_config(app).map_err(|error| {
        tracing::warn!(function, %error, "cannot create the configuration");
        Failure::new(Error::Io, error)
    })?;
    let mut editor = ConfigEditor::open(&path).map_err(|e| failure(function, key, &e))?;
    // The key map is compiled quietly: its warnings are compared, not
    // logged (the watch logs them when it applies the file).
    let quiet = |config: Option<&tessera_app_core::Config>| -> Vec<String> {
        config.map_or_else(Vec::new, |config| {
            Keymap::compile_quietly(&config.keys)
                .warnings()
                .iter()
                .map(ToString::to_string)
                .collect()
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
        // The edit took the key path, so it parses; it displays escaped as
        // the loader escapes the file's text (S3, S8): the message reaches
        // a settings window, and the caller's text must not reorder it.
        let path = key
            .parse::<tessera_app_core::edit::KeyPath>()
            .map_or_else(|_| "?".to_owned(), |path| path.to_string());
        tracing::info!(function, %path, kind = "refused by the key map", "config edit not saved");
        return Err(Failure::new(
            Error::Refused,
            format!("`{path}` refused: {}", new.join("; ")),
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
/// configuration file documents, under the same names. Each profile also
/// has `"source"`: `"config"` for a `[[profiles]]` entry of `config.toml`,
/// `"profiles/<stem>.toml"` for one read from that file, which the
/// `[[profiles]]` entries come before and which `tessera_config_set` and
/// `tessera_config_remove` refuse to edit. Read it at start and again on
/// every `on_config_applied`.
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
        // Read together (#348): read one after the other, an apply in
        // between gave a configuration with another apply's generation.
        let (config, generation) = app.core.config_and_generation();
        let json = serde_json::json!({
            "generation": generation,
            "config": config_value(&config),
        })
        .to_string();
        // SAFETY: `len` and `buf` per the contract.
        unsafe { copy_out(&json, buf, cap, len) }
    })
}

/// `config` as JSON, each profile with its `source` (#312): `"config"`
/// for a `[[profiles]]` entry, `"profiles/<stem>.toml"` for a profile
/// file's (not a key: the loader sets it, see
/// `tessera_config::ProfileSource`).
fn config_value(config: &tessera_app_core::Config) -> serde_json::Value {
    let mut value = serde_json::to_value(config).unwrap_or(serde_json::Value::Null);
    if let Some(serde_json::Value::Array(profiles)) = value.get_mut("profiles") {
        for (json, profile) in profiles.iter_mut().zip(&config.profiles) {
            if let serde_json::Value::Object(object) = json {
                object.insert(
                    "source".to_owned(),
                    serde_json::Value::String(profile.source.label()),
                );
            }
        }
    }
    value
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
///   value in place of a table, a profile whose `"source"` in
///   `tessera_config_json` is a file in `profiles/`);
/// * `TESSERA_ERROR_IO` when the file cannot be read or written, or is not
///   one the editor writes (not UTF-8 or TOML, read-only, a second hard
///   link, mixed line endings or other TOML the editor would not write
///   back byte for byte): it is left for the user to fix by hand;
/// * `TESSERA_ERROR_NULL_POINTER` for a `NULL` `app`, or a `NULL` `key` or
///   `value_json` with a non-zero length.
///
/// On every failure a message for the user (UTF-8, no NUL terminator, cut
/// to `msg_cap` bytes on a character boundary) goes to `msg` and its
/// length to `*msg_len`; on success `*msg_len` is 0. `msg_len` may be
/// `NULL` to skip the message.
///
/// Any thread: the app calls it on its background write queue, off the
/// main thread; it reads and writes the file (a few milliseconds). Edits
/// from two threads are serialised, never interleaved.
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
/// Any thread (the app's background write queue), as for
/// `tessera_config_set`.
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
/// Any thread (the app's background write queue); it reads the scheme
/// and writes `themes/`.
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
            // The id comes from a name the user chose: its length only (S8).
            tracing::info!(id_len = imported.id.len(), "theme imported");
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

    /// #344: `open_config` from two threads at once (the Open Config menu
    /// item and a Settings edit) starts one watch, never two: the check
    /// and the start are under one lock. 100 rounds, each a fresh
    /// directory the two threads race to create.
    #[test]
    fn two_callers_at_once_start_one_watch() {
        for round in 0..100 {
            let dir = tempfile::tempdir().unwrap();
            let app = std::sync::Arc::new(App {
                core: tessera_app_core::App::new(
                    tessera_app_core::Config::default(),
                    tessera_app_core::ConfigPaths::new(dir.path().join("tessera")),
                ),
            });
            let address = std::ptr::from_ref::<App>(&app) as usize;
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let callers: Vec<_> = (0..2)
                .map(|_| {
                    let app = std::sync::Arc::clone(&app);
                    let barrier = std::sync::Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        open_config(&app).unwrap()
                    })
                })
                .collect();
            for caller in callers {
                assert!(caller.join().unwrap().is_file());
            }
            assert!(app.core.is_watching_config());
            let starts = WATCH_STARTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|&&started| started == address)
                .count();
            assert_eq!(starts, 1, "round {round}");
            // The next app may reuse the address.
            drop(app);
            WATCH_STARTS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|&started| started != address);
        }
    }

    /// #348: a refusal names the caller's key path escaped, so an
    /// invisible character in it (U+200B) reaches the settings window as
    /// `\u{200b}`, never as itself (S3, S8).
    #[test]
    fn a_refusal_escapes_the_callers_key_path() {
        let dir = tempfile::tempdir().unwrap();
        let app = App {
            core: tessera_app_core::App::new(
                tessera_app_core::Config::default(),
                tessera_app_core::ConfigPaths::new(dir.path()),
            ),
        };
        let value = || Change::Set(TomlValue::String("new_tab".into()));
        let failure = edit("test", &app, "keys.\"cmd+\u{200B}\"", value()).unwrap_err();
        assert_eq!(failure.code, Error::Refused, "{}", failure.message);
        assert!(
            failure
                .message
                .starts_with("`keys.\"cmd+\\u{200b}\"` refused")
                && !failure.message.contains('\u{200B}'),
            "{}",
            failure.message
        );
        // Refused by the key map (an unknown command), the same way.
        let bogus = Change::Set(TomlValue::String("bogus".into()));
        let failure = edit("test", &app, "keys.\"cmd+j\"", bogus).unwrap_err();
        assert_eq!(failure.code, Error::Refused, "{}", failure.message);
        assert!(
            failure.message.starts_with("`keys.\"cmd+j\"` refused: ")
                && failure.message.contains("unknown command `bogus`"),
            "{}",
            failure.message
        );
    }

    /// #348, staged: an apply started between the configuration and its
    /// generation cannot finish there (the combined read holds the lock
    /// the configuration is recorded under), so the JSON is the old
    /// configuration with the old generation. Read one after the other
    /// (in either order), the apply lands in between and they disagree.
    #[test]
    fn an_apply_cannot_land_between_the_configuration_and_its_generation() {
        use std::sync::Arc;
        let app = Arc::new(App {
            core: tessera_app_core::App::new(
                tessera_app_core::Config::default(),
                tessera_app_core::ConfigPaths::new("/nonexistent/tessera"),
            ),
        });
        let (start, started) = std::sync::mpsc::channel::<()>();
        let (finish, finished) = std::sync::mpsc::channel::<()>();
        let applier = {
            let app = Arc::clone(&app);
            std::thread::spawn(move || {
                let bound =
                    tessera_app_core::Config::from_toml_lenient("[keys]\n'cmd+u' = 'new_tab'\n")
                        .unwrap()
                        .config;
                started.recv().unwrap();
                app.core
                    .apply_config(&bound, &tessera_config::Theme::default());
                // The reader stopped listening if the apply had to wait.
                let _ = finish.send(());
            })
        };
        tessera_app_core::App::pause_between_reads_for_test(move || {
            start.send(()).unwrap();
            // Long enough for an unblocked apply even under load; a
            // blocked one never finishes here.
            let _ = finished.recv_timeout(std::time::Duration::from_millis(500));
        });
        let mut buf = vec![0u8; 1 << 20];
        let mut len = 0usize;
        // SAFETY: a live handle, a writable length and a buffer of its size.
        let code = unsafe {
            tessera_config_json(Arc::as_ptr(&app), buf.as_mut_ptr(), buf.len(), &raw mut len)
        };
        applier.join().unwrap();
        assert_eq!(code, Error::Ok);
        let json: serde_json::Value = serde_json::from_slice(&buf[..len]).unwrap();
        assert_eq!(
            (
                json["generation"].as_u64().unwrap(),
                &json["config"]["keys"]
            ),
            (0, &serde_json::json!({})),
            "an apply landed between the reads"
        );
        assert_eq!(app.core.config_generation(), 1);
    }

    /// #312, F5: each profile in `tessera_config_json` names its source,
    /// `[[profiles]]` entries first; an edit of a file profile through
    /// the C ABI is refused (`TESSERA_ERROR_INVALID_ARGUMENT`, as any edit
    /// the editor does not make) with its message and writes nothing,
    /// and an edit of a `config.toml` profile is saved.
    #[test]
    fn profiles_carry_their_source_and_file_profiles_are_not_edited() {
        let dir = tempfile::tempdir().unwrap();
        let paths = tessera_app_core::ConfigPaths::new(dir.path());
        std::fs::create_dir_all(paths.profiles_dir()).unwrap();
        std::fs::write(paths.config_file(), "[[profiles]]\nname = \"entry\"\n").unwrap();
        let file = paths.profiles_dir().join("work.toml");
        std::fs::write(&file, "command = \"/bin/zsh\"\n").unwrap();
        let store = tessera_config::ConfigStore::open(paths.clone()).unwrap();
        let app = App {
            core: tessera_app_core::App::new((*store.config()).clone(), paths.clone()),
        };
        let mut buf = vec![0u8; 1 << 20];
        let mut len = 0usize;
        // SAFETY: a live handle, a writable length and a buffer of its size.
        let code = unsafe {
            tessera_config_json(
                std::ptr::from_ref(&app),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut len,
            )
        };
        assert_eq!(code, Error::Ok);
        let json: serde_json::Value = serde_json::from_slice(&buf[..len]).unwrap();
        let profiles: Vec<_> = json["config"]["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|profile| (profile["name"].clone(), profile["source"].clone()))
            .collect();
        assert_eq!(
            profiles,
            [
                (serde_json::json!("entry"), serde_json::json!("config")),
                (
                    serde_json::json!("work"),
                    serde_json::json!("profiles/work.toml")
                ),
            ]
        );

        let config_before = std::fs::read(paths.config_file()).unwrap();
        let file_before = std::fs::read(&file).unwrap();
        let set = || Change::Set(TomlValue::String("/bin/sh".into()));
        let failure = edit("test", &app, "profiles[1].command", set()).unwrap_err();
        // An edit the editor does not make, as for any unsupported one.
        assert_eq!(failure.code, Error::InvalidArgument, "{}", failure.message);
        assert!(
            failure
                .message
                .contains(tessera_config::edit::FILE_PROFILE_REFUSED),
            "{}",
            failure.message
        );
        assert_eq!(std::fs::read(paths.config_file()).unwrap(), config_before);
        assert_eq!(std::fs::read(&file).unwrap(), file_before);

        if let Err(failure) = edit("test", &app, "profiles[0].command", set()) {
            panic!("{}", failure.message);
        }
        let saved = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(saved.contains("command = \"/bin/sh\""), "{saved}");
        assert_eq!(std::fs::read(&file).unwrap(), file_before);
    }

    /// #312 B1: a profile file deleted after the configuration was
    /// applied leaves the editor no file profiles, so `profiles[1]` (the
    /// caller's index of that file's profile) would be one past the
    /// entries and append to `config.toml`. The applied configuration
    /// still names it a file's profile, so the edit is refused and nothing
    /// is written.
    #[test]
    fn a_file_profile_deleted_since_the_last_apply_is_not_edited() {
        let dir = tempfile::tempdir().unwrap();
        let paths = tessera_app_core::ConfigPaths::new(dir.path());
        std::fs::create_dir_all(paths.profiles_dir()).unwrap();
        std::fs::write(paths.config_file(), "[[profiles]]\nname = \"entry\"\n").unwrap();
        let file = paths.profiles_dir().join("p00.toml");
        std::fs::write(&file, "command = \"/bin/p00\"\n").unwrap();
        let store = tessera_config::ConfigStore::open(paths.clone()).unwrap();
        let app = App {
            core: tessera_app_core::App::new((*store.config()).clone(), paths.clone()),
        };
        std::fs::remove_file(&file).unwrap();
        let before = std::fs::read(paths.config_file()).unwrap();
        let set = || Change::Set(TomlValue::String("/bin/meant-for-file".into()));
        for change in [set(), Change::Remove] {
            let failure = edit("test", &app, "profiles[1].command", change).unwrap_err();
            assert_eq!(failure.code, Error::InvalidArgument, "{}", failure.message);
            assert!(
                failure
                    .message
                    .contains(tessera_config::edit::FILE_PROFILE_REFUSED),
                "{}",
                failure.message
            );
        }
        let failure = edit("test", &app, "profiles[1]", Change::Remove).unwrap_err();
        assert_eq!(failure.code, Error::InvalidArgument, "{}", failure.message);
        assert_eq!(std::fs::read(paths.config_file()).unwrap(), before);
        // The entry itself is still edited.
        if let Err(failure) = edit("test", &app, "profiles[0].command", set()) {
            panic!("{}", failure.message);
        }
    }

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
