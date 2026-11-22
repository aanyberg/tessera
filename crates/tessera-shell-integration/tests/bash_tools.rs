//! bash with the prompt tools users run, through Tessera's injection
//! (`Injection::for_shell`) in a real PTY: the mark table of
//! `tests/fixtures/bash_spec.txt` (login shells, which load the script from
//! the `PROMPT_COMMAND` loader, and `--rcfile` shells) for 20 prompts each,
//! login shells whose startup files assign `PROMPT_COMMAND` or add to it
//! (with the children they start), an entry appended to `PROMPT_COMMAND` at a prompt, the loader text against the
//! script, and the OSC 7 encoding cache.
//!
//! The tools are stand-ins in `tests/fixtures/bash-tools/` (starship 1.27
//! and master, bash-preexec 0.7.0, 0.6.0 and 0.5.0, oh-my-bash; see each
//! file's header). They run on the macOS `/bin/bash` (3.2); a bash 4 or
//! later, when found (on `PATH` or Homebrew's), runs the same cases. The
//! table has its own first-prompt columns for bash 5.1 and later (array
//! `PROMPT_COMMAND`); a bash 4.0 to 5.0 holds the rules every row shares but
//! not the first prompt. Without one, each test prints a skip line.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tessera_pty::{NativePty, Pty, PtyEvent, PtyEvents, SpawnOptions};
use tessera_shell_integration::scripts::materialise;
use tessera_shell_integration::{Injection, Launch, Shell};

const SPEC: &str = include_str!("fixtures/bash_spec.txt");

/// The stand-ins, by file name.
const TOOLS: [(&str, &str); 8] = [
    (
        "starship-init-1.27.bash",
        include_str!("fixtures/bash-tools/starship-init-1.27.bash"),
    ),
    (
        "starship-init-master.bash",
        include_str!("fixtures/bash-tools/starship-init-master.bash"),
    ),
    (
        "bash-preexec-install.sh",
        include_str!("fixtures/bash-tools/bash-preexec-install.sh"),
    ),
    (
        "appended-theme.sh",
        include_str!("fixtures/bash-tools/appended-theme.sh"),
    ),
    (
        "bash-preexec-debug.sh",
        include_str!("fixtures/bash-tools/bash-preexec-debug.sh"),
    ),
    (
        "bash-preexec-0.6.sh",
        include_str!("fixtures/bash-tools/bash-preexec-0.6.sh"),
    ),
    (
        "bash-preexec-0.5.sh",
        include_str!("fixtures/bash-tools/bash-preexec-0.5.sh"),
    ),
    (
        "oh-my-bash-prompt.sh",
        include_str!("fixtures/bash-tools/oh-my-bash-prompt.sh"),
    ),
];

/// How long one step (a prompt, the exit) may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// The prompts each mark row runs.
const PROMPTS: usize = 20;

/// Commands typed at the prompts, in turn, with their exit statuses.
const COMMANDS: [(&str, u8); 4] = [
    ("true", 0),
    ("false", 1),
    ("/bin/sh -c 'exit 3'", 3),
    ("echo hi", 0),
];

/// Text in the output that means a startup file or one of our entries
/// failed (bash writes its errors to the terminal too).
const ERRORS: [&str; 5] = [
    "command not found",
    "No such file or directory",
    "syntax error",
    "bash: ",
    "unbound variable",
];

/// The macOS bash.
const BASH: &str = "/bin/bash";

/// Where a bash 4 or later is looked for besides `PATH`.
const NEWER_BASH: [&str; 2] = ["/opt/homebrew/bin/bash", "/usr/local/bin/bash"];

/// Which first-prompt columns of the table a bash is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Columns {
    /// bash 3.2, the macOS one.
    Bash32,
    /// bash 4.0 to 5.0: none, the rules every row shares.
    Unverified,
    /// bash 5.1 and later (`PROMPT_COMMAND` can be an array).
    Arrays,
}

/// A bash to run.
struct Bash {
    program: PathBuf,
    columns: Columns,
}

/// `/bin/bash`, and a bash 4 or later when one is installed (the first of
/// `PATH` and [`NEWER_BASH`] whose `BASH_VERSINFO` says so); prints a skip
/// line when there is none.
fn bashes(test: &str) -> Vec<Bash> {
    let mut found = vec![Bash {
        program: BASH.into(),
        columns: Columns::Bash32,
    }];
    let path = std::env::var_os("PATH").unwrap_or_default();
    let newer = std::env::split_paths(&path)
        .map(|dir| dir.join("bash"))
        .chain(NEWER_BASH.iter().map(PathBuf::from))
        .find_map(|program| {
            let version = version(&program)?;
            (version >= (4, 0)).then_some((program, version))
        });
    match newer {
        Some((program, version)) => {
            eprintln!("{test}: also run on {}", program.display());
            let columns = if version >= (5, 1) {
                Columns::Arrays
            } else {
                Columns::Unverified
            };
            found.push(Bash { program, columns });
        }
        None => eprintln!(
            "{test}: bash >= 4 part skipped: no bash 4 or later on PATH or in {NEWER_BASH:?}"
        ),
    }
    found
}

