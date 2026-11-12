//! Live check of mouse reporting (#12) with the programs users run it for:
//! vim, tmux and htop, each in an `App` session at 80x24 that receives
//! mouse events through [`App::send_input`], exactly as the frontend sends
//! them.
//!
//! - **vim** (`-u NONE -N -c 'set mouse=a'`) on 30 numbered lines: a left
//!   click on row 10, column 3 moves vim's cursor there.
//! - **tmux** with `mouse on` and two panes side by side: a click in the
//!   inactive pane makes it the active one (`#{pane_index}`, queried with
//!   `tmux display -p`).
//! - **htop**: a click on a column header makes it the sort column, which
//!   htop marks by drawing that header in a style of its own.
//!
//! Ignored, and not run by the quality gate: it needs the programs and is
//! slow. Run it with
//! `cargo nextest run -p tessera-app-core --test mouse_apps --run-ignored only --no-capture`.
//! A program that is not installed is skipped with a message such as
//! `SKIP htop: not installed (brew install htop)`, and the test prints
//! which probes ran. Every child runs with `HOME`, `XDG_CONFIG_HOME` and
//! `TMUX_TMPDIR` in a temporary directory, so nothing reads or writes the
//! real home directory.
//!
//! Each probe runs twice: with `TERM=xterm-256color`, the default (ADR
//! 0005), and with `TERM=tessera` and `TERMINFO` set to the entry from
//! `shell-integration/terminfo/tessera.terminfo` compiled with `tic -x`
//! into the temporary directory (#194.9), as the app bundles it. Without
//! `tic` the `tessera` runs are skipped with a message.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tessera_app_core::input::{MouseButton, MouseEvent, MouseEventKind};
use tessera_app_core::{
    App, Config, ConfigPaths, InputEvent, InputOutcome, PtySize, SessionId, SpawnOptions,
};
use tessera_grid::Point;
use tessera_term::{MouseEncoding, MouseTracking};

/// How long a program gets to draw, turn on mouse tracking, or react to a
/// click.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Where `name` is on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// The terminal type a probe's child runs with.
#[derive(Debug, Clone)]
struct Term {
    /// `TERM`.
    name: &'static str,
    /// `TERMINFO`, for an entry that is not installed.
    terminfo: Option<PathBuf>,
}

impl Term {
    /// `TERM=xterm-256color`, from the system's terminfo.
    fn xterm() -> Self {
        Self {
            name: "xterm-256color",
            terminfo: None,
        }
    }

    /// `TERM=tessera`, its entry compiled with `tic -x` into `dir`.
    fn tessera(dir: &Path) -> Result<Self, String> {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../shell-integration/terminfo/tessera.terminfo");
        let terminfo = dir.join("terminfo");
        let status = Command::new("tic")
            .arg("-x")
            .arg("-o")
            .arg(&terminfo)
            .arg(&source)
            .status()
            .map_err(|e| format!("tic: {e}"))?;
        if !status.success() {
            return Err(format!("tic {}: {status}", source.display()));
        }
        Ok(Self {
            name: "tessera",
            terminfo: Some(terminfo),
        })
    }
}

/// A program running in a session of its own `App`.
struct Running {
    app: App,
    id: SessionId,
}

