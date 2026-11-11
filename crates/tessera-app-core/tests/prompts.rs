//! The prompt navigation fixture, `tests/fixtures/prompts_spec.txt`
//! (#45a, ADR 0039): every row of layers N, S and E runs here through
//! [`App`] on a session without a child (`App::spawn_test_session`, bytes
//! fed with `App::feed_for_test`, one chunk of output per call). The R rows
//! are unit tests of the record state machine in `src/commands.rs` (fake
//! clock); the T rows and the T half of N10 are in
//! `crates/tessera-term/tests/prompt_marks.rs`. Every row of these layers
//! has a test (checked below).
//!
//! The fixture's lead decisions (wave-13 D14) are the expectations: Page
//! Up / Down only when the history holds no prompt mark, a stop at the
//! oldest prompt, the live screen after the newest, a running command
//! skipped, `false` for a command that printed nothing.

use tessera_app_core::command::CommandId;
use tessera_app_core::{
    App, CommandState, Config, ConfigPaths, InputOutcome, MAX_COMMAND_RECORDS, PtySize,
    ScrollTarget, SessionId,
};
use tessera_term::{Key, KeyEvent, Modifiers};

const FIXTURE: &str = include_str!("fixtures/prompts_spec.txt");
const UNIT_TESTS: &str = include_str!("../src/commands.rs");

/// The rows each test here covers, by fixture id prefix.
const COVERED: &[&str] = &[
    "N01", "N02", "N03", "N04", "N05", "N06", "N07", "N08", "N09", "N10", "N11", "N13", "N14",
    "N15", "N16", "N17", "N18", "N19", "S01", "S02", "S03", "S04", "S05", "S06", "S07", "S08",
    "S09", "S10", "S11", "E01", "E02", "E03", "E04", "E05", "E06", "E07", "E08", "E09",
];

/// Rows of these layers covered elsewhere: N12 is a terminal row (layer
/// T, `prompt_marks.rs`), R12 (R+E) the unit test `r12_*`; N19's lock
/// chunking is also a unit test.
const ELSEWHERE: &[&str] = &["N12", "R12"];

/// Every N, S and E row has a test here (or names where it has one), and
/// every R row a unit test `rNN_*` in `src/commands.rs`.
#[test]
fn every_row_has_a_test() {
    let mut rows = Vec::new();
    let mut layer = None;
    for line in FIXTURE.lines() {
        if let Some(id) = line.strip_prefix("### ") {
            layer = None;
            rows.push((id.trim().to_owned(), String::new()));
        } else if let Some(value) = line.strip_prefix("layer: ")
            && layer.is_none()
        {
            layer = Some(());
            rows.last_mut().unwrap().1 = value.trim().to_owned();
        }
    }
    assert!(rows.len() > 60, "{} rows", rows.len());
    for (id, layer) in &rows {
        let prefix = &id[..3];
        let ours = layer
            .split(['+', ' '])
            .any(|part| matches!(part, "N" | "S" | "E"));
        if ours && !ELSEWHERE.contains(&prefix) {
            assert!(COVERED.contains(&prefix), "{id} ({layer}) has no test");
        }
        if layer.split(['+', ' ']).any(|part| part == "R") {
            let name = format!("fn {}_", prefix.to_ascii_lowercase());
            assert!(UNIT_TESTS.contains(&name), "{id}: no unit test `{name}`");
        }
    }
    for prefix in COVERED {
        assert!(
            rows.iter().any(|(id, _)| id.starts_with(prefix)),
            "{prefix} is no fixture row"
        );
    }
}

// ---------------------------------------------------------------- harness

fn app() -> App {
    App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"))
}

fn session(app: &App, rows: u16, cols: u16, scrollback: usize) -> SessionId {
    let id = app.spawn_test_session(PtySize::new(rows, cols)).unwrap();
    app.set_max_scrollback_for_test(id, scrollback).unwrap();
    id
}

fn feed(app: &App, id: SessionId, bytes: &[u8]) {
    app.feed_for_test(id, bytes).unwrap();
}

/// OSC 133 `payload`.
fn m(payload: &str) -> String {
    format!("\x1b]133;{payload}\x07")
}

/// A prompt as Tessera's zsh script draws it: `D;status` (unless
/// `status` is `None`: the first prompt), `A`, `$ `, `B`, the command.
fn prompt(status: Option<i32>, command: &str) -> String {
    let end = status.map_or(String::new(), |s| m(&format!("D;{s}")));
    format!("{end}{}$ {}{command}", m("A"), m("B"))
}

/// The lines of buffer B1, each to be fed as one chunk (each ends with
/// CRLF but the live prompt).
fn b1_lines() -> Vec<String> {
    vec![
        format!("{}\r\n", prompt(None, "ls")),
        format!("{}a.txt\r\n", m("C")),
        "b.txt\r\n".to_owned(),
        format!("{}\r\n", prompt(Some(0), "pwd")),
        format!("{}/tmp\r\n", m("C")),
        format!("{}\r\n", prompt(Some(0), "false")),
        format!("{}{}\r\n", m("C"), prompt(Some(1), "echo hi")),
        format!("{}hi\r\n", m("C")),
        prompt(Some(0), ""),
    ]
}

/// B1: 4 rows, 5 lines of history (line id = fixture index).
fn b1(app: &App) -> SessionId {
    let id = session(app, 4, 20, 100);
    for line in b1_lines() {
        feed(app, id, line.as_bytes());
    }
    let (pushed, len) = app
        .with_terminal(id, |t| {
            (t.grid().scrollback_pushed(), t.grid().scrollback_len())
        })
        .unwrap();
    assert_eq!((pushed, len), (5, 5));
    id
}

fn to(app: &App, id: SessionId, target: ScrollTarget) -> (bool, usize) {
    let moved = app.scroll_to(id, target).unwrap();
    (moved, app.scroll_offset(id).unwrap())
}

fn prev(app: &App, id: SessionId) -> (bool, usize) {
    to(app, id, ScrollTarget::PreviousPrompt)
}

fn next(app: &App, id: SessionId) -> (bool, usize) {
    to(app, id, ScrollTarget::NextPrompt)
}

fn offset(app: &App, id: SessionId, n: usize) {
    app.scroll_to(id, ScrollTarget::Offset(n)).unwrap();
    assert_eq!(app.scroll_offset(id).unwrap(), n);
}

/// Select Last Output: whether it selected, and the selection's text.
fn select(app: &App, id: SessionId) -> (bool, Option<String>) {
    let selected = app.select_command_output(id).unwrap();
    (selected, app.selection_text(id).unwrap())
}

