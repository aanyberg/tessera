//! The wave 27 rows of `tests/fixtures/bash_spec.txt` (#158, #540), one test
//! per row: bash through Tessera's injection in a real PTY against the same
//! bash started without it, on the macOS `/bin/bash` (3.2) and a bash 5 when
//! one is installed (Homebrew's; skipped with a message otherwise).
//!
//! - `pipestatus`: `$?`, `PIPESTATUS` and `$_` after the hooks, on the
//!   `--rcfile` and the login route;
//! - `pc`: appends to `PROMPT_COMMAND` in `.bash_profile` (the loader's
//!   terminator, B5, and what the entries after it see, B6);
//! - `rcfile`: launches whose `--rcfile` some bash does not read leave no
//!   `TESSERA_` variable in the shell (M10);
//! - `hold` and `scan` name the tests elsewhere that cover them; a test here
//!   checks that those exist.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tessera_pty::{NativePty, Pty, PtyEvent, SpawnOptions};
use tessera_shell_integration::scripts::materialise;
use tessera_shell_integration::{Injection, Launch, Shell};

const SPEC: &str = include_str!("fixtures/bash_spec.txt");
/// The longest wait for one prompt or for the shell to exit.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// Every row's prompt.
const PROMPT: &str = "P> ";
const NEWER_BASH: [&str; 2] = ["/opt/homebrew/bin/bash", "/usr/local/bin/bash"];
/// A message bash 3.2's job control prints now and then on a loaded Mac.
const JOB_CONTROL_NOISE: &str = "child setpgid (";

/// The bashes a row's `bashes` field names that are installed: `3.2` is
/// `/bin/bash`, `5.3` the first bash 5 of [`NEWER_BASH`].
fn bashes(field: &str, id: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for version in field.split(' ') {
        match version {
            "3.2" => found.push(PathBuf::from("/bin/bash")),
            "5.3" => match NEWER_BASH
                .iter()
                .map(PathBuf::from)
                .find(|bash| major(bash) >= Some(5))
            {
                Some(bash) => found.push(bash),
                None => eprintln!("{id}: bash 5 part skipped: none in {NEWER_BASH:?}"),
            },
            other => panic!("{id}: unknown bash {other:?}"),
        }
    }
    found
}

fn major(program: &Path) -> Option<u32> {
    let output = std::process::Command::new(program)
        .args(["-c", "echo ${BASH_VERSINFO[0]}"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// The fixture row `id` of `kind`, its fields after the id.
fn row(kind: &str, id: &str) -> Vec<String> {
    let found: Vec<Vec<String>> = SPEC
        .lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| line.split(" | ").map(unescape).collect::<Vec<_>>())
        .filter(|fields| fields.len() > 2 && fields[0] == kind && fields[1] == id)
        .map(|fields| fields[2..].to_vec())
        .collect();
    assert_eq!(found.len(), 1, "{kind} row {id}");
    found.into_iter().next().unwrap()
}

/// The ids of every row of `kind`.
fn ids(kind: &str) -> Vec<&'static str> {
    SPEC.lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split(" | ");
            (fields.next() == Some(kind)).then(|| fields.next().unwrap())
        })
        .collect()
}

/// `\n` a newline, `\|` a pipe, `\\` a backslash.
fn unescape(field: &str) -> String {
    let mut out = String::new();
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some(c @ ('|' | '\\')) => out.push(c),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// How the bash starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// `bash -i`: the `--rcfile` route; the startup text is `.bashrc`.
    Rcfile,
    /// `bash -l` as Tessera's login shell: the loader; the startup text is
    /// `.bash_profile`.
    Login,
}

/// One bash run: the injected one, or the same bash without the injection.
struct Run<'a> {
    program: &'a Path,
    route: Route,
    injected: bool,
    startup: &'a str,
    /// Arguments in place of the route's (`-i` / `-l`), for [`Route::Rcfile`].
    args: Option<&'a [&'a str]>,
    env: &'a [(&'a str, &'a str)],
    /// Types every line at once, without waiting for prompts.
    ahead: bool,
}

/// What a run printed, and whether the injection changed anything.
struct Output {
    text: String,
    injected: bool,
}

