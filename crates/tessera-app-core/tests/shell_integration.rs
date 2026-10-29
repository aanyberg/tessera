//! `App::spawn_session` with shell integration, through real spawns: fake
//! `zsh`, `bash` and `fish` executables record the arguments and
//! environment they were started with, and the `execve` size check counts
//! the injected variables.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tessera_app_core::{App, Config, ConfigPaths, PtyError, ShellIntegration, SpawnOptions};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Setup {
    _root: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
    home: PathBuf,
}

/// A config root, a home with a `.zshrc`, and fake shells in `bin/` that
/// write `arg:<each argv entry>` lines and their environment to
/// `$TESSERA_TEST_OUT`.
fn setup() -> Setup {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let bin = root.join("bin");
    let home = root.join("home");
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join(".zshrc"), "").unwrap();
    let fake = "#!/bin/sh\n\
                { for a in \"$0\" \"$@\"; do printf 'arg:%s\\n' \"$a\"; done; env; } \
                > \"$TESSERA_TEST_OUT.tmp\"\n\
                mv \"$TESSERA_TEST_OUT.tmp\" \"$TESSERA_TEST_OUT\"\n";
    for name in ["zsh", "bash", "fish", "sh"] {
        let path = bin.join(name);
        std::fs::write(&path, fake).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
    Setup {
        _root: temp,
        root,
        bin,
        home,
    }
}

fn app(setup: &Setup, setting: ShellIntegration) -> App {
    let mut config = Config::default();
    config.terminal.shell_integration = setting;
    App::new(config, ConfigPaths::new(setup.root.join("config")))
}

/// What a fake shell recorded.
struct Recorded {
    args: Vec<String>,
    env: HashMap<String, String>,
}

