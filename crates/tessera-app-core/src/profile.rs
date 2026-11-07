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