impl Run<'_> {
    /// Types each line at its prompt, then `exit`; returns everything the
    /// shell printed. A shell that exits early (`set -e`) ends the run.
    #[expect(clippy::too_many_lines, reason = "one run, start to exit")]
    fn lines(&self, lines: &[&str]) -> Output {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let scripts = materialise(&root.join("scripts")).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let file = match self.route {
            Route::Rcfile => ".bashrc",
            Route::Login => ".bash_profile",
        };
        std::fs::write(
            home.join(file),
            format!("{}\nPS1='{PROMPT}'\n", self.startup),
        )
        .unwrap();
        let user_args: Vec<OsString> = match (self.route, self.args) {
            (_, Some(args)) => args.iter().map(OsString::from).collect(),
            (Route::Rcfile, None) => vec!["-i".into()],
            (Route::Login, None) => vec!["-l".into()],
        };
        let home_os = home.clone().into_os_string();
        let env = self.env;
        let injection = if self.injected {
            let launch = match (self.route, self.args) {
                (Route::Login, None) => Launch::Login,
                _ => Launch::Args(&user_args),
            };
            Injection::for_shell(Shell::Bash, &scripts, launch, |key| {
                if key == "HOME" {
                    return Some(home_os.clone());
                }
                env.iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.into())
            })
        } else {
            Injection::default()
        };
        let args = injection.args.clone().unwrap_or(user_args);
        let home_str = home.to_str().unwrap();
        // PROMPT_COMMAND unset, as outside Tessera, unless the injection
        // sets it (login route).
        let mut command: Vec<OsString> = Vec::new();
        if !injection.env.iter().any(|(key, _)| key == "PROMPT_COMMAND") {
            command.extend(["-u".into(), "PROMPT_COMMAND".into()]);
        }
        command.push(self.program.as_os_str().to_os_string());
        command.extend(args);
        let mut options = SpawnOptions::command("/usr/bin/env", "bash-rows")
            .args(command)
            .cwd(&home);
        for (key, value) in [
            ("HOME", home_str),
            ("PWD", home_str),
            ("HISTFILE", &format!("{home_str}/.history")),
            ("TERM", "xterm-256color"),
            ("LANG", "en_US.UTF-8"),
            ("PS1", PROMPT),
            ("ENV", ""),
            ("BASH_ENV", ""),
            ("BASH_SILENCE_DEPRECATION_WARNING", "1"),
        ] {
            options = options.env(key, value);
        }
        for (key, value) in self.env {
            options = options.env(key, value);
        }
        for (key, value) in &injection.env {
            options = options.env(key, value);
        }
        let (mut pty, events) = NativePty::spawn(&options).expect("spawn bash");
        let mut output = Vec::new();
        let mut exited = false;
        let read_until = |output: &mut Vec<u8>, done: &dyn Fn(&[u8]) -> bool| -> bool {
            let deadline = Instant::now() + STEP_TIMEOUT;
            while !done(output) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "no prompt within {STEP_TIMEOUT:?}: {:?}",
                    String::from_utf8_lossy(output)
                );
                match events.recv_timeout(remaining) {
                    Ok(PtyEvent::Output(bytes)) => output.extend_from_slice(&bytes),
                    Err(RecvTimeoutError::Timeout) => {}
                    Ok(PtyEvent::Exited(_)) | Err(RecvTimeoutError::Disconnected) => return false,
                }
            }
            true
        };
        for (n, line) in lines.iter().chain(&["exit"]).enumerate() {
            // Typed ahead: the M10 launches may not show `PROMPT`.
            let prompts = |output: &[u8]| self.ahead || count(output, PROMPT.as_bytes()) > n;
            if !read_until(&mut output, &prompts) {
                exited = true;
                break;
            }
            pty.write(line.as_bytes()).expect("type a line");
            pty.write(b"\r").expect("type a line");
        }
        if !exited {
            read_until(&mut output, &|_| false);
        }
        Output {
            text: String::from_utf8_lossy(&output).into_owned(),
            injected: !injection.is_empty(),
        }
    }
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// The lines a run printed that are not a prompt (with the typed line
/// after it), with escape sequences and carriage returns removed.
fn transcript(text: &str) -> Vec<String> {
    let mut plain = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                Some('[') => while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {},
                _ => {}
            },
            '\r' | '\x07' => {}
            _ => plain.push(c),
        }
    }
    plain
        .lines()
        .filter(|line| !line.starts_with(PROMPT) && !line.trim().is_empty())
        .filter(|line| !line.contains(JOB_CONTROL_NOISE))
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .skip_while(|line| line == "exit" || line == "logout")
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Checks a row's `expected` field against the injected and the base
/// transcripts.
fn check(what: &str, expected: &str, injected: &Output, base: &Output) -> Result<(), String> {
    let got = transcript(&injected.text);
    let base_lines = transcript(&base.text);
    let fail = |want: &dyn std::fmt::Debug| {
        Err(format!(
            "{what}: got {got:?}, want {want:?}, base {base_lines:?}: {:?}",
            injected.text
        ))
    };
    if let Some(lines) = expected.strip_prefix("lines: ") {
        let want: Vec<&str> = lines.split(" / ").collect();
        return if got == want { Ok(()) } else { fail(&want) };
    }
    if let Some(limit) = expected.strip_prefix("limit: ") {
        let want: Vec<&str> = limit.split(" / ").collect();
        if got != want {
            return fail(&want);
        }
        if got == base_lines {
            return Err(format!("{what}: the limit is gone; make the row `base`"));
        }
        return Ok(());
    }
    if let Some(word) = expected.strip_prefix("base ignoring ") {
        let without = |lines: &[String]| -> Vec<String> {
            lines.iter().filter(|line| *line != word).cloned().collect()
        };
        let words = |lines: &[String]| lines.iter().filter(|line| *line == word).count();
        if without(&got) != without(&base_lines) || words(&got) < words(&base_lines) {
            return fail(&base_lines);
        }
        return Ok(());
    }
    let skip = if let Some(n) = expected.strip_prefix("base after ") {
        n.parse().unwrap()
    } else {
        assert_eq!(expected, "base", "{what}");
        0
    };
    let skip_of = |lines: &[String]| {
        lines
            .get(skip.min(lines.len())..)
            .unwrap_or_default()
            .to_vec()
    };
    if skip_of(&got) == skip_of(&base_lines) {
        Ok(())
    } else {
        fail(&base_lines)
    }
}

