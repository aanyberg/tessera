//! Live probe check of the reply path (#10): real programs that query the
//! terminal at start-up (vim, nvim, tmux, fzf, less, htop and fish) run in
//! an `App` session at 80x24, and every query they send must be answered,
//! except the deliberate silences of `reports_spec.txt` (ADR 0024).
//!
//! Ignored, and not run by the quality gate: it needs the programs and is
//! slow. Run it with
//! `cargo nextest run -p tessera-app-core --test probe_replies --run-ignored only --no-capture`.
//! A program that is not installed is skipped with a message such as
//! `SKIP htop: not installed (brew install htop)`.
//!
//! Each program runs behind a small relay (`python3`, written to a
//! temporary directory) that gives it its own PTY of the same size and
//! records what it writes (queries) and what it reads (replies) with
//! timestamps. The test then:
//!
//! - checks the program drew its first screen within 1 s of the spawn;
//! - finds every query in its output (`CSI ... c`, `n`, `$ p`, `> q`,
//!   `? u`, the XTWINOPS reports, `DCS $ q` and `DCS + q`, the OSC colour
//!   and clipboard queries), asks a fresh [`Terminal`] whether Tessera
//!   answers it, and checks the replies the program read are those, in
//!   query order (by kind: introducer, markers and final byte);
//! - writes a transcript of every query and reply to
//!   `target/probe-replies/<program>.txt`.
//!
//! The deliberate silences: `CSI 13 t` (vim's `getwinpos()`, which then
//! waits its own timeout), `CSI 11/15/20/21 t` and `14 ; 2 t`, `CSI 14 t`
//! and `16 t` while the session has no view (the cell size is unknown),
//! XTGETTCAP (`DCS + q`), tmux's colour scheme query `CSI ? 996 n` (a
//! DEC DSR outside D-DSRSET) and, until #10's colour part lands, the OSC
//! 4/10/11/12 colour queries.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tessera_app_core::{App, Config, ConfigPaths, PtySize, SessionEvent, SpawnOptions};
use tessera_term::{Request, Terminal};

/// How long a program gets for its first screen.
const FIRST_SCREEN: Duration = Duration::from_secs(1);

/// How long a program runs after its first screen, so the queries it sends
/// later (and their timeouts) are in the transcript.
const SETTLE: Duration = Duration::from_millis(2500);

/// The relay: runs `argv[2..]` in a PTY of the relay's own size and logs
/// each read as `OUT|IN <seconds> <len>\n<bytes>\n` to `argv[1]`.
const RELAY: &str = r"
import fcntl, os, pty, select, sys, termios, time, tty
log = open(sys.argv[1], 'wb')
ws = fcntl.ioctl(0, termios.TIOCGWINSZ, b'\0' * 8)
pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, ws)
    os.execvp(sys.argv[2], sys.argv[2:])
tty.setraw(0)
t0 = time.monotonic()
def rec(tag, data):
    log.write(b'%s %.6f %d\n' % (tag, time.monotonic() - t0, len(data)) + data + b'\n')
    log.flush()
while True:
    r, _, _ = select.select([0, fd], [], [])
    if fd in r:
        try:
            d = os.read(fd, 65536)
        except OSError:
            break
        if not d:
            break
        rec(b'OUT', d)
        os.write(1, d)
    if 0 in r:
        d = os.read(0, 65536)
        if not d:
            break
        rec(b'IN', d)
        os.write(fd, d)
";

/// One program to probe.
struct Probe {
    name: &'static str,
    args: &'static [&'static str],
    install: &'static str,
}

const PROBES: &[Probe] = &[
    Probe {
        name: "vim",
        args: &["--clean", "-c", "set title"],
        install: "brew install vim",
    },
    Probe {
        name: "nvim",
        args: &["--clean"],
        install: "brew install neovim",
    },
    Probe {
        name: "tmux",
        args: &["-L", "tessera-probe", "-f", "/dev/null", "new-session"],
        install: "brew install tmux",
    },
    Probe {
        name: "fzf",
        args: &[],
        install: "brew install fzf",
    },
    Probe {
        name: "less",
        args: &["/etc/hosts"],
        install: "brew install less",
    },
    Probe {
        name: "htop",
        args: &[],
        install: "brew install htop",
    },
    Probe {
        name: "fish",
        args: &["--no-config"],
        install: "brew install fish",
    },
];

