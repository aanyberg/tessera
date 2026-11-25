//! Helpers shared by the integration tests: they keep the tests out of the
//! real home directory.
//!
//! Both change the process environment, which is sound because nextest (the
//! workspace's runner: `just test`, the quality gate) runs each test in its
//! own process; plain `cargo test` runs the tests of a binary as threads of
//! one process, where it could race another test's spawn.

use std::ffi::c_int;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Points `tessera_app_new` (through `TESSERA_CONFIG_DIR`) at a temporary
/// configuration directory for the rest of this test process, so the tests
/// neither read nor create `~/Library/Application Support/Tessera` and
/// every application starts with the defaults. Call it before the first
/// `tessera_app_new`; later calls return the same directory.
///
/// The directory is removed when the process exits, whether its test
/// passed or failed (#698): a `static` is never dropped, so the
/// [`tempfile::TempDir`] lives in [`CONFIG_DIR`] and an `atexit` handler
/// drops it. `atexit` handlers run only on `exit`: a process that aborts
/// (`std::process::abort`, a panic that aborts) or is killed by a signal,
/// SIGKILL included (a nextest timeout ends with one), leaves the
/// directory behind.
pub fn hermetic_config_dir() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = tempfile::tempdir().expect("temporary config directory");
        // SAFETY: set once, before this process's first `tessera_app_new`
        // reads it; std's own environment accesses are serialised, and
        // under nextest (one process per test) no other thread runs yet.
        unsafe { std::env::set_var("TESSERA_CONFIG_DIR", dir.path()) };
        let path = dir.path().to_path_buf();
        *CONFIG_DIR.lock().expect("config directory lock") = Some(dir);
        // SAFETY: `atexit` (libSystem, always linked) only stores the
        // pointer of a function that takes no argument and never unwinds;
        // this runs once per process (inside the `OnceLock`).
        let registered = unsafe { atexit(remove_config_dir) };
        assert_eq!(
            registered, 0,
            "atexit refused the config directory's cleanup"
        );
        path
    })
    .as_path()
}

/// The directory [`hermetic_config_dir`] made, until the process exits.
static CONFIG_DIR: Mutex<Option<tempfile::TempDir>> = Mutex::new(None);

/// Removes the configuration directory at exit (registered with `atexit`).
/// Dropping the [`tempfile::TempDir`] ignores removal errors and cannot
/// panic; a lock that is held or poisoned leaves the directory in place
/// instead of blocking or unwinding out of the handler.
extern "C" fn remove_config_dir() {
    if let Ok(mut dir) = CONFIG_DIR.try_lock() {
        drop(dir.take());
    }
}

unsafe extern "C" {
    /// C's `atexit(3)`: runs `callback` when the process calls `exit`, as
    /// libtest does after the last test, passed or failed.
    fn atexit(callback: extern "C" fn()) -> c_int;
}

/// The home directory the process started with, and the one the tests use.
pub struct Homes {
    /// `$HOME` before [`isolate_home`] replaced it.
    pub real: Option<PathBuf>,
    /// `<CARGO_TARGET_TMPDIR>/ffi-home`.
    pub test: PathBuf,
}

/// Points `HOME` at `<CARGO_TARGET_TMPDIR>/ffi-home`, once per process, and
/// returns both homes, so a login shell a test spawns reads no real startup
/// file. The test home has an empty `.zshrc`, so a zsh login
/// shell is injected as a real user's would be. Call it before creating an
/// app or starting a thread.
pub fn isolate_home() -> &'static Homes {
    static HOMES: OnceLock<Homes> = OnceLock::new();
    HOMES.get_or_init(|| {
        let real = std::env::var_os("HOME").map(PathBuf::from);
        let test = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ffi-home");
        std::fs::create_dir_all(&test).expect("create the test home");
        let zshrc = test.join(".zshrc");
        if !zshrc.exists() {
            std::fs::write(&zshrc, "").expect("write the test home's .zshrc");
        }
        // SAFETY: every harness calls this before it creates an app or
        // starts a thread, and the workspace runs tests with nextest, one
        // test per process, so no other thread reads the environment now.
        // (Under `cargo test` the tests of a binary share the process and
        // this could race another test's spawn.)
        // An inherited ZDOTDIR or XDG_CONFIG_HOME would still point a login
        // shell (and zsh's new-user check) at the real startup files.
        unsafe {
            std::env::set_var("HOME", &test);
            std::env::remove_var("ZDOTDIR");
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let homes = Homes { real, test };
        // Every binary reads both fields (so none needs a dead-code
        // allowance): the test home must not be the real one.
        assert_ne!(
            homes.real.as_deref(),
            Some(homes.test.as_path()),
            "HOME already is the test home"
        );
        homes
    })
}