/// Panics with every failure of a row.
fn report(id: &str, failures: &[String]) {
    assert!(failures.is_empty(), "{id}:\n{}", failures.join("\n"));
}

fn pipestatus_row(id: &str) {
    let fields = row("pipestatus", id);
    let [versions, commands, expected] = &fields[..] else {
        panic!("{id}: {fields:?}")
    };
    run_row(
        id,
        versions,
        "",
        commands,
        expected,
        &[Route::Rcfile, Route::Login],
    );
}

fn pc_row(id: &str) {
    let fields = row("pc", id);
    let [versions, profile, commands, expected] = &fields[..] else {
        panic!("{id}: {fields:?}")
    };
    run_row(id, versions, profile, commands, expected, &[Route::Login]);
}

/// Runs a row on each bash it names and each route, injected and not.
fn run_row(
    id: &str,
    versions: &str,
    startup: &str,
    commands: &str,
    expected: &str,
    routes: &[Route],
) {
    let lines: Vec<&str> = commands.split('\n').collect();
    let mut failures = Vec::new();
    for program in bashes(versions, id) {
        for &route in routes {
            let run = |injected| {
                Run {
                    program: &program,
                    route,
                    injected,
                    startup,
                    args: None,
                    env: &[],
                    ahead: false,
                }
                .lines(&lines)
            };
            let what = format!("{id} {} {route:?}", program.display());
            let injected = run(true);
            if startup.is_empty() && !injected.injected {
                failures.push(format!("{what}: nothing injected"));
            }
            if let Err(failure) = check(&what, expected, &injected, &run(false)) {
                failures.push(failure);
            }
        }
    }
    report(id, &failures);
}

/// M10: the launch's shell has no `TESSERA_` variable.
fn rcfile_row(id: &str) {
    let fields = row("rcfile", id);
    let [env, args] = &fields[..] else {
        panic!("{id}: {fields:?}")
    };
    let env: Vec<(&str, &str)> = match env.as_str() {
        "-" => Vec::new(),
        pair => vec![pair.split_once('=').unwrap()],
    };
    let args: Vec<&str> = args.split(' ').collect();
    for program in bashes("3.2 5.3", id) {
        let output = Run {
            program: &program,
            route: Route::Rcfile,
            injected: true,
            startup: "",
            args: Some(&args),
            env: &env,
            ahead: true,
        }
        .lines(&["env | grep ^TESSERA_; echo END-$((40+2))"]);
        let what = format!("{id} {} {env:?} {args:?}", program.display());
        assert!(output.injected, "{what}: nothing injected");
        assert!(output.text.contains("END-42"), "{what}: {:?}", output.text);
        let leaked: Vec<String> = transcript(&output.text)
            .into_iter()
            .filter(|line| {
                line.starts_with("TESSERA_SHELL_INTEGRATION") || line.starts_with("TESSERA_BASH_")
            })
            .collect();
        assert!(leaked.is_empty(), "{what}: {leaked:?}");
    }
}

macro_rules! row_tests {
    ($runner:ident: $($name:ident => $id:literal),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                $runner($id);
            }
        )*
    };
}

mod pipestatus {
    use super::*;
    pub(super) const TESTED: &[&str] = &[
        "P1", "P2", "P3", "P4", "P5", "P6", "P7", "P8", "P9", "P10", "P11", "P12", "P13", "P14",
        "P15", "P15b", "P16", "P17", "G1", "G1b",
    ];
    row_tests!(pipestatus_row:
        p1 => "P1", p2 => "P2", p3 => "P3", p4 => "P4", p5 => "P5", p6 => "P6", p7 => "P7",
        p8 => "P8", p9 => "P9", p10 => "P10", p11 => "P11", p12 => "P12", p13 => "P13",
        p14 => "P14", p15 => "P15", p15b => "P15b", p16 => "P16", p17 => "P17", g1 => "G1",
        g1b => "G1b",
    );
}