/// `program`'s `BASH_VERSINFO` major and minor, or `None` when it does not
/// run.
fn version(program: &Path) -> Option<(u32, u32)> {
    let output = std::process::Command::new(program)
        .args(["-c", "echo ${BASH_VERSINFO[0]} ${BASH_VERSINFO[1]}"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let (major, minor) = text.trim().split_once(' ')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// How the bash starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// `bash -l`, injected as Tessera's login shell (`Launch::Login`).
    Login,
    /// `bash -i`: the `--rcfile` route.
    Rcfile,
    /// `bash -i` with no injection, `.bashrc` sourcing the script as the
    /// README's manual installation says.
    Sourced,
    /// `bash --posix -i`: the loader, with the script already removed (bash
    /// 3.2 reads `.bashrc` here, bash 5 `$ENV`, so a startup file could not
    /// remove it on both).
    PosixScriptGone,
}

/// A bash with the injection applied, in a temporary home.
struct Session {
    _temp: tempfile::TempDir,
    /// Whether the injection changed anything.
    injected: bool,
    home: PathBuf,
    pty: NativePty,
    events: PtyEvents,
    output: Vec<u8>,
    /// OSC 7 sequences seen whole so far, and where the scan for the next
    /// one starts.
    osc7_seen: usize,
    osc7_scanned: usize,
}

impl Session {
    /// Starts `program` by `route` with `startup` as its `.bash_profile`
    /// (login) or `.bashrc` (the others), the stand-ins in `~/tools/`, and
    /// the scripts materialised under a path with a space and a `'` in it.
    /// The injection must not be empty.
    fn start(program: &Path, route: Route, startup: &str) -> Self {
        let session = Self::start_with(program, route, startup, &[]);
        assert!(
            session.injected || matches!(route, Route::Sourced),
            "nothing injected for {route:?}: {startup:?}"
        );
        session
    }

    /// [`Session::start`] with more files in the home directory, and an
    /// injection that may be empty: such a bash starts as it would without
    /// Tessera, with no `PROMPT_COMMAND` in its environment.
    fn start_with(program: &Path, route: Route, startup: &str, files: &[(&str, &str)]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let scripts =
            materialise(&root.join("Application Support/it's/shell-integration")).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(home.join("tools")).unwrap();
        for (name, contents) in TOOLS {
            std::fs::write(home.join("tools").join(name), contents).unwrap();
        }
        let startup_file = match route {
            Route::Login => ".bash_profile",
            Route::Rcfile | Route::Sourced | Route::PosixScriptGone => ".bashrc",
        };
        let startup = match route {
            Route::Login | Route::Rcfile | Route::PosixScriptGone => startup.to_owned(),
            Route::Sourced => {
                format!("{startup}export TESSERA_SHELL_INTEGRATION=enabled\nsource @SCRIPT@\n")
            }
        };
        let script = scripts.join("bash/tessera.bash");
        let script = script.to_str().unwrap().replace('\'', r"'\''");
        let startup = startup.replace("@SCRIPT@", &format!("'{script}'"));
        std::fs::write(home.join(startup_file), startup).unwrap();
        for (name, contents) in files {
            let path = home.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        let user_args: Vec<OsString> = match route {
            Route::Login => vec!["-l".into()],
            Route::Rcfile | Route::Sourced => vec!["-i".into()],
            Route::PosixScriptGone => vec!["--posix".into(), "-i".into()],
        };
        let home_os = home.clone().into_os_string();
        let inject = |launch| {
            Injection::for_shell(Shell::Bash, &scripts, launch, |key| {
                (key == "HOME").then(|| home_os.clone())
            })
        };
        let injection = match route {
            Route::Login => inject(Launch::Login),
            Route::Rcfile | Route::PosixScriptGone => inject(Launch::Args(&user_args)),
            Route::Sourced => Injection::default(),
        };
        let injected = !injection.is_empty();
        if route == Route::PosixScriptGone {
            std::fs::remove_file(scripts.join("bash/tessera.bash")).unwrap();
        }
        let args = injection.args.clone().unwrap_or(user_args);
        let mut options = if injected || matches!(route, Route::Sourced) {
            SpawnOptions::command(program, "bash-tools").args(args)
        } else {
            // `PROMPT_COMMAND` is set below for every other case.
            let mut env_args: Vec<OsString> = vec![
                "-u".into(),
                "PROMPT_COMMAND".into(),
                program.as_os_str().to_os_string(),
            ];
            env_args.extend(args);
            SpawnOptions::command("/usr/bin/env", "bash-tools").args(env_args)
        }
        .cwd(&home);
        let home_str = home.to_str().unwrap();
        for (key, value) in [
            ("HOME", home_str),
            ("PWD", home_str),
            ("HISTFILE", &format!("{home_str}/.history")),
            ("TERM", "xterm-256color"),
            ("LANG", "en_US.UTF-8"),
            ("PS1", "p\\$ "),
            // Inherited values that would change what bash runs.
            ("PROMPT_COMMAND", ""),
            ("ENV", ""),
            ("BASH_ENV", ""),
            ("BASH_SILENCE_DEPRECATION_WARNING", "1"),
        ] {
            options = options.env(key, value);
        }
        for (key, value) in &injection.env {
            options = options.env(key, value);
        }
        let (pty, events) = NativePty::spawn(&options).expect("spawn bash");
        Self {
            _temp: temp,
            injected,
            home,
            pty,
            events,
            output: Vec::new(),
            osc7_seen: 0,
            osc7_scanned: 0,
        }
    }

    /// Waits for prompt `n` (the end of the `n`th OSC 7, which every
    /// prompt sends, marked or not), then types `line`.
    fn at_prompt(&mut self, n: usize, line: &str) {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            self.scan_osc7();
            if self.osc7_seen >= n {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "no prompt {n} within {STEP_TIMEOUT:?}: {:?}",
                String::from_utf8_lossy(&self.output)
            );
            match self.events.recv_timeout(remaining) {
                Ok(PtyEvent::Output(bytes)) => self.output.extend_from_slice(&bytes),
                Err(RecvTimeoutError::Timeout) => {}
                Ok(PtyEvent::Exited(_)) | Err(RecvTimeoutError::Disconnected) => panic!(
                    "shell gone before prompt {n}: {:?}",
                    String::from_utf8_lossy(&self.output)
                ),
            }
        }
        self.pty.write(line.as_bytes()).expect("type a line");
        self.pty.write(b"\r").expect("type a line");
    }

    /// Waits until `text` was printed `n` times, then types `line`: for a
    /// shell without the integration, which sends no OSC 7.
    fn after_text(&mut self, n: usize, text: &str, line: &str) {
        let deadline = Instant::now() + STEP_TIMEOUT;
        while String::from_utf8_lossy(&self.output).matches(text).count() < n {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "no {text:?} {n} within {STEP_TIMEOUT:?}: {:?}",
                String::from_utf8_lossy(&self.output)
            );
            match self.events.recv_timeout(remaining) {
                Ok(PtyEvent::Output(bytes)) => self.output.extend_from_slice(&bytes),
                Err(RecvTimeoutError::Timeout) => {}
                Ok(PtyEvent::Exited(_)) | Err(RecvTimeoutError::Disconnected) => panic!(
                    "shell gone before {text:?} {n}: {:?}",
                    String::from_utf8_lossy(&self.output)
                ),
            }
        }
        self.pty.write(line.as_bytes()).expect("type a line");
        self.pty.write(b"\r").expect("type a line");
    }

    /// Waits for readline's prompt `n` (bash 5.1 and later turn bracketed
    /// paste on at every prompt), then types `line`: for a bash 5.1+ shell
    /// that may leave the integration (an array `PROMPT_COMMAND`), whose
    /// later prompts send no OSC 7.
    fn at_readline(&mut self, n: usize, line: &str) {
        self.after_text(n, BRACKETED_PASTE_ON, line);
    }

    /// Counts the OSC 7 sequences that have ended (with BEL) since the
    /// last scan.
    fn scan_osc7(&mut self) {
        while let Some(start) = find(&self.output[self.osc7_scanned..], b"\x1b]7;") {
            let start = self.osc7_scanned + start;
            let Some(end) = self.output[start..].iter().position(|&b| b == 0x07) else {
                return;
            };
            self.osc7_seen += 1;
            self.osc7_scanned = start + end + 1;
        }
    }

    /// Reads until the shell exits.
    fn finish(mut self) -> Vec<u8> {
        let deadline = Instant::now() + STEP_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "bash did not exit: {:?}",
                String::from_utf8_lossy(&self.output)
            );
            match self.events.recv_timeout(remaining) {
                Ok(PtyEvent::Output(bytes)) => self.output.extend_from_slice(&bytes),
                Err(RecvTimeoutError::Timeout) => {}
                Ok(PtyEvent::Exited(_)) | Err(RecvTimeoutError::Disconnected) => {
                    return self.output;
                }
            }
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The OSC payloads in `output`, BEL- or ST-terminated, in order.
fn osc_payloads(output: &[u8]) -> Vec<String> {
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
        payloads.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        rest = &rest[end + terminator..];
    }
    payloads
}

/// The OSC 133 marks among `payloads` (`A`, `B`, `C`, `D`, `D;1`, ...).
fn marks(payloads: &[String]) -> Vec<String> {
    payloads
        .iter()
        .filter_map(|p| p.strip_prefix("133;"))
        .map(str::to_owned)
        .collect()
}

/// The fixture rows of `kind`, split into fields (the kind dropped).
fn rows(kind: &str) -> Vec<Vec<&'static str>> {
    let rows: Vec<Vec<&str>> = SPEC
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.split(" | ").collect::<Vec<_>>())
        .filter(|fields| fields[0] == kind)
        .map(|fields| fields[1..].to_vec())
        .collect();
    assert!(!rows.is_empty(), "no {kind} rows");
    rows
}

