//! The scripts in real shells: each case materialises them, applies the
//! injection to a shell spawned through `tessera-pty` (with `HOME` and the
//! XDG directories in a temporary directory, so no real startup or history
//! file is read or written), types `cd /tmp`, `true`, `false`, `exit` one
//! prompt at a time, and checks the OSC payloads the shell wrote against the
//! spec fixture's live rows (`tests/fixtures/osc_sequences.txt`, rows LZ, LB
//! and LX): the OSC 133 marks as decoded by `parse_osc133`, exactly one
//! handshake before the first prompt, and the OSC 7 paths in order.
//!
//! Cases for a shell that is not installed (fish, usually) are skipped with
//! a message.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tessera_grid::Mark;
use tessera_pty::{NativePty, Pty, PtyEvent, PtyEvents, SpawnOptions};
use tessera_shell_integration::osc7::MAX_OSC7_LEN;
use tessera_shell_integration::scripts::materialise;
use tessera_shell_integration::{
    Injection, Launch, Shell, parse_osc7, parse_osc133, parse_osc1337,
};

const FIXTURE: &str = include_str!("fixtures/osc_sequences.txt");

/// How long one step (a prompt, the exit) may take on a loaded machine.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

const INPUT: [&str; 4] = ["cd /tmp\r", "true\r", "false\r", "exit\r"];

/// The payloads of a fixture row: the fields after the id and the scenario,
/// with `[cmd] ` annotations stripped and `(...)` notes dropped.
fn fixture_row(id: &str) -> Vec<String> {
    let line = FIXTURE
        .lines()
        .find(|line| line.split('|').next().map(str::trim) == Some(id))
        .unwrap_or_else(|| panic!("fixture row {id} missing"));
    line.split('|')
        .skip(2)
        .map(str::trim)
        .filter(|field| !field.starts_with('('))
        .map(|field| match field.strip_prefix('[') {
            Some(rest) => rest.split_once("] ").map_or(rest, |(_, payload)| payload),
            None => field,
        })
        .map(str::to_owned)
        .collect()
}

/// The OSC payloads (between `ESC ]` and BEL or `ESC \`) in `output`.
fn osc_payloads(output: &[u8]) -> Vec<Vec<u8>> {
    let mut payloads = Vec::new();
    let mut rest = output;
    while let Some(start) = rest.windows(2).position(|w| w == b"\x1b]") {
        rest = &rest[start + 2..];
        let end = rest.iter().enumerate().find_map(|(i, &b)| match b {
            0x07 => Some((i, 1)),
            0x1b if rest.get(i + 1) == Some(&b'\\') => Some((i, 2)),
            _ => None,
        });
        let Some((end, terminator)) = end else { break };
        payloads.push(rest[..end].to_vec());
        rest = &rest[end + terminator..];
    }
    payloads
}

fn marks<'a>(payloads: impl IntoIterator<Item = &'a [u8]>) -> Vec<Mark> {
    payloads
        .into_iter()
        .filter_map(|payload| payload.strip_prefix(b"133;"))
        .map(|payload| parse_osc133(payload).expect("an OSC 133 the parser knows"))
        .collect()
}

/// This machine's host name as the scripts report it.
fn host_name() -> String {
    let output = std::process::Command::new("/bin/hostname")
        .output()
        .expect("run hostname");
    let name: String = String::from_utf8_lossy(&output.stdout)
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    if name.is_empty() {
        "localhost".to_owned()
    } else {
        name
    }
}

/// The first of `candidates` that exists.
fn find_shell(candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

struct Case<'a> {
    shell: Shell,
    program: PathBuf,
    /// `None`: arguments the shell gets as a login shell would (see
    /// `Launch::Login`); the case still runs it with `-l`.
    args: Option<&'a [&'a str]>,
    /// Startup files written into the temporary home.
    files: &'a [(&'a str, &'a str)],
    /// Extra environment for the shell.
    env: &'a [(&'a str, &'a str)],
}

/// What a case observed.
struct Observed {
    /// Everything the shell wrote.
    output: Vec<u8>,
    payloads: Vec<Vec<u8>>,
    start_cwd: String,
    /// How many startup files print [`RC_MARK`].
    rc_files: usize,
}

/// A startup file prints this (as OSC 9) so the case can tell it ran.
const RC_MARK: &str = r"printf '\033]9;rc\007'";

/// Queries fish 4 sends and waits for (DA1 before the first prompt, the
/// cursor position at exit), with the answers a terminal gives.
const ANSWERS: [(&[u8], &[u8]); 3] = [
    (b"\x1b[c", b"\x1b[?62;22c"),
    (b"\x1b[0c", b"\x1b[?62;22c"),
    (b"\x1b[6n", b"\x1b[1;1R"),
];

/// The shell's output so far, answering the terminal queries in it.
struct Output {
    bytes: Vec<u8>,
    answered: [usize; ANSWERS.len()],
}

impl Output {
    fn push(&mut self, pty: &mut NativePty, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        for ((query, answer), answered) in ANSWERS.iter().zip(&mut self.answered) {
            let asked = count(&self.bytes, query);
            while *answered < asked {
                // The shell may be gone already (the answer to `exit`).
                let _ = pty.write(answer);
                *answered += 1;
            }
        }
    }
}

/// Reads `pty` output until `done` holds for it. Returns whether it did
/// before the timeout or the child's exit.
fn pump(
    pty: &mut NativePty,
    events: &PtyEvents,
    out: &mut Output,
    done: impl Fn(&[u8]) -> bool,
) -> bool {
    pump_for(STEP_TIMEOUT, pty, events, out, done)
}

/// [`pump`] with its own timeout.
fn pump_for(
    timeout: Duration,
    pty: &mut NativePty,
    events: &PtyEvents,
    out: &mut Output,
    done: impl Fn(&[u8]) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while !done(&out.bytes) {
        // Checked here too: output that keeps arriving must not keep the
        // wait going past the deadline.
        if Instant::now() >= deadline {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(PtyEvent::Output(bytes)) => out.push(pty, &bytes),
            Ok(PtyEvent::Exited(_)) | Err(_) => return done(&out.bytes),
        }
    }
    true
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// A shell ready to start: its options, and the directories they use.
struct Prepared {
    _temp: tempfile::TempDir,
    home: String,
    options: SpawnOptions,
    /// Whether the injection changed anything.
    injected: bool,
}

/// How a case loads the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Through the injection (`Injection::for_bash`, `for_shell`).
    Injected,
    /// By hand: the shell starts with the case's arguments and nothing
    /// injected, and a startup file sources the script (see [`BY_HAND`]).
    ByHand,
}

/// What a `.bashrc` adds to load the bash script by hand, as the script's
/// header tells users to.
const BY_HAND: &str =
    "export TESSERA_SHELL_INTEGRATION=enabled\n. \"$TESSERA_TEST_SCRIPTS/bash/tessera.bash\"\n";

/// [`prepare_via`] through the injection.
fn prepare(case: &Case<'_>) -> Prepared {
    prepare_via(case, Route::Injected)
}

/// Materialises the scripts, writes the case's startup files and builds the
/// spawn options: the case's program with the injection applied (on
/// [`Route::Injected`]), then
/// `case.env` (which wins over the injection; `$SCRIPTS` in a value is the
/// scripts directory). `$TESSERA_TEST_SCRIPTS` is
/// the scripts directory, for startup files that source a script.
fn prepare_via(case: &Case<'_>, route: Route) -> Prepared {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    // Like the real location, with a space in it.
    let scripts = materialise(&root.join("Application Support/Tessera/shell-integration")).unwrap();
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    for (name, contents) in case.files {
        let path = home.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    let home_str = home.to_str().unwrap().to_owned();

    // The user's environment as Tessera would see it: only HOME. A real
    // ZDOTDIR of the person running the tests must not leak in.
    // `user:<NAME>` entries of `case.env` are in the user's environment too
    // (so the injection sees them).
    let user_home = home_str.clone();
    let user_vars: Vec<(String, String)> = case
        .env
        .iter()
        .filter_map(|(key, value)| {
            Some((key.strip_prefix("user:")?.to_owned(), (*value).to_owned()))
        })
        .collect();
    let user_env = {
        let user_vars = user_vars.clone();
        move |key: &str| {
            if key == "HOME" {
                return Some(OsString::from(&user_home));
            }
            user_vars
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| OsString::from(value))
        }
    };
    let user_args: Vec<OsString> = case
        .args
        .unwrap_or(&["-l"])
        .iter()
        .map(OsString::from)
        .collect();
    let launch = match case.args {
        Some(_) => Launch::Args(&user_args),
        None => Launch::Login,
    };
    let injection = match route {
        Route::ByHand => Injection::default(),
        Route::Injected => Injection::for_shell(case.shell, &scripts, launch, user_env),
    };
    let injected = !injection.is_empty();

    let mut options = SpawnOptions::command(&case.program, "live")
        .args(injection.args.clone().unwrap_or(user_args))
        .cwd(&home);
    for (key, value) in [
        ("HOME", home_str.as_str()),
        ("PWD", home_str.as_str()),
        ("XDG_CONFIG_HOME", &format!("{home_str}/.config")),
        ("XDG_DATA_HOME", &format!("{home_str}/.local/share")),
        ("XDG_CACHE_HOME", &format!("{home_str}/.cache")),
        ("XDG_STATE_HOME", &format!("{home_str}/.local/state")),
        ("HISTFILE", &format!("{home_str}/.history")),
        ("TESSERA_TEST_SCRIPTS", scripts.to_str().unwrap()),
        // Inherited values that would change what the shells run.
        ("PROMPT_COMMAND", ""),
        ("ENV", ""),
        ("BASH_ENV", ""),
        ("fish_features", ""),
        ("BASH_SILENCE_DEPRECATION_WARNING", "1"),
    ] {
        options = options.env(key, value);
    }
    for (key, value) in &user_vars {
        options = options.env(key, value);
    }
    for (key, value) in &injection.env {
        options = options.env(key, value);
    }
    // `$SCRIPTS` in a case's value is the scripts directory.
    for (key, value) in case.env.iter().filter(|(key, _)| !key.starts_with("user:")) {
        options = options.env(key, value.replace("$SCRIPTS", scripts.to_str().unwrap()));
    }
    Prepared {
        _temp: temp,
        home: home_str,
        options,
        injected,
    }
}

/// Reads the shell's output until it exits; panics after [`STEP_TIMEOUT`].
fn wait_exit(pty: &mut NativePty, events: &PtyEvents, out: &mut Output) {
    let started = Instant::now();
    loop {
        assert!(
            started.elapsed() < STEP_TIMEOUT,
            "shell did not exit within {STEP_TIMEOUT:?}; output: {:?}",
            String::from_utf8_lossy(&out.bytes)
        );
        match events.recv_timeout(STEP_TIMEOUT.saturating_sub(started.elapsed())) {
            Ok(PtyEvent::Output(bytes)) => out.push(pty, &bytes),
            // `exit` after `false` exits with 1.
            Ok(PtyEvent::Exited(_)) => break,
            Err(err) => panic!(
                "shell did not exit: {err:?}; output: {:?}",
                String::from_utf8_lossy(&out.bytes)
            ),
        }
    }
    // A shell that waits on an unanswered query before exiting would be a
    // harness bug (fish waits 10 s for a cursor position report).
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "exit took {:?}",
        started.elapsed()
    );
}

/// When to type the next input.
#[derive(Debug, Clone, Copy)]
enum Wait {
    /// Once this many prompts (OSC 133 A) have been drawn.
    Prompts(usize),
}

