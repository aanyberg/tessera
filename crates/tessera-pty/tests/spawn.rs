//! End-to-end tests against real kernel PTYs.
//!
//! Every test spawns `/bin/sh`, the login shell or `/bin/sleep`, so they only
//! run on Unix.
//! Each is bounded by [`TIMEOUT`] so a regression hangs for seconds, not
//! forever.

#![cfg(unix)]

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tessera_pty::{
    ExitStatus, NativePty, Pty, PtyError, PtyEvent, PtyEvents, PtySize, SpawnOptions,
    WRITE_QUEUE_BYTES,
};

const TIMEOUT: Duration = Duration::from_secs(10);

// Signal numbers are identical on every Unix this crate targets.
const SIGHUP: i32 = 1;
const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;

/// Output gathered from a session, with `\r\n` normalised to `\n`.
#[derive(Default)]
struct Collected {
    text: String,
    exit: Option<ExitStatus>,
}

impl Collected {
    fn push(&mut self, event: PtyEvent) {
        match event {
            PtyEvent::Output(bytes) => {
                self.text
                    .push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n"));
            }
            PtyEvent::Exited(status) => self.exit = Some(status),
        }
    }
}

/// Pulls events until `done` holds or the child exits.
fn collect_until(events: &PtyEvents, mut done: impl FnMut(&Collected) -> bool) -> Collected {
    let deadline = Instant::now() + TIMEOUT;
    let mut collected = Collected::default();
    while !done(&collected) && collected.exit.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(event) => collected.push(event),
            Err(RecvTimeoutError::Timeout) => {
                panic!(
                    "timed out waiting for pty; output so far: {:?}",
                    collected.text
                )
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    collected
}

/// Drains the stream until the child exits.
fn collect_all(events: &PtyEvents) -> Collected {
    collect_until(events, |_| false)
}

fn sh(script: &str) -> SpawnOptions {
    SpawnOptions::command("/bin/sh", "test-session").args(["-c", script])
}

#[test]
fn exit_code_is_reported() {
    let (pty, events) = NativePty::spawn(&sh("exit 7")).unwrap();
    assert!(pty.child_pid().is_some());
    let collected = collect_all(&events);
    assert_eq!(collected.exit, Some(ExitStatus::Exited(7)));
    assert_eq!(pty.exit_status(), Some(ExitStatus::Exited(7)));
    // The stream ends after `Exited`: the sender is gone, not merely idle.
    assert!(matches!(
        events.recv_timeout(Duration::from_secs(2)),
        Err(RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn login_shell_runs_exit_and_reports_code() {
    // Point zsh at an empty ZDOTDIR so the developer's rc files cannot
    // interfere; other shells read nothing from it.
    let dir = std::env::temp_dir().join(format!("tessera-pty-login-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let options = SpawnOptions::login_shell("login-session").env("ZDOTDIR", &dir);
    let (mut pty, events) = NativePty::spawn(&options).unwrap();
    pty.write(b"exit 5\n").unwrap();
    let collected = collect_all(&events);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        collected.exit,
        Some(ExitStatus::Exited(5)),
        "output: {:?}",
        collected.text
    );
}

#[test]
fn identification_env_is_injected() {
    let script = r#"printf '%s|%s|%s|%s|%s\n' "$TERM" "$COLORTERM" "$TERM_PROGRAM" "$TERM_PROGRAM_VERSION" "$TESSERA_SESSION_ID""#;
    let options = sh(script);
    let (_pty, events) = NativePty::spawn(&options).unwrap();
    let collected = collect_all(&events);
    let expected = format!(
        "xterm-256color|truecolor|Tessera|{}|test-session\n",
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(collected.text, expected);
    assert_eq!(collected.exit, Some(ExitStatus::Exited(0)));
}

#[test]
fn caller_env_overrides_injected_env() {
    let options = sh(r#"printf '%s|%s\n' "$TERM" "$TESSERA_EXTRA""#)
        .env("TERM", "dumb")
        .env("TESSERA_EXTRA", "yes");
    let (_pty, events) = NativePty::spawn(&options).unwrap();
    assert_eq!(collect_all(&events).text, "dumb|yes\n");
}

#[test]
fn cwd_is_honoured() {
    let (_pty, events) = NativePty::spawn(&sh("pwd").cwd("/")).unwrap();
    assert_eq!(collect_all(&events).text, "/\n");
}

#[test]
fn resize_delivers_sigwinch_and_new_size() {
    // The child prints its size, arms a SIGWINCH trap that prints the new
    // size and exits, then idles. Only a delivered signal ends the script.
    let script = "trap 'echo WINCH; stty size; exit 0' WINCH; stty size; echo READY; \
                  while :; do sleep 0.02; done";
    let options = sh(script).size(PtySize::new(24, 80));
    let (mut pty, events) = NativePty::spawn(&options).unwrap();
    assert_eq!(pty.size(), PtySize::new(24, 80));

    let before = collect_until(&events, |c| c.text.contains("READY\n"));
    assert!(
        before.exit.is_none(),
        "child exited early: {:?}",
        before.text
    );
    assert_eq!(before.text, "24 80\nREADY\n");

    pty.resize(PtySize::new(30, 100)).unwrap();
    assert_eq!(pty.size(), PtySize::new(30, 100));

    let after = collect_all(&events);
    assert_eq!(after.text, "WINCH\n30 100\n");
    assert_eq!(after.exit, Some(ExitStatus::Exited(0)));
}

#[test]
fn stdin_reaches_the_child() {
    let (mut pty, events) = NativePty::spawn(&sh("read line; echo \"got:$line\"")).unwrap();
    pty.write(b"hello\n").unwrap();
    let collected = collect_all(&events);
    assert!(
        collected.text.ends_with("got:hello\n"),
        "output: {:?}",
        collected.text
    );
    assert_eq!(collected.exit, Some(ExitStatus::Exited(0)));
}

#[test]
fn kill_terminates_with_signal() {
    let (mut pty, events) = NativePty::spawn(&sh("sleep 30")).unwrap();
    pty.kill().unwrap();
    let collected = collect_all(&events);
    assert_eq!(collected.exit, Some(ExitStatus::Signaled(SIGKILL)));
    let status = collected.exit.unwrap();
    assert!(!status.success());
    assert_eq!(status.signal_name(), Some("SIGKILL"));
}

#[test]
fn hangup_terminates_with_sighup() {
    let (mut pty, events) = NativePty::spawn(&sh("sleep 30")).unwrap();
    pty.hangup().unwrap();
    let collected = collect_all(&events);
    assert_eq!(collected.exit, Some(ExitStatus::Signaled(SIGHUP)));
}

#[test]
fn drop_hangs_up_a_running_child() {
    let (pty, events) = NativePty::spawn(&sh("sleep 30")).unwrap();
    drop(pty);
    let collected = collect_all(&events);
    assert_eq!(collected.exit, Some(ExitStatus::Signaled(SIGHUP)));
}

#[test]
fn closing_the_session_injects_no_input() {
    // Regression test: dropping the handle used to let portable-pty's writer
    // push "\n" + VEOF into the child. The child ignores SIGHUP, copies raw
    // (non-canonical, so no line editing hides stray bytes) input to a file
    // and is killed by a helper subshell a second later, which is the only
    // way the session ends. The file must contain exactly what we wrote.
    let dir = std::env::temp_dir().join(format!("tessera-pty-noinject-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("input");
    let script = format!(
        "stty -icanon -echo; trap '' HUP; echo READY; \
         (sleep 1; kill -TERM $$) & exec cat > '{}'",
        file.display()
    );
    let (mut pty, events) = NativePty::spawn(&sh(&script)).unwrap();
    let ready = collect_until(&events, |c| c.text.contains("READY\n"));
    assert!(ready.exit.is_none(), "child exited early: {:?}", ready.text);
    pty.write(b"partial").unwrap();
    // Let the writer thread hand the bytes to the kernel before closing.
    std::thread::sleep(Duration::from_millis(100));
    drop(pty);

    let collected = collect_all(&events);
    let contents = std::fs::read(&file).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(collected.exit, Some(ExitStatus::Signaled(SIGTERM)));
    assert_eq!(
        contents,
        b"partial",
        "child received {:?}",
        String::from_utf8_lossy(&contents)
    );
}

#[test]
fn try_write_reports_would_block_when_queue_is_full() {
    // Non-canonical mode makes the kernel block master writes once its
    // input queue is full (in canonical mode an over-long line is discarded
    // instead), so with a child that never reads, our queue fills up.
    let (mut pty, events) =
        NativePty::spawn(&sh("stty -icanon -echo; echo READY; sleep 30")).unwrap();
    let ready = collect_until(&events, |c| c.text.contains("READY\n"));
    assert!(ready.exit.is_none(), "child exited early: {:?}", ready.text);

    let chunk = vec![b'x'; 64 * 1024];
    let max_chunks = WRITE_QUEUE_BYTES / chunk.len() + 2;
    let mut queued = 0;
    let error = loop {
        match pty.try_write(&chunk) {
            Ok(()) => {
                queued += 1;
                assert!(queued <= max_chunks, "queue accepted {queued} chunks");
            }
            Err(err) => break err,
        }
    };
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock, "{error}");
    assert!(queued > 0, "nothing was queued");
    pty.kill().unwrap();
    assert_eq!(
        collect_all(&events).exit,
        Some(ExitStatus::Signaled(SIGKILL))
    );
}

#[test]
fn zero_sized_windows_are_rejected() {
    let (mut pty, _events) = NativePty::spawn(&sh("sleep 30")).unwrap();
    assert!(matches!(
        pty.resize(PtySize::new(0, 80)),
        Err(PtyError::InvalidSize(_))
    ));
    assert!(matches!(
        pty.resize(PtySize::new(24, 0)),
        Err(PtyError::InvalidSize(_))
    ));
    assert_eq!(pty.size(), PtySize::default(), "size must not change");
    assert!(matches!(
        NativePty::spawn(&sh("true").size(PtySize::new(0, 0))),
        Err(PtyError::InvalidSize(_))
    ));
}

#[test]
fn writes_fail_after_exit() {
    let (mut pty, events) = NativePty::spawn(&sh("exit 0")).unwrap();
    collect_all(&events);
    assert_eq!(
        pty.write(b"x").unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
    assert_eq!(
        pty.try_write(b"x").unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
    // Signals after exit are no-ops rather than hitting a recycled pid.
    pty.hangup().unwrap();
    pty.kill().unwrap();
}

#[test]
fn missing_program_is_a_spawn_error() {
    let options = SpawnOptions::command("/nonexistent/tessera-no-such-binary", "s");
    match NativePty::spawn(&options) {
        Err(PtyError::Spawn { program, .. }) => {
            assert_eq!(program, "/nonexistent/tessera-no-such-binary");
        }
        Err(other) => panic!("unexpected error: {other}"),
        Ok(_) => panic!("spawn unexpectedly succeeded"),
    }
}

#[test]
fn large_output_is_delivered_in_order() {
    // 2048 lines of 1004 bytes (~2 MiB) through a 64 KiB read buffer and a
    // bounded queue, checked for completeness and ordering.
    let script = "pad=$(printf '%01000d' 0 | tr 0 x); i=0; \
                  while [ $i -lt 2048 ]; do printf '%04d%s\\n' $i \"$pad\"; i=$((i+1)); done";
    let (_pty, events) = NativePty::spawn(&sh(script)).unwrap();
    let collected = collect_all(&events);
    assert_eq!(collected.exit, Some(ExitStatus::Exited(0)));
    let lines: Vec<&str> = collected.text.lines().collect();
    assert_eq!(lines.len(), 2048);
    for (i, line) in lines.iter().enumerate() {
        assert!(line.starts_with(&format!("{i:04}")), "line {i}: {line:?}");
        assert_eq!(line.len(), 1004, "line {i} length");
    }
}

/// Number of threads in this process, from `ps -M` (one header line plus
/// one line per thread). Only meaningful when the test runs alone in its
/// process, as nextest runs every test.
fn thread_count() -> usize {
    let output = std::process::Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&output.stdout).lines().count() - 1
}

/// `true` when this process has no child at all, reaped or not; always
/// `true` unless the test runs alone in its process (nextest), because
/// sibling tests under `cargo test` have children of their own.
///
/// Must not be called while this process may have a zombie child some
/// reader thread is about to reap: `waitpid` would reap it first.
fn no_children() -> bool {
    use nix::errno::Errno;
    use nix::sys::wait::{WaitPidFlag, waitpid};
    if std::env::var_os("NEXTEST").is_none() {
        return true;
    }
    waitpid(None, Some(WaitPidFlag::WNOHANG)) == Err(Errno::ECHILD)
}

#[test]
fn oversized_arguments_are_refused_before_forking() {
    let alone = std::env::var_os("NEXTEST").is_some();
    let threads = thread_count();
    // 5 MiB in one argument: more than any `ARG_MAX` this crate runs under.
    let options = sh("exit 0").arg("x".repeat(5 * 1024 * 1024));
    match NativePty::spawn(&options) {
        Err(PtyError::ArgumentsTooLong { bytes, max }) => {
            assert!(bytes > 5 * 1024 * 1024, "counted {bytes} bytes");
            assert!(max < bytes, "limit {max} not below {bytes}");
            assert!(max > 0);
        }
        Err(other) => panic!("unexpected error: {other}"),
        Ok((_pty, events)) => panic!("spawned; child {:?}", collect_all(&events).exit),
    }
    assert!(no_children(), "a child was forked for refused arguments");
    if alone {
        assert_eq!(thread_count(), threads, "a refused spawn left a thread");
    }
}

#[test]
fn oversized_environment_is_refused_before_forking() {
    // The environment counts against the same limit as the arguments.
    let options = sh("exit 0").env("TESSERA_HUGE", "y".repeat(5 * 1024 * 1024));
    assert!(matches!(
        NativePty::spawn(&options),
        Err(PtyError::ArgumentsTooLong { .. })
    ));
    assert!(no_children());
}

#[test]
fn many_small_arguments_count_their_pointers() {
    // 200 000 empty arguments are only 200 000 bytes of strings, but the
    // kernel also copies one pointer per argument (1.6 MB on 64-bit), which
    // is over macOS's 1 MiB.
    let options = sh("exit 0").args(std::iter::repeat_n("", 200_000));
    match NativePty::spawn(&options) {
        Err(PtyError::ArgumentsTooLong { bytes, .. }) => {
            assert!(bytes >= 200_000 * (1 + size_of::<usize>()), "{bytes}");
        }
        Err(other) => panic!("unexpected error: {other}"),
        Ok((_pty, events)) => panic!("spawned; child {:?}", collect_all(&events).exit),
    }
}

#[test]
fn arguments_well_under_the_limit_still_spawn() {
    // 256 KiB fits the smallest `ARG_MAX` among supported systems (macOS
    // reports 1 MiB, Linux a quarter of the stack limit), in 64 KiB pieces
    // because Linux also caps each string at 128 KiB.
    let piece = "z".repeat(64 * 1024);
    let script = r#"[ "${#1}.${#2}.${#3}.${#4}" = 65536.65536.65536.65536 ] && exit 7"#;
    let options = sh(script)
        .arg("argv0")
        .args([&piece, &piece, &piece, &piece]);
    let (_pty, events) = NativePty::spawn(&options).unwrap();
    assert_eq!(collect_all(&events).exit, Some(ExitStatus::Exited(7)));
}

#[test]
fn an_argument_with_an_interior_nul_is_a_spawn_error() {
    // Only Rust callers can build one (C strings end at the NUL).
    let options = sh("exit 0").arg("a\0b");
    assert!(matches!(
        NativePty::spawn(&options),
        Err(PtyError::Spawn { .. })
    ));
    assert!(no_children());
}

#[test]
fn non_utf8_arguments_reach_the_child_unchanged() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    // `printf %s` writes the argument's bytes; `od` shows them in hex.
    let options = sh(r#"printf '<%s>' "$(printf %s "$1" | od -An -tx1 | tr -d ' \n')""#)
        .arg("argv0")
        .arg(OsStr::from_bytes(b"\xff\xfe"));
    let (_pty, events) = NativePty::spawn(&options).unwrap();
    let collected = collect_all(&events);
    assert!(collected.text.contains("<fffe>"), "{:?}", collected.text);
}

fn child_pid(pty: &NativePty) -> nix::unistd::Pid {
    nix::unistd::Pid::from_raw(i32::try_from(pty.child_pid().unwrap()).unwrap())
}

#[test]
fn the_child_leads_its_own_session_on_the_pty_as_controlling_terminal() {
    // A shell is the wrong probe: bash (and so `/bin/sh` on macOS) takes a
    // controlling terminal itself when it starts as a session leader without
    // one, so `sh -c 'exec 3</dev/tty'` passes even when the spawn gave none
    // and zsh or fish would run without job control (no Ctrl-C). `sleep`
    // opens nothing, so whatever terminal it has came from the spawn; `ps`
    // reads the terminal and its foreground group from the kernel.
    let options = SpawnOptions::command("/bin/sleep", "test-session").args(["30"]);
    let (pty, _events) = NativePty::spawn(&options).unwrap();
    let pid = child_pid(&pty);
    assert_eq!(nix::unistd::getsid(Some(pid)), Ok(pid), "session leader");
    assert_eq!(nix::unistd::getpgid(Some(pid)), Ok(pid), "group leader");
    assert_ne!(
        nix::unistd::getsid(Some(pid)),
        nix::unistd::getsid(None),
        "a new session, not ours"
    );
    let ps = std::process::Command::new("ps")
        .args(["-o", "tpgid=,tty=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let ps = String::from_utf8_lossy(&ps.stdout);
    let fields: Vec<&str> = ps.split_whitespace().collect();
    assert_eq!(
        fields.first().copied(),
        Some(pid.to_string().as_str()),
        "the child's group is the terminal's foreground group: {ps:?}"
    );
    assert!(
        fields.get(1).is_some_and(|tty| !tty.starts_with('?')),
        "the child has a controlling terminal: {ps:?}"
    );
}

/// A temporary directory removed on drop.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tessera-pty-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Writes `bytes` to `name` with `mode` and returns its path.
    fn file(&self, name: &str, bytes: &[u8], mode: u32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.0.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spawns `options`, which must be refused with [`PtyError::Spawn`]
/// before any fork, and returns the reason.
fn refused_reason(options: &SpawnOptions) -> String {
    let reason = match NativePty::spawn(options) {
        Err(PtyError::Spawn { reason, .. }) => reason,
        Err(other) => panic!("unexpected error: {other}"),
        Ok((_pty, events)) => panic!("spawned; child {:?}", collect_all(&events).exit),
    };
    assert!(no_children(), "a child was forked for a refused spawn");
    reason
}

#[test]
fn a_mach_o_binary_and_a_shell_script_still_spawn() {
    let dir = TempDir::new("exec-ok");
    let script = dir.file("script", b"#!/bin/sh\necho from-script\n", 0o755);
    for (options, text) in [
        (
            SpawnOptions::command("/bin/echo", "s").arg("from-echo"),
            "from-echo\n",
        ),
        (SpawnOptions::command(&script, "s"), "from-script\n"),
    ] {
        let collected = collect_all(&NativePty::spawn(&options).unwrap().1);
        assert_eq!(collected.exit, Some(ExitStatus::Exited(0)));
        assert_eq!(collected.text, text);
    }
}

#[test]
fn a_script_with_a_missing_interpreter_is_refused_before_forking() {
    let dir = TempDir::new("exec-missing");
    let script = dir.file("script", b"#!/nonexistent/interp\necho x\n", 0o755);
    let options = SpawnOptions::command(&script, "s");
    let reason = refused_reason(&options);
    assert!(reason.contains("\"/nonexistent/interp\""), "{reason}");
    assert!(reason.contains("ENOENT"), "{reason}");
    let display = NativePty::spawn(&options).err().unwrap().to_string();
    assert!(
        display.starts_with(&format!("failed to spawn {}: ", script.display())),
        "{display}"
    );
}

#[test]
fn a_crlf_script_is_refused_with_a_hint() {
    let dir = TempDir::new("exec-crlf");
    let script = dir.file("script", b"#!/bin/sh\r\necho crlf\r\n", 0o755);
    let reason = refused_reason(&SpawnOptions::command(&script, "s"));
    assert!(reason.contains(r#""/bin/sh\r""#), "{reason}");
    assert!(reason.contains("CRLF"), "{reason}");
}

#[test]
fn the_interpreter_check_follows_the_programs_path_lookup() {
    // portable-pty looks a bare name up in the command's `PATH`, joined to
    // the working directory; a `./` name is joined to the working
    // directory alone. The check inspects that same file.
    let dir = TempDir::new("exec-path");
    let bin = dir.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(bin.join("tessera-bad-tool")).unwrap();
    let decoy = TempDir::new("exec-path-decoy");
    decoy.file("tessera-bad-tool", b"#!/bin/sh\necho decoy\n", 0o644);
    std::fs::write(bin.join("tessera-bad-tool/x"), b"").unwrap();
    let real = TempDir::new("exec-path-real");
    real.file("tessera-bad-tool", b"#!/nonexistent/x\n", 0o755);
    // A directory and a file without x permission come first in `PATH`;
    // both are skipped, as portable-pty skips them.
    let path = std::env::join_paths([&bin, &decoy.0, &real.0]).unwrap();
    let options = SpawnOptions::command("tessera-bad-tool", "s").env("PATH", &path);
    assert!(refused_reason(&options).contains("/nonexistent/x"));

    // A relative `PATH` entry is joined to the working directory.
    let options = SpawnOptions::command("tessera-bad-tool", "s")
        .env("PATH", "exec-path-sub")
        .cwd(&dir.0);
    std::fs::create_dir_all(dir.0.join("exec-path-sub")).unwrap();
    dir.file(
        "exec-path-sub/tessera-bad-tool",
        b"#!/nonexistent/y\n",
        0o755,
    );
    assert!(refused_reason(&options).contains("/nonexistent/y"));

    dir.file("local", b"#!/nonexistent/z\n", 0o755);
    let options = SpawnOptions::command("./local", "s").cwd(&dir.0);
    assert!(refused_reason(&options).contains("/nonexistent/z"));

    // The same names resolving to a good script spawn.
    dir.file("good", b"#!/bin/sh\necho good\n", 0o755);
    let options = SpawnOptions::command("./good", "s").cwd(&dir.0);
    assert_eq!(
        collect_all(&NativePty::spawn(&options).unwrap().1).text,
        "good\n"
    );
}

#[test]
fn a_login_shell_that_is_a_broken_script_is_refused() {
    let dir = TempDir::new("exec-login");
    let shell = dir.file("shell", b"#!/nonexistent/shell\n", 0o755);
    let reason = refused_reason(&SpawnOptions::login_shell("s").env("SHELL", &shell));
    assert!(reason.contains("/nonexistent/shell"), "{reason}");
}

#[test]
fn a_directory_and_a_file_without_x_permission_are_refused_before_forking() {
    let dir = TempDir::new("exec-kinds");
    let sub = dir.0.join("dir");
    std::fs::create_dir_all(&sub).unwrap();
    assert!(refused_reason(&SpawnOptions::command(&sub, "s")).contains("directory"));
    let nox = dir.file("nox", b"#!/bin/sh\necho x\n", 0o644);
    assert!(refused_reason(&SpawnOptions::command(&nox, "s")).contains("not executable"));
}

/// One row of `tests/fixtures/exec_spec.txt`.
#[derive(Debug)]
struct ExecRow {
    id: String,
    kind: String,
    content: Vec<u8>,
    execvp: String,
    tessera: String,
}

/// Decodes the fixture's content escapes (`\n \r \t \0 \xHH \\`, `$DIR`,
/// `{c*N}`).
fn decode_content(text: &str, dir: &std::path::Path) -> Vec<u8> {
    let text = text.replace("$DIR", &dir.display().to_string());
    let mut bytes = Vec::new();
    let mut rest = text.as_str();
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        match c {
            '\\' => {
                let escape = rest.chars().next().expect("escape at end of content");
                rest = &rest[1..];
                match escape {
                    'n' => bytes.push(b'\n'),
                    'r' => bytes.push(b'\r'),
                    't' => bytes.push(b'\t'),
                    '0' => bytes.push(0),
                    '\\' => bytes.push(b'\\'),
                    'x' => {
                        bytes.push(u8::from_str_radix(&rest[..2], 16).expect("hex escape"));
                        rest = &rest[2..];
                    }
                    other => panic!("unknown escape \\{other}"),
                }
            }
            '{' => {
                let end = rest.find('}').expect("unclosed repetition");
                let (ch, count) = rest[..end].split_once('*').expect("{c*N}");
                let count: usize = count.parse().expect("repetition count");
                bytes.extend(ch.repeat(count).bytes());
                rest = &rest[end + 1..];
            }
            other => {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    bytes
}

fn exec_rows(dir: &std::path::Path) -> Vec<ExecRow> {
    include_str!("fixtures/exec_spec.txt")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split(" | ").collect();
            assert_eq!(fields.len(), 7, "malformed fixture row: {line}");
            ExecRow {
                id: fields[0].to_owned(),
                kind: fields[1].to_owned(),
                content: decode_content(fields[2].trim(), dir),
                execvp: fields[4].to_owned(),
                tessera: fields[5].to_owned(),
            }
        })
        .collect()
}

/// Creates `row`'s file in `dir`.
fn create(row: &ExecRow, dir: &TempDir) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.0.join(&row.id);
    match row.kind.as_str() {
        "file" => {
            dir.file(&row.id, &row.content, 0o755);
        }
        "nox" => {
            dir.file(&row.id, &row.content, 0o644);
        }
        "dir" => {
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        "fifo" => {
            let made = std::process::Command::new("mkfifo")
                .args(["-m", "755"])
                .arg(&path)
                .status()
                .unwrap();
            assert!(made.success(), "mkfifo failed");
        }
        "link" => {
            use std::os::unix::ffi::OsStrExt;
            let target = std::ffi::OsStr::from_bytes(&row.content);
            std::os::unix::fs::symlink(target, &path).unwrap();
        }
        "none" | "doc" => {}
        other => panic!("unknown kind {other} in row {}", row.id),
    }
}

/// What exec itself does with `path`, run in its own directory, as the standard library's spawn
/// reports it (on macOS `posix_spawn`, which runs an `ENOEXEC` file with
/// `/bin/sh` as `execvp` does): `ok`, or the errno.
fn exec_outcome(path: &std::path::Path) -> String {
    use std::process::{Command, Stdio};
    match Command::new(path)
        .current_dir(path.parent().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            child.wait().unwrap();
            "ok".to_owned()
        }
        Err(err) => {
            settle_failed_spawn();
            match err.raw_os_error() {
                Some(2) => "ENOENT".to_owned(),
                Some(13) => "EACCES".to_owned(),
                other => format!("errno {other:?}"),
            }
        }
    }
}

/// Waits until the process a failed `posix_spawn` leaves behind is gone.
/// XNU creates the child before the exec fails and then reaps it itself;
/// until it has, `waitpid` sees a running child, which a loaded machine
/// keeps visible long enough to trip the next row's `no_children` check.
/// Like that check, it runs only alone in its process (under nextest).
fn settle_failed_spawn() {
    use nix::errno::Errno;
    use nix::sys::wait::{WaitPidFlag, waitpid};
    if std::env::var_os("NEXTEST").is_none() {
        return;
    }
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Err(Errno::ECHILD) => return,
            Ok(nix::sys::wait::WaitStatus::StillAlive) | Err(Errno::EINTR) => {}
            other => panic!("a failed spawn left a child we reaped: {other:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "a failed spawn left a child behind"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Every row of `tests/fixtures/exec_spec.txt` (issue #127): exec's own
/// outcome on this machine matches the `execvp` column, and
/// [`NativePty::spawn`] does what the `tessera` column says. The `refused`
/// rows are exactly the files exec fails on with `ENOENT` or `EACCES`
/// because of their `#!` interpreter, so the pre-check agrees with exec.
#[cfg(target_os = "macos")]
#[test]
fn exec_spec_fixture() {
    let dir = TempDir::new("exec-spec");
    let rows = exec_rows(&dir.0);
    for row in &rows {
        create(row, &dir);
    }
    let mut checked = 0;
    for row in rows.iter().filter(|row| row.tessera != "doc") {
        let path = dir.0.join(&row.id);
        let exec = exec_outcome(&path);
        let expected_exec = if row.execvp == "sh" {
            "ok"
        } else {
            row.execvp.as_str()
        };
        assert_eq!(exec, expected_exec, "row {}: exec outcome", row.id);

        let options = SpawnOptions::command(&path, "exec-spec").cwd(&dir.0);
        let (verdict, detail) = row.tessera.split_once(' ').unwrap_or((&row.tessera, ""));
        match verdict {
            "runs" => {
                let (code, text) = detail.split_once(' ').unwrap_or((detail, ""));
                let (_pty, events) = NativePty::spawn(&options)
                    .unwrap_or_else(|err| panic!("row {}: {err}", row.id));
                let collected = collect_all(&events);
                assert_eq!(
                    collected.exit,
                    Some(ExitStatus::Exited(code.parse().unwrap())),
                    "row {}: {:?}",
                    row.id,
                    collected.text
                );
                assert!(
                    collected.text.contains(text),
                    "row {}: {:?}",
                    row.id,
                    collected.text
                );
            }
            "refused" => {
                let reason = refused_reason(&options);
                assert!(reason.contains(detail), "row {}: {reason}", row.id);
                assert!(reason.contains("#! "), "row {}: {reason}", row.id);
                assert_eq!(exec, detail, "row {}: exec agrees", row.id);
            }
            "refused-pp" => {
                let reason = refused_reason(&options);
                assert!(reason.contains(detail), "row {}: {reason}", row.id);
            }
            "returns" => {
                // On a thread, so a spawn that blocks fails the test
                // instead of hanging it.
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    if let Ok((_pty, events)) = NativePty::spawn(&options) {
                        collect_all(&events);
                    }
                    let _ = done_tx.send(());
                });
                assert!(
                    done_rx.recv_timeout(TIMEOUT).is_ok(),
                    "row {}: spawn blocked",
                    row.id
                );
            }
            other => panic!("row {}: unknown verdict {other}", row.id),
        }
        checked += 1;
    }
    assert!(checked >= 37, "only {checked} rows checked");
}