/// Where `name` is on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// A control sequence found in a byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seq {
    /// The bytes, introducer to terminator.
    bytes: Vec<u8>,
    /// `b'['` (CSI), `b'P'` (DCS) or `b']'` (OSC).
    intro: u8,
    /// The parameter bytes (`0x30..=0x3F`) of a CSI or DCS header, or an
    /// OSC's payload.
    params: Vec<u8>,
    /// The intermediate bytes (`0x20..=0x2F`) of a CSI or DCS header.
    inters: Vec<u8>,
    /// The final byte of a CSI or DCS header.
    fin: u8,
    /// A DCS's payload after its header.
    data: Vec<u8>,
}

impl Seq {
    /// What a reply is matched by: introducer, private marker,
    /// intermediates and final byte. A DCS reply also by its whole header,
    /// so the status (`1$r` / `0$r`, `!|`, `>|`) counts, and by the last
    /// payload byte (a DECRQSS reply's setting: `m`, `r`, `q`, `p`), never
    /// by the payload itself, which depends on the terminal's state.
    fn kind(&self) -> String {
        let marker = self
            .params
            .first()
            .filter(|b| b"<=>?".contains(b))
            .map(|&b| char::from(b));
        let mut kind = format!(
            "{}{}{}{}",
            char::from(self.intro),
            marker.map(String::from).unwrap_or_default(),
            String::from_utf8_lossy(&self.inters),
            char::from(self.fin)
        );
        if self.intro == b'P' {
            let setting = self.data.last().map(|&b| char::from(b));
            kind = format!(
                "P{}{}{}{}",
                String::from_utf8_lossy(&self.params),
                String::from_utf8_lossy(&self.inters),
                char::from(self.fin),
                setting.map(String::from).unwrap_or_default()
            );
        }
        kind
    }
}

/// Every CSI, DCS and OSC sequence in `bytes` (7-bit introducers).
fn sequences(bytes: &[u8]) -> Vec<Seq> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != 0x1b || !b"[P]".contains(&bytes[i + 1]) {
            i += 1;
            continue;
        }
        let start = i;
        let intro = bytes[i + 1];
        i += 2;
        let mut seq = Seq {
            bytes: Vec::new(),
            intro,
            params: Vec::new(),
            inters: Vec::new(),
            fin: 0,
            data: Vec::new(),
        };
        if intro == b']' {
            while i < bytes.len() && bytes[i] != 0x07 && !bytes[i..].starts_with(b"\x1b\\") {
                seq.params.push(bytes[i]);
                i += 1;
            }
            i += if bytes.get(i) == Some(&0x07) { 1 } else { 2 };
        } else {
            while i < bytes.len() && (0x30..=0x3F).contains(&bytes[i]) {
                seq.params.push(bytes[i]);
                i += 1;
            }
            while i < bytes.len() && (0x20..=0x2F).contains(&bytes[i]) {
                seq.inters.push(bytes[i]);
                i += 1;
            }
            let Some(&fin) = bytes.get(i) else { break };
            seq.fin = fin;
            i += 1;
            if intro == b'P' {
                while i < bytes.len() && !bytes[i..].starts_with(b"\x1b\\") {
                    seq.data.push(bytes[i]);
                    i += 1;
                }
                i += 2;
            }
        }
        seq.bytes = bytes[start..i.min(bytes.len())].to_vec();
        out.push(seq);
    }
    out
}

/// Whether `seq` (from the program's output) asks the terminal something.
fn is_query(seq: &Seq) -> bool {
    let first = seq.params.split(|&b| b == b';').next().unwrap_or_default();
    match (seq.intro, seq.fin) {
        (b'[', b'c' | b'n') => true,
        (b'[', b'p') => seq.inters == b"$",
        (b'[', b'q') => seq.params.starts_with(b">"),
        (b'[', b'u') => seq.params.starts_with(b"?"),
        (b'[', b't') => matches!(
            first,
            b"11" | b"13" | b"14" | b"15" | b"16" | b"18" | b"19" | b"20" | b"21"
        ),
        (b'P', b'q') => seq.inters == b"$" || seq.inters == b"+",
        (b']', _) => {
            let mut pieces = seq.params.split(|&b| b == b';');
            let command = pieces.next().unwrap_or_default();
            matches!(command, b"4" | b"10" | b"11" | b"12" | b"52") && seq.params.ends_with(b"?")
        }
        _ => false,
    }
}