/// A field's marks: `-` is none.
fn field_marks(field: &str) -> Vec<String> {
    if field == "-" {
        Vec::new()
    } else {
        field.split(' ').map(str::to_owned).collect()
    }
}

/// The startup file that sources `tools` (a field: names or `-`).
fn startup_sourcing(tools: &str) -> String {
    let mut startup = String::from("PS1='p\\$ '\n");
    if tools != "-" {
        for tool in tools.split(' ') {
            assert!(TOOLS.iter().any(|(name, _)| *name == tool), "{tool}");
            startup.push_str(". ~/tools/");
            startup.push_str(tool);
            startup.push('\n');
        }
    }
    startup
}

/// A message bash 3.2's job control prints now and then on a loaded Mac
/// when it forks a command (`child setpgid (<pid> to <pid>): Operation not
/// permitted`); the hooks fork nothing, and the command still runs.
const JOB_CONTROL_NOISE: &str = "bash: child setpgid (";

/// Asserts the output has none of [`ERRORS`] (on lines that are not
/// [`JOB_CONTROL_NOISE`]).
fn assert_no_errors(what: &str, output: &[u8]) {
    let text: String = String::from_utf8_lossy(output)
        .split_inclusive('\n')
        .filter(|line| !line.contains(JOB_CONTROL_NOISE))
        .collect();
    for error in ERRORS {
        assert!(!text.contains(error), "{what}: {error:?} in {text:?}");
    }
}

