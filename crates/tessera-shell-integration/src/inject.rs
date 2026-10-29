//! Automatic injection: how a shell Tessera starts is made to load the
//! scripts.
//!
//! The model is kitty's: each shell's own startup mechanism is redirected
//! through the environment (and, for bash, the arguments), so nothing has to
//! be installed and the user's startup files run as before. See
//! `docs/adr/0021-shell-integration-injection.md`.
//!
//! - **zsh**: `ZDOTDIR=<scripts>/zsh`. Its `.zshenv` restores the user's
//!   `ZDOTDIR` (kept in `TESSERA_ORIG_ZDOTDIR` when there was one), sources
//!   the user's `.zshenv` and loads the integration; zsh then reads the other
//!   startup files from the user's directory. A user with no zsh startup
//!   file at all gets no injection, so `zsh-newuser-install` can still run,
//!   and neither does a zsh running a command (`-c`) or a script without
//!   `-i`, or one told to skip startup files (`-f`, `--no-rcs`): it would
//!   never put `ZDOTDIR` back, and programs it starts (tmux) would inherit
//!   ours.
//! - **bash**: not injected yet ([`Injection::for_shell`] leaves bash
//!   alone; bash support is a follow-up, see ADR 0021: the login route's
//!   `PROMPT_COMMAND` loader breaks under prompt tools that wrap the
//!   existing `PROMPT_COMMAND`, such as starship). [`Injection::for_bash`]
//!   keeps the rules, which the live tests use to test the bash script: an
//!   interactive non-login shell would start as
//!   `bash --rcfile <scripts>/bash/tessera.bash`, and the script sources
//!   `~/.bashrc` (or the user's own `--rcfile`, carried in
//!   `TESSERA_BASH_RCFILE`). A login shell ignores `--rcfile` and, as
//!   Tessera's default shell, takes no arguments at all, so it (and a
//!   `--posix` or `--norc` shell) instead gets an exported `PROMPT_COMMAND`
//!   that sources the script at the first prompt. kitty's `ENV` plus
//!   `--posix` does not work on the macOS `/bin/bash` 3.2. A shell running a
//!   command (`-c`) or a script file is not touched.
//! - **fish**: `<scripts>` goes first in `XDG_DATA_DIRS`, so fish loads
//!   `<scripts>/fish/vendor_conf.d/tessera-shell-integration.fish`, which
//!   takes it out again.
//!
//! Every injection sets [`ENABLED_VAR`] to [`FEATURES`].

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Environment variable that turns the scripts on and carries their feature
/// words (`enabled`, `no-cwd`, `no-prompt-mark`); `0` or unset is off.
pub const ENABLED_VAR: &str = "TESSERA_SHELL_INTEGRATION";

/// The value of [`ENABLED_VAR`] in an injection: every feature on.
pub const FEATURES: &str = "enabled";

/// The text of the `PROMPT_COMMAND` that loads the bash script at the first
/// prompt. The script removes exactly this text again, so the two must
/// match.
pub const BASH_LOADER: &str = r#". "$TESSERA_BASH_SCRIPT""#;

/// A shell with integration scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shell {
    /// zsh 5.1 or later.
    Zsh,
    /// bash 3.2 or later.
    Bash,
    /// fish 3.2 or later.
    Fish,
}

impl Shell {
    /// The shell `argv0` runs: the file name of a path, with a leading `-`
    /// (a login shell's `argv[0]`) ignored. `None` for any other program,
    /// `sh` included.
    #[must_use]
    pub fn detect(argv0: impl AsRef<OsStr>) -> Option<Self> {
        let name = Path::new(argv0.as_ref()).file_name()?.to_str()?;
        match name.strip_prefix('-').unwrap_or(name) {
            "zsh" => Some(Self::Zsh),
            "bash" => Some(Self::Bash),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    /// The name the scripts report in their handshake (`zsh`, `bash`,
    /// `fish`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Fish => "fish",
        }
    }
}

