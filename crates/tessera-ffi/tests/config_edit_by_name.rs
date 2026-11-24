//! `tessera_config_set_profile` and `tessera_config_remove_profile`
//! (#565, ADR 0044): a profile's edit addressed by the profile's name is
//! looked up in `config.toml` as the call reads it, so an edit made before
//! a hand edit that inserted a profile ahead of it lands on its profile;
//! a name no entry has is `TESSERA_ERROR_NO_SUCH_PROFILE` and writes
//! nothing; of two entries with one name the one of the ordinal shown is
//! edited, also after a hand edit added a profile ahead of them; an empty
//! key removes the entry; the name's bounds, `NULL` and UTF-8 checks come
//! before any file is touched.
//!
//! One test, in a binary of its own: it sets `TESSERA_CONFIG_DIR`, which no
//! other test may see.

#![cfg(target_os = "macos")]

use std::path::Path;
use std::ptr;

use tessera_ffi::{
    App, Error, MAX_PROFILE_NAME_LEN, tessera_app_free, tessera_app_new,
    tessera_config_remove_profile, tessera_config_set_profile,
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

/// Sets `key` of the profile `name` of `ordinal` to `json`; the code
/// and message.
fn set(app: *mut App, name: &[u8], ordinal: usize, key: &str, json: &str) -> (Error, String) {
    let mut msg = [0u8; 512];
    let mut msg_len = 99;
    // SAFETY: every buffer is valid for its length; `msg_len` is writable.
    let code = unsafe {
        tessera_config_set_profile(
            app,
            name.as_ptr(),
            name.len(),
            ordinal,
            key.as_ptr(),
            key.len(),
            json.as_ptr(),
            json.len(),
            msg.as_mut_ptr(),
            msg.len(),
            &raw mut msg_len,
        )
    };
    (code, String::from_utf8_lossy(&msg[..msg_len]).into_owned())
}

/// Removes `key` of the profile `name` of `ordinal`; the code and
/// message.
fn remove(app: *mut App, name: &[u8], ordinal: usize, key: &str) -> (Error, String) {
    let mut msg = [0u8; 512];
    let mut msg_len = 99;
    // SAFETY: as for `set`.
    let code = unsafe {
        tessera_config_remove_profile(
            app,
            name.as_ptr(),
            name.len(),
            ordinal,
            key.as_ptr(),
            key.len(),
            msg.as_mut_ptr(),
            msg.len(),
            &raw mut msg_len,
        )
    };
    (code, String::from_utf8_lossy(&msg[..msg_len]).into_owned())
}

/// The names and commands of the file's profiles.
fn profiles(config: &Path) -> Vec<(String, Option<String>)> {
    let text = std::fs::read_to_string(config).unwrap();
    let loaded = tessera_config::Config::from_toml_lenient(&text).unwrap();
    loaded
        .config
        .profiles
        .iter()
        .map(|p| (p.name.clone(), p.command.clone()))
        .collect()
}

fn entry(name: &str, command: Option<&str>) -> (String, Option<String>) {
    (name.to_owned(), command.map(str::to_owned))
}

#[test]
fn a_profile_edit_by_name_finds_its_profile_in_the_file_written() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let text = "[[profiles]]\nname = \"Work\"\n\n[[profiles]]\nname = \"Shell\"\n";
    std::fs::write(&config, text).unwrap();
    let app = new_app(dir.path());

    let (code, message) = set(app, b"Shell", 0, "command", "\"/bin/zsh\"");
    assert_eq!(code, Error::Ok, "{message}");
    assert_eq!(message, "", "no message on success");
    assert_eq!(
        profiles(&config),
        [entry("Work", None), entry("Shell", Some("/bin/zsh"))]
    );

    // A hand edit inserts a profile first, after the app read the file
    // (it had "Shell" at index 1) and before the watch applied it.
    let hand = format!(
        "[[profiles]]\nname = \"Hand\"\n\n{}",
        std::fs::read_to_string(&config).unwrap()
    );
    std::fs::write(&config, &hand).unwrap();
    let (code, message) = set(app, b"Shell", 0, "command", "\"/bin/fish\"");
    assert_eq!(code, Error::Ok, "{message}");
    assert_eq!(
        profiles(&config),
        [
            entry("Hand", None),
            entry("Work", None),
            entry("Shell", Some("/bin/fish"))
        ],
        "on “Shell”, not on “Work”, now at its old index"
    );

    // A name no entry has: nothing written, its own code and a message.
    let before = std::fs::read(&config).unwrap();
    let (code, message) = set(app, b"Gone", 0, "command", "\"/bin/sh\"");
    assert_eq!(code, Error::NoSuchProfile, "{message}");
    assert!(message.contains("Gone"), "{message}");
    let (code, _) = remove(app, b"Gone", 0, "");
    assert_eq!(code, Error::NoSuchProfile);
    let (code, _) = remove(app, b"Gone", 0, "command");
    assert_eq!(code, Error::NoSuchProfile);

    // The name's checks: past the cap, not UTF-8, NULL with a length; an
    // empty key for a set; each writes nothing.
    let long = vec![b'a'; MAX_PROFILE_NAME_LEN + 1];
    assert_eq!(
        set(app, &long, 0, "command", "\"x\"").0,
        Error::InvalidArgument
    );
    assert_eq!(remove(app, &long, 0, "").0, Error::InvalidArgument);
    let exact = vec![b'a'; MAX_PROFILE_NAME_LEN];
    assert_eq!(
        set(app, &exact, 0, "command", "\"x\"").0,
        Error::NoSuchProfile
    );
    assert_eq!(
        set(app, b"Sh\xffell", 0, "command", "\"x\"").0,
        Error::InvalidArgument
    );
    assert_eq!(set(app, b"Shell", 0, "", "\"x\"").0, Error::InvalidArgument);
    let mut msg_len = 99;
    // SAFETY: a NULL name with a length is checked before it is read.
    let code = unsafe {
        tessera_config_set_profile(
            app,
            ptr::null(),
            5,
            0,
            b"command".as_ptr(),
            7,
            b"\"x\"".as_ptr(),
            3,
            ptr::null_mut(),
            0,
            &raw mut msg_len,
        )
    };
    assert_eq!(code, Error::NullPointer);
    assert_eq!(msg_len, 0, "the message cut to a cap of 0");
    // A value the loader warns about is refused, as for any set.
    assert_eq!(
        set(app, b"Shell", 0, "appearance.opacity", "7.5").0,
        Error::Refused
    );
    assert_eq!(std::fs::read(&config).unwrap(), before);

    // Removed by name: a key, then the entry itself.
    assert_eq!(remove(app, b"Shell", 0, "command").0, Error::Ok);
    assert_eq!(
        remove(app, b"Shell", 0, "command").0,
        Error::Ok,
        "nothing left"
    );
    assert_eq!(remove(app, b"Hand", 0, "").0, Error::Ok);
    assert_eq!(
        profiles(&config),
        [entry("Work", None), entry("Shell", None)]
    );

    two_profiles_of_one_name(app, &config);

    // SAFETY: `app` is live and freed once.
    unsafe { tessera_app_free(app) };
}

