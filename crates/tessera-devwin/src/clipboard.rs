//! Reading the clipboard through the platform's command-line tool.
//!
//! winit has no clipboard API, so a paste runs `pbpaste` on macOS and
//! `wl-paste`, then `xclip`, elsewhere ([`native_commands`]). [`read`]
//! blocks, so the window calls it on a helper thread and receives the
//! result through its mailbox; the event loop never waits for it.
//!
//! [`read`] is bounded in time and memory whatever the tool does:
//!
//! * Each tool runs in its own process group. One that is still running
//!   at the deadline, or whose output exceeds the byte limit, is killed
//!   with its whole group (`kill -KILL -<pgid>`), so a grandchild holding
//!   the pipe open cannot keep the read going. The child is reaped.
//! * At most `max_bytes + 1` bytes are read; more is
//!   [`ClipboardError::TooLarge`].
//! * A tool that cannot be started or exits unsuccessfully is skipped and
//!   the next one tried; a timeout or an oversized clipboard ends the read.
//! * Bytes that are not UTF-8 are replaced with `U+FFFD`.
//!
//! # Limits
//!
//! * A tool that succeeds is reaped, but a background child it left
//!   behind that has closed the pipe is not killed: the group is only
//!   killed on a timeout or an oversized clipboard. (Killing it after the
//!   tool is reaped could hit an unrelated group that reused the id.)
//! * The group kill runs `kill` from `PATH`; without one only the direct
//!   child is killed, and a grandchild holding the pipe keeps the drain
//!   thread (below) alive until it exits.
//! * The pipe is drained on one more thread, which ends when every process
//!   holding the pipe has exited. A grandchild that left the group
//!   (`setsid`) and keeps the pipe open keeps that thread alive; [`read`]
//!   still returns at the deadline.
//! * A tool still running when the window exits is not waited for; it is
//!   orphaned, and its read result is never delivered.

use std::fmt;
use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How often [`read`] polls a child whose pipe has closed for its exit.
const EXIT_POLL: Duration = Duration::from_millis(5);

/// One command that prints the clipboard's text on stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCommand {
    /// Program name or path, resolved through `PATH`.
    pub program: String,
    /// Arguments after the program name.
    pub args: Vec<String>,
    /// Environment variables set for the program, on top of this
    /// process's.
    pub env: Vec<(String, String)>,
}

impl ClipboardCommand {
    /// `program` with `args`.
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|&arg| arg.to_owned()).collect(),
            env: Vec::new(),
        }
    }

    /// The same command with `key` set to `value` in its environment.
    #[must_use]
    pub fn with_env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }
}

/// The commands that read the clipboard on this platform, in the order
/// they are tried.
pub fn native_commands() -> Vec<ClipboardCommand> {
    if cfg!(target_os = "macos") {
        // `pbpaste` encodes its output for the locale's character set, so
        // under `LANG=C` (or none, as when launched outside a terminal)
        // it would turn non-ASCII text into `?`. `LC_ALL` overrides every
        // other locale variable, including an inherited `LC_ALL=C`; it needs
        // a full locale name (macOS's bare `UTF-8` locale defines only
        // `LC_CTYPE`, so `LC_ALL=UTF-8` is ASCII), and `en_US.UTF-8` ships
        // with every macOS.
        vec![ClipboardCommand::new("pbpaste", &[]).with_env("LC_ALL", "en_US.UTF-8")]
    } else {
        vec![
            ClipboardCommand::new("wl-paste", &["--no-newline", "--type", "text"]),
            ClipboardCommand::new("xclip", &["-selection", "clipboard", "-o"]),
        ]
    }
}

/// Why the clipboard could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardError {
    /// The clipboard holds more than this many bytes.
    TooLarge(usize),
    /// A tool was still running at the deadline; it was killed.
    TimedOut(Duration),
    /// No command could be started or succeeded.
    Unavailable,
    /// The thread reading the clipboard ended without a result (it
    /// panicked).
    Lost,
}

impl fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge(max) => write!(f, "the clipboard holds more than {max} bytes"),
            Self::TimedOut(timeout) => write!(f, "reading the clipboard took over {timeout:?}"),
            Self::Unavailable => f.write_str("no clipboard tool could be run"),
            Self::Lost => f.write_str("the clipboard read ended without a result"),
        }
    }
}

/// The clipboard's text from the first of `commands` that succeeds,
/// reading at most `max_bytes` and giving up after `timeout` in all; see
/// the [module docs](self).
pub fn read(
    commands: &[ClipboardCommand],
    max_bytes: usize,
    timeout: Duration,
) -> Result<String, ClipboardError> {
    let deadline = Instant::now() + timeout;
    for command in commands {
        match run(command, max_bytes, deadline) {
            Ok(Some(bytes)) => return Ok(text_of(bytes)),
            Ok(None) => {}
            Err(Bound::TooLarge) => return Err(ClipboardError::TooLarge(max_bytes)),
            Err(Bound::Deadline) => return Err(ClipboardError::TimedOut(timeout)),
        }
    }
    Err(ClipboardError::Unavailable)
}