/// `count` plain lines.
fn plain(count: usize) -> Vec<u8> {
    (0..count)
        .flat_map(|i| format!("line {i}\r\n").into_bytes())
        .collect()
}

// ------------------------------------------------------------- N: navigate

#[test]
fn n01_prev_from_live_screen() {
    let app = app();
    let id = b1(&app);
    assert_eq!(prev(&app, id), (true, 2));
}

#[test]
fn n02_prev_again() {
    let app = app();
    let id = b1(&app);
    offset(&app, id, 2);
    assert_eq!(prev(&app, id), (true, 5));
}

#[test]
fn n03_prev_at_oldest_stops() {
    let app = app();
    let id = b1(&app);
    offset(&app, id, 5);
    assert_eq!(prev(&app, id), (false, 5), "no Page Up");
}

#[test]
fn n04_next_from_oldest() {
    let app = app();
    let id = b1(&app);
    offset(&app, id, 5);
    assert_eq!(next(&app, id), (true, 2));
}

#[test]
fn n05_next_lands_on_live_screen() {
    let app = app();
    let id = b1(&app);
    offset(&app, id, 2);
    assert_eq!(next(&app, id), (true, 0));
}

#[test]
fn n06_next_at_live_screen_is_noop() {
    let app = app();
    let id = b1(&app);
    assert_eq!(next(&app, id), (false, 0));
}

/// D-NEXT-END: with no prompt below the top row in the history, Next
/// Prompt returns to the live screen. The fixture's literal buffer (only
/// prompts 0, 3 and 5, view top at 2) still has prompt 3 below the top
/// row, so that one lands on it (offset 2); the variant with the view
/// below every prompt of the history (top at 4) shows the decision.
#[test]
fn n07_next_with_no_prompt_below_but_scrolled_up() {
    let app = app();
    let id = session(&app, 4, 20, 100);
    let mut lines = b1_lines();
    lines[6] = format!("{}$ echo hi\r\n", m("C"));
    lines[8] = "$ ".to_owned();
    for line in &lines {
        feed(&app, id, line.as_bytes());
    }
    offset(&app, id, 3);
    assert_eq!(next(&app, id), (true, 2), "prompt 3 is below the top row");
    // Top row 4: prompt 5 is on the screen, nothing newer in the history.
    offset(&app, id, 1);
    assert_eq!(next(&app, id), (true, 0));
}

#[test]
fn n08_multi_step_walk_is_symmetric() {
    let app = app();
    let id = b1(&app);
    let walk: Vec<usize> = [prev, prev, next, next]
        .iter()
        .map(|step| step(&app, id).1)
        .collect();
    assert_eq!(walk, [2, 5, 2, 0]);
}

/// A prompt soft-wrapped over rows 10 and 11, both marked, is one stop.
#[test]
fn n09_wrapped_prompt_counts_once() {
    let app = app();
    let id = session(&app, 4, 10, 100);
    let mut bytes = plain(2);
    bytes.extend(format!("{}$ x\r\n", m("A")).bytes());
    bytes.extend(plain(7));
    // Row 10: `A` and 12 characters, wrapping into row 11, marked again.
    bytes.extend(format!("{}$ abcdefghij{}\r\n", m("A"), m("A")).bytes());
    bytes.extend(plain(11));
    feed(&app, id, &bytes);
    let pushed = app
        .with_terminal(id, |t| {
            let grid = t.grid();
            assert!(grid.logical_line(10).unwrap().wrapped);
            assert_eq!(grid.logical_line(11).unwrap().marks.len(), 1);
            grid.scrollback_pushed()
        })
        .unwrap();
    assert_eq!(pushed, 20, "top row 20");
    assert_eq!(prev(&app, id), (true, 10), "row 10, not 11");
    assert_eq!(prev(&app, id), (true, 18), "the prompt above row 10");
    assert_eq!(next(&app, id), (true, 10));
}

/// The T half is in `prompt_marks.rs`: `A;k=s` makes no mark, so only row
/// 0 is a stop.
#[test]
fn n10_ps2_never_stops() {
    let app = app();
    let id = session(&app, 4, 30, 100);
    let input = "\x1b]133;A\x07$ \x1b]133;B\x07for x in 1; do\r\n\x1b]133;A;k=s\x07> \x1b]133;B\x07echo $x; done\r\n\x1b]133;C\x07";
    feed(&app, id, input.as_bytes());
    feed(&app, id, &plain(5));
    let pushed = app
        .with_terminal(id, |t| t.grid().scrollback_pushed())
        .unwrap();
    assert_eq!(prev(&app, id), (true, usize::try_from(pushed).unwrap()));
    assert_eq!(prev(&app, id), (false, usize::try_from(pushed).unwrap()));
    assert_eq!(next(&app, id), (true, 0), "row 1 is no stop either");
}

#[test]
fn n11_alt_screen_refuses() {
    let app = app();
    let id = b1(&app);
    feed(&app, id, b"\x1b[?1049h");
    assert_eq!(prev(&app, id), (false, 0));
    assert_eq!(next(&app, id), (false, 0));
    assert!(!app.select_command_output(id).unwrap());
    assert!(!app.has_command_output(id).unwrap());
    feed(&app, id, b"\x1b[?1049l");
    assert_eq!(app.scroll_offset(id).unwrap(), 0);
    assert_eq!(
        prev(&app, id),
        (true, 2),
        "the main screen's marks are intact"
    );
    assert!(app.has_command_output(id).unwrap());
}

/// The scan stops after [`tessera_app_core::MAX_PROMPT_SCAN_LINES`]
/// lines: a prompt further up is not found, and the history holding a
/// prompt mark, the keys do not page either.
#[test]
fn n13_scan_cap() {
    let cap = tessera_app_core::MAX_PROMPT_SCAN_LINES;
    let app = app();
    let id = session(&app, 2, 2, cap + 10);
    feed(&app, id, format!("{}\r\n", m("A")).as_bytes());
    feed(&app, id, &b"\n".repeat(cap + 4));
    let len = app
        .with_terminal(id, |t| t.grid().scrollback_len())
        .unwrap();
    assert_eq!(len, cap + 4, "the prompt is still in the history");
    assert_eq!(
        prev(&app, id),
        (false, 0),
        "beyond the cap: no move, no page"
    );
    // Inside the cap it is found.
    offset(&app, id, 20);
    assert_eq!(prev(&app, id), (true, cap + 4));
}

