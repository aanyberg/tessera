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
//! - `rcexit`: the end of the `--rcfile` route's `.bashrc` under `set -e`,
//!   and the status the first prompt sees (#659);
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

/// The environment variable naming a bash 5.1 for the array rows (#613;
/// Homebrew ships the newest bash only).
const BASH51_VAR: &str = "TESSERA_TEST_BASH51";

/// The bashes a row's `bashes` field names that are installed: `3.2` is
/// `/bin/bash`, `5.3` the first bash 5 of [`NEWER_BASH`], `5.1` the bash
/// [`BASH51_VAR`] names (one skip line naming it when it is unset).
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
            "5.1" => match std::env::var_os(BASH51_VAR) {
                Some(bash) if !bash.is_empty() => found.push(PathBuf::from(bash)),
                _ => eprintln!("{id}: bash 5.1 part skipped: {BASH51_VAR} is unset"),
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
    fn lines(&self, lines: &[&str]) -> Output {
        self.lines_with(lines, &[])
    }

    /// [`Run::lines`] with more files (name, contents) in the home.
    #[expect(clippy::too_many_lines, reason = "one run, start to exit")]
    fn lines_with(&self, lines: &[&str], files: &[(&str, &str)]) -> Output {
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
        for (name, contents) in files {
            std::fs::write(home.join(name), contents).unwrap();
        }
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
        let mut n = 0;
        for line in lines.iter().chain(&["exit"]) {
            // `^C <text>`: a Ctrl-C once the shell printed the text (before
            // any prompt, while a startup file runs), not a typed line.
            if let Some(text) = line.strip_prefix("^C ") {
                let shown = |output: &[u8]| count(output, text.as_bytes()) > 0;
                if !read_until(&mut output, &shown) {
                    exited = true;
                    break;
                }
                pty.write(b"\x03").expect("type a Ctrl-C");
                continue;
            }
            // Typed ahead: the M10 launches may not show `PROMPT`.
            let prompts = |output: &[u8]| self.ahead || count(output, PROMPT.as_bytes()) > n;
            if !read_until(&mut output, &prompts) {
                exited = true;
                break;
            }
            pty.write(line.as_bytes()).expect("type a line");
            pty.write(b"\r").expect("type a line");
            n += 1;
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
    // The terminal's echo of a Ctrl-C row's `^C`.
    let plain = plain.replace("^C", "");
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

/// #659: the `.bashrc` is the row's field exactly, so the file ends with
/// its last line; the injected shell and the same bash without the
/// injection both print the row's lines (`lines: -`: nothing, as both exit
/// before their first prompt), or the row is a `limit:`.
fn rcexit_row(id: &str) {
    let fields = row("rcexit", id);
    let [versions, bashrc, commands, expected] = &fields[..] else {
        panic!("{id}: {fields:?}")
    };
    let lines: Vec<&str> = commands.split('\n').collect();
    let mut failures = Vec::new();
    for program in bashes(versions, id) {
        let run = |injected| {
            Run {
                program: &program,
                route: Route::Rcfile,
                injected,
                startup: "",
                args: None,
                env: &[],
                ahead: false,
            }
            .lines_with(&lines, &[(".bashrc", bashrc.as_str())])
        };
        let what = format!("{id} {}", program.display());
        let injected = run(true);
        let base = run(false);
        if !injected.injected {
            failures.push(format!("{what}: nothing injected"));
        }
        let Some(want) = expected.strip_prefix("lines: ") else {
            if let Err(failure) = check(&what, expected, &injected, &base) {
                failures.push(failure);
            }
            continue;
        };
        let want: Vec<&str> = match want {
            "-" => Vec::new(),
            want => want.split(" / ").collect(),
        };
        for (who, output) in [("injected", &injected), ("base", &base)] {
            let got = transcript(&output.text);
            if got != want {
                failures.push(format!(
                    "{what} {who}: got {got:?}, want {want:?}: {:?}",
                    output.text
                ));
            }
        }
    }
    report(id, &failures);
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

mod rcexit {
    use super::*;
    pub(super) const TESTED: &[&str] = &[
        "E1", "E2", "E3", "E4", "E5", "E6", "E7", "E8", "E9", "E10", "E11", "E12", "E13", "E14",
        "E15", "E16", "E17", "E18", "E19", "E20", "E21", "E22", "E23", "E24", "E25", "E26", "E27",
        "E28", "E29", "E30",
    ];
    row_tests!(rcexit_row:
        e1 => "E1", e2 => "E2", e3 => "E3", e4 => "E4", e5 => "E5", e6 => "E6", e7 => "E7",
        e8 => "E8", e9 => "E9", e10 => "E10", e11 => "E11", e12 => "E12", e13 => "E13",
        e14 => "E14", e15 => "E15", e16 => "E16", e17 => "E17", e18 => "E18", e19 => "E19",
        e20 => "E20", e21 => "E21", e22 => "E22", e23 => "E23", e24 => "E24", e25 => "E25",
        e26 => "E26", e27 => "E27", e28 => "E28", e29 => "E29", e30 => "E30",
    );
}

#[test]
fn every_row_has_its_test() {
    for (kind, tested) in [
        ("pipestatus", pipestatus::TESTED),
        ("pc", pc::TESTED),
        ("rcfile", rcfile::TESTED),
        ("rcexit", rcexit::TESTED),
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

/// Startup text for the array rows: `rc` returns its argument, `st`, `f1`
/// and `f2` print the `$?` and `PIPESTATUS` they start with.
const ARRAY_DEFINE: &str = "rc() { return $1; }\n\
                            st() { echo \"PSEEN st e=$? ps=${PIPESTATUS[*]}\"; }\n\
                            f1() { echo \"PSEEN f1 e=$? ps=${PIPESTATUS[*]}\"; }\n\
                            f2() { echo \"PSEEN f2 e=$? ps=${PIPESTATUS[*]}\"; }\n";

/// An array `PROMPT_COMMAND` at startup (bash 5.1+, #613) is integrated
/// with one element of ours on the `--rcfile` route: the transcript of the
/// same shell without Tessera (every element, also ones added at a prompt,
/// sees the user's `$?` and `PIPESTATUS`), each command's `D;<status>` (the
/// `$?` the elements see), no DEBUG trap left at a prompt, no error under
/// `set -u`. On the login route the assignment drops the loader (B5h): the
/// shell is left as without Tessera.
#[test]
fn an_array_prompt_command_is_integrated_with_one_element() {
    let lines = [
        "rc 7",
        "false",
        "rc 3",
        "PROMPT_COMMAND+=(f1 f2)",
        "rc 7",
        "true | false | true",
        "rc 3",
        "trap -p DEBUG",
    ];
    let mut failures = Vec::new();
    for program in bashes("5.3 5.1", "array startup") {
        for extra in [
            "PROMPT_COMMAND=('history -a' st)",
            "PROMPT_COMMAND=()",
            "PROMPT_COMMAND=(st)\nset -u",
        ] {
            let startup = format!("{ARRAY_DEFINE}{extra}");
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
                let got = transcript(&injected.text);
                let want = transcript(&base.text);
                let marks = d_marks(&injected.text);
                let integrated = route == Route::Rcfile;
                let marks_ok = if integrated {
                    marks == ["7", "1", "3", "0", "7", "0", "3", "0"]
                } else {
                    our_sequences(&injected.text) == 0
                };
                if !marks_ok || got != want || injected.text.contains("unbound") {
                    failures.push(format!(
                        "{what}: got {got:?}, base {want:?}, D {marks:?}: {:?}",
                        injected.text
                    ));
                }
            }
        }
    }
    report("array startup", &failures);
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
/// `PROMPT_COMMAND` under `set -u`:
/// [`an_array_prompt_command_is_integrated_with_one_element`].)
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
/// #613): from the next prompt on the integration continues with our
/// element (the head and tail text leave the user's element), or, after a
/// whole new array (`=(...)`), drops: nothing of ours runs again, so no C
/// or D from then on (PS1 keeps A and B) and no trap of ours is left. No
/// mark ever carries a wrong status (the edit's own command ends with
/// `D;<status>` when our head still ran first, a bare D when an element
/// was put before it), and the transcript is that of bash without Tessera.
#[test]
fn a_string_turned_into_an_array_continues_or_drops_the_integration() {
    let startup = "rc() { return $1; }\n\
                   f() { echo \"PSEEN f e=$? ps=${PIPESTATUS[*]}\"; }\n\
                   f1() { echo \"PSEEN f1 e=$? ps=${PIPESTATUS[*]}\"; }\n\
                   PROMPT_COMMAND=f";
    let tail = ["rc 3", "true | false", "trap -p DEBUG"];
    // The edit, the D marks, and whether the integration continues.
    let cases: [(&str, &[&str], bool); 4] = [
        ("PROMPT_COMMAND+=(f1)", &["5", "0", "3", "1", "0"], true),
        ("PROMPT_COMMAND[1]=f1", &["5", "0", "3", "1", "0"], true),
        (
            "PROMPT_COMMAND=(f1 \"${PROMPT_COMMAND[@]}\")",
            &["5", "-", "3", "1", "0"],
            true,
        ),
        ("PROMPT_COMMAND=(f1)", &["5"], false),
    ];
    let mut failures = Vec::new();
    for program in bashes("5.3 5.1", "string to array") {
        for (edit, marks, continues) in cases {
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
            let got = transcript(&injected.text);
            let base = transcript(&run(false).text);
            let shown = d_marks(&injected.text);
            // The output from the prompt after the edit's on.
            let after = injected
                .text
                .split_once(&format!("{edit}\r\n"))
                .and_then(|(_, rest)| rest.split_once("\x1b[?2004h"))
                .map_or("", |(_, rest)| rest);
            let commands = after.matches("\x1b]133;C").count();
            let ends = after.matches("\x1b]133;D").count();
            // Continued: a C for each later command (and `exit`), a D for
            // each but `exit`; dropped: neither.
            let later_ok = if continues {
                commands == tail.len() + 1 && ends == tail.len()
            } else {
                commands == 0 && ends == 0 && after.matches("\x1b]133;B").count() > 0
            };
            if !injected.injected || shown != marks || after.is_empty() || !later_ok || got != base
            {
                failures.push(format!(
                    "{edit:?} {}: got {got:?}, base {base:?}, D {shown:?}: {:?}",
                    program.display(),
                    injected.text
                ));
            }
        }
    }
    report("string to array", &failures);
}

/// The string turned into an array from no `PROMPT_COMMAND` at startup:
/// `PROMPT_COMMAND+=(f1)` appends to the string that holds only our head
/// and tail, so element 0 goes and ours follows the user's:
/// `([1]=f1 [2]=ours)`, where bash without Tessera has `([0]=f1)`. The
/// integration continues, and but for that line the transcript is that of
/// bash without Tessera.
#[test]
fn an_unset_prompt_command_turned_into_an_array_keeps_only_the_users_and_ours() {
    let startup = "rc() { return $1; }\n\
                   f1() { echo \"PSEEN f1 e=$? ps=${PIPESTATUS[*]}\"; }";
    let lines = [
        "rc 5",
        "PROMPT_COMMAND+=(f1)",
        "rc 3",
        "declare -p PROMPT_COMMAND",
    ];
    let element =
        "_tessera_si_element && builtin eval \\\"\\$_tessera_si_go\\\" && _tessera_si_nop";
    let want_ours = format!("declare -a PROMPT_COMMAND=([1]=\"f1\" [2]=\"{element}\")");
    let mut failures = Vec::new();
    for program in bashes("5.3 5.1", "unset to array") {
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
        let declared = |lines: &mut Vec<String>| {
            let at = lines
                .iter()
                .position(|line| line.starts_with("declare -a PROMPT_COMMAND="));
            at.map(|n| lines.remove(n))
        };
        let mut got = transcript(&injected.text);
        let mut base = transcript(&run(false).text);
        let (ours, theirs) = (declared(&mut got), declared(&mut base));
        let shown = d_marks(&injected.text);
        if !injected.injected
            || ours.as_deref() != Some(want_ours.as_str())
            || theirs.as_deref() != Some("declare -a PROMPT_COMMAND=([0]=\"f1\")")
            || shown != ["5", "0", "3", "0"]
            || got != base
        {
            failures.push(format!(
                "{}: {ours:?} (without Tessera {theirs:?}), D {shown:?}, got {got:?}, base {base:?}: {:?}",
                program.display(),
                injected.text
            ));
        }
    }
    report("unset to array", &failures);
}

/// The starship stand-in (`PROMPT_COMMAND=pre\n<existing>\n\npost`, PS1
/// rebuilt in `post`), with the test's prompt.
const STARSHIP: &str = include_str!("fixtures/bash-tools/starship-init-master.bash");

/// E12b: an empty array at startup gets our element at index 1, never
/// at 0, so starship's scalar wrapping (`PROMPT_COMMAND=pre\n"${PROMPT_COMMAND}"\n\npost`)
/// fills element 0 and leaves ours alone; the shell stays integrated.
#[test]
fn an_empty_array_gets_our_element_at_index_1() {
    let starship = format!("{STARSHIP}\n_fake_starship_prompt() {{ printf '%s' '{PROMPT}'; }}\n");
    let mut failures = Vec::new();
    for program in bashes("5.3 5.1", "E12b") {
        let injected = Run {
            program: &program,
            route: Route::Rcfile,
            injected: true,
            startup: "PROMPT_COMMAND=()",
            args: None,
            env: &[],
            ahead: false,
        }
        .lines_with(
            &[". ~/starship.bash", "false", "declare -p PROMPT_COMMAND"],
            &[("starship.bash", &starship)],
        );
        let want = "declare -a PROMPT_COMMAND=([0]=\"starship_precmd\" \
                    [1]=\"_tessera_si_element && builtin eval \\\"\\$_tessera_si_go\\\" && _tessera_si_nop\")";
        if !injected.text.contains(want) || d_marks(&injected.text) != ["0", "1", "0"] {
            failures.push(format!("{}: {:?}", program.display(), injected.text));
        }
    }
    report("E12b", &failures);
}

/// Our element found by its exact text: a startup file whose array
/// already holds a copy of it (pasted from `declare -p PROMPT_COMMAND`)
/// keeps that one copy, with no second appended, and is integrated.
#[test]
fn a_copy_of_our_element_at_startup_is_not_appended_again() {
    let startup = "u1() { :; }\n\
                   PROMPT_COMMAND=(u1 '_tessera_si_element && builtin eval \"$_tessera_si_go\" && _tessera_si_nop')";
    let mut failures = Vec::new();
    for program in bashes("5.3 5.1", "array copy") {
        let output = Run {
            program: &program,
            route: Route::Rcfile,
            injected: true,
            startup,
            args: None,
            env: &[],
            ahead: false,
        }
        .lines(&[
            "false",
            "declare -p PROMPT_COMMAND | grep -o _tessera_si_element | wc -l | tr -d ' '",
        ]);
        let copies = transcript(&output.text);
        if copies.last().map(String::as_str) != Some("1") || d_marks(&output.text) != ["1", "0"] {
            failures.push(format!(
                "{}: {copies:?}: {:?}",
                program.display(),
                output.text
            ));
        }
    }
    report("array copy", &failures);
}

/// #676: a `set -e` `.bashrc` that sources tessera.bash itself keeps the
/// marks on the `--rcfile` route. With its own `TESSERA_SHELL_INTEGRATION`
/// that load is the live one (its `no-cwd` holds: no OSC 7); without, the
/// injected words load it after the file. Either way one handshake, a C
/// for every command and each command's D status, and no error (`set +e`
/// after the source, so `rc 5` does not end the shell).
#[test]
fn a_bashrc_that_sources_the_script_keeps_the_marks() {
    let source = ". \"$HOME\"/../scripts/*/bash/tessera.bash";
    let mut failures = Vec::new();
    for program in bashes("3.2 5.3", "#676 marks") {
        for (words, osc7) in [
            ("TESSERA_SHELL_INTEGRATION='enabled no-cwd' ", false),
            ("", true),
        ] {
            let startup =
                format!("rc() {{ return $1; }}\nset -e\n{words}{source}\necho sourced\nset +e");
            let output = Run {
                program: &program,
                route: Route::Rcfile,
                injected: true,
                startup: &startup,
                args: None,
                env: &[],
                ahead: false,
            }
            .lines(&["rc 5", "true"]);
            let text = &output.text;
            let handshakes = text.matches("\x1b]1337;ShellIntegrationVersion=").count();
            let c = text.matches("\x1b]133;C").count();
            let d = d_marks(text);
            let cwd = text.contains("\x1b]7;");
            let got = transcript(text);
            if !output.injected
                || handshakes != 1
                || c != 3
                || d != ["5", "0"]
                || cwd != osc7
                || got != ["sourced"]
            {
                failures.push(format!(
                    "{} {words:?}: {handshakes} handshakes, {c} C, D {d:?}, OSC 7 {cwd}, \
                     got {got:?}: {text:?}",
                    program.display()
                ));
            }
        }
    }
    report("#676 marks", &failures);
}

/// #713: a Ctrl-C while the `.bashrc` runs still leaves the shell
/// integrated (rows E19-E22 check what is left and the status): one
/// handshake, a C for every command and each command's D status.
#[test]
fn a_ctrl_c_in_the_bashrc_keeps_the_marks() {
    let mut failures = Vec::new();
    for program in bashes("3.2 5.3", "#713 marks") {
        let output = Run {
            program: &program,
            route: Route::Rcfile,
            injected: true,
            startup: "echo READY\nwhile (( SECONDS < 20 )); do :; done\necho after",
            args: None,
            env: &[],
            ahead: false,
        }
        .lines(&["^C READY", "false", "true"]);
        let text = &output.text;
        let handshakes = text.matches("\x1b]1337;ShellIntegrationVersion=").count();
        let c = text.matches("\x1b]133;C").count();
        let d = d_marks(text);
        let got = transcript(text);
        if !output.injected || handshakes != 1 || c != 3 || d != ["1", "0"] || got != ["READY"] {
            failures.push(format!(
                "{}: {handshakes} handshakes, {c} C, D {d:?}, got {got:?}: {text:?}",
                program.display()
            ));
        }
    }
    report("#713 marks", &failures);
}

/// #676: the --rcfile route's end runs also when there is no startup file
/// to read (the status is set when the file ran, and otherwise to 0): the
/// first command sees 0 and nothing of that end is left, as bash alone.
#[test]
fn a_missing_startup_file_leaves_nothing_of_the_rc_end() {
    let args = ["--rcfile", "/nonexistent-676/bashrc", "-i"];
    let line = "echo \"st=$? v=[$(compgen -v _tessera_si_rc_)] \
                f=[$(compgen -A function _tessera_si_rc_)]\"";
    let want = ["st=0 v=[] f=[]"];
    let mut failures = Vec::new();
    for program in bashes("3.2 5.3", "#676 no startup file") {
        for injected in [true, false] {
            let output = Run {
                program: &program,
                route: Route::Rcfile,
                injected,
                startup: "",
                args: Some(&args),
                env: &[],
                ahead: false,
            }
            .lines(&[line]);
            let got = transcript(&output.text);
            if output.injected != injected || got != want {
                failures.push(format!(
                    "{} injected {injected}: got {got:?}: {:?}",
                    program.display(),
                    output.text
                ));
            }
        }
    }
    report("#676 no startup file", &failures);
}

/// An empty line in array mode: no command line, so PS0 is not expanded
/// and nothing arms the trap. The user's elements at the next prompt run
/// without a C, and that prompt's D is bare (no status of a command that
/// did not run).
#[test]
fn an_empty_line_in_array_mode_has_no_c() {
    let define = "for f in u1 u2; do eval \"$f() { echo \\\"PSEEN $f\\\"; }\"; done\n\
                  rc() { return $1; }\nPROMPT_COMMAND=(u1 u2)";
    let mut failures = Vec::new();
    for program in bashes("5.3 5.1", "array empty line") {
        let output = Run {
            program: &program,
            route: Route::Rcfile,
            injected: true,
            startup: define,
            args: None,
            env: &[],
            ahead: false,
        }
        .lines(&["rc 3", "", "true"]);
        let c = output.text.matches("\x1b]133;C").count();
        let d = d_marks(&output.text);
        if c != 3 || d != ["3", "-", "0"] {
            failures.push(format!(
                "{}: {c} C, D {d:?}: {:?}",
                program.display(),
                output.text
            ));
        }
    }
    report("array empty line", &failures);
}

/// `shopt -u promptvars` in array mode, at startup or at a prompt: PS0
/// would print our piece instead of expanding it, so the piece leaves PS0
/// and no trap of ours is armed or kept. The user's DEBUG trap keeps
/// running for every command (`UTRAP echo hi`), nothing stray is printed,
/// and from the first command on the transcript is that of bash without
/// Tessera, but for the trap's lines naming our own commands.
#[test]
fn promptvars_off_in_array_mode_keeps_ps0_and_the_users_trap() {
    // (Not for the test's `PS1=` line: it would show the prompt early.)
    let define = "u1() { :; }\nPROMPT_COMMAND=(u1)\n\
                  trap '[[ $BASH_COMMAND == PS1=* ]] || echo UTRAP $BASH_COMMAND >&2' DEBUG\n";
    let tail = ["echo hi", "echo two", "trap -p DEBUG"];
    let mut failures = Vec::new();
    for program in bashes("5.3 5.1", "array promptvars off") {
        for at_prompt in [false, true] {
            let startup = if at_prompt {
                define.to_owned()
            } else {
                format!("{define}shopt -u promptvars\n")
            };
            let mut lines: Vec<&str> = Vec::new();
            if at_prompt {
                lines.push("shopt -u promptvars");
            }
            lines.extend(tail);
            let run = |injected| {
                Run {
                    program: &program,
                    route: Route::Rcfile,
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
            // From the first command on, without the trap's lines for our
            // commands (the trap also runs for them, as on the string route).
            let from_hi = |text: &str| -> Vec<String> {
                transcript(text)
                    .into_iter()
                    .skip_while(|line| line != "UTRAP echo hi")
                    .filter(|line| {
                        !line.starts_with("UTRAP _tessera_si")
                            && !line.starts_with("UTRAP builtin ")
                    })
                    .collect()
            };
            let got = from_hi(&injected.text);
            let want = from_hi(&base.text);
            // Nothing stray from the first prompt on (the trap shows the
            // loader's own lines before it).
            let stray = injected
                .text
                .split_once("\x1b]133;B")
                .is_none_or(|(_, rest)| rest.contains("_tessera_si_none"));
            if !injected.injected
                || stray
                || got.first().map(String::as_str) != Some("UTRAP echo hi")
                || got != want
            {
                failures.push(format!(
                    "{} at prompt {at_prompt}: got {got:?}, without Tessera {want:?}: {:?}",
                    program.display(),
                    injected.text
                ));
            }
        }
    }
    report("array promptvars off", &failures);
}

/// Adversarial B1 (#613 cycle 1, lead decision L3): an element that
/// expands `${PS1@P}` (prompt-width code), with the array doubled at a
/// prompt (E9), still closes every command once with its status: one
/// `D;<status>` per prompt and no bare D after it (Tessera keeps the last
/// D of a line, so a bare one would lose the status). The transcript is
/// that of bash without Tessera (`${PS1@P}` itself measures our marks'
/// text too, as on the string route).
#[test]
fn a_doubled_ps1_width_element_keeps_one_d_with_the_status() {
    let startup = "w() { local p=${PS1@P}; W=${#p}; }\nPROMPT_COMMAND=(w)\n";
    let lines = [
        r#"PROMPT_COMMAND+=("${PROMPT_COMMAND[@]}")"#,
        "false",
        "true",
        "true",
    ];
    let mut failures = Vec::new();
    for program in bashes("5.3 5.1", "array doubled ps1 width") {
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
        let base = run(false);
        let d = d_marks(&injected.text);
        let got = transcript(&injected.text);
        let want = transcript(&base.text);
        if !injected.injected
            || d.get(..4) != Some(&["0", "1", "0", "0"].map(str::to_owned)[..])
            || got != want
        {
            failures.push(format!(
                "{}: D marks {d:?}, transcript {got:?}, without Tessera {want:?}: {:?}",
                program.display(),
                injected.text
            ));
        }
    }
    report("array doubled ps1 width", &failures);
}

/// Adversarial M2 (#613 cycle 1, lead decision L4): a startup file that
/// makes `PROMPT_COMMAND` a readonly array leaves the shell to bash, as
/// before arrays were integrated: no "readonly variable" error, and the
/// transcript (with the elements' `$?`) is that of bash without Tessera.
#[test]
fn a_readonly_array_is_left_to_bash() {
    let startup = "u1() { echo \"[u1 rc=$?]\"; }\nPROMPT_COMMAND=(u1)\nreadonly PROMPT_COMMAND\n";
    let lines = ["false", "true", "false | true"];
    let mut failures = Vec::new();
    for program in bashes("5.3 5.1", "array readonly") {
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
        let base = run(false);
        let got = transcript(&injected.text);
        let want = transcript(&base.text);
        if injected.text.contains("readonly")
            || !got.iter().any(|line| line.contains("[u1 rc=1]"))
            || got != want
        {
            failures.push(format!(
                "{}: transcript {got:?}, without Tessera {want:?}: {:?}",
                program.display(),
                injected.text
            ));
        }
    }
    report("array readonly", &failures);
}

/// Adversarial M5 (#613 cycle 1, lead decision L4): in array mode no
/// command inherits our copies of standard output: `/dev/fd` of a
/// command lists what bash without Tessera lists (a line that only runs a
/// subshell fires no DEBUG trap and keeps them, as on the string route),
/// with `promptvars` on (the trap writes the C and closes them),
/// off at startup or at a prompt (no trap of ours, so none are opened),
/// and with an element after ours that reassigns PS0 at every prompt
/// (nothing arms our trap: it closes them before the command runs).
#[test]
fn array_mode_leaks_no_stdout_copy() {
    let define = "u1() { :; }\nps0() { PS0='${x-}'; }\nPROMPT_COMMAND=(u1)\n";
    let ls = r"/bin/ls /dev/fd/ | tr '\n' ' '; echo";
    let cases: [(&str, &str, &[&str]); 4] = [
        ("promptvars on", "", &[]),
        ("promptvars off at startup", "shopt -u promptvars\n", &[]),
        ("promptvars off at a prompt", "", &["shopt -u promptvars"]),
        (
            "an element after ours sets PS0",
            "",
            &["PROMPT_COMMAND+=(ps0)"],
        ),
    ];
    let mut failures = Vec::new();
    for program in bashes("5.3 5.1", "array stdout copies") {
        for (name, extra, first) in cases {
            let startup = format!("{define}{extra}");
            let mut lines: Vec<&str> = first.to_vec();
            lines.extend([ls, ls]);
            let run = |injected| {
                Run {
                    program: &program,
                    route: Route::Rcfile,
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
            let listed = |text: &str| -> Vec<String> {
                transcript(text)
                    .into_iter()
                    .filter(|line| line.starts_with("0 1 2"))
                    .collect()
            };
            let got = listed(&injected.text);
            let want = listed(&base.text);
            if !injected.injected || got.len() != 2 || got != want {
                failures.push(format!(
                    "{} {name}: /dev/fd {got:?}, without Tessera {want:?}: {:?}",
                    program.display(),
                    injected.text
                ));
            }
        }
    }
    report("array stdout copies", &failures);
}

/// The array edit rows of the #613 table (E1 to E17; S4 and S1 in E1 and
/// S4's lines) on bash 5.3 and 5.1, `--rcfile` route, `PROMPT_COMMAND=(u1
/// u2)` at startup. KEEP rows: every command's `D;<status>` is the `$?`
/// the user's elements see (`PSEEN`), A, B, C and D and an OSC 7 at every
/// prompt, and the transcript of the same shell without Tessera (each
/// element runs as often). DROP rows (a whole new array, `()`, or our
/// element unset by index): the command that did it has a C and no D,
/// later prompts have A and B and no C or D, the user's elements run as
/// without Tessera, and no trap of ours is left (`trap -p DEBUG`). Every
/// run ends with `bash -c env` showing nothing of ours (S8).
#[test]
#[expect(clippy::too_many_lines, reason = "one table, one report")]
fn array_edits_keep_or_drop_the_integration() {
    let define = "for f in u1 u2 u3 u4 f1; do eval \"$f() { echo \\\"PSEEN $f e=\\$? ps=\\${PIPESTATUS[*]}\\\"; }\"; done\n\
                  rc() { return $1; }\n";
    let env_line = "bash -c env | grep -c -E '^(_tessera|TESSERA_BASH_|PROMPT_COMMAND)'";
    let starship = format!("{STARSHIP}\n_fake_starship_prompt() {{ printf '%s' '{PROMPT}'; }}\n");
    // id, extra startup, the edit, whether the integration is kept.
    let rows: [(&str, &str, &str, bool); 19] = [
        ("E1", "", "true", true),
        ("S4", "", "false | true", true),
        ("E2", "", "unset 'PROMPT_COMMAND[0]'", true),
        ("E3", "", "PROMPT_COMMAND[0]=f1", true),
        ("E4", "", "PROMPT_COMMAND+=(u3)", true),
        ("E5", "", "PROMPT_COMMAND=(f1)", false),
        (
            "E6",
            "",
            "unset PROMPT_COMMAND; PROMPT_COMMAND+=(f1)",
            false,
        ),
        (
            "E7",
            "",
            "PROMPT_COMMAND=(\"${PROMPT_COMMAND[@]}\" u4)",
            true,
        ),
        (
            "E8",
            "",
            "PROMPT_COMMAND=(u4 \"${PROMPT_COMMAND[@]}\")",
            true,
        ),
        ("E9", "", "PROMPT_COMMAND+=(\"${PROMPT_COMMAND[@]}\")", true),
        (
            "E10",
            "",
            "PROMPT_COMMAND+=(u3); unset 'PROMPT_COMMAND[-1]'",
            true,
        ),
        ("E11", "", "PROMPT_COMMAND=f1", true),
        ("E12", "", "STARSHIP", true),
        ("E13", "", "PROMPT_COMMAND=()", false),
        ("E14", "", "unset 'PROMPT_COMMAND[2]'", false),
        ("E15", "", "PROMPT_COMMAND=(\"${PROMPT_COMMAND[@]}\")", true),
        ("E16", "", "PROMPT_COMMAND+=(u3 u3)", true),
        // PS0 arms our trap: a PS0 the user sets at startup keeps its text
        // first, and one reassigned at a prompt gets our piece back from
        // the next prompt on (the line that reassigns it ran with the old
        // PS0, so no prompt is late).
        (
            "PS0a",
            "PS0=$'pre\\n'\n",
            "echo \"P0=[${PS0//$'\\n'/|}]\"",
            true,
        ),
        ("PS0b", "", "PS0=$'ps0\\n'", true),
    ];
    let mut failures = Vec::new();
    for program in bashes("5.3 5.1", "array edits") {
        for (id, extra, edit, keep) in rows {
            let startup = format!("{define}PROMPT_COMMAND=(u1 u2)\n{extra}");
            let edit = if edit == "STARSHIP" {
                ". ~/starship.bash"
            } else {
                edit
            };
            let lines = [
                "rc 4",
                edit,
                "false",
                "rc 3",
                "true | false | true",
                "set -o pipefail",
                "false | true",
                "set +o pipefail",
                "trap -p DEBUG",
                env_line,
            ];
            let run = |injected| {
                Run {
                    program: &program,
                    route: Route::Rcfile,
                    injected,
                    startup: &startup,
                    args: None,
                    env: &[],
                    ahead: false,
                }
                .lines_with(&lines, &[("starship.bash", &starship)])
            };
            let injected = run(true);
            let base = run(false);
            let mut got = transcript(&injected.text);
            let mut want = transcript(&base.text);
            let text = &injected.text;
            let shown = d_marks(text);
            let mut wrong = Vec::new();
            if id == "PS0a" {
                // Our piece after the user's PS0 text, never before it.
                let ps0 = |lines: &mut Vec<String>| {
                    let line = lines.iter().position(|l| l.starts_with("P0=["));
                    line.map(|n| lines.remove(n))
                };
                let (ours, theirs) = (ps0(&mut got), ps0(&mut want));
                let piece = "${_tessera_si_none[_tessera_si_armed=1]-}";
                if theirs.as_deref() != Some("P0=[pre|]")
                    || ours != Some(format!("P0=[pre|{piece}]"))
                {
                    wrong.push(format!("PS0 {ours:?}, without Tessera {theirs:?}"));
                }
            }
            // Per prompt from the second: the D and the `$?` u2 saw.
            let pairs: Vec<(String, String)> = text
                .split("\x1b]133;A")
                .skip(1)
                .filter_map(|piece| {
                    let d = d_marks(piece).first()?.clone();
                    let rest = piece.split_once("PSEEN u2 e=")?.1;
                    Some((d, rest.chars().take_while(char::is_ascii_digit).collect()))
                })
                .collect();
            let after = text
                .split_once(&format!("{edit}\r\n"))
                .map_or("", |(_, rest)| rest);
            if got != want {
                wrong.push(format!("transcript {got:?}, without Tessera {want:?}"));
            }
            if !got.iter().any(|line| line == "0") {
                wrong.push("bash -c env shows ours".to_owned());
            }
            if keep {
                // `rc 4`, the edit, then each line's status (E17: under
                // pipefail `false | true` is 1, PIPESTATUS `1 0`).
                let want_d = ["4", "0", "1", "3", "0", "0", "1", "0", "0", "1"];
                if shown != want_d {
                    wrong.push(format!("D {shown:?}"));
                }
                if pairs.len() != want_d.len() || pairs.iter().any(|(d, seen)| d != seen) {
                    wrong.push(format!("D and u2's $? per prompt {pairs:?}"));
                }
                let prompts = lines.len() + 1;
                for mark in ["133;A", "133;B", "7;", "133;C"] {
                    let n = text.matches(&format!("\x1b]{mark}")).count();
                    if n != prompts {
                        wrong.push(format!("{n} {mark}, not {prompts}"));
                    }
                }
            } else {
                let c = after.matches("\x1b]133;C").count();
                let d = after.matches("\x1b]133;D").count();
                let b = after.matches("\x1b]133;B").count();
                // The one C is the edit line's own.
                if shown != ["4"] || c != 1 || d != 0 || b != lines.len() - 1 {
                    wrong.push(format!("dropped: D {shown:?}, {c} C, {d} D, {b} B after"));
                }
            }
            if !wrong.is_empty() {
                failures.push(format!(
                    "{id} {}: {}: {text:?}",
                    program.display(),
                    wrong.join("; ")
                ));
            }
        }
    }
    report("array edits", &failures);
}
