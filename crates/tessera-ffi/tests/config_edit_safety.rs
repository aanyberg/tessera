//! Settings write-back through the C ABI under load (#324): a hundred
//! edits of a file with three warnings log those warnings once, not once
//! per save (item 9); edits from two threads both land (item 5); and a
//! file the editor refuses at open is `TESSERA_ERROR_IO` with no key path
//! in the message (item 1).
//!
//! One test, in a binary of its own: it sets `TESSERA_CONFIG_DIR` and a
//! global log subscriber (the watch logs on a thread of its own), which no
//! other test may see.

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::fmt::Write as _;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tessera_ffi::{
    App, Callbacks, Error, tessera_app_free, tessera_app_new, tessera_app_set_callbacks,
    tessera_config_set,
};

/// How long the count of applied configurations must stay put to count as
/// settled: longer than the watch's debounce and its directory poll (two
/// seconds).
const SETTLE: Duration = Duration::from_millis(2500);

/// A `config.toml` write is applied within the watch's debounce plus
/// 500 ms; the bound leaves headroom for a loaded machine (the wait ends
/// as soon as the callback fires).
const BOUND: Duration = Duration::from_millis(4000);

/// Edits per run, and per thread in the two-thread run.
const EDITS: usize = 100;

/// Every event at `WARN` or above, on any thread: its fields,
/// `name=value`.
struct Capture(Mutex<Vec<String>>);

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        let _ = write!(self.0, "{}={value} ", field.name());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

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
        // More verbose levels compare greater.
        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        self.0.lock().unwrap().push(line);
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

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
    };
    // SAFETY: live handle and a valid table; `applied` outlives the app.
    assert_eq!(
        unsafe { tessera_app_set_callbacks(app, &raw const callbacks) },
        Error::Ok
    );
}

/// Waits until `applied` has not moved for [`SETTLE`].
fn settle(applied: &AtomicU64) {
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
}

/// The handle, for the threads of the two-thread run.
#[derive(Clone, Copy)]
struct Handle(*mut App);

// SAFETY: the test frees the handle only after both threads are joined;
// the edits themselves are what is under test.
unsafe impl Send for Handle {}

fn set(app: Handle, key: &str, json: &str) -> (Error, String) {
    let mut msg = [0u8; 512];
    let mut msg_len = 0;
    // SAFETY: live handle; the slices and buffers are valid for their
    // lengths.
    let code = unsafe {
        tessera_config_set(
            app.0,
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

#[test]
fn edits_log_old_warnings_once_serialise_and_refuse_an_uneditable_file() {
    let capture = Arc::new(Capture(Mutex::new(Vec::new())));
    tracing::subscriber::set_global_default(Arc::clone(&capture)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[general]\nbogus_one = 1\n[appearance]\nbogus_two = 2\nopacity = 1.0\n\
         [terminal]\nbogus_three = 3\n",
    )
    .unwrap();
    let applied = AtomicU64::new(0);
    let app = Handle(new_app(dir.path()));
    install(app.0, &applied);

    // Item 9: a hundred saves; the watch applies the first three one by
    // one (the later ones may coalesce).
    for i in 0..EDITS {
        let json = format!("0.{}", 10 + i % 80);
        let before = applied.load(Ordering::SeqCst);
        assert_eq!(
            set(app, "appearance.opacity", &json),
            (Error::Ok, String::new())
        );
        let deadline = Instant::now() + BOUND;
        while i < 3 && applied.load(Ordering::SeqCst) == before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    settle(&applied);
    assert!(
        applied.load(Ordering::SeqCst) >= 3,
        "the watch applied the saves"
    );
    {
        let lines = capture.0.lock().unwrap();
        for key in ["bogus_one", "bogus_two", "bogus_three"] {
            let n = lines.iter().filter(|line| line.contains(key)).count();
            assert!(n <= 1, "{key} logged {n} times: {lines:#?}");
        }
    }

    // Item 5: two threads, two keys, every edit saved, both last values in
    // the file.
    let threads: Vec<_> = [("appearance.opacity", 0.0), ("appearance.font_size", 10.0)]
        .into_iter()
        .map(|(key, base)| {
            std::thread::spawn(move || {
                for i in 1..=EDITS {
                    #[expect(clippy::cast_precision_loss, reason = "small counts")]
                    let value = base + i as f64 / 200.0;
                    let (code, msg) = set(app, key, &format!("{value:.3}"));
                    assert_eq!(code, Error::Ok, "{key} edit {i}: {msg}");
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(saved.contains("\nopacity = 0.5\n"), "{saved}");
    assert!(saved.contains("\nfont_size = 10.5\n"), "{saved}");

    // Item 1: refused at open, as an I/O failure, naming no key path.
    let mixed = "[appearance]\r\nopacity = 1.0\n";
    std::fs::write(&config, mixed).unwrap();
    let (code, msg) = set(app, "appearance.opacity", "0.5");
    assert_eq!(code, Error::Io, "{msg}");
    assert!(msg.contains("cannot be edited"), "{msg}");
    assert!(!msg.contains('`'), "no key path: {msg}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), mixed);

    // SAFETY: live handle, freed once; both threads are joined.
    unsafe { tessera_app_free(app.0) };
}