/// D4 beyond the cap: a shell that never reported a prompt pages at
/// once, however long the history (#45a B1: with more than
/// [`tessera_app_core::MAX_PROMPT_SCAN_LINES`] lines the keys did
/// nothing).
#[test]
fn n13_no_marks_beyond_the_cap_still_pages() {
    let cap = tessera_app_core::MAX_PROMPT_SCAN_LINES;
    let app = app();
    let id = session(&app, 2, 2, cap + 10);
    feed(&app, id, &b"\n".repeat(cap + 4));
    assert_eq!(prev(&app, id), (true, 1), "Page Up");
    offset(&app, id, 5);
    assert_eq!(next(&app, id), (true, 4), "Page Down");
}

/// The look for any prompt in the history (the live prompt is on the
/// screen, more than the cap of plain lines above) ends at the cap: that
/// pages too, never nothing.
#[test]
fn n13_marks_only_on_screen_beyond_the_cap_page() {
    let cap = tessera_app_core::MAX_PROMPT_SCAN_LINES;
    let app = app();
    let id = session(&app, 2, 2, cap + 10);
    feed(&app, id, &b"\n".repeat(cap + 4));
    feed(&app, id, format!("{}$", m("A")).as_bytes());
    // Next: nothing newer in the history; none within the cap above.
    offset(&app, id, 5);
    assert_eq!(next(&app, id), (true, 4), "Page Down");
    // Previous one line below the oldest: none within the cap below.
    app.scroll_to(id, ScrollTarget::Top).unwrap();
    let top = app.scroll_offset(id).unwrap();
    offset(&app, id, top - 1);
    assert_eq!(prev(&app, id), (true, top), "Page Up");
}

#[test]
fn n14_no_marks_falls_back_to_page_keys() {
    let app = app();
    let id = session(&app, 24, 20, 1000);
    feed(&app, id, &plain(100));
    feed(&app, id, b"$ ");
    assert_eq!(prev(&app, id), (true, 23));
    // The ⌘↑ route: the default chord runs Previous Prompt.
    let outcome = app
        .run_key_binding(id, &KeyEvent::new(Key::Up, Modifiers::SUPER))
        .unwrap();
    let previous = CommandId::find("prompt_previous", None).unwrap();
    assert_eq!(outcome, Some(InputOutcome::Command(previous)));
}

#[test]
fn n15_marks_only_on_screen_falls_back() {
    let app = app();
    let id = session(&app, 24, 20, 1000);
    feed(&app, id, &plain(100));
    feed(&app, id, prompt(Some(0), "").as_bytes());
    assert_eq!(prev(&app, id), (true, 23));
}

#[test]
fn n16_marks_exist_above_but_view_above_all_of_them() {
    let app = app();
    let id = session(&app, 4, 20, 1000);
    feed(&app, id, &plain(50));
    for line in b1_lines() {
        feed(&app, id, line.as_bytes());
    }
    app.scroll_to(id, ScrollTarget::Top).unwrap();
    let top = app.scroll_offset(id).unwrap();
    assert_eq!(top, 55);
    assert_eq!(prev(&app, id), (false, 55));
    assert_eq!(next(&app, id), (true, 5), "the first prompt, line 50");
}

/// N16 with the view inside the plain lines (top row 10, not the oldest):
/// the history holds prompts, so Previous stops there instead of paging.
#[test]
fn n16_view_inside_the_lines_above_every_prompt_stops() {
    let app = app();
    let id = session(&app, 4, 20, 1000);
    feed(&app, id, &plain(50));
    for line in b1_lines() {
        feed(&app, id, line.as_bytes());
    }
    offset(&app, id, 45);
    assert_eq!(prev(&app, id), (false, 45), "no Page Up");
}

#[test]
fn n17_next_fallback() {
    let app = app();
    let id = session(&app, 24, 20, 1000);
    feed(&app, id, &plain(100));
    offset(&app, id, 40);
    assert_eq!(next(&app, id), (true, 17));
}

#[test]
fn n18_clear_history_removes_marks_and_fallback_returns() {
    let app = app();
    let id = b1(&app);
    feed(&app, id, b"\x1b[3J");
    assert_eq!(
        app.with_terminal(id, |t| t.grid().scrollback_len())
            .unwrap(),
        0
    );
    // Page Up into an empty history: nothing moves.
    assert_eq!(prev(&app, id), (false, 0));
    assert_eq!(next(&app, id), (false, 0));
}

/// N19: the scan reads the lines a chunk of
/// [`tessera_app_core::PROMPT_SCAN_LINES_PER_LOCK`] per lock hold (counted
/// in the unit test `n19_scans_hold_the_lock_a_chunk_at_a_time`). Here:
/// a reader feeding output all through a walk over 200 000 lines gets the
/// lock between the walk's holds, so its chunks land *during* the walk.
/// Held for the whole walk instead, every chunk would wait for its end:
/// at most the one blocked chunk could land inside it (#334). Measured:
/// about 2000 chunks land during the walk, 1 or 2 without the yield
/// between holds. Only chunks landing in the middle half of the walk count:
/// the walk's own short holds before and after the scan leave gaps a held
/// scan would not. The walk takes about 0.8 ms in release, and a loaded
/// machine can deschedule the feeder for all of it (4 of 2400 runs at 24
/// in parallel did so for five walks in a row, #368), so walks are tried
/// for up to `TRYING` and the test passes once half of the last
/// `WINDOW` walks see the chunks land (at least `LEAST` walks), or when
/// time runs out and every walk did: a loaded machine slows a walk to
/// seconds (one took 10 s at load 100 and 24 runs in parallel, #368). The
/// feeder yields between chunks so it does not starve the walk. A held scan sees them in about 2% of
/// walks (51 of 2645 measured, #368), never most of a window. The
/// feeder's chunks grow no history, so trying that long never evicts the
/// prompt.
#[test]
fn n19_nav_does_not_hold_lock_longer_than_a_snapshot() {
    /// How long walks are tried before the test fails.
    const TRYING: Duration = Duration::from_secs(10);
    /// How many consecutive walks the criterion looks at.
    const WINDOW: usize = 20;
    /// The fewest walks the criterion looks at.
    const LEAST: usize = 3;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let app = Arc::new(app());
    let id = session(&app, 2, 2, 300_000);
    feed(&app, id, format!("{}\r\n", m("A")).as_bytes());
    feed(&app, id, &b"\n".repeat(200_000));
    let stop = Arc::new(AtomicBool::new(false));
    let fed = Arc::new(AtomicUsize::new(0));
    let landed = Arc::new(Mutex::new(Vec::new()));
    let feeder = {
        let (app, stop, fed, landed) = (
            Arc::clone(&app),
            Arc::clone(&stop),
            Arc::clone(&fed),
            Arc::clone(&landed),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                // Each chunk rewrites the 2-column screen's top row: no
                // line id moves and the history does not grow.
                app.feed_for_test(id, b"\x1b[Hxy").unwrap();
                landed.lock().unwrap().push(Instant::now());
                fed.fetch_add(1, Ordering::Release);
                std::thread::yield_now();
            }
        })
    };
    while fed.load(Ordering::Acquire) == 0 {
        std::thread::yield_now();
    }
    // Half of the last walks (at least `LEAST`, unless time ran
    // out and all of them did) saw chunks land.
    let good = |walks: &[(usize, Duration)], out: bool| {
        let window = &walks[walks.len().saturating_sub(WINDOW)..];
        let landed = window.iter().filter(|&&(during, _)| during >= 10).count();
        let all = out && landed == window.len() && landed > 0;
        all || (window.len() >= LEAST && landed * 2 >= window.len())
    };
    let mut walks = Vec::new();
    let trying = Instant::now();
    while trying.elapsed() < TRYING {
        app.scroll_to(id, ScrollTarget::Bottom).unwrap();
        landed.lock().unwrap().clear();
        let started = Instant::now();
        let moved = app.scroll_to(id, ScrollTarget::PreviousPrompt).unwrap();
        let ended = Instant::now();
        assert!(moved);
        let quarter = (ended - started) / 4;
        let (from, to) = (started + quarter, ended.checked_sub(quarter).unwrap());
        let during = landed
            .lock()
            .unwrap()
            .iter()
            .filter(|&&at| from < at && at < to)
            .count();
        walks.push((during, ended - started));
        if walks.len() >= WINDOW && good(&walks, false) {
            break;
        }
    }
    stop.store(true, Ordering::Release);
    feeder.join().unwrap();
    let window = &walks[walks.len().saturating_sub(WINDOW)..];
    assert!(
        good(&walks, true),
        "chunks landed in the middle of the last {WINDOW} of {} walks: {window:?}",
        walks.len()
    );
}