mod pc {
    use super::*;
    pub(super) const TESTED: &[&str] = &[
        "B5a", "B5b", "B5c", "B5d", "B5e", "B5f", "B5g", "B5h", "B5i", "B5j", "B5k", "B5l", "B5m",
        "B6a", "B6c", "B6e",
    ];
    row_tests!(pc_row:
        b5a => "B5a", b5b => "B5b", b5c => "B5c", b5d => "B5d", b5e => "B5e", b5f => "B5f",
        b5g => "B5g", b5h => "B5h", b5i => "B5i", b5j => "B5j", b5k => "B5k", b5l => "B5l",
        b5m => "B5m", b6a => "B6a", b6c => "B6c",
        b6e => "B6e",
    );
}

mod rcfile {
    use super::*;
    pub(super) const TESTED: &[&str] = &[
        "R1", "R2", "R3", "R4", "R5", "R6", "R7", "R8", "R9", "R10", "R11",
    ];
    row_tests!(rcfile_row:
        r1 => "R1", r2 => "R2", r3 => "R3", r4 => "R4", r5 => "R5", r6 => "R6", r7 => "R7",
        r8 => "R8", r9 => "R9", r10 => "R10", r11 => "R11",
    );
}

#[test]
fn every_row_has_its_test() {
    for (kind, tested) in [
        ("pipestatus", pipestatus::TESTED),
        ("pc", pc::TESTED),
        ("rcfile", rcfile::TESTED),
    ] {
        assert_eq!(ids(kind), tested, "{kind}");
    }
    for (kind, source) in [
        ("hold", include_str!("live.rs")),
        ("scan", include_str!("../src/bash_startup.rs")),
    ] {
        let tests: Vec<&str> = SPEC
            .lines()
            .filter_map(|line| line.strip_prefix(kind)?.strip_prefix(" | "))
            .filter_map(|rest| rest.split(" | ").nth(1))
            .collect();
        assert!(!tests.is_empty(), "{kind}");
        for test in tests {
            assert!(
                source.contains(&format!("fn {test}()")),
                "{kind}: no test {test}"
            );
        }
    }
}

/// P18: bash 4 and later keep `PIPESTATUS` around the DEBUG trap
/// themselves, so with no `PROMPT_COMMAND` entries of the user's the hooks
/// restore nothing there and fork for nothing: every `_tessera_si_rv` (the
/// restoring pipelines' command, here logging `$BASHPID`) runs in the shell
/// itself. With an entry, the head's restore for it forks once per pipeline
/// command (3 here), which also shows that the log sees forks. On bash 3.2
/// the line naming PIPESTATUS is restored once.
#[test]
fn p18_bash_4_restores_nothing() {
    let log = "_tessera_si_rv() { echo \"$BASHPID\" >> \"$HOME/rv\"; builtin return \"$1\"; }; \
               _tessera_si_ri() { echo \"$BASHPID\" >> \"$HOME/rv\"; builtin return \"${_tessera_si_ps[$1]}\"; }";
    let forked = "echo \"forked=$(grep -cvx \"$$\" \"$HOME/rv\")\"";
    for (program, restores) in bashes("3.2", "P18")
        .into_iter()
        .map(|bash| (bash, "1"))
        .chain(bashes("5.3", "P18").into_iter().map(|bash| (bash, "0")))
    {
        let newer = restores == "0";
        for route in [Route::Rcfile, Route::Login] {
            let mut lines = vec![
                "true | false | true",
                "echo \"ps=${PIPESTATUS[*]}\"; echo \"R=$_tessera_si_restores\"",
            ];
            let mut expected = vec!["ps=0 1 0".to_owned(), format!("R={restores}")];
            if newer {
                lines.extend([
                    log,
                    "true | false | true",
                    forked,
                    "PROMPT_COMMAND+=$'\\n'true",
                    "true | false | true",
                    forked,
                ]);
                expected.extend(["forked=0".to_owned(), "forked=3".to_owned()]);
            }
            let output = Run {
                program: &program,
                route,
                injected: true,
                startup: "",
                args: None,
                env: &[],
                ahead: false,
            }
            .lines(&lines);
            let what = format!("{} {route:?}", program.display());
            assert_eq!(
                transcript(&output.text),
                expected,
                "{what}: {:?}",
                output.text
            );
        }
    }
}

