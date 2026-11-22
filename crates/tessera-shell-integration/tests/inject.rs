//! `Shell::detect` and `Injection::for_shell` for each shell and startup
//! form, including users who already have `ZDOTDIR`, `PROMPT_COMMAND` or
//! `XDG_DATA_DIRS` set.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use tessera_shell_integration::inject::{BASH_LOADER_GUARD, ENABLED_VAR, FEATURES, bash_loader};
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
    Injection::for_shell(shell, Path::new(SCRIPTS_DIR), launch, env(user))
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

/// The loader for [`BASH_SCRIPT`], written out.
const LOADER: &str = r#"builtin [ -n "${_tessera_si_loader-}" ] || _tessera_si_load='enabled' . '/s/1/bash/tessera.bash' 2>/dev/null"#;

/// A home directory with no startup files, for a login bash's scan.
const EMPTY_HOME: (&str, &str) = ("HOME", "/nonexistent/tessera-test-home");

#[test]
fn the_bash_loader_quotes_the_words_and_the_path_literally() {
    assert_eq!(bash_loader(Path::new(BASH_SCRIPT)), OsString::from(LOADER));
    assert!(LOADER.starts_with(BASH_LOADER_GUARD));
    assert!(LOADER.contains(&format!("'{FEATURES}'")));
    // A space needs nothing; a `'` ends the quotes, is escaped and reopens
    // them. No variable is read, so none has to be in the environment.
    let script = Path::new("/a b/it's/bash/tessera.bash");
    assert_eq!(
        bash_loader(script),
        OsString::from(
            r#"builtin [ -n "${_tessera_si_loader-}" ] || _tessera_si_load='enabled' . '/a b/it'\''s/bash/tessera.bash' 2>/dev/null"#
        )
    );
    assert!(!LOADER.contains("TESSERA"), "{LOADER}");
    // Bytes that are not UTF-8 are kept as they are.
    let script = OsStr::from_bytes(b"/\xff/bash/tessera.bash");
    assert!(
        bash_loader(Path::new(script))
            .as_bytes()
            .ends_with(b" . '/\xff/bash/tessera.bash' 2>/dev/null")
    );
}

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
    // The loader alone: no variable of ours to leak into what the startup
    // files run, or to stay behind when they drop the loader.
    let expected = Injection {
        env: vars(&[("PROMPT_COMMAND", &format!("{LOADER}\n"))]),
        args: None,
    };
    assert_eq!(inject(Shell::Bash, Launch::Login, &[EMPTY_HOME]), expected);
    for user_args in [
        &["-l"][..],
        &["-il"][..],
        &["--login"][..],
        &["--posix"][..],
        &["--norc", "-i"][..],
        &["--login", "--rcfile", "/u/rc"][..],
        // M10: bash does not read `--rcfile` in these, or 5.x does not.
        &["-norc"][..],
        &["-norc", "-i"][..],
        &["-posix"][..],
        &["-o", "posix"][..],
        &["-o", "posix", "-i"][..],
        &["-oposix"][..],
    ] {
        assert_eq!(
            inject(Shell::Bash, Launch::Args(&args(user_args)), &[EMPTY_HOME]),
            expected,
            "{user_args:?}"
        );
    }
}

/// The route decides whether the startup files matter: only the loader can
/// be lost to an assignment, and only the files that bash reads count.
#[test]
fn bash_startup_files_decide_the_loader_route_only() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().to_str().unwrap();
    std::fs::write(home.path().join(".bash_profile"), "PROMPT_COMMAND=x\n").unwrap();
    let user = [("HOME", home_path)];
    assert_eq!(
        inject(Shell::Bash, Launch::Login, &user),
        Injection::default()
    );
    for login in [&["-l"][..], &["--login"][..], &["--norc", "-l"][..]] {
        assert_eq!(
            inject(Shell::Bash, Launch::Args(&args(login)), &user),
            Injection::default(),
            "{login:?}"
        );
    }
    // In .bashrc: bash 3.2 reads it for --posix; --norc does not, and the
    // rcfile route sources it before anything of ours depends on it.
    let bashrc = home.path().join(".bashrc");
    std::fs::rename(home.path().join(".bash_profile"), &bashrc).unwrap();
    let posix = inject(Shell::Bash, Launch::Args(&args(&["--posix"])), &user);
    assert_eq!(posix, Injection::default());
    for no_rc in [&["--norc"][..], &["--posix", "--norc"][..]] {
        let injection = inject(Shell::Bash, Launch::Args(&args(no_rc)), &user);
        assert_eq!(
            injection.env,
            vars(&[("PROMPT_COMMAND", &format!("{LOADER}\n"))]),
            "{no_rc:?}"
        );
    }
    let rcfile = inject(Shell::Bash, Launch::Args(&[]), &user);
    assert_eq!(rcfile.args, Some(args(&["--rcfile", BASH_SCRIPT])));
    // `--posix` reads $ENV, also with --norc.
    let posix = inject(
        Shell::Bash,
        Launch::Args(&args(&["--posix", "--norc"])),
        &[("HOME", home_path), ("ENV", bashrc.to_str().unwrap())],
    );
    assert_eq!(posix, Injection::default());
    // A login bash without a usable HOME cannot be checked.
    assert_eq!(
        inject(Shell::Bash, Launch::Login, &[]),
        Injection::default()
    );
}