/// `bytes` as text: moved as is when it is UTF-8, else with each invalid
/// sequence replaced by `U+FFFD`.
fn text_of(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// A bound [`run`] hit; the child's group was killed.
enum Bound {
    TooLarge,
    Deadline,
}

/// Runs `command`: its stdout if it exited successfully, `None` if it
/// could not be started or failed.
fn run(
    command: &ClipboardCommand,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Option<Vec<u8>>, Bound> {
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .envs(command.env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut process, 0);
    let Ok(mut child) = process.spawn() else {
        return Ok(None);
    };
    let Some(stdout) = child.stdout.take() else {
        kill_group(&mut child);
        return Ok(None);
    };
    let limit = u64::try_from(max_bytes).map_or(u64::MAX, |max| max.saturating_add(1));
    let (sender, receiver) = mpsc::channel();
    let drain = std::thread::Builder::new()
        .name("clipboard-read".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout.take(limit).read_to_end(&mut bytes);
            let _ = sender.send(result.map(|_| bytes));
        });
    if drain.is_err() {
        kill_group(&mut child);
        return Ok(None);
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let bytes = match receiver.recv_timeout(remaining) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            kill_group(&mut child);
            return Ok(None);
        }
        Err(_) => {
            kill_group(&mut child);
            return Err(Bound::Deadline);
        }
    };
    if bytes.len() > max_bytes {
        kill_group(&mut child);
        return Err(Bound::TooLarge);
    }
    // The pipe is closed; the child may still be running.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success().then_some(bytes)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(EXIT_POLL),
            Ok(None) => {
                kill_group(&mut child);
                return Err(Bound::Deadline);
            }
            Err(_) => {
                kill_group(&mut child);
                return Ok(None);
            }
        }
    }
}

