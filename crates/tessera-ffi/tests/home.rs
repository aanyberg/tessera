//! Guard: the FFI tests never touch the real home directory. With the
//! helpers of `common`, the configuration root of `tessera_app_new` (and the
//! shell integration scripts a login shell's spawn writes there) is a
//! temporary directory, a login shell runs in a test home, and the real
//! `~/Library/Application Support/Tessera` is left exactly as it was.

use std::path::{Path, PathBuf};
use std::ptr;
use std::time::SystemTime;

use tessera_ffi::{
    App, Error, SpawnOptions, tessera_app_config_path, tessera_app_free, tessera_app_new,
    tessera_session_close, tessera_session_spawn,
};

mod common;

/// What a directory tree looks like: every path under `root` (itself
/// included) with its modification time; `None` when `root` is missing.
fn snapshot(root: &Path) -> Option<Vec<(PathBuf, SystemTime)>> {
    fn walk(path: &Path, out: &mut Vec<(PathBuf, SystemTime)>) {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        out.push((path.to_owned(), meta.modified().expect("mtime")));
        if meta.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(path)
                .expect("read dir")
                .map(|entry| entry.expect("dir entry").path())
                .collect();
            entries.sort();
            for entry in entries {
                walk(&entry, out);
            }
        }
    }
    std::fs::symlink_metadata(root).ok()?;
    let mut out = Vec::new();
    walk(root, &mut out);
    Some(out)
}

fn app_support(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Tessera")
}

#[test]
fn the_app_and_a_login_shell_stay_out_of_the_real_home() {
    let homes = common::isolate_home();
    let config_dir = common::hermetic_config_dir();
    let real = homes.real.as_deref().map(app_support);
    let before = real.as_deref().map(snapshot);
    // A zsh login shell, so the spawn injects (and writes) the scripts.
    // SAFETY: no app exists and no thread has been started yet (nextest
    // runs this test alone in its process).
    unsafe { std::env::set_var("SHELL", "/bin/zsh") };

    let mut app: *mut App = ptr::null_mut();
    // SAFETY: `app` is writable storage for one pointer.
    assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
    let mut buf = vec![0u8; 4096];
    let mut len = 0;
    // SAFETY: live handle; `buf` has `buf.len()` writable bytes.
    assert_eq!(
        unsafe { tessera_app_config_path(app, buf.as_mut_ptr().cast(), buf.len(), &raw mut len) },
        Error::Ok
    );
    let config = PathBuf::from(String::from_utf8(buf[..len].to_vec()).unwrap());
    assert_eq!(config, config_dir.join("config.toml"));

    let options = SpawnOptions {
        program: ptr::null(),
        args: ptr::null(),
        arg_count: 0,
        cwd: ptr::null(),
        rows: 24,
        cols: 80,
        width_px: 0,
        height_px: 0,
    };
    let mut id = 0;
    // SAFETY: live handle; valid options; `id` is writable.
    assert_eq!(
        unsafe { tessera_session_spawn(app, &raw const options, &raw mut id) },
        Error::Ok
    );
    // The scripts are written before the shell starts.
    let versions = config_dir.join("shell-integration");
    let written = std::fs::read_dir(&versions).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path().join("zsh/.zshenv").is_file())
    });
    assert!(written, "no scripts under {}", versions.display());
    // SAFETY: live handle and session; the handle is freed once.
    unsafe {
        assert_eq!(tessera_session_close(app, id), Error::Ok);
        tessera_app_free(app);
    }

    if let (Some(real), Some(before)) = (real, before) {
        assert_eq!(
            snapshot(&real),
            before,
            "{} changed during the test",
            real.display()
        );
    }
}

/// Every test file that creates an app gives it a temporary configuration
/// directory, and every one that spawns sessions isolates the home first
/// (a new test file that forgot would write into, or run the startup files
/// of, the real home without any test noticing).
#[test]
fn every_test_file_stays_out_of_the_real_home() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read tests/") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read a test file");
        if source.contains("tessera_app_new(&raw mut") {
            checked += 1;
            assert!(
                source.contains("common::hermetic_config_dir()")
                    || source.contains("set_var(\"TESSERA_CONFIG_DIR\""),
                "{} creates an app without a temporary configuration directory",
                path.display()
            );
        }
        if source.contains("tessera_session_spawn(") {
            assert!(
                source.contains("common::isolate_home()"),
                "{} spawns sessions without common::isolate_home()",
                path.display()
            );
        }
    }
    assert!(
        checked >= 5,
        "found only {checked} test files creating apps"
    );
}