/// B5: once loaded, the user's entries that followed the loader with a
/// separator start with their command: the `;`, `&&` and blanks that tied
/// them to the loader are gone (a leading `;` or `&&` is a syntax error at
/// every later prompt).
#[test]
fn b5_loaded_entries_lose_the_separator() {
    let show = "pc=${PROMPT_COMMAND#\"$_tessera_si_head\"}; printf '[%s]\\n' \"${pc%\"$_tessera_si_tail\"}\"";
    let mut failures = Vec::new();
    for program in bashes("3.2 5.3", "B5") {
        for startup in [
            "PROMPT_COMMAND=\"$PROMPT_COMMAND; echo USER_PC\"",
            "PROMPT_COMMAND=\"$PROMPT_COMMAND ; echo USER_PC\"",
            "PROMPT_COMMAND=\"${PROMPT_COMMAND:+$PROMPT_COMMAND;}echo USER_PC\"",
            "PROMPT_COMMAND+=';echo USER_PC'",
            "PROMPT_COMMAND=\"$PROMPT_COMMAND && echo USER_PC\"",
            "PROMPT_COMMAND=\"$PROMPT_COMMAND || echo USER_PC\"",
        ] {
            let output = Run {
                program: &program,
                route: Route::Login,
                injected: true,
                startup,
                args: None,
                env: &[],
                ahead: false,
            }
            .lines(&[show]);
            let shown: Vec<String> = transcript(&output.text)
                .into_iter()
                .filter(|line| line.starts_with('['))
                .collect();
            if !output.injected || shown != ["[echo USER_PC]"] {
                failures.push(format!(
                    "{} {startup:?}: {shown:?} {:?}",
                    program.display(),
                    output.text
                ));
            }
        }
    }
    report("B5 loaded", &failures);
}

/// P18 under `set -T` (the DEBUG trap runs inside functions), with a user
/// entry function from the startup file: the 3.2 restore for the user's
/// entries runs before the commands of that function, never before a
/// command inside one of the hooks; bash 5.x forks only for the head's
/// restore. Each `_tessera_si_rv` logs the pid of the shell running it (a
/// child `sh`'s `$PPID`, also on 3.2, which has no `$BASHPID`); the pids
/// other than the shell's own are the forks, 3 per restore of
/// `true | false | true`. On 3.2 that is 5 restores: two for the call of
/// `f` (before it and at its start) and one per command `f` runs (3); the
/// head restores nothing there (the trap restores before each entry
/// command). Without the `FUNCNAME` check in the trap text, the hooks' own
/// functions restore too. The entry still sees the user's `PIPESTATUS`.
#[test]
fn p18_set_t_forks_for_the_users_commands_only() {
    let startup =
        "set -T\nf() { echo \"ps=${PIPESTATUS[*]}\"; true; true; }\nPROMPT_COMMAND+=$'\\nf'";
    let log = "_tessera_si_rv() { /bin/sh -c 'echo $PPID' >> \"$HOME/rv\"; builtin return \"$1\"; }; \
               _tessera_si_ri() { /bin/sh -c 'echo $PPID' >> \"$HOME/rv\"; \
               builtin return \"${_tessera_si_ps[$1]}\"; }";
    let forks = "echo \"forks=$(sort -u \"$HOME/rv\" | grep -cvx \"$$\")\"";
    let mut failures = Vec::new();
    for (program, expected) in bashes("3.2", "P18")
        .into_iter()
        .map(|bash| (bash, "forks=15"))
        .chain(
            bashes("5.3", "P18")
                .into_iter()
                .map(|bash| (bash, "forks=3")),
        )
    {
        for route in [Route::Rcfile, Route::Login] {
            let output = Run {
                program: &program,
                route,
                injected: true,
                startup,
                args: None,
                env: &[],
                ahead: false,
            }
            .lines(&[log, ": > \"$HOME/rv\"; true | false | true", forks]);
            let shown: Vec<String> = transcript(&output.text)
                .into_iter()
                .filter(|line| line.starts_with("forks=") || line.starts_with("ps="))
                .collect();
            let want = ["ps=0", "ps=0", "ps=0 1 0", expected, "ps=0"];
            if !output.injected || shown != want {
                failures.push(format!("{} {route:?}: {shown:?}", program.display()));
            }
        }
    }
    report("P18 set -T", &failures);
}

