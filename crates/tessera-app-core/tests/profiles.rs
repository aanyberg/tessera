//! Spawning a real session with a profile (issue #39): the profile's
//! `command` runs in place of the login shell, in its
//! `working_directory`, with `~` the home directory and a missing
//! directory falling back to it; an unknown profile name falls back to
//! `[general] default_profile`.
//!
//! Each profile command prints its directory and a marker, then waits in
//! `cat` (no CPU, killed by `close_session`). Waits are bounded by
//! attempts, not by a clock, so a loaded machine stretches them instead
//! of failing them.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use tessera_app_core::{App, Config, ConfigPaths, PtySize, SessionId, SpawnOptions};

/// Pause between two polls.
const POLL: Duration = Duration::from_millis(10);
/// Polls before a wait for output gives up: 30 s on an idle machine.
const OUTPUT_ATTEMPTS: usize = 3000;

/// The command every profile here runs: its physical directory, then a
/// marker, then an idle wait.
const PRINT_DIR: &str = "pwd -P; echo ran-$((6 * 7)); exec cat";

/// A configuration whose profiles all run [`PRINT_DIR`], each in the
/// working directory given, `None` for none.
fn config(profiles: &[(&str, Option<&str>)], default_profile: &str) -> Config {
    let mut text = format!("[general]\ndefault_profile = \"{default_profile}\"\n");
    for (name, dir) in profiles {
        writeln!(
            text,
            "[[profiles]]\nname = \"{name}\"\ncommand = \"{PRINT_DIR}\""
        )
        .unwrap();
        if let Some(dir) = dir {
            writeln!(text, "working_directory = '{dir}'").unwrap();
        }
    }
    let loaded = Config::from_toml_lenient(&text).expect("a valid configuration");
    assert_eq!(loaded.warnings, [], "{text}");
    loaded.config
}

fn app(root: &tempfile::TempDir, config: Config) -> App {
    App::new(config, ConfigPaths::new(root.path().join("config")))
}

/// Spawns the login shell's options with `profile` and waits until its
/// command has printed its marker; the directory it printed.
fn directory_of(app: &App, profile: Option<&str>) -> PathBuf {
    // Wide enough that no temporary directory's path wraps.
    let options = SpawnOptions::login_shell("").size(PtySize::new(24, 400));
    let id = app
        .spawn_session_with_profile(options, profile)
        .expect("spawn with a profile");
    let rows = wait_for_marker(app, id);
    app.close_session(id).expect("close");
    let at = rows
        .iter()
        .position(|row| row == "ran-42")
        .expect("the marker");
    assert!(at > 0, "a directory before the marker: {rows:?}");
    PathBuf::from(&rows[at - 1])
}

/// The visible rows, trailing blanks trimmed, once one reads `ran-42`.
fn wait_for_marker(app: &App, id: SessionId) -> Vec<String> {
    let mut rows = Vec::new();
    for _ in 0..OUTPUT_ATTEMPTS {
        rows = app
            .with_terminal(id, |term| {
                term.grid()
                    .visible_lines()
                    .iter()
                    .map(|line| line.text().trim_end().to_owned())
                    .collect::<Vec<_>>()
            })
            .expect("live session");
        if rows.iter().any(|row| row == "ran-42") {
            return rows;
        }
        thread::sleep(POLL);
    }
    panic!("the profile's command never ran: {rows:?}");
}

fn physical(path: &Path) -> PathBuf {
    path.canonicalize().expect("an existing directory")
}

fn home() -> PathBuf {
    physical(Path::new(&std::env::var_os("HOME").expect("HOME is set")))
}

/// Done-when 2: the profile's command runs, in its working directory.
#[test]
fn a_profile_runs_its_command_in_its_working_directory() {
    let root = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().to_str().unwrap();
    let app = app(&root, config(&[("here", Some(dir))], "Default"));
    assert_eq!(directory_of(&app, Some("here")), physical(work.path()));
}

/// Done-when 2: `~` is the home directory, and a directory that does not
/// exist falls back to it (with a warning, `profile` unit tests).
#[test]
fn a_tilde_and_a_missing_directory_both_start_at_home() {
    let root = tempfile::tempdir().unwrap();
    let app = app(
        &root,
        config(
            &[
                ("tilde", Some("~")),
                ("missing", Some("/nonexistent/tessera/profile-dir")),
                ("unset", None),
            ],
            "Default",
        ),
    );
    assert_eq!(directory_of(&app, Some("tilde")), home());
    assert_eq!(directory_of(&app, Some("missing")), home());
    assert_eq!(directory_of(&app, Some("unset")), home());
}

/// A name no profile has falls back to `[general] default_profile`, and
/// naming none uses it too.
#[test]
fn an_unknown_or_absent_profile_name_uses_the_default_profile() {
    let root = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().to_str().unwrap();
    let app = app(&root, config(&[("work", Some(dir))], "work"));
    assert_eq!(
        directory_of(&app, Some("no such profile")),
        physical(work.path())
    );
    assert_eq!(directory_of(&app, None), physical(work.path()));
}

/// The caller's own program wins over the profile's command: a spawn that
/// names one runs it, in the profile's directory.
#[test]
fn an_explicit_program_wins_over_the_profiles_command() {
    let root = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().to_str().unwrap();
    let mut config = config(&[("here", Some(dir))], "Default");
    config.profiles[0].command = Some("/usr/bin/false".into());
    let app = app(&root, config);
    let options = SpawnOptions::command("/bin/sh", "")
        .arg("-c")
        .arg(PRINT_DIR)
        .size(PtySize::new(24, 400));
    let id = app
        .spawn_session_with_profile(options, Some("here"))
        .expect("spawn");
    let rows = wait_for_marker(&app, id);
    app.close_session(id).expect("close");
    assert!(
        rows.iter()
            .any(|row| Path::new(row) == physical(work.path())),
        "{rows:?}"
    );
}
