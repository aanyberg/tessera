//! `tessera_session_appearance` reports each session's opacity and blur
//! from its profile, and `tessera_app_set_display_options` with
//! `reduce_transparency` makes every session opaque and unblurred, tells
//! the host through `on_appearance_changed`, and is undone by turning it
//! off (ADR 0043).
//!
//! A binary of its own: it points `TESSERA_CONFIG_DIR` at a configuration
//! of its own, which no other test may see.

#![cfg(unix)]

mod common;

use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use tessera_ffi::{
    App, Callbacks, DisplayOptions, Error, SessionAppearance, SpawnOptions, tessera_app_free,
    tessera_app_new, tessera_app_set_callbacks, tessera_app_set_display_options,
    tessera_session_appearance, tessera_session_close, tessera_session_spawn,
};

/// `on_appearance_changed` runs on the calling thread before
/// `tessera_app_set_display_options` returns, so the wait never needs this
/// long; the bound only keeps a regression from hanging the run.
const BOUND: Duration = Duration::from_secs(5);

const CONFIG: &str = "[appearance]\n\
                      theme = \"Dracula\"\n\
                      opacity = 0.5\n\
                      blur = true\n\
                      [[profiles]]\n\
                      name = \"solid\"\n\
                      [profiles.appearance]\n\
                      opacity = 1.0\n\
                      [[profiles]]\n\
                      name = \"clear\"\n\
                      [profiles.appearance]\n\
                      blur = false\n";

unsafe extern "C" fn on_appearance(user_data: *mut c_void, opaque: bool) {
    // SAFETY: `user_data` is the `Sender` installed below, alive until the
    // application is freed.
    let sender = unsafe { &*user_data.cast::<Sender<bool>>() };
    let _ = sender.send(opaque);
}

fn install(app: *mut App, sender: &Sender<bool>) {
    let callbacks = Callbacks {
        user_data: ptr::from_ref(sender).cast_mut().cast(),
        on_session_output: None,
        on_session_title: None,
        on_session_bell: None,
        on_session_exit: None,
        on_appearance_changed: Some(on_appearance),
        on_session_clipboard: None,
        on_config_applied: None,
    };
    // SAFETY: live handle and a valid table; `sender` outlives the app.
    assert_eq!(
        unsafe { tessera_app_set_callbacks(app, &raw const callbacks) },
        Error::Ok
    );
}

fn spawn(app: *mut App, profile: &CStr) -> u64 {
    let args: [*const c_char; 2] = [c"-c".as_ptr(), c"sleep 30".as_ptr()];
    let options = SpawnOptions {
        program: c"/bin/sh".as_ptr(),
        args: args.as_ptr(),
        arg_count: args.len(),
        cwd: ptr::null(),
        rows: 24,
        cols: 80,
        width_px: 0,
        height_px: 0,
        profile: profile.as_ptr(),
    };
    let mut id = 0;
    // SAFETY: live handle; valid options; `id` is writable.
    assert_eq!(
        unsafe { tessera_session_spawn(app, &raw const options, &raw mut id) },
        Error::Ok
    );
    id
}

fn appearance(app: *mut App, id: u64) -> (bool, bool) {
    let mut out = SessionAppearance {
        opaque: false,
        blur: true,
    };
    // SAFETY: live handle; `out` is writable.
    assert_eq!(
        unsafe { tessera_session_appearance(app, id, &raw mut out) },
        Error::Ok
    );
    (out.opaque, out.blur)
}

fn set_reduce(app: *mut App, reduce_transparency: bool) {
    set_options(
        app,
        &DisplayOptions {
            reduce_transparency: u8::from(reduce_transparency),
        },
    );
}

fn set_options(app: *mut App, options: &DisplayOptions) {
    // SAFETY: live handle; `options` is readable.
    assert_eq!(
        unsafe { tessera_app_set_display_options(app, options) },
        Error::Ok
    );
}

/// The options as a C frontend that `memset`s the struct to `byte` passes
/// them.
fn memset_options(byte: u8) -> DisplayOptions {
    let mut options = std::mem::MaybeUninit::<DisplayOptions>::uninit();
    // SAFETY: writes every byte of the struct, which has no padding and
    // only `uint8_t` fields (ADR 0011), so any byte pattern is a valid
    // `DisplayOptions`.
    unsafe {
        options
            .as_mut_ptr()
            .write_bytes(byte, std::mem::size_of::<DisplayOptions>());
        options.assume_init()
    }
}