impl Running {
    /// Starts `program` with `args` at 80x24 under `term`, its home in
    /// `home`.
    fn start(program: &Path, args: &[&str], home: &Path, term: &Term) -> Self {
        let app = App::new(
            Config::default(),
            ConfigPaths::new(home.join("tessera-config")),
        );
        let mut options = SpawnOptions::command(program, "mouse-probe")
            .args(args)
            .cwd(home)
            .env("TERM", term.name)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home)
            .env("TMUX_TMPDIR", home)
            .env("SHELL", "/bin/sh");
        if let Some(terminfo) = &term.terminfo {
            options = options.env("TERMINFO", terminfo);
        }
        let options = SpawnOptions {
            size: PtySize::new(24, 80),
            ..options
        };
        let id = app.spawn_session(options).expect("spawn the program");
        Self { app, id }
    }

    /// The visible rows as text.
    fn screen(&self) -> Vec<String> {
        self.app
            .with_terminal(self.id, |term| {
                term.grid()
                    .visible_lines()
                    .iter()
                    .map(tessera_grid::Line::text)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The tracking mode and encoding the program turned on.
    fn mouse_modes(&self) -> (MouseTracking, MouseEncoding) {
        self.app
            .with_terminal(self.id, |term| {
                (term.modes().mouse, term.modes().mouse_encoding)
            })
            .expect("live session")
    }

    /// Polls `done` until it holds or [`TIMEOUT`] passes.
    fn wait(&self, mut done: impl FnMut(&Self) -> bool) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if done(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        done(self)
    }

    /// A left click (press and release) on cell (`col`, `row`); both must
    /// be reported.
    fn click(&self, col: u16, row: u16) {
        for kind in [MouseEventKind::Press, MouseEventKind::Release] {
            let event = MouseEvent::new(kind, MouseButton::Left, col, row);
            let outcome = self
                .app
                .send_input(self.id, InputEvent::Mouse(event))
                .expect("send the click");
            assert_eq!(outcome, InputOutcome::Consumed, "{kind:?} not reported");
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.app.close_session(self.id);
    }
}

/// vim: a click moves the cursor to the clicked cell.
fn vim(program: &Path, home: &Path, term: &Term) -> Result<String, String> {
    let file = home.join("numbers.txt");
    let lines = (1..=30).fold(String::new(), |mut text, n| {
        let _ = writeln!(text, "line {n:02}");
        text
    });
    std::fs::write(&file, lines).map_err(|e| e.to_string())?;
    let file = file.to_string_lossy().into_owned();
    let vim = Running::start(
        program,
        &["-u", "NONE", "-i", "NONE", "-N", "-c", "set mouse=a", &file],
        home,
        term,
    );
    if !vim.wait(|vim| {
        vim.screen()
            .first()
            .is_some_and(|row| row.starts_with("line 01"))
            && vim.mouse_modes().0 != MouseTracking::None
    }) {
        return Err(format!(
            "vim never drew the file with mouse tracking on: {:?}, {:?}",
            vim.mouse_modes(),
            vim.screen().first()
        ));
    }
    let modes = vim.mouse_modes();
    // Row 10, column 3 (one-based): "line 10", on its 'n'.
    vim.click(2, 9);
    let moved = vim.wait(|vim| {
        vim.app
            .with_terminal(vim.id, |term| term.grid().cursor())
            .is_ok_and(|cursor| cursor == Point::new(9, 2))
    });
    let cursor = vim.app.with_terminal(vim.id, |term| term.grid().cursor());
    if moved {
        Ok(format!("cursor moved to row 10, col 3 ({modes:?})"))
    } else {
        Err(format!("cursor at {cursor:?} after the click ({modes:?})"))
    }
}

/// tmux: a click in the inactive pane makes it active.
fn tmux(program: &Path, home: &Path, term: &Term) -> Result<String, String> {
    let socket = format!("tessera-test-{}", std::process::id());
    let tmux = Running::start(
        program,
        &[
            "-L",
            &socket,
            "-f",
            "/dev/null",
            "new-session",
            ";",
            "set",
            "-g",
            "mouse",
            "on",
            ";",
            "split-window",
            "-h",
        ],
        home,
        term,
    );
    let query = |format: &str| {
        Command::new(program)
            .args(["-L", &socket, "display", "-p", format])
            .env("TMUX_TMPDIR", home)
            .env("HOME", home)
            .env_remove("TMUX")
            .output()
            .map_or_default(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let result = (|| {
        if !tmux.wait(|tmux| {
            tmux.mouse_modes().0 != MouseTracking::None && query("#{window_panes}") == "2"
        }) {
            return Err(format!(
                "tmux never split with mouse tracking on: {:?}, panes {:?}",
                tmux.mouse_modes(),
                query("#{window_panes}")
            ));
        }
        let modes = tmux.mouse_modes();
        let before = query("#{pane_index}");
        // The new pane (right, index 1) is active; click in the left one.
        tmux.click(5, 5);
        let after = || query("#{pane_index}");
        if tmux.wait(|_| after() != before) {
            Ok(format!(
                "active pane {before} -> {} after the click ({modes:?})",
                after()
            ))
        } else {
            Err(format!(
                "active pane stayed {before} after the click ({modes:?})"
            ))
        }
    })();
    let _ = Command::new(program)
        .args(["-L", &socket, "kill-server"])
        .env("TMUX_TMPDIR", home)
        .status();
    result
}

/// htop: a click on a column header makes it the sort column.
fn htop(program: &Path, home: &Path, term: &Term) -> Result<String, String> {
    let htop = Running::start(program, &[], home, term);
    let header = |htop: &Running| {
        htop.screen()
            .iter()
            .position(|row| row.contains("PID") && row.contains("Command"))
    };
    if !htop.wait(|htop| header(htop).is_some() && htop.mouse_modes().0 != MouseTracking::None) {
        return Err(format!(
            "htop never drew its header with mouse tracking on: {:?}",
            htop.mouse_modes()
        ));
    }
    let modes = htop.mouse_modes();
    let row = header(&htop).expect("found above");
    let row16 = u16::try_from(row).expect("a screen row");
    // Each header label and the style of its first cell; the sort column
    // is the one drawn differently.
    let labels = |htop: &Running| -> Vec<(String, u16, tessera_grid::Style)> {
        htop.app
            .with_terminal(htop.id, |term| {
                let text = term
                    .grid()
                    .line(row16)
                    .map_or_default(tessera_grid::Line::text);
                // Labels are separated by spaces, or by the sort direction
                // mark htop draws after the sort column's label.
                let separator = |ch: char| ch == ' ' || !ch.is_ascii();
                let mut out: Vec<(String, u16, tessera_grid::Style)> = Vec::new();
                let mut in_label = false;
                for (col, ch) in text.chars().enumerate() {
                    if separator(ch) {
                        in_label = false;
                        continue;
                    }
                    if in_label {
                        if let Some((name, _, _)) = out.last_mut() {
                            name.push(ch);
                        }
                        continue;
                    }
                    in_label = true;
                    let at = u16::try_from(col).expect("a column");
                    let style = term
                        .grid()
                        .cell(Point::new(row16, at))
                        .map_or_default(|cell| cell.style);
                    out.push((ch.to_string(), at, style));
                }
                out
            })
            .unwrap_or_default()
    };
    let sorted = |labels: &[(String, u16, tessera_grid::Style)]| -> Option<String> {
        labels
            .iter()
            .find(|(_, _, style)| labels.iter().filter(|(_, _, other)| other == style).count() == 1)
            .map(|(name, _, _)| name.clone())
    };
    let first = labels(&htop);
    let Some(before) = sorted(&first) else {
        return Err(format!("no sort column stands out in {first:?}"));
    };
    let Some((target, col, _)) = first
        .iter()
        .find(|(name, _, _)| *name != before && (name == "PID" || name == "USER"))
        .cloned()
    else {
        return Err(format!("no PID or USER header in {first:?}"));
    };
    htop.click(col + 1, row16);
    if htop.wait(|htop| sorted(&labels(htop)).as_deref() == Some(target.as_str())) {
        Ok(format!("sort column {before} -> {target} ({modes:?})"))
    } else {
        Err(format!(
            "sort column {:?} after clicking {target} ({modes:?})",
            sorted(&labels(&htop))
        ))
    }
}

type ProbeFn = fn(&Path, &Path, &Term) -> Result<String, String>;

#[test]
#[ignore = "live: runs vim, tmux and htop; not part of the gate"]
fn programs_respond_to_mouse_reports() {
    let probes: [(&str, &str, ProbeFn); 3] = [
        ("vim", "brew install vim", vim),
        ("tmux", "brew install tmux", tmux),
        ("htop", "brew install htop", htop),
    ];
    let mut ran = Vec::new();
    let mut failures = Vec::new();
    for (name, install, probe) in probes {
        let Some(program) = which(name) else {
            eprintln!("SKIP {name}: not installed ({install})");
            continue;
        };
        for tessera in [false, true] {
            let home = tempfile::tempdir().expect("temp dir");
            let term = if tessera {
                match Term::tessera(home.path()) {
                    Ok(term) => term,
                    Err(why) => {
                        eprintln!("SKIP {name} with TERM=tessera: {why}");
                        continue;
                    }
                }
            } else {
                Term::xterm()
            };
            let label = format!("{name} with TERM={}", term.name);
            match probe(&program, home.path(), &term) {
                Ok(what) => {
                    eprintln!("RAN {label}: {what}");
                    ran.push(label);
                }
                Err(why) => failures.push(format!("{label}: {why}")),
            }
        }
    }
    eprintln!("probes that ran and passed: {ran:?}");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