/// How the shell is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch<'a> {
    /// As a login shell with no arguments (`argv[0]` starting with `-`), the
    /// way `tessera_pty::Program::LoginShell` runs `$SHELL`.
    Login,
    /// As a program with these arguments (after `argv[0]`).
    Args(&'a [OsString]),
}

/// What to change in a shell's spawn so it loads the integration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Injection {
    /// Variables to set, overriding any inherited or requested value.
    pub env: Vec<(OsString, OsString)>,
    /// The arguments to start the shell with instead of the requested ones,
    /// or `None` to keep them.
    pub args: Option<Vec<OsString>>,
}

impl Injection {
    /// Whether this injection changes nothing (an unknown shell, or a shell
    /// that must not be touched).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.env.is_empty() && self.args.is_none()
    }

    /// The injection that makes `shell`, started as `launch`, load the
    /// scripts in `scripts_dir` (the directory
    /// [`scripts::materialise`](crate::scripts::materialise) returns).
    ///
    /// `user_env` looks up a variable in the environment the shell would get
    /// without the injection (what Tessera inherited, overridden by the
    /// spawn's own variables); `None` means unset, which is not the same as
    /// empty. For zsh this reads the file system: when neither the user's
    /// `ZDOTDIR` nor (if that is unset or empty) `HOME` holds a `.zshrc`,
    /// `.zshenv`, `.zprofile` or `.zlogin`, nothing is injected (kitty's
    /// rule, so `zsh-newuser-install` runs for a new user; kitty also asks
    /// zsh for a `ZDOTDIR` set by `/etc/zshenv` in that case, which this
    /// does not).
    ///
    /// Returns an empty injection when nothing should change: any bash (bash
    /// support is a follow-up, see [`Injection::for_bash`]), a zsh running a
    /// command (`-c`) or a script without `-i`, a zsh skipping
    /// its startup files (`-f`, `--no-rcs`, `-o norcs`), a fish whose
    /// `scripts_dir` contains `:` (it could not be listed in
    /// `XDG_DATA_DIRS`), or the zsh case above.
    #[must_use]
    pub fn for_shell(
        shell: Shell,
        scripts_dir: &Path,
        launch: Launch<'_>,
        user_env: impl Fn(&str) -> Option<OsString>,
    ) -> Self {
        match shell {
            Shell::Zsh => zsh(scripts_dir, launch, &user_env),
            Shell::Bash => Self::default(),
            Shell::Fish => fish(scripts_dir, &user_env),
        }
    }

    /// What injecting a bash would take; [`Injection::for_shell`] does not
    /// use it (Tessera starts bash unchanged until bash support lands, see
    /// ADR 0021). Kept so the bash script and these rules stay tested: an
    /// interactive non-login bash starts with `--rcfile` and our script,
    /// which sources `~/.bashrc` (or the user's `--rcfile`); a login,
    /// `--posix` or `--norc` bash gets an exported `PROMPT_COMMAND` loader;
    /// a bash running a command (`-c`) or a script without `-i` gets nothing.
    #[must_use]
    pub fn for_bash(
        scripts_dir: &Path,
        launch: Launch<'_>,
        user_env: impl Fn(&str) -> Option<OsString>,
    ) -> Self {
        bash(scripts_dir, launch, &user_env)
    }
}

fn enabled() -> (OsString, OsString) {
    (ENABLED_VAR.into(), FEATURES.into())
}

