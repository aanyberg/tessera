//! `tessera_config_set` repairs a failing file (#449, ADR 0040): a
//! wrong-typed scalar in an inline profile of an inline `profiles` array is
//! replaced and saved, not refused as if the whole array were rejected
//! (adversarial B1).
//!
//! One test, in a binary of its own: it sets `TESSERA_CONFIG_DIR`, which no
//! other test may see.

#![cfg(target_os = "macos")]

use std::ptr;

use tessera_ffi::{Error, tessera_app_free, tessera_app_new, tessera_config_set};

#[test]
fn a_broken_inline_profile_command_is_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file = root.join("config.toml");
    let broken = "profiles = [\n  { name = \"Work\", command = 3 },\n]\n";
    std::fs::write(&file, broken).unwrap();
    // SAFETY: this binary's only test runs on one thread, and nothing else
    // in the process reads the environment concurrently.
    unsafe { std::env::set_var("TESSERA_CONFIG_DIR", &root) };
    let mut app = ptr::null_mut();
    // SAFETY: `app` is writable storage for one pointer.
    assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
    assert!(!app.is_null());

    let key = "profiles[0].command";
    let json = "\"/bin/zsh\"";
    let mut msg = [0u8; 512];
    let mut msg_len = 0;
    // SAFETY: `key` and `json` are valid for their lengths; `msg` and
    // `msg_len` are writable storage of the size given.
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
    let message = String::from_utf8_lossy(&msg[..msg_len]).into_owned();
    assert_eq!(code, Error::Ok, "{message}");

    let text = std::fs::read_to_string(&file).unwrap();
    assert_ne!(text, broken);
    let config = tessera_config::Config::from_toml_lenient(&text).unwrap();
    assert_eq!(
        config.config.profiles[0].command.as_deref(),
        Some("/bin/zsh")
    );
    assert_eq!(config.config.profiles[0].name, "Work");

    // SAFETY: `app` came from `tessera_app_new` and is freed once.
    unsafe { tessera_app_free(app) };
}
