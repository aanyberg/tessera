//! The screen-clear fixture, `tests/fixtures/clear_generation_spec.txt`
//! (#377, #378 M3, ADR 0039 amendment of 2026-11-11): the gutter's hover
//! never lends a command's duration to a prompt drawn after a clear of the
//! whole screen erased that command's lines, and a command whose prompt
//! the clear left in the history keeps its duration. The streams are the
//! fixture's captured zsh, bash and fish chunks (section C), fed through
//! [`App`] on a session without a child, one chunk of output per call,
//! with the marks of each chunk processed at a time the test picks
//! (`App::feed_for_test_at`), so every command has a duration of its own.
//! Rows E01 to E14 run here for every shell; rows B01 to B15 are
//! `crates/tessera-term/tests/screen_clears.rs` and (B13)
//! `screen_clears_alloc.rs` beside it.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tessera_app_core::{App, CommandMark, Config, ConfigPaths, PtySize, ScrollTarget, SessionId};

const FIXTURE: &str = include_str!("fixtures/clear_generation_spec.txt");
const TERM_TESTS: [&str; 2] = [
    include_str!("../../tessera-term/tests/screen_clears.rs"),
    include_str!("../../tessera-term/tests/screen_clears_alloc.rs"),
];

/// The shells of section C.
const SHELLS: [&str; 3] = ["zsh", "bash", "fish"];

/// What clears the screen in the clear flows: the captured Ctrl-L's
/// `CSI H CSI 2 J`, the `clear` of screen, tmux and vt100 terminfo (E13),
/// and both orders of macOS and Homebrew `clear(1)`.
const CLEARS: [&[u8]; 4] = [
    b"\x1b[H\x1b[2J",
    b"\x1b[H\x1b[J",
    b"\x1b[3J\x1b[H\x1b[2J",
    b"\x1b[H\x1b[2J\x1b[3J",
];

/// Every E row has a test here and every B row one in the terminal's
/// tests (`fn bNN_`).
#[test]
fn every_row_has_a_test() {
    let ours = include_str!("clear_generation.rs");
    let mut rows = 0;
    for line in FIXTURE.lines() {
        let Some((id, _)) = line.split_once(" | ") else {
            continue;
        };
        let name = format!("fn {}_", id.to_ascii_lowercase());
        match id.as_bytes().first() {
            Some(b'E') => assert!(ours.contains(&name), "{id}: no test `{name}`"),
            Some(b'B') => assert!(
                TERM_TESTS.iter().any(|tests| tests.contains(&name)),
                "{id}: no test `{name}`"
            ),
            _ => continue,
        }
        rows += 1;
    }
    assert_eq!(rows, 14 + 15);
}

// ---------------------------------------------------------------- harness

/// A flow's chunks: `(name, bytes)`, in order.
type Chunks = Vec<(String, Vec<u8>)>;

/// Section C: `(shell, flow)` to its chunks, by name, in order.
fn streams() -> BTreeMap<(String, String), Chunks> {
    let mut streams = BTreeMap::new();
    let mut current = None;
    for line in FIXTURE.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            let mut words = title.split_whitespace();
            current = match (words.next(), words.next(), words.next()) {
                (Some(shell), Some(flow), None) if SHELLS.contains(&shell) => {
                    Some((shell.to_owned(), flow.to_owned()))
                }
                _ => None,
            };
            continue;
        }
        let Some(key) = &current else { continue };
        let Some(rest) = line.strip_prefix("  ") else {
            continue;
        };
        let (name, literal) = rest.split_once(' ').expect("chunk name and bytes");
        let literal = literal.trim();
        let body = literal
            .strip_prefix("b\"")
            .and_then(|l| l.strip_suffix('"'))
            .unwrap_or_else(|| panic!("{key:?} {name}: not a b\"...\" literal"));
        streams
            .entry(key.clone())
            .or_insert_with(Vec::new)
            .push((name.to_owned(), unescape(body)));
    }
    assert_eq!(streams.len(), 9, "{:?}", streams.keys());
    streams
}

/// The bytes of a Rust byte string literal's body.
fn unescape(body: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut bytes = body.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        match bytes.next() {
            Some(b'x') => {
                let hex = [bytes.next().unwrap(), bytes.next().unwrap()];
                let hex = std::str::from_utf8(&hex).unwrap();
                out.push(u8::from_str_radix(hex, 16).unwrap());
            }
            Some(b'r') => out.push(b'\r'),
            Some(b'n') => out.push(b'\n'),
            Some(b'\\') => out.push(b'\\'),
            Some(b'"') => out.push(b'"'),
            other => panic!("unknown escape {other:?}"),
        }
    }
    out
}

