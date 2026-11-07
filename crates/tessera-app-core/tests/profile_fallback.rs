//! A profile whose session cannot start (#333, lead decision F3): a
//! `working_directory` that exists but cannot be entered, or a `command`
//! that cannot be executed, starts the built-in profile instead (the login
//! shell in the home directory), once, with a one-line notice as the
//! terminal's first line. The notice's profile name is sanitised and
//! capped (docs/security.md S3); the warning logged carries the profile's
//! index and the `errno` only (S8).
//!
//! Every session gets `SHELL=/bin/sh` and `HOME` a directory of its own,
//! so the fallback is a known shell in a known place. Waits are bounded by
//! attempts, not by a clock.

#![cfg(unix)]

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tessera_app_core::{
    App, Config, ConfigPaths, Error, PtyError, PtySize, SessionId, SpawnOptions,
};

/// Pause between two polls.
const POLL: Duration = Duration::from_millis(10);
/// Polls before a wait for output gives up: 30 s on an idle machine.
const OUTPUT_ATTEMPTS: usize = 3000;

/// A command that cannot be executed: it does not exist.
const MISSING_COMMAND: &str = "/nonexistent/tessera-profile-command";

/// A directory nobody may enter, its permissions restored when dropped so
/// its temporary parent can be removed.
struct Locked(PathBuf);

impl Locked {
    fn new(parent: &Path) -> Self {
        let dir = parent.join("locked");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        Self(dir)
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// A configuration of `profiles` (name, command, working directory),
/// `default` its `[general] default_profile`.
fn config(profiles: &[(&str, Option<&str>, Option<&str>)], default: &str) -> Config {
    let mut text = format!("[general]\ndefault_profile = {}\n", quoted(default));
    for (name, command, dir) in profiles {
        writeln!(text, "[[profiles]]\nname = {}", quoted(name)).unwrap();
        if let Some(command) = command {
            writeln!(text, "command = {}", quoted(command)).unwrap();
        }
        if let Some(dir) = dir {
            writeln!(text, "working_directory = {}", quoted(dir)).unwrap();
        }
    }
    Config::from_toml_lenient(&text)
        .expect("a valid configuration")
        .config
}

/// `text` as a TOML basic string.
fn quoted(text: &str) -> String {
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => write!(out, "\\u{:04X}", u32::from(c)).unwrap(),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn app(root: &tempfile::TempDir, config: Config) -> App {
    App::new(config, ConfigPaths::new(root.path().join("config")))
}

/// The login shell's options, `/bin/sh` with `home` as `HOME`, wide
/// enough that no notice wraps.
fn options(home: &Path) -> SpawnOptions {
    SpawnOptions::login_shell("")
        .size(PtySize::new(24, 400))
        .env("SHELL", "/bin/sh")
        .env("HOME", home)
}

/// The visible rows, trailing blanks trimmed.
fn rows(app: &App, id: SessionId) -> Vec<String> {
    app.with_terminal(id, |term| {
        term.grid()
            .visible_lines()
            .iter()
            .map(|line| line.text().trim_end().to_owned())
            .collect::<Vec<_>>()
    })
    .expect("live session")
}

/// The rows once one of them is `wanted`.
fn wait_for_row(app: &App, id: SessionId, wanted: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for _ in 0..OUTPUT_ATTEMPTS {
        seen = rows(app, id);
        if seen.iter().any(|row| row == wanted) {
            return seen;
        }
        thread::sleep(POLL);
    }
    panic!("no row {wanted:?}: {seen:?}");
}

/// Checks the session is a working shell started in `home`: it runs a
/// command typed into it and prints `home` as its directory.
fn assert_a_shell_in(app: &App, id: SessionId, home: &Path) {
    app.write(id, b"echo ok-$((6 * 7)); pwd -P; echo done-$((6 * 7))\n")
        .unwrap();
    // The last marker: both lines before it are printed by then.
    let rows = wait_for_row(app, id, "done-42");
    let at = rows.iter().position(|row| row == "ok-42").expect("ok-42");
    let home = home.canonicalize().unwrap();
    assert_eq!(rows.get(at + 1).map(PathBuf::from), Some(home), "{rows:?}");
}

/// The notice for profile `name` that failed with `reason`.
fn notice(name: &str, reason: &str) -> String {
    format!("[Tessera] profile \"{name}\" could not start ({reason}); started the default shell.")
}

/// Every event at `WARN` or above on this thread, its fields as
/// `name=value`.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<String>>>);

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        write!(self.0, "{}={value:?} ", field.name()).unwrap();
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        // More verbose levels compare greater.
        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        self.0.lock().unwrap().push(line);
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Runs `f` and returns what it logged, on this thread, at `WARN` or above.
fn warnings<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    let capture = Capture::default();
    let out = tracing::subscriber::with_default(capture.clone(), f);
    let lines = capture.0.lock().unwrap().clone();
    (out, lines)
}

/// A working directory that exists but cannot be entered: the default
/// shell in `$HOME`, the notice its first line, and a warning naming the
/// profile by index and `errno` only.
#[test]
fn a_directory_that_cannot_be_entered_starts_the_default_shell_at_home() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let locked = Locked::new(root.path());
    let app = app(
        &root,
        config(
            &[
                ("first", None, None),
                ("secret-name", None, Some(locked.path())),
            ],
            "Default",
        ),
    );
    let (id, logged) = warnings(|| {
        app.spawn_session_with_profile(options(home.path()), Some("secret-name"))
            .expect("the fallback starts")
    });
    let rows = wait_for_row(&app, id, &notice("secret-name", "Permission denied"));
    assert_eq!(
        rows[0],
        notice("secret-name", "Permission denied"),
        "{rows:?}"
    );
    assert_a_shell_in(&app, id, home.path());
    app.close_session(id).unwrap();
    let fallback: Vec<_> = logged
        .iter()
        .filter(|line| line.contains("could not start"))
        .collect();
    assert_eq!(fallback.len(), 1, "{logged:?}");
    assert!(fallback[0].contains("profile=Some(1)"), "{logged:?}");
    assert!(fallback[0].contains("errno=Some(13)"), "{logged:?}");
    for line in &logged {
        assert!(!line.contains("secret-name"), "{logged:?}");
        assert!(!line.contains("locked"), "{logged:?}");
    }
}

/// A command that does not exist: the default shell in `$HOME`.
#[test]
fn a_command_that_cannot_be_executed_starts_the_default_shell_at_home() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let app = app(
        &root,
        config(&[("tool", Some(MISSING_COMMAND), None)], "Default"),
    );
    let (id, logged) = warnings(|| {
        app.spawn_session_with_profile(options(home.path()), Some("tool"))
            .expect("the fallback starts")
    });
    let rows = wait_for_row(&app, id, &notice("tool", "No such file or directory"));
    assert_eq!(rows[0], notice("tool", "No such file or directory"));
    assert_a_shell_in(&app, id, home.path());
    app.close_session(id).unwrap();
    for line in &logged {
        assert!(!line.contains("nonexistent"), "{logged:?}");
    }
}