// ---------------------------------------------------------- S: select output

#[test]
fn s01_last_command_single_row() {
    let app = app();
    let id = b1(&app);
    assert_eq!(select(&app, id), (true, Some("hi".to_owned())));
}

#[test]
fn s02_multi_row_output() {
    let app = app();
    let id = session(&app, 4, 20, 100);
    for line in &b1_lines()[..3] {
        feed(&app, id, line.as_bytes());
    }
    feed(&app, id, prompt(Some(0), "").as_bytes());
    assert_eq!(select(&app, id), (true, Some("a.txt\nb.txt".to_owned())));
    let records = app.command_records(id).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        (
            records[0].output_line,
            records[0].end_line,
            records[0].exit_code
        ),
        (Some(1), Some(3), Some(0))
    );
}

/// D-EMPTY-OUTPUT: `true` printed nothing; the selection there was stays.
#[test]
fn s03_command_with_no_output() {
    let app = app();
    let id = b1(&app);
    feed(&app, id, b"true\r\n");
    feed(
        &app,
        id,
        format!("{}{}", m("C"), prompt(Some(0), "")).as_bytes(),
    );
    app.select_all(id).unwrap();
    let all = app.selection_text(id).unwrap();
    assert_eq!(select(&app, id), (false, all));
    assert!(!app.has_command_output(id).unwrap());
}

/// D-RUNNING: the running command is skipped.
#[test]
fn s04_running_command() {
    let app = app();
    let id = b1(&app);
    feed(&app, id, b"sleep 100\r\n");
    feed(&app, id, format!("{}partial\r\n", m("C")).as_bytes());
    assert_eq!(select(&app, id), (true, Some("hi".to_owned())));
    assert!(app.has_command_output(id).unwrap());
}

#[test]
fn s05_soft_wrapped_output_row() {
    let app = app();
    let id = session(&app, 4, 10, 100);
    feed(&app, id, format!("{}\r\n", prompt(None, "echo")).as_bytes());
    feed(
        &app,
        id,
        format!("{}aaaaaaaaaabbbbbbbbbbccccc\r\n", m("C")).as_bytes(),
    );
    feed(&app, id, prompt(Some(0), "").as_bytes());
    assert_eq!(
        select(&app, id),
        (true, Some("aaaaaaaaaabbbbbbbbbbccccc".to_owned()))
    );
}

#[test]
fn s06_output_with_blank_rows() {
    let app = app();
    let id = session(&app, 4, 10, 100);
    feed(&app, id, format!("{}\r\n", prompt(None, "x")).as_bytes());
    feed(
        &app,
        id,
        format!("{}a\r\n\r\nb\r\n\r\n\r\n", m("C")).as_bytes(),
    );
    feed(&app, id, prompt(Some(0), "").as_bytes());
    assert_eq!(select(&app, id), (true, Some("a\n\nb".to_owned())));
}

/// zsh's `PROMPT_SP`: `foo%` and blanks soft-wrap into the prompt row.
/// The same text is selected after a resize there and back.
#[test]
fn s07_output_without_trailing_newline_zsh_prompt_sp() {
    let app = app();
    let id = session(&app, 4, 20, 100);
    feed(
        &app,
        id,
        format!("{}\r\n", prompt(None, "printf foo")).as_bytes(),
    );
    let sp = format!("{}foo%{} \r{}", m("C"), " ".repeat(16), prompt(Some(0), ""));
    feed(&app, id, sp.as_bytes());
    let wrapped = app
        .with_terminal(id, |t| t.grid().logical_line(1).unwrap().wrapped)
        .unwrap();
    assert!(wrapped);
    assert_eq!(select(&app, id), (true, Some("foo%".to_owned())));
    app.resize(id, PtySize::new(4, 37)).unwrap();
    app.resize(id, PtySize::new(4, 20)).unwrap();
    assert_eq!(select(&app, id), (true, Some("foo%".to_owned())));
}

/// D-EVICTED-OUTPUT: the output's first line left the history.
#[test]
fn s08_output_start_evicted() {
    let app = app();
    let id = session(&app, 4, 20, 4);
    feed(
        &app,
        id,
        format!("{}\r\n", prompt(None, "seq 20")).as_bytes(),
    );
    feed(
        &app,
        id,
        format!("{}{}", m("C"), String::from_utf8(plain(20)).unwrap()).as_bytes(),
    );
    feed(&app, id, prompt(Some(0), "").as_bytes());
    assert_eq!(select(&app, id), (false, None));
    assert!(!app.has_command_output(id).unwrap());
}

