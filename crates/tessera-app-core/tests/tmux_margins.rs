//! Live check of left and right margins (#97) with the program that needs
//! them: tmux, which scrolls one of two side-by-side panes with DECSLRM
//! (`CSI Pl ; Pr s`) once DA1 lists 21. Output flooding the left pane must
//! never write into the right one: after `seq 1 500` in the left pane,
//! each pane's cells in Tessera's grid equal what `tmux capture-pane`
//! says the pane holds.
//!
//! Skipped (with a message) when tmux is not installed. tmux runs on a
//! private socket (`-L tessera-test-<pid>`), without a configuration file,
//! with `HOME`, `XDG_CONFIG_HOME` and `TMUX_TMPDIR` in a temporary
//! directory, and the server is killed at the end.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tessera_app_core::{App, Config, ConfigPaths, PtySize, SessionId, SpawnOptions};

/// How long tmux gets to start, split, run `seq` and redraw.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Where `name` is on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// A tmux server on a private socket, killed on drop.
struct Tmux<'a> {
    program: &'a Path,
    socket: String,
    home: &'a Path,
}

impl Tmux<'_> {
    /// Runs a tmux client command against the server and returns its
    /// standard output.
    fn run(&self, args: &[&str]) -> String {
        Command::new(self.program)
            .args(["-L", &self.socket])
            .args(args)
            .env("TMUX_TMPDIR", self.home)
            .env("HOME", self.home)
            .env_remove("TMUX")
            .output()
            .map_or_default(|out| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `(left, top, width, height)` of pane `target`.
    fn geometry(&self, target: &str) -> Option<(usize, usize, usize, usize)> {
        let out = self.run(&[
            "display",
            "-p",
            "-t",
            target,
            "#{pane_left} #{pane_top} #{pane_width} #{pane_height}",
        ]);
        let numbers: Vec<usize> = out
            .split_whitespace()
            .filter_map(|n| n.parse().ok())
            .collect();
        match numbers[..] {
            [left, top, width, height] => Some((left, top, width, height)),
            _ => None,
        }
    }

    /// What tmux says pane `target` shows, one string per row, trailing
    /// blanks trimmed.
    fn capture(&self, target: &str) -> Vec<String> {
        self.run(&["capture-pane", "-p", "-t", target])
            .lines()
            .map(|line| line.trim_end().to_owned())
            .collect()
    }
}

impl Drop for Tmux<'_> {
    fn drop(&mut self) {
        self.run(&["kill-server"]);
    }
}

/// The text of Tessera's cells in the rectangle of a pane, one string per
/// row, trailing blanks trimmed.
fn pane_text(
    app: &App,
    id: SessionId,
    (left, top, width, height): (usize, usize, usize, usize),
) -> Vec<String> {
    app.with_terminal(id, |term| {
        term.grid().visible_lines()[top..top + height]
            .iter()
            .map(|line| {
                line.cells[left..left + width]
                    .iter()
                    .map(|cell| cell.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Polls `done` until it holds or [`TIMEOUT`] passes.
fn wait(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    done()
}

#[test]
fn tmux_side_by_side_panes_scroll_inside_their_margins() {
    let Some(program) = which("tmux") else {
        eprintln!("SKIP tmux: not installed (brew install tmux)");
        return;
    };
    let home = tempfile::tempdir().expect("temp dir");
    let home = home.path();
    let tmux = Tmux {
        program: &program,
        socket: format!("tessera-test-{}", std::process::id()),
        home,
    };
    let app = App::new(
        Config::default(),
        ConfigPaths::new(home.join("tessera-config")),
    );
    let options = SpawnOptions::command(&program, "margins-probe")
        .args([
            "-L",
            tmux.socket.as_str(),
            "-f",
            "/dev/null",
            "new-session",
            ";",
            "split-window",
            "-h",
            ";",
            "select-pane",
            "-t",
            ":.0",
        ])
        .cwd(home)
        .env("TERM", "xterm-256color")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("TMUX_TMPDIR", home)
        .env("SHELL", "/bin/sh")
        .env("PS1", "$ ");
    let options = SpawnOptions {
        size: PtySize::new(24, 80),
        ..options
    };
    let id = app.spawn_session(options).expect("spawn tmux");

    assert!(
        wait(|| tmux.run(&["display", "-p", "#{window_panes}"]).trim() == "2"),
        "tmux never split the window"
    );
    let left = tmux.geometry(":.0").expect("left pane geometry");
    let right = tmux.geometry(":.1").expect("right pane geometry");
    assert!(
        left.0 + left.2 < right.0,
        "panes side by side: {left:?} {right:?}"
    );

    tmux.run(&["send-keys", "-t", ":.1", "echo right-pane-marker", "Enter"]);
    assert!(
        wait(|| pane_text(&app, id, right)
            .iter()
            .any(|row| row == "right-pane-marker")),
        "the right pane never showed its marker: {:?}",
        pane_text(&app, id, right)
    );
    tmux.run(&["send-keys", "-t", ":.0", "seq 1 500", "Enter"]);

    // Both panes as tmux has them, and as Tessera drew them, agree once
    // tmux has caught up.
    let agree = || {
        let left_now = tmux.capture(":.0");
        left_now.iter().any(|row| row == "500")
            && pane_text(&app, id, left) == left_now
            && pane_text(&app, id, right) == tmux.capture(":.1")
    };
    assert!(
        wait(agree),
        "panes differ from tmux after the flood:\nleft  tessera {:?}\nleft  tmux    {:?}\nright tessera {:?}\nright tmux    {:?}",
        pane_text(&app, id, left),
        tmux.capture(":.0"),
        pane_text(&app, id, right),
        tmux.capture(":.1"),
    );
    // tmux scrolled the left pane with left/right margins, so the test ran
    // the margin path and not a full redraw.
    let margins_on = app
        .with_terminal(id, |term| term.modes().left_right_margins)
        .expect("live session");
    assert!(
        margins_on,
        "tmux never turned on DECLRMM (DA1 21 not seen?)"
    );
}