/// Spawns `options` (with `HOME` and the output file added) and returns
/// what the fake shell recorded.
fn spawn(app: &App, setup: &Setup, options: SpawnOptions, name: &str) -> Recorded {
    let out = setup.root.join(format!("{name}.out"));
    let options = options
        .env("HOME", &setup.home)
        .env("TESSERA_TEST_OUT", &out);
    let id = app.spawn_session(options).expect("spawn");
    let deadline = Instant::now() + TIMEOUT;
    while !out.exists() {
        assert!(
            Instant::now() < deadline,
            "{name}: the fake shell wrote nothing"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = app.close_session(id);
    let text = std::fs::read_to_string(&out).unwrap();
    let mut args = Vec::new();
    let mut env = HashMap::new();
    for line in text.lines() {
        if let Some(arg) = line.strip_prefix("arg:") {
            args.push(arg.to_owned());
        } else if let Some((key, value)) = line.split_once('=') {
            env.insert(key.to_owned(), value.to_owned());
        }
    }
    Recorded { args, env }
}

fn scripts(setup: &Setup) -> PathBuf {
    tessera_shell_integration::scripts::version_dir(&setup.root.join("config/shell-integration"))
}

fn path(p: &Path) -> String {
    p.to_str().unwrap().to_owned()
}

#[test]
fn zsh_starts_with_the_zdotdir_redirect() {
    let setup = setup();
    let app = app(&setup, ShellIntegration::Auto);
    let zsh = setup.bin.join("zsh");
    let got = spawn(
        &app,
        &setup,
        SpawnOptions::command(&zsh, "x").arg("-i"),
        "zsh",
    );
    assert_eq!(got.args, [path(&zsh), "-i".to_owned()]);
    assert_eq!(got.env["TESSERA_SHELL_INTEGRATION"], "enabled");
    assert_eq!(got.env["ZDOTDIR"], path(&scripts(&setup).join("zsh")));
    assert!(scripts(&setup).join("zsh/.zshenv").is_file());
}

#[test]
fn bash_is_started_unchanged() {
    // Bash support is a follow-up (ADR 0021).
    let setup = setup();
    let app = app(&setup, ShellIntegration::Auto);
    let bash = setup.bin.join("bash");
    let options = SpawnOptions::command(&bash, "x").args(["--rcfile", "/u/rc", "-i"]);
    let got = spawn(&app, &setup, options, "bash");
    assert_eq!(
        got.args,
        [
            path(&bash),
            "--rcfile".to_owned(),
            "/u/rc".to_owned(),
            "-i".to_owned()
        ]
    );
    let options = SpawnOptions::login_shell("x").env("SHELL", &bash);
    let got = spawn(&app, &setup, options, "login-bash");
    assert_eq!(got.args, [path(&bash)]);
    for key in [
        "TESSERA_SHELL_INTEGRATION",
        "TESSERA_BASH_INJECT",
        "TESSERA_BASH_SCRIPT",
    ] {
        assert!(!got.env.contains_key(key), "{key}");
    }
}

#[test]
fn fish_starts_with_our_data_dir_first() {
    let setup = setup();
    let app = app(&setup, ShellIntegration::Auto);
    let fish = setup.bin.join("fish");
    let options = SpawnOptions::command(&fish, "x").env("XDG_DATA_DIRS", "/a:/b");
    let got = spawn(&app, &setup, options, "fish");
    assert_eq!(got.args, [path(&fish)]);
    assert_eq!(
        got.env["XDG_DATA_DIRS"],
        format!("{}:/a:/b", path(&scripts(&setup)))
    );
    assert_eq!(got.env["TESSERA_FISH_XDG_DATA_DIR"], path(&scripts(&setup)));
}

#[test]
fn off_and_other_programs_start_unchanged() {
    let setup = setup();
    let off = app(&setup, ShellIntegration::Off);
    let zsh = setup.bin.join("zsh");
    let got = spawn(
        &off,
        &setup,
        SpawnOptions::command(&zsh, "x").arg("-i"),
        "off",
    );
    assert_eq!(got.args, [path(&zsh), "-i".to_owned()]);
    for key in ["TESSERA_SHELL_INTEGRATION", "TESSERA_ORIG_ZDOTDIR"] {
        assert!(!got.env.contains_key(key), "{key} set with off");
    }
    assert_ne!(
        got.env.get("ZDOTDIR").map(String::as_str),
        Some(path(&scripts(&setup).join("zsh")).as_str())
    );

    let auto = app(&setup, ShellIntegration::Auto);
    let sh = setup.bin.join("sh");
    let got = spawn(&auto, &setup, SpawnOptions::command(&sh, "x"), "sh");
    assert!(!got.env.contains_key("TESSERA_SHELL_INTEGRATION"));
    // Preset by the caller: kept as given, nothing added.
    let options = SpawnOptions::command(&zsh, "x").env("TESSERA_SHELL_INTEGRATION", "0");
    let got = spawn(&auto, &setup, options, "preset");
    assert_eq!(got.env["TESSERA_SHELL_INTEGRATION"], "0");
    assert!(!scripts(&setup).exists(), "scripts written for nothing");
}

/// The size check of `execve` arguments and environment (#74) counts the
/// variables the injection adds: options that fit with integration off are
/// refused with it on.
#[test]
fn injected_variables_count_toward_the_exec_limit() {
    let setup = setup();
    let off = app(&setup, ShellIntegration::Off);
    let auto = app(&setup, ShellIntegration::Auto);
    let zsh = setup.bin.join("zsh");
    let out = setup.root.join("limit.out");
    let options = |padding: usize| {
        SpawnOptions::command(&zsh, "x")
            .env("HOME", &setup.home)
            .env("TESSERA_TEST_OUT", &out)
            .env("TESSERA_TEST_PADDING", "p".repeat(padding))
    };
    // Measure: far over the limit, the error reports the footprint and the
    // limit.
    let probe = 2 * 1024 * 1024;
    let Err(tessera_app_core::Error::Pty(PtyError::ArgumentsTooLong { bytes, max })) =
        off.spawn_session(options(probe))
    else {
        panic!("a {probe}-byte variable should not fit");
    };
    let at_limit = probe - (bytes - max);
    // Exactly at the limit with integration off: accepted.
    let id = off.spawn_session(options(at_limit)).expect("at the limit");
    let _ = off.close_session(id);
    // The same options with integration on are over it.
    match auto.spawn_session(options(at_limit)) {
        Err(tessera_app_core::Error::Pty(PtyError::ArgumentsTooLong { bytes, max: limit })) => {
            assert_eq!(limit, max);
            assert!(bytes > max, "{bytes} <= {max}");
        }
        other => panic!("injection not counted: {other:?}"),
    }
    assert_eq!(auto.session_ids(), []);
}

/// A configuration change reaches the next spawn: Auto, then Off, then
/// Auto again.
#[test]
fn a_changed_setting_applies_to_the_next_spawn() {
    let setup = setup();
    let app = app(&setup, ShellIntegration::Auto);
    let zsh = setup.bin.join("zsh");
    let injected = |name: &str| {
        spawn(&app, &setup, SpawnOptions::command(&zsh, "x"), name)
            .env
            .contains_key("TESSERA_SHELL_INTEGRATION")
    };
    let apply = |setting| {
        let mut config = Config::default();
        config.terminal.shell_integration = setting;
        app.apply_config(&config, &tessera_config::Theme::default());
    };
    assert!(injected("auto-1"));
    apply(ShellIntegration::Off);
    assert!(!injected("off"));
    apply(ShellIntegration::Auto);
    assert!(injected("auto-2"));
}