fn expect_change(changes: &Receiver<bool>, what: &str) -> bool {
    changes
        .recv_timeout(BOUND)
        .unwrap_or_else(|e| panic!("{what}: no on_appearance_changed within {BOUND:?}: {e}"))
}

#[test]
fn reduce_transparency_makes_every_session_opaque_until_turned_off() {
    common::isolate_home();
    let dir = common::hermetic_config_dir();
    std::fs::write(dir.join("config.toml"), CONFIG).unwrap();
    let mut app = ptr::null_mut();
    // SAFETY: `app` is writable storage for one pointer.
    assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
    let (tx, changes) = mpsc::channel();
    install(app, &tx);

    let base = spawn(app, c"no-such-profile");
    let solid = spawn(app, c"solid");
    let clear = spawn(app, c"clear");
    assert_eq!(appearance(app, base), (false, true), "[appearance]");
    assert_eq!(appearance(app, solid), (true, false), "opacity 1.0");
    assert_eq!(appearance(app, clear), (false, false), "blur off");

    set_reduce(app, true);
    assert!(expect_change(&changes, "reduce on"), "every view opaque");
    for id in [base, solid, clear] {
        assert_eq!(appearance(app, id), (true, false), "session {id}");
    }
    // Asking again changes nothing and says nothing.
    set_reduce(app, true);
    assert!(changes.try_recv().is_err(), "a repeat is silent");

    set_reduce(app, false);
    assert!(!expect_change(&changes, "reduce off"), "translucent again");
    assert_eq!(appearance(app, base), (false, true));
    assert_eq!(appearance(app, clear), (false, false));

    // SAFETY: live handle and sessions; freed once.
    unsafe {
        for id in [base, solid, clear] {
            assert_eq!(tessera_session_close(app, id), Error::Ok);
        }
        tessera_app_free(app);
    }
}

#[test]
fn the_display_options_are_one_byte_per_flag() {
    // The C layout #52 appends to: one `uint8_t` per flag, no padding.
    assert_eq!(std::mem::size_of::<DisplayOptions>(), 1);
    assert_eq!(std::mem::align_of::<DisplayOptions>(), 1);
}

#[test]
fn any_non_zero_byte_turns_reduce_transparency_on() {
    common::isolate_home();
    let dir = common::hermetic_config_dir();
    std::fs::write(dir.join("config.toml"), CONFIG).unwrap();
    let mut app = ptr::null_mut();
    // SAFETY: `app` is writable storage for one pointer.
    assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
    let id = spawn(app, c"no-such-profile");
    assert_eq!(appearance(app, id), (false, true), "translucent to start");

    for options in [
        DisplayOptions {
            reduce_transparency: 2,
        },
        memset_options(0x80),
        memset_options(0xff),
    ] {
        set_options(app, &options);
        assert_eq!(appearance(app, id), (true, false), "{options:?} is on");
        set_options(app, &memset_options(0));
        assert_eq!(appearance(app, id), (false, true), "a zeroed struct is off");
    }

    // SAFETY: live handle and session; freed once.
    unsafe {
        assert_eq!(tessera_session_close(app, id), Error::Ok);
        tessera_app_free(app);
    }
}

#[test]
fn null_pointers_and_unknown_sessions_fail_opaque() {
    common::isolate_home();
    common::hermetic_config_dir();
    let mut app = ptr::null_mut();
    // SAFETY: `app` is writable storage for one pointer.
    assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
    let options = DisplayOptions::default();
    let mut out = SessionAppearance {
        opaque: false,
        blur: true,
    };
    // SAFETY: NULL pointers are part of the contract; `app` is live.
    unsafe {
        assert_eq!(
            tessera_app_set_display_options(ptr::null(), &raw const options),
            Error::NullPointer
        );
        assert_eq!(
            tessera_app_set_display_options(app, ptr::null()),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_appearance(ptr::null(), 1, &raw mut out),
            Error::NullPointer
        );
        assert_eq!((out.opaque, out.blur), (true, false), "reset on failure");
        out.opaque = false;
        assert_eq!(
            tessera_session_appearance(app, 9999, &raw mut out),
            Error::SessionNotFound
        );
        assert_eq!((out.opaque, out.blur), (true, false), "reset on failure");
        assert_eq!(
            tessera_session_appearance(app, 1, ptr::null_mut()),
            Error::NullPointer
        );
        tessera_app_free(app);
    }
}