/// Whether the process `pid` has a child named `name`.
fn has_child(pid: u32, name: &str) -> bool {
    std::process::Command::new("/usr/bin/pgrep")
        .args(["-P", &pid.to_string(), "-x", name])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Pumps output until the shell `pid` has a child named `name` (so a
/// signal reaches it, not the shell between `C` and the fork). False after
/// [`STEP_TIMEOUT`].
fn pump_until_child(
    pid: u32,
    name: &str,
    pty: &mut NativePty,
    events: &PtyEvents,
    out: &mut Output,
) -> bool {
    let deadline = Instant::now() + STEP_TIMEOUT;
    while !has_child(pid, name) && Instant::now() < deadline {
        while let Ok(PtyEvent::Output(bytes)) = events.recv_timeout(Duration::from_millis(20)) {
            out.push(pty, &bytes);
        }
    }
    has_child(pid, name)
}

/// Runs the case, typing [`INPUT`] one prompt (one OSC 133 A) at a time.
fn run(case: &Case<'_>) -> Observed {
    let steps: Vec<(Wait, &str)> = INPUT
        .iter()
        .enumerate()
        .map(|(i, line)| (Wait::Prompts(i + 1), *line))
        .collect();
    run_steps(case, &steps)
}

/// Runs the case through the injection, typing each input once its wait
/// is over.
fn run_steps(case: &Case<'_>, steps: &[(Wait, &str)]) -> Observed {
    run_steps_via(case, steps, Route::Injected)
}

/// [`run_steps`] on `route`.
fn run_steps_via(case: &Case<'_>, steps: &[(Wait, &str)], route: Route) -> Observed {
    let prepared = prepare_via(case, route);
    assert!(
        route == Route::ByHand
            || prepared.injected
            || prepared
                .options
                .env
                .iter()
                .any(|(key, _)| key == "TESSERA_SHELL_INTEGRATION"),
        "nothing injected"
    );
    let (mut pty, events) = NativePty::spawn(&prepared.options).expect("spawn the shell");
    let mut out = Output {
        bytes: Vec::new(),
        answered: [0; ANSWERS.len()],
    };
    for &(wait, input) in steps {
        let Wait::Prompts(n) = wait;
        let ready = pump(&mut pty, &events, &mut out, |out| {
            count(out, b"\x1b]133;A") >= n
        });
        assert!(
            ready,
            "no {wait:?} within {STEP_TIMEOUT:?}; output so far: {:?}",
            String::from_utf8_lossy(&out.bytes)
        );
        pty.write(input.as_bytes()).expect("type a command");
    }
    wait_exit(&mut pty, &events, &mut out);
    Observed {
        payloads: osc_payloads(&out.bytes),
        output: out.bytes,
        start_cwd: prepared.home,
        rc_files: case
            .files
            .iter()
            .filter(|(_, contents)| contents.contains(RC_MARK))
            .count(),
    }
}

/// Runs the case with `input` typed at once (no prompt marks to wait for)
/// and returns everything the shell wrote.
fn run_raw(case: &Case<'_>, input: &str) -> Vec<u8> {
    let prepared = prepare(case);
    let (mut pty, events) = NativePty::spawn(&prepared.options).expect("spawn the shell");
    let mut out = Output {
        bytes: Vec::new(),
        answered: [0; ANSWERS.len()],
    };
    pty.write(input.as_bytes()).expect("type the input");
    wait_exit(&mut pty, &events, &mut out);
    out.bytes
}

/// Checks `observed` against fixture row `row` for `shell`; `drop_first_b`
/// expects the first prompt without its B.
///
/// fish differs from the fixture's zsh and bash rows in two observed ways
/// (fish 4.9.3): it runs `fish_postexec` (and closes its own marks) for
/// `exit` too, so one D follows the last C, and from 4.3 on it reports the
/// directory itself at every prompt, so repeated OSC 7 reports are folded.
fn check(observed: &Observed, row: &str, shell: Shell, drop_first_b: bool) {
    let fish = shell == Shell::Fish;
    let shell = shell.name();
    let expected = fixture_row(row);
    let payloads: Vec<&[u8]> = observed.payloads.iter().map(Vec::as_slice).collect();
    let describe = || {
        payloads
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect::<Vec<_>>()
            .join(" | ")
    };

    // (a) the OSC 133 marks, in order.
    let mut want = marks(expected.iter().map(String::as_bytes));
    if drop_first_b {
        let first_b = want.iter().position(|m| *m == Mark::CommandStart).unwrap();
        want.remove(first_b);
    }
    let mut got = marks(payloads.iter().copied());
    if fish {
        assert!(
            matches!(got.last(), Some(Mark::CommandEnd { .. })),
            "fish closes `exit` with a D: {}",
            describe()
        );
        got.pop();
    }
    assert_eq!(got, want, "OSC 133 marks; payloads: {}", describe());
    // Our marks carry no parameters: no `cmdline=` (ADR 0021). fish's own
    // (fish >= 4.0 with `mark-prompt` on) do, and are fish's.
    if !(fish && payloads.iter().any(|p| p.starts_with(b"133;A;"))) {
        for payload in payloads.iter().filter_map(|p| p.strip_prefix(b"133;")) {
            let bare = matches!(payload, b"A" | b"B" | b"C" | b"D")
                || payload
                    .strip_prefix(b"D;")
                    .is_some_and(|code| !code.is_empty() && code.iter().all(u8::is_ascii_digit));
            assert!(bare, "OSC 133 with parameters: {}", describe());
        }
    }

    // (b) one handshake, before the first prompt mark.
    // The row's own, with this shell's name (fish uses the zsh row).
    let handshake = expected
        .iter()
        .find_map(|p| p.strip_prefix("1337;"))
        .and_then(|p| p.split_once(";shell="))
        .map_or_else(
            || panic!("fixture row {row} has no handshake"),
            |(version, _)| format!("1337;{version};shell={shell}"),
        );
    let handshakes: Vec<usize> = payloads
        .iter()
        .enumerate()
        .filter(|(_, p)| p.starts_with(b"1337;"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(handshakes.len(), 1, "handshakes: {}", describe());
    assert_eq!(payloads[handshakes[0]], handshake.as_bytes());
    let parsed = parse_osc1337(&payloads[handshakes[0]][5..]).unwrap();
    assert_eq!((parsed.version, parsed.shell.as_str()), (1, shell));
    let first_a = payloads
        .iter()
        .position(|p| p.starts_with(b"133;A"))
        .unwrap();
    assert!(
        handshakes[0] < first_a,
        "handshake after the first A: {}",
        describe()
    );

    // The user's startup files ran, once each.
    let ran = payloads.iter().filter(|p| **p == b"9;rc").count();
    assert_eq!(ran, observed.rc_files, "startup files run: {}", describe());

    // (c) the OSC 7 reports: the start directory, then /tmp, from this host.
    let host = host_name();
    let want: Vec<String> = expected
        .iter()
        .filter_map(|p| p.strip_prefix("7;file://HOST"))
        .map(|path| path.replace("/START_CWD", &observed.start_cwd))
        .collect();
    let mut got: Vec<String> = Vec::new();
    for payload in payloads.iter().filter_map(|p| p.strip_prefix(b"7;")) {
        let cwd = parse_osc7(payload)
            .unwrap_or_else(|| panic!("bad OSC 7 {:?}", String::from_utf8_lossy(payload)));
        assert_eq!(cwd.host, host, "OSC 7 host");
        // zsh and fish report at every prompt: repeats are folded.
        if got.last() != Some(&cwd.path) {
            got.push(cwd.path);
        }
    }
    assert_eq!(got, want, "OSC 7 paths; payloads: {}", describe());
}

const ZSH: &str = "/bin/zsh";
const BASH: &str = "/bin/bash";
/// Homebrew's bash (5.x), run as well when installed.
const BASH5: &[&str] = &["/opt/homebrew/bin/bash", "/usr/local/bin/bash"];

/// The bashes to run: the macOS `/bin/bash` (3.2) and, when installed,
/// Homebrew's (5.x).
fn bashes() -> Vec<PathBuf> {
    let mut found = vec![PathBuf::from(BASH)];
    match find_shell(BASH5) {
        Some(bash) => found.push(bash),
        None => eprintln!("bash 5 part skipped: not installed (looked in {BASH5:?})"),
    }
    found
}
/// B1-B4 hold on both injected routes: each bash with the `--rcfile` route
/// (`-i`, `.bashrc`) and the login route's loader (`.bash_profile`).
fn bash_routes() -> Vec<(PathBuf, Option<&'static [&'static str]>, &'static str)> {
    bashes()
        .into_iter()
        .flat_map(|bash| {
            [
                (bash.clone(), Some(&["-i"][..]), ".bashrc"),
                (bash, None, ".bash_profile"),
            ]
        })
        .collect()
}

const FISH: &[&str] = &[
    "/opt/homebrew/bin/fish",
    "/usr/local/bin/fish",
    "/usr/bin/fish",
];

#[test]
fn zsh_with_an_empty_zshrc() {
    // kitty's rule: no startup file at all means no injection (fixture Z5).
    let observed = run(&Case {
        shell: Shell::Zsh,
        program: ZSH.into(),
        args: Some(&["-i"]),
        files: &[(".zshrc", "")],
        env: &[],
    });
    check(&observed, "LZ", Shell::Zsh, false);
}

#[test]
fn zsh_with_a_custom_prompt_and_precmd() {
    let observed = run(&Case {
        shell: Shell::Zsh,
        program: ZSH.into(),
        args: Some(&["-i"]),
        files: &[(
            ".zshrc",
            "PS1='custom%# '\nprecmd() { print -n '' }\nprintf '\\033]9;rc\\007'\n",
        )],
        env: &[],
    });
    check(&observed, "LZ", Shell::Zsh, false);
}

#[test]
fn zsh_login_shell_reads_its_profile() {
    let observed = run(&Case {
        shell: Shell::Zsh,
        program: ZSH.into(),
        args: None,
        files: &[(".zprofile", "PS1='login%# '\nprintf '\\033]9;rc\\007'\n")],
        env: &[],
    });
    check(&observed, "LZ", Shell::Zsh, false);
}

#[test]
fn zsh_theme_resetting_the_prompt_in_a_precmd_hook() {
    // The hook runs after ours at the first prompt, so ours prints A itself
    // (no B there) and moves to the end; from then on it marks PS1.
    let observed = run(&Case {
        shell: Shell::Zsh,
        program: ZSH.into(),
        args: Some(&["-i"]),
        files: &[(
            ".zshrc",
            "theme() { PS1='theme%# ' }\nprecmd_functions+=(theme)\nprintf '\\033]9;rc\\007'\n",
        )],
        env: &[],
    });
    check(&observed, "LZ", Shell::Zsh, true);
}

#[test]
fn bash_with_no_startup_files() {
    for bash in bashes() {
        eprintln!("bash: {}", bash.display());
        let observed = run(&Case {
            shell: Shell::Bash,
            program: bash.clone(),
            args: Some(&["-i"]),
            files: &[],
            env: &[],
        });
        check(&observed, "LB", Shell::Bash, false);
    }
}

#[test]
fn bash_with_a_custom_prompt_and_prompt_command() {
    for bash in bashes() {
        eprintln!("bash: {}", bash.display());
        let observed = run(&Case {
            shell: Shell::Bash,
            program: bash.clone(),
            args: Some(&["-i"]),
            files: &[(
                ".bashrc",
                "PS1='custom\\$ '\nPROMPT_COMMAND='PS1=\"custom\\$ \"'\nprintf '\\033]9;rc\\007'\n",
            )],
            env: &[],
        });
        check(&observed, "LB", Shell::Bash, false);
    }
}

#[test]
fn bash_login_shell_loads_from_prompt_command() {
    for bash in bashes() {
        eprintln!("bash: {}", bash.display());
        let observed = run(&Case {
            shell: Shell::Bash,
            program: bash.clone(),
            args: None,
            files: &[(
                ".bash_profile",
                "PS1='login\\$ '\nPROMPT_COMMAND=\"history -a; $PROMPT_COMMAND\"\nprintf '\\033]9;rc\\007'\n",
            )],
            env: &[],
        });
        check(&observed, "LB", Shell::Bash, false);
    }
}

fn fish_case(env: &[(&str, &str)]) {
    let Some(program) = find_shell(FISH) else {
        eprintln!("skipped: fish is not installed (looked in {FISH:?})");
        return;
    };
    let observed = run(&Case {
        shell: Shell::Fish,
        program,
        args: Some(&["-i"]),
        files: &[],
        env,
    });
    // fish has no fixture row of its own (it was not installed when the
    // fixture was made); LX(a) gives one OSC 133 sequence for every shell.
    check(&observed, "LZ", Shell::Fish, false);
}

#[test]
fn fish_with_its_own_marks() {
    // fish >= 4.0 marks prompts itself; the script then only shakes hands.
    fish_case(&[]);
}

#[test]
fn fish_with_the_scripts_marks() {
    // With fish's marks off (as in fish < 4.0) the script marks prompts.
    fish_case(&[("fish_features", "no-mark-prompt")]);
}

#[test]
fn fixture_rows_parse() {
    let lz = fixture_row("LZ");
    assert_eq!(lz[0], "1337;ShellIntegrationVersion=1;shell=zsh");
    assert_eq!(lz.last().map(String::as_str), Some("133;C"));
    assert!(fixture_row("LB").contains(&"7;file://HOST/tmp".to_owned()));
}

/// Whether `output` has any OSC 133, 1337 or 7 sequence.
fn has_integration_oscs(output: &[u8]) -> bool {
    osc_payloads(output).iter().any(|payload| {
        payload.starts_with(b"133;") || payload.starts_with(b"1337;") || payload.starts_with(b"7;")
    })
}

/// `$((40+2))` typed shows as itself; `42` on a line of its own is the
/// shell's output, so the commands did run.
fn ran(output: &[u8]) -> bool {
    output.windows(4).any(|w| w == b"42\r\n")
}

#[test]
fn zsh_and_bash_do_nothing_when_disabled() {
    // `0` from the spawn, empty from the spawn, `0` from a startup file.
    let mut cases = vec![
        (
            Shell::Zsh,
            PathBuf::from(ZSH),
            &[(".zshrc", "")][..],
            &[("TESSERA_SHELL_INTEGRATION", "0")][..],
        ),
        (
            Shell::Zsh,
            PathBuf::from(ZSH),
            &[(".zshrc", "")][..],
            &[("TESSERA_SHELL_INTEGRATION", "")][..],
        ),
        (
            Shell::Zsh,
            PathBuf::from(ZSH),
            &[(".zshrc", "export TESSERA_SHELL_INTEGRATION=0\n")][..],
            &[][..],
        ),
    ];
    for bash in bashes() {
        cases.push((
            Shell::Bash,
            bash.clone(),
            &[][..],
            &[("TESSERA_SHELL_INTEGRATION", "0")][..],
        ));
        cases.push((
            Shell::Bash,
            bash.clone(),
            &[][..],
            &[("TESSERA_SHELL_INTEGRATION", "")][..],
        ));
        cases.push((
            Shell::Bash,
            bash,
            &[(".bashrc", "export TESSERA_SHELL_INTEGRATION=0\n")][..],
            &[][..],
        ));
    }
    for (shell, program, files, env) in cases {
        let output = run_raw(
            &Case {
                shell,
                program: program.clone(),
                args: Some(&["-i"]),
                files,
                env,
            },
            "echo $((40+2))\rexit\r",
        );
        assert!(
            ran(&output),
            "{shell:?} {program:?} {files:?} {env:?}: {:?}",
            String::from_utf8_lossy(&output)
        );
        assert!(
            !has_integration_oscs(&output),
            "{shell:?} {files:?} {env:?} is integrated: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}

#[test]
fn fish_does_nothing_when_disabled() {
    let Some(program) = find_shell(FISH) else {
        eprintln!("skipped: fish is not installed (looked in {FISH:?})");
        return;
    };
    let config_off = "set -gx TESSERA_SHELL_INTEGRATION 0\n";
    for (files, env) in [
        (&[][..], &[("TESSERA_SHELL_INTEGRATION", "0")][..]),
        (&[][..], &[("TESSERA_SHELL_INTEGRATION", "")][..]),
        (&[(".config/fish/config.fish", config_off)][..], &[][..]),
    ] {
        // fish's own marks off, so any OSC 133 would be ours.
        let mut env = env.to_vec();
        env.push(("fish_features", "no-mark-prompt"));
        let output = run_raw(
            &Case {
                shell: Shell::Fish,
                program: program.clone(),
                args: Some(&["-i"]),
                files,
                env: &env,
            },
            "echo (math 40+2)\rexit\r",
        );
        assert!(
            ran(&output),
            "{files:?} {env:?}: {:?}",
            String::from_utf8_lossy(&output)
        );
        let ours = osc_payloads(&output)
            .into_iter()
            .filter(|p| p.starts_with(b"133;") || p.starts_with(b"1337;"))
            .count();
        assert_eq!(
            ours,
            0,
            "{files:?} {env:?}: {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}

#[test]
fn scripts_do_nothing_in_non_interactive_shells() {
    // zsh reads our .zshenv for every shell: it must only restore ZDOTDIR.
    // Tessera does not inject into `zsh -c`, so give it the injection's
    // environment itself (as a program run with it would).
    let output = run_raw(
        &Case {
            shell: Shell::Zsh,
            program: ZSH.into(),
            args: Some(&[
                "-c",
                r#"echo $((40+2)); echo "zdotdir=${ZDOTDIR-unset} hooks=${#precmd_functions}""#,
            ]),
            files: &[(".zshrc", "")],
            env: &[
                ("ZDOTDIR", "$SCRIPTS/zsh"),
                ("TESSERA_SHELL_INTEGRATION", "enabled"),
            ],
        },
        "",
    );
    let text = String::from_utf8_lossy(&output);
    assert!(
        ran(&output) && text.contains("zdotdir=unset hooks=0\r\n"),
        "{text:?}"
    );
    assert!(!has_integration_oscs(&output), "{text:?}");

    // bash: sourcing the script in a non-interactive shell installs nothing.
    for bash in bashes() {
        let output = run_raw(
            &Case {
                shell: Shell::Bash,
                program: bash.clone(),
                args: Some(&[
                    "-c",
                    r#". "$TESSERA_TEST_SCRIPTS/bash/tessera.bash"; echo $((40+2)); echo "trap=$(trap -p DEBUG)""#,
                ]),
                files: &[],
                env: &[("TESSERA_SHELL_INTEGRATION", "enabled")],
            },
            "",
        );
        let text = String::from_utf8_lossy(&output);
        assert!(ran(&output) && text.contains("trap=\r\n"), "{text:?}");
        assert!(!has_integration_oscs(&output), "{text:?}");
    }

    // fish loads the vendor file in every shell: it must only restore
    // XDG_DATA_DIRS (unset before, so erased).
    if let Some(program) = find_shell(FISH) {
        let output = run_raw(
            &Case {
                shell: Shell::Fish,
                program,
                args: Some(&[
                    "-c",
                    r#"echo (math 40+2); functions -q __tessera_si_schedule; echo "xdg=$XDG_DATA_DIRS hook=$status""#,
                ]),
                files: &[],
                env: &[],
            },
            "",
        );
        let text = String::from_utf8_lossy(&output);
        assert!(ran(&output) && text.contains("xdg= hook=1\r\n"), "{text:?}");
        assert!(!has_integration_oscs(&output), "{text:?}");
    } else {
        eprintln!("fish part skipped: fish is not installed (looked in {FISH:?})");
    }
}

#[test]
fn zsh_loading_the_integration_again_installs_one_hook_set() {
    let zshrc = format!(
        "{RC_MARK}\nexport TESSERA_SHELL_INTEGRATION=enabled\n\
         autoload -Uz -- \"$TESSERA_TEST_SCRIPTS/zsh/tessera-integration\"\n\
         tessera-integration\ntessera-integration\nunfunction tessera-integration\n"
    );
    let observed = run(&Case {
        shell: Shell::Zsh,
        program: ZSH.into(),
        args: Some(&["-i"]),
        files: &[(".zshrc", &zshrc)],
        env: &[],
    });
    check(&observed, "LZ", Shell::Zsh, false);
}

#[test]
fn bash_sourcing_the_script_again_installs_one_hook_set() {
    for bash in bashes() {
        eprintln!("bash: {}", bash.display());
        // Exported again before each: the script unsets it once read.
        let bashrc = format!(
            "{RC_MARK}\nexport TESSERA_SHELL_INTEGRATION=enabled\n\
         . \"$TESSERA_TEST_SCRIPTS/bash/tessera.bash\"\n\
         export TESSERA_SHELL_INTEGRATION=enabled\n\
         . \"$TESSERA_TEST_SCRIPTS/bash/tessera.bash\"\n"
        );
        let observed = run(&Case {
            shell: Shell::Bash,
            program: bash.clone(),
            args: Some(&["-i"]),
            files: &[(".bashrc", &bashrc)],
            env: &[],
        });
        check(&observed, "LB", Shell::Bash, false);
    }
}

#[test]
fn fish_sourcing_the_script_again_installs_one_hook_set() {
    let Some(program) = find_shell(FISH) else {
        eprintln!("skipped: fish is not installed (looked in {FISH:?})");
        return;
    };
    let config = "set -gx TESSERA_SHELL_INTEGRATION enabled\n\
                  source $TESSERA_TEST_SCRIPTS/fish/vendor_conf.d/tessera-shell-integration.fish\n\
                  set -gx TESSERA_SHELL_INTEGRATION enabled\n\
                  source $TESSERA_TEST_SCRIPTS/fish/vendor_conf.d/tessera-shell-integration.fish\n";
    let observed = run(&Case {
        shell: Shell::Fish,
        program,
        args: Some(&["-i"]),
        files: &[(".config/fish/config.fish", config)],
        env: &[("fish_features", "no-mark-prompt")],
    });
    check(&observed, "LZ", Shell::Fish, false);
}

/// Whether the process `pid` is asleep (`ps` state `S`, or `I` for a sleep
/// of more than about 20 s): a shell that drew its whole prompt is then
/// blocked reading the terminal.
fn asleep(pid: u32) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|output| matches!(output.stdout.first(), Some(b'S' | b'I')))
}

/// Pumps output until the shell `pid` is asleep with nothing new written
/// in between (so it is blocked reading the terminal: a prompt's last mark
/// can arrive before the shell reads, and zsh 5.9 loses a Ctrl-C that
/// lands there, #615). False after [`STEP_TIMEOUT`].
fn pump_until_reading(pid: u32, pty: &mut NativePty, events: &PtyEvents, out: &mut Output) -> bool {
    let deadline = Instant::now() + STEP_TIMEOUT;
    while Instant::now() < deadline {
        let before = out.bytes.len();
        while let Ok(PtyEvent::Output(bytes)) = events.recv_timeout(Duration::from_millis(10)) {
            out.push(pty, &bytes);
        }
        if asleep(pid) && out.bytes.len() == before {
            return true;
        }
    }
    false
}

/// Fixture rows LY, C1-C3, C5, C6 and S9: an empty line and Ctrl-C at the
/// prompt each close the prompt with a bare D (zsh reports 130 and bash 3.2
/// the previous status there; the bare D hides the difference), and a
/// command interrupted with Ctrl-C ends with D;130.
///
/// Each input goes in once the marks before it are all there (checked at
/// every step) and the shell is reading the terminal: zsh 5.9 checks for
/// an interrupt before its blocking read of the first key, so a Ctrl-C
/// sent between the two is lost (#615).
#[test]
fn empty_lines_and_ctrl_c_close_with_a_bare_d() {
    use Mark::{CommandEnd, CommandStart, OutputStart, PromptStart};
    let bare = CommandEnd { exit_code: None };
    let interrupted = CommandEnd {
        exit_code: Some(130),
    };
    let want = [
        PromptStart,
        CommandStart,
        bare.clone(),
        PromptStart,
        CommandStart,
        bare,
        PromptStart,
        CommandStart,
        OutputStart,
        interrupted,
        PromptStart,
        CommandStart,
        OutputStart,
    ];
    // (marks before the input, wait for this child first, input)
    let steps: [(usize, Option<&str>, &str); 5] = [
        (2, None, "\r"),
        (5, None, "\x03"),
        (8, None, "sleep 20\r"),
        (9, Some("sleep"), "\x03"),
        (12, None, "exit\r"),
    ];
    let mut cases = vec![(Shell::Zsh, PathBuf::from(ZSH), ".zshrc")];
    cases.extend(
        bashes()
            .into_iter()
            .map(|bash| (Shell::Bash, bash, ".bashrc")),
    );
    for (shell, program, rc) in cases {
        let what = format!("{shell:?} {program:?}");
        let prepared = prepare(&Case {
            shell,
            program: program.clone(),
            args: Some(&["-i"]),
            files: &[(rc, "")],
            env: &[],
        });
        let (mut pty, events) = NativePty::spawn(&prepared.options).expect("spawn the shell");
        let pid = pty.child_pid().expect("the shell's pid");
        let mut out = Output {
            bytes: Vec::new(),
            answered: [0; ANSWERS.len()],
        };
        let marks_so_far = |bytes: &[u8]| marks(osc_payloads(bytes).iter().map(Vec::as_slice));
        for (before, child, input) in steps {
            let arrived = pump(&mut pty, &events, &mut out, |bytes| {
                marks_so_far(bytes).len() >= before
            });
            let text = String::from_utf8_lossy(&out.bytes).into_owned();
            assert!(
                arrived,
                "{what}: no {before} marks before {input:?}: {text:?}"
            );
            assert_eq!(
                marks_so_far(&out.bytes),
                want[..before],
                "{what}: before {input:?}: {text:?}"
            );
            let ready = match child {
                Some(name) => pump_until_child(pid, name, &mut pty, &events, &mut out),
                None => pump_until_reading(pid, &mut pty, &events, &mut out),
            };
            assert!(ready, "{what}: not ready for {input:?}: {text:?}");
            pty.write(input.as_bytes()).expect("type an input");
        }
        wait_exit(&mut pty, &events, &mut out);
        let got = marks_so_far(&out.bytes);
        assert_eq!(
            got,
            want,
            "{what}: {:?}",
            String::from_utf8_lossy(&out.bytes)
        );
    }
}

/// #205: a program run from the shell (`ssh` to an integrated host, say)
/// may report another machine's directory; the shell reports its own again
/// at every following prompt, even though `$PWD` did not change, so the
/// remote-host prefix clears at the next local prompt.
#[test]
fn every_prompt_reports_the_directory_after_a_program_reported_another() {
    let steps = [
        (
            Wait::Prompts(1),
            "printf '\\033]7;file://far.example/srv\\007'\r",
        ),
        (Wait::Prompts(2), "\r"),
        (Wait::Prompts(3), "exit\r"),
    ];
    let host = host_name();
    let mut cases = vec![(Shell::Zsh, PathBuf::from(ZSH), ".zshrc")];
    cases.extend(
        bashes()
            .into_iter()
            .map(|bash| (Shell::Bash, bash, ".bashrc")),
    );
    for (shell, program, rc) in cases {
        let observed = run_steps(
            &Case {
                shell,
                program: program.clone(),
                args: Some(&["-i"]),
                files: &[(rc, "")],
                env: &[],
            },
            &steps,
        );
        let describe = || {
            observed
                .payloads
                .iter()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect::<Vec<_>>()
                .join(" | ")
        };
        let far = observed
            .payloads
            .iter()
            .position(|p| p.as_slice() == b"7;file://far.example/srv")
            .unwrap_or_else(|| panic!("{shell:?}: no far report: {}", describe()));
        // After it, each prompt (A) is preceded by a report from this host.
        let mut local = false;
        let mut prompts = 0;
        for payload in &observed.payloads[far + 1..] {
            if let Some(report) = payload.strip_prefix(b"7;") {
                local = parse_osc7(report).is_some_and(|cwd| cwd.host == host);
            } else if payload.starts_with(b"133;A") {
                assert!(
                    local,
                    "{shell:?} {program:?}: prompt {} without a report: {}",
                    prompts + 2,
                    describe()
                );
                local = false;
                prompts += 1;
            }
        }
        assert_eq!(prompts, 2, "{shell:?} {program:?}: {}", describe());
    }
}

/// OSC 7 paths outside `[/._~A-Za-z0-9-]` take the scripts' slow path (a
/// byte loop in zsh and bash, `string escape --style=url` in fish); the
/// payload must be Terminal.app's encoding exactly (fixture P1, E1-E6), and
/// a plain path (the fast path) is sent as is.
#[test]
fn osc7_encodes_like_terminal_app() {
    let odd = "a b/café/🐱#?%+,;=:@~x";
    let mut cases = vec![(Shell::Zsh, PathBuf::from(ZSH), ".zshrc")];
    cases.extend(
        bashes()
            .into_iter()
            .map(|bash| (Shell::Bash, bash, ".bashrc")),
    );
    if let Some(fish) = find_shell(FISH) {
        cases.push((Shell::Fish, fish, ".config/fish/config.fish"));
    } else {
        eprintln!("fish part skipped: fish is not installed (looked in {FISH:?})");
    }
    for (shell, program, rc) in cases {
        let observed = run_steps(
            &Case {
                shell,
                program,
                args: Some(&["-i"]),
                // The directory comes from the environment, so only ASCII
                // is typed.
                files: &[(rc, ""), (&format!("{odd}/keep"), "")],
                env: &[("LANG", "en_US.UTF-8"), ("TESSERA_TEST_DIR", odd)],
            },
            &[
                (Wait::Prompts(1), "cd \"$HOME/$TESSERA_TEST_DIR\"\r"),
                (Wait::Prompts(2), "exit\r"),
            ],
        );
        let host = host_name();
        let dir = format!("{}/{odd}", observed.start_cwd);
        let mut want = vec![
            format!("7;file://{host}{}", observed.start_cwd),
            format!("7;file://{host}{}", encode(&dir)),
        ];
        let mut got: Vec<String> = observed
            .payloads
            .iter()
            .filter(|p| p.starts_with(b"7;"))
            .map(|p| String::from_utf8(p.clone()).unwrap())
            .collect();
        got.dedup();
        if shell == Shell::Fish {
            // fish >= 4.3 sends its own; its encoding is the same rule.
            want.dedup();
        }
        assert_eq!(got, want, "{shell:?}");
        assert_eq!(
            parse_osc7(&want[1].as_bytes()[2..]).map(|cwd| cwd.path),
            Some(dir)
        );
    }
}

/// #358: a directory whose report would pass the 4096-byte payload cap
/// (`MAX_OSC7_LEN`, which drops a longer one whole and so keeps the last
/// report, another host's say, in effect) is reported as its deepest
/// ancestor that fits, never as nothing. Eight levels of 120 `é` encode to
/// about 5.8 KB. fish 4.3 and later send their own report, so only zsh and
/// bash run.
#[test]
fn a_directory_past_the_osc7_cap_reports_its_deepest_ancestor_that_fits() {
    let component = "\u{e9}".repeat(120);
    let mut cases = vec![(Shell::Zsh, PathBuf::from(ZSH), ".zshrc")];
    cases.extend(
        bashes()
            .into_iter()
            .map(|bash| (Shell::Bash, bash, ".bashrc")),
    );
    for (shell, program, rc) in cases {
        let observed = run_steps(
            &Case {
                shell,
                program,
                args: Some(&["-i"]),
                files: &[(rc, "")],
                env: &[("LANG", "en_US.UTF-8"), ("TESSERA_TEST_DIR", &component)],
            },
            &[
                (
                    Wait::Prompts(1),
                    "for i in 1 2 3 4 5 6 7 8; do mkdir \"$TESSERA_TEST_DIR\" && cd \"$TESSERA_TEST_DIR\"; done\r",
                ),
                (Wait::Prompts(2), "exit\r"),
            ],
        );
        let host = host_name();
        let full = encode(&format!(
            "{}{}",
            observed.start_cwd,
            format!("/{component}").repeat(8)
        ));
        let room = MAX_OSC7_LEN - "file://".len() - host.len();
        assert!(full.len() > room, "the directory is past the cap");
        let cut = full[..=room].rfind('/').expect("a slash");
        let want = format!("7;file://{host}{}", &full[..cut]);
        let reports: Vec<&Vec<u8>> = observed
            .payloads
            .iter()
            .filter(|p| p.starts_with(b"7;"))
            .collect();
        for report in &reports {
            assert!(
                report.len() - 2 <= MAX_OSC7_LEN,
                "{shell:?}: {} bytes",
                report.len()
            );
            assert!(
                parse_osc7(&report[2..]).is_some(),
                "{shell:?}: {:?}",
                String::from_utf8_lossy(report)
            );
        }
        let last = reports.last().expect("a report");
        assert_eq!(String::from_utf8_lossy(last), want, "{shell:?}");
        assert!(
            want.len() - 2 > MAX_OSC7_LEN - 721,
            "{shell:?}: the deepest that fits"
        );
    }
}

/// Terminal.app's encoder (fixture P1): every byte outside
/// `[/._~A-Za-z0-9-]` as `%XX`.
fn encode(path: &str) -> String {
    path.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"/._~-".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Loading a script again at a prompt (after it installed itself) adds no
/// hooks and no second handshake: one command gives `A B C D;0`, then
/// `exit` gives `A B C`.
#[test]
fn loading_again_at_a_prompt_installs_nothing_new() {
    use Mark::{CommandEnd, CommandStart, OutputStart, PromptStart};
    let zsh = "export TESSERA_SHELL_INTEGRATION=enabled; \
               autoload -Uz -- \"$TESSERA_TEST_SCRIPTS/zsh/tessera-integration\"; \
               tessera-integration\r";
    let bash = "export TESSERA_SHELL_INTEGRATION=enabled; \
                . \"$TESSERA_TEST_SCRIPTS/bash/tessera.bash\"\r";
    let fish = "set -gx TESSERA_SHELL_INTEGRATION enabled; \
                source $TESSERA_TEST_SCRIPTS/fish/vendor_conf.d/tessera-shell-integration.fish\r";
    let mut cases = vec![(Shell::Zsh, PathBuf::from(ZSH), ".zshrc", zsh)];
    cases.extend(
        bashes()
            .into_iter()
            .map(|program| (Shell::Bash, program, ".bashrc", bash)),
    );
    if let Some(program) = find_shell(FISH) {
        cases.push((Shell::Fish, program, ".config/fish/config.fish", fish));
    } else {
        eprintln!("fish part skipped: fish is not installed (looked in {FISH:?})");
    }
    for (shell, program, rc, reload) in cases {
        let observed = run_steps(
            &Case {
                shell,
                program,
                args: Some(&["-i"]),
                files: &[(rc, "")],
                // fish's own marks off, so the marks are the script's.
                env: &[("fish_features", "no-mark-prompt")],
            },
            &[(Wait::Prompts(1), reload), (Wait::Prompts(2), "exit\r")],
        );
        let handshakes = observed
            .payloads
            .iter()
            .filter(|p| p.starts_with(b"1337;"))
            .count();
        assert_eq!(handshakes, 1, "{shell:?}");
        let mut got = marks(observed.payloads.iter().map(Vec::as_slice));
        if shell == Shell::Fish {
            // fish closes `exit` with a D too (see `check`).
            assert!(matches!(got.pop(), Some(CommandEnd { .. })), "{shell:?}");
        }
        let want = [
            PromptStart,
            CommandStart,
            OutputStart,
            CommandEnd { exit_code: Some(0) },
            PromptStart,
            CommandStart,
            OutputStart,
        ];
        assert_eq!(got, want, "{shell:?}");
    }
}

/// The DEBUG trap runs before every simple command, so the script keeps
/// its own only from the prompt to the start of the user's command: while
/// a command runs there is no trap (or only the user's), so a loop in an
/// interactive bash costs what it costs without the integration. A user's
/// own DEBUG trap keeps running for every command, the first one after the
/// prompt included.
#[test]
fn bash_debug_trap_is_gone_while_a_command_runs() {
    for bash in bashes() {
        eprintln!("bash: {}", bash.display());
        let probe = "echo \"trap=[$(trap -p DEBUG)]\"; \
                 echo \"seen=$(printf '%s\\n' \"${seen[@]}\" | grep -c '^echo first$')\"\r";
        for (bashrc, trap) in [
            ("", "trap=[]".to_owned()),
            (
                r#"trap 'seen+=("$BASH_COMMAND")' DEBUG"#,
                r#"trap=[trap -- 'seen+=("$BASH_COMMAND")' DEBUG]"#.to_owned(),
            ),
        ] {
            let observed = run_steps(
                &Case {
                    shell: Shell::Bash,
                    program: bash.clone(),
                    args: Some(&["-i"]),
                    files: &[(".bashrc", bashrc)],
                    env: &[],
                },
                &[
                    (Wait::Prompts(1), "echo first\r"),
                    (Wait::Prompts(2), probe),
                    (Wait::Prompts(3), "exit\r"),
                ],
            );
            let text = String::from_utf8_lossy(&observed.output);
            assert!(
                text.contains(&format!("{trap}\r\n")),
                "{bashrc:?}: {text:?}"
            );
            if !bashrc.is_empty() {
                assert!(
                    text.contains("seen=1\r\n"),
                    "the user's trap missed a command: {text:?}"
                );
            }
            // And the marks are unaffected.
            let marks = marks(observed.payloads.iter().map(Vec::as_slice));
            assert_eq!(
                marks.iter().filter(|m| **m == Mark::OutputStart).count(),
                3,
                "{bashrc:?}: {text:?}"
            );
        }
    }
}

/// The user's prompt still sees each command's status: a `PROMPT_COMMAND`
/// function that reads `$?` first runs after the script's first entry, and
/// PS1's `$?` (which bash restores after `PROMPT_COMMAND`: plain bash 3.2
/// shows `st=1` after `false` even with `PROMPT_COMMAND='(exit 7)'`).
#[test]
fn bash_prompt_sees_the_last_status() {
    for bash in bashes() {
        eprintln!("bash: {}", bash.display());
        for bashrc in [
            "PS1='st=$?\\$ '\n",
            "PS1='st=$?\\$ '\nPROMPT_COMMAND='(exit 7)'\n",
            "pc() { local s=$?; PS1=\"st=$s\\$ \"; }\nPROMPT_COMMAND=pc\n",
        ] {
            let observed = run_steps(
                &Case {
                    shell: Shell::Bash,
                    program: bash.clone(),
                    args: Some(&["-i"]),
                    files: &[(".bashrc", bashrc)],
                    env: &[],
                },
                &[
                    (Wait::Prompts(1), "false\r"),
                    (Wait::Prompts(2), "true\r"),
                    (Wait::Prompts(3), "exit\r"),
                ],
            );
            let text = String::from_utf8_lossy(&observed.output);
            let prompts: Vec<&str> = text
                .match_indices("st=")
                .map(|(i, _)| &text[i..i + 4])
                .collect();
            assert_eq!(prompts, ["st=0", "st=1", "st=0"], "{bashrc:?}: {text:?}");
        }
    }
}

/// A bash started from an integrated one (both injection routes) sees no
/// `PROMPT_COMMAND` of ours: no "command not found" for our functions, and
/// its own DEBUG trap is left alone.
#[test]
fn child_shells_inherit_no_hooks() {
    let child = "printf '%s\\n' \"trap 'n=1' DEBUG\" true \
                 'echo \"child-trap=[$(trap -p DEBUG)]\"' | bash --norc -i 2>&1; \
                 echo \"done-$((40+2))\"; \
                 echo \"session-env=[$(env | grep -E '^TESSERA_(SHELL_INTEGRATION|BASH_)')]\"\r";
    // What a startup file's children see (the --rcfile route takes the
    // injection's variables out of the environment first).
    let rc_env = "echo \"rc-env=[$(env | grep -E '^TESSERA_(SHELL_INTEGRATION|BASH_)')]\"\n";
    for bash in bashes() {
        for args in [Some(&["-i"][..]), None] {
            let observed = run_steps(
                &Case {
                    shell: Shell::Bash,
                    program: bash.clone(),
                    args,
                    files: &[(".bashrc", rc_env), (".bash_profile", "")],
                    env: &[],
                },
                &[(Wait::Prompts(1), child), (Wait::Prompts(2), "exit\r")],
            );
            let text = String::from_utf8_lossy(&observed.output);
            let what = format!("{} {args:?}", bash.display());
            assert!(text.contains("session-env=[]\r\n"), "{what}: {text:?}");
            if args.is_some() {
                assert!(text.contains("rc-env=[]\r\n"), "{what}: {text:?}");
            }
            assert!(text.contains("done-42\r\n"), "{what}: {text:?}");
            assert!(!text.contains("command not found"), "{what}: {text:?}");
            assert!(
                text.contains("child-trap=[trap -- 'n=1' DEBUG]"),
                "{what}: {text:?}"
            );
        }
    }
}

/// The DEBUG trap leaves `$_` (the previous command's last argument) as it
/// was (`mkdir d && cd $_`), also with a user's DEBUG trap after ours that
/// keeps it itself (one that does not, like `trap 'n=1' DEBUG`, clears `$_`
/// in a plain bash too).
#[test]
fn bash_keeps_the_last_argument() {
    for bash in bashes() {
        for bashrc in ["", "trap ': \"$_\"' DEBUG\n"] {
            let observed = run_steps(
                &Case {
                    shell: Shell::Bash,
                    program: bash.clone(),
                    args: Some(&["-i"]),
                    files: &[(".bashrc", bashrc)],
                    env: &[],
                },
                // `$_` on the next line: the trap runs between the two.
                &[
                    (Wait::Prompts(1), "echo a lastarg_x\r"),
                    (Wait::Prompts(2), "echo \"[$_]\"\r"),
                    (Wait::Prompts(3), "mkdir -p made\r"),
                    (Wait::Prompts(4), "cd $_ && echo \"in=${PWD##*/}\"\r"),
                    (Wait::Prompts(5), "exit\r"),
                ],
            );
            let text = String::from_utf8_lossy(&observed.output);
            let what = format!("{} {bashrc:?}", bash.display());
            assert!(text.contains("[lastarg_x]\r\n"), "{what}: {text:?}");
            assert!(text.contains("in=made\r\n"), "{what}: {text:?}");
        }
    }
}

/// The cursor position after replaying `output` up to `end` in a 24x80
/// terminal (the PTY's size).
fn cursor_after(output: &[u8], end: usize) -> (u16, u16) {
    let mut terminal = tessera_term::Terminal::new(24, 80);
    terminal.advance(&output[..end]);
    let cursor = terminal.grid().cursor();
    (cursor.row, cursor.col)
}

/// The marks in the prompt take no width: zsh places a right prompt and
/// then puts the cursor back after the left one by the width it computed,
/// and readline wraps a long command line by it; a mark counted as text
/// moves the cursor. Checked by replaying the output through the emulator.
#[test]
fn prompt_marks_take_no_width() {
    // zsh: PS1 `abc% ` (5 columns) and a right prompt; the cursor waits in
    // column 5 when the first typed character is echoed.
    let observed = run_steps(
        &Case {
            shell: Shell::Zsh,
            program: ZSH.into(),
            args: Some(&["-i"]),
            files: &[(".zshrc", "PS1='abc%# '\nRPS1='<r>'\n")],
            env: &[],
        },
        &[(Wait::Prompts(1), "exit\r")],
    );
    let output = &observed.output;
    let after_b = output
        .windows(8)
        .position(|w| w == b"\x1b]133;B\x07")
        .expect("a B mark")
        + 8;
    let typed = after_b
        + output[after_b..]
            .windows(4)
            .position(|w| w == b"exit")
            .expect("the echoed command");
    assert_eq!(
        cursor_after(output, typed).1,
        5,
        "zsh: {:?}",
        String::from_utf8_lossy(output)
    );
    // zsh moves to the right prompt relative to the width it computed for
    // the left one: a mark counted as text puts `<r>` too far left. It ends
    // one column before the edge (ZLE_RPROMPT_INDENT=1): columns 76-78.
    let mut terminal = tessera_term::Terminal::new(24, 80);
    terminal.advance(&output[..typed]);
    let row = terminal.grid().cursor().row;
    let line = terminal.grid().line(row).expect("the prompt row").text();
    assert_eq!(
        line.find("<r>"),
        Some(76),
        "zsh prompt row {line:?}: {:?}",
        String::from_utf8_lossy(output)
    );

    // bash: PS1 `ab$ ` (4 columns) and a 100-character command line,
    // which wraps once: it ends in column 4 + 100 - 80 = 24 of the next row.
    let long = format!(": {}\r", "x".repeat(98));
    for bash in bashes() {
        let observed = run_steps(
            &Case {
                shell: Shell::Bash,
                program: bash.clone(),
                args: Some(&["-i"]),
                files: &[(".bashrc", "PS1='ab\\$ '\n")],
                env: &[],
            },
            &[(Wait::Prompts(1), &long), (Wait::Prompts(2), "exit\r")],
        );
        let output = &observed.output;
        // The last echoed `x` before the command's C mark.
        let c_mark = output
            .windows(8)
            .position(|w| w == b"\x1b]133;C\x07")
            .expect("the C mark");
        let last_x = (0..c_mark)
            .rev()
            .find(|&i| output[i] == b'x')
            .expect("the echoed command line");
        let start = (0..last_x)
            .rev()
            .find(|&i| output[i..].starts_with(b"\x1b]133;A"))
            .expect("the prompt");
        let (row, col) = cursor_after(output, last_x + 1);
        let (prompt_row, _) = cursor_after(output, start);
        assert_eq!(
            (row - prompt_row, col),
            (1, 24),
            "{}: {:?}",
            bash.display(),
            String::from_utf8_lossy(output)
        );
    }
}

/// With `set -T` (functrace) the DEBUG trap also fires inside functions,
/// ours included: those must not be taken for the user's command, and a
/// user function called at the prompt is one command.
#[test]
fn bash_with_functrace() {
    for bash in bashes() {
        eprintln!("bash: {}", bash.display());
        let observed = run(&Case {
            shell: Shell::Bash,
            program: bash.clone(),
            args: Some(&["-i"]),
            files: &[(
                ".bashrc",
                "set -T\nf() { true; true; }\nPROMPT_COMMAND='f'\n",
            )],
            env: &[],
        });
        check(&observed, "LB", Shell::Bash, false);
    }
}

/// The harness's waits end at their deadline even while output keeps
/// arriving (a shell flooding the terminal).
#[test]
fn harness_waits_end_at_their_deadline_under_a_flood() {
    let options = SpawnOptions::command("/bin/sh", "live").args(["-c", "exec yes"]);
    let (mut pty, events) = NativePty::spawn(&options).expect("spawn yes");
    let mut out = Output {
        bytes: Vec::new(),
        answered: [0; ANSWERS.len()],
    };
    let started = Instant::now();
    let done = pump_for(Duration::from_secs(1), &mut pty, &events, &mut out, |_| {
        false
    });
    let took = started.elapsed();
    let _ = pty.kill();
    assert!(!done);
    assert!(!out.bytes.is_empty(), "no flood");
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

/// A login bash whose startup file turns the integration off still takes
/// the loader out of `PROMPT_COMMAND` and stops exporting it, as the
/// injection exported it.
#[test]
fn a_disabled_login_bash_exports_no_prompt_command() {
    for bash in bashes() {
        let output = run_raw(
            &Case {
                shell: Shell::Bash,
                program: bash.clone(),
                args: None,
                files: &[(".bash_profile", "export TESSERA_SHELL_INTEGRATION=0\n")],
                env: &[],
            },
            "echo \"pc-env=[$(env | grep '^PROMPT_COMMAND=')] $((40+2))\"\rexit\r",
        );
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("pc-env=[] 42\r\n"),
            "{}: {text:?}",
            bash.display()
        );
        assert!(
            !has_integration_oscs(&output),
            "{}: {text:?}",
            bash.display()
        );
    }
}

/// fish takes our entry out of `XDG_DATA_DIRS` again and keeps the user's.
#[test]
fn fish_restores_the_users_xdg_data_dirs() {
    let Some(program) = find_shell(FISH) else {
        eprintln!("skipped: fish is not installed (looked in {FISH:?})");
        return;
    };
    let output = run_raw(
        &Case {
            shell: Shell::Fish,
            program,
            args: Some(&["-i"]),
            files: &[],
            env: &[("user:XDG_DATA_DIRS", "/opt/x/share:/usr/share")],
        },
        "echo \"xdg=[$XDG_DATA_DIRS] tsi=[$TESSERA_FISH_XDG_DATA_DIR] \"(math 40+2)\rexit\r",
    );
    let text = String::from_utf8_lossy(&output);
    assert!(
        text.contains("xdg=[/opt/x/share:/usr/share] tsi=[] 42\r\n"),
        "{text:?}"
    );
}

/// fish (with its own marks off, as before 4.0): an empty line at the prompt
/// closes it with a bare D.
#[test]
fn fish_empty_line_is_a_bare_d() {
    let Some(program) = find_shell(FISH) else {
        eprintln!("skipped: fish is not installed (looked in {FISH:?})");
        return;
    };
    let observed = run_steps(
        &Case {
            shell: Shell::Fish,
            program,
            args: Some(&["-i"]),
            files: &[],
            env: &[("fish_features", "no-mark-prompt")],
        },
        &[(Wait::Prompts(1), "\r"), (Wait::Prompts(2), "exit\r")],
    );
    let got = marks(observed.payloads.iter().map(Vec::as_slice));
    let c = got
        .iter()
        .position(|m| *m == Mark::OutputStart)
        .expect("exit's C");
    let bare = got[..c]
        .iter()
        .filter(|m| **m == Mark::CommandEnd { exit_code: None })
        .count();
    assert_eq!(bare, 1, "{got:?}");
}

/// The marks one successful command after another gives: `A B C D;0` each
/// (`exit_codes[i]` for command i), the last (`exit`) `A B C`.
/// [`marks_for`], but on the login route with a DEBUG trap set in
/// `.bash_profile` bash 3.2's first command has no C and a bare D: the
/// loader cannot arm the trap on the loading prompt there (ACCEPTED LIMIT,
/// ADR 0021).
fn marks_for_route(bash: &Path, startup: &str, bashrc: &str, exit_codes: &[i32]) -> Vec<Mark> {
    let mut want = marks_for(exit_codes);
    if bash == Path::new("/bin/bash") && startup == ".bash_profile" && bashrc.contains(" DEBUG") {
        want.splice(
            0..4,
            [
                Mark::PromptStart,
                Mark::CommandStart,
                Mark::CommandEnd { exit_code: None },
            ],
        );
    }
    want
}

fn marks_for(exit_codes: &[i32]) -> Vec<Mark> {
    let mut want = Vec::new();
    for &code in exit_codes {
        want.extend([
            Mark::PromptStart,
            Mark::CommandStart,
            Mark::OutputStart,
            Mark::CommandEnd {
                exit_code: Some(code),
            },
        ]);
    }
    want.extend([Mark::PromptStart, Mark::CommandStart, Mark::OutputStart]);
    want
}

/// Under `shopt -s extdebug` a DEBUG trap that returns non-zero skips the
/// command (and 2 means `return`): the trap must not hand on the last
/// command's status, so `echo` after `false` still runs.
#[test]
fn bash_extdebug_runs_every_command() {
    for bash in bashes() {
        let observed = run_steps(
            &Case {
                shell: Shell::Bash,
                program: bash.clone(),
                args: Some(&["-i"]),
                // A function returns 2 (a subshell would inherit the trap
                // under extdebug: see the script's limits).
                files: &[(".bashrc", "shopt -s extdebug\nret2() { return 2; }\n")],
                env: &[],
            },
            &[
                (Wait::Prompts(1), "false\r"),
                (Wait::Prompts(2), "echo visible-$((40+1))\r"),
                (Wait::Prompts(3), "ret2\r"),
                (Wait::Prompts(4), "echo visible-$((40+2))\r"),
                (Wait::Prompts(5), "exit\r"),
            ],
        );
        let text = String::from_utf8_lossy(&observed.output);
        let what = bash.display();
        assert!(text.contains("visible-41\r\n"), "{what}: {text:?}");
        assert!(text.contains("visible-42\r\n"), "{what}: {text:?}");
        let got = marks(observed.payloads.iter().map(Vec::as_slice));
        assert_eq!(got, marks_for(&[1, 0, 2, 0]), "{what}: {text:?}");
    }
}

/// `set -u` (from `.bashrc`, or typed at a prompt): the script still loads
/// and marks every command; no "unbound variable".
#[test]
fn bash_with_nounset() {
    for bash in bashes() {
        for (bashrc, first) in [("set -u\n", "true\r"), ("", "set -u\r")] {
            let observed = run_steps(
                &Case {
                    shell: Shell::Bash,
                    program: bash.clone(),
                    args: Some(&["-i"]),
                    files: &[(".bashrc", bashrc)],
                    env: &[],
                },
                &[
                    (Wait::Prompts(1), first),
                    (Wait::Prompts(2), "true\r"),
                    (Wait::Prompts(3), "exit\r"),
                ],
            );
            let text = String::from_utf8_lossy(&observed.output);
            let what = format!("{} {bashrc:?}", bash.display());
            assert!(!text.contains("unbound variable"), "{what}: {text:?}");
            let got = marks(observed.payloads.iter().map(Vec::as_slice));
            assert_eq!(got, marks_for(&[0, 0]), "{what}: {text:?}");
        }
    }
}

/// `[A, B, C, D;code]` for one command (`c`: whether it has its C;
/// `code`: `None` for a bare D).
fn command_marks(c: bool, code: Option<i32>) -> Vec<Mark> {
    let mut marks = vec![Mark::PromptStart, Mark::CommandStart];
    if c {
        marks.push(Mark::OutputStart);
    }
    marks.push(Mark::CommandEnd { exit_code: code });
    marks
}

/// Processes that keep running `[[ -e ]]` and `-ef` on /dev/fd/59 to 57
/// of their own, at the lowest priority; dropped, they are killed and
/// reaped. Each stops by itself once the test process is gone (`nice`
/// execs bash, so `$PPID` is the test process), so a killed test leaves
/// no spinning orphans behind. It looks every 100 rounds: a look every
/// round left too few stats at the lowest priority to catch a single
/// `-ef` try.
struct FdStatters(Vec<std::process::Child>);

impl FdStatters {
    fn start(count: usize) -> Self {
        let text = "exec 59>/dev/null 58>/dev/null 57>/dev/null
                    while kill -0 $PPID 2>/dev/null; do
                        for i in {1..100}; do
                            [[ -e /dev/fd/59 ]]; [[ /dev/fd/58 -ef /dev/fd/57 ]]
                        done
                    done";
        Self(
            (0..count)
                .map(|_| {
                    std::process::Command::new("/usr/bin/nice")
                        .args(["-n", "20", "/bin/bash", "-c", text])
                        .stdin(std::process::Stdio::null())
                        .spawn()
                        .expect("start a statter")
                })
                .collect(),
        )
    }
}

impl Drop for FdStatters {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// #611: a stat of /dev/fd/N fails now and then (EBADF) while another
/// process stats its own N (XNU shares one fdesc node per number; every
/// integrated shell uses the same numbers). With processes doing that all
/// the time, every command still gets its C (the copies were taken for a
/// redirection, 1 command in 20 lost its C), and a descriptor of the
/// user's is never taken for a free one (it was replaced and then closed).
#[test]
fn bash_marks_hold_while_other_processes_stat_the_same_descriptors() {
    const COMMANDS: usize = 200;
    let mut inputs = vec!["true\r"; COMMANDS];
    inputs.push("echo kept-$((6*7)) >&59; /usr/bin/sed 's/^/file:/' \"$HOME/kept\"\r");
    inputs.push("exit\r");
    let steps: Vec<(Wait, &str)> = inputs
        .iter()
        .enumerate()
        .map(|(i, input)| (Wait::Prompts(i + 1), *input))
        .collect();
    let _statters = FdStatters::start(10);
    for bash in bashes() {
        let observed = run_steps(
            &Case {
                shell: Shell::Bash,
                program: bash.clone(),
                args: Some(&["-i"]),
                files: &[(".bashrc", "exec 59>\"$HOME/kept\"\n")],
                env: &[],
            },
            &steps,
        );
        let text = String::from_utf8_lossy(&observed.output);
        let what = bash.display();
        assert!(
            text.contains("file:kept-42\r\n"),
            "{what}: the user's descriptor 59 was lost: {text:?}"
        );
        let got = marks(observed.payloads.iter().map(Vec::as_slice));
        assert_eq!(got, marks_for(&[0; COMMANDS + 1]), "{what}: {text:?}");
    }
}

/// #611 adversarial B1: with every descriptor from 10 up to the limit
/// open (EMFILE), bash 3.2 cannot save standard output and error for a
/// redirection and keeps the redirection for good, so a probe of a
/// descriptor (`{ : >&50; } 2>/dev/null`) left the shell writing to the
/// user's file at 50 with no prompt shown. At a limit of 64 and of 256
/// (10 to 254: 255 is bash's terminal), and with only 10 free (one save
/// at a time, the probe needs two), the shell's output stays on the
/// terminal, every command gets its marks, and the user's file holds only
/// what the user wrote to it. So they do after `ulimit -n 11` (10 and 11
/// look free, but only 10 is below the limit), also with 5 to 9 open
/// (#611 adversarial cycle 2). (Each prompt still prints bash's
/// "cannot duplicate fd" twice, from the forked `trap -p` reader, as on
/// main; that changes no descriptor of the shell.)
#[test]
fn bash_marks_hold_with_no_free_descriptor_from_ten_up() {
    let user = "exec 50>>\"$HOME/userfile\"; echo user-line >&50";
    let mut cases = Vec::new();
    for (limit, ten) in [
        (64, "exec 10</dev/null"),
        (256, "exec 10</dev/null"),
        (64, ":"),
    ] {
        let top = limit.min(255);
        cases.push((
            format!("a limit of {limit} ({ten})"),
            format!("ulimit -n {limit}; {user}\r"),
            format!(
                "for ((i = 5; i < {top}; i++)); do [[ $i == 50 || $i == 10 ]] || \
                 eval \"exec $i</dev/null\"; done 2>/dev/null; {ten}\r"
            ),
            format!(
                "for ((i = 5; i < {top}; i++)); do [[ $i == 50 ]] || eval \"exec $i<&-\"; done\r"
            ),
        ));
    }
    for (fill, close) in [
        ("", ":"),
        (
            "for i in 5 6 7 8 9; do eval \"exec $i</dev/null\"; done 2>/dev/null; ",
            "for i in 5 6 7 8 9; do eval \"exec $i<&-\"; done",
        ),
    ] {
        cases.push((
            format!("ulimit -n 11 after {fill:?}"),
            format!("{user}\r"),
            format!("{fill}ulimit -n 11\r"),
            format!("{close}\r"),
        ));
    }
    for (label, open, fill, close) in &cases {
        let inputs = [
            open.as_str(),
            fill.as_str(),
            "echo visible-$((6*7))\r",
            close.as_str(),
            "while IFS= read -r l; do echo \"kept:$l\"; done < \"$HOME/userfile\"\r",
            "exit\r",
        ];
        let steps: Vec<(Wait, &str)> = inputs
            .iter()
            .enumerate()
            .map(|(i, input)| (Wait::Prompts(i + 1), *input))
            .collect();
        for bash in bashes() {
            let observed = run_steps(
                &Case {
                    shell: Shell::Bash,
                    program: bash.clone(),
                    args: Some(&["-i"]),
                    files: &[],
                    env: &[],
                },
                &steps,
            );
            let text = String::from_utf8_lossy(&observed.output);
            let what = format!("{} at {label}", bash.display());
            assert!(text.contains("visible-42\r\n"), "{what}: {text:?}");
            assert!(!text.contains("Bad file descriptor"), "{what}: {text:?}");
            assert_eq!(
                text.matches("kept:").count() - text.matches("kept:$l").count(),
                1,
                "{what}: the user's file got the shell's output: {text:?}"
            );
            assert!(text.contains("kept:user-line\r\n"), "{what}: {text:?}");
            let got = marks(observed.payloads.iter().map(Vec::as_slice));
            assert_eq!(got, marks_for(&[0; 5]), "{what}: {text:?}");
        }
    }
}

/// Output redirected to a file is byte for byte the command's: the C mark
/// (written from the DEBUG trap, which fires inside the redirection of a
/// compound command, and inside a subshell's under `set -T`) goes to the
/// terminal instead, also when standard error is redirected too. #538.
#[test]
fn bash_redirected_output_gets_no_marks() {
    for (bash, args, startup) in bash_routes() {
        for functrace in [false, true] {
            let out = tempfile::tempdir().unwrap();
            let bashrc = format!(
                "{}fout() {{ echo f; }}\nfbody() {{ echo b; }} > \"$OUT/body\"\n",
                if functrace { "set -T\n" } else { "" }
            );
            let observed = run_steps(
                &Case {
                    shell: Shell::Bash,
                    program: bash.clone(),
                    args,
                    files: &[(startup, &bashrc)],
                    env: &[("OUT", out.path().to_str().unwrap())],
                },
                &[
                    (Wait::Prompts(1), "{ echo g; } > \"$OUT/group\"\r"),
                    (Wait::Prompts(2), "( echo s ) > \"$OUT/subshell\"\r"),
                    (Wait::Prompts(3), "fout > \"$OUT/function\"\r"),
                    (Wait::Prompts(4), "fbody\r"),
                    (
                        Wait::Prompts(5),
                        "for i in 1; do echo $i; done > \"$OUT/loop\"\r",
                    ),
                    (Wait::Prompts(6), "{ echo both; } > \"$OUT/both\" 2>&1\r"),
                    (Wait::Prompts(7), "exit\r"),
                ],
            );
            let text = String::from_utf8_lossy(&observed.output);
            let what = format!("{} {startup} functrace={functrace}", bash.display());
            for (name, want) in [
                ("group", "g\n"),
                ("subshell", "s\n"),
                ("function", "f\n"),
                ("body", "b\n"),
                ("loop", "1\n"),
                ("both", "both\n"),
            ] {
                let got = std::fs::read(out.path().join(name)).unwrap();
                assert_eq!(
                    String::from_utf8_lossy(&got),
                    want,
                    "{what}: {name}; terminal: {text:?}"
                );
            }
            // Every C reached the terminal. A command line that only runs
            // a subshell has none (with `set -T` the subshell sends it) and
            // ends with a bare D (the script's limits).
            let mut want = command_marks(true, Some(0));
            want.extend(command_marks(functrace, None));
            for _ in 0..4 {
                want.extend(command_marks(true, Some(0)));
            }
            want.extend([Mark::PromptStart, Mark::CommandStart, Mark::OutputStart]);
            let got = marks(observed.payloads.iter().map(Vec::as_slice));
            assert_eq!(got, want, "{what}: {text:?}");
        }
    }
}

/// A session logged through a pipe (`exec > >(tee -a log) 2>&1` in
/// `.bashrc`): D follows the command's output, which is still in the pipe
/// when the command ends, so D must take the same way (it once went to
/// /dev/tty and arrived before the last 40 KB of `seq 1 20000`).
#[test]
fn bash_session_through_tee_gets_d_after_the_output() {
    for bash in bashes() {
        let observed = run_steps(
            &Case {
                shell: Shell::Bash,
                program: bash.clone(),
                args: Some(&["-i"]),
                files: &[(".bashrc", "exec > >(tee -a \"$HOME/session.log\") 2>&1\n")],
                env: &[],
            },
            // The prompt (A) goes through the pipe too: once the third is
            // in, so is everything `seq` wrote before it.
            &[
                (Wait::Prompts(1), "seq 1 20000\r"),
                (Wait::Prompts(2), "true\r"),
                (Wait::Prompts(3), "exit\r"),
            ],
        );
        let text = String::from_utf8_lossy(&observed.output);
        let what = bash.display();
        let output = &observed.output;
        let end = output
            .windows(9)
            .position(|w| w == b"\r\n20000\r\n")
            .unwrap_or_else(|| panic!("{what}: no end of output: {text:?}"))
            + 9;
        assert_eq!(
            count(&output[..end], b"\x1b]133;D"),
            0,
            "{what}: a D before the output ended"
        );
        // `exit`'s C may still be in the pipe when bash exits.
        let mut got = marks(observed.payloads.iter().map(Vec::as_slice));
        got.truncate(10);
        let mut want = marks_for(&[0, 0]);
        want.truncate(10);
        assert_eq!(got, want, "{what}: {text:?}");
    }
}

/// Two lines pasted at once into a session logged through a pipe: the
/// second line's C takes the pipe, as the first line's output did, so it
/// follows that output and the first line's D (through /dev/tty it once
/// landed some 30 KB inside `seq 1 20000`).
#[test]
fn bash_pasted_lines_through_tee_keep_c_after_the_output() {
    for bash in bashes() {
        let observed = run_steps(
            &Case {
                shell: Shell::Bash,
                program: bash.clone(),
                args: Some(&["-i"]),
                files: &[(".bashrc", "exec > >(tee -a \"$HOME/session.log\") 2>&1\n")],
                env: &[],
            },
            &[
                (
                    Wait::Prompts(1),
                    "seq 1 20000; echo ENDOUT\rprintf 'lastline\\n'\r",
                ),
                (Wait::Prompts(3), "exit\r"),
            ],
        );
        let text = String::from_utf8_lossy(&observed.output);
        let what = bash.display();
        let output = &observed.output;
        let tail = b"\r\n20000\r\nENDOUT\r\n";
        let end = output
            .windows(tail.len())
            .position(|w| w == tail)
            .unwrap_or_else(|| panic!("{what}: no end of output: {text:?}"))
            + tail.len();
        let at = |mark: &[u8]| -> Vec<usize> {
            output
                .windows(mark.len())
                .enumerate()
                .filter(|(_, w)| *w == mark)
                .map(|(i, _)| i)
                .collect()
        };
        let cs = at(b"\x1b]133;C");
        let ds = at(b"\x1b]133;D");
        // The third (exit's) may or may not be in.
        assert!(cs.len() >= 2, "{what}: {text:?}");
        assert!(!ds.is_empty(), "{what}: {text:?}");
        assert!(cs[0] < end, "{what}: first C after the output");
        assert!(
            cs[1] > end && cs[1] > ds[0],
            "{what}: second C at {} inside the first line's output (ends at {end}, D at {})",
            cs[1],
            ds[0]
        );
        // `exit`'s C may still be in the pipe when bash exits.
        let mut got = marks(observed.payloads.iter().map(Vec::as_slice));
        got.truncate(10);
        let mut want = marks_for(&[0, 0]);
        want.truncate(10);
        assert_eq!(got, want, "{what}: {text:?}");
    }
}

/// The copy of standard output the hooks keep for C is closed before the
/// user's command runs: no command sees an fd it did not open (the copies
/// are two of 59 down to 50; `ls` itself opens 3).
#[test]
fn bash_commands_inherit_no_mark_fd() {
    for bash in bashes() {
        for bashrc in ["", "exec > >(tee -a \"$HOME/session.log\") 2>&1\n"] {
            let out = tempfile::tempdir().unwrap();
            let observed = run_steps(
                &Case {
                    shell: Shell::Bash,
                    program: bash.clone(),
                    args: Some(&["-i"]),
                    files: &[(".bashrc", bashrc)],
                    env: &[("OUT", out.path().to_str().unwrap())],
                },
                &[
                    (Wait::Prompts(1), "/bin/ls /dev/fd > \"$OUT/plain\"\r"),
                    (Wait::Prompts(2), "{ /bin/ls /dev/fd; } > \"$OUT/group\"\r"),
                    (Wait::Prompts(3), "\r"),
                    (Wait::Prompts(4), "/bin/ls /dev/fd > \"$OUT/after_empty\"\r"),
                    (Wait::Prompts(5), "exit\r"),
                ],
            );
            let text = String::from_utf8_lossy(&observed.output);
            let what = format!("{} {bashrc:?}", bash.display());
            for name in ["plain", "group", "after_empty"] {
                let got = std::fs::read_to_string(out.path().join(name)).unwrap();
                let fds: Vec<u32> = got.lines().map(|l| l.parse().unwrap()).collect();
                assert!(
                    !fds.is_empty() && fds.iter().all(|&fd| fd < 10),
                    "{what}: {name}: {got:?}; terminal: {text:?}"
                );
            }
        }
    }
}

/// A compound command that redirects a descriptor the hooks keep a copy
/// of standard output on (59 and 58) keeps its data: the trap, which runs
/// inside that redirection, writes no C there and closes nothing (C once
/// went into `f59` and the user's 59 was closed before `echo` ran). Any
/// other number (11, which bash 4.1+ `{var}` once picked) gets its C.
#[test]
fn bash_compound_redirecting_a_mark_fd_keeps_its_data() {
    for (bash, args, startup) in bash_routes() {
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("in"), "x\ny\n").unwrap();
        let observed = run_steps(
            &Case {
                shell: Shell::Bash,
                program: bash.clone(),
                args,
                files: &[(startup, "")],
                env: &[("OUT", out.path().to_str().unwrap())],
            },
            &[
                (Wait::Prompts(1), "{ echo a >&59; } 59>\"$OUT/f59\"\r"),
                (
                    Wait::Prompts(2),
                    "while read -u 59 l; do echo \"got $l\"; done 59<\"$OUT/in\" > \"$OUT/read\"\r",
                ),
                (Wait::Prompts(3), "{ echo b >&58; } 58>\"$OUT/f58\"\r"),
                (Wait::Prompts(4), "{ echo c >&11; } 11>\"$OUT/f11\"\r"),
                (Wait::Prompts(5), "/bin/ls /dev/fd > \"$OUT/fds\"\r"),
                (Wait::Prompts(6), "exit\r"),
            ],
        );
        let text = String::from_utf8_lossy(&observed.output);
        let what = format!("{} {startup}", bash.display());
        assert!(!text.contains("file descriptor"), "{what}: {text:?}");
        for (name, want) in [
            ("f59", "a\n"),
            ("read", "got x\ngot y\n"),
            ("f58", "b\n"),
            ("f11", "c\n"),
        ] {
            let got = std::fs::read_to_string(out.path().join(name)).unwrap();
            assert_eq!(got, want, "{what}: {name}; terminal: {text:?}");
        }
        let fds = std::fs::read_to_string(out.path().join("fds")).unwrap();
        assert!(
            fds.lines().all(|l| l.parse::<u32>().unwrap() < 10),
            "{what}: a copy outlived its prompt: {fds:?}"
        );
        let mut want = Vec::new();
        for _ in 0..3 {
            want.extend(command_marks(false, Some(0)));
        }
        for _ in 0..2 {
            want.extend(command_marks(true, Some(0)));
        }
        want.extend([Mark::PromptStart, Mark::CommandStart, Mark::OutputStart]);
        let got = marks(observed.payloads.iter().map(Vec::as_slice));
        assert_eq!(got, want, "{what}: {text:?}");
    }
}

/// Our hooks work whatever IFS holds: the DEBUG trap's conditional
/// commands are not split with it (`IFS=,` gave "builtin eval builtin trap
/// - DEBUG: command not found" around every command).
#[test]
fn bash_with_any_ifs() {
    for (bash, args, startup) in bash_routes() {
        // Also with a user's DEBUG trap (chained after ours).
        for bashrc in ["", "trap 'n=1' DEBUG\n"] {
            let observed = run_steps(
                &Case {
                    shell: Shell::Bash,
                    program: bash.clone(),
                    args,
                    files: &[(startup, bashrc)],
                    env: &[],
                },
                &[
                    (Wait::Prompts(1), "IFS=,\r"),
                    (Wait::Prompts(2), "true\r"),
                    (Wait::Prompts(3), "IFS=$'\\n'\r"),
                    (Wait::Prompts(4), "false\r"),
                    (Wait::Prompts(5), "IFS=\r"),
                    (Wait::Prompts(6), "true\r"),
                    (Wait::Prompts(7), "exit\r"),
                ],
            );
            let text = String::from_utf8_lossy(&observed.output);
            let what = format!("{} {startup} {bashrc:?}", bash.display());
            assert!(!text.contains("not found"), "{what}: {text:?}");
            let got = marks(observed.payloads.iter().map(Vec::as_slice));
            let want = marks_for_route(&bash, startup, bashrc, &[0, 0, 0, 1, 0, 0]);
            assert_eq!(got, want, "{what}: {text:?}");
        }
    }
}

/// A user's ERR trap runs for the user's failing commands only, as without
/// the integration: our hooks hand on the last status without failing a
/// command (they ran it once each after every failing command), also with
/// `set -E` (errtrace) and a user's DEBUG trap.
#[test]
fn bash_err_trap_runs_for_the_users_commands_only() {
    for (bash, args, startup) in bash_routes() {
        for bashrc in [
            "trap 'echo trap-err' ERR\n",
            "set -E\ntrap ': \"$_\"' DEBUG\ntrap 'echo trap-err' ERR\n",
        ] {
            let observed = run_steps(
                &Case {
                    shell: Shell::Bash,
                    program: bash.clone(),
                    args,
                    files: &[(startup, bashrc)],
                    env: &[],
                },
                &[
                    (Wait::Prompts(1), "false\r"),
                    (Wait::Prompts(2), "true\r"),
                    (Wait::Prompts(3), "exit\r"),
                ],
            );
            let text = String::from_utf8_lossy(&observed.output);
            let what = format!("{} {startup} {bashrc:?}", bash.display());
            assert_eq!(
                count(&observed.output, b"trap-err\r\n"),
                1,
                "{what}: {text:?}"
            );
            let got = marks(observed.payloads.iter().map(Vec::as_slice));
            assert_eq!(
                got,
                marks_for_route(&bash, startup, bashrc, &[1, 0]),
                "{what}: {text:?}"
            );
        }
    }
}

/// `set -e` (errexit): a failed command the shell survives without the
/// integration (`false && true`) does not exit it through our hooks either.
#[test]
fn bash_with_errexit() {
    for (bash, args, startup) in bash_routes() {
        for bashrc in ["set -e\n", "set -e\ntrap 'n=1' DEBUG\n"] {
            let observed = run_steps(
                &Case {
                    shell: Shell::Bash,
                    program: bash.clone(),
                    args,
                    files: &[(startup, bashrc)],
                    env: &[],
                },
                &[
                    (Wait::Prompts(1), "false && true\r"),
                    (Wait::Prompts(2), "echo alive-$((40+3))\r"),
                    (Wait::Prompts(3), "exit\r"),
                ],
            );
            let text = String::from_utf8_lossy(&observed.output);
            let what = format!("{} {startup} {bashrc:?}", bash.display());
            assert!(text.contains("alive-43\r\n"), "{what}: {text:?}");
            let got = marks(observed.payloads.iter().map(Vec::as_slice));
            assert_eq!(
                got,
                marks_for_route(&bash, startup, bashrc, &[1, 0]),
                "{what}: {text:?}"
            );
        }
    }
}

/// A prompt framework's own preexec (starship's shape: `PROMPT_COMMAND` is
/// its precmd, which arms it at its end, and its DEBUG trap starts a timer
/// at the first command after that) must start on the user's command, as
/// without the integration, and see that command's `$?` before it.
#[test]
fn bash_prompt_framework_preexec_sees_the_users_command() {
    let bashrc = r#"
fw_precmd() {
    [[ -n "${fw_started:-}" ]] && echo "started-by=[$fw_started] status=[$fw_status]"
    fw_started=
    fw_ready=1
}
fw_preexec() {
    local status=$?
    [[ "${fw_ready:-}" == 1 ]] || return 0
    fw_ready=
    fw_started=$BASH_COMMAND
    fw_status=$status
}
PROMPT_COMMAND=fw_precmd
trap 'fw_preexec "$_"' DEBUG
"#;
    // Also with the script sourced by hand after the framework's setup
    // (its trap was hidden from the script then, #542).
    let by_hand = format!("{bashrc}{BY_HAND}");
    for bash in bashes() {
        for (how, route) in [
            ("plain", None),
            ("injected", Some(Route::Injected)),
            ("by hand", Some(Route::ByHand)),
        ] {
            let mut env = vec![];
            if route.is_none() {
                env.push(("TESSERA_SHELL_INTEGRATION", "0"));
            }
            let case = Case {
                shell: Shell::Bash,
                program: bash.clone(),
                args: Some(&["-i"]),
                files: &[(
                    ".bashrc",
                    if route == Some(Route::ByHand) {
                        &by_hand
                    } else {
                        bashrc
                    },
                )],
                env: &env,
            };
            let output = match route {
                Some(route) => {
                    let observed = run_steps_via(
                        &case,
                        &[
                            (Wait::Prompts(1), "false\r"),
                            (Wait::Prompts(2), "echo two\r"),
                            (Wait::Prompts(3), "exit\r"),
                        ],
                        route,
                    );
                    let got = marks(observed.payloads.iter().map(Vec::as_slice));
                    assert_eq!(
                        got,
                        marks_for(&[1, 0]),
                        "{} {how}: {:?}",
                        bash.display(),
                        String::from_utf8_lossy(&observed.output)
                    );
                    observed.output
                }
                None => run_raw(&case, "false\recho two\rexit\r"),
            };
            let text = String::from_utf8_lossy(&output);
            let what = format!("{} {how}", bash.display());
            assert!(
                text.contains("started-by=[false] status=[0]\r\n"),
                "{what}: {text:?}"
            );
            assert!(
                text.contains("started-by=[echo two] status=[1]\r\n"),
                "{what}: {text:?}"
            );
        }
    }
}

/// Where a case's own DEBUG trap comes from.
struct Placement {
    what: &'static str,
    route: Route,
    bashrc: String,
    /// Whether the trap is typed at the first prompt (before the inputs).
    typed: bool,
}

/// A `.bashrc` that sets `trap` before or after the line that loads the
/// script by hand, a `.bashrc` read through the injection that sets it, and
/// the trap typed at the first prompt (on either route).
fn trap_placements(trap: &str) -> [Placement; 5] {
    let line = format!("{trap}\n");
    [
        Placement {
            what: "by hand, set before",
            route: Route::ByHand,
            bashrc: format!("{line}{BY_HAND}"),
            typed: false,
        },
        Placement {
            what: "by hand, set after",
            route: Route::ByHand,
            bashrc: format!("{BY_HAND}{line}"),
            typed: false,
        },
        Placement {
            what: "by hand, set at a prompt",
            route: Route::ByHand,
            bashrc: BY_HAND.into(),
            typed: true,
        },
        Placement {
            what: "injected, set in .bashrc",
            route: Route::Injected,
            bashrc: line,
            typed: false,
        },
        Placement {
            what: "injected, set at a prompt",
            route: Route::Injected,
            bashrc: String::new(),
            typed: true,
        },
    ]
}

/// Runs `bash` with `place`'s `.bashrc`, typing `inputs` one prompt at a
/// time (after `trap` when the placement types it).
fn run_placed(bash: &std::path::Path, place: &Placement, trap: &str, inputs: &[&str]) -> Observed {
    let typed = format!("{trap}\r");
    let steps: Vec<(Wait, &str)> = place
        .typed
        .then_some(typed.as_str())
        .into_iter()
        .chain(inputs.iter().copied())
        .enumerate()
        .map(|(i, input)| (Wait::Prompts(i + 1), input))
        .collect();
    run_steps_via(
        &Case {
            shell: Shell::Bash,
            program: bash.to_path_buf(),
            args: Some(&["-i"]),
            files: &[(".bashrc", &place.bashrc)],
            env: &[],
        },
        &steps,
        place.route,
    )
}

/// A user's own DEBUG trap keeps running for the user's commands wherever
/// it was set: before or after the line in `.bashrc` that sources the
/// script by hand (where `trap -p DEBUG` in the sourced file shows
/// nothing, #542), through the injection, or later at a prompt. The marks
/// stay as they are without it. An empty line first (it fires no trap, so
/// ours is still there at the next prompt) changes nothing. The trap has
/// quotes in it, which `trap -p` escapes.
#[test]
fn bash_users_debug_trap_keeps_running_wherever_it_was_set() {
    // Adds `$BASH_COMMAND` joined with an empty string.
    let trap = r#"trap 'seen+=("$BASH_COMMAND"'\'''\'')' DEBUG"#;
    let probe = "echo \"trap=[$(trap -p DEBUG)]\"; \
                 echo \"seen=$(printf '%s\\n' \"${seen[@]}\" | grep -c '^echo first$')\"\r";
    for bash in bashes() {
        for place in trap_placements(trap) {
            let observed = run_placed(
                &bash,
                &place,
                trap,
                &["\r", "echo first\r", probe, "exit\r"],
            );
            let text = String::from_utf8_lossy(&observed.output);
            let what = format!("{} {}", bash.display(), place.what);
            assert!(
                text.contains(&format!("trap=[{}]", trap.replace("trap '", "trap -- '"))),
                "{what}: {text:?}"
            );
            assert!(
                text.contains("seen=1\r\n"),
                "{what}: the user's trap missed a command: {text:?}"
            );
            let got = marks(observed.payloads.iter().map(Vec::as_slice));
            let mut want = Vec::new();
            if place.typed {
                want.extend(command_marks(true, Some(0)));
            }
            // The empty line: a bare D.
            want.extend(command_marks(false, None));
            want.extend(command_marks(true, Some(0)));
            want.extend(command_marks(true, Some(0)));
            want.extend([Mark::PromptStart, Mark::CommandStart, Mark::OutputStart]);
            assert_eq!(got, want, "{what}: {text:?}");
        }
    }
}

/// A guard DEBUG trap (`extdebug`, a non-zero status skips the command)
/// that refuses `rm` keeps refusing it wherever it was set, and every
/// other command still runs (#542: hand-sourced, the guard was gone).
#[test]
fn bash_guard_debug_trap_keeps_guarding_wherever_it_was_set() {
    let trap = r#"shopt -s extdebug; trap '[[ "$BASH_COMMAND" != rm\ * ]]' DEBUG"#;
    let inputs = [
        "touch kept\r",
        "rm kept\r",
        "[[ -e kept ]] && echo still-$((40+2))\r",
        "exit\r",
    ];
    for bash in bashes() {
        for place in trap_placements(trap) {
            let observed = run_placed(&bash, &place, trap, &inputs);
            let text = String::from_utf8_lossy(&observed.output);
            let what = format!("{} {}", bash.display(), place.what);
            assert!(text.contains("still-42\r\n"), "{what}: {text:?}");
            let got = marks(observed.payloads.iter().map(Vec::as_slice));
            // The refused `rm` ends with 0, as in a plain bash (the status of
            // a skipped command).
            let commands = inputs.len() - 1 + usize::from(place.typed);
            assert_eq!(got, marks_for(&vec![0; commands]), "{what}: {text:?}");
        }
    }
}

/// What bash prints when the hooks feed it broken shell text.
const SHELL_ERRORS: [&str; 4] = [
    "unexpected EOF",
    "syntax error",
    "command not found",
    "bad substitution",
];

/// A DEBUG trap that prints keeps running wherever it was set, also with
/// `set -T` or extdebug (where it runs inside the hooks' read of the trap
/// too), and the hooks never take what it printed for the trap (#542: 5.3
/// read the trap in this shell, the user's trap printed into the read, and
/// the script installed and ran the result). An empty line first (no
/// command fires the trap) changes nothing.
#[test]
fn bash_printing_debug_trap_keeps_running_wherever_it_was_set() {
    let base = r#"trap 'printf "[t:%s]" "${BASH_COMMAND%% *}"' DEBUG"#;
    let shown = base.replace("trap '", "trap -- '");
    let inputs = ["\r", "echo first\r", "trap -p DEBUG\r", "exit\r"];
    for bash in bashes() {
        for prefix in ["", "set -T; ", "shopt -s extdebug; "] {
            let trap = format!("{prefix}{base}");
            for place in trap_placements(&trap) {
                let observed = run_placed(&bash, &place, &trap, &inputs);
                let text = String::from_utf8_lossy(&observed.output);
                let what = format!("{} {} {trap}", bash.display(), place.what);
                for error in SHELL_ERRORS {
                    assert!(!text.contains(error), "{what}: {text:?}");
                }
                assert!(
                    text.contains(&format!("[t:trap]{shown}\r\n")),
                    "{what}: the trap was not kept: {text:?}"
                );
                assert!(
                    text.contains("[t:echo]first\r\n"),
                    "{what}: the user's trap missed a command: {text:?}"
                );
                let got = marks(observed.payloads.iter().map(Vec::as_slice));
                let mut want = Vec::new();
                if place.typed {
                    want.extend(command_marks(true, Some(0)));
                }
                // The empty line: a bare D.
                want.extend(command_marks(false, None));
                want.extend(marks_for(&[0, 0]));
                assert_eq!(got, want, "{what}: {text:?}");
            }
        }
    }
}

/// A DEBUG trap that prints the directory's name in a directory whose name
/// is shell code with a stray quote: the hooks never run the name, print no
/// errors and keep the user's trap, also with `set -T` (the trap runs inside
/// the hooks' read) and when what it prints starts as `trap -p` output does.
#[test]
fn bash_debug_trap_printing_the_directory_runs_no_code_from_its_name() {
    let title = r#"trap 'printf "\033]0;%s\007" "${PWD##*/}"' DEBUG"#;
    let raw = r#"trap 'printf %s "${PWD##*/}"' DEBUG"#;
    let cases = [
        (
            "x;touch PWNED;'",
            "",
            title,
            "\u{1b}]0;x;touch PWNED;'\u{7}",
        ),
        (
            "x;touch PWNED;'",
            "set -T; ",
            title,
            "\u{1b}]0;x;touch PWNED;'\u{7}",
        ),
        (
            "trap -- 'touch PWNED;'",
            "set -T; ",
            raw,
            "trap -- 'touch PWNED;'",
        ),
    ];
    let inputs = [
        "echo first\r",
        "trap -p DEBUG\r",
        "[[ -e PWNED ]] || echo no-$((1+1))-pwned\r",
        "exit\r",
    ];
    for bash in bashes() {
        for (dir, prefix, base, printed) in cases {
            let trap = format!("{prefix}{base}");
            let shown = base.replace("trap '", "trap -- '");
            let setup = format!("mkdir -p \"$HOME/{dir}\" && cd \"$HOME/{dir}\"\n");
            for mut place in trap_placements(&trap) {
                place.bashrc = format!("{setup}{}", place.bashrc);
                let observed = run_placed(&bash, &place, &trap, &inputs);
                let text = String::from_utf8_lossy(&observed.output);
                let what = format!("{} {} [{dir}] {trap}", bash.display(), place.what);
                for error in SHELL_ERRORS {
                    assert!(!text.contains(error), "{what}: {text:?}");
                }
                assert!(
                    text.contains("no-2-pwned\r\n"),
                    "{what}: the name ran: {text:?}"
                );
                assert!(
                    text.contains(&format!("{printed}{shown}\r\n")),
                    "{what}: the trap was not kept: {text:?}"
                );
                assert!(
                    text.contains(&format!("{printed}first\r\n")),
                    "{what}: the user's trap missed a command: {text:?}"
                );
                let got = marks(observed.payloads.iter().map(Vec::as_slice));
                let commands = inputs.len() - 1 + usize::from(place.typed);
                assert_eq!(got, marks_for(&vec![0; commands]), "{what}: {text:?}");
            }
        }
    }
}

/// A DEBUG trap whose status is not zero (a false `&&` list, as in a trap
/// that acts on some commands only) runs for the user's commands without
/// the hooks making it a failure (no ERR trap run for the status, no exit of
/// a `set -e` shell), as in a plain bash (#542: the hooks ran it as a plain
/// command, and ERR ran three times per command). A command failing inside
/// the trap still runs ERR, as in a plain bash.
#[test]
fn bash_failing_debug_trap_status_is_no_hook_failure_and_keeps_a_set_e_shell() {
    let trap =
        r#"trap 'printf "[t:%s]" "${BASH_COMMAND%% *}"; [[ $BASH_COMMAND == ssh* ]] && :' DEBUG"#;
    let inputs = ["echo first\r", "echo alive-$((1+1))\r", "exit\r"];
    for bash in bashes() {
        for setup in ["trap 'echo \"[ERR $?]\"' ERR\n", "set -e\n"] {
            for mut place in trap_placements(trap) {
                place.bashrc = format!("{setup}{}", place.bashrc);
                let observed = run_placed(&bash, &place, trap, &inputs);
                let text = String::from_utf8_lossy(&observed.output);
                let what = format!("{} {} {setup:?}", bash.display(), place.what);
                assert!(!text.contains("[ERR"), "{what}: {text:?}");
                assert!(
                    text.contains("alive-2\r\n"),
                    "{what}: the shell exited: {text:?}"
                );
                assert!(
                    text.contains("[t:echo]first\r\n"),
                    "{what}: the user's trap missed a command: {text:?}"
                );
                let got = marks(observed.payloads.iter().map(Vec::as_slice));
                let commands = inputs.len() - 1 + usize::from(place.typed);
                assert_eq!(got, marks_for(&vec![0; commands]), "{what}: {text:?}");
            }
        }
    }
}

/// zsh reports the directory at every prompt and when it changes, but not
/// for a subshell's or a command substitution's `cd`: after `cd /tmp`,
/// `(cd /usr && true)` and `$(cd /var; pwd)` the terminal's directory is
/// still /tmp.
#[test]
fn zsh_reports_the_shells_directory_not_a_subshells() {
    let observed = run_steps(
        &Case {
            shell: Shell::Zsh,
            program: ZSH.into(),
            args: Some(&["-i"]),
            files: &[(".zshrc", "")],
            env: &[],
        },
        &[
            (Wait::Prompts(1), "cd /tmp\r"),
            (Wait::Prompts(2), "(cd /usr && true)\r"),
            (Wait::Prompts(3), "echo $(cd /var; pwd)\r"),
            (Wait::Prompts(4), "pwd\r"),
            (Wait::Prompts(5), "exit\r"),
        ],
    );
    let paths: Vec<String> = observed
        .payloads
        .iter()
        .filter_map(|p| p.strip_prefix(b"7;"))
        .map(|p| parse_osc7(p).expect("an OSC 7").path)
        .collect();
    assert!(
        !paths
            .iter()
            .any(|path| path == "/usr" || path.ends_with("/var")),
        "{paths:?}"
    );
    assert_eq!(paths.last().map(String::as_str), Some("/tmp"), "{paths:?}");
    // One report per prompt after the `cd` (5 prompts), plus the startup
    // directory and the `cd` itself.
    assert_eq!(
        paths.iter().filter(|path| *path == "/tmp").count(),
        5,
        "{paths:?}"
    );
}
