//! The temporary configuration directory that `common::hermetic_config_dir`
//! gives a test process is removed when that process exits, after a
//! passing test and after a failing one (#698). It lived in a `static`,
//! which is never dropped, so every test process that made one left a
//! `.tmpXXXXXX` directory in `$TMPDIR`.
//!
//! The test runs this binary again as a child for each outcome and checks
//! that the directory the child printed is gone once the child has exited.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;

/// Set in the child's environment to `pass` or `fail`: the outcome
/// [`child_makes_a_config_dir`] ends with.
const CHILD: &str = "TESSERA_FFI_CONFIG_DIR_CHILD";

/// The child's half: does nothing in a normal run. Under [`CHILD`] it makes
/// the configuration directory, puts a file in it (so removing it is a
/// recursive removal, as after an app wrote its shell-integration files),
/// prints its path, and then passes or panics.
#[test]
fn child_makes_a_config_dir() {
    let Some(outcome) = std::env::var_os(CHILD) else {
        return;
    };
    common::isolate_home();
    let dir = common::hermetic_config_dir();
    std::fs::write(dir.join("config.toml"), "").expect("write into the config directory");
    println!("config dir: {}", dir.display());
    assert_eq!(outcome, "pass", "this child was told to fail");
}

#[test]
fn the_config_dir_is_removed_at_exit_after_a_pass_and_after_a_failure() {
    let exe = std::env::current_exe().expect("this test binary");
    for outcome in ["pass", "fail"] {
        let out = Command::new(&exe)
            .args(["--exact", "child_makes_a_config_dir", "--nocapture"])
            .env(CHILD, outcome)
            .output()
            .expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            out.status.success(),
            outcome == "pass",
            "the {outcome} child exited with {}:\n{stdout}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let dir = stdout
            .lines()
            .find_map(|line| line.strip_prefix("config dir: "))
            .unwrap_or_else(|| panic!("the {outcome} child printed no directory:\n{stdout}"));
        assert!(
            !Path::new(dir).exists(),
            "the {outcome} child left its config directory {dir}"
        );
    }
}