/// Why Tessera stays silent on `seq` on purpose, if it does.
fn deliberate_silence(seq: &Seq) -> Option<&'static str> {
    let first = seq.params.split(|&b| b == b';').next().unwrap_or_default();
    match (seq.intro, seq.fin) {
        (b'[', b't') if first == b"13" => Some("CSI 13 t (window position, D-WIN2)"),
        (b'[', b't') if matches!(first, b"11" | b"15" | b"20" | b"21") => {
            Some("window state, screen size or title report (D-WIN2, D-SECURITY)")
        }
        (b'[', b't') if seq.params == b"14;2" => Some("CSI 14 ; 2 t (D-WIN2)"),
        (b'[', b't') if matches!(first, b"14" | b"16") => {
            Some("pixel size without a view (D-CELLPX)")
        }
        (b'[', b'n') if seq.params == b"?996" => {
            Some("colour scheme query (theme updates, ?2031, are not implemented; D-DSRSET)")
        }
        (b'P', b'q') if seq.inters == b"+" => Some("XTGETTCAP (out of scope)"),
        (b']', _) => Some("OSC colour or clipboard query (#10 colours, not this part)"),
        _ => None,
    }
}

/// Tessera's reply to `query` alone, from a fresh 24x80 terminal.
fn tessera_reply(query: &[u8]) -> Vec<u8> {
    let mut term = Terminal::new(24, 80);
    term.advance(query);
    term.take_requests()
        .into_iter()
        .filter_map(|request| match request {
            Request::Reply(bytes) => Some(bytes),
            _ => None,
        })
        .flatten()
        .collect()
}

/// One read the relay logged.
struct Record {
    out: bool,
    at: f64,
    bytes: Vec<u8>,
}

fn read_log(path: &Path) -> Vec<Record> {
    let log = std::fs::read(path).unwrap_or_default();
    let mut records = Vec::new();
    let mut rest = &log[..];
    while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
        let header = String::from_utf8_lossy(&rest[..newline]).into_owned();
        let mut words = header.split(' ');
        let (Some(tag), Some(at), Some(len)) = (words.next(), words.next(), words.next()) else {
            break;
        };
        let len: usize = len.parse().unwrap_or(0);
        let body = &rest[newline + 1..];
        if body.len() < len {
            break;
        }
        records.push(Record {
            out: tag == "OUT",
            at: at.parse().unwrap_or(0.0),
            bytes: body[..len].to_vec(),
        });
        rest = &body[(len + 1).min(body.len())..];
    }
    records
}

/// Runs `program` behind the relay in an `App` session, logging to `log`,
/// and returns when it drew its first screen (`None`: never).
fn launch(probe: &Probe, program: &Path, dir: &Path, log: &Path) -> Option<Duration> {
    let relay = dir.join("relay.py");
    let app = App::new(Config::default(), ConfigPaths::new(dir.join("config")));
    let (tx, events) = mpsc::channel();
    app.set_observer(Some(Box::new(move |_, event| {
        let _ = tx.send(event);
    })));
    let options = SpawnOptions::command("python3", "probe")
        .arg("-I")
        .arg(&relay)
        .arg(log)
        .arg(program)
        .args(probe.args)
        .env("TERM", "xterm-256color")
        .env("FZF_DEFAULT_COMMAND", "printf 'alpha\\nbeta\\ngamma\\n'");
    let options = SpawnOptions {
        size: PtySize::new(24, 80),
        ..options
    };
    let started = Instant::now();
    let id = app.spawn_session(options).expect("spawn the relay");
    let mut first_screen = None;
    let deadline = started + FIRST_SCREEN + SETTLE;
    while Instant::now() < deadline {
        if let Ok(SessionEvent::Exit(_)) = events.recv_timeout(Duration::from_millis(5)) {
            break;
        }
        if first_screen.is_none() {
            let drawn = app
                .with_terminal(id, |term| {
                    term.grid()
                        .visible_lines()
                        .iter()
                        .any(|line| !line.text().trim().is_empty())
                })
                .unwrap_or(false);
            if drawn {
                first_screen = Some(started.elapsed());
            }
        }
    }
    let _ = app.close_session(id);
    drop(app);
    if probe.name == "tmux" {
        let _ = std::process::Command::new(program)
            .args(["-L", "tessera-probe", "kill-server"])
            .status();
    }
    first_screen
}