/// One shell's flow.
struct Flow {
    chunks: Vec<(String, Vec<u8>)>,
}

impl Flow {
    fn new(shell: &str, flow: &str) -> Self {
        let chunks = streams()
            .remove(&(shell.to_owned(), flow.to_owned()))
            .unwrap_or_else(|| panic!("no {shell} {flow} stream"));
        Self { chunks }
    }

    fn chunk(&self, name: &str) -> &[u8] {
        &self
            .chunks
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no chunk {name}"))
            .1
    }

    /// The `ctrl-l` chunk with its clear replaced by `clear`.
    fn ctrl_l(&self, clear: &[u8]) -> Vec<u8> {
        let chunk = self.chunk("ctrl-l");
        let rest = chunk
            .strip_prefix(b"\x1b[H\x1b[2J")
            .expect("Ctrl-L starts with the clear");
        [clear, rest].concat()
    }
}

/// A session without a child of the fixture's size, and the clock its
/// chunks are processed by.
struct Run {
    app: App,
    id: SessionId,
    now: Instant,
}

impl Run {
    fn new() -> Self {
        let app = App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"));
        let id = app.spawn_test_session(PtySize::new(24, 80)).unwrap();
        app.set_max_scrollback_for_test(id, 1000).unwrap();
        Self {
            app,
            id,
            now: Instant::now(),
        }
    }

    /// Feeds `bytes` as one chunk, `gap` after the previous one.
    fn feed(&mut self, bytes: &[u8], gap: Duration) {
        self.now += gap;
        self.app.feed_for_test_at(self.id, bytes, self.now).unwrap();
    }

    /// Feeds `chunk` as two chunks, split after its `C` mark, the second
    /// `ran` after the first: the command runs for `ran`.
    fn run(&mut self, chunk: &[u8], ran: Duration) {
        let (start, rest) = chunk.split_at(after_mark(chunk, b"C"));
        self.feed(start, Duration::from_millis(1));
        self.feed(rest, ran);
    }

    /// Feeds `bytes` one byte per chunk, the `k`th byte `k` + 1
    /// microseconds after the previous one, so no two commands take
    /// equally long.
    fn feed_bytewise(&mut self, bytes: &[u8]) {
        for (k, byte) in bytes.iter().enumerate() {
            self.feed(&[*byte], Duration::from_micros(k as u64 + 1));
        }
    }

    /// The hover on row `row` of the view.
    fn hover(&self, row: u16) -> Option<CommandMark> {
        self.app.command_mark_at(self.id, row).unwrap()
    }

    /// `(exit status, duration)` of the hover on row `row`, which must
    /// have a mark.
    fn mark(&self, row: u16) -> (Option<i32>, Option<Duration>) {
        let mark = self
            .hover(row)
            .unwrap_or_else(|| panic!("row {row} has no mark"));
        (mark.exit_code, mark.duration)
    }

    fn scroll(&self, offset: usize) {
        self.app
            .scroll_to(self.id, ScrollTarget::Offset(offset))
            .unwrap();
    }

    fn pushed(&self) -> u64 {
        self.app
            .with_terminal(self.id, |t| t.grid().scrollback_pushed())
            .unwrap()
    }
}

/// The index just past the byte that completes the first OSC 133 mark of
/// kind `kind` in `chunk`: its BEL, or the ESC of its ST (the parser
/// dispatches the OSC there; the `\` that follows is an ESC sequence of
/// its own).
fn after_mark(chunk: &[u8], kind: &[u8]) -> usize {
    let mut needle = b"\x1b]133;".to_vec();
    needle.extend_from_slice(kind);
    let start = chunk
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or_else(|| panic!("no {:?} mark", String::from_utf8_lossy(kind)));
    let end = (start..chunk.len())
        .find(|&i| chunk[i] == 0x07 || chunk[i..].starts_with(b"\x1b\\"))
        .expect("terminated");
    end + 1
}

const MS: Duration = Duration::from_millis(1);
/// How long `x` runs.
const DX: Duration = Duration::from_millis(37);
/// How long `cd`, `true` and `export` run.
const DCD: Duration = Duration::from_millis(11);
const DTRUE: Duration = Duration::from_millis(13);
const DEXPORT: Duration = Duration::from_millis(17);

/// `prompt0`, then `x` running for [`DX`]: x's hover on row 0 has it.
fn start(shell: &str, flow: &Flow) -> Run {
    let mut run = Run::new();
    run.feed(flow.chunk("prompt0"), MS);
    run.run(flow.chunk("x"), DX);
    assert_eq!(
        run.mark(0),
        (Some(0), Some(DX)),
        "{shell}: x before the clear"
    );
    run
}