/// Kills `child`'s process group (it leads one) and reaps `child`.
fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> ClipboardCommand {
        ClipboardCommand::new("/bin/sh", &["-c", script])
    }

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn reads_the_first_successful_command() {
        assert_eq!(
            read(&[sh("printf 'a\\nb'")], 1024, TIMEOUT),
            Ok("a\nb".into())
        );
    }

    #[test]
    fn failing_and_missing_commands_fall_through() {
        let commands = [
            ClipboardCommand::new("/nonexistent/tessera-no-such-tool", &[]),
            sh("printf partial; exit 1"),
            sh("printf second"),
        ];
        assert_eq!(read(&commands, 1024, TIMEOUT), Ok("second".into()));
    }

    #[test]
    fn nothing_that_succeeds_is_unavailable() {
        let commands = [
            ClipboardCommand::new("/nonexistent/tessera-no-such-tool", &[]),
            sh("exit 3"),
        ];
        assert_eq!(
            read(&commands, 1024, TIMEOUT),
            Err(ClipboardError::Unavailable)
        );
        assert_eq!(read(&[], 1024, TIMEOUT), Err(ClipboardError::Unavailable));
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        assert_eq!(
            read(&[sh("printf 'a\\377b'")], 1024, TIMEOUT),
            Ok("a\u{fffd}b".into())
        );
    }

    #[test]
    fn the_byte_limit_is_inclusive() {
        let exactly = read(&[sh("head -c 1024 /dev/zero")], 1024, TIMEOUT);
        assert_eq!(exactly.map(|text| text.len()), Ok(1024));
        assert_eq!(
            read(&[sh("head -c 1025 /dev/zero")], 1024, TIMEOUT),
            Err(ClipboardError::TooLarge(1024))
        );
    }

    #[test]
    fn an_endless_clipboard_stops_at_the_session_limit() {
        use tessera_app_core::session::PENDING_INPUT_BYTES;
        let started = Instant::now();
        // `yes` never ends: only the cap and the group kill stop it, and
        // no later command runs.
        let commands = [sh("yes"), sh("printf never")];
        assert_eq!(
            read(&commands, PENDING_INPUT_BYTES, TIMEOUT),
            Err(ClipboardError::TooLarge(PENDING_INPUT_BYTES))
        );
        assert!(started.elapsed() < TIMEOUT, "{:?}", started.elapsed());
        let exactly = format!("head -c {PENDING_INPUT_BYTES} /dev/zero");
        assert_eq!(
            read(&[sh(&exactly)], PENDING_INPUT_BYTES, TIMEOUT).map(|t| t.len()),
            Ok(PENDING_INPUT_BYTES)
        );
    }

    /// Whether process `pid` is gone.
    fn gone(pid: &str) -> bool {
        !Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    /// Waits up to a second for `pid` to disappear (the kernel reparents
    /// and reaps a killed grandchild asynchronously).
    fn eventually_gone(pid: &str) -> bool {
        let until = Instant::now() + Duration::from_secs(1);
        while Instant::now() < until {
            if gone(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        gone(pid)
    }

    #[test]
    fn a_hung_tool_times_out_and_is_killed() {
        let dir = std::env::temp_dir().join(format!("tessera-clip-hang-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let started = Instant::now();
        let timeout = Duration::from_millis(300);
        let commands = [sh(&script), sh("printf never")];
        assert_eq!(
            read(&commands, 1024, timeout),
            Err(ClipboardError::TimedOut(timeout))
        );
        let elapsed = started.elapsed();
        assert!(elapsed >= timeout, "{elapsed:?}");
        assert!(elapsed < timeout + Duration::from_secs(2), "{elapsed:?}");
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        assert!(eventually_gone(pid.trim()), "the hung tool was not killed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_grandchild_holding_the_pipe_is_killed_with_its_group() {
        let dir = std::env::temp_dir().join(format!("tessera-clip-gc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        // The tool prints and exits at once, but its background child keeps
        // stdout open.
        let script = format!("printf text; sleep 30 & echo $! > '{}'", pid_file.display());
        let started = Instant::now();
        let timeout = Duration::from_millis(300);
        assert_eq!(
            read(&[sh(&script)], 1024, timeout),
            Err(ClipboardError::TimedOut(timeout))
        );
        assert!(started.elapsed() < timeout + Duration::from_secs(2));
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        assert!(eventually_gone(pid.trim()), "the grandchild was not killed");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_deadline_covers_all_commands() {
        // Each failing tool alone fits in the timeout; together they and
        // the hung one do not.
        let timeout = Duration::from_millis(300);
        let commands = [
            sh("sleep 0.2; exit 1"),
            sh("sleep 0.2; exit 1"),
            sh("sleep 5"),
        ];
        let started = Instant::now();
        assert_eq!(
            read(&commands, 1024, timeout),
            Err(ClipboardError::TimedOut(timeout))
        );
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(600), "{elapsed:?}");
    }

    #[test]
    fn text_is_moved_when_valid() {
        let bytes = "é漢".as_bytes().to_vec();
        let pointer = bytes.as_ptr();
        let text = text_of(bytes);
        assert_eq!(text, "é漢");
        assert_eq!(text.as_ptr(), pointer, "valid UTF-8 is not copied");
        assert_eq!(text_of(vec![b'a', 0xff]), "a\u{fffd}");
    }

    #[test]
    fn a_tool_that_closes_stdout_but_lingers_times_out() {
        let timeout = Duration::from_millis(300);
        assert_eq!(
            read(&[sh("printf x; exec >&-; sleep 30")], 1024, timeout),
            Err(ClipboardError::TimedOut(timeout))
        );
    }

    #[test]
    fn commands_get_their_environment() {
        // A variable this process cannot have inherited, so only
        // `with_env` can set it.
        let name = format!("TESSERA_DEVWIN_CLIP_{}", std::process::id());
        assert!(std::env::var_os(&name).is_none());
        let command = sh(&format!("printf '%s' \"${name}\"")).with_env(&name, "set");
        assert_eq!(read(&[command], 1024, TIMEOUT), Ok("set".into()));
    }

    /// The environment `pbpaste` gets selects a UTF-8 locale, whatever
    /// this process inherited (`LC_ALL` overrides every `LC_*` and `LANG`).
    #[cfg(target_os = "macos")]
    #[test]
    fn pbpaste_gets_a_utf8_locale() {
        let command = ClipboardCommand {
            env: native_commands()[0].env.clone(),
            ..sh("LC_CTYPE=C LANG=C; export LC_CTYPE LANG; locale charmap")
        };
        assert_eq!(read(&[command], 1024, TIMEOUT), Ok("UTF-8\n".into()));
    }

    #[test]
    fn native_commands_are_the_platform_tools() {
        let programs: Vec<_> = native_commands().into_iter().map(|c| c.program).collect();
        if cfg!(target_os = "macos") {
            assert_eq!(programs, ["pbpaste"]);
            assert_eq!(
                native_commands()[0].env,
                [("LC_ALL".to_owned(), "en_US.UTF-8".to_owned())]
            );
        } else {
            assert_eq!(programs, ["wl-paste", "xclip"]);
            assert_eq!(
                native_commands()[0].args,
                ["--no-newline", "--type", "text"]
            );
        }
    }
}
