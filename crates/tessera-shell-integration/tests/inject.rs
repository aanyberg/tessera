//! `Shell::detect` and `Injection::for_shell` for each shell and startup
//! form, including users who already have `ZDOTDIR`, `PROMPT_COMMAND` or
//! `XDG_DATA_DIRS` set.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;

use tessera_shell_integration::inject::{BASH_LOADER, ENABLED_VAR, FEATURES};
use tessera_shell_integration::scripts::SCRIPTS;
use tessera_shell_integration::{Injection, Launch, Shell};

const SCRIPTS_DIR: &str = "/s/1";

/// A user environment made of `vars`.
fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let vars: HashMap<String, OsString> = vars
        .iter()
        .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
        .collect();
    move |key| vars.get(key).cloned()
}

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

fn vars(list: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    list.iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)))
        .collect()
}

fn inject(shell: Shell, launch: Launch<'_>, user: &[(&str, &str)]) -> Injection {
    match shell {
        // Not injected yet; the rules are kept (and tested) for later.
        Shell::Bash => Injection::for_bash(Path::new(SCRIPTS_DIR), launch, env(user)),
        _ => Injection::for_shell(shell, Path::new(SCRIPTS_DIR), launch, env(user)),
    }
}

#[test]
fn bash_is_not_injected_yet() {
    for user_args in [
        &[][..],
        &["-i"][..],
        &["-l"][..],
        &["--rcfile", "/u/rc", "-i"][..],
    ] {
        let user_args = args(user_args);
        for launch in [Launch::Args(&user_args), Launch::Login] {
            assert!(
                Injection::for_shell(
                    Shell::Bash,
                    Path::new(SCRIPTS_DIR),
                    launch,
                    env(&[("PROMPT_COMMAND", "x")])
                )
                .is_empty(),
                "{launch:?}"
            );
        }
    }
}

#[test]
fn detects_shells_by_file_name() {
    assert_eq!(Shell::detect("/bin/zsh"), Some(Shell::Zsh));
    assert_eq!(Shell::detect("-zsh"), Some(Shell::Zsh));
    assert_eq!(Shell::detect("bash"), Some(Shell::Bash));
    assert_eq!(Shell::detect("/bin/-bash"), Some(Shell::Bash));
    assert_eq!(Shell::detect("/opt/homebrew/bin/fish"), Some(Shell::Fish));
    for other in ["/bin/sh", "dash", "zsh5", "/usr/bin/env", "", "/", "-"] {
        assert_eq!(Shell::detect(other), None, "{other:?}");
    }
    assert_eq!(Shell::Fish.name(), "fish");
}

// ---- zsh ----

/// A home directory with the given zsh startup files.
fn zsh_home(files: &[&str]) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    for file in files {
        std::fs::write(home.path().join(file), "").unwrap();
    }
    home
}

#[test]
fn zsh_redirects_zdotdir_when_the_user_had_none() {
    let home = zsh_home(&[".zshrc"]);
    let home = home.path().to_str().unwrap();
    let injection = inject(Shell::Zsh, Launch::Login, &[("HOME", home)]);
    assert_eq!(
        injection,
        Injection {
            env: vars(&[(ENABLED_VAR, FEATURES), ("ZDOTDIR", "/s/1/zsh")]),
            args: None,
        }
    );
}

#[test]
fn zsh_keeps_the_users_zdotdir_for_the_bootstrap_to_restore() {
    let zdotdir = zsh_home(&[".zshenv"]);
    let zdotdir = zdotdir.path().to_str().unwrap();
    let injection = inject(
        Shell::Zsh,
        Launch::Args(&args(&["-i"])),
        &[("HOME", "/nonexistent"), ("ZDOTDIR", zdotdir)],
    );
    assert_eq!(
        injection.env,
        vars(&[
            (ENABLED_VAR, FEATURES),
            ("ZDOTDIR", "/s/1/zsh"),
            ("TESSERA_ORIG_ZDOTDIR", zdotdir),
        ])
    );
    assert_eq!(injection.args, None);
}

