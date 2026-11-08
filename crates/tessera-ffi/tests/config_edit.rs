//! Settings write-back through the C ABI (#40, ADR 0040): a
//! `tessera_config_set` saves `config.toml` once, and the running
//! configuration watch applies that write exactly once
//! (`on_config_applied`), so the configuration in effect and
//! `tessera_config_json` show the new value. A refused edit and an edit
//! that changes nothing write nothing, so nothing is applied.
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
    App, Callbacks, Error, tessera_app_free, tessera_app_new, tessera_app_set_callbacks,
    tessera_config_json, tessera_config_remove, tessera_config_set, tessera_test_with_core,
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
    assert_eq!(settled(&applied, 1), 1, "one write, applied once");
    assert!((opacity(app) - 0.5).abs() < f32::EPSILON);
    let json = config_json(app);
    assert_eq!(json["generation"], 1);
    assert_eq!(json["config"]["appearance"]["opacity"], 0.5);

    // Refused, or the same value again: nothing written, nothing applied.
    let before = std::fs::read_to_string(&config).unwrap();
    let (code, msg) = set(app, "appearance.opacity", "7.5");
    assert_eq!(code, Error::Refused);
    assert!(msg.contains("appearance.opacity"), "{msg}");
    let (code, _) = set(app, "appearance.opacity", "0.5");
    assert_eq!(code, Error::Ok);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    assert_eq!(settled(&applied, 1), 1, "nothing more applied");

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
    assert_eq!(settled(&applied, 2), 2);
    assert!((opacity(app) - 1.0).abs() < f32::EPSILON);
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "# Mine.\n[appearance]\n# opaque\n"
    );

    // SAFETY: live handle, freed once; no other call races.
    unsafe { tessera_app_free(app) };
}