#[test]
fn s09_no_marks_at_all() {
    let app = app();
    let id = session(&app, 4, 20, 100);
    feed(&app, id, &plain(10));
    assert_eq!(select(&app, id), (false, None));
    assert!(!app.has_command_output(id).unwrap());
    assert_eq!(app.command_records(id).unwrap(), []);
}

/// The selection is an ordinary one: navigation keeps it, a click clears
/// it, output that scrolls it into the history keeps it.
#[test]
fn s10_select_then_selection_is_normal() {
    let app = app();
    let id = b1(&app);
    assert!(app.select_command_output(id).unwrap());
    assert_eq!(prev(&app, id), (true, 2));
    assert_eq!(app.selection_text(id).unwrap().as_deref(), Some("hi"));
    feed(&app, id, b"\r\nmore\r\n");
    assert_eq!(app.selection_text(id).unwrap().as_deref(), Some("hi"));
    app.select_clear(id).unwrap();
    assert!(!app.has_selection(id).unwrap());
}

/// D-SELECT-SCROLL: the view scrolls to show the output's start; on the
/// alternate screen nothing is selected.
#[test]
fn s11_alt_screen_and_scrolled_view() {
    let app = app();
    let id = session(&app, 4, 20, 100);
    for line in &b1_lines()[..8] {
        feed(&app, id, line.as_bytes());
    }
    feed(&app, id, &plain(6));
    feed(&app, id, prompt(Some(0), "").as_bytes());
    // `echo hi`'s output line 7 is in the history; the view is live.
    assert_eq!(
        select(&app, id),
        (
            true,
            Some("hi\nline 0\nline 1\nline 2\nline 3\nline 4\nline 5".to_owned())
        )
    );
    let pushed = app
        .with_terminal(id, |t| t.grid().scrollback_pushed())
        .unwrap();
    assert_eq!(
        app.scroll_offset(id).unwrap(),
        usize::try_from(pushed).unwrap() - 7,
        "line 7 at the top"
    );
    // Already visible: the view stays.
    offset(&app, id, usize::try_from(pushed).unwrap() - 6);
    assert!(app.select_command_output(id).unwrap());
    assert_eq!(
        app.scroll_offset(id).unwrap(),
        usize::try_from(pushed).unwrap() - 6
    );
    feed(&app, id, b"\x1b[?1049h");
    assert!(!app.select_command_output(id).unwrap());
}

/// `seq 1 3`, `echo b`, `echo d` in a 4x20 session, the live prompt
/// last.
fn three_commands(app: &App) -> SessionId {
    let id = session(app, 4, 20, 100);
    for chunk in [
        format!("{}\r\n", prompt(None, "seq 1 3")),
        format!("{}1\r\n2\r\n3\r\n", m("C")),
        format!("{}\r\n", prompt(Some(0), "echo b")),
        format!("{}b\r\n", m("C")),
        format!("{}\r\n", prompt(Some(0), "echo d")),
        format!("{}d\r\n", m("C")),
        prompt(Some(0), ""),
    ] {
        feed(app, id, chunk.as_bytes());
    }
    id
}

/// zsh's Ctrl-L: `CSI H CSI 2J` (history kept) and the prompt again.
fn clear_screen(app: &App, id: SessionId) {
    feed(
        app,
        id,
        format!("\x1b[H\x1b[2J{}", prompt(None, "")).as_bytes(),
    );
}

/// zsh's redraw of the prompt line after a resize.
fn redraw_prompt(app: &App, id: SessionId) {
    feed(app, id, format!("\r\x1b[K{}", prompt(None, "")).as_bytes());
}

/// A screen clear (zsh/fish/bash Ctrl-L: `CSI H CSI 2J`, history kept)
/// erased the last command's output and its `C`: the older output still
/// in the history is not offered in its place (#45a adversarial B1).
#[test]
fn select_after_a_screen_clear_offers_no_older_output() {
    let app = app();
    let id = three_commands(&app);
    assert!(app.has_command_output(id).unwrap());
    clear_screen(&app, id);
    assert_eq!(select(&app, id), (false, None));
    assert_eq!(app.scroll_offset(id).unwrap(), 0, "the view stays");
    assert!(!app.has_command_output(id).unwrap());
}

/// The same with a resize before the clear: the newest command's `C` is
/// followed into the new line ids (#45a adversarial cycle 2).
#[test]
fn select_after_a_resize_then_a_screen_clear_offers_no_older_output() {
    for size in [PtySize::new(4, 30), PtySize::new(6, 20)] {
        let app = app();
        let id = three_commands(&app);
        app.resize(id, size).unwrap();
        redraw_prompt(&app, id);
        clear_screen(&app, id);
        assert_eq!(select(&app, id), (false, None), "{size:?}");
        assert!(!app.has_command_output(id).unwrap(), "{size:?}");
    }
}

/// And with a resize after the clear (and back): the erased `C` stays
/// erased.
#[test]
fn select_after_a_screen_clear_then_resizes_offers_no_older_output() {
    let app = app();
    let id = three_commands(&app);
    clear_screen(&app, id);
    for size in [PtySize::new(4, 30), PtySize::new(4, 20)] {
        app.resize(id, size).unwrap();
        redraw_prompt(&app, id);
        assert_eq!(select(&app, id), (false, None), "{size:?}");
        assert!(!app.has_command_output(id).unwrap(), "{size:?}");
    }
}

/// A command that finished (or was running) before resizes keeps its
/// output selectable after them.
#[test]
fn select_after_resizes_still_selects_the_last_output() {
    for size in [PtySize::new(4, 30), PtySize::new(6, 20), PtySize::new(3, 8)] {
        let app = app();
        let id = three_commands(&app);
        app.resize(id, size).unwrap();
        redraw_prompt(&app, id);
        app.resize(id, PtySize::new(5, 12)).unwrap();
        assert_eq!(select(&app, id), (true, Some("d".to_owned())), "{size:?}");
        assert!(app.has_command_output(id).unwrap(), "{size:?}");
    }
    // Resized while `echo e` runs.
    let app = app();
    let id = three_commands(&app);
    feed(&app, id, b"echo e\r\n");
    feed(&app, id, format!("{}e\r\n", m("C")).as_bytes());
    app.resize(id, PtySize::new(4, 30)).unwrap();
    feed(&app, id, prompt(Some(0), "").as_bytes());
    assert_eq!(select(&app, id), (true, Some("e".to_owned())));
    assert!(app.has_command_output(id).unwrap());
}