/// The rules every case shares: no D before the first A, every C right
/// after a B, one handshake (before the first A).
fn assert_mark_rules(what: &str, payloads: &[String]) {
    let marks = marks(payloads);
    let first_a = marks.iter().position(|m| m == "A");
    let first_d = marks.iter().position(|m| m.starts_with('D'));
    if let (Some(d), Some(a)) = (first_d, first_a) {
        assert!(a < d, "{what}: D before the first A: {marks:?}");
    }
    for (i, mark) in marks.iter().enumerate() {
        if mark == "C" {
            assert!(
                i > 0 && marks[i - 1] == "B",
                "{what}: a C not right after a B at {i}: {marks:?}"
            );
        }
    }
    let handshakes: Vec<usize> = payloads
        .iter()
        .enumerate()
        .filter(|(_, p)| p.starts_with("1337;"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(handshakes.len(), 1, "{what}: handshakes in {payloads:?}");
    assert_eq!(
        payloads[handshakes[0]],
        "1337;ShellIntegrationVersion=1;shell=bash"
    );
    if let Some(a) = payloads.iter().position(|p| p == "133;A") {
        assert!(handshakes[0] < a, "{what}: handshake after the first A");
    }
}

/// What readline prints at every prompt on bash 5.1 and later.
const BRACKETED_PASTE_ON: &str = "\u{1b}[?2004h";

/// Asserts a [`run_prompts`] shell ran as without Tessera: no OSC 133, at
/// most the handshake and one OSC 7 (bash-preexec 0.6 makes the array at
/// its first prompt, after the script loaded), and every `echo hi` printed.
fn assert_left_alone(what: &str, output: &[u8]) {
    let payloads = osc_payloads(output);
    let count = |prefix: &str| payloads.iter().filter(|p| p.starts_with(prefix)).count();
    assert!(
        count("133;") == 0 && count("1337;") <= 1 && count("7;") <= 1,
        "{what}: not integrated, yet {payloads:?}"
    );
    let echoes = (1..PROMPTS)
        .filter(|prompt| COMMANDS[(prompt - 1) % COMMANDS.len()].0 == "echo hi")
        .count();
    assert_eq!(
        String::from_utf8_lossy(output).matches("\rhi\r\n").count(),
        echoes,
        "{what}: every command ran"
    );
}

/// Runs [`PROMPTS`] prompts: [`COMMANDS`] in turn, then `exit` at the last.
/// Returns the output. A bash 5.1+ (`columns`) is waited for by readline's
/// prompts, not by OSC 7 (it may leave the integration).
fn run_prompts(program: &Path, columns: Columns, route: Route, startup: &str) -> Vec<u8> {
    let mut session = Session::start(program, route, startup);
    for prompt in 1..=PROMPTS {
        let line = if prompt == PROMPTS {
            "exit"
        } else {
            COMMANDS[(prompt - 1) % COMMANDS.len()].0
        };
        if columns == Columns::Arrays {
            session.at_readline(prompt, line);
        } else {
            session.at_prompt(prompt, line);
        }
    }
    session.finish()
}

/// The marks of a [`run_prompts`] run: `first_prompt` and `first_command`,
/// then for every later prompt the last command's `D;<status>` (when it
/// was marked), `A B` and the next command's `C` (`exit` at the last).
fn expected_marks(first_prompt: &[String], first_command: &[String]) -> Vec<String> {
    let mut want: Vec<String> = first_prompt.iter().chain(first_command).cloned().collect();
    for prompt in 2..=PROMPTS {
        if prompt > 2 || !first_command.is_empty() {
            let status = COMMANDS[(prompt - 2) % COMMANDS.len()].1;
            want.push(format!("D;{status}"));
        }
        want.extend(["A", "B", "C"].map(str::to_owned));
    }
    want
}

#[test]
fn prompt_tools_keep_every_mark_from_the_second_prompt() {
    for bash in bashes("prompt_tools_keep_every_mark_from_the_second_prompt") {
        for row in rows("mark") {
            let [
                id,
                route,
                tools,
                first_prompt,
                first_command,
                arrays_prompt,
                arrays_command,
            ] = row[..]
            else {
                panic!("bad mark row {row:?}");
            };
            let route = match route {
                "login" => Route::Login,
                "rcfile" => Route::Rcfile,
                // oh-my-bash's `PROMPT_COMMAND+=(...)` leaves a login shell
                // to bash on every bash (B5h, bash_startup.rs: the scan
                // reads its text); the rcfile rows cover it.
                "login-alone" => continue,
                _ => panic!("bad route in {row:?}"),
            };
            let what = format!("{id} on {}", bash.program.display());
            let output = run_prompts(&bash.program, bash.columns, route, &startup_sourcing(tools));
            assert_no_errors(&what, &output);
            if bash.columns == Columns::Arrays && arrays_prompt == "off" {
                assert_eq!(arrays_command, "off", "{what}: both fields are off");
                assert_left_alone(&what, &output);
                continue;
            }
            let payloads = osc_payloads(&output);
            assert_mark_rules(&what, &payloads);
            assert_eq!(
                payloads.iter().filter(|p| p.starts_with("7;")).count(),
                PROMPTS,
                "{what}: an OSC 7 per prompt: {payloads:?}"
            );
            let got = marks(&payloads);
            let (prompt_field, command_field) = match bash.columns {
                Columns::Arrays => (arrays_prompt, arrays_command),
                Columns::Bash32 | Columns::Unverified => (first_prompt, first_command),
            };
            let want = expected_marks(&field_marks(prompt_field), &field_marks(command_field));
            if bash.columns == Columns::Unverified {
                // Every prompt after the first.
                let first_d = got.iter().position(|m| m.starts_with("D;")).unwrap_or(0);
                let tail = want.len() - want.iter().position(|m| m == "D;1").unwrap() + 1;
                assert!(
                    got.len() >= tail && got[got.len() - tail..] == want[want.len() - tail..],
                    "{what} (from the second prompt; first D at {first_d}): {got:?}"
                );
            } else {
                assert_eq!(got, want, "{what}");
            }
        }
    }
}

/// Files for [`Session::start_with`]: name under the home, contents.
type HomeFiles = &'static [(&'static str, &'static str)];

/// Startup text with a function and an assignment naming it.
const ASSIGNS_FUNCTION: &str = "__p() { :; }; PROMPT_COMMAND=__p\n";

/// Types, at the shell's first prompts, a child `bash`, `bash --norc` and
/// `sh -i` (each runs a command at a prompt and exits), then a line with the
/// count of `PROMPT_COMMAND` in the environment, then `exit`.
fn type_children_and_exit(session: &mut Session, program: &Path) {
    let bash = program.display();
    let child = "echo child-$((1+1))\rexit\r";
    let lines = format!(
        "{bash}\r{child}{bash} --norc\r{child}/bin/sh -i\r{child}\
         echo \"exported=[$(env | grep -c '^PROMPT_COMMAND=')] $((40+2))\"\rexit\r"
    );
    session.pty.write(lines.as_bytes()).unwrap();
}

/// The children of [`type_children_and_exit`] all reached a prompt, printed
/// no error there, and nothing exported `PROMPT_COMMAND` to them.
fn assert_children_clean(what: &str, output: &[u8]) {
    let text = String::from_utf8_lossy(output);
    assert_eq!(text.matches("child-2").count(), 3, "{what}: {text:?}");
    assert!(text.contains("exported=[0] 42\r\n"), "{what}: {text:?}");
    assert_no_errors(what, output);
}

/// A login bash whose startup files replace `PROMPT_COMMAND` starts as it
/// would without Tessera: not integrated, nothing of ours in its
/// environment, and no exported `PROMPT_COMMAND`, so a child `bash`,
/// `bash --norc` and `sh -i` print no error at their prompts. With the
/// loader those children ran `__p`, which they do not have. The assignment
/// is in `.bash_profile`, in a sourced `.bashrc` or another sourced file, or
/// names a command that does exist.
#[test]
fn a_startup_file_replacing_prompt_command_leaves_bash_alone() {
    let cases: [(&str, &str, HomeFiles); 4] = [
        ("profile", ASSIGNS_FUNCTION, &[]),
        ("bashrc", ". ~/.bashrc\n", &[(".bashrc", ASSIGNS_FUNCTION)]),
        ("command", "PROMPT_COMMAND='history -a'\n", &[]),
        (
            "sourced",
            "source \"$HOME/lib/prompt\"\n",
            &[("lib/prompt", ASSIGNS_FUNCTION)],
        ),
    ];
    for bash in bashes("a_startup_file_replacing_prompt_command_leaves_bash_alone") {
        for (name, profile, files) in cases {
            let what = format!("{} {name}", bash.program.display());
            let startup = format!("PS1='p\\$ '\n{profile}");
            let mut session = Session::start_with(&bash.program, Route::Login, &startup, files);
            assert!(!session.injected, "{what}");
            type_children_and_exit(&mut session, &bash.program);
            let output = session.finish();
            assert_children_clean(&what, &output);
            let text = String::from_utf8_lossy(&output);
            assert!(
                marks(&osc_payloads(&output)).is_empty() && !text.contains("\x1b]1337;"),
                "{what}: integrated: {text:?}"
            );
        }
    }
}

/// What the uninjected shell of a replacing startup file keeps: the user's
/// value, not exported, and no variable of ours.
#[test]
fn a_replaced_prompt_command_stays_the_users_and_unexported() {
    let probe = "echo \"env=[$(env | grep -c -E '^(TESSERA_SHELL_INTEGRATION|TESSERA_BASH_|_tessera)')] \
                 child=[$(bash -c env | grep -c -E '^(TESSERA_SHELL_INTEGRATION|TESSERA_BASH_|_tessera)')] \
                 pc=[$PROMPT_COMMAND] exported=[$(env | grep -c '^PROMPT_COMMAND=')] $((40+2))\"";
    for bash in bashes("a_replaced_prompt_command_stays_the_users_and_unexported") {
        let mut session = Session::start_with(
            &bash.program,
            Route::Login,
            "PS1='p\\$ '\nPROMPT_COMMAND='history -a'\n",
            &[],
        );
        session.pty.write(probe.as_bytes()).unwrap();
        session.pty.write(b"\rexit\r").unwrap();
        let output = session.finish();
        let what = bash.program.display().to_string();
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("env=[0] child=[0] pc=[history -a] exported=[0] 42\r\n"),
            "{what}: {text:?}"
        );
        assert_no_errors(&what, &output);
    }
}

/// A startup file that adds to `PROMPT_COMMAND` keeps the loader: that shell
/// is integrated, and its children are as clean as anyone's.
#[test]
fn a_startup_file_adding_to_prompt_command_keeps_the_integration() {
    for bash in bashes("a_startup_file_adding_to_prompt_command_keeps_the_integration") {
        for form in [
            "PROMPT_COMMAND=\"kept=1;$PROMPT_COMMAND\"",
            "PROMPT_COMMAND=\"$PROMPT_COMMAND;kept=1\"",
            "PROMPT_COMMAND=\"${PROMPT_COMMAND:+$PROMPT_COMMAND$'\\n'}kept=1\"",
            "PROMPT_COMMAND+=$'\\nkept=1'",
            // The array forms leave the shell alone (B5h: on bash 3.2 the
            // element lands at [1], which never runs; bash_startup.rs).
            "export PROMPT_COMMAND",
        ] {
            let what = format!("{} {form}", bash.program.display());
            let startup = format!("PS1='p\\$ '\n{form}\n");
            let mut session = Session::start(&bash.program, Route::Login, &startup);
            session.at_prompt(1, "true");
            session.at_prompt(2, "true");
            session.at_prompt(3, "");
            type_children_and_exit(&mut session, &bash.program);
            let output = session.finish();
            assert_children_clean(&what, &output);
            let marks = marks(&osc_payloads(&output));
            assert!(marks.iter().any(|mark| mark == "D;0"), "{what}: {marks:?}");
        }
    }
}

/// On every route and with every prompt tool of the table, an integrated
/// shell's environment has nothing of ours once the script loaded, so neither
/// its children nor their children get it (spec V1). That includes
/// `PROMPT_COMMAND`, exported by Tessera on the login route and by the
/// harness (standing in for the user's environment) on the others: it holds
/// our entries once the script ran.
#[test]
fn an_integrated_bash_leaves_nothing_to_its_children() {
    const OURS: &str =
        "PROMPT_COMMAND|TESSERA_SHELL_INTEGRATION|TESSERA_BASH_[A-Z_]*|_tessera[a-z_0-9]*";
    let grep = format!("grep -c -E '^({OURS})='");
    let probe = format!(
        "echo \"env=[$(env | {grep})] grandchild=[$(/bin/bash -c '/bin/bash -c env' | {grep})] $((40+2))\""
    );
    let mut cases = vec![("sourced", Route::Sourced, "-")];
    for row in rows("mark") {
        let [id, route, tools, ..] = row[..] else {
            panic!("bad mark row {row:?}");
        };
        let route = match route {
            "login" => Route::Login,
            "rcfile" => Route::Rcfile,
            // Not injected (B5h): see the mark rows' test.
            "login-alone" => continue,
            _ => panic!("bad route in {row:?}"),
        };
        cases.push((id, route, tools));
    }
    for bash in bashes("an_integrated_bash_leaves_nothing_to_its_children") {
        for (id, route, tools) in &cases {
            let what = format!("{id} on {}", bash.program.display());
            let mut session = Session::start(&bash.program, *route, &startup_sourcing(tools));
            // An array PROMPT_COMMAND (bash 5.1+) leaves the shell alone,
            // with no OSC 7.
            if bash.columns == Columns::Arrays {
                session.at_readline(1, "true");
                session.at_readline(2, &probe);
                session.at_readline(3, "exit");
            } else {
                session.at_prompt(1, "true");
                session.at_prompt(2, &probe);
                session.at_prompt(3, "exit");
            }
            let output = session.finish();
            let text = String::from_utf8_lossy(&output);
            assert!(
                text.contains("env=[0] grandchild=[0] 42\r\n"),
                "{what}: {text:?}"
            );
            assert_no_errors(&what, &output);
        }
    }
}

/// `PROMPT_COMMAND="$PROMPT_COMMAND; echo -n"` typed at a prompt (X13, and
/// #358 M2, which was found on a sourced script): valid, no stray C from the
/// appended entry, and every command keeps its C and `D;<status>`.
#[test]
fn appending_to_prompt_command_keeps_every_mark() {
    for bash in bashes("appending_to_prompt_command_keeps_every_mark") {
        for route in [Route::Login, Route::Rcfile, Route::Sourced] {
            let what = format!("{} {route:?}", bash.program.display());
            let mut session = Session::start(&bash.program, route, "");
            session.at_prompt(1, "PROMPT_COMMAND=\"$PROMPT_COMMAND; echo -n\"");
            session.at_prompt(2, "false");
            session.at_prompt(3, "true");
            session.at_prompt(4, "/bin/sh -c 'exit 3'");
            session.at_prompt(5, "PROMPT_COMMAND+=$'\\n'\"echo -n\"");
            session.at_prompt(6, "false");
            session.at_prompt(7, "exit");
            let output = session.finish();
            assert_no_errors(&what, &output);
            let payloads = osc_payloads(&output);
            assert_mark_rules(&what, &payloads);
            let mut want = vec!["A".to_owned(), "B".to_owned(), "C".to_owned()];
            for status in [0, 1, 0, 3, 0, 1] {
                want.extend([format!("D;{status}"), "A".into(), "B".into(), "C".into()]);
            }
            assert_eq!(marks(&payloads), want, "{what}");
        }
    }
}

/// bash-preexec 0.7.0 sourced at a prompt of bash 3.2 (#158 r3 B3): the
/// hooks leave its `PROMPT_COMMAND` alone at every later prompt (re-placing
/// head and tail in front of its prologue made each prompt cost seconds
/// with the real bash-preexec), and every command from the second after
/// the source keeps its C and `D;<status>`.
#[test]
fn bash_preexec_sourced_at_a_prompt_keeps_its_prompt_command() {
    for route in [Route::Login, Route::Rcfile] {
        let what = format!("{BASH} {route:?}");
        let mut session = Session::start(Path::new(BASH), route, "");
        session.at_prompt(1, ". ~/tools/bash-preexec-install.sh");
        session.at_prompt(2, "printf 'PC[%s]PC' \"$PROMPT_COMMAND\"");
        session.at_prompt(3, "false");
        session.at_prompt(4, "printf 'PC[%s]PC' \"$PROMPT_COMMAND\"");
        session.at_prompt(5, "/bin/sh -c 'exit 3'");
        session.at_prompt(6, "printf 'PC[%s]PC' \"$PROMPT_COMMAND\"");
        session.at_prompt(7, "exit");
        let output = session.finish();
        assert_no_errors(&what, &output);
        let payloads = osc_payloads(&output);
        assert_mark_rules(&what, &payloads);
        // The typed lines echo `PC[%s]PC`; the values follow `PC[` elsewhere.
        let text = String::from_utf8_lossy(&output);
        let values: Vec<&str> = text
            .split("PC[")
            .skip(1)
            .filter(|piece| !piece.starts_with("%s]PC"))
            .filter_map(|piece| piece.split_once("]PC").map(|(value, _)| value))
            .collect();
        let [pc2, pc4, pc6] = values[..] else {
            panic!("{what}: {values:?}");
        };
        assert!(
            pc2.starts_with("__bp_precmd_invoke_cmd"),
            "{what}: bash-preexec's prologue is not first: {pc2:?}"
        );
        assert_eq!(pc4, pc2, "{what}: PROMPT_COMMAND changed");
        assert_eq!(pc6, pc2, "{what}: PROMPT_COMMAND changed");
        // The first command after the source runs while bash-preexec
        // installs itself: no C, a bare D. Every later one is marked.
        let mut want: Vec<String> = ["A", "B", "C", "D;0", "A", "B", "D"]
            .map(str::to_owned)
            .into();
        for status in [1, 0, 3, 0] {
            want.extend(["A".into(), "B".into(), "C".into(), format!("D;{status}")]);
        }
        want.extend(["A", "B", "C"].map(str::to_owned));
        assert_eq!(marks(&payloads), want, "{what}");
    }
}

/// bash-preexec sourced at a prompt runs the user's preexec functions from
/// the next command on, on every bash and version (#158 r4: 0.5's install
/// text, appended after our tail, saved our DEBUG trap as the trap to keep;
/// run as a preexec function it put ours back over bash-preexec's, whose
/// preexec functions then never ran again).
#[test]
fn bash_preexec_sourced_at_a_prompt_runs_preexec_functions() {
    let startup = "pe() { printf 'PE[%s]\\n' \"$1\"; }\n";
    for bash in bashes("bash_preexec_sourced_at_a_prompt_runs_preexec_functions") {
        for tool in [
            "bash-preexec-0.5.sh",
            "bash-preexec-0.6.sh",
            "bash-preexec-install.sh",
        ] {
            let what = format!("{} {tool}", bash.program.display());
            let mut session = Session::start(&bash.program, Route::Rcfile, startup);
            // On bash 5.1+ 0.6 and the stand-in make PROMPT_COMMAND an
            // array at their first prompt: the shell is left alone from
            // there (no OSC 7), and bash-preexec runs as without Tessera.
            let lines = [
                format!(". ~/tools/{tool}"),
                "preexec_functions+=(pe)".to_owned(),
                "true".to_owned(),
                "echo hi".to_owned(),
                "exit".to_owned(),
            ];
            for (prompt, line) in (1..).zip(&lines) {
                if bash.columns == Columns::Arrays {
                    session.at_readline(prompt, line);
                } else {
                    session.at_prompt(prompt, line);
                }
            }
            let output = session.finish();
            assert_no_errors(&what, &output);
            assert_mark_rules(&what, &osc_payloads(&output));
            let text = String::from_utf8_lossy(&output);
            // The stand-in's 0.7 PS0 path passes the history line (with
            // its number), the DEBUG trap path the command.
            let seen: Vec<&str> = text
                .split("PE[")
                .skip(1)
                .filter_map(|piece| piece.split_once(']').map(|(command, _)| command))
                .collect();
            for command in ["true", "echo hi", "exit"] {
                assert!(
                    seen.iter().any(|got| got.trim_start().ends_with(command)),
                    "{what}: no preexec for {command:?}: {text:?}"
                );
            }
        }
    }
}

/// bash-preexec under `set -u` on bash 5.1+: the array it makes (0.6, at
/// its first prompt) or one the user makes (after 0.5) leaves the shell
/// alone with no error, the user's preexec functions run from the first
/// command, and nothing is marked after the opt-out (#158 r5 B1: the
/// opt-out read variables bash-preexec's install never set).
#[test]
fn bash_preexec_under_set_u_leaves_an_array_alone_without_errors() {
    let pe = "pe() { printf 'PE[%s]\\n' \"$1\"; }\npreexec_functions+=(pe)\n";
    for bash in bashes("bash_preexec_under_set_u_leaves_an_array_alone_without_errors") {
        if bash.columns != Columns::Arrays {
            continue;
        }
        for (tool, lines) in [
            ("bash-preexec-0.6.sh", &["true", "echo hi", "exit"][..]),
            (
                "bash-preexec-0.5.sh",
                &["PROMPT_COMMAND+=(f1)", "true", "echo hi", "exit"][..],
            ),
        ] {
            for route in [Route::Rcfile, Route::Login] {
                let what = format!("{} {tool} {route:?}", bash.program.display());
                let startup = format!("set -u\nf1() {{ :; }}\n. ~/tools/{tool}\n{pe}");
                let mut session = Session::start(&bash.program, route, &startup);
                for (prompt, line) in (1..).zip(lines) {
                    session.at_readline(prompt, line);
                }
                let output = session.finish();
                assert_no_errors(&what, &output);
                let text = String::from_utf8_lossy(&output);
                let seen: Vec<&str> = text
                    .split("PE[")
                    .skip(1)
                    .filter_map(|piece| piece.split_once(']').map(|(command, _)| command))
                    .collect();
                // After 0.5 the user's array element runs after
                // bash-preexec's last entry, which then takes it for the
                // command: no later preexec, as without Tessera.
                let preexec = if tool.ends_with("0.6.sh") {
                    lines
                } else {
                    &lines[..1]
                };
                for command in preexec {
                    assert!(
                        seen.iter().any(|got| got.trim_start().ends_with(command)),
                        "{what}: no preexec for {command:?}: {text:?}"
                    );
                }
                // The array is there from the first prompt (0.6) or from
                // the second (the user's): no mark after that prompt's D.
                let got = marks(&osc_payloads(&output));
                let after = got
                    .iter()
                    .position(|m| m.starts_with('D'))
                    .map_or(0, |d| d + 1);
                let allowed = if tool.ends_with("0.6.sh") { 0 } else { after };
                assert!(
                    got.len() <= allowed,
                    "{what}: marks after the opt-out: {got:?}"
                );
            }
        }
    }
}

/// An entry put before bash-preexec's prologue (`PROMPT_COMMAND="cmd;
/// $PROMPT_COMMAND"`) runs before anything reads $?: every D from there on
/// is bare, never that entry's status, whether bash-preexec was sourced at
/// a prompt (after the script) or by the startup file (before it) (#158 r5
/// B2 and M-A).
#[test]
fn an_entry_put_before_bash_preexec_never_sends_a_wrong_status() {
    let rc = "rc() { return \"$1\"; }\n";
    let prepend = "PROMPT_COMMAND=\"rc 9; $PROMPT_COMMAND\"; rc 2";
    let at_a_prompt = format!("{rc}f() {{ :; }}\nPROMPT_COMMAND=f\n");
    let at_startup = format!("{rc}. ~/tools/bash-preexec-0.5.sh\n");
    let cases: [(&str, &str, &[&str], &[&str]); 2] = [
        (
            "sourced at a prompt",
            &at_a_prompt,
            &[
                "rc 5",
                ". ~/tools/bash-preexec-0.5.sh",
                "rc 6",
                prepend,
                "rc 3",
                "true",
                "exit",
            ],
            // The first command after the source runs while bash-preexec
            // installs itself (bash_preexec_sourced_at_a_prompt_keeps_its_prompt_command).
            &["D;5", "D;0", "D", "D", "D", "D"],
        ),
        (
            "sourced by the startup file",
            &at_startup,
            &["rc 5", prepend, "rc 3", "true", "exit"],
            &["D;5", "D", "D", "D"],
        ),
    ];
    for bash in bashes("an_entry_put_before_bash_preexec_never_sends_a_wrong_status") {
        for (case, startup, lines, want) in cases {
            let what = format!("{} {case}", bash.program.display());
            let mut session = Session::start(&bash.program, Route::Rcfile, startup);
            for (prompt, line) in (1..).zip(lines) {
                session.at_prompt(prompt, line);
            }
            let output = session.finish();
            assert_no_errors(&what, &output);
            let payloads = osc_payloads(&output);
            assert_mark_rules(&what, &payloads);
            let got: Vec<String> = marks(&payloads)
                .into_iter()
                .filter(|m| m.starts_with('D'))
                .collect();
            assert_eq!(got, *want, "{what}");
        }
    }
}

/// Under `set -u`, an unset `PS1` (or `HOSTNAME`), from the startup file
/// or at a prompt, with or without bash-preexec, keeps every mark and
/// prints no error (#158 r6 B1: the prompt entry read `$PS1` unguarded).
#[test]
fn an_unset_ps1_under_set_u_keeps_every_mark() {
    let rc = "set -u\nrc() { return \"$1\"; }\n";
    let bp = ". ~/tools/bash-preexec-0.5.sh\n";
    let cases: [(String, &[&str]); 4] = [
        (
            format!("{rc}unset PS1\n"),
            &["rc 5", "rc 3", "true", "exit"],
        ),
        (
            format!("{rc}{bp}unset PS1\n"),
            &["rc 5", "rc 3", "true", "exit"],
        ),
        (format!("{rc}{bp}"), &["unset PS1", "rc 3", "true", "exit"]),
        (
            format!("{rc}unset HOSTNAME\n"),
            &["rc 5", "rc 3", "true", "exit"],
        ),
    ];
    for bash in bashes("an_unset_ps1_under_set_u_keeps_every_mark") {
        for (startup, lines) in &cases {
            for route in [Route::Rcfile, Route::Login] {
                let what = format!("{} {route:?} {startup:?}", bash.program.display());
                let mut session = Session::start(&bash.program, route, startup);
                for (prompt, line) in (1..).zip(*lines) {
                    session.at_prompt(prompt, line);
                }
                let output = session.finish();
                assert_no_errors(&what, &output);
                let payloads = osc_payloads(&output);
                assert_mark_rules(&what, &payloads);
                let mut want: Vec<String> = vec!["A".into(), "B".into(), "C".into()];
                for line in &lines[..lines.len() - 1] {
                    let status = line.strip_prefix("rc ").unwrap_or("0");
                    want.extend([format!("D;{status}"), "A".into(), "B".into(), "C".into()]);
                }
                assert_eq!(marks(&payloads), want, "{what}");
            }
        }
    }
}

/// A shell that inherits the loader after the script was removed (the
/// loader stays in a tmux server's environment when a startup file runs
/// `exec tmux`, and an update prunes the old scripts) runs a no-op at every
/// prompt: no error, no marks.
#[test]
fn the_loader_does_nothing_once_the_script_is_gone() {
    for bash in bashes("the_loader_does_nothing_once_the_script_is_gone") {
        // In POSIX mode a failing `.` is a special builtin's error, which
        // must not end an interactive shell.
        for (route, startup, prompt) in [
            (Route::Login, "rm -f @SCRIPT@\nPS1='ready> '\n", "ready> "),
            (Route::PosixScriptGone, "", "p$ "),
        ] {
            let what = format!("{} {route:?}", bash.program.display());
            let mut session = Session::start(&bash.program, route, startup);
            session.after_text(1, prompt, "true");
            session.after_text(2, prompt, "echo $((40+2))");
            session.after_text(3, prompt, "exit");
            let output = session.finish();
            let text = String::from_utf8_lossy(&output);
            assert!(text.contains("42\r\n"), "{what}: {text:?}");
            assert_no_errors(&what, &output);
            assert_eq!(
                osc_payloads(&output)
                    .iter()
                    .filter(|p| !p.starts_with("7;"))
                    .count(),
                0,
                "{what}: {text:?}"
            );
        }
    }
}

/// The loader's text is what the script takes out of `PROMPT_COMMAND`
/// again (for a path with a space and a `'`), in normal and POSIX mode; it
/// sets no variable of ours in the environment, and runs nothing once
/// loaded.
#[test]
fn the_loader_is_taken_out_and_runs_once() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let scripts = materialise(&root.join("Application Support/it's/shell-integration")).unwrap();
    let injection = Injection::for_shell(Shell::Bash, &scripts, Launch::Login, |key| match key {
        "PROMPT_COMMAND" => Some(OsString::from("user_entry=1")),
        "HOME" => Some(root.join("no-home").into_os_string()),
        _ => None,
    });
    let [(key, prompt_command)] = &injection.env[..] else {
        panic!("{injection:?}");
    };
    assert_eq!(key, "PROMPT_COMMAND");
    let check = r#"eval "$PROMPT_COMMAND"
printf '[%s]' "$PROMPT_COMMAND" "${_tessera_si_loader-}" "$(env | grep -c -E '^(_tessera|TESSERA_)')"
_tessera_si_loader=2
PROMPT_COMMAND=$LOADER
eval "$PROMPT_COMMAND"
printf '[%s]\n' "${_tessera_si_loader-}""#;
    for bash in bashes("the_loader_is_taken_out_and_runs_once") {
        for mode in [&[][..], &["--posix"][..]] {
            let output = std::process::Command::new(&bash.program)
                .args(mode)
                .args(["-c", check])
                .env("PROMPT_COMMAND", prompt_command)
                .env("LOADER", prompt_command)
                .env_remove("TESSERA_SHELL_INTEGRATION")
                .output()
                .unwrap();
            let what = format!("{} {mode:?}", bash.program.display());
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                // Every word of the loader and its newline are gone.
                "[user_entry=1][1][0][2]\n",
                "{what}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty(), "{what}");
        }
    }
}

