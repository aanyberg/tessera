//! `tessera_config_add_profile` at its edges (#312, ADR 0040): every
//! out-pointer given is 0 when another is `NULL` (adversarial M1); a
//! profile of exactly `TESSERA_MAX_CONFIG_VALUE_LEN` bytes is taken and
//! one byte more is not; a member given twice takes its last value, as
//! documented; and a key that is too long once prefixed with
//! `profiles[N].` is refused.
//!
//! One test, in a binary of its own: it sets `TESSERA_CONFIG_DIR`, which no
//! other test may see.

#![cfg(target_os = "macos")]

use std::path::Path;
use std::ptr;

use tessera_ffi::{
    App, Error, MAX_CONFIG_VALUE_LEN, MAX_PROFILE_NAME_LEN, tessera_app_free, tessera_app_new,
    tessera_config_add_profile,
};

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

/// What one call returned and wrote.
struct Added {
    code: Error,
    index: usize,
    name: String,
    message: String,
}

/// Adds `json` with every out-pointer given.
fn add(app: *mut App, json: &[u8]) -> Added {
    let mut index = 99;
    let mut name = [0u8; MAX_PROFILE_NAME_LEN];
    let mut name_len = 99;
    let mut msg = [0u8; 512];
    let mut msg_len = 0;
    // SAFETY: `json` is valid for its length; every out-pointer is
    // writable storage of the size given.
    let code = unsafe {
        tessera_config_add_profile(
            app,
            json.as_ptr(),
            json.len(),
            &raw mut index,
            name.as_mut_ptr(),
            name.len(),
            &raw mut name_len,
            msg.as_mut_ptr(),
            msg.len(),
            &raw mut msg_len,
        )
    };
    Added {
        code,
        index,
        name: String::from_utf8(name[..name_len].to_vec()).unwrap(),
        message: String::from_utf8_lossy(&msg[..msg_len]).into_owned(),
    }
}

/// A profile object of exactly `len` bytes: `name` and a padded `command`.
fn profile_of_len(name: &str, len: usize) -> Vec<u8> {
    let head = format!("{{\"name\": \"{name}\", \"command\": \"/bin/sh ");
    let tail = "\"}";
    let pad = len - head.len() - tail.len();
    let json = format!("{head}{}{tail}", "x".repeat(pad));
    assert_eq!(json.len(), len);
    json.into_bytes()
}

#[test]
fn add_profile_zeroes_its_outputs_and_takes_a_profile_up_to_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let app = new_app(dir.path());
    let config = dir.path().join("config.toml");
    let work = b"{\"name\": \"Work\"}";

    // A NULL name_buf or name_len: the others given are 0, nothing written.
    for null in ["name_buf", "name_len", "index"] {
        let mut index = 99;
        let mut name = [0u8; MAX_PROFILE_NAME_LEN];
        let mut name_len = 99;
        let index_ptr = if null == "index" {
            ptr::null_mut()
        } else {
            &raw mut index
        };
        let name_ptr = if null == "name_buf" {
            ptr::null_mut()
        } else {
            name.as_mut_ptr()
        };
        let len_ptr = if null == "name_len" {
            ptr::null_mut()
        } else {
            &raw mut name_len
        };
        // SAFETY: each pointer is NULL or writable storage of its size.
        let code = unsafe {
            tessera_config_add_profile(
                app,
                work.as_ptr(),
                work.len(),
                index_ptr,
                name_ptr,
                name.len(),
                len_ptr,
                ptr::null_mut(),
                0,
                ptr::null_mut(),
            )
        };
        assert_eq!(code, Error::NullPointer, "{null}");
        if null != "index" {
            assert_eq!(index, 0, "{null}");
        }
        if null != "name_len" {
            assert_eq!(name_len, 0, "{null}");
        }
    }
    assert!(!config.exists());

    // One byte past the cap is refused before it is parsed; exactly the
    // cap is taken.
    let over = add(app, &profile_of_len("over", MAX_CONFIG_VALUE_LEN + 1));
    assert_eq!(over.code, Error::InvalidArgument, "{}", over.message);
    assert_eq!((over.index, over.name.as_str()), (0, ""));
    let exact = add(app, &profile_of_len("exact", MAX_CONFIG_VALUE_LEN));
    assert_eq!(exact.code, Error::Ok, "{}", exact.message);
    assert_eq!((exact.index, exact.name.as_str()), (0, "exact"));
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("name = \"exact\""), "{}", text.len());

    // A member given twice takes its last value, `name` too (documented).
    let twice = add(
        app,
        b"{\"name\": \"first\", \"command\": \"/bin/a\", \"name\": \"last\", \"command\": \"/bin/b\"}",
    );
    assert_eq!(twice.code, Error::Ok, "{}", twice.message);
    assert_eq!((twice.index, twice.name.as_str()), (1, "last"));
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("command = \"/bin/b\""), "{text:.200}");
    assert!(!text.contains("\"/bin/a\"") && !text.contains("\"first\""));

    // A key of 1024 bytes parses alone, and is too long with its prefix.
    let key = "k".repeat(1024);
    let long = add(
        app,
        format!("{{\"name\": \"long\", \"{key}\": 1}}").as_bytes(),
    );
    assert_eq!(long.code, Error::InvalidArgument, "{}", long.message);
    assert_eq!((long.index, long.name.as_str()), (0, ""));
    assert!(!std::fs::read_to_string(&config).unwrap().contains("long"));

    // SAFETY: `app` is live and freed once.
    unsafe { tessera_app_free(app) };
}