/// A command that deletes its own first output row (DL) took its `C`
/// with it: the previous command's output is not selected instead.
#[test]
fn select_after_the_first_output_row_was_deleted_offers_no_older_output() {
    let app = app();
    let id = session(&app, 4, 20, 100);
    for chunk in [
        format!("{}\r\n", prompt(None, "echo old")),
        format!("{}old\r\n", m("C")),
        format!("{}\r\n", prompt(Some(0), "x")),
        format!("{}x\r\n\x1b[A\x1b[Mok\r\n", m("C")),
        prompt(Some(0), ""),
    ] {
        feed(&app, id, chunk.as_bytes());
    }
    assert_eq!(select(&app, id), (false, None));
    assert!(!app.has_command_output(id).unwrap());
}

/// The records lost two commands to one chunk's coalescing: the grid's
/// newer `C` (with its prompt below) is still selected.
#[test]
fn select_reads_the_grid_when_the_records_lag_behind() {
    let app = app();
    let id = session(&app, 6, 20, 100);
    feed(&app, id, format!("{}\r\n", prompt(None, "a")).as_bytes());
    feed(&app, id, format!("{}1\r\n", m("C")).as_bytes());
    let chunk = format!(
        "{}\r\n{}2\r\n{}\r\n{}3\r\n",
        prompt(Some(0), "b"),
        m("C"),
        prompt(Some(0), "c"),
        m("C"),
    );
    feed(&app, id, chunk.as_bytes());
    let records = app.command_records(id).unwrap();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0].output_line, Some(1), "`b` left no record");
    assert_eq!(select(&app, id), (true, Some("2".to_owned())));
    assert!(app.has_command_output(id).unwrap());
    app.resize(id, PtySize::new(6, 30)).unwrap();
    assert_eq!(select(&app, id), (true, Some("2".to_owned())), "resized");
    assert!(app.has_command_output(id).unwrap());
}

// ------------------------------------------------------- E: ring, eviction

fn cycle(i: usize) -> String {
    format!(
        "{}{}x\r\n",
        prompt(Some(0), "c"),
        m(&format!("C\x07\x1b]133;D;{i}"))
    )
}

#[test]
fn e01_ring_cap() {
    let app = app();
    let id = session(&app, 4, 20, 5000);
    for i in 0..=MAX_COMMAND_RECORDS {
        let bytes = format!("{}\r\n{}x\r\n{}", m("A"), m("C"), m(&format!("D;{i}")));
        feed(&app, id, bytes.as_bytes());
    }
    let records = app.command_records(id).unwrap();
    assert_eq!(records.len(), MAX_COMMAND_RECORDS);
    assert_eq!(records[0].exit_code, Some(1), "command #0 gave way");
    assert_eq!(
        records[MAX_COMMAND_RECORDS - 1].exit_code,
        i32::try_from(MAX_COMMAND_RECORDS).ok()
    );
    let _ = cycle;
}

/// 100 000 commands, each its own chunk: the ring stays at its cap.
#[test]
fn e02_ring_cap_flood_of_commands() {
    let app = app();
    let id = session(&app, 4, 20, 100);
    let one = format!("{}{}{}", m("A"), m("C"), m("D;0"));
    for _ in 0..100_000 {
        feed(&app, id, one.as_bytes());
    }
    let records = app.command_records(id).unwrap();
    assert_eq!(records.len(), MAX_COMMAND_RECORDS);
    assert!(records.iter().all(|r| r.state == CommandState::Finished));
}

#[test]
fn e03_scrollback_eviction_drops_records() {
    let app = app();
    let id = session(&app, 4, 20, 5);
    for i in 0..20 {
        feed(
            &app,
            id,
            format!("{}\r\n", prompt(Some(0), &format!("c{i}"))).as_bytes(),
        );
        feed(&app, id, format!("{}out\r\nout\r\n", m("C")).as_bytes());
    }
    feed(&app, id, prompt(Some(0), "").as_bytes());
    let (base, pushed) = app
        .with_terminal(id, |t| {
            let g = t.grid();
            (
                g.scrollback_pushed() - g.scrollback_len() as u64,
                g.scrollback_pushed(),
            )
        })
        .unwrap();
    assert_eq!((base, pushed), (52, 57));
    let records = app.command_records(id).unwrap();
    // Command i is lines 3i..3i+2 (prompt, output, output); 17 (its
    // output only), 18 and 19 still have lines.
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].prompt_line, None, "line 51 was evicted");
    assert_eq!(records[0].output_line, Some(52));
    assert_eq!(app.command_at(id, 51).unwrap(), None);
    assert_eq!(app.command_at(id, 50).unwrap(), None);
    assert_eq!(app.command_at(id, 53).unwrap(), Some(records[0]));
    let at = app.command_at(id, 58).unwrap().unwrap();
    assert_eq!((at.prompt_line, at.output_line), (Some(57), Some(58)));
}

/// ED 3 drops the records of the cleared lines; ⌘K (`clear_scrollback`)
/// drops every finished one.
#[test]
fn e04_clear_history_drops_records() {
    let app = app();
    let id = b1(&app);
    assert_eq!(app.command_records(id).unwrap().len(), 4);
    feed(&app, id, b"\x1b[3J");
    let records = app.command_records(id).unwrap();
    let outputs: Vec<_> = records.iter().map(|r| r.output_line).collect();
    assert_eq!(outputs, [Some(6), Some(7)], "#3 false and #4 echo hi stay");
    app.clear_scrollback(id).unwrap();
    assert_eq!(app.command_records(id).unwrap(), []);
}