/// B6 for an entry added at a prompt (after the tail, which then moves
/// back to the end): from the next prompt on it sees the user's command's
/// `$?` and `PIPESTATUS` as without Tessera, in each way of appending, on
/// both bashes and both routes. A `:` left where the tail was reset both.
#[test]
fn entries_added_at_a_prompt_see_the_users_status() {
    let define = "rc() { return $1; }; f() { echo \"PSEEN e=$? ps=${PIPESTATUS[*]}\"; }";
    let mut failures = Vec::new();
    for (versions, append) in [
        ("3.2 5.3", "PROMPT_COMMAND+=$'\\n'f"),
        (
            "3.2 5.3",
            "PROMPT_COMMAND=\"${PROMPT_COMMAND:+$PROMPT_COMMAND; }f\"",
        ),
        ("5.3", "PROMPT_COMMAND+=(f)"),
    ] {
        let lines = [
            define,
            append,
            "rc 7",
            "true",
            "true | false | true",
            "rc 3",
            "true",
        ];
        for program in bashes(versions, "B6") {
            for route in [Route::Rcfile, Route::Login] {
                let run = |injected| {
                    Run {
                        program: &program,
                        route,
                        injected,
                        startup: "",
                        args: None,
                        env: &[],
                        ahead: false,
                    }
                    .lines(&lines)
                };
                let injected = run(true);
                let what = format!("{append:?} {} {route:?}", program.display());
                if !injected.injected {
                    failures.push(format!("{what}: nothing injected"));
                }
                if let Err(failure) = check(&what, "base", &injected, &run(false)) {
                    failures.push(failure);
                }
            }
        }
    }
    report("B6 at a prompt", &failures);
}

/// Our sequences in a run's output: the handshake, OSC 133 and OSC 7.
fn our_sequences(text: &str) -> usize {
    ["\x1b]1337;", "\x1b]133;", "\x1b]7;"]
        .iter()
        .map(|start| text.matches(start).count())
        .sum()
}

/// What a line that prints `_tessera_si_off` shows; [`transcript`]s are
/// compared without it (bash without Tessera has none).
const OFF_LINE: &str = "echo \"OFF[${_tessera_si_off-}]\"";

/// An array `PROMPT_COMMAND` at startup (bash 5.1+, the lead decision after
/// #158 r4, #613) leaves the shell as it is without Tessera: none of our
/// sequences, the transcript of bash without Tessera (every element, also
/// ones added at a prompt, sees the user's `$?` and `PIPESTATUS`), the same
/// `PROMPT_COMMAND` and no DEBUG trap at the end, and `_tessera_si_off`
/// saying why on the `--rcfile` route (on the login route the assignment
/// drops the loader: the script never loads, B5h). (bash-preexec's array:
/// `bash_tools.rs`, the `off` rows.)
#[test]
fn an_array_prompt_command_is_left_alone() {
    let define = "rc() { return $1; }\n\
                  st() { echo \"PSEEN st e=$? ps=${PIPESTATUS[*]}\"; }\n\
                  f1() { echo \"PSEEN f1 e=$? ps=${PIPESTATUS[*]}\"; }\n\
                  f2() { echo \"PSEEN f2 e=$? ps=${PIPESTATUS[*]}\"; }\n";
    let lines = [
        "rc 7",
        "false",
        "rc 3",
        "PROMPT_COMMAND+=(f1 f2)",
        "rc 7",
        "true | false | true",
        "rc 3",
        OFF_LINE,
        "declare -p PROMPT_COMMAND; trap -p DEBUG",
    ];
    let mut failures = Vec::new();
    for program in bashes("5.3", "B2") {
        for extra in [
            "PROMPT_COMMAND=('history -a' st)",
            "PROMPT_COMMAND=()",
            "PROMPT_COMMAND=(st)\nset -u",
        ] {
            let startup = format!("{define}{extra}");
            for route in [Route::Rcfile, Route::Login] {
                let run = |injected| {
                    Run {
                        program: &program,
                        route,
                        injected,
                        startup: &startup,
                        args: None,
                        env: &[],
                        ahead: false,
                    }
                    .lines(&lines)
                };
                let injected = run(true);
                let base = run(false);
                let what = format!("{extra:?} {} {route:?}", program.display());
                let why = if route == Route::Rcfile {
                    "OFF[array]"
                } else {
                    "OFF[]"
                };
                let mut got = transcript(&injected.text);
                let off = got.iter().position(|line| line.starts_with("OFF["));
                let off = off.map(|at| got.remove(at));
                let mut want = transcript(&base.text);
                want.retain(|line| !line.starts_with("OFF["));
                if our_sequences(&injected.text) != 0
                    || got != want
                    || off.as_deref() != Some(why)
                    || injected.text.contains("unbound")
                {
                    failures.push(format!(
                        "{what}: got {got:?} ({off:?}), base {want:?}: {:?}",
                        injected.text
                    ));
                }
            }
        }
    }
    report("arrays left alone", &failures);
}

/// The bytes of ours before a string `PROMPT_COMMAND`'s first user entry
/// on bash 3.2, at most (what the script ships): bash-preexec 0.7.0's
/// sanitizer slows down sharply with the text before its install `:`
/// (R2-B2), so the head is one short line (main's 260 bytes stalled
/// `. bash-preexec.sh` for 7 to 11 s; this one for under 1 s).
const HEAD_MAX_3: usize = 71;
/// [`HEAD_MAX_3`] on bash 4 and later: the line plus the compact
/// `PIPESTATUS` case. bash-preexec on 5.1+ appends its install string as
/// an element of its own, which the sanitizer handles apart from this
/// one; on 4.x and 5.0 it sanitizes the whole string, 0.09 s for this
/// head (measured on 5.3).
const HEAD_MAX_5: usize = 2026;