// ------------------------------------------------- E: the clear flows

/// E01 (`crit_p2`): Ctrl-L and `cd` in one chunk. Coalescing leaves no
/// record of the new prompt on row 0; x's record, whose lines the clear
/// erased, must not lend it x's duration.
#[test]
fn e01_ctrl_l_and_a_command_in_one_chunk_borrow_no_duration() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "clear");
        for clear in CLEARS {
            let mut run = start(shell, &flow);
            run.feed(
                &[flow.ctrl_l(clear), flow.chunk("cd").to_vec()].concat(),
                MS,
            );
            assert_eq!(run.mark(0), (Some(0), None), "{shell} {clear:?}");
        }
    }
}

/// E02: Ctrl-L, `cd`, `true` and the `export` prompt in one chunk: no row
/// shows x's duration (the coalesced commands have none).
#[test]
fn e02_ctrl_l_and_three_commands_in_one_chunk_borrow_no_duration() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "clear");
        let mut run = start(shell, &flow);
        let chunk = [
            flow.ctrl_l(CLEARS[0]),
            flow.chunk("cd").to_vec(),
            flow.chunk("true").to_vec(),
            flow.chunk("export").to_vec(),
        ]
        .concat();
        run.feed(&chunk, MS);
        for row in 0..3 {
            assert_eq!(run.mark(row), (Some(0), None), "{shell} row {row}");
        }
    }
}

/// The clear flow after `x`, each chunk on its own, every command with a
/// duration of its own: the hover shows each command's, never x's.
fn clear_flow_chunked(shell: &str, flow: &Flow, clear: &[u8]) {
    let mut run = start(shell, flow);
    run.feed(&flow.ctrl_l(clear), MS);
    run.run(flow.chunk("cd"), DCD);
    run.run(flow.chunk("true"), DTRUE);
    run.run(flow.chunk("export"), DEXPORT);
    let durations = [DCD, DTRUE, DEXPORT];
    for (row, ran) in (0..).zip(durations) {
        assert_eq!(
            run.mark(row),
            (Some(0), Some(ran)),
            "{shell} {clear:?} row {row}"
        );
    }
}

/// E03: Ctrl-L, `cd`, `true` and `export` as they arrive: each prompt's
/// own duration, the newest of the two records starting on row 0 (x's,
/// erased, and cd's) winning.
#[test]
fn e03_each_chunk_on_its_own_gives_each_command_its_duration() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "clear");
        for clear in CLEARS {
            clear_flow_chunked(shell, &flow, clear);
        }
    }
}

/// E04: the bytes after `x` fed one at a time: row 0 shows `cd`'s own
/// duration, as the clock processed its marks, never x's.
#[test]
fn e04_the_clear_flow_fed_byte_by_byte_borrows_no_duration() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "clear");
        let mut run = start(shell, &flow);
        let ctrl_l = flow.ctrl_l(CLEARS[0]);
        let cd = flow.chunk("cd");
        let rest = [&ctrl_l[..], cd, flow.chunk("true")].concat();
        run.feed_bytewise(&rest);
        // The `k`th byte came k + 1 µs after the one before it: cd ran
        // from the byte that completed its `C` to the one that completed
        // its `D`.
        let (c, d) = (after_mark(cd, b"C"), after_mark(cd, b"D"));
        let at = |end: usize| -> u64 {
            let k = (ctrl_l.len() + end - 1) as u64;
            (k + 1) * (k + 2) / 2
        };
        let dcd = Duration::from_micros(at(d) - at(c));
        let (exit, ran) = run.mark(0);
        assert_eq!((exit, ran), (Some(0), Some(dcd)), "{shell}");
        assert_ne!(ran, Some(DX));
        assert_eq!(run.mark(1).0, Some(0), "{shell}: true finished");
        assert_ne!(run.mark(1).1, Some(DX));
    }
}

/// E13: the `clear` of screen, tmux and vt100 terminfo (`CSI H CSI J`)
/// is a clear as `CSI H CSI 2 J` is: E01 and E03 give the same results.
#[test]
fn e13_ed_0_at_home_clears_as_ed_2_does() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "clear");
        let mut run = start(shell, &flow);
        run.feed(
            &[flow.ctrl_l(CLEARS[1]), flow.chunk("cd").to_vec()].concat(),
            MS,
        );
        assert_eq!(run.mark(0), (Some(0), None), "{shell}: E01");
        clear_flow_chunked(shell, &flow, CLEARS[1]);
    }
}