/// Any resize moves the line ids (width or height, growing or
/// shrinking): the records keep their status and times but no lines;
/// navigation and select read the re-wrapped grid; new commands resolve.
#[test]
fn e05_resize_invalidates_line_ids() {
    for size in [(4, 15), (3, 20), (6, 20), (4, 20)] {
        let app = app();
        let id = b1(&app);
        assert!(app.command_at(id, 7).unwrap().is_some());
        let size = PtySize::new(size.0, size.1);
        if size == PtySize::new(4, 20) {
            // The same size is no resize.
            app.resize(id, size).unwrap();
            assert!(app.command_at(id, 7).unwrap().is_some(), "{size:?}");
            continue;
        }
        app.resize(id, size).unwrap();
        for line in 0..12 {
            assert_eq!(
                app.command_at(id, line).unwrap(),
                None,
                "{size:?} line {line}"
            );
        }
        let records = app.command_records(id).unwrap();
        assert_eq!(records.len(), 4, "{size:?}");
        assert!(
            records
                .iter()
                .all(|r| r.output_line.is_none() && r.exit_code.is_some())
        );
        assert!(app.has_command_output(id).unwrap());
        assert_eq!(select(&app, id), (true, Some("hi".to_owned())), "{size:?}");
        assert!(prev(&app, id).0 || size.rows > 4, "{size:?}");
        feed(&app, id, b"echo new\r\n");
        feed(
            &app,
            id,
            format!("{}new\r\n{}", m("C"), prompt(Some(3), "")).as_bytes(),
        );
        let pushed = app
            .with_terminal(id, |t| {
                t.grid().scrollback_pushed() + u64::from(t.grid().cursor().row)
            })
            .unwrap();
        let at = app
            .command_at(id, pushed - 1)
            .unwrap()
            .expect("the new command");
        assert_eq!(
            (at.output_line, at.exit_code),
            (Some(pushed - 1), Some(3)),
            "{size:?}"
        );
    }
}

/// The CRITIC case of E05: growing the height pulls history rows back on
/// the screen (ids shift down); an old record must not name another
/// command's row.
#[test]
fn e05_height_grow_maps_no_old_record() {
    let app = app();
    let id = b1(&app);
    let before: Vec<_> = (0..9).map(|l| app.command_at(id, l).unwrap()).collect();
    assert!(before[1].is_some() && before[7].is_some());
    app.resize(id, PtySize::new(8, 20)).unwrap();
    let pushed = app
        .with_terminal(id, |t| t.grid().scrollback_pushed())
        .unwrap();
    assert_eq!(pushed, 5, "rows pulled back are not counted");
    for line in 0..16 {
        assert_eq!(app.command_at(id, line).unwrap(), None, "line {line}");
    }
}

#[test]
fn e06_resize_during_running_command() {
    let app = app();
    let id = session(&app, 4, 20, 100);
    feed(&app, id, format!("{}\r\n", prompt(None, "make")).as_bytes());
    feed(&app, id, format!("{}building\r\n", m("C")).as_bytes());
    assert_eq!(
        app.command_records(id).unwrap()[0].state,
        CommandState::Running
    );
    app.resize(id, PtySize::new(4, 30)).unwrap();
    feed(&app, id, prompt(Some(0), "").as_bytes());
    let r = app.command_records(id).unwrap()[0];
    assert_eq!(
        (r.state, r.exit_code, r.output_line),
        (CommandState::Finished, Some(0), None)
    );
    assert!(r.duration().is_some());
    assert_eq!(select(&app, id), (true, Some("building".to_owned())));
}

#[test]
fn e07_resize_roundtrip_marks() {
    let app = app();
    let id = session(&app, 4, 80, 100);
    for line in b1_lines() {
        feed(&app, id, line.as_bytes());
    }
    let marks = |app: &App| {
        app.with_terminal(id, |t| {
            t.grid()
                .logical_lines()
                .map(|l| l.marks.clone())
                .collect::<Vec<_>>()
        })
        .unwrap()
    };
    let walk = |app: &App| {
        app.scroll_to(id, ScrollTarget::Bottom).unwrap();
        let steps: Vec<_> = [prev, prev, prev, next, next, next]
            .iter()
            .map(|step| step(app, id))
            .collect();
        (steps, select(app, id))
    };
    let (before_marks, before) = (marks(&app), walk(&app));
    app.resize(id, PtySize::new(4, 37)).unwrap();
    app.resize(id, PtySize::new(4, 80)).unwrap();
    assert_eq!(marks(&app), before_marks);
    assert_eq!(walk(&app), before);
}

#[test]
fn e08_new_prompt_after_eviction_of_pending() {
    let app = app();
    let id = session(&app, 4, 20, 3);
    feed(&app, id, format!("{}$ \r\n", m("A")).as_bytes());
    feed(&app, id, &plain(10));
    feed(
        &app,
        id,
        format!("{}out\r\n{}", m("C"), m("D;0")).as_bytes(),
    );
    let r = app.command_records(id).unwrap()[0];
    assert_eq!(
        (r.prompt_line, r.exit_code),
        (None, Some(0)),
        "line 0 was evicted"
    );
    assert!(r.output_line.is_some());
}

/// One chunk of output longer than a slice of the reader (4096 bytes),
/// with a command in its first slice and another in its last: the chunk
/// keeps the last mark of each kind across its slices, so the record is
/// the second command's.
#[test]
fn a_chunk_over_several_slices_records_its_last_command() {
    let app = app();
    let id = session(&app, 4, 20, 1000);
    let mut chunk = format!("{}\r\n{}out\r\n{}", prompt(None, "ls"), m("C"), m("D;0")).into_bytes();
    chunk.extend(plain(600));
    chunk.extend(format!("{}\r\n{}y\r\n{}", prompt(None, "x"), m("C"), m("D;7")).bytes());
    assert!(
        chunk.len() > 4096 + 100,
        "the cycles are in different slices"
    );
    feed(&app, id, &chunk);
    let records = app.command_records(id).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].prompt_line, Some(602), "the second prompt");
    assert_eq!(records[0].exit_code, Some(7));
}

#[test]
fn e09_ring_and_grid_disagree_is_harmless() {
    let app = app();
    let id = session(&app, 4, 20, 5000);
    for i in 0..=MAX_COMMAND_RECORDS {
        let bytes = format!("{}$ c\r\n{}x\r\n{}", m("A"), m("C"), m(&format!("D;{i}")));
        feed(&app, id, bytes.as_bytes());
    }
    feed(&app, id, format!("{}$ ", m("A")).as_bytes());
    // Command #0 (lines 0 and 1) gave way; its marks are still there.
    assert_eq!(app.command_at(id, 1).unwrap(), None);
    assert!(app.command_at(id, 3).unwrap().is_some());
    app.scroll_to(id, ScrollTarget::Top).unwrap();
    app.scroll_to(id, ScrollTarget::PageUp).unwrap();
    offset(&app, id, app.scroll_offset(id).unwrap() - 1);
    // Top row 1: the prompt on line 0 is the stop above.
    let top = app.scroll_offset(id).unwrap();
    assert_eq!(prev(&app, id), (true, top + 1));
}

// ------------------------------------------------------------ the ⌘↑ rule

