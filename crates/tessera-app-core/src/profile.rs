//! What a profile changes about a session's start (#39): the command in
//! place of the login shell and the starting directory.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use tessera_config::ResolvedProfile;
use tessera_pty::{Program, SpawnOptions};

/// `options` with `resolved`'s command and working directory, each only
/// where the caller left the choice open: the command when
/// `options.program` is the login shell, the directory when
/// `options.cwd` is `None`. See [`crate::App::spawn_session_with_profile`].
pub(crate) fn apply(mut options: SpawnOptions, resolved: &ResolvedProfile) -> SpawnOptions {
    if options.program == Program::LoginShell
        && let Some(command) = resolved.command.as_deref()
    {
        options.program = program_for(command).unwrap_or(Program::LoginShell);
    }
    if options.cwd.is_none()
        && let Some(dir) = resolved.working_directory.as_deref()
    {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        options.cwd = directory_for(dir, home.as_deref());
    }
    options
}

/// The `errno` behind a [`tessera_pty::PtyError::Spawn`] reason, when it
/// names one: for `tessera-pty`'s refusal of a `#!` script, the problem it
/// states ([`interpreter_errno`]); for `portable-pty`'s refusal of a
/// program it cannot find or run, the problem it states
/// ([`lookup_errno`]); otherwise `(os error N)` as `std::io::Error` writes
/// it, or one of the problems those refusals state. `None` for any other
/// reason; the reason itself (it may hold paths and `PATH`) is never shown
/// or logged (docs/security.md S8), and no path or `PATH` entry in it
/// chooses the `errno` (#519, #526 M3).
pub(crate) fn spawn_errno(reason: &str) -> Option<i32> {
    if let Some(detail) = reason.strip_prefix("its #! ") {
        return interpreter_errno(detail);
    }
    if let Some(detail) = reason.strip_prefix("Unable to spawn ") {
        return lookup_errno(detail);
    }
    if let Some(errno) = os_error(reason) {
        return Some(errno);
    }
    if reason.contains("is a directory") {
        Some(errno::EISDIR)
    } else if reason.contains("not executable") {
        Some(errno::EACCES)
    } else if [
        "doesn't exist",
        "does not exist",
        "No viable candidates",
        "Unable to resolve the PATH",
    ]
    .iter()
    .any(|needle| reason.contains(needle))
    {
        Some(errno::ENOENT)
    } else {
        None
    }
}

/// The `errno` of a `#!` refusal, `detail` being its text after `its #! `.
/// Only what follows the interpreter is read: the name comes from the
/// script and is quoted with `Debug`, which escapes every `"` in it, so the
/// last `" ` is the end of the quotes and the name cannot choose the
/// `errno`. A script with CRLF line endings is `ENOEXEC` whatever became of
/// its `/bin/sh\r`: the file is what is wrong, and "No such file or
/// directory" would send the user looking for `/bin/sh`.
fn interpreter_errno(detail: &str) -> Option<i32> {
    let problem = detail
        .rsplit_once("\" ")
        .map_or(detail, |(_, problem)| problem);
    if problem.contains("CRLF line endings") {
        Some(errno::ENOEXEC)
    } else if problem.starts_with("cannot be reached") {
        os_error(problem)
    } else if problem.starts_with("is not a regular file")
        || problem.starts_with("is not executable")
    {
        Some(errno::EACCES)
    } else if problem.starts_with("does not exist")
        || problem.starts_with("line names no interpreter")
    {
        Some(errno::ENOENT)
    } else {
        None
    }
}