// --------------------------------------------- E: the flows with no clear

/// E05: `x` in one chunk keeps its record's duration (its `C` and `D`
/// came together: zero), and nothing counts as a clear.
#[test]
fn e05_a_command_in_one_chunk_keeps_its_duration() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "noclear");
        let mut run = Run::new();
        run.feed(flow.chunk("prompt0"), MS);
        run.feed(flow.chunk("x"), MS);
        assert_eq!(run.mark(0), (Some(0), Some(Duration::ZERO)), "{shell}");
        let clears = run
            .app
            .with_terminal(run.id, tessera_term::Terminal::screen_clears)
            .unwrap();
        assert_eq!(clears, 0, "{shell}: zsh's CR ED 0 is no clear (E14)");
    }
}

/// E06: `x`, `cd`, `true` and `export` as they arrive: each its own.
#[test]
fn e06_each_command_keeps_its_duration() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "noclear");
        let mut run = start(shell, &flow);
        run.run(flow.chunk("cd"), DCD);
        run.run(flow.chunk("true"), DTRUE);
        run.run(flow.chunk("export"), DEXPORT);
        for (row, ran) in (0..).zip([DX, DCD, DTRUE, DEXPORT]) {
            assert_eq!(run.mark(row), (Some(0), Some(ran)), "{shell} row {row}");
        }
    }
}

/// E07: `x`, `cd` and `true` in one chunk. Coalescing keeps one record of
/// the three, from x's prompt to true's output (D-COALESCE, as before
/// #377): row 0 keeps that record's duration, the others have none, and
/// nothing hides it.
#[test]
fn e07_commands_coalesced_in_one_chunk_keep_what_they_had() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "noclear");
        let mut run = Run::new();
        run.feed(flow.chunk("prompt0"), MS);
        let chunk = [flow.chunk("x"), flow.chunk("cd"), flow.chunk("true")].concat();
        run.feed(&chunk, MS);
        assert_eq!(run.mark(0), (Some(0), Some(Duration::ZERO)), "{shell}");
        for row in 1..3 {
            assert_eq!(run.mark(row), (Some(0), None), "{shell} row {row}");
        }
    }
}

/// E08 (#368): the clear, the next prompt and a command that printed a
/// line, in one chunk: its `C` lies above the pending prompt.
#[test]
fn e08_the_368_shape_still_borrows_no_duration() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "clear");
        let mut run = start(shell, &flow);
        let prompt = flow.chunk("prompt0");
        let chunk = [
            &b"\x1b[H\x1b[2J"[..],
            prompt,
            b"printf hi\r\n\x1b]133;C\x07hi\r\n\x1b]133;D;0\x07",
            prompt,
        ]
        .concat();
        run.feed(&chunk, MS);
        assert_eq!(run.mark(0), (Some(0), None), "{shell}");
    }
}

// ------------------------------------------- E: the prompt in the history

/// The long flow up to and including `x` (`seq 1 60`): x's prompt and
/// `C` scroll into the history, its `D` is on the screen.
fn long_start(shell: &str) -> (Flow, Run) {
    let flow = Flow::new(shell, "long");
    let mut run = Run::new();
    run.feed(flow.chunk("prompt0"), MS);
    run.run(flow.chunk("x"), DX);
    assert_eq!(run.pushed(), 38, "{shell}");
    (flow, run)
}

/// x's hover, on line 0 of the history.
fn history_hover(run: &Run) -> (Option<i32>, Option<Duration>) {
    run.scroll(38);
    let mark = run.mark(0);
    run.scroll(0);
    mark
}

/// E09 (#378 M3): Ctrl-L erases x's `D` with the screen but not its
/// prompt, in the history: the gutter reports its status unknown, and the
/// record, which still describes that prompt, keeps its duration.
#[test]
fn e09_a_prompt_in_the_history_keeps_its_duration_across_a_clear() {
    for shell in SHELLS {
        let (flow, mut run) = long_start(shell);
        assert_eq!(history_hover(&run), (Some(0), Some(DX)), "{shell}");
        run.feed(&flow.ctrl_l(CLEARS[0]), MS);
        run.run(flow.chunk("cd"), DCD);
        assert_eq!(history_hover(&run), (None, Some(DX)), "{shell}");
        assert_eq!(run.mark(0), (Some(0), Some(DCD)), "{shell}: cd");
        run.feed(&flow.ctrl_l(CLEARS[1]), MS);
        assert_eq!(
            history_hover(&run),
            (None, Some(DX)),
            "{shell}: a 2nd clear"
        );
    }
}