/// `[general] default_profile` naming such a profile: New Tab (no profile
/// asked for) still opens a usable shell.
#[test]
fn a_broken_default_profile_still_opens_a_usable_shell() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let locked = Locked::new(root.path());
    let app = app(
        &root,
        config(&[("broken", None, Some(locked.path()))], "broken"),
    );
    let id = app
        .spawn_session(options(home.path()))
        .expect("the fallback starts");
    let rows = wait_for_row(&app, id, &notice("broken", "Permission denied"));
    assert_eq!(rows[0], notice("broken", "Permission denied"));
    assert_a_shell_in(&app, id, home.path());
    app.close_session(id).unwrap();
}

/// A hostile profile name reaches the notice without its control and
/// bidirectional formatting characters, cut to 128 bytes (even in a
/// configuration built by hand, whose names the loader never capped): the
/// escape sequence in it is printed as text, never run.
#[test]
fn a_hostile_name_is_sanitised_and_capped_in_the_notice() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let name = format!("\u{1b}[31m\u{202e}\u{7}{}", "é".repeat(200));
    // Built by hand: the loader would cut the name to 128 bytes first.
    let mut config = config(&[("x", Some(MISSING_COMMAND), None)], "Default");
    config.profiles[0].name.clone_from(&name);
    let app = app(&root, config);
    let id = app
        .spawn_session_with_profile(options(home.path()), Some(&name))
        .expect("the fallback starts");
    let shown = format!("[31m{}", "é".repeat(62));
    assert_eq!(shown.len(), 128);
    let rows = wait_for_row(&app, id, &notice(&shown, "No such file or directory"));
    // `[31m` shows as text: the escape before it never reached the
    // terminal, so no SGR ran.
    assert_eq!(rows[0], notice(&shown, "No such file or directory"));
    app.close_session(id).unwrap();
}

/// The built-in profile is tried once: when it fails too (its home
/// directory cannot be entered either), the profile's own error is
/// returned and nothing is retried again.
#[test]
fn the_default_shell_is_tried_once_and_the_profiles_error_returned() {
    let root = tempfile::tempdir().unwrap();
    let locked = Locked::new(root.path());
    let app = app(
        &root,
        config(&[("tool", Some(MISSING_COMMAND), None)], "Default"),
    );
    let (result, logged) =
        warnings(|| app.spawn_session_with_profile(options(&locked.0), Some("tool")));
    match result {
        Err(Error::Pty(PtyError::Spawn { program, .. })) => {
            assert!(program.contains("tessera-profile-command"), "{program}");
        }
        other => panic!("the profile's spawn error, got {other:?}"),
    }
    let tries = logged
        .iter()
        .filter(|line| line.contains("could not start"))
        .count();
    assert_eq!(tries, 1, "{logged:?}");
    assert_eq!(app.session_ids(), Vec::new());
}

/// A session the caller fully chose (its own program and directory) is
/// not the profile's doing: its failure is returned, with no retry.
#[test]
fn a_failure_of_the_callers_own_options_is_not_retried() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let app = app(
        &root,
        config(&[("tool", Some(MISSING_COMMAND), None)], "Default"),
    );
    let options = SpawnOptions::command(MISSING_COMMAND, "")
        .size(PtySize::new(24, 400))
        .env("HOME", home.path());
    let (result, logged) = warnings(|| app.spawn_session_with_profile(options, Some("tool")));
    assert!(
        matches!(result, Err(Error::Pty(PtyError::Spawn { .. }))),
        "{result:?}"
    );
    assert!(
        !logged.iter().any(|line| line.contains("could not start")),
        "{logged:?}"
    );
    assert_eq!(app.session_ids(), Vec::new());
}
