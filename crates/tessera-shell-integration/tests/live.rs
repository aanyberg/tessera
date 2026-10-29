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
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tessera_grid::Mark;
use tessera_pty::{NativePty, Pty, PtyEvent, PtyEvents, SpawnOptions};
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
}

/// Materialises the scripts, writes the case's startup files and builds the
/// spawn options: the case's program with the injection applied, then
/// `case.env` (which wins over the injection; `$SCRIPTS` in a value is the
/// scripts directory). `$TESSERA_TEST_SCRIPTS` is
/// the scripts directory, for startup files that source a script.
fn prepare(case: &Case<'_>) -> Prepared {
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
    // Tessera does not inject bash yet; its rules are kept in `for_bash`,
    // which these tests use so the bash script stays tested.
    let injection = match case.shell {
        Shell::Bash => Injection::for_bash(&scripts, launch, user_env),
        _ => Injection::for_shell(case.shell, &scripts, launch, user_env),
    };

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
    /// Once the shell has a child process with this name (so a signal
    /// reaches it, not the shell between `C` and the fork).
    Child(&'static str),
}

/// Whether the process `pid` has a child named `name`.
fn has_child(pid: u32, name: &str) -> bool {
    std::process::Command::new("/usr/bin/pgrep")
        .args(["-P", &pid.to_string(), "-x", name])
        .output()
        .is_ok_and(|output| output.status.success())
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

/// Runs the case, typing each input once its wait is over.
fn run_steps(case: &Case<'_>, steps: &[(Wait, &str)]) -> Observed {
    let prepared = prepare(case);
    assert!(
        prepared
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
        let ready = match wait {
            Wait::Prompts(n) => pump(&mut pty, &events, &mut out, |out| {
                count(out, b"\x1b]133;A") >= n
            }),
            Wait::Child(name) => {
                let pid = pty.child_pid().expect("the shell's pid");
                let deadline = Instant::now() + STEP_TIMEOUT;
                while !has_child(pid, name) && Instant::now() < deadline {
                    while let Ok(PtyEvent::Output(bytes)) =
                        events.recv_timeout(Duration::from_millis(20))
                    {
                        out.push(&mut pty, &bytes);
                    }
                }
                has_child(pid, name)
            }
        };
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

/// Fixture rows LY, C1-C3, C5, C6 and S9: an empty line and Ctrl-C at the
/// prompt each close the prompt with a bare D (zsh reports 130 and bash 3.2
/// the previous status there; the bare D hides the difference), and a
/// command interrupted with Ctrl-C ends with D;130.
#[test]
fn empty_lines_and_ctrl_c_close_with_a_bare_d() {
    use Mark::{CommandEnd, CommandStart, OutputStart, PromptStart};
    let steps = [
        (Wait::Prompts(1), "\r"),
        (Wait::Prompts(2), "\x03"),
        (Wait::Prompts(3), "sleep 20\r"),
        (Wait::Child("sleep"), "\x03"),
        (Wait::Prompts(4), "exit\r"),
    ];
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
        let got = marks(observed.payloads.iter().map(Vec::as_slice));
        assert_eq!(got, want, "{shell:?} {program:?}");
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

/// Whether `bash` is 5.1 or later (runs every element of an array
/// `PROMPT_COMMAND`).
fn bash_runs_prompt_command_arrays(bash: &std::path::Path) -> bool {
    let output = std::process::Command::new(bash)
        .args(["-c", "echo ${BASH_VERSINFO[0]} ${BASH_VERSINFO[1]}"])
        .output()
        .expect("run bash");
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text
        .split_whitespace()
        .map(|n| n.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    major > 5 || (major == 5 && minor >= 1)
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

/// bash 5.1 and later run every element of an array `PROMPT_COMMAND`: the
/// user's elements (set in `.bashrc`, or added at a prompt) keep running and
/// ours stay around them, so every command gets exactly one C.
#[test]
fn bash_prompt_command_arrays() {
    let arrays: Vec<PathBuf> = bashes()
        .into_iter()
        .filter(|bash| bash_runs_prompt_command_arrays(bash))
        .collect();
    if arrays.is_empty() {
        eprintln!("skipped: no bash 5.1 or later installed (looked in {BASH5:?})");
        return;
    }
    for bash in arrays {
        let observed = run_steps(
            &Case {
                shell: Shell::Bash,
                program: bash.clone(),
                args: Some(&["-i"]),
                files: &[(
                    ".bashrc",
                    "PROMPT_COMMAND=('pc_one=1')\nPROMPT_COMMAND+=('pc_two=2')\n",
                )],
                env: &[],
            },
            &[
                (Wait::Prompts(1), "unset pc_one pc_two\r"),
                (Wait::Prompts(2), "echo \"pc=$pc_one$pc_two\"\r"),
                (Wait::Prompts(3), "PROMPT_COMMAND+=('pc_three=3')\r"),
                (Wait::Prompts(4), "unset pc_three\r"),
                (Wait::Prompts(5), "echo \"pc=$pc_three\"\r"),
                (Wait::Prompts(6), "exit\r"),
            ],
        );
        let text = String::from_utf8_lossy(&observed.output);
        assert!(text.contains("pc=12\r\n"), "{}: {text:?}", bash.display());
        assert!(text.contains("pc=3\r\n"), "{}: {text:?}", bash.display());
        // Six commands, each A B C D;0 (the last, `exit`, without D): a
        // user element running after ours would put its C before an A.
        // The one exception: bash runs a copy of the array taken when the
        // prompt starts, so at the prompt right after `PROMPT_COMMAND+=`
        // the new element still runs after ours; the script does not arm
        // its trap then, so that command (the fourth) has no C and ends
        // with a bare D.
        let mut want = Vec::new();
        for command in 0..6 {
            want.extend([Mark::PromptStart, Mark::CommandStart]);
            if command == 3 {
                want.push(Mark::CommandEnd { exit_code: None });
                continue;
            }
            want.push(Mark::OutputStart);
            if command < 5 {
                want.push(Mark::CommandEnd { exit_code: Some(0) });
            }
        }
        let got = marks(observed.payloads.iter().map(Vec::as_slice));
        assert_eq!(got, want, "{}: {text:?}", bash.display());
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
    for bash in bashes() {
        for integrated in [false, true] {
            let mut env = vec![];
            if !integrated {
                env.push(("TESSERA_SHELL_INTEGRATION", "0"));
            }
            let output = if integrated {
                run_steps(
                    &Case {
                        shell: Shell::Bash,
                        program: bash.clone(),
                        args: Some(&["-i"]),
                        files: &[(".bashrc", bashrc)],
                        env: &env,
                    },
                    &[
                        (Wait::Prompts(1), "false\r"),
                        (Wait::Prompts(2), "echo two\r"),
                        (Wait::Prompts(3), "exit\r"),
                    ],
                )
                .output
            } else {
                run_raw(
                    &Case {
                        shell: Shell::Bash,
                        program: bash.clone(),
                        args: Some(&["-i"]),
                        files: &[(".bashrc", bashrc)],
                        env: &env,
                    },
                    "false\recho two\rexit\r",
                )
            };
            let text = String::from_utf8_lossy(&output);
            let what = format!("{} integrated={integrated}", bash.display());
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
