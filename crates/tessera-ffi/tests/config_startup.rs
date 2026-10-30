//! `tessera_app_new` reads the user's configuration at start and watches
//! it: the theme `config.toml` names (here a file in `themes/`) is the one
//! views draw with, an edit of that theme file reaches the views and the
//! `on_appearance_changed` callback, and a missing or broken configuration
//! starts with the defaults instead of failing. Starting creates nothing
//! on disk; `tessera_app_create_config` creates the configuration and
//! starts watching it.
//!
//! One test, in a binary of its own: it points `TESSERA_CONFIG_DIR` at a
//! different directory per step, which no other test may see.

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::path::Path;
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use tessera_ffi::{
    App, Callbacks, Error, tessera_app_create_config, tessera_app_free, tessera_app_new,
    tessera_app_set_callbacks, tessera_test_with_core,
};

/// A theme-file edit is applied within a debounce plus 500 ms; the bound
/// leaves headroom for a loaded machine (the wait ends as soon as the
/// callback fires).
const BOUND: Duration = Duration::from_millis(2000);

fn theme_doc(name: &str, background: &str) -> String {
    let mut theme = tessera_config::Theme::bundled("Dracula").unwrap().clone();
    theme.name = name.into();
    theme.background = background.parse().unwrap();
    theme.to_toml()
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

fn with_core<R>(app: *mut App, f: impl FnOnce(&tessera_app_core::App) -> R) -> R {
    // SAFETY: `app` is a live handle used on this thread only.
    unsafe { tessera_test_with_core(app, f) }.expect("live handle")
}

fn background(app: *mut App) -> [u8; 3] {
    with_core(app, |core| core.view_theme().background.to_srgb8())
}

unsafe extern "C" fn on_appearance(user_data: *mut c_void, opaque: bool) {
    // SAFETY: `user_data` is the `Sender` installed below, alive until the
    // application is freed.
    let sender = unsafe { &*user_data.cast::<Sender<bool>>() };
    let _ = sender.send(opaque);
}

/// Installs callbacks that report `on_appearance_changed` on the returned
/// channel; `sender` must outlive the application.
fn install(app: *mut App, sender: &Sender<bool>) {
    let callbacks = Callbacks {
        user_data: ptr::from_ref(sender).cast_mut().cast(),
        on_session_output: None,
        on_session_title: None,
        on_session_bell: None,
        on_session_exit: None,
        on_appearance_changed: Some(on_appearance),
        on_session_clipboard: None,
    };
    // SAFETY: live handle and a valid table; `sender` outlives the app.
    assert_eq!(
        unsafe { tessera_app_set_callbacks(app, &raw const callbacks) },
        Error::Ok
    );
}

fn expect_change(changes: &Receiver<bool>, what: &str) -> bool {
    changes
        .recv_timeout(BOUND)
        .unwrap_or_else(|e| panic!("{what}: no on_appearance_changed within {BOUND:?}: {e}"))
}

/// The theme `config.toml` names is used at start; edits apply live and are reported; freeing stops the watch.
fn configured_theme_and_live_edits(root: &Path) {
    // A theme named in config.toml, found in themes/.
    let dir = root.join("Tessera");
    std::fs::create_dir_all(dir.join("themes")).unwrap();
    std::fs::write(dir.join("themes/mine.toml"), theme_doc("Mine", "#123456")).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[appearance]\ntheme = \"mine\"\nfont_size = 15\n",
    )
    .unwrap();
    let app = new_app(&dir);
    assert_eq!(background(app), [0x12, 0x34, 0x56]);
    assert_eq!(
        with_core(app, |core| core.config().appearance.theme.clone()),
        "mine"
    );
    assert_eq!(
        with_core(app, |core| core.config().appearance.font_size),
        15.0
    );
    assert!(with_core(app, tessera_app_core::App::is_watching_config));

    // The watch applies an edit of that file and tells the host.
    let (tx, changes) = mpsc::channel();
    install(app, &tx);
    std::fs::write(dir.join("themes/mine.toml"), theme_doc("Mine", "#654321")).unwrap();
    assert!(expect_change(&changes, "theme edit"), "still opaque");
    assert_eq!(background(app), [0x65, 0x43, 0x21]);
    // A translucent opacity is reported as not opaque.
    std::fs::write(
        dir.join("config.toml"),
        "[appearance]\ntheme = \"mine\"\nopacity = 0.75\n",
    )
    .unwrap();
    assert!(!expect_change(&changes, "opacity"), "translucent");

    // Freeing stops the watch promptly; nothing is reported afterwards.
    let started = Instant::now();
    // SAFETY: live handle, freed once.
    unsafe { tessera_app_free(app) };
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    std::fs::write(dir.join("themes/mine.toml"), theme_doc("Mine", "#000000")).unwrap();
    assert!(changes.recv_timeout(Duration::from_millis(500)).is_err());
    drop(tx);
}

