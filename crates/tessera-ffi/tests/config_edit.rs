//! Settings write-back through the C ABI (#40, ADR 0040): a
//! `tessera_config_set` saves `config.toml` once, and the running
//! configuration watch applies that write exactly once
//! (`on_config_applied`), so the configuration in effect and
//! `tessera_config_json` show the new value. A refused edit and an edit
//! that changes nothing write nothing, so nothing is applied.
//! `appearance.app_appearance` (#365) is set as a string, and the next
//! configuration applied reports it through `tessera_app_app_appearance`.
//!
//! One test, in a binary of its own: it sets `TESSERA_CONFIG_DIR`, which no
//! other test may see.

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tessera_ffi::{
    App, AppAppearance, Callbacks, Error, tessera_app_app_appearance, tessera_app_free,
    tessera_app_new, tessera_app_set_callbacks, tessera_config_json, tessera_config_remove,
    tessera_config_set, tessera_test_with_core,
};

/// A `config.toml` write is applied within the watch's debounce plus
/// 500 ms; the bound leaves headroom for a loaded machine (the wait ends
/// as soon as the callback fires).
const BOUND: Duration = Duration::from_millis(4000);

/// How long a count must stay put to count as settled: longer than the
/// watch's debounce and its directory poll (two seconds).
const SETTLE: Duration = Duration::from_millis(2500);

/// Creates an application with `TESSERA_CONFIG_DIR` set to `dir`.
fn new_app(dir: &Path) -> *mut App {
    // SAFETY: this binary's only test runs on one thread, and nothing else
    // in the process reads the environment concurrently.
    unsafe { std::env::set_var("TESSERA_CONFIG_DIR", dir) };
    let mut app = ptr::null_mut();
    // SAFETY: `app` is writable storage for one pointer.
    assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
    assert!(!app.is_null());
    app
}

unsafe extern "C" fn on_config_applied(user_data: *mut c_void, _generation: u64) {
    // SAFETY: `user_data` is the counter installed below, alive until the
    // application is freed.
    let applied = unsafe { &*user_data.cast::<AtomicU64>() };
    applied.fetch_add(1, Ordering::SeqCst);
}

fn install(app: *mut App, applied: &AtomicU64) {
    let callbacks = Callbacks {
        user_data: ptr::from_ref(applied).cast_mut().cast(),
        on_session_output: None,
        on_session_title: None,
        on_session_bell: None,
        on_session_exit: None,
        on_appearance_changed: None,
        on_session_clipboard: None,
        on_config_applied: Some(on_config_applied),
        on_session_remote: None,
        on_config_invalid: None,
        on_session_command_finished: None,
        on_font_index_ready: None,
    };
    // SAFETY: live handle and a valid table; `applied` outlives the app.
    assert_eq!(
        unsafe { tessera_app_set_callbacks(app, &raw const callbacks) },
        Error::Ok
    );
}

/// Waits until `applied` reaches `n` (or the bound passes), then until it
/// has not moved for [`SETTLE`]; returns the settled count.
fn settled(applied: &AtomicU64, n: u64) -> u64 {
    let deadline = Instant::now() + BOUND;
    while applied.load(Ordering::SeqCst) < n && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut seen = applied.load(Ordering::SeqCst);
    let mut since = Instant::now();
    while since.elapsed() < SETTLE {
        std::thread::sleep(Duration::from_millis(50));
        let now = applied.load(Ordering::SeqCst);
        if now != seen {
            seen = now;
            since = Instant::now();
        }
    }
    seen
}

/// Waits for the announcements of one write after `n` and returns the
/// settled count: one for the apply, plus at most one `Unchanged` from a
/// re-read whose sample of the files raced the load: the watcher samples
/// the files the load reads, then loads, so a write landing between the
/// two is loaded by one re-read and still differs from its sample at the
/// next, which finds it applied and reports `Unchanged` (a late event on
/// a loaded machine, #535, #731). More is a double announcement.
fn applied_once(applied: &AtomicU64, n: u64, what: &str) -> u64 {
    let seen = settled(applied, n + 1);
    assert!(
        (n + 1..=n + 2).contains(&seen),
        "{what}: applied once (plus at most one unchanged re-read): {n} -> {seen}"
    );
    seen
}

fn set(app: *mut App, key: &str, json: &str) -> (Error, String) {
    let mut msg = [0u8; 512];
    let mut msg_len = 0;
    // SAFETY: live handle; the slices and buffers are valid for their
    // lengths.
    let code = unsafe {
        tessera_config_set(
            app,
            key.as_ptr(),
            key.len(),
            json.as_ptr(),
            json.len(),
            msg.as_mut_ptr(),
            msg.len(),
            &raw mut msg_len,
        )
    };
    (code, String::from_utf8(msg[..msg_len].to_vec()).unwrap())
}