/// The `errno` of `portable-pty`'s refusal of the program (its
/// `search_path`), `detail` being its text after `Unable to spawn `. The
/// program's path comes first and may hold any text (a profile's command,
/// a `PATH` entry), so only the fixed words after it are read:
///
/// - `<path> because it is a directory`: `EISDIR`;
/// - `<path> because it is not executable (<errno>)`: `EACCES`;
/// - `<path> because it doesn't exist on the filesystem (<errno>)` and
///   `<path> because it does not exist`: `ENOENT`;
/// - the `PATH` search, `<name> because:` and one line per candidate found
///   (`<candidate> exists but is a directory.` or `... is not executable.`),
///   then `No viable candidates found in PATH "<PATH>"` (quoted with
///   `Debug`, so on one line) or `Unable to resolve the PATH`: `EISDIR` for
///   a candidate directory, else `EACCES` for one not executable, else
///   `ENOENT`. A `PATH` entry with a line break can only make up a
///   candidate line, so it can only pick among these three.
///
/// The `<errno>` text is `nix`'s (`EACCES: Permission denied`), never
/// `because it `, so the last `because it ` is the one `portable-pty`
/// wrote.
fn lookup_errno(detail: &str) -> Option<i32> {
    let search = detail.ends_with("\nUnable to resolve the PATH")
        || detail
            .rsplit_once('\n')
            .is_some_and(|(_, last)| last.starts_with("No viable candidates found in PATH \""));
    if search {
        let found = |problem: &str| {
            detail
                .lines()
                .any(|line| line.ends_with(&format!(" exists but is {problem}.")))
        };
        return Some(if found("a directory") {
            errno::EISDIR
        } else if found("not executable") {
            errno::EACCES
        } else {
            errno::ENOENT
        });
    }
    let (_, problem) = detail.rsplit_once(" because it ")?;
    if problem == "is a directory" {
        Some(errno::EISDIR)
    } else if problem.starts_with("is not executable (") {
        Some(errno::EACCES)
    } else if problem == "does not exist"
        || problem.starts_with("doesn't exist on the filesystem (")
    {
        Some(errno::ENOENT)
    } else {
        None
    }
}

/// The `N` of the last `(os error N)` in `text`.
fn os_error(text: &str) -> Option<i32> {
    let (_, rest) = text.rsplit_once("(os error ")?;
    let (digits, _) = rest.split_once(')')?;
    digits.parse().ok()
}

/// The `errno` values [`spawn_errno`] maps `portable-pty`'s and
/// `tessera-pty`'s own messages to (the same on every Unix Tessera builds
/// for, but for `ETIMEDOUT`), and the one a refused
/// [`tessera_pty::PtyError::ArgumentsTooLong`] stands for.
pub(crate) mod errno {
    /// `Argument list too long`.
    pub(crate) const E2BIG: i32 = 7;
    pub(super) const ENOENT: i32 = 2;
    /// `Exec format error`.
    pub(super) const ENOEXEC: i32 = 8;
    pub(super) const EACCES: i32 = 13;
    pub(super) const EISDIR: i32 = 21;
    /// `Operation timed out`, for a pty open that gave no answer in time
    /// ([`tessera_pty::PtyError::OpenTimedOut`], #568). Unlike the others
    /// its number differs between Unix systems: 60 on macOS and the BSDs.
    #[cfg(not(target_os = "linux"))]
    pub(crate) const ETIMEDOUT: i32 = 60;
    /// `Connection timed out`, as Linux words `ETIMEDOUT`.
    #[cfg(target_os = "linux")]
    pub(crate) const ETIMEDOUT: i32 = 110;
}

/// The line written into a session that started on the built-in profile
/// because profile `name` could not (#333, lead decision F3), ending in
/// CR LF: the name with control and bidirectional formatting characters
/// left out and cut to [`tessera_config::edit::MAX_DISPLAY_NAME_LEN`]
/// bytes (docs/security.md S3), and the `errno`'s text without its
/// `(os error N)` suffix.
pub(crate) fn fallback_notice(name: &str, errno: Option<i32>) -> Vec<u8> {
    let reason = errno.map_or_else(
        || "the program could not be started".to_owned(),
        |errno| {
            let text = std::io::Error::from_raw_os_error(errno).to_string();
            match text.rsplit_once(" (os error ") {
                Some((text, _)) => text.to_owned(),
                None => text,
            }
        },
    );
    format!(
        "[Tessera] profile \"{}\" could not start ({reason}); started the default shell.\r\n",
        tessera_config::edit::display_name(name)
    )
    .into_bytes()
}

/// The program a profile's `command` runs: one word (no white space) as
/// the program itself, anything else through `/bin/sh -c`; `None` for an
/// empty command (the login shell then).
fn program_for(command: &str) -> Option<Program> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    Some(if command.contains(char::is_whitespace) {
        Program::Command {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(command)],
        }
    } else {
        Program::Command {
            program: PathBuf::from(command),
            args: Vec::new(),
        }
    })
}

