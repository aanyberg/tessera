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
use std::time::{Duration, Instant};

use tessera_app_core::{
    App, Config, ConfigPaths, Error, LookupStep, PtyError, PtySize, SessionId, SpawnOptions,
};
use tessera_pty::test_hooks;

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

    /// Whether the lock holds for this user: not for root, who enters any
    /// directory (#342 M8). `USER` is checked, then a file is created in
    /// the directory as a probe, for a root whose `USER` says otherwise.
    fn holds(&self) -> bool {
        if std::env::var("USER").is_ok_and(|user| user == "root") {
            eprintln!("skipped: run as root, who enters a mode-000 directory");
            return false;
        }
        let probe = self.0.join("probe");
        if std::fs::File::create(&probe).is_ok() {
            let _ = std::fs::remove_file(&probe);
            eprintln!("skipped: a mode-000 directory does not stop this user");
            return false;
        }
        true
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// An executable script in `dir` (it would print `ran` and exit), for a
/// profile `command` that exists and could run.
fn script(dir: &Path) -> PathBuf {
    let path = dir.join("stalled-tool");
    std::fs::write(&path, "#!/bin/sh\necho ran\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Releases a program lookup held with
/// [`test_hooks::hold_program_lookup`] when dropped, so a failed assert
/// does not leave its helper blocked.
struct HeldLookup(PathBuf);

impl HeldLookup {
    fn new(path: &Path) -> Self {
        test_hooks::hold_program_lookup(path);
        Self(path.to_path_buf())
    }
}

impl Drop for HeldLookup {
    fn drop(&mut self) {
        test_hooks::release_program_lookup(&self.0);
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
    if !locked.holds() {
        return;
    }
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

/// #342 (lead decision D2-7): a command longer than a program's arguments
/// may be (`PtyError::ArgumentsTooLong`, refused before the fork) starts
/// the default shell too; as `default_profile` it would otherwise fail
/// every new tab.
#[test]
fn a_command_too_long_to_run_starts_the_default_shell_at_home() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let long = format!("/bin/echo {}", "x".repeat(1_100_000));
    let app = app(&root, config(&[("long", Some(&long), None)], "long"));
    let (id, logged) = warnings(|| {
        app.spawn_session(options(home.path()))
            .expect("the fallback starts")
    });
    let rows = wait_for_row(&app, id, &notice("long", "Argument list too long"));
    assert_eq!(rows[0], notice("long", "Argument list too long"));
    assert_a_shell_in(&app, id, home.path());
    app.close_session(id).unwrap();
    let fallback: Vec<_> = logged
        .iter()
        .filter(|line| line.contains("could not start"))
        .collect();
    assert_eq!(fallback.len(), 1, "{logged:?}");
    assert!(fallback[0].contains("errno=Some(7)"), "{logged:?}");
    for line in &logged {
        assert!(!line.contains("xxxx"), "the command is never logged");
    }
}

/// `[general] default_profile` naming such a profile: New Tab (no profile
/// asked for) still opens a usable shell.
#[test]
fn a_broken_default_profile_still_opens_a_usable_shell() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let locked = Locked::new(root.path());
    if !locked.holds() {
        return;
    }
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
    if !locked.holds() {
        return;
    }
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

/// #602: a profile whose `command` lives on a stalled volume (its lookup
/// never answers) starts the default shell in `$HOME` once the lookup's
/// time is up, with an `ETIMEDOUT` notice, instead of failing the tab;
/// the warning names no path.
#[test]
fn a_command_whose_lookup_never_answers_starts_the_default_shell_at_home() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let tool = script(root.path());
    let app = app(
        &root,
        config(
            &[("stalled", Some(tool.to_str().unwrap()), None)],
            "Default",
        ),
    );
    let held = HeldLookup::new(&tool);
    let started = Instant::now();
    let (id, logged) = warnings(|| {
        app.spawn_session_with_profile(options(home.path()), Some("stalled"))
            .expect("the fallback starts")
    });
    let waited = started.elapsed();
    let (callers, finished) = test_hooks::program_lookup_calls(&tool);
    assert_eq!(callers, ["pty-spawn-program"], "the lookup was not held");
    assert_eq!(finished, 0, "the held lookup returned before the fallback");
    assert!(
        waited >= test_hooks::PROGRAM_LOOKUP_TIMEOUT,
        "fell back after {waited:?}, before the lookup's time was up"
    );
    assert!(
        waited < test_hooks::PROGRAM_LOOKUP_TIMEOUT + Duration::from_secs(2),
        "fell back after {waited:?}"
    );
    let reason = std::io::Error::from_raw_os_error(nix::errno::Errno::ETIMEDOUT as i32).to_string();
    let reason = reason.split(" (os error").next().unwrap();
    let rows = wait_for_row(&app, id, &notice("stalled", reason));
    assert_eq!(rows[0], notice("stalled", reason), "{rows:?}");
    assert!(!rows.iter().any(|row| row == "ran"), "{rows:?}");
    assert_a_shell_in(&app, id, home.path());
    app.close_session(id).unwrap();
    let fallback: Vec<_> = logged
        .iter()
        .filter(|line| line.contains("could not start"))
        .collect();
    assert_eq!(fallback.len(), 1, "{logged:?}");
    assert!(
        fallback[0].contains(&format!(
            "errno=Some({})",
            nix::errno::Errno::ETIMEDOUT as i32
        )),
        "{logged:?}"
    );
    for line in &logged {
        assert!(!line.contains("stalled-tool"), "{logged:?}");
    }
    drop(held);
    for _ in 0..OUTPUT_ATTEMPTS {
        if test_hooks::program_lookup_calls(&tool).1 == 1 {
            return;
        }
        thread::sleep(POLL);
    }
    panic!("the held lookup did not return on release");
}

/// #673: a profile `working_directory` on a volume that never answers
/// (its stat held) holds the caller, the app's main thread, no longer than
/// the bounded lookup's timeout: the directory is stated on the lookup's
/// helper only, never on the calling thread, and the default shell starts
/// in `$HOME` with an `ETIMEDOUT` notice. Neither the warning nor the
/// notice names the directory.
#[test]
fn a_working_directory_that_never_answers_does_not_hold_the_caller() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let volume = root.path().join("stalled-volume");
    std::fs::create_dir(&volume).unwrap();
    let app = app(
        &root,
        config(
            &[("stalled", None, Some(volume.to_str().unwrap()))],
            "Default",
        ),
    );
    let held = HeldLookup::new(&volume);
    let started = Instant::now();
    let (id, logged) = warnings(|| {
        app.spawn_session_with_profile(options(home.path()), Some("stalled"))
            .expect("the fallback starts")
    });
    let waited = started.elapsed();
    let (callers, finished) = test_hooks::program_lookup_calls(&volume);
    assert_eq!(
        callers,
        ["pty-spawn-program"],
        "the directory was not stated on the lookup's helper alone"
    );
    assert_eq!(finished, 0, "the held stat returned before the fallback");
    assert!(
        waited >= test_hooks::PROGRAM_LOOKUP_TIMEOUT,
        "fell back after {waited:?}, before the lookup's time was up"
    );
    assert!(
        waited < test_hooks::PROGRAM_LOOKUP_TIMEOUT + Duration::from_secs(2),
        "the caller was held {waited:?}"
    );
    let reason = std::io::Error::from_raw_os_error(nix::errno::Errno::ETIMEDOUT as i32).to_string();
    let reason = reason.split(" (os error").next().unwrap();
    let rows = wait_for_row(&app, id, &notice("stalled", reason));
    assert_eq!(rows[0], notice("stalled", reason), "{rows:?}");
    assert_a_shell_in(&app, id, home.path());
    app.close_session(id).unwrap();
    assert!(
        logged.iter().any(|line| line.contains("could not start")
            && line.contains(&format!(
                "errno=Some({})",
                nix::errno::Errno::ETIMEDOUT as i32
            ))),
        "{logged:?}"
    );
    for line in &logged {
        assert!(!line.contains("stalled-volume"), "{logged:?}");
    }
    drop(held);
    for _ in 0..OUTPUT_ATTEMPTS {
        if test_hooks::program_lookup_calls(&volume).1 == 1 {
            return;
        }
        thread::sleep(POLL);
    }
    panic!("the held stat did not return on release");
}

/// #602: a profile whose program lookup could not start its helper thread
/// (the process out of threads) starts the default shell with the
/// thread start's `EAGAIN` in the notice, not a timeout.
#[test]
fn a_command_whose_lookup_thread_cannot_start_starts_the_default_shell() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let tool = script(root.path());
    let app = app(
        &root,
        config(
            &[("threads", Some(tool.to_str().unwrap()), None)],
            "Default",
        ),
    );
    let eagain = nix::errno::Errno::EAGAIN as i32;
    // The first helper the spawn starts on this thread is its lookup.
    test_hooks::fail_next_helper_start(eagain);
    let (id, logged) = warnings(|| {
        app.spawn_session_with_profile(options(home.path()), Some("threads"))
            .expect("the fallback starts")
    });
    let reason = std::io::Error::from_raw_os_error(eagain).to_string();
    let reason = reason.split(" (os error").next().unwrap();
    let rows = wait_for_row(&app, id, &notice("threads", reason));
    assert_eq!(rows[0], notice("threads", reason), "{rows:?}");
    assert!(!rows.iter().any(|row| row == "ran"), "{rows:?}");
    assert_a_shell_in(&app, id, home.path());
    app.close_session(id).unwrap();
    assert!(
        logged.iter().any(|line| line.contains("could not start")
            && line.contains(&format!("errno=Some({eagain})"))),
        "{logged:?}"
    );
}

/// The `strerror` text of `errno`, without its `(os error N)`.
fn errno_text(errno: nix::errno::Errno) -> String {
    let text = std::io::Error::from_raw_os_error(errno as i32).to_string();
    text.split(" (os error").next().unwrap().to_owned()
}

/// Waits until the held lookups of `path` have all returned.
fn wait_for_lookups_to_return(path: &Path) {
    for _ in 0..OUTPUT_ATTEMPTS {
        let (callers, finished) = test_hooks::program_lookup_calls(path);
        if finished == callers.len() {
            return;
        }
        thread::sleep(POLL);
    }
    panic!("the held lookups did not return on release");
}

/// #661 E1: four tabs of a profile whose command is on a stalled volume,
/// opened at once, fill the shared lookup helpers with stuck lookups; each
/// still starts the default shell (on the helper kept for the built-in
/// start) instead of failing with "too many earlier program checks". So
/// do a fifth tab of that profile (refused at once, `EAGAIN`) and a tab of
/// the built-in profile. Fills the process's helpers, so it runs only
/// alone in its process (as under nextest).
#[test]
fn four_stalled_profile_tabs_at_once_each_start_the_default_shell() {
    if std::env::var_os("NEXTEST").is_none() {
        eprintln!("skipped: fills the process-wide helper pool; run under nextest");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let tool = script(root.path());
    let app = app(
        &root,
        config(
            &[("stalled", Some(tool.to_str().unwrap()), None)],
            "Default",
        ),
    );
    let held = HeldLookup::new(&tool);
    let started = Instant::now();
    let ids: Vec<SessionId> = thread::scope(|scope| {
        let tabs: Vec<_> = (0..4)
            .map(|_| {
                scope
                    .spawn(|| app.spawn_session_with_profile(options(home.path()), Some("stalled")))
            })
            .collect();
        tabs.into_iter()
            .map(|tab| {
                tab.join()
                    .unwrap()
                    .expect("the fallback starts at the helper cap")
            })
            .collect()
    });
    let waited = started.elapsed();
    assert!(
        waited < test_hooks::PROGRAM_LOOKUP_TIMEOUT + Duration::from_secs(4),
        "the four tabs took {waited:?}"
    );
    let (callers, finished) = test_hooks::program_lookup_calls(&tool);
    assert_eq!(callers.len(), 4, "{callers:?}");
    assert_eq!(finished, 0, "a held lookup returned");
    let timed_out = notice("stalled", &errno_text(nix::errno::Errno::ETIMEDOUT));
    for &id in &ids {
        let rows = wait_for_row(&app, id, &timed_out);
        assert_eq!(rows[0], timed_out, "{rows:?}");
        assert_a_shell_in(&app, id, home.path());
    }

    // Every shared helper is stuck now: a fifth tab is refused at once and
    // falls back too, as does a tab that changes nothing.
    let fifth = app
        .spawn_session_with_profile(options(home.path()), Some("stalled"))
        .expect("a fifth tab starts the default shell");
    let busy = notice("stalled", &errno_text(nix::errno::Errno::EAGAIN));
    let rows = wait_for_row(&app, fifth, &busy);
    assert_eq!(rows[0], busy, "{rows:?}");
    assert_a_shell_in(&app, fifth, home.path());
    let plain = app
        .spawn_session(options(home.path()))
        .expect("a built-in tab starts");
    assert_a_shell_in(&app, plain, home.path());
    assert_eq!(test_hooks::program_lookup_calls(&tool).0.len(), 4);

    for id in ids.into_iter().chain([fifth, plain]) {
        app.close_session(id).unwrap();
    }
    drop(held);
    wait_for_lookups_to_return(&tool);
}

/// #661 M1: with `$SHELL` on a stalled volume, a profile whose command is
/// a local `/bin/sh` times out on the `$SHELL` check that every start
/// makes. The built-in start would wait on the same check, so it is not
/// tried: the tab fails once the one lookup's time is up (not after a
/// second 5 s wait on a second helper), with an error that names the
/// login shell and `$SHELL`'s length, not `/bin/sh`.
#[test]
fn a_local_command_under_a_stalled_login_shell_fails_after_one_lookup() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let shell = root.path().join("stalled-login-shell");
    std::os::unix::fs::symlink("/bin/sh", &shell).unwrap();
    let app = app(
        &root,
        config(&[("local", Some("/bin/sh"), None)], "Default"),
    );
    let held = HeldLookup::new(&shell);
    let started = Instant::now();
    let (result, logged) = warnings(|| {
        app.spawn_session_with_profile(options(home.path()).env("SHELL", &shell), Some("local"))
    });
    let waited = started.elapsed();
    let (callers, finished) = test_hooks::program_lookup_calls(&shell);
    assert_eq!(callers, ["pty-spawn-program"], "a second lookup was made");
    assert_eq!(finished, 0, "the held lookup returned");
    let err = match result {
        Err(Error::Pty(
            err @ PtyError::ProgramTimedOut {
                stalled: LookupStep::LoginShell,
                ..
            },
        )) => err,
        other => panic!("a timed-out login shell check, got {other:?}"),
    };
    assert!(
        matches!(err, PtyError::ProgramTimedOut { path_bytes, .. }
            if path_bytes == shell.as_os_str().len()),
        "{err:?}"
    );
    let shown = err.to_string();
    assert!(shown.starts_with("the login shell ($SHELL) ("), "{shown}");
    assert!(!shown.contains("(7-byte path)"), "blames /bin/sh: {shown}");
    assert!(
        waited >= test_hooks::PROGRAM_LOOKUP_TIMEOUT,
        "failed after {waited:?}"
    );
    assert!(
        waited < test_hooks::PROGRAM_LOOKUP_TIMEOUT + Duration::from_secs(2),
        "failed after {waited:?}"
    );
    assert!(
        !logged
            .iter()
            .any(|line| line.contains("trying the built-in")),
        "{logged:?}"
    );
    assert!(
        logged
            .iter()
            .any(|line| line.contains("would wait on the same lookup")),
        "{logged:?}"
    );
    for line in &logged {
        assert!(!line.contains("stalled-login-shell"), "{logged:?}");
    }
    assert_eq!(app.session_ids(), Vec::new());
    drop(held);
    wait_for_lookups_to_return(&shell);
}

/// #661 M1: the second start is skipped only when it would repeat the
/// stalled step. A profile whose working directory stalls (the built-in
/// start does not ask about it) still starts the default shell at home.
#[test]
fn a_working_directory_whose_lookup_never_answers_starts_the_default_shell_at_home() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let dir = root.path().join("stalled-dir");
    std::fs::create_dir(&dir).unwrap();
    let app = app(
        &root,
        config(&[("far", None, Some(dir.to_str().unwrap()))], "Default"),
    );
    let held = HeldLookup::new(&dir);
    let started = Instant::now();
    let id = app
        .spawn_session_with_profile(options(home.path()), Some("far"))
        .expect("the fallback starts");
    let waited = started.elapsed();
    assert!(
        waited < test_hooks::PROGRAM_LOOKUP_TIMEOUT + Duration::from_secs(2),
        "fell back after {waited:?}"
    );
    assert_eq!(
        test_hooks::program_lookup_calls(&dir).0,
        ["pty-spawn-program"],
        "the directory's lookup was not held"
    );
    let timed_out = notice("far", &errno_text(nix::errno::Errno::ETIMEDOUT));
    let rows = wait_for_row(&app, id, &timed_out);
    assert_eq!(rows[0], timed_out, "{rows:?}");
    assert_a_shell_in(&app, id, home.path());
    app.close_session(id).unwrap();
    drop(held);
    wait_for_lookups_to_return(&dir);
}