fn config_json(app: *mut App) -> serde_json::Value {
    let mut buf = vec![0u8; 1 << 20];
    let mut len = 0;
    // SAFETY: live handle; `buf` is valid for its length.
    let code = unsafe { tessera_config_json(app, buf.as_mut_ptr(), buf.len(), &raw mut len) };
    assert_eq!(code, Error::Ok);
    serde_json::from_slice(&buf[..len]).unwrap()
}

fn opacity(app: *mut App) -> f32 {
    // SAFETY: `app` is a live handle used on this thread only.
    unsafe { tessera_test_with_core(app, |core| core.config().appearance.opacity) }.unwrap()
}

fn app_appearance(app: *const App) -> u32 {
    let mut out = u32::MAX;
    // SAFETY: a live handle and writable storage for one `uint32_t`.
    assert_eq!(
        unsafe { tessera_app_app_appearance(app, &raw mut out) },
        Error::Ok
    );
    out
}

/// #365: `appearance.app_appearance` is written as a string, the watch
/// applies the write once, and `tessera_app_app_appearance` reports the
/// new value once `on_config_applied` has been called; a value that is
/// not one of the three is refused and writes nothing.
fn app_appearance_round_trips(root: &Path) {
    let dir = root.join("app-appearance");
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    std::fs::write(&config, "[appearance]\nfont_size = 13.0\n").unwrap();
    let applied = AtomicU64::new(0);
    let app = new_app(&dir);
    install(app, &applied);
    assert_eq!(
        app_appearance(app),
        AppAppearance::Dark as u32,
        "missing key"
    );
    assert_eq!(
        (
            AppAppearance::Dark as u32,
            AppAppearance::Light as u32,
            AppAppearance::System as u32
        ),
        (0, 1, 2),
        "the values the header names"
    );

    let mut n = 0;
    for (name, want) in [
        ("light", AppAppearance::Light),
        ("system", AppAppearance::System),
        ("dark", AppAppearance::Dark),
    ] {
        let (code, msg) = set(app, "appearance.app_appearance", &format!("\"{name}\""));
        assert_eq!((code, msg.as_str()), (Error::Ok, ""), "{name}");
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            format!("[appearance]\nfont_size = 13.0\napp_appearance = \"{name}\"\n"),
            "{name}"
        );
        n = applied_once(&applied, n, name);
        assert_eq!(app_appearance(app), want as u32, "{name}");
        assert_eq!(
            config_json(app)["config"]["appearance"]["app_appearance"],
            name
        );
    }

    let before = std::fs::read_to_string(&config).unwrap();
    for bad in ["\"Light\"", "\"auto\"", "1", "true"] {
        let (code, msg) = set(app, "appearance.app_appearance", bad);
        assert_eq!(code, Error::Refused, "{bad}: {msg}");
        assert!(msg.contains("appearance.app_appearance"), "{bad}: {msg}");
    }
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    assert_eq!(settled(&applied, n), n, "nothing more applied");

    let mut out = 7;
    // SAFETY: NULL handle with writable `out`; a live handle with NULL
    // `out`.
    unsafe {
        assert_eq!(
            tessera_app_app_appearance(ptr::null(), &raw mut out),
            Error::NullPointer
        );
        assert_eq!(out, AppAppearance::Dark as u32, "reset on failure");
        assert_eq!(
            tessera_app_app_appearance(app, ptr::null_mut()),
            Error::NullPointer
        );
    }
    // SAFETY: live handle, freed once; no other call races.
    unsafe { tessera_app_free(app) };
}

