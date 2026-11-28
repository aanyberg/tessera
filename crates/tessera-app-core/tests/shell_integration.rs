//! `App::spawn_session` with shell integration, through real spawns: fake
//! `zsh`, `bash` and `fish` executables record the arguments and
//! environment they were started with, and the `execve` size check counts
//! the injected variables: a start they take over it runs without them,
//! with a notice (#519).

use std::collections::HashMap;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tessera_app_core::{
    App, Config, ConfigPaths, Error, PtyError, PtySize, SessionId, ShellIntegration, SpawnOptions,
};

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
        std::fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
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
    let recorded = recorded(&out, name);
    let _ = app.close_session(id);
    recorded
}

/// What the fake shell recorded in `out`, once it has.
fn recorded(out: &Path, name: &str) -> Recorded {
    let deadline = Instant::now() + TIMEOUT;
    while !out.exists() {
        assert!(
            Instant::now() < deadline,
            "{name}: the fake shell wrote nothing"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let text = std::fs::read_to_string(out).unwrap();
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
fn bash_starts_with_our_rcfile_or_a_prompt_command_loader() {
    let setup = setup();
    let app = app(&setup, ShellIntegration::Auto);
    let bash = setup.bin.join("bash");
    let script = scripts(&setup).join("bash/tessera.bash");
    let options = SpawnOptions::command(&bash, "x").args(["--rcfile", "/u/rc", "-i"]);
    let got = spawn(&app, &setup, options, "bash");
    assert_eq!(
        got.args,
        [
            path(&bash),
            "--rcfile".to_owned(),
            path(&script),
            "-i".to_owned()
        ]
    );
    assert_eq!(got.env["TESSERA_SHELL_INTEGRATION"], "enabled");
    assert_eq!(got.env["TESSERA_BASH_INJECT"], "rcfile");
    assert_eq!(got.env["TESSERA_BASH_RCFILE"], "/u/rc");
    assert!(script.is_file());
    // A login shell gets the loader alone (an empty PROMPT_COMMAND
    // requested, so an inherited one does not split the recorded line).
    let options = SpawnOptions::login_shell("x")
        .env("SHELL", &bash)
        .env("PROMPT_COMMAND", "");
    let got = spawn(&app, &setup, options, "login-bash");
    assert_eq!(got.args, [path(&bash)]);
    let prompt_command = &got.env["PROMPT_COMMAND"];
    assert!(
        prompt_command.ends_with(&format!(" . '{}' 2>/dev/null", path(&script))),
        "{prompt_command}"
    );
    for key in [
        "TESSERA_SHELL_INTEGRATION",
        "TESSERA_BASH_INJECT",
        "TESSERA_BASH_RCFILE",
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

/// The notice a session started without shell integration because of the
/// exec limit begins with (#519).
const TOO_LARGE: &str =
    "[Tessera] Shell integration is off for this session: the environment is too large (";

/// The visible rows of session `id`, trailing blanks trimmed.
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

/// The padding at which `options(padding)` takes exactly the exec limit
/// with integration off, and that limit: measured from the refusal of a
/// far larger one, so neither the injection's size nor this process's
/// environment is written into the test. Every app's first sessions have
/// one-digit ids, so the `TESSERA_SESSION_ID` the app adds is as long in
/// each. The fake shell's record in `out` of the start at the limit is
/// waited for and removed, so it cannot be read as a later start's.
fn at_the_limit(off: &App, out: &Path, options: impl Fn(usize) -> SpawnOptions) -> (usize, usize) {
    let probe = 2 * 1024 * 1024;
    let Err(Error::Pty(PtyError::ArgumentsTooLong { bytes, max })) =
        off.spawn_session(options(probe))
    else {
        panic!("a {probe}-byte variable should not fit");
    };
    let at_limit = probe - (bytes - max);
    let id = off
        .spawn_session(options(at_limit))
        .expect("exactly at the limit, integration off");
    recorded(out, "measure");
    std::fs::remove_file(out).unwrap();
    let _ = off.close_session(id);
    (at_limit, max)
}

/// The size check of `execve` arguments and environment (#74) counts the
/// variables the injection adds. Options that fit with integration off but
/// not with it on start without the injection, the shell's environment
/// and arguments as with integration off, and the terminal's first line
/// says so once, with the sizes (#519): for zsh's `ZDOTDIR` redirect and
/// for bash's `--rcfile` (#158). One byte more is refused with integration
/// on exactly as with it off.
#[test]
fn a_start_the_injection_takes_over_the_exec_limit_runs_without_it() {
    for name in ["zsh", "bash"] {
        let setup = setup();
        let off = app(&setup, ShellIntegration::Off);
        let auto = app(&setup, ShellIntegration::Auto);
        let shell = setup.bin.join(name);
        let out = setup.root.join(format!("{name}.out"));
        let options = |padding: usize| {
            SpawnOptions::command(&shell, "x")
                .arg("-i")
                .size(PtySize::new(24, 400))
                .env("HOME", &setup.home)
                .env("TESSERA_TEST_OUT", &out)
                .env("TESSERA_TEST_PADDING", "p".repeat(padding))
        };
        let (at_limit, max) = at_the_limit(&off, &out, options);

        let id = auto.spawn_session(options(at_limit)).expect("started");
        let got = recorded(&out, name);
        assert_eq!(got.args, [path(&shell), "-i".to_owned()], "{name}");
        for key in [
            "TESSERA_SHELL_INTEGRATION",
            "TESSERA_ORIG_ZDOTDIR",
            "TESSERA_BASH_INJECT",
            "TESSERA_BASH_RCFILE",
        ] {
            assert!(!got.env.contains_key(key), "{name}: {key} injected");
        }
        assert_ne!(
            got.env.get("ZDOTDIR").map(String::as_str),
            Some(path(&scripts(&setup).join("zsh")).as_str()),
            "{name}"
        );
        assert_eq!(got.env["TESSERA_TEST_PADDING"].len(), at_limit, "{name}");
        // The notice, once, as the first line; the fake shell writes
        // nothing to the terminal.
        let shown = rows(&auto, id);
        let notices: Vec<&String> = shown.iter().filter(|row| row.contains(TOO_LARGE)).collect();
        assert_eq!(notices.len(), 1, "{name}: {shown:?}");
        assert!(shown[0].starts_with(TOO_LARGE), "{name}: {shown:?}");
        let sizes = shown[0]
            .strip_prefix(TOO_LARGE)
            .and_then(|rest| rest.strip_suffix(" allowed)."))
            .and_then(|rest| rest.split_once(" bytes with it, more than the "))
            .unwrap_or_else(|| panic!("{name}: {:?}", shown[0]));
        let bytes: usize = sizes.0.parse().unwrap();
        assert_eq!(sizes.1.parse::<usize>().unwrap(), max, "{name}");
        assert!(bytes > max, "{name}: {bytes} <= {max}");
        assert!(shown[1..].iter().all(String::is_empty), "{name}: {shown:?}");
        let _ = auto.close_session(id);

        // Too large without the injection too: refused as with it off.
        let refused = |app: &App| match app.spawn_session(options(at_limit + 1)) {
            Err(Error::Pty(PtyError::ArgumentsTooLong { bytes, max })) => (bytes, max),
            other => panic!("{name}: one byte over the limit: {other:?}"),
        };
        assert_eq!(refused(&off), (max + 1, max), "{name}");
        assert_eq!(refused(&auto), (max + 1, max), "{name}");
        assert_eq!(auto.session_ids(), [], "{name}");
    }
}

/// A program shell integration leaves alone gets no retry and no notice:
/// over the limit it is refused as with integration off.
#[test]
fn a_program_without_integration_over_the_limit_is_refused_as_before() {
    let setup = setup();
    let off = app(&setup, ShellIntegration::Off);
    let auto = app(&setup, ShellIntegration::Auto);
    let sh = setup.bin.join("sh");
    let out = setup.root.join("sh.out");
    let options = |padding: usize| {
        SpawnOptions::command(&sh, "x")
            .env("HOME", &setup.home)
            .env("TESSERA_TEST_OUT", &out)
            .env("TESSERA_TEST_PADDING", "p".repeat(padding))
    };
    let (at_limit, max) = at_the_limit(&off, &out, options);
    let id = auto.spawn_session(options(at_limit)).expect("at the limit");
    recorded(&out, "sh");
    assert!(
        rows(&auto, id).iter().all(|row| !row.contains("[Tessera]")),
        "a notice for nothing"
    );
    let _ = auto.close_session(id);
    match auto.spawn_session(options(at_limit + 1)) {
        Err(Error::Pty(PtyError::ArgumentsTooLong { bytes, max: limit })) => {
            assert_eq!((bytes, limit), (max + 1, max));
        }
        other => panic!("one byte over the limit: {other:?}"),
    }
    assert!(!scripts(&setup).exists(), "scripts written for nothing");
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

/// Gives a directory locked with mode 000 its mode back when dropped, so
/// a test that fails while it is locked still leaves a temporary directory
/// that can be removed (#519 m7).
struct Unlock<'a>(&'a Path);

impl Drop for Unlock<'_> {
    fn drop(&mut self) {
        // Nothing to report from a drop: a failure here leaves the
        // directory behind, as before the guard.
        let _ = std::fs::set_permissions(self.0, Permissions::from_mode(0o700));
    }
}

/// With a profile (#333): a profile whose start fails falls back to the
/// login shell, and the injection taking that start over the limit too
/// gives both notices, each once: the profile's first, then shell
/// integration's. A profile that starts without the injection gets only
/// shell integration's.
#[test]
fn a_profile_fallback_and_the_exec_limit_retry_each_say_so_once() {
    let setup = setup();
    let zsh = setup.bin.join("zsh");
    let out = setup.root.join("login.out");
    let locked = setup.root.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, Permissions::from_mode(0o000)).unwrap();
    let unlock = Unlock(&locked);
    let entered = std::fs::read_dir(&locked).is_ok();
    let with_profile = |dir: &Path, setting: &str| {
        let text = format!(
            "[general]\ndefault_profile = \"p\"\n[terminal]\nshell_integration = \"{setting}\"\n\
             [[profiles]]\nname = \"p\"\nworking_directory = \"{}\"\n",
            dir.display()
        );
        let config = Config::from_toml_lenient(&text).unwrap().config;
        App::new(config, ConfigPaths::new(setup.root.join("config")))
    };
    let options = |padding: usize| {
        SpawnOptions::login_shell("x")
            .size(PtySize::new(24, 400))
            .env("SHELL", &zsh)
            .env("HOME", &setup.home)
            .env("TESSERA_TEST_OUT", &out)
            .env("TESSERA_TEST_PADDING", "p".repeat(padding))
    };
    let (at_limit, _) = at_the_limit(&app(&setup, ShellIntegration::Off), &out, options);

    // A directory that cannot be entered: the profile falls back.
    if entered {
        eprintln!("skipped the fallback half: a mode-000 directory does not stop this user");
    } else {
        let auto = with_profile(&locked, "auto");
        let id = auto.spawn_session(options(at_limit)).expect("fell back");
        let got = recorded(&out, "fallback");
        std::fs::remove_file(&out).unwrap();
        assert!(!got.env.contains_key("TESSERA_SHELL_INTEGRATION"));
        let shown = rows(&auto, id);
        assert!(
            shown[0].starts_with("[Tessera] profile \"p\" could not start ("),
            "{shown:?}"
        );
        assert!(shown[1].starts_with(TOO_LARGE), "{shown:?}");
        assert_eq!(
            shown.iter().filter(|row| row.contains("[Tessera]")).count(),
            2,
            "{shown:?}"
        );
        let _ = auto.close_session(id);
    }
    drop(unlock);

    // A directory that can: the profile's session, one notice.
    let auto = with_profile(&setup.home, "auto");
    let id = auto.spawn_session(options(at_limit)).expect("started");
    let got = recorded(&out, "profile");
    assert!(!got.env.contains_key("TESSERA_SHELL_INTEGRATION"));
    let shown = rows(&auto, id);
    assert!(shown[0].starts_with(TOO_LARGE), "{shown:?}");
    assert!(shown[1..].iter().all(String::is_empty), "{shown:?}");
    let _ = auto.close_session(id);
}
