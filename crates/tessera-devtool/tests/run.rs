//! End-to-end tests of the `tessera-devtool` binary and the dump formatters.
//!
//! The binary tests spawn real PTYs running `/bin/sh`; every wait is bounded
//! by the tool's own `--timeout`.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tessera_devtool::dump;
use tessera_devtool::run::{self, Completion, RunOptions};
use tessera_grid::{Color, Grid, Style};
use tessera_pty::PtyError;

/// Runs `tessera-devtool run <command> <args…>` with a 10 s timeout unless
/// `args` sets one.
fn devtool(command: &str, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tessera-devtool"));
    cmd.arg("run").arg(command).args(args);
    if !args.contains(&"--timeout") {
        cmd.args(["--timeout", "10"]);
    }
    cmd.output().expect("the devtool binary runs")
}

fn stdout_lines(output: &Output) -> Vec<&str> {
    std::str::from_utf8(&output.stdout)
        .expect("dumps are UTF-8")
        .lines()
        .collect()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("dumps are UTF-8")
}

/// Runs `command` on a small terminal and returns the `text` dump, checking
/// that the command succeeded.
fn text_dump(command: &str, cols: &str, rows: &str) -> String {
    let output = devtool(
        command,
        &["--cols", cols, "--rows", rows, "--format", "text"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout(&output)
}

#[test]
fn printf_with_a_tab_dumps_as_plain_text() {
    // The issue's example: `run 'printf "a\tb\n"' --cols 20 --rows 3`.
    assert_eq!(
        text_dump(r#"printf "a\tb\n""#, "20", "3"),
        "a       b\n\n\n"
    );
}

#[test]
fn wide_character_dumps_once() {
    // U+4E2D 中 takes two columns and still prints as one character.
    assert_eq!(text_dump(r"printf '\344\270\255x'", "6", "1"), "中x\n");
}

#[test]
fn combining_mark_joins_its_base() {
    // e + U+0301 COMBINING ACUTE ACCENT is one cell with a cluster.
    assert_eq!(text_dump(r"printf 'e\314\201!'", "6", "1"), "e\u{301}!\n");
    let cells = stdout(&devtool(
        r"printf 'e\314\201!'",
        &["--cols", "3", "--rows", "1", "--format", "cells"],
    ));
    assert_eq!(
        cells,
        "0:0 \"e\\u{301}\" cluster fg=default bg=default attrs=-\n\
         0:1 \"!\" - fg=default bg=default attrs=-\n\
         0:2 \" \" - fg=default bg=default attrs=-\n"
    );
}

#[test]
fn erase_display_and_home_clear_the_screen() {
    assert_eq!(
        text_dump(r"printf 'old\nlines\033[2J\033[Hnew'", "10", "3"),
        "new\n\n\n"
    );
}

#[test]
fn child_exit_code_propagates() {
    let output = devtool("printf 'bye'; exit 3", &["--rows", "1", "--format", "text"]);
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(stdout(&output), "bye\n");
}

#[test]
fn signal_exit_reports_128_plus_signal() {
    let output = devtool("kill -TERM $$", &["--rows", "1", "--format", "text"]);
    assert_eq!(output.status.code(), Some(128 + 15));
}

#[test]
fn timeout_kills_the_child_and_still_prints_the_screen() {
    let started = Instant::now();
    let output = devtool(
        "printf 'before'; sleep 30",
        &[
            "--timeout",
            "1",
            "--cols",
            "10",
            "--rows",
            "2",
            "--format",
            "text",
        ],
    );
    let elapsed = started.elapsed();
    assert_eq!(output.status.code(), Some(124));
    assert!(
        // Far below the 10 s default it guards against, with room for a
        // heavily loaded machine (timeout 1 s + hangup grace 0.2 s).
        elapsed < Duration::from_secs(6),
        "took {elapsed:?}, the bar is 6 s"
    );
    assert_eq!(stdout(&output), "before\n\n");
}

#[test]
fn timeout_hangs_up_before_killing() {
    // The shell traps SIGHUP, prints and exits: the hangup arrives first and
    // the trap's output is in the dump.
    let started = Instant::now();
    let output = devtool(
        r"trap 'printf hup; kill $!; exit 0' HUP; printf start; sleep 30 & wait",
        &[
            "--timeout",
            "1",
            "--cols",
            "20",
            "--rows",
            "1",
            "--format",
            "text",
        ],
    );
    assert_eq!(output.status.code(), Some(124));
    assert!(started.elapsed() < Duration::from_secs(6));
    assert_eq!(stdout(&output), "starthup\n");
}

#[test]
fn command_starting_with_a_hyphen_is_a_command() {
    // `sh -c -- -x`: the shell looks for a command called `-x` instead of
    // turning on tracing.
    let output = devtool("-x", &["--rows", "1", "--format", "text"]);
    assert_eq!(output.status.code(), Some(127));
}

#[test]
fn timeout_holds_while_the_child_floods_a_large_screen() {
    // Every line inserts a line and clears a 300x300 screen: far more output
    // than the terminal can parse in a second, so events never stop
    // arriving. The deadline must still win.
    let mut child = Command::new(env!("CARGO_BIN_EXE_tessera-devtool"))
        .args([
            "run",
            r#"yes "$(printf '\033[L\033[2J')""#,
            "--cols",
            "300",
            "--rows",
            "300",
            "--format",
            "text",
            "--timeout",
            "1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the devtool binary starts");
    // Generous: timeout (1 s) + kill grace (1 s) + one slice + start-up on a
    // loaded machine. Unbounded before the fix.
    let bound = Duration::from_secs(10);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait works") {
            break Some(status);
        }
        if started.elapsed() > bound {
            break None;
        }
        thread::sleep(Duration::from_millis(50));
    };
    let Some(status) = status else {
        child.kill().expect("kill the stuck devtool");
        let _ = child.wait();
        panic!("--timeout 1 did not stop a flooding child within {bound:?}");
    };
    assert_eq!(status.code(), Some(124));
}

#[test]
fn child_runs_in_the_current_directory() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .canonicalize()
        .expect("src exists");
    let output = Command::new(env!("CARGO_BIN_EXE_tessera-devtool"))
        .args([
            "run",
            r#"printf %s "$(pwd -P)""#,
            "--cols",
            "400",
            "--rows",
            "1",
            "--format",
            "text",
        ])
        .current_dir(&dir)
        .output()
        .expect("the devtool binary runs");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), format!("{}\n", dir.display()));
}