/// Terminal.app's OSC 7 rule.
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

/// The path of an OSC 7 payload (`7;file://<host><path>`).
fn osc7_path(payload: &str) -> &str {
    let url = payload.strip_prefix("7;file://").expect("an OSC 7");
    &url[url.find('/').expect("a path")..]
}

/// The path the script reports for the encoded `path` under `host`: a
/// payload over 4096 bytes (`file://`, the host and the path) names the
/// deepest ancestor that fits.
fn capped(path: &str, host: &str) -> String {
    let limit = 4096 - 7 - host.len();
    if path.len() <= limit {
        return path.to_owned();
    }
    let cut = &path[..=limit];
    match cut.rfind('/') {
        Some(end) if end > 0 => cut[..end].to_owned(),
        _ => "/".to_owned(),
    }
}

/// Each M1 row's directory is reported with the row's encoding, which is
/// Terminal.app's rule; the encoding is done once per directory (counted by
/// `_tessera_si_encodes`), also for a 3.4 KB non-ASCII directory over 50
/// prompts.
#[test]
fn the_directory_is_encoded_once_per_directory() {
    for bash in bashes("the_directory_is_encoded_once_per_directory") {
        let what = bash.program.display().to_string();
        let mut session = Session::start(&bash.program, Route::Rcfile, "");
        let home = session.home.to_str().unwrap().to_owned();
        let mut prompt = 1;
        let mut expected = vec![encode(&home)];
        for row in rows("osc7") {
            let [id, name, encoded] = row[..] else {
                panic!("bad osc7 row {row:?}");
            };
            let name = name.replace("\\n", "\n");
            assert_eq!(encode(&name), encoded, "{id}: the row is the rule");
            std::fs::create_dir(session.home.join(&name)).unwrap();
            session.at_prompt(prompt, &format!("cd ~/$'{}'", name.replace('\n', "\\n")));
            prompt += 1;
            expected.push(format!("{}/{encoded}", encode(&home)));
            session.at_prompt(prompt, "cd ~");
            prompt += 1;
            expected.push(encode(&home));
        }
        // 17 levels of 100 two-byte characters: 3.4 KB, past PATH_MAX, so
        // made from inside the shell.
        let level = "é".repeat(100);
        session.at_prompt(
            prompt,
            &format!("for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17; do mkdir {level} && cd {level}; done"),
        );
        prompt += 1;
        let deep = format!("{home}{}", format!("/{level}").repeat(17));
        assert!(deep.len() > 3400);
        expected.push(encode(&deep));
        session.at_prompt(prompt, "echo \"before=$_tessera_si_encodes\"");
        prompt += 1;
        for _ in 0..50 {
            expected.push(encode(&deep));
            session.at_prompt(prompt, "true");
            prompt += 1;
        }
        expected.push(encode(&deep));
        session.at_prompt(prompt, "echo \"after=$_tessera_si_encodes\"");
        prompt += 1;
        expected.push(encode(&deep));
        session.at_prompt(prompt, "exit");
        let output = session.finish();
        assert_no_errors(&what, &output);
        let payloads = osc_payloads(&output);
        let reported: Vec<String> = payloads
            .iter()
            .filter(|p| p.starts_with("7;"))
            .map(|p| osc7_path(p).to_owned())
            .collect();
        let host = payloads
            .iter()
            .find_map(|p| p.strip_prefix("7;file://"))
            .and_then(|url| url.split('/').next())
            .expect("an OSC 7");
        let expected: Vec<String> = expected.iter().map(|path| capped(path, host)).collect();
        assert_eq!(reported, expected, "{what}");
        // The home at the start, the rows' 12 directory changes and the
        // deep directory: 14 encodings, none during the 50 prompts.
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("before=14\r\n") && text.contains("after=14\r\n"),
            "{what}: {text:?}"
        );
    }
}