/// How long after four stalled tabs the fifth of
/// [`five_tabs_on_one_stall`] starts in the `shell5`/`prog5` tests: well
/// inside the four's 5 s, so the fifth's wait for a helper is a long one.
const GAP: Duration = Duration::from_millis(300);

/// How long the fifth tab's healthy but slow login shell takes to answer
/// in #661 round 2's test: well within a lookup timeout and under the 1 s
/// floor, but longer than the milliseconds a refusal for want of a helper
/// leaves of the 5 s.
const SLOW_SHELL: Duration = Duration::from_millis(400);

/// The longest a tab refused for want of a helper may take with its
/// fallback (#661 B1): the 5 s lookup timeout, the fallback's 1 s floor,
/// and 1 s of slack, against 10 s for a second full wait.
const ONE_WAIT: Duration = Duration::from_secs(7);

/// Waits until `n` lookups of `path` are held.
fn wait_for_held_lookups(path: &Path, n: usize) {
    for _ in 0..OUTPUT_ATTEMPTS {
        if test_hooks::program_lookup_calls(path).0.len() >= n {
            return;
        }
        thread::sleep(POLL);
    }
    panic!("fewer than {n} lookups of the path were held");
}

/// Starts four tabs of `profile` with `tab()`'s options at once, whose
/// lookups all stall on `held`, and a fifth `gap` after they are all
/// waiting; the fifth, waiting for a shared helper, is refused only once
/// the four time out. Returns the four results and the fifth's, with how
/// long the fifth took.
fn five_tabs_on_one_stall(
    app: &App,
    held: &Path,
    profile: &str,
    gap: Duration,
    tab: impl Fn() -> SpawnOptions + Sync,
) -> (
    Vec<Result<SessionId, Error>>,
    Result<SessionId, Error>,
    Duration,
) {
    thread::scope(|scope| {
        let four: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| app.spawn_session_with_profile(tab(), Some(profile))))
            .collect();
        wait_for_held_lookups(held, 4);
        thread::sleep(gap);
        let started = Instant::now();
        let fifth = app.spawn_session_with_profile(tab(), Some(profile));
        let took = started.elapsed();
        let four = four.into_iter().map(|tab| tab.join().unwrap()).collect();
        (four, fifth, took)
    })
}