#[test]
fn closed_stdout_still_reports_the_child_status() {
    // `tessera-devtool … | head -1`: the reader goes away before the dump is
    // written; that is not a tool failure.
    let mut child = Command::new(env!("CARGO_BIN_EXE_tessera-devtool"))
        .args([
            "run",
            "sleep 0.2; exit 3",
            "--format",
            "cells",
            "--timeout",
            "10",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the devtool binary starts");
    drop(child.stdout.take());
    let output = child.wait_with_output().expect("the devtool finishes");
    assert_eq!(
        output.status.code(),
        Some(3),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn cells_label_history_rows_negative_and_mark_wrapped_rows() {
    // `abcde` wraps twice at 2 columns; five rows on a two-row screen push
    // the three rows of `abcde` into the scrollback.
    let output = devtool(
        r"printf 'abcde\nx\ny'",
        &[
            "--cols",
            "2",
            "--rows",
            "2",
            "--format",
            "cells",
            "--scrollback",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let blank = "fg=default bg=default attrs=-";
    let expected = [
        format!("-3:0 \"a\" - {blank}"),
        format!("-3:1 \"b\" - {blank}"),
        "-3:wrapped".to_owned(),
        format!("-2:0 \"c\" - {blank}"),
        format!("-2:1 \"d\" - {blank}"),
        "-2:wrapped".to_owned(),
        format!("-1:0 \"e\" - {blank}"),
        format!("-1:1 \" \" - {blank}"),
        format!("0:0 \"x\" - {blank}"),
        format!("0:1 \" \" - {blank}"),
        format!("1:0 \"y\" - {blank}"),
        format!("1:1 \" \" - {blank}"),
    ];
    assert_eq!(stdout_lines(&output), expected);
}

#[test]
fn cells_format_lists_every_cell_with_markers() {
    let output = devtool(
        r"printf '\344\270\255'",
        &[
            "--cols", "3", "--rows", "1", "--format", "cells", "--cursor",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output),
        "0:0 \"中\" wide fg=default bg=default attrs=-\n\
         0:1 \" \" spacer fg=default bg=default attrs=-\n\
         0:2 \" \" - fg=default bg=default attrs=- <cursor\n"
    );
}

#[test]
fn ansi_is_the_default_format() {
    let output = devtool("printf 'hi'", &["--cols", "4", "--rows", "2"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "hi\x1b[0m\n\x1b[0m\n");
}

#[test]
fn cursor_flag_marks_the_cursor_in_text() {
    let output = devtool(
        "printf 'ab\\nc'",
        &["--cols", "5", "--rows", "2", "--format", "text", "--cursor"],
    );
    // The marker follows the cursor's cell: the blank after `c`.
    assert_eq!(stdout(&output), "ab\nc |\n");
}

#[test]
fn scrollback_flag_prints_history_above_the_screen() {
    let screen_only = text_dump("printf '1\\n2\\n3\\n4\\n5'", "4", "2");
    assert_eq!(screen_only, "4\n5\n");
    let output = devtool(
        "printf '1\\n2\\n3\\n4\\n5'",
        &[
            "--cols",
            "4",
            "--rows",
            "2",
            "--format",
            "text",
            "--scrollback",
        ],
    );
    assert_eq!(stdout(&output), "1\n2\n3\n4\n5\n");
}

#[test]
fn soft_wrapped_rows_end_with_a_backslash() {
    assert_eq!(text_dump("printf 'abcdef'", "4", "2"), "abcd\\\nef\n");
}

#[test]
fn zero_size_is_a_usage_error() {
    let output = devtool("true", &["--cols", "0"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn library_run_returns_the_terminal_and_status() {
    let options = RunOptions {
        cols: 12,
        rows: 2,
        // Well above the expected ~0.1 s, so load cannot turn it into 124.
        timeout: Duration::from_secs(60),
        ..RunOptions::new("printf 'from lib'; exit 7")
    };
    let outcome = run::run(&options).expect("sh starts");
    assert_eq!(
        outcome.completion,
        Completion::Exited(tessera_pty::ExitStatus::Exited(7))
    );
    assert_eq!(outcome.exit_code(), 7);
    assert_eq!(dump::text(outcome.terminal.grid()), "from lib\n\n");
}

#[test]
fn library_run_rejects_a_zero_size() {
    let options = RunOptions {
        cols: 0,
        ..RunOptions::new("true")
    };
    let error = run::run(&options).expect_err("a zero-width pty is refused");
    assert!(
        matches!(error, run::Error::Spawn(PtyError::InvalidSize(_))),
        "{error:?}"
    );
}

#[test]
fn hand_styled_grid_dumps_exact_sgr() {
    let mut grid = Grid::new(2, 12);
    grid.set_pen(Style::DEFAULT.with(Style::BOLD));
    grid.put_str("B");
    grid.set_pen(Style {
        fg: Color::Indexed(1),
        ..Style::DEFAULT.with(Style::ITALIC | Style::UNDERLINE)
    });
    grid.put_str("r");
    grid.set_pen(Style {
        fg: Color::Indexed(12),
        bg: Color::Indexed(3),
        ..Style::DEFAULT.with(Style::DIM | Style::BLINK | Style::INVERSE)
    });
    grid.put_str("y");
    grid.set_pen(Style {
        fg: Color::Indexed(196),
        bg: Color::Rgb(0, 128, 255),
        ..Style::DEFAULT.with(Style::HIDDEN | Style::STRIKETHROUGH)
    });
    grid.put_str("x");
    grid.set_pen(Style {
        fg: Color::Rgb(1, 2, 3),
        bg: Color::Indexed(232),
        ..Style::DEFAULT.with(Style::DOUBLE_UNDERLINE)
    });
    grid.put_str("中");
    grid.set_pen(Style::DEFAULT);
    grid.put_str("p");
    // A blank with a background survives trimming; its pen keeps only bg.
    grid.set_pen(Style {
        bg: Color::Indexed(10),
        ..Style::DEFAULT
    });
    grid.put_str(" ");
    grid.set_pen(Style::DEFAULT);
    grid.carriage_return();
    grid.line_feed();
    grid.put_str("plain");

    assert_eq!(
        dump::ansi(&grid),
        "\x1b[0;1mB\
         \x1b[0;3;4;31mr\
         \x1b[0;2;5;7;94;43my\
         \x1b[0;8;9;38;5;196;48;2;0;128;255mx\
         \x1b[0;21;38;2;1;2;3;48;5;232m中\
         \x1b[0mp\
         \x1b[0;102m \
         \x1b[0m\n\
         plain\x1b[0m\n"
    );
}

#[test]
fn ffi_and_app_core_do_not_depend_on_the_devtool() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("metadata is JSON");

    let reachable_from_ffi = linked_closure(&metadata, "tessera-ffi");
    let reachable_from_app_core = linked_closure(&metadata, "tessera-app-core");
    let devtool = package_id(&metadata, "tessera-devtool");
    // The walk is real: the app links the emulator the devtool also uses.
    assert!(reachable_from_ffi.contains(&package_id(&metadata, "tessera-term")));
    assert!(
        !reachable_from_ffi.contains(&devtool),
        "tessera-ffi links tessera-devtool"
    );
    assert!(
        !reachable_from_app_core.contains(&devtool),
        "tessera-app-core links tessera-devtool"
    );
}

/// The id of the workspace package called `name`.
fn package_id(metadata: &Value, name: &str) -> String {
    metadata["packages"]
        .as_array()
        .expect("packages is an array")
        .iter()
        .find(|package| package["name"] == name)
        .and_then(|package| package["id"].as_str())
        .unwrap_or_else(|| panic!("{name} is a workspace package"))
        .to_owned()
}

/// Every package linked into `root`: the transitive closure over normal and
/// build dependencies. Dev-dependencies are not followed because they never
/// end up in the artifact.
fn linked_closure(metadata: &Value, root: &str) -> HashSet<String> {
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolve.nodes is an array");
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([package_id(metadata, root)]);
    while let Some(id) = queue.pop_front() {
        let node = nodes
            .iter()
            .find(|node| node["id"] == id.as_str())
            .unwrap_or_else(|| panic!("{id} is in the resolve graph"));
        let deps = node["deps"].as_array().expect("deps is an array");
        for dep in deps {
            let linked = dep["dep_kinds"]
                .as_array()
                .expect("dep_kinds is an array")
                .iter()
                .any(|kind| kind["kind"].is_null() || kind["kind"] == "build");
            let pkg = dep["pkg"].as_str().expect("pkg is a string").to_owned();
            if linked && seen.insert(pkg.clone()) {
                queue.push_back(pkg);
            }
        }
    }
    seen
}