/// Whether a zsh started with `args` reads startup files interactively:
/// not with `-c` (a command, even with `-i`), `-f` / `--no-rcs` /
/// `-o norcs` / `+o rcs` (no startup files beyond `/etc/zshenv`, so our
/// `.zshenv` would never put `ZDOTDIR` back), or a script operand without
/// `-i` (or `--interactive`). `-` and `--` end the options; after `-s`
/// (commands from standard input) an operand is a positional parameter, not
/// a script. Option names are compared the way zsh does: case and `_`
/// ignored, a `no` prefix inverting.
fn zsh_reads_startup_files(args: &[OsString]) -> bool {
    /// Whether option `name`, set with `-o` (`on`) or `+o`, turns `rcs` off.
    fn turns_off_rcs(name: &[u8], on: bool) -> bool {
        let name = normalized(name);
        match name.strip_prefix(b"no") {
            Some(b"rcs") => on,
            _ => name == b"rcs" && !on,
        }
    }
    /// Option `name` with `_` and `-` dropped and lower-cased.
    fn normalized(name: &[u8]) -> Vec<u8> {
        name.iter()
            .filter(|&&b| b != b'_' && b != b'-')
            .map(u8::to_ascii_lowercase)
            .collect()
    }
    let mut interactive = false;
    let mut stdin = false;
    let mut option_name: Option<bool> = None;
    let mut end_of_options = false;
    for arg in args {
        let bytes = arg.as_encoded_bytes();
        if let Some(on) = option_name.take() {
            if turns_off_rcs(bytes, on) {
                return false;
            }
            continue;
        }
        if end_of_options {
            return interactive || stdin;
        }
        if bytes == b"-" || bytes == b"--" {
            end_of_options = true;
            continue;
        }
        if let Some(long) = bytes.strip_prefix(b"--") {
            if turns_off_rcs(long, true) {
                return false;
            }
            match normalized(long).as_slice() {
                b"interactive" => interactive = true,
                b"shinstdin" => stdin = true,
                _ => {}
            }
            continue;
        }
        let on = match bytes.first() {
            Some(b'-') => true,
            Some(b'+') => false,
            _ => return interactive || stdin,
        };
        let letters = &bytes[1..];
        for (i, &letter) in letters.iter().enumerate() {
            match letter {
                b'c' => return false,
                b'f' if on => return false,
                b'i' => interactive = interactive || on,
                b'o' => {
                    let rest = &letters[i + 1..];
                    if rest.is_empty() {
                        option_name = Some(on);
                    } else if turns_off_rcs(rest, on) {
                        return false;
                    }
                    break;
                }
                b's' => stdin = stdin || on,
                _ => {}
            }
        }
    }
    true
}

fn zsh(
    scripts_dir: &Path,
    launch: Launch<'_>,
    user_env: &dyn Fn(&str) -> Option<OsString>,
) -> Injection {
    if let Launch::Args(args) = launch
        && !zsh_reads_startup_files(args)
    {
        return Injection::default();
    }
    let orig = user_env("ZDOTDIR");
    let rc_dir = match &orig {
        Some(dir) if !dir.is_empty() => dir.clone(),
        _ => match user_env("HOME") {
            Some(home) if !home.is_empty() => home,
            _ => return Injection::default(),
        },
    };
    let rc_dir = Path::new(&rc_dir);
    if ![".zshrc", ".zshenv", ".zprofile", ".zlogin"]
        .iter()
        .any(|file| rc_dir.join(file).exists())
    {
        return Injection::default();
    }
    let mut env = vec![
        enabled(),
        ("ZDOTDIR".into(), scripts_dir.join("zsh").into_os_string()),
    ];
    if let Some(orig) = orig {
        env.push(("TESSERA_ORIG_ZDOTDIR".into(), orig));
    }
    Injection { env, args: None }
}

/// What the arguments of a bash say about how it runs.
#[derive(Debug, Default)]
struct BashArgs {
    /// Runs a command string (`-c`) or a script without `-i`: not touched.
    skip: bool,
    /// Reads the login startup files (`-l`, `--login`), which never read an
    /// `--rcfile`.
    login: bool,
    /// `--posix` or `--norc`: does not read `~/.bashrc` either.
    no_rc: bool,
    /// The file named by `--rcfile` or `--init-file`.
    rcfile: Option<OsString>,
    /// Indices of the `--rcfile`/`--init-file` arguments, removed when ours
    /// replaces them.
    remove: Vec<usize>,
}