/// #661 B1 (adversarial `shell5`): with `$SHELL` on a stalled volume and
/// four tabs of a profile with a local `command = "/bin/sh"` stuck on its
/// check, a fifth tab is refused for want of a helper only after waiting
/// for one. Its fallback runs within that same wait's 5 s (or its 1 s
/// floor), so the tab fails after about one lookup timeout (not two),
/// with the error of the
/// fallback's own lookup: the login shell and `$SHELL`'s length, not the
/// profile's `/bin/sh`. Fills the process's helpers, so it runs only alone
/// in its process (as under nextest).
#[test]
fn a_fifth_tab_under_a_stalled_login_shell_fails_within_one_lookup_timeout() {
    if std::env::var_os("NEXTEST").is_none() {
        eprintln!("skipped: fills the process-wide helper pool; run under nextest");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let shell = root.path().join("stalled-login-shell");
    std::os::unix::fs::symlink("/bin/sh", &shell).unwrap();
    let app = app(
        &root,
        config(&[("local", Some("/bin/sh"), None)], "Default"),
    );
    let held = HeldLookup::new(&shell);
    let (four, fifth, waited) = five_tabs_on_one_stall(&app, &shell, "local", GAP, || {
        options(home.path()).env("SHELL", &shell)
    });
    let stalled_shell = |result: &Result<SessionId, Error>| {
        matches!(result, Err(Error::Pty(PtyError::ProgramTimedOut {
            stalled: LookupStep::LoginShell,
            path_bytes,
        })) if *path_bytes == shell.as_os_str().len())
    };
    assert!(four.iter().all(stalled_shell), "{four:?}");
    assert!(stalled_shell(&fifth), "{fifth:?}");
    let shown = fifth.unwrap_err().to_string();
    assert!(shown.starts_with("the login shell ($SHELL) ("), "{shown}");
    assert!(waited < ONE_WAIT, "the fifth tab failed after {waited:?}");
    // The four shared helpers and the reserved one: the 4 + 1 bound.
    assert_eq!(test_hooks::program_lookup_calls(&shell).0.len(), 5);
    assert_eq!(app.session_ids(), Vec::new());
    drop(held);
    wait_for_lookups_to_return(&shell);
}

/// #661 B1 (adversarial `prog5`): the caller's own command on a stalled
/// volume, under a profile that changes only the working directory. Four
/// tabs time out on the program and are not retried (the built-in start
/// would run the same one); a fifth, refused after waiting for a helper,
/// falls back within that wait's 5 s (or its 1 s floor) and fails after
/// about one lookup timeout, naming the program. Runs only alone in its
/// process.
#[test]
fn a_fifth_tab_of_a_stalled_command_fails_within_one_lookup_timeout() {
    if std::env::var_os("NEXTEST").is_none() {
        eprintln!("skipped: fills the process-wide helper pool; run under nextest");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let tool = script(root.path());
    let dir = tempfile::tempdir().unwrap();
    let app = app(
        &root,
        config(
            &[("far", None, Some(dir.path().to_str().unwrap()))],
            "Default",
        ),
    );
    let held = HeldLookup::new(&tool);
    let (four, fifth, waited) = five_tabs_on_one_stall(&app, &tool, "far", GAP, || {
        SpawnOptions::command(&tool, "")
            .size(PtySize::new(24, 400))
            .env("SHELL", "/bin/sh")
            .env("HOME", home.path())
    });
    let stalled_tool = |result: &Result<SessionId, Error>| {
        matches!(result, Err(Error::Pty(PtyError::ProgramTimedOut {
            stalled: LookupStep::Program,
            path_bytes,
        })) if *path_bytes == tool.as_os_str().len())
    };
    assert!(four.iter().all(stalled_tool), "{four:?}");
    assert!(stalled_tool(&fifth), "{fifth:?}");
    assert!(waited < ONE_WAIT, "the fifth tab failed after {waited:?}");
    assert_eq!(test_hooks::program_lookup_calls(&tool).0.len(), 5);
    assert_eq!(app.session_ids(), Vec::new());
    drop(held);
    wait_for_lookups_to_return(&tool);
}

/// #661 B2 (adversarial `pinwait`): four tabs whose lookups are slow (held
/// for a while, then released) fill the shared helpers, and a built-in
/// tab whose lookup stalls holds the reserved one. A healthy login shell
/// opened then waits for whichever helper frees first: it starts once the
/// slow lookups return, not refused after waiting on the stalled reserved
/// helper alone. Runs only alone in its process.
#[test]
fn a_builtin_tab_behind_a_stalled_reserved_lookup_takes_the_first_free_helper() {
    if std::env::var_os("NEXTEST").is_none() {
        eprintln!("skipped: fills the process-wide helper pool; run under nextest");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let slow_dir = tempfile::tempdir().unwrap();
    let stalled_dir = tempfile::tempdir().unwrap();
    let slow = script(slow_dir.path());
    let stalled = script(stalled_dir.path());
    let app = app(&root, config(&[], "Default"));
    let slow_held = HeldLookup::new(&slow);
    let stalled_held = HeldLookup::new(&stalled);
    let command = |path: &Path| {
        SpawnOptions::command(path, "")
            .size(PtySize::new(24, 400))
            .env("SHELL", "/bin/sh")
            .env("HOME", home.path())
    };
    thread::scope(|scope| {
        let four: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| app.spawn_session(command(&slow))))
            .collect();
        wait_for_held_lookups(&slow, 4);
        let pinned = scope.spawn(|| app.spawn_session(command(&stalled)));
        wait_for_held_lookups(&stalled, 1);
        let started = Instant::now();
        let healthy = scope.spawn(|| app.spawn_session(options(home.path())));
        // The healthy tab waits for a helper by now; the slow lookups then
        // return and free the shared ones.
        thread::sleep(Duration::from_millis(500));
        drop(slow_held);
        let healthy = healthy.join().unwrap();
        let took = started.elapsed();
        let id = healthy.expect("the healthy tab starts on a freed shared helper");
        assert!(
            took + Duration::from_secs(1) < test_hooks::PROGRAM_LOOKUP_TIMEOUT,
            "the healthy tab started after {took:?}"
        );
        assert_a_shell_in(&app, id, home.path());
        for tab in four {
            app.close_session(tab.join().unwrap().expect("a slow tab starts"))
                .unwrap();
        }
        app.close_session(id).unwrap();
        // The stalled tab is refused once its own 5 s are up; at most one
        // lookup of it was made, on the reserved helper.
        let pinned = pinned.join().unwrap();
        assert!(
            matches!(
                pinned,
                Err(Error::Pty(PtyError::ProgramTimedOut {
                    stalled: LookupStep::Program,
                    ..
                }))
            ),
            "{pinned:?}"
        );
    });
    assert_eq!(test_hooks::program_lookup_calls(&stalled).0.len(), 1);
    drop(stalled_held);
    wait_for_lookups_to_return(&stalled);
}

/// #661 round 2: a profile whose command is on a stalled volume, `$SHELL`
/// healthy, five tabs opened together. Four take the shared helpers; the
/// fifth, waiting for one, is refused just as their lookups time out, with
/// only milliseconds of its own 5 s left. Its fallback still gets the 1 s
/// floor, enough for a login shell that answers in [`SLOW_SHELL`] (well
/// within a lookup timeout, but slower than the milliseconds left), so it
/// starts the default shell like the other four (with the `EAGAIN` notice)
/// instead of failing with no time left or naming `$SHELL`. Runs only
/// alone in its process.
#[test]
fn a_fifth_tab_refused_as_the_four_time_out_still_starts_the_default_shell() {
    if std::env::var_os("NEXTEST").is_none() {
        eprintln!("skipped: fills the process-wide helper pool; run under nextest");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let tool = script(root.path());
    let app = app(
        &root,
        config(
            &[("stalled", Some(tool.to_str().unwrap()), None)],
            "Default",
        ),
    );
    let held = HeldLookup::new(&tool);
    let shell = root.path().join("slow-login-shell");
    std::os::unix::fs::symlink("/bin/sh", &shell).unwrap();
    let slow = HeldLookup::new(&shell);
    let tabs = std::sync::atomic::AtomicUsize::new(0);
    let (four, fifth, waited) = thread::scope(|scope| {
        scope.spawn(|| {
            wait_for_held_lookups(&shell, 1);
            thread::sleep(SLOW_SHELL);
            test_hooks::release_program_lookup(&shell);
        });
        five_tabs_on_one_stall(&app, &tool, "stalled", Duration::ZERO, || {
            let tab = tabs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let options = options(home.path());
            if tab < 4 {
                options
            } else {
                options.env("SHELL", &shell)
            }
        })
    });
    let fifth = fifth
        .unwrap_or_else(|err| panic!("the fifth tab failed after {waited:?}: {err:?} / {err}"));
    assert!(waited < ONE_WAIT, "the fifth tab started after {waited:?}");
    let busy = notice("stalled", &errno_text(nix::errno::Errno::EAGAIN));
    let rows = wait_for_row(&app, fifth, &busy);
    assert_eq!(rows[0], busy, "{rows:?}");
    assert_a_shell_in(&app, fifth, home.path());
    let timed_out = notice("stalled", &errno_text(nix::errno::Errno::ETIMEDOUT));
    for tab in four {
        let id = tab.expect("each of the four starts the default shell");
        let rows = wait_for_row(&app, id, &timed_out);
        assert_eq!(rows[0], timed_out, "{rows:?}");
        app.close_session(id).unwrap();
    }
    app.close_session(fifth).unwrap();
    assert_eq!(test_hooks::program_lookup_calls(&tool).0.len(), 4);
    assert_eq!(test_hooks::program_lookup_calls(&shell).0.len(), 1);
    drop(held);
    drop(slow);
    wait_for_lookups_to_return(&tool);
    wait_for_lookups_to_return(&shell);
}

/// How long each lookup of the slow but healthy login shell in the #661 F1
/// test takes: well within a lookup timeout, but five of them queued on the
/// one helper kept for the built-in start take 3 s, far past the 1 s floor.
const SLOWER_SHELL: Duration = Duration::from_millis(600);

/// How far apart the four stalled tabs of the #661 F1 test start, so the
/// first of them times out (and takes the reserved helper for its
/// fallback) well before the fifth tab is refused.
const STAGGER: Duration = Duration::from_millis(50);

/// #661 F1 (adversarial cycle 2 `fifth`): a profile whose command is on a
/// stalled volume, `$SHELL` healthy but slow ([`SLOWER_SHELL`] per
/// lookup), and five tabs. Four take the shared helpers and time out on
/// the command; the fifth waits for a shared helper and is refused as they
/// do. All five fallbacks then queue on the one reserved helper, and each
/// starts the default shell: the fifth's fallback gets its own 5 s (the
/// taken helpers waited on the profile's command, which the fallback does
/// not ask about), not the 1 s floor that slow queue would outlast. Runs
/// only alone in its process.
#[test]
fn five_tabs_of_a_stalled_command_under_a_slow_login_shell_all_start_the_default_shell() {
    if std::env::var_os("NEXTEST").is_none() {
        eprintln!("skipped: fills the process-wide helper pool; run under nextest");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let tool = script(root.path());
    let shell = root.path().join("slow-login-shell");
    std::os::unix::fs::symlink("/bin/sh", &shell).unwrap();
    let app = app(
        &root,
        config(
            &[("stalled", Some(tool.to_str().unwrap()), None)],
            "Default",
        ),
    );
    let held = HeldLookup::new(&tool);
    test_hooks::delay_program_lookup(&shell, SLOWER_SHELL);
    let tab = || options(home.path()).env("SHELL", &shell);
    let (four, fifth, waited) = thread::scope(|scope| {
        let four: Vec<_> = (0..4_u32)
            .map(|n| {
                let (app, tab) = (&app, &tab);
                scope.spawn(move || {
                    thread::sleep(STAGGER * n);
                    app.spawn_session_with_profile(tab(), Some("stalled"))
                })
            })
            .collect();
        wait_for_held_lookups(&tool, 4);
        let started = Instant::now();
        let fifth = app.spawn_session_with_profile(tab(), Some("stalled"));
        let fifth_waited = started.elapsed();
        let four: Vec<_> = four.into_iter().map(|tab| tab.join().unwrap()).collect();
        (four, fifth, fifth_waited)
    });
    let fifth = fifth
        .unwrap_or_else(|err| panic!("the fifth tab failed after {waited:?}: {err:?} / {err}"));
    assert!(
        waited < test_hooks::PROGRAM_LOOKUP_TIMEOUT * 2,
        "the fifth tab started after {waited:?}"
    );
    let busy = notice("stalled", &errno_text(nix::errno::Errno::EAGAIN));
    let rows = wait_for_row(&app, fifth, &busy);
    assert_eq!(rows[0], busy, "{rows:?}");
    let timed_out = notice("stalled", &errno_text(nix::errno::Errno::ETIMEDOUT));
    let ids: Vec<SessionId> = four
        .into_iter()
        .map(|tab| tab.unwrap_or_else(|err| panic!("one of the four failed: {err:?} / {err}")))
        .collect();
    for &id in &ids {
        let rows = wait_for_row(&app, id, &timed_out);
        assert_eq!(rows[0], timed_out, "{rows:?}");
    }
    for id in ids.into_iter().chain([fifth]) {
        app.close_session(id).unwrap();
    }
    assert_eq!(test_hooks::program_lookup_calls(&tool).0.len(), 4);
    drop(held);
    wait_for_lookups_to_return(&tool);
}

/// #661 F2 (adversarial cycle 2 `b1shell` with extra tabs): with `$SHELL`
/// on a stalled volume and four tabs of a profile with a local `command =
/// "/bin/sh"` stuck on its check, two more tabs are refused for want of a
/// helper. One takes the reserved helper for its fallback and times out
/// on `$SHELL`; the other, refused again, fails with the first refusal,
/// which names what the taken helpers wait on: the login shell and
/// `$SHELL`'s length, not the profile's 7-byte `/bin/sh`. Runs only alone
/// in its process.
#[test]
fn tabs_refused_under_a_stalled_login_shell_name_the_login_shell() {
    if std::env::var_os("NEXTEST").is_none() {
        eprintln!("skipped: fills the process-wide helper pool; run under nextest");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let shell = root.path().join("stalled-login-shell");
    std::os::unix::fs::symlink("/bin/sh", &shell).unwrap();
    let app = app(
        &root,
        config(&[("local", Some("/bin/sh"), None)], "Default"),
    );
    let held = HeldLookup::new(&shell);
    let tab = || options(home.path()).env("SHELL", &shell);
    let (four, extra) = thread::scope(|scope| {
        let four: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| app.spawn_session_with_profile(tab(), Some("local"))))
            .collect();
        wait_for_held_lookups(&shell, 4);
        let extra: Vec<_> = (0..2)
            .map(|_| scope.spawn(|| app.spawn_session_with_profile(tab(), Some("local"))))
            .collect();
        let four: Vec<_> = four.into_iter().map(|tab| tab.join().unwrap()).collect();
        let extra: Vec<_> = extra.into_iter().map(|tab| tab.join().unwrap()).collect();
        (four, extra)
    });
    assert_eq!(four.len(), 4);
    let names_the_shell = format!(
        "the login shell ($SHELL) ({}-byte path) could not be looked up: ",
        shell.as_os_str().len()
    );
    for result in four.iter().chain(&extra) {
        let shown = match result {
            Err(err) => err.to_string(),
            Ok(id) => panic!("a tab started: {id:?}"),
        };
        assert!(shown.starts_with(&names_the_shell), "{shown}");
    }
    assert!(
        extra
            .iter()
            .any(|result| matches!(result, Err(Error::Pty(PtyError::ProgramLookupBusy { .. })))),
        "{extra:?}"
    );
    // The four shared helpers and the reserved one: the 4 + 1 bound.
    assert_eq!(test_hooks::program_lookup_calls(&shell).0.len(), 5);
    assert_eq!(app.session_ids(), Vec::new());
    drop(held);
    wait_for_lookups_to_return(&shell);
}