/// M10: POSIX mode from the environment keeps bash off the rcfile route on
/// every launch, so no variable of ours is left in the shell.
#[test]
fn bash_posix_mode_from_the_environment_takes_the_loader() {
    for posix in [
        ("POSIXLY_CORRECT", "1"),
        ("POSIXLY_CORRECT", ""),
        ("SHELLOPTS", "braceexpand:posix"),
    ] {
        for user_args in [&[][..], &["-i"][..], &["--noprofile"][..], &["-l"][..]] {
            let injection = inject(
                Shell::Bash,
                Launch::Args(&args(user_args)),
                &[EMPTY_HOME, posix],
            );
            assert_eq!(injection.args, None, "{posix:?} {user_args:?}");
            assert_eq!(
                injection.env,
                vars(&[("PROMPT_COMMAND", &format!("{LOADER}\n"))]),
                "{posix:?} {user_args:?}"
            );
        }
    }
    let injection = inject(
        Shell::Bash,
        Launch::Args(&[]),
        &[EMPTY_HOME, ("SHELLOPTS", "braceexpand:vi")],
    );
    assert_eq!(injection.args, Some(args(&["--rcfile", BASH_SCRIPT])));
}

/// Tessera's own login launch counts posix mode from the environment too:
/// such a bash reads `~/.profile` even when `~/.bash_profile` exists, so an
/// assignment there leaves it to bash.
#[test]
fn a_posix_login_from_the_environment_scans_every_profile() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().to_str().unwrap();
    std::fs::write(home.path().join(".bash_profile"), "").unwrap();
    std::fs::write(home.path().join(".profile"), "PROMPT_COMMAND=__p\n").unwrap();
    let injected = inject(Shell::Bash, Launch::Login, &[("HOME", home_path)]);
    assert_eq!(
        injected.env,
        vars(&[("PROMPT_COMMAND", &format!("{LOADER}\n"))])
    );
    for posix in [("POSIXLY_CORRECT", "1"), ("SHELLOPTS", "braceexpand:posix")] {
        let injection = inject(Shell::Bash, Launch::Login, &[("HOME", home_path), posix]);
        assert!(injection.is_empty(), "{posix:?}: {injection:?}");
    }
}

/// B5: the loader is followed by a newline unless every append in the
/// startup files starts with a separator; with both kinds bash is left alone.
#[test]
fn bash_startup_appends_pick_what_follows_the_loader() {
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().to_str().unwrap();
    let user = [("HOME", home_path), ("PROMPT_COMMAND", "history -a")];
    let profile = home.path().join(".bash_profile");
    for (text, expected) in [
        (
            "PROMPT_COMMAND+='echo x'\n",
            Some(format!("history -a\n{LOADER}\n")),
        ),
        (
            "PROMPT_COMMAND=\"$PROMPT_COMMAND; x\"\n",
            Some(format!("history -a\n{LOADER}")),
        ),
        (
            "PROMPT_COMMAND=\"$PROMPT_COMMAND; x\"\nPROMPT_COMMAND+=y\n",
            None,
        ),
        ("PROMPT_COMMAND+=(x)\n", None),
    ] {
        std::fs::write(&profile, text).unwrap();
        let injection = inject(Shell::Bash, Launch::Login, &user);
        let expected = expected.map_or_else(Injection::default, |value| Injection {
            env: vars(&[("PROMPT_COMMAND", &value)]),
            args: None,
        });
        assert_eq!(injection, expected, "{text:?}");
    }
}

#[test]
fn bash_keeps_an_exported_prompt_command_before_the_loader() {
    let injection = inject(
        Shell::Bash,
        Launch::Login,
        &[EMPTY_HOME, ("PROMPT_COMMAND", "history -a")],
    );
    assert_eq!(
        injection.env,
        vars(&[("PROMPT_COMMAND", &format!("history -a\n{LOADER}\n"))])
    );
    // Set but empty: nothing to keep.
    let injection = inject(
        Shell::Bash,
        Launch::Login,
        &[EMPTY_HOME, ("PROMPT_COMMAND", "")],
    );
    assert!(injection.env.contains(&(
        "PROMPT_COMMAND".into(),
        OsString::from(format!("{LOADER}\n"))
    )));
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
