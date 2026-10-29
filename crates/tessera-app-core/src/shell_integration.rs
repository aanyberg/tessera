//! Shell integration on spawn (ADR 0021).
//!
//! [`App::spawn_session`](crate::App::spawn_session) passes its options
//! through [`apply`]: with `[terminal] shell_integration = "auto"` a zsh or
//! fish gets the environment that makes it load the scripts
//! `tessera-shell-integration` materialises under
//! [`ConfigPaths::shell_integration_dir`](tessera_config::ConfigPaths::shell_integration_dir).
//! The added variables are part of `SpawnOptions::env`, so the spawn's
//! `execve` size check counts them like any other. bash is started
//! unchanged for now (bash support is a follow-up, ADR 0021).

use std::ffi::OsString;
use std::path::Path;

use tessera_config::ShellIntegration;
use tessera_pty::{Program, SpawnOptions};
use tessera_shell_integration::inject::ENABLED_VAR;
use tessera_shell_integration::{Injection, Launch, Shell, scripts};

/// `options` with the shell integration applied, or unchanged: when
/// `setting` is off, when the child's environment already has
/// `TESSERA_SHELL_INTEGRATION` (set by the caller or inherited, whatever its
/// value), when the program is not a shell with scripts, when the scripts
/// cannot be written to `dir` (logged), or when the shell must not be
/// touched (see [`Injection::for_shell`]).
///
/// The shell is the program's file name, or `$SHELL` (the child's) for
/// [`Program::LoginShell`]; a login shell whose `$SHELL` is unset is not
/// changed (the PTY then falls back to the password database's shell, which
/// is not looked up here).
pub(crate) fn apply(options: SpawnOptions, setting: ShellIntegration, dir: &Path) -> SpawnOptions {
    apply_with(options, setting, dir, |key| std::env::var_os(key))
}

/// The injection for `options`, or `None` to start it unchanged.
fn injection_for(
    options: &SpawnOptions,
    dir: &Path,
    inherited: impl Fn(&str) -> Option<OsString>,
) -> Option<Injection> {
    // What the child sees without the injection: the spawn's own variables
    // (the last one of a name wins) over the inherited ones.
    let user_env = |key: &str| {
        options
            .env
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .or_else(|| inherited(key))
    };
    if user_env(ENABLED_VAR).is_some() {
        return None;
    }
    let (shell, launch) = match &options.program {
        Program::Command { program, args } => {
            (Shell::detect(program), Launch::Args(args.as_slice()))
        }
        Program::LoginShell => (user_env("SHELL").and_then(Shell::detect), Launch::Login),
    };
    let shell = shell?;
    // Decide first, so a shell that is left alone writes nothing.
    let injection = Injection::for_shell(shell, &scripts::version_dir(dir), launch, user_env);
    if injection.is_empty() {
        return None;
    }
    match scripts::materialise(dir) {
        Ok(_) => Some(injection),
        Err(err) => {
            tracing::warn!(
                target: "tessera_app_core",
                dir = %dir.display(),
                %err,
                "shell integration scripts could not be written; starting {} without them",
                shell.name()
            );
            None
        }
    }
}

