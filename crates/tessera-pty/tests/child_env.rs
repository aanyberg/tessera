//! What a child inherits of this process's environment (#737): Tessera's
//! own variables, every `DYLD_*` and `__XPC_DYLD_*`, and what an Xcode
//! scheme launch adds are left out, the rest is passed on.
//!
//! The test runs again in a process of its own whose environment holds
//! the variables to strip (set at its start, so no test changes a shared
//! environment), and that process spawns its child through the PTY: this
//! test binary once more, which prints its environment. The child is not a
//! system program, since dyld removes `DYLD_*` from the environment of the
//! programs it restricts (`/bin/sh`, `/usr/bin/env`), which would hide a
//! filter that lets them through.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use tessera_pty::{NativePty, PtyEvent, PtyEvents, SpawnOptions};

const TIMEOUT: Duration = Duration::from_secs(30);

/// Set in the process that spawns the child; a test hook the child must
/// not inherit.
const ROLE: &str = "TESSERA_PTY_CHILD_ENV_ROLE";
/// Set by the spawning process for its child, which then prints its
/// environment.
const PRINT: &str = "TESSERA_PTY_CHILD_ENV_PRINT";
const BEGIN: &str = "<<child-env";
const END: &str = "child-env>>";

/// The test this file's parent process runs.
const PARENT_TEST: &str = "the_child_does_not_inherit_internal_or_dyld_variables";
/// The test the child runs.
const CHILD_TEST: &str = "child_prints_its_environment";

/// A library every process has loaded already, so inserting it changes
/// nothing about the processes that do inherit it.
const INSERTED: &str = "/usr/lib/libSystem.B.dylib";

/// The variables the parent process is started with, and whether its
/// child must inherit each.
const PARENT_ENV: &[(&str, &str, bool)] = &[
    ("DYLD_INSERT_LIBRARIES", INSERTED, false),
    (
        "DYLD_FALLBACK_LIBRARY_PATH",
        "/nonexistent/tessera-dyld",
        false,
    ),
    ("TESSERA_CONFIG_DIR", "/nonexistent/tessera-config", false),
    ("TESSERA_SKIP_GPU_TESTS", "1", false),
    // Replaced by the child's own (checked below).
    ("TESSERA_SESSION_ID", "inherited-session", false),
    (ROLE, "parent", false),
    (
        "XCTestConfigurationFilePath",
        "/nonexistent/tessera.xctestconfiguration",
        false,
    ),
    // What an Xcode scheme launch adds besides `DYLD_*`.
    (
        "__XPC_DYLD_LIBRARY_PATH",
        "/nonexistent/tessera-xpc-dyld",
        false,
    ),
    ("XCInjectBundleInto", "/nonexistent/Tessera.app", false),
    (
        "__XCODE_BUILT_PRODUCTS_DIR_PATHS",
        "/nonexistent/tessera-products",
        false,
    ),
    // The user's own opt-out of the shell integration (ADR 0021).
    ("TESSERA_SHELL_INTEGRATION", "0", true),
    ("LANG", "en_US.UTF-8", true),
    ("TESSERA_PTY_CHILD_ENV_UNRELATED", "x", false),
    ("MY_TESSERA_CONFIG_DIR", "kept", true),
];

#[test]
fn the_child_does_not_inherit_internal_or_dyld_variables() {
    if std::env::var_os(ROLE).is_none() {
        run_parent();
        return;
    }
    let home = std::env::var("HOME").expect("HOME is set");
    let path = std::env::var("PATH").expect("PATH is set");
    let exe = std::env::current_exe().expect("test binary");
    let options = SpawnOptions::command(&exe, "child-env")
        .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
        .env(PRINT, "1");
    // The built-in start (the profile fallback, #728) builds its command
    // the same way; both are checked.
    let spawned = [
        ("spawn", NativePty::spawn(&options)),
        (
            "spawn_builtin",
            NativePty::spawn_builtin(&options, Instant::now()),
        ),
    ];
    for (how, result) in spawned {
        let (_pty, events) = result.unwrap_or_else(|err| panic!("{how}: {err}"));
        let env = child_env(&events);
        for &(key, value, kept) in PARENT_ENV {
            if kept {
                assert_eq!(env.get(key), Some(&escaped(value)), "{how}: {key}");
            } else if key != "TESSERA_SESSION_ID" {
                assert_eq!(env.get(key), None, "{how}: {key} was inherited");
            }
        }
        assert!(
            !env.keys().any(|key| key.starts_with("DYLD_")),
            "{how}: {env:?}"
        );
        assert_eq!(env.get("HOME"), Some(&escaped(&home)), "{how}");
        assert_eq!(env.get("PATH"), Some(&escaped(&path)), "{how}");
        // What the PTY sets for the child is set after the filter.
        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));
        assert_eq!(env.get("TERM_PROGRAM").map(String::as_str), Some("Tessera"));
        assert_eq!(
            env.get("TESSERA_SESSION_ID").map(String::as_str),
            Some("child-env"),
            "{how}"
        );
        assert_eq!(env.get(PRINT).map(String::as_str), Some("1"), "{how}");
    }
}

/// Runs [`PARENT_TEST`] in a process of its own started with
/// [`PARENT_ENV`], and fails unless it passed there.
fn run_parent() {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test binary"));
    command.args(["--exact", PARENT_TEST, "--nocapture", "--test-threads=1"]);
    for &(key, value, _) in PARENT_ENV {
        command.env(key, value);
    }
    let output = command.output().expect("run the parent process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "parent: {}\n{stdout}\n{stderr}",
        output.status
    );
}

/// The environment the child printed: each name and its value as
/// [`escaped`] writes it.
fn child_env(events: &PtyEvents) -> BTreeMap<String, String> {
    let deadline = Instant::now() + TIMEOUT;
    let mut text = String::new();
    let mut exited = false;
    while !text.contains(END) && !exited {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(PtyEvent::Output(bytes)) => text.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(PtyEvent::Exited(_)) => exited = true,
            Err(RecvTimeoutError::Timeout) => panic!("timed out; output so far: {text:?}"),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let text = text.replace("\r\n", "\n");
    let start = text.find(BEGIN).map(|at| at + BEGIN.len());
    let end = text.find(END);
    let (Some(start), Some(end)) = (start, end) else {
        panic!("the child printed no environment: {text:?}");
    };
    text[start..end]
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// `value` as the child prints it: one line, control characters escaped.
fn escaped(value: &str) -> String {
    value.escape_debug().to_string()
}

/// The child's side: prints this process's environment between markers
/// when [`PRINT`] is set, and does nothing otherwise (a plain test run).
#[test]
fn child_prints_its_environment() {
    if std::env::var_os(PRINT).is_none() {
        return;
    }
    let mut out = format!("{BEGIN}\n");
    for (key, value) in std::env::vars_os() {
        writeln!(
            out,
            "{}={}",
            key.to_string_lossy().escape_debug(),
            escaped(&value.to_string_lossy())
        )
        .expect("writing to a String does not fail");
    }
    writeln!(out, "{END}").expect("writing to a String does not fail");
    print!("{out}");
}