/// Reads bash's arguments the way kitty's `setup_bash_env` does: long
/// options only before the first short one, `-c` (even in `-ic`) means a
/// command, a file operand without `-i` a script; `-s` and `--`/`-` end the
/// options. Unlike kitty, `-o`/`+o` take an argument like `-O`/`+O`, and
/// `-l`/`--login` are noted.
fn parse_bash_args(args: &[OsString]) -> BashArgs {
    let mut parsed = BashArgs::default();
    let mut long_options = true;
    let mut skip_next = false;
    let mut interactive = false;
    let mut file_operand = false;
    let mut end_of_options = false;
    for (i, arg) in args.iter().enumerate() {
        if end_of_options {
            file_operand = true;
            break;
        }
        if skip_next {
            skip_next = false;
            continue;
        }
        let bytes = arg.as_encoded_bytes();
        if bytes == b"-" || bytes == b"--" {
            end_of_options = true;
            continue;
        }
        if bytes.len() > 1 && (bytes[0] == b'-' || bytes[0] == b'+') && bytes[1] != b'-' {
            long_options = false;
            let mut options = &bytes[1..];
            // `-O name` / `-o name`: the rest of the cluster or the next
            // argument is the option name.
            if let Some(at) = options.iter().position(|&b| b == b'O' || b == b'o') {
                skip_next = at + 1 == options.len();
                options = &options[..at];
            }
            if options.contains(&b'c') {
                parsed.skip = true;
                return parsed;
            }
            if options.contains(&b'l') {
                parsed.login = true;
            }
            if options.contains(&b'i') {
                interactive = true;
            }
            if options.contains(&b's') {
                break;
            }
        } else if bytes.starts_with(b"--") && long_options {
            match bytes {
                b"--login" => parsed.login = true,
                b"--posix" | b"--norc" => parsed.no_rc = true,
                b"--rcfile" | b"--init-file" if i + 1 < args.len() => {
                    parsed.rcfile = Some(args[i + 1].clone());
                    parsed.remove.extend([i, i + 1]);
                    skip_next = true;
                }
                _ => {}
            }
        } else {
            file_operand = true;
            break;
        }
    }
    parsed.skip = file_operand && !interactive;
    parsed
}

fn bash(
    scripts_dir: &Path,
    launch: Launch<'_>,
    user_env: &dyn Fn(&str) -> Option<OsString>,
) -> Injection {
    let script = scripts_dir.join("bash").join("tessera.bash");
    let rcfile_args = match launch {
        Launch::Login => None,
        Launch::Args(args) => {
            let parsed = parse_bash_args(args);
            if parsed.skip {
                return Injection::default();
            }
            (!parsed.login && !parsed.no_rc).then(|| {
                let mut new_args: Vec<OsString> = vec!["--rcfile".into(), script.clone().into()];
                new_args.extend(
                    args.iter()
                        .enumerate()
                        .filter(|(i, _)| !parsed.remove.contains(i))
                        .map(|(_, arg)| arg.clone()),
                );
                (new_args, parsed.rcfile)
            })
        }
    };
    let mut env = vec![enabled()];
    if let Some((args, rcfile)) = rcfile_args {
        env.push(("TESSERA_BASH_INJECT".into(), "rcfile".into()));
        if let Some(rcfile) = rcfile {
            env.push(("TESSERA_BASH_RCFILE".into(), rcfile));
        }
        return Injection {
            env,
            args: Some(args),
        };
    }
    // Login (or --posix/--norc) shell: load from the first prompt. The
    // loader goes last so it runs after the user's own entries (an
    // exported PROMPT_COMMAND the shell inherits). The script stops
    // exporting PROMPT_COMMAND once loaded.
    let mut prompt_command = user_env("PROMPT_COMMAND").unwrap_or_default();
    if !prompt_command.is_empty() {
        prompt_command.push("\n");
    }
    prompt_command.push(BASH_LOADER);
    env.extend([
        ("TESSERA_BASH_INJECT".into(), "prompt-command".into()),
        ("TESSERA_BASH_SCRIPT".into(), script.into_os_string()),
        ("PROMPT_COMMAND".into(), prompt_command),
    ]);
    Injection { env, args: None }
}

/// The XDG Base Directory default for an unset `XDG_DATA_DIRS`.
const XDG_DATA_DIRS_DEFAULT: &str = "/usr/local/share:/usr/share";

