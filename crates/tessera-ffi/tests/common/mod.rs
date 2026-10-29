//! Helpers shared by the integration tests: they keep the tests out of the
//! real home directory.
//!
//! Both change the process environment, which is sound because nextest (the
//! workspace's runner: `just test`, the quality gate) runs each test in its
//! own process; plain `cargo test` runs the tests of a binary as threads of
//! one process, where it could race another test's spawn.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Points `tessera_app_new` (through `TESSERA_CONFIG_DIR`) at a temporary
/// configuration directory for the rest of this test process, so the tests
/// neither read nor create `~/Library/Application Support/Tessera` and
/// every application starts with the defaults. Call it before the first
/// `tessera_app_new`; later calls return the same directory.
pub fn hermetic_config_dir() -> &'static Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("temporary config directory");
        // SAFETY: set once, before this process's first `tessera_app_new`
        // reads it; std's own environment accesses are serialised, and
        // under nextest (one process per test) no other thread runs yet.
        unsafe { std::env::set_var("TESSERA_CONFIG_DIR", dir.path()) };
        dir
    })
    .path()
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