/// Runs one probe; returns its failures (empty: passed) or `None` when the
/// program is not installed.
fn run(probe: &Probe, dir: &Path, out_dir: &Path) -> Option<Vec<String>> {
    let Some(program) = which(probe.name) else {
        eprintln!("SKIP {}: not installed ({})", probe.name, probe.install);
        return None;
    };
    let log = dir.join(format!("{}.log", probe.name));
    let first_screen = launch(probe, &program, dir, &log);
    let records = read_log(&log);
    let output: Vec<u8> = records
        .iter()
        .filter(|r| r.out)
        .flat_map(|r| r.bytes.clone())
        .collect();
    let input: Vec<u8> = records
        .iter()
        .filter(|r| !r.out)
        .flat_map(|r| r.bytes.clone())
        .collect();
    let mut failures = Vec::new();
    let mut transcript = String::new();
    let _ = writeln!(
        transcript,
        "# {} {:?}: first screen after {first_screen:?}",
        probe.name, probe.args
    );
    for record in &records {
        let found: Vec<Seq> = sequences(&record.bytes)
            .into_iter()
            .filter(|seq| !record.out || is_query(seq))
            .collect();
        for seq in found {
            let _ = writeln!(
                transcript,
                "{:9.3} {} {}",
                record.at,
                if record.out { ">> query" } else { "<< reply" },
                seq.bytes.escape_ascii()
            );
        }
    }
    match first_screen {
        Some(after) if after <= FIRST_SCREEN => {}
        other => failures.push(format!(
            "first screen after {other:?}, bar {FIRST_SCREEN:?}"
        )),
    }
    let mut expected = Vec::new();
    for query in sequences(&output).into_iter().filter(is_query) {
        let reply = tessera_reply(&query.bytes);
        if let Some(why) = deliberate_silence(&query) {
            let _ = writeln!(transcript, "silent: {} ({why})", query.bytes.escape_ascii());
            continue;
        }
        if reply.is_empty() {
            failures.push(format!("unanswered query {}", query.bytes.escape_ascii()));
            continue;
        }
        expected.extend(sequences(&reply).iter().map(Seq::kind));
    }
    let got: Vec<String> = sequences(&input).iter().map(Seq::kind).collect();
    if got != expected {
        failures.push(format!("replies read {got:?}, expected {expected:?}"));
    }
    let path = out_dir.join(format!("{}.txt", probe.name));
    std::fs::write(&path, &transcript).expect("write the transcript");
    eprintln!(
        "{}: {} replies, first screen {first_screen:?}, transcript {}",
        probe.name,
        got.len(),
        path.display()
    );
    Some(failures)
}

/// The matcher compares a DCS reply by header, status and setting, not by
/// payload: nvim sets `CSI 4:3 m` before `DCS $ q m ST`, so Tessera answers
/// `1$r0;4:3m` while a fresh terminal answers `1$r0m` (#10 critic B2).
#[test]
fn dcs_replies_match_by_header_status_and_setting() {
    let kind = |bytes: &[u8]| sequences(bytes)[0].kind();
    assert_eq!(kind(b"\x1bP1$r0;4:3m\x1b\\"), kind(b"\x1bP1$r0m\x1b\\"));
    assert_ne!(kind(b"\x1bP1$r0m\x1b\\"), kind(b"\x1bP0$r\x1b\\"));
    assert_ne!(kind(b"\x1bP1$r0m\x1b\\"), kind(b"\x1bP1$r1;24r\x1b\\"));
    assert_ne!(
        kind(b"\x1bP>|Tessera 0.1.0\x1b\\"),
        kind(b"\x1bP!|00000000\x1b\\")
    );
}

#[test]
#[ignore = "live: runs vim, nvim, tmux, fzf, less, htop and fish; not part of the gate"]
fn every_probe_is_answered() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("relay.py"), RELAY).expect("write the relay");
    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/probe-replies");
    std::fs::create_dir_all(&out_dir).expect("transcript dir");
    let mut skipped = Vec::new();
    let mut failures = Vec::new();
    for probe in PROBES {
        match run(probe, dir.path(), &out_dir) {
            None => skipped.push(probe.name),
            Some(found) => {
                failures.extend(found.into_iter().map(|f| format!("{}: {f}", probe.name)));
            }
        }
    }
    eprintln!("skipped: {skipped:?}");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