/// [`apply`] with `inherited` standing in for this process's environment.
fn apply_with(
    mut options: SpawnOptions,
    setting: ShellIntegration,
    dir: &Path,
    inherited: impl Fn(&str) -> Option<OsString>,
) -> SpawnOptions {
    if setting == ShellIntegration::Off {
        return options;
    }
    let Some(injection) = injection_for(&options, dir, inherited) else {
        return options;
    };
    options
        .env
        .retain(|(name, _)| !injection.env.iter().any(|(key, _)| key == name));
    options.env.extend(injection.env);
    if let (Some(new_args), Program::Command { args, .. }) = (injection.args, &mut options.program)
    {
        *args = new_args;
    }
    options
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsStr;

    use tessera_shell_integration::scripts::version_dir;

    use super::*;

    /// A temporary scripts root and a home directory with a `.zshrc`.
    struct Dirs {
        _root: tempfile::TempDir,
        scripts: std::path::PathBuf,
        home: String,
    }

    fn dirs() -> Dirs {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join(".zshrc"), "").unwrap();
        Dirs {
            scripts: root.path().join("shell-integration"),
            home: home.to_str().unwrap().to_owned(),
            _root: root,
        }
    }

    fn inherited(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        move |key| vars.get(key).cloned()
    }

    fn env_of<'a>(options: &'a SpawnOptions, key: &str) -> Vec<&'a OsStr> {
        options
            .env
            .iter()
            .filter(|(name, _)| name == key)
            .map(|(_, value)| value.as_os_str())
            .collect()
    }

    fn args_of(options: &SpawnOptions) -> Vec<String> {
        match &options.program {
            Program::Command { args, .. } => args
                .iter()
                .map(|arg| arg.to_str().unwrap().to_owned())
                .collect(),
            Program::LoginShell => Vec::new(),
        }
    }

    #[test]
    fn zsh_gets_the_zdotdir_redirect() {
        let dirs = dirs();
        let options = SpawnOptions::command("/bin/zsh", "1").arg("-i");
        let out = apply_with(
            options,
            ShellIntegration::Auto,
            &dirs.scripts,
            inherited(&[("HOME", &dirs.home), ("ZDOTDIR", &dirs.home)]),
        );
        let zsh = version_dir(&dirs.scripts).join("zsh");
        assert_eq!(env_of(&out, "ZDOTDIR"), [zsh.as_os_str()]);
        assert_eq!(
            env_of(&out, "TESSERA_ORIG_ZDOTDIR"),
            [OsStr::new(&dirs.home)]
        );
        assert_eq!(env_of(&out, ENABLED_VAR), [OsStr::new("enabled")]);
        assert_eq!(args_of(&out), ["-i"]);
        assert!(zsh.join(".zshenv").is_file(), "scripts not written");
    }

    #[test]
    fn a_requested_variable_replaces_the_inherited_one_and_is_overridden() {
        let dirs = dirs();
        // The spawn asks for ZDOTDIR=/req: that is the user's value, and
        // the injection's ZDOTDIR replaces the entry instead of following it.
        let options = SpawnOptions::command("zsh", "1").env("ZDOTDIR", &dirs.home);
        let out = apply_with(
            options,
            ShellIntegration::Auto,
            &dirs.scripts,
            inherited(&[("HOME", "/nonexistent"), ("ZDOTDIR", "/inherited")]),
        );
        let zsh = version_dir(&dirs.scripts).join("zsh");
        assert_eq!(env_of(&out, "ZDOTDIR"), [zsh.as_os_str()]);
        assert_eq!(
            env_of(&out, "TESSERA_ORIG_ZDOTDIR"),
            [OsStr::new(&dirs.home)]
        );
    }

    #[test]
    fn bash_is_started_unchanged() {
        let dirs = dirs();
        for options in [
            SpawnOptions::command("/bin/bash", "1"),
            SpawnOptions::command("/bin/bash", "1").arg("-i"),
            SpawnOptions::login_shell("1"),
        ] {
            let out = apply_with(
                options.clone(),
                ShellIntegration::Auto,
                &dirs.scripts,
                inherited(&[("SHELL", "/bin/bash"), ("HOME", &dirs.home)]),
            );
            assert_eq!(out, options);
        }
        assert!(!dirs.scripts.exists(), "scripts written for nothing");
    }

    #[test]
    fn a_login_shell_is_found_through_the_childs_shell_variable() {
        let dirs = dirs();
        // The spawn's own SHELL wins over the inherited one.
        let options = SpawnOptions::login_shell("1").env("SHELL", "/opt/homebrew/bin/fish");
        let out = apply_with(
            options,
            ShellIntegration::Auto,
            &dirs.scripts,
            inherited(&[("SHELL", "/bin/bash")]),
        );
        assert_eq!(env_of(&out, "PROMPT_COMMAND"), Vec::<&OsStr>::new());
        assert_eq!(
            env_of(&out, "TESSERA_FISH_XDG_DATA_DIR"),
            [version_dir(&dirs.scripts).as_os_str()]
        );
        // No SHELL at all: unchanged.
        let options = SpawnOptions::login_shell("1");
        let out = apply_with(
            options.clone(),
            ShellIntegration::Auto,
            &dirs.scripts,
            inherited(&[]),
        );
        assert_eq!(out, options);
    }

    #[test]
    fn off_other_programs_and_a_preset_variable_change_nothing() {
        let dirs = dirs();
        let user = [("HOME", dirs.home.as_str()), ("SHELL", "/bin/zsh")];
        let zsh = SpawnOptions::command("/bin/zsh", "1");
        let unchanged = |options: SpawnOptions, setting, user: &[(&str, &str)]| {
            let out = apply_with(options.clone(), setting, &dirs.scripts, inherited(user));
            assert_eq!(out, options);
        };
        unchanged(zsh.clone(), ShellIntegration::Off, &user);
        unchanged(SpawnOptions::login_shell("1"), ShellIntegration::Off, &user);
        unchanged(
            SpawnOptions::command("/bin/sh", "1"),
            ShellIntegration::Auto,
            &user,
        );
        unchanged(
            SpawnOptions::command("/bin/bash", "1").args(["-c", "true"]),
            ShellIntegration::Auto,
            &user,
        );
        // Requested by the caller, or inherited: either value.
        unchanged(
            zsh.clone().env(ENABLED_VAR, "0"),
            ShellIntegration::Auto,
            &user,
        );
        unchanged(
            zsh.clone().env(ENABLED_VAR, "enabled"),
            ShellIntegration::Auto,
            &user,
        );
        let mut preset = user.to_vec();
        preset.push((ENABLED_VAR, "0"));
        unchanged(zsh, ShellIntegration::Auto, &preset);
        assert!(!dirs.scripts.exists(), "scripts written for nothing");
    }

    #[test]
    fn unwritable_scripts_dir_starts_the_shell_unchanged() {
        let dirs = dirs();
        let blocker = dirs.scripts.with_file_name("file");
        std::fs::write(&blocker, "").unwrap();
        let options = SpawnOptions::command("/bin/zsh", "1");
        let out = apply_with(
            options.clone(),
            ShellIntegration::Auto,
            &blocker.join("shell-integration"),
            inherited(&[("HOME", &dirs.home)]),
        );
        assert_eq!(out, options);
    }
}