/// N1: only the default chord is upgraded; a user's own binding of ⌘↑ to
/// `scroll_page_up` pages.
#[test]
fn the_prompt_rule_applies_to_the_default_chord_only() {
    let up = KeyEvent::new(Key::Up, Modifiers::SUPER);
    let down = KeyEvent::new(Key::Down, Modifiers::SUPER);
    let command = |name: &str| CommandId::find(name, None).unwrap();
    let app = app();
    let id = session(&app, 4, 20, 100);
    assert_eq!(
        app.run_key_binding(id, &up).unwrap(),
        Some(InputOutcome::Command(command("prompt_previous")))
    );
    assert_eq!(
        app.run_key_binding(id, &down).unwrap(),
        Some(InputOutcome::Command(command("prompt_next")))
    );
    let config = Config::from_toml_lenient(
        "[keys]\n'cmd+up' = 'scroll_page_up'\n'cmd+shift+up' = 'scroll_page_down'\n",
    )
    .unwrap()
    .config;
    let app = App::new(config, ConfigPaths::new("/nonexistent/tessera"));
    let id = session(&app, 4, 20, 100);
    assert_eq!(
        app.run_key_binding(id, &up).unwrap(),
        Some(InputOutcome::Command(command("scroll_page_up")))
    );
    let shift_up = KeyEvent::new(Key::Up, Modifiers::SUPER | Modifiers::SHIFT);
    assert_eq!(
        app.run_key_binding(id, &shift_up).unwrap(),
        Some(InputOutcome::Command(command("scroll_page_down"))),
        "not the default chord"
    );
    // ⌘↓ is still the default.
    assert_eq!(
        app.run_key_binding(id, &down).unwrap(),
        Some(InputOutcome::Command(command("prompt_next")))
    );
}

// ------------------------------------------- G: the exit-status gutter (#45b)

/// The gutter's hover (`App::command_mark_at`): the exit status a prompt's
/// mark is drawn for, from the grid, and the command's duration from its
/// record. A row with no mark has none; commands that one chunk ran at
/// once keep their marks (D-COALESCE) but the coalesced ones have no
/// duration; after a resize no duration is known (ADR 0039).
#[test]
fn g01_the_gutter_hover_reports_exit_status_and_duration() {
    use std::time::Duration;

    let app = app();
    let id = session(&app, 6, 20, 100);
    feed(
        &app,
        id,
        format!("{}\r\n", prompt(None, "sleep")).as_bytes(),
    );
    feed(&app, id, m("C").as_bytes());
    std::thread::sleep(Duration::from_millis(30));
    feed(
        &app,
        id,
        format!("{}\r\n", prompt(Some(1), "a; b")).as_bytes(),
    );
    // `a; b` in one chunk: `a` exits 0, `b` 2, and the next prompt.
    let chunk = format!(
        "{}{}\r\n{}{}",
        m("C"),
        prompt(Some(0), "b"),
        m("C"),
        prompt(Some(2), "")
    );
    feed(&app, id, chunk.as_bytes());
    let at = |row| app.command_mark_at(id, row).unwrap();
    let sleep = at(0).expect("row 0 is a finished command's prompt");
    assert_eq!(sleep.exit_code, Some(1));
    let ran = sleep.duration.expect("its record knows how long it ran");
    assert!(ran >= Duration::from_millis(30), "{ran:?}");
    assert_eq!(at(1).map(|mark| mark.exit_code), Some(Some(0)));
    let b = at(2).expect("a coalesced command keeps its mark");
    assert_eq!((b.exit_code, b.duration), (Some(2), None));
    assert_eq!(at(3), None, "the prompt being typed at");
    assert_eq!(at(5), None, "a blank row");
    assert_eq!(at(u16::MAX), None);
    app.resize(id, PtySize::new(6, 30)).unwrap();
    let after = at(0).expect("the grid keeps the mark");
    assert_eq!((after.exit_code, after.duration), (Some(1), None));
    assert!(app.command_mark_at(SessionId::from_raw(9999), 0).is_err());
}

/// The hover pairs a mark with a record only when they agree: the record
/// of a command that ended with `D;1` does not lend its duration to a mark
/// that, once the `D`'s row was erased, the grid draws as unknown.
#[test]
fn g02_a_record_that_disagrees_with_the_mark_gives_no_duration() {
    let app = app();
    let id = session(&app, 6, 20, 100);
    feed(&app, id, format!("{}\r\n", prompt(None, "x")).as_bytes());
    feed(&app, id, format!("{}out\r\n", m("C")).as_bytes());
    feed(&app, id, m("D;1").as_bytes());
    let ended = app.command_mark_at(id, 0).unwrap().expect("finished");
    assert_eq!(ended.exit_code, Some(1));
    assert!(ended.duration.is_some(), "the record agrees with the mark");
    // Delete the `D`'s row (DL) and draw the next prompt in its place.
    feed(&app, id, format!("\r\x1b[M{}", prompt(None, "")).as_bytes());
    let mark = app
        .command_mark_at(id, 0)
        .unwrap()
        .expect("ended by the prompt");
    assert_eq!((mark.exit_code, mark.duration), (None, None));
}

/// #368 (adversarial h1): after Ctrl-L the shell clears the screen in
/// place (ED 2, no row goes to the history) and draws its prompt on the
/// line the cleared command's prompt had. When the next command runs and
/// ends inside one chunk, coalescing leaves the log no record of it, and
/// the hover must not lend it the cleared command's duration.
#[test]
fn g03_a_command_run_after_an_in_place_clear_has_no_borrowed_duration() {
    use std::time::Duration;

    let app = app();
    let id = session(&app, 6, 20, 100);
    feed(&app, id, format!("{}\r\n", prompt(None, "x")).as_bytes());
    feed(&app, id, format!("{}out\r\n", m("C")).as_bytes());
    std::thread::sleep(Duration::from_millis(30));
    feed(&app, id, prompt(Some(0), "").as_bytes());
    let cleared = app.command_mark_at(id, 0).unwrap().expect("finished");
    assert_eq!(cleared.exit_code, Some(0));
    assert!(
        cleared
            .duration
            .is_some_and(|d| d >= Duration::from_millis(30))
    );
    // Ctrl-L, the prompt redrawn on row 0, and `y` run to its end, in one
    // chunk: the same rows and exit status as `x`.
    let chunk = format!(
        "\x1b[H\x1b[2J{}\r\n{}out\r\n{}",
        prompt(None, "y"),
        m("C"),
        prompt(Some(0), "")
    );
    feed(&app, id, chunk.as_bytes());
    let mark = app.command_mark_at(id, 0).unwrap().expect("y finished");
    assert_eq!((mark.exit_code, mark.duration), (Some(0), None));
}