fn fish(scripts_dir: &Path, user_env: &dyn Fn(&str) -> Option<OsString>) -> Injection {
    let dir = scripts_dir.as_os_str();
    if dir.as_encoded_bytes().contains(&b':') {
        return Injection::default();
    }
    let mut env = vec![
        enabled(),
        ("TESSERA_FISH_XDG_DATA_DIR".into(), dir.to_os_string()),
    ];
    let mut value = dir.to_os_string();
    match user_env("XDG_DATA_DIRS") {
        Some(dirs) if !dirs.is_empty() => {
            for entry in dirs
                .as_encoded_bytes()
                .split(|&b| b == b':')
                .filter(|entry| !entry.is_empty())
            {
                value.push(":");
                value.push(OsStr::from_bytes(entry));
            }
        }
        // fish (like every XDG reader) uses the default dirs when the
        // variable is unset or empty; keep them visible to fish.
        _ => {
            value.push(":");
            value.push(XDG_DATA_DIRS_DEFAULT);
            env.push(("TESSERA_FISH_XDG_WAS_UNSET".into(), "1".into()));
        }
    }
    env.push(("XDG_DATA_DIRS".into(), value));
    Injection { env, args: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn zsh_startup_file_rules() {
        for (list, reads) in [
            (&[][..], true),
            (&["-i"][..], true),
            (&["-l"][..], true),
            (&["-il"][..], true),
            (&["-o", "vi"][..], true),
            (&["-s", "a"][..], true),
            (&["-sf"][..], false),
            (&["-s", "-f"][..], false),
            (&["-is", "--no-rcs"][..], false),
            (&["-is", "-o", "norcs"][..], false),
            (&["--interactive", "script.zsh"][..], true),
            (&["--shinstdin", "a"][..], true),
            (&["-i", "script.zsh"][..], true),
            (&["-c", "true"][..], false),
            (&["-ic", "true"][..], false),
            (&["-f"][..], false),
            (&["-if"][..], false),
            (&["+f"][..], true),
            (&["--no-rcs"][..], false),
            (&["--norcs"][..], false),
            (&["--NO_RCS"][..], false),
            (&["--rcs"][..], true),
            (&["-o", "norcs"][..], false),
            (&["-o", "NO_RCS"][..], false),
            (&["-onorcs"][..], false),
            (&["+o", "rcs"][..], false),
            (&["+o", "norcs"][..], true),
            (&["-o", "rcs"][..], true),
            (&["script.zsh"][..], false),
            (&["--", "script.zsh"][..], false),
            (&["-x", "script.zsh"][..], false),
        ] {
            assert_eq!(zsh_reads_startup_files(&args(list)), reads, "{list:?}");
        }
    }

    #[test]
    fn bash_short_option_clusters() {
        assert!(parse_bash_args(&args(&["-ic", "true"])).skip);
        assert!(parse_bash_args(&args(&["script.sh"])).skip);
        assert!(!parse_bash_args(&args(&["-i", "script.sh"])).skip);
        assert!(parse_bash_args(&args(&["-il"])).login);
        // `-O extglob` / `-o vi` take a name, not a file operand.
        assert!(!parse_bash_args(&args(&["-O", "extglob"])).skip);
        assert!(!parse_bash_args(&args(&["-o", "vi"])).skip);
        assert!(!parse_bash_args(&args(&["+O", "extglob"])).skip);
        // ...unless the name is in the cluster.
        assert!(parse_bash_args(&args(&["-Oextglob", "x.sh"])).skip);
        // `-s`: what follows are positional parameters.
        assert!(!parse_bash_args(&args(&["-s", "a", "b"])).skip);
        // `--`: the next argument is the script.
        assert!(parse_bash_args(&args(&["--", "x.sh"])).skip);
    }

    #[test]
    fn bash_long_options_only_before_short_ones() {
        let parsed = parse_bash_args(&args(&["--rcfile", "/r", "-i"]));
        assert_eq!(parsed.rcfile, Some("/r".into()));
        assert_eq!(parsed.remove, vec![0, 1]);
        // After a short option bash takes `--login` for a file operand.
        assert!(parse_bash_args(&args(&["-x", "--login"])).skip);
    }
}