/// Two entries with one name, listed as Keep, Twin, Twin: the second of
/// them (ordinal 1) is edited and deleted, also after a hand edit inserted
/// a profile ahead of them; an ordinal past the last writes nothing.
fn two_profiles_of_one_name(app: *mut App, config: &Path) {
    let twins = "[[profiles]]\nname = \"Hand\"\n\n[[profiles]]\nname = \"Keep\"\n\n\
                 [[profiles]]\nname = \"Twin\"\n\n[[profiles]]\nname = \"Twin\"\n";
    std::fs::write(config, twins).unwrap();
    assert_eq!(
        set(app, b"Twin", 1, "command", "\"/bin/second\"").0,
        Error::Ok
    );
    assert_eq!(
        profiles(config),
        [
            entry("Hand", None),
            entry("Keep", None),
            entry("Twin", None),
            entry("Twin", Some("/bin/second"))
        ]
    );
    let before = std::fs::read(config).unwrap();
    let (code, message) = set(app, b"Twin", 2, "command", "\"/bin/x\"");
    assert_eq!(code, Error::NoSuchProfile, "{message}");
    assert!(message.contains("Twin"), "{message}");
    assert_eq!(remove(app, b"Twin", 9, "").0, Error::NoSuchProfile);
    assert_eq!(std::fs::read(config).unwrap(), before, "nothing written");
    assert_eq!(remove(app, b"Twin", 1, "").0, Error::Ok);
    assert_eq!(
        profiles(config),
        [
            entry("Hand", None),
            entry("Keep", None),
            entry("Twin", None)
        ]
    );
}