/// The head the shipped script puts before the user's entries stays within
/// [`HEAD_MAX_3`] and [`HEAD_MAX_5`], so `. bash-preexec.sh` at a prompt
/// stays fast.
#[test]
fn the_head_before_the_users_entries_stays_short() {
    let string = "PROMPT_COMMAND='history -a'";
    let measure = "p=${PROMPT_COMMAND%%history -a*}; echo \"HEADLEN ${#p}\"";
    let mut failures = Vec::new();
    let mut cases = Vec::new();
    for bash in bashes("3.2", "B6") {
        cases.push((bash, HEAD_MAX_3));
    }
    for bash in bashes("5.3", "B6") {
        cases.push((bash, HEAD_MAX_5));
    }
    for (program, max) in cases {
        let output = Run {
            program: &program,
            route: Route::Rcfile,
            injected: true,
            startup: string,
            args: None,
            env: &[],
            ahead: false,
        }
        .lines(&[measure]);
        let shown: Vec<String> = transcript(&output.text)
            .into_iter()
            .filter(|line| line.starts_with("HEADLEN "))
            .collect();
        let len = shown
            .first()
            .and_then(|line| line.strip_prefix("HEADLEN "))
            .and_then(|word| word.parse::<usize>().ok());
        let ok =
            output.injected && shown.len() == 1 && len.is_some_and(|len| (1..=max).contains(&len));
        if !ok {
            failures.push(format!(
                "{} {string}: {shown:?} (at most {max})",
                program.display()
            ));
        }
    }
    report("head length", &failures);
}

/// The `OSC 133;D` marks in a run's output, in order: the status, or `-`
/// for a bare D.
fn d_marks(text: &str) -> Vec<String> {
    text.split("\x1b]133;D")
        .skip(1)
        .map(|rest| {
            rest.strip_prefix(';').map_or_else(
                || "-".to_owned(),
                |rest| rest.chars().take_while(char::is_ascii_digit).collect(),
            )
        })
        .collect()
}

/// B1 (adversarial r3): a `set -u` startup file is integrated on both
/// bashes and both routes: no unbound-variable error, the transcript bash
/// prints without Tessera, and each command's status. (An array
/// `PROMPT_COMMAND` under `set -u`: [`an_array_prompt_command_is_left_alone`].)
#[test]
fn set_u_shells_are_integrated() {
    let define = "rc() { return $1; }\nf() { echo \"PSEEN f e=$?\"; }\nset -u";
    let mut cases = Vec::new();
    for route in [Route::Rcfile, Route::Login] {
        for bash in bashes("3.2 5.3", "B1") {
            cases.push((bash, route, ""));
        }
    }
    let mut failures = Vec::new();
    for (program, route, extra) in cases {
        let startup = format!("{define}{extra}");
        let run = |injected| {
            Run {
                program: &program,
                route,
                injected,
                startup: &startup,
                args: None,
                env: &[],
                ahead: false,
            }
            .lines(&["rc 7", "true"])
        };
        let injected = run(true);
        let base = run(false);
        let got = transcript(&injected.text);
        let marks = d_marks(&injected.text);
        if !injected.injected
            || injected.text.contains("unbound")
            || got != transcript(&base.text)
            || marks != ["7", "0"]
        {
            failures.push(format!(
                "{} {route:?} {extra:?}: {got:?} D {marks:?}: {:?}",
                program.display(),
                injected.text
            ));
        }
    }
    report("B1 set -u", &failures);
}