/// E10: E09 with Ctrl-L and `cd` in one chunk: x keeps its duration, the
/// new prompt on the screen borrows none.
#[test]
fn e10_the_history_prompt_keeps_its_duration_with_a_merged_chunk() {
    for shell in SHELLS {
        let (flow, mut run) = long_start(shell);
        run.feed(
            &[flow.ctrl_l(CLEARS[0]), flow.chunk("cd").to_vec()].concat(),
            MS,
        );
        assert_eq!(history_hover(&run), (None, Some(DX)), "{shell}");
        assert_eq!(run.mark(0), (Some(0), None), "{shell}: the screen's row 0");
    }
}

// ------------------------------------------------- E: no clear at all

/// E11: ED 3 alone erases no screen row: x keeps its duration.
#[test]
fn e11_ed_3_alone_keeps_the_duration() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "clear");
        let mut run = start(shell, &flow);
        run.feed(b"\x1b[3J", MS);
        assert_eq!(run.mark(0), (Some(0), Some(DX)), "{shell}");
    }
}

/// E12: a clear on the alternate screen leaves the primary one, and x's
/// duration, alone.
#[test]
fn e12_a_clear_on_the_alternate_screen_keeps_the_duration() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "clear");
        let mut run = start(shell, &flow);
        run.feed(b"\x1b[?1049h\x1b[H\x1b[2J\x1b[?1049l", MS);
        assert_eq!(run.mark(0), (Some(0), Some(DX)), "{shell}");
        // In one chunk with the next command too.
        run.feed(
            &[
                &b"\x1b[?1049h\x1b[H\x1b[2J\x1b[?1049l"[..],
                flow.chunk("cd"),
            ]
            .concat(),
            MS,
        );
        assert_eq!(run.mark(0), (Some(0), Some(DX)), "{shell}");
    }
}

/// E14: zsh draws every prompt after `CR ED 0`, below home: no clear, so
/// E05 to E07 keep their durations (E05 checks the count).
#[test]
fn e14_zsh_s_prompt_redraw_is_no_clear() {
    let flow = Flow::new("zsh", "noclear");
    let mut run = start("zsh", &flow);
    run.run(flow.chunk("cd"), DCD);
    assert!(flow.chunk("cd").windows(4).any(|w| w == b"\r\x1b[0"));
    let clears = run
        .app
        .with_terminal(run.id, tessera_term::Terminal::screen_clears)
        .unwrap();
    assert_eq!(clears, 0);
    assert_eq!(run.mark(0), (Some(0), Some(DX)));
    assert_eq!(run.mark(1), (Some(0), Some(DCD)));
}

// ------------------------------------------------ chunking (fixture D3)

/// D3 for E01 and E05: whatever the split of the chunk into two, the
/// hover on row 0 never shows an erased command's duration after a clear,
/// and keeps the command's own without one.
#[test]
fn every_split_of_e01_and_e05_keeps_the_invariant() {
    for shell in SHELLS {
        let flow = Flow::new(shell, "clear");
        let merged = [flow.ctrl_l(CLEARS[0]), flow.chunk("cd").to_vec()].concat();
        for at in 0..=merged.len() {
            let mut run = start(shell, &flow);
            let (a, b) = merged.split_at(at);
            run.feed(a, MS);
            run.feed(b, DCD);
            let (exit, ran) = run.mark(0);
            assert_eq!(exit, Some(0), "{shell} E01 split at {at}");
            assert!(
                ran.is_none_or(|ran| ran == DCD || ran == Duration::ZERO),
                "{shell} E01 split at {at}: {ran:?}"
            );
        }
        let flow = Flow::new(shell, "noclear");
        let x = flow.chunk("x");
        let c = after_mark(x, b"C");
        for at in 0..=x.len() {
            let mut run = Run::new();
            run.feed(flow.chunk("prompt0"), MS);
            let (a, b) = x.split_at(at);
            run.feed(a, MS);
            run.feed(b, DX);
            let ran = if (c..after_mark(x, b"D")).contains(&at) {
                DX
            } else {
                Duration::ZERO
            };
            assert_eq!(
                run.mark(0),
                (Some(0), Some(ran)),
                "{shell} E05 split at {at}"
            );
        }
        // And one byte at a time.
        let mut run = Run::new();
        run.feed(flow.chunk("prompt0"), MS);
        run.feed_bytewise(x);
        assert!(
            run.mark(0).1.is_some_and(|ran| ran > Duration::ZERO),
            "{shell}"
        );
    }
}