#[test]
fn zsh_keeps_an_empty_zdotdir_and_finds_rc_files_in_home() {
    // Empty is not unset: zsh reads "/" then, and the bootstrap must
    // re-export the empty value. kitty looks in HOME for rc files.
    let home = zsh_home(&[".zprofile"]);
    let home = home.path().to_str().unwrap();
    let injection = inject(
        Shell::Zsh,
        Launch::Login,
        &[("HOME", home), ("ZDOTDIR", "")],
    );
    assert_eq!(
        injection.env,
        vars(&[
            (ENABLED_VAR, FEATURES),
            ("ZDOTDIR", "/s/1/zsh"),
            ("TESSERA_ORIG_ZDOTDIR", ""),
        ])
    );
}

#[test]
fn zsh_is_left_alone_for_a_new_user() {
    // No startup file: zsh-newuser-install must still run (kitty's rule).
    let home = zsh_home(&[".bashrc", ".zsh_history"]);
    let home = home.path().to_str().unwrap();
    assert!(inject(Shell::Zsh, Launch::Login, &[("HOME", home)]).is_empty());
    // Every one of the four startup files counts.
    for file in [".zshrc", ".zshenv", ".zprofile", ".zlogin"] {
        let home = zsh_home(&[file]);
        let home = home.path().to_str().unwrap();
        assert!(
            !inject(Shell::Zsh, Launch::Login, &[("HOME", home)]).is_empty(),
            "{file}"
        );
    }
    // A ZDOTDIR without startup files: no injection even if HOME has some.
    let home = zsh_home(&[".zshrc"]);
    let empty = zsh_home(&[]);
    let user = [
        ("HOME", home.path().to_str().unwrap()),
        ("ZDOTDIR", empty.path().to_str().unwrap()),
    ];
    assert!(inject(Shell::Zsh, Launch::Login, &user).is_empty());
    // No HOME at all.
    assert!(inject(Shell::Zsh, Launch::Login, &[]).is_empty());
}

#[test]
fn zsh_running_a_command_a_script_or_no_startup_files_is_left_alone() {
    let home = zsh_home(&[".zshrc"]);
    let user = [("HOME", home.path().to_str().unwrap())];
    for user_args in [
        &["-c", "true"][..],
        &["-ic", "true"][..],
        &["script.zsh"][..],
        &["-f"][..],
        &["--no-rcs"][..],
        &["-o", "norcs"][..],
        &["+o", "rcs"][..],
    ] {
        assert!(
            inject(Shell::Zsh, Launch::Args(&args(user_args)), &user).is_empty(),
            "{user_args:?}"
        );
    }
    for user_args in [
        &["-i"][..],
        &["-l"][..],
        &["-i", "script.zsh"][..],
        &["-o", "vi"][..],
    ] {
        assert!(
            !inject(Shell::Zsh, Launch::Args(&args(user_args)), &user).is_empty(),
            "{user_args:?}"
        );
    }
}

// ---- bash ----

const BASH_SCRIPT: &str = "/s/1/bash/tessera.bash";

#[test]
fn bash_non_login_starts_with_our_rcfile() {
    for user_args in [
        &[][..],
        &["-i"][..],
        &["-O", "extglob"][..],
        &["-i", "s.sh"][..],
    ] {
        let user_args = args(user_args);
        let injection = inject(Shell::Bash, Launch::Args(&user_args), &[]);
        let mut expected = args(&["--rcfile", BASH_SCRIPT]);
        expected.extend(user_args.iter().cloned());
        assert_eq!(injection.args, Some(expected), "{user_args:?}");
        assert_eq!(
            injection.env,
            vars(&[(ENABLED_VAR, FEATURES), ("TESSERA_BASH_INJECT", "rcfile")])
        );
    }
}

#[test]
fn bash_carries_the_users_rcfile() {
    for option in ["--rcfile", "--init-file"] {
        let user_args = args(&[option, "/u/rc", "-i"]);
        let injection = inject(Shell::Bash, Launch::Args(&user_args), &[]);
        assert_eq!(
            injection.args,
            Some(args(&["--rcfile", BASH_SCRIPT, "-i"])),
            "{option}"
        );
        assert_eq!(
            injection.env,
            vars(&[
                (ENABLED_VAR, FEATURES),
                ("TESSERA_BASH_INJECT", "rcfile"),
                ("TESSERA_BASH_RCFILE", "/u/rc"),
            ])
        );
    }
}

