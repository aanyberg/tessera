//! Helpers shared by the integration tests.

use std::path::Path;
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