/// `load_error` in `tessera_config_json` (#547): `null` while the file
/// loads, the store's error once a broken file was read (the generation
/// and the configuration in effect stay), `null` again once it is fixed.
fn load_error_round_trips(root: &Path) {
    let dir = root.join("load-error");
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    std::fs::write(&config, "[appearance]\nopacity = 0.5\n").unwrap();
    let applied = AtomicU64::new(0);
    let app = new_app(&dir);
    install(app, &applied);
    let json = config_json(app);
    assert_eq!(json["load_error"], serde_json::Value::Null, "{json}");
    let generation = json["generation"].clone();

    std::fs::write(&config, "[appearance]\nopacity = 0.25\nnot toml\n").unwrap();
    let deadline = Instant::now() + BOUND;
    let mut json = config_json(app);
    while json["load_error"].is_null() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        json = config_json(app);
    }
    let error = json["load_error"]
        .as_str()
        .unwrap_or_else(|| panic!("no load_error: {json}"));
    assert!(error.contains("not toml"), "{error}");
    assert!(!error.contains('\n'), "escaped (S8): {error}");
    assert_eq!(json["generation"], generation, "nothing applied");
    assert_eq!(
        json["config"]["appearance"]["opacity"], 0.5,
        "the configuration in effect"
    );

    std::fs::write(&config, "[appearance]\nopacity = 0.25\n").unwrap();
    assert_eq!(settled(&applied, 1), 1, "the fixed file applies once");
    let json = config_json(app);
    assert_eq!(json["load_error"], serde_json::Value::Null, "{json}");
    assert_eq!(json["config"]["appearance"]["opacity"], 0.25);
    // SAFETY: live handle, freed once; no other call races.
    unsafe { tessera_app_free(app) };
}

#[test]
fn the_watch_applies_a_write_once() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let text = "# Mine.\n[appearance]\nopacity = 1.0 # opaque\n";
    std::fs::write(&config, text).unwrap();
    let applied = AtomicU64::new(0);
    let app = new_app(dir.path());
    install(app, &applied);
    assert_eq!(settled(&applied, 0), 0, "nothing applied at start");

    let (code, msg) = set(app, "appearance.opacity", "0.5");
    assert_eq!((code, msg.as_str()), (Error::Ok, ""));
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "# Mine.\n[appearance]\nopacity = 0.5 # opaque\n"
    );
    let n = applied_once(&applied, 0, "one write");
    assert!((opacity(app) - 0.5).abs() < f32::EPSILON);
    let json = config_json(app);
    assert_eq!(json["generation"], n);
    assert_eq!(json["config"]["appearance"]["opacity"], 0.5);

    // Refused, or the same value again: nothing written, nothing applied.
    let before = std::fs::read_to_string(&config).unwrap();
    let (code, msg) = set(app, "appearance.opacity", "7.5");
    assert_eq!(code, Error::Refused);
    assert!(msg.contains("appearance.opacity"), "{msg}");
    let (code, _) = set(app, "appearance.opacity", "0.5");
    assert_eq!(code, Error::Ok);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    assert_eq!(settled(&applied, n), n, "nothing more applied");

    // A remove is applied once too: the default is back.
    let key = "appearance.opacity";
    let mut msg_len = 0;
    // SAFETY: live handle; `key` is valid for its length; no message
    // buffer.
    let code = unsafe {
        tessera_config_remove(
            app,
            key.as_ptr(),
            key.len(),
            ptr::null_mut(),
            0,
            &raw mut msg_len,
        )
    };
    assert_eq!((code, msg_len), (Error::Ok, 0));
    applied_once(&applied, n, "a remove");
    assert!((opacity(app) - 1.0).abs() < f32::EPSILON);
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "# Mine.\n[appearance]\n# opaque\n"
    );

    // SAFETY: live handle, freed once; no other call races.
    unsafe { tessera_app_free(app) };

    app_appearance_round_trips(dir.path());
    an_undone_save_is_announced(dir.path());
    load_error_round_trips(dir.path());
}

/// #535: a Settings save that another app undoes right away (inside the
/// watch's debounce, when the timing allows) still reaches the host:
/// `on_config_applied` is called with a new generation and the
/// configuration then read is the file's, so a value the host holds as
/// pending settles to it. Before the fix a re-read that found the
/// configuration unchanged called nothing and the host waited forever.
fn an_undone_save_is_announced(root: &Path) {
    let dir = root.join("undone");
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    let text = "[appearance]\nfont_size = 13.0\n";
    std::fs::write(&config, text).unwrap();
    let applied = AtomicU64::new(0);
    let app = new_app(&dir);
    install(app, &applied);
    assert_eq!(settled(&applied, 0), 0, "nothing applied at start");

    let (code, msg) = set(app, "appearance.font_size", "14.0");
    assert_eq!((code, msg.as_str()), (Error::Ok, ""));
    std::fs::write(&config, text).unwrap();
    let count = settled(&applied, 1);
    assert!(count >= 1, "the re-read reached the host");
    let json = config_json(app);
    assert_eq!(json["generation"], count, "one generation per call");
    assert_eq!(
        json["config"]["appearance"]["font_size"], 13.0,
        "the file's value"
    );

    // SAFETY: live handle, freed once; no other call races.
    unsafe { tessera_app_free(app) };
}