/// No directory: starting creates and watches nothing until `tessera_app_create_config`.
fn missing_directory_until_created(root: &Path) {
    // No directory yet: starting creates nothing and watches nothing;
    // `tessera_app_create_config` creates it and starts the watch.
    let fresh = root.join("Fresh");
    let app = new_app(&fresh);
    assert!(!fresh.exists(), "starting created {}", fresh.display());
    assert_eq!(background(app), [0, 0, 0], "Tessera Dark");
    assert!(!with_core(app, tessera_app_core::App::is_watching_config));
    let (tx, changes) = mpsc::channel();
    install(app, &tx);
    // SAFETY: live handle.
    assert_eq!(unsafe { tessera_app_create_config(app) }, Error::Ok);
    assert!(fresh.join("config.toml").is_file());
    assert!(fresh.join("themes").is_dir());
    assert!(with_core(app, tessera_app_core::App::is_watching_config));
    let default_file = std::fs::read_to_string(fresh.join("config.toml")).unwrap();
    // SAFETY: live handle; a second call changes nothing.
    assert_eq!(unsafe { tessera_app_create_config(app) }, Error::Ok);
    assert_eq!(
        std::fs::read_to_string(fresh.join("config.toml")).unwrap(),
        default_file
    );
    std::fs::write(
        fresh.join("config.toml"),
        "[appearance]\ntheme = \"Nord\"\n",
    )
    .unwrap();
    expect_change(&changes, "edit after create_config");
    assert_eq!(background(app), [0x2e, 0x34, 0x40]);
    // SAFETY: live handle, freed once.
    unsafe { tessera_app_free(app) };
    drop(tx);
}

/// A directory without `config.toml` is watched and not written to.
fn directory_without_a_file(root: &Path) {
    // A directory without config.toml is watched, and still not written.
    let empty = root.join("Empty");
    std::fs::create_dir_all(&empty).unwrap();
    let app = new_app(&empty);
    assert!(with_core(app, tessera_app_core::App::is_watching_config));
    assert_eq!(
        std::fs::read_dir(&empty).unwrap().count(),
        0,
        "nothing written"
    );
    // SAFETY: live handle, freed once.
    unsafe { tessera_app_free(app) };
}

/// A `config.toml` that does not parse gives the defaults; the repair applies.
fn broken_config(root: &Path) {
    // A config.toml that does not parse: the defaults, not an error.
    let broken = root.join("Broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(
        broken.join("config.toml"),
        "[appearance\ntheme = \"Nord\"\n",
    )
    .unwrap();
    let app = new_app(&broken);
    assert_eq!(background(app), [0, 0, 0]);
    assert_eq!(
        with_core(app, |core| core.config().appearance.theme.clone()),
        "Tessera Dark"
    );
    // Repairing it applies it.
    let (tx, changes) = mpsc::channel();
    install(app, &tx);
    std::fs::write(
        broken.join("config.toml"),
        "[appearance]\ntheme = \"Nord\"\n",
    )
    .unwrap();
    expect_change(&changes, "repaired config");
    assert_eq!(background(app), [0x2e, 0x34, 0x40]);
    // SAFETY: live handle, freed once.
    unsafe { tessera_app_free(app) };
    drop(tx);
}

/// A directory that cannot be created: defaults, no watch, no error from starting.
fn uncreatable_directory(root: &Path) {
    // A directory that cannot be created (a file is in the way): the
    // defaults, without a watch, and still no error from starting; only
    // `tessera_app_create_config` fails.
    let blocked = root.join("file");
    std::fs::write(&blocked, "not a directory").unwrap();
    let app = new_app(&blocked.join("Tessera"));
    assert_eq!(background(app), [0, 0, 0]);
    assert!(!with_core(app, tessera_app_core::App::is_watching_config));
    // SAFETY: live handle.
    assert_eq!(unsafe { tessera_app_create_config(app) }, Error::Io);
    // SAFETY: NULL is rejected.
    assert_eq!(
        unsafe { tessera_app_create_config(ptr::null_mut()) },
        Error::NullPointer
    );
    // SAFETY: live handle, freed once.
    unsafe { tessera_app_free(app) };
}

#[test]
fn the_configured_theme_applies_at_start_and_edits_apply_live() {
    let root = tempfile::tempdir().unwrap();
    configured_theme_and_live_edits(root.path());
    missing_directory_until_created(root.path());
    directory_without_a_file(root.path());
    broken_config(root.path());
    uncreatable_directory(root.path());
}