/// The starting directory for a profile's `working_directory`: `~` and
/// `~/...` relative to `home`. `None` (the home directory) when it is not
/// a directory, with a warning naming it, escaped (S8).
fn directory_for(dir: &str, home: Option<&Path>) -> Option<PathBuf> {
    let expanded = match (dir.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_path_buf(),
        (Some(rest), Some(home)) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(dir),
    };
    if expanded.is_dir() {
        Some(expanded)
    } else {
        tracing::warn!(
            target: "tessera_app_core",
            directory = %tessera_config::escaped(dir, tessera_config::MAX_WARNING_PATH_LEN),
            "the profile's working directory is not a directory; starting in the home directory"
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(command: Option<&str>, dir: Option<&str>) -> ResolvedProfile {
        ResolvedProfile {
            name: Some("p".into()),
            command: command.map(Into::into),
            working_directory: dir.map(Into::into),
            appearance: tessera_config::Appearance::default(),
            warning: None,
        }
    }

    #[test]
    fn a_one_word_command_runs_directly_and_others_through_sh() {
        assert_eq!(
            program_for(" /usr/bin/top "),
            Some(Program::Command {
                program: "/usr/bin/top".into(),
                args: vec![]
            })
        );
        assert_eq!(
            program_for("ssh build-box"),
            Some(Program::Command {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "ssh build-box".into()]
            })
        );
        assert_eq!(program_for("  "), None);
    }

    #[test]
    fn the_callers_choice_wins_over_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let profile = resolved(Some("top"), Some(dir.path().to_str().unwrap()));
        let chosen = SpawnOptions::command("/bin/echo", "1").cwd("/");
        assert_eq!(apply(chosen.clone(), &profile), chosen);
        let open = apply(SpawnOptions::login_shell("1"), &profile);
        assert_eq!(
            open.program,
            Program::Command {
                program: "top".into(),
                args: vec![]
            }
        );
        assert_eq!(open.cwd.as_deref(), Some(dir.path()));
        // No command: still the login shell.
        let open = apply(SpawnOptions::login_shell("1"), &resolved(None, None));
        assert_eq!(open, SpawnOptions::login_shell("1"));
    }

    #[test]
    fn a_tilde_is_the_home_directory_and_a_missing_directory_is_dropped() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("src")).unwrap();
        let at = |dir: &str| directory_for(dir, Some(home.path()));
        assert_eq!(at("~").as_deref(), Some(home.path()));
        assert_eq!(at("~/src"), Some(home.path().join("src")));
        assert_eq!(at("~/missing"), None);
        assert_eq!(at("/nonexistent/tessera/dir"), None);
        // `~user` is not expanded (and here not a directory).
        assert_eq!(at("~src"), None);
        // A file is not a directory.
        let file = home.path().join("f");
        std::fs::write(&file, "").unwrap();
        assert_eq!(at(file.to_str().unwrap()), None);
        // Without a home directory `~` stays as written.
        assert_eq!(directory_for("~", None), None);
    }

    #[test]
    fn a_spawn_reason_gives_its_errno_and_nothing_else() {
        assert_eq!(spawn_errno("Permission denied (os error 13)"), Some(13));
        assert_eq!(
            spawn_errno("Unable to spawn /x because it doesn't exist on the filesystem (ENOENT)"),
            Some(2)
        );
        assert_eq!(
            spawn_errno("Unable to spawn x because it is a directory"),
            Some(21)
        );
        assert_eq!(
            spawn_errno("Unable to spawn x because it is not executable (mode)"),
            Some(13)
        );
        assert_eq!(
            spawn_errno("No viable candidates found in PATH \"/bin\""),
            Some(2)
        );
        assert_eq!(spawn_errno("nul byte found in provided data"), None);
        assert_eq!(spawn_errno("(os error x)"), None);
    }

    /// Every reason `check_interpreter` gives, as `format!` builds it there.
    #[test]
    fn a_shebang_refusal_gives_the_errno_of_its_problem() {
        let crlf = " (the script has CRLF line endings; save it with LF endings)";
        let reached = "cannot be reached (Not a directory (os error 20))";
        for (problem, errno) in [
            ("does not exist (ENOENT)".to_owned(), Some(errno::ENOENT)),
            (
                "is not a regular file (EACCES)".to_owned(),
                Some(errno::EACCES),
            ),
            ("is not executable (EACCES)".to_owned(), Some(errno::EACCES)),
            (reached.to_owned(), Some(20)),
            ("cannot be reached (Too many levels)".to_owned(), None),
            (
                format!("does not exist (ENOENT){crlf}"),
                Some(errno::ENOEXEC),
            ),
            (
                format!("is not executable (EACCES){crlf}"),
                Some(errno::ENOEXEC),
            ),
            (format!("{reached}{crlf}"), Some(errno::ENOEXEC)),
            ("something new".to_owned(), None),
        ] {
            let reason = format!("its #! interpreter {:?} {problem}", "/bin/sh");
            assert_eq!(spawn_errno(&reason), errno, "{reason}");
        }
        assert_eq!(
            spawn_errno("its #! line names no interpreter (ENOENT)"),
            Some(errno::ENOENT)
        );
    }

    /// An interpreter path is the script's text: whatever it holds, the
    /// `errno` is the one a plain path with the same failure gets.
    #[test]
    fn an_interpreter_path_cannot_choose_the_errno() {
        let hostile = [
            "/x/(os error 13)",
            "/x (os error 1)",
            "/x/is a directory",
            "/x/does not exist",
            "/x/not executable",
            "/x/No viable candidates",
            "/x/CRLF line endings",
            "/x\" is not executable (EACCES)",
            "/x\" does not exist (ENOENT)",
            "/x\" cannot be reached (os error 1)",
            "\" ",
            "/x\" ",
        ];
        for problem in [
            "does not exist (ENOENT)",
            "is not a regular file (EACCES)",
            "is not executable (EACCES)",
            "cannot be reached (Not a directory (os error 20))",
            "does not exist (ENOENT) (the script has CRLF line endings; save it with LF endings)",
        ] {
            let plain = spawn_errno(&format!("its #! interpreter {:?} {problem}", "/bin/sh"));
            assert!(plain.is_some(), "{problem}");
            for path in hostile {
                let reason = format!("its #! interpreter {path:?} {problem}");
                assert_eq!(spawn_errno(&reason), plain, "{reason}");
            }
        }
    }

    /// The reasons as `tessera-pty` really words them: a change of wording
    /// there fails here, not silently in the notice. Each is refused before
    /// the fork, so nothing runs.
    #[test]
    fn every_real_shebang_refusal_maps_to_its_errno() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        let plain = dir.join("plain");
        std::fs::write(&plain, "").unwrap();
        let looped = dir.join("loop");
        std::os::unix::fs::symlink(&looped, &looped).unwrap();
        let loop_errno = std::fs::metadata(&looped).unwrap_err().raw_os_error();
        let below_file = std::fs::metadata(plain.join("x"))
            .unwrap_err()
            .raw_os_error();
        let path = |path: &Path| path.to_str().unwrap().to_owned();
        let script = dir.join("script");
        for (line, errno) in [
            (
                format!("#!{}\n", path(&dir.join("missing"))),
                Some(errno::ENOENT),
            ),
            (format!("#!{}\n", path(&dir)), Some(errno::EACCES)),
            (format!("#!{}\n", path(&plain)), Some(errno::EACCES)),
            (format!("#!{}/x\n", path(&plain)), below_file),
            (format!("#!{}\n", path(&looped)), loop_errno),
            ("#!\0/x\n".to_owned(), Some(errno::ENOENT)),
            ("#!/bin/sh\r\necho hi\r\n".to_owned(), Some(errno::ENOEXEC)),
        ] {
            std::fs::write(&script, &line).unwrap();
            let Err(tessera_pty::PtyError::Spawn { reason, .. }) =
                tessera_pty::NativePty::spawn(&SpawnOptions::command(&script, "1"))
            else {
                panic!("{line:?} was not refused");
            };
            assert_eq!(spawn_errno(&reason), errno, "{line:?}: {reason}");
        }
    }

    /// #526 M3: the program's path and the `PATH` entries are the user's
    /// text, and `portable-pty` quotes them in its refusal; whatever they
    /// hold, the `errno` is the one of the problem it states. Each reason is
    /// a real refusal, so a change of wording there fails here. Nothing is
    /// forked: every program is refused before.
    #[test]
    fn a_path_or_path_entry_cannot_choose_the_errno() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let hostile = root.join("x (os error 1) because it is a directory");
        std::fs::create_dir(&hostile).unwrap();
        std::fs::create_dir(hostile.join("cmd-dir")).unwrap();
        let plain = hostile.join("cmd-plain");
        std::fs::write(&plain, "").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let in_path = format!(
            "{}:/nonexistent/(os error 13):/nonexistent/x exists but is a directory.\n:/n (os error 4)",
            hostile.display()
        );
        let reason = |options: SpawnOptions| match tessera_pty::NativePty::spawn(&options) {
            Err(tessera_pty::PtyError::Spawn { reason, .. }) => reason,
            other => panic!("not refused: {:?}", other.map(|_| ())),
        };
        let searched = |name: &str| {
            reason(
                SpawnOptions::command(name, "1")
                    .env("PATH", &in_path)
                    .env("HOME", &root),
            )
        };
        for (reason, errno) in [
            (searched("tessera-no-such-command"), errno::ENOENT),
            (searched("cmd-dir"), errno::EISDIR),
            (searched("cmd-plain"), errno::EACCES),
            (
                reason(SpawnOptions::command(
                    hostile.join("missing (os error 13)"),
                    "1",
                )),
                errno::ENOENT,
            ),
            (
                reason(SpawnOptions::command(hostile.join("cmd-dir"), "1")),
                errno::EISDIR,
            ),
            (reason(SpawnOptions::command(&plain, "1")), errno::EACCES),
            (
                reason(SpawnOptions::command("./missing (os error 13)", "1").cwd(&hostile)),
                errno::ENOENT,
            ),
        ] {
            assert!(reason.starts_with("Unable to spawn "), "{reason:?}");
            assert_eq!(spawn_errno(&reason), Some(errno), "{reason:?}");
        }
    }

    /// `os_error` takes the last `(os error N)`: `cannot be reached
    /// (<io error>)` ends with the one `std::io::Error` wrote.
    #[test]
    fn the_last_os_error_is_the_one_read() {
        assert_eq!(os_error("a (os error 1) b (os error 13)"), Some(13));
        assert_eq!(os_error("a (os error 13) (os error x)"), None);
        assert_eq!(
            interpreter_errno(
                "interpreter \"/x\" cannot be reached (a (os error 1) (os error 20))"
            ),
            Some(20)
        );
    }

    /// A CRLF script's notice names the format, not a missing file.
    #[test]
    fn the_notice_for_a_crlf_script_says_exec_format_error() {
        let reason = "its #! interpreter \"/bin/sh\\r\" does not exist (ENOENT) \
                      (the script has CRLF line endings; save it with LF endings)";
        let notice = String::from_utf8(fallback_notice("dev", spawn_errno(reason))).unwrap();
        assert!(notice.contains("(Exec format error)"), "{notice}");
        assert!(!notice.contains("No such file or directory"), "{notice}");
        assert!(!notice.contains("/bin/sh"), "{notice}");
    }

    #[test]
    fn the_notice_names_the_profile_sanitised_and_capped() {
        let notice = String::from_utf8(fallback_notice("dev", Some(13))).unwrap();
        assert_eq!(
            notice,
            "[Tessera] profile \"dev\" could not start (Permission denied); \
             started the default shell.\r\n"
        );
        let notice = String::from_utf8(fallback_notice("x", None)).unwrap();
        assert!(
            notice.contains("(the program could not be started)"),
            "{notice}"
        );
        let hostile = format!("\u{1b}[31m\u{202e}{}\u{7}\n", "é".repeat(200));
        let notice = String::from_utf8(fallback_notice(&hostile, Some(2))).unwrap();
        let name = notice
            .strip_prefix("[Tessera] profile \"")
            .and_then(|rest| rest.split_once("\" could not start"))
            .unwrap()
            .0;
        assert_eq!(name, format!("[31m{}", "é".repeat(62)));
        assert!(name.len() <= tessera_config::edit::MAX_DISPLAY_NAME_LEN);
        let body = notice.strip_suffix("\r\n").unwrap();
        assert!(!body.contains(char::is_control), "{notice:?}");
        assert!(!notice.contains('\u{202e}'), "{notice:?}");
    }

    /// The missing directory is named in its warning, escaped; the command
    /// is never logged (docs/security.md S8).
    #[test]
    fn the_warning_names_the_directory_escaped_and_never_the_command() {
        let profile = resolved(
            Some("deploy --token=s3cret"),
            Some("/nonexistent/\u{1b}[31mdir"),
        );
        let warnings = crate::session::tests::warnings(|| {
            let options = apply(SpawnOptions::login_shell("1"), &profile);
            assert_eq!(options.cwd, None, "the home directory");
        });
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("/nonexistent/\\u{1b}[31mdir"),
            "{warnings:?}"
        );
        assert!(!warnings[0].contains('\u{1b}'), "{warnings:?}");
        assert!(!warnings[0].contains("s3cret"), "{warnings:?}");
    }
}