#[test]
fn bash_login_posix_and_norc_shells_load_from_prompt_command() {
    let expected = Injection {
        env: vars(&[
            (ENABLED_VAR, FEATURES),
            ("TESSERA_BASH_INJECT", "prompt-command"),
            ("TESSERA_BASH_SCRIPT", BASH_SCRIPT),
            ("PROMPT_COMMAND", BASH_LOADER),
        ]),
        args: None,
    };
    assert_eq!(inject(Shell::Bash, Launch::Login, &[]), expected);
    for user_args in [
        &["-l"][..],
        &["-il"][..],
        &["--login"][..],
        &["--posix"][..],
        &["--norc", "-i"][..],
        &["--login", "--rcfile", "/u/rc"][..],
    ] {
        assert_eq!(
            inject(Shell::Bash, Launch::Args(&args(user_args)), &[]),
            expected,
            "{user_args:?}"
        );
    }
}

#[test]
fn bash_keeps_an_exported_prompt_command_before_the_loader() {
    let injection = inject(
        Shell::Bash,
        Launch::Login,
        &[("PROMPT_COMMAND", "history -a")],
    );
    assert_eq!(
        injection.env,
        vars(&[
            (ENABLED_VAR, FEATURES),
            ("TESSERA_BASH_INJECT", "prompt-command"),
            ("TESSERA_BASH_SCRIPT", BASH_SCRIPT),
            ("PROMPT_COMMAND", &format!("history -a\n{BASH_LOADER}")),
        ])
    );
    // Set but empty: nothing to keep.
    let injection = inject(Shell::Bash, Launch::Login, &[("PROMPT_COMMAND", "")]);
    assert!(
        injection
            .env
            .contains(&("PROMPT_COMMAND".into(), OsString::from(BASH_LOADER)))
    );
}

#[test]
fn bash_running_a_command_or_a_script_is_left_alone() {
    for user_args in [
        &["-c", "true"][..],
        &["-ic", "true"][..],
        &["-lc", "true"][..],
        &["script.sh"][..],
        &["-x", "script.sh"][..],
        &["--", "script.sh"][..],
        &["-O", "extglob", "script.sh"][..],
    ] {
        assert!(
            inject(Shell::Bash, Launch::Args(&args(user_args)), &[]).is_empty(),
            "{user_args:?}"
        );
    }
}

#[test]
fn the_bash_loader_is_the_text_the_script_removes() {
    let script = SCRIPTS
        .iter()
        .find(|script| script.path == "bash/tessera.bash")
        .unwrap();
    let removal = format!("PROMPT_COMMAND=${{PROMPT_COMMAND//'{BASH_LOADER}'/:}}");
    assert!(
        script.contents.contains(&removal),
        "tessera.bash must remove {BASH_LOADER:?} from PROMPT_COMMAND"
    );
}

// ---- fish ----

#[test]
fn fish_prepends_the_scripts_dir_to_xdg_data_dirs() {
    let injection = inject(Shell::Fish, Launch::Login, &[("XDG_DATA_DIRS", "/a::/b:")]);
    assert_eq!(
        injection,
        Injection {
            env: vars(&[
                (ENABLED_VAR, FEATURES),
                ("TESSERA_FISH_XDG_DATA_DIR", SCRIPTS_DIR),
                ("XDG_DATA_DIRS", "/s/1:/a:/b"),
            ]),
            args: None,
        }
    );
}

#[test]
fn fish_without_xdg_data_dirs_keeps_the_defaults_visible() {
    for user in [&[][..], &[("XDG_DATA_DIRS", "")][..]] {
        let injection = inject(Shell::Fish, Launch::Args(&args(&["-i"])), user);
        assert_eq!(
            injection.env,
            vars(&[
                (ENABLED_VAR, FEATURES),
                ("TESSERA_FISH_XDG_DATA_DIR", SCRIPTS_DIR),
                ("TESSERA_FISH_XDG_WAS_UNSET", "1"),
                ("XDG_DATA_DIRS", "/s/1:/usr/local/share:/usr/share"),
            ]),
            "{user:?}"
        );
    }
}

#[test]
fn fish_cannot_list_a_dir_with_a_colon() {
    let injection = Injection::for_shell(Shell::Fish, Path::new("/a:b/1"), Launch::Login, env(&[]));
    assert!(injection.is_empty());
}