/// R4-B1 (adversarial r4): an entry put before our head (`PROMPT_COMMAND=
/// "cmd; $PROMPT_COMMAND"`, at a prompt or from a `.bashrc` sourced again)
/// runs before the head reads `$?`, so that command's D is bare, never the
/// entry's status; the tail puts the head first again, and from the next
/// prompt every command gets its own `D;<status>`. The repro's two cases,
/// on both bashes: `. ~/.bashrc` (its last line fails: status 1, once sent
/// as `D;0`) and a `[ -n "$TMUX" ] && ...` entry (status 0, once `D;1`).
#[test]
fn an_entry_put_before_our_head_never_sends_a_wrong_status() {
    let define = "rc() { return $1; }\n";
    let bashrc = "PROMPT_COMMAND=\"history -a; ${PROMPT_COMMAND}\"\n\
                  [ -f ~/.bash_aliases ] && . ~/.bash_aliases";
    let tmux = "PROMPT_COMMAND=\"[ -n \\\"$TMUX\\\" ] && echo tmux; $PROMPT_COMMAND\"";
    let cases: [(&str, Route, &[&str], &[&str]); 3] = [
        (
            bashrc,
            Route::Rcfile,
            &["rc 5", ". ~/.bashrc", "rc 3", ". ~/.bashrc", "true"],
            &["5", "-", "3", "-", "0"],
        ),
        (
            "",
            Route::Rcfile,
            &["rc 5", tmux, "rc 3", "true"],
            &["5", "-", "3", "0"],
        ),
        (
            "",
            Route::Login,
            &["rc 5", tmux, "rc 3", "true"],
            &["5", "-", "3", "0"],
        ),
    ];
    let mut failures = Vec::new();
    for program in bashes("3.2 5.3", "R4-B1") {
        for (extra, route, lines, marks) in cases {
            let startup = format!("{define}{extra}");
            let run = |injected| {
                Run {
                    program: &program,
                    route,
                    injected,
                    startup: &startup,
                    args: None,
                    env: &[],
                    ahead: false,
                }
                .lines(lines)
            };
            let injected = run(true);
            let got = transcript(&injected.text);
            let base = transcript(&run(false).text);
            let shown = d_marks(&injected.text);
            if !injected.injected || shown != marks || got != base {
                failures.push(format!(
                    "{lines:?} {} {route:?}: D {shown:?}, got {got:?}, base {base:?}",
                    program.display()
                ));
            }
        }
    }
    report("R4-B1", &failures);
}

/// A string `PROMPT_COMMAND` turned into an array at a prompt (bash 5.1+,
/// the lead decision after #158 r4, #613) takes the integration out at the
/// next prompt: no mark ever carries a wrong status (the edit's own command
/// ends with `D;<status>` when our head still ran first, a bare D when an
/// element was put before it), no OSC 133 from the prompt after on, the
/// transcript of bash without Tessera, the user's elements as they set
/// them, and no DEBUG trap left. A whole new array (`=(...)`) leaves
/// nothing of ours to run, as a new string does (ADR 0021): no C or D
/// from then on, PS1's A and B only.
#[test]
fn a_string_turned_into_an_array_is_left_alone() {
    let startup = "rc() { return $1; }\n\
                   f() { echo \"PSEEN f e=$? ps=${PIPESTATUS[*]}\"; }\n\
                   f1() { echo \"PSEEN f1 e=$? ps=${PIPESTATUS[*]}\"; }\n\
                   PROMPT_COMMAND=f";
    let tail = [
        "rc 3",
        "true | false",
        OFF_LINE,
        "declare -p PROMPT_COMMAND; trap -p DEBUG",
    ];
    // The edit, the D marks, and whether the integration is taken out.
    let cases: [(&str, &[&str], bool); 4] = [
        ("PROMPT_COMMAND+=(f1)", &["5", "0"], true),
        ("PROMPT_COMMAND[1]=f1", &["5", "0"], true),
        (
            "PROMPT_COMMAND=(f1 \"${PROMPT_COMMAND[@]}\")",
            &["5", "-"],
            true,
        ),
        ("PROMPT_COMMAND=(f1)", &["5"], false),
    ];
    let mut failures = Vec::new();
    for program in bashes("5.3", "B2") {
        for (edit, marks, taken_out) in cases {
            let mut lines = vec!["rc 5", edit];
            lines.extend(tail);
            let run = |injected| {
                Run {
                    program: &program,
                    route: Route::Rcfile,
                    injected,
                    startup,
                    args: None,
                    env: &[],
                    ahead: false,
                }
                .lines(&lines)
            };
            let injected = run(true);
            let mut got = transcript(&injected.text);
            let off = got.iter().position(|line| line.starts_with("OFF["));
            let off = off.map(|at| got.remove(at));
            let mut base = transcript(&run(false).text);
            base.retain(|line| !line.starts_with("OFF["));
            let shown = d_marks(&injected.text);
            // The output from the prompt after the edit's on.
            let after = injected
                .text
                .split_once(&format!("{edit}\r\n"))
                .and_then(|(_, rest)| rest.split_once("\x1b[?2004h"))
                .map_or("", |(_, rest)| rest);
            let later_marks = if taken_out {
                after.matches("\x1b]133;").count()
            } else {
                after.matches("\x1b]133;C").count() + after.matches("\x1b]133;D").count()
            };
            let why = if taken_out { "OFF[array]" } else { "OFF[]" };
            if !injected.injected
                || shown != marks
                || after.is_empty()
                || later_marks != 0
                || off.as_deref() != Some(why)
                || got != base
            {
                failures.push(format!(
                    "{edit:?} {}: got {got:?} ({off:?}), base {base:?}, D {shown:?}: {:?}",
                    program.display(),
                    injected.text
                ));
            }
        }
    }
    report("string to array", &failures);
}
