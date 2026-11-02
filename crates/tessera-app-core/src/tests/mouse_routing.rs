//! Mouse and wheel routing through [`App::send_input`] (#12, ADR 0027).
//!
//! [`every_routing_row`] runs the routing rows (`D`, `W`, `G`) of
//! `tessera-term`'s `tests/fixtures/mouse_spec.txt` on a session with a fake
//! PTY: each `step:` is a child write or one input event with its expected
//! outcome, `written:` the bytes the child must have received, in order, and
//! `offset:` the scroll offset afterwards. The encoder rows of the same
//! fixture are `tessera-term`'s. The tests after it cover what a table row
//! cannot: the flood, a report refused for want of room, and SGR-pixels
//! reports through a real view.

use super::*;
use input::ScrollEvent;

const FIXTURE: &str = include_str!("../../../tessera-term/tests/fixtures/mouse_spec.txt");
const FIXTURE_NAME: &str = "tessera-term/tests/fixtures/mouse_spec.txt";

/// Routing rows in the fixture.
const ROUTING_ROWS: usize = 39;

/// `\e` for ESC, `\xNN` for one raw byte, `\\` for a backslash; every
/// other character is itself (UTF-8).
fn decode(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buf = [0; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('e') => out.push(0x1b),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                out.push(u8::from_str_radix(&hex, 16).expect("two hex digits after \\x"));
            }
            other => panic!("unknown escape \\{other:?} in {text:?}"),
        }
    }
    out
}

fn parse_mods(text: &str) -> Modifiers {
    text.split('+')
        .map(|name| match name {
            "S" => Modifiers::SHIFT,
            "A" => Modifiers::ALT,
            "C" => Modifiers::CTRL,
            "Cmd" => Modifiers::SUPER,
            other => panic!("unknown modifier {other:?}"),
        })
        .fold(Modifiers::NONE, |all, one| all | one)
}

fn parse_cell(text: &str) -> (u16, u16) {
    let (col, row) = text.split_once(',').expect("`col,row`");
    (col.parse().expect("a column"), row.parse().expect("a row"))
}

/// One `step:` of a routing row.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// The child writes these bytes.
    Feed(Vec<u8>),
    /// The child writes this many numbered lines.
    Fill(usize),
    /// An input event and the outcome it must have.
    Input(InputEvent, InputOutcome),
}

fn parse_step(text: &str) -> Step {
    if let Some(bytes) = text.strip_prefix("feed ") {
        return Step::Feed(decode(bytes));
    }
    if let Some(lines) = text.strip_prefix("fill ") {
        return Step::Fill(lines.parse().expect("a line count"));
    }
    let (event, outcome) = text.rsplit_once(" -> ").expect("`<event> -> C|I`");
    let outcome = match outcome {
        "C" => InputOutcome::Consumed,
        "I" => InputOutcome::Ignored,
        other => panic!("unknown outcome {other:?}"),
    };
    let words: Vec<&str> = event.split_whitespace().collect();
    let event = match words.as_slice() {
        ["focus", "gained"] => InputEvent::Focus(FocusEvent::Gained),
        ["focus", "lost"] => InputEvent::Focus(FocusEvent::Lost),
        ["wheel", lines, cell, rest @ ..] => {
            let (col, row) = parse_cell(cell);
            let mods = rest.first().map_or(Modifiers::NONE, |m| parse_mods(m));
            InputEvent::Scroll(
                ScrollEvent::new(lines.parse().expect("delta lines"))
                    .with_cell(col, row)
                    .with_modifiers(mods),
            )
        }
        [kind, button, cell, rest @ ..] => {
            let kind = match *kind {
                "press" => MouseEventKind::Press,
                "release" => MouseEventKind::Release,
                "motion" => MouseEventKind::Motion,
                other => panic!("unknown kind {other:?}"),
            };
            let button = match *button {
                "none" => MouseButton::None,
                "left" => MouseButton::Left,
                "middle" => MouseButton::Middle,
                "right" => MouseButton::Right,
                "back" => MouseButton::Back,
                "forward" => MouseButton::Forward,
                other => panic!("unknown button {other:?}"),
            };
            let (col, row) = parse_cell(cell);
            let mut mouse = input::MouseEvent::new(kind, button, col, row);
            let mut rest = rest.iter();
            while let Some(&word) = rest.next() {
                if word == "px" {
                    let (x, y) = parse_cell(rest.next().expect("`px x,y`"));
                    mouse = mouse.with_pixels(f32::from(x), f32::from(y));
                } else {
                    mouse = mouse.with_modifiers(parse_mods(word));
                }
            }
            InputEvent::Mouse(mouse)
        }
        other => panic!("unknown step {other:?}"),
    };
    Step::Input(event, outcome)
}

/// One routing row.
#[derive(Debug)]
struct Row {
    id: String,
    size: Option<(u16, u16)>,
    steps: Vec<Step>,
    written: Vec<u8>,
    offset: Option<usize>,
    generated: bool,
}

/// The routing rows: every `###` row without `tracking:`.
fn rows() -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut encoder = false;
    for line in FIXTURE.lines() {
        if let Some(id) = line.strip_prefix("### ") {
            rows.push(Row {
                id: id.trim().to_owned(),
                size: None,
                steps: Vec::new(),
                written: Vec::new(),
                offset: None,
                generated: false,
            });
            encoder = false;
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let (key, value) = line.split_once(": ").expect("`key: value`");
        let row = rows.last_mut().expect("a key before the first row");
        match key {
            "tracking" => encoder = true,
            "step" => row.steps.push(parse_step(value.trim())),
            "size" => {
                let (r, c) = value.trim().split_once('x').expect("`RxC`");
                row.size = Some((r.parse().expect("rows"), c.parse().expect("cols")));
            }
            "written" => {
                let value = value.trim();
                if value != "none" {
                    let (count, text) = match value.split_once("x \\") {
                        Some((n, rest)) if n.chars().all(|c| c.is_ascii_digit()) => {
                            (n.parse().expect("a count"), format!("\\{rest}"))
                        }
                        _ => (1, value.to_owned()),
                    };
                    row.written.extend(decode(&text).repeat(count));
                }
            }
            "offset" => row.offset = Some(value.trim().parse().expect("an offset")),
            "generated" => row.generated = true,
            _ => {}
        }
        if encoder {
            row.id.clear();
        }
    }
    rows.retain(|row| !row.id.is_empty());
    rows
}

/// Runs `row` on a fresh session and checks every outcome, the offset and
/// the bytes the child received.
fn check(row: &Row) {
    if row.generated {
        flood();
        return;
    }
    let app = app();
    let events = observe(&app);
    let (id, fake) = attach_fake(&app, false);
    if let Some((rows, cols)) = row.size {
        app.resize(id, PtySize::new(rows, cols)).unwrap();
    }
    for (n, step) in row.steps.iter().enumerate() {
        match step {
            Step::Feed(bytes) => output(&fake, &events, bytes),
            Step::Fill(lines) => output(&fake, &events, &numbered_lines(0, *lines)),
            Step::Input(event, want) => {
                let got = app.send_input(id, event.clone()).unwrap();
                assert_eq!(got, *want, "{}: step {} ({event:?})", row.id, n + 1);
            }
        }
    }
    if let Some(offset) = row.offset {
        assert_eq!(app.scroll_offset(id).unwrap(), offset, "{}: offset", row.id);
    }
    assert_nothing_written_after(&fake, &row.written, &app, id);
}

#[test]
fn every_routing_row() {
    let rows = rows();
    assert_eq!(rows.len(), ROUTING_ROWS, "{FIXTURE_NAME}: routing rows");
    let mut failed = Vec::new();
    for row in &rows {
        if std::panic::catch_unwind(AssertUnwindSafe(|| check(row))).is_err() {
            failed.push(row.id.clone());
        }
    }
    assert!(
        failed.is_empty(),
        "{FIXTURE_NAME}: {} of {} rows failed (messages above): {}",
        failed.len(),
        rows.len(),
        failed.join(", ")
    );
}

fn motion(col: u16) -> InputEvent {
    InputEvent::Mouse(input::MouseEvent::new(
        MouseEventKind::Motion,
        MouseButton::None,
        col,
        0,
    ))
}

/// Fixture row D21 and the brief's flood: 100,000 any-motion events
/// write exactly one report per change of cell, each event returns at
/// once, and what is queued is the reports and nothing else.
fn flood() {
    const EVENTS: usize = 100_000;
    let app = app();
    let events = observe(&app);
    let (id, fake) = attach_fake(&app, false);
    output(&fake, &events, b"\x1b[?1003h\x1b[?1006h");
    let report = |col: u16| format!("\x1b[<35;{};1M", col + 1).into_bytes();
    let mut expected = Vec::new();
    let mut consumed = 0;
    let mut slowest = Duration::ZERO;
    let started = Instant::now();
    // Across 10 cells, 10,000 events in each: one report per cell.
    for n in 0..EVENTS {
        let col = u16::try_from(n / (EVENTS / 10)).unwrap();
        let t = Instant::now();
        let outcome = app.send_input(id, motion(col)).unwrap();
        slowest = slowest.max(t.elapsed());
        if outcome == InputOutcome::Consumed {
            consumed += 1;
            expected.extend(report(col));
        }
    }
    assert_eq!(consumed, 10, "reports for 10 cells");
    // Alternating between two cells: every event reports.
    for n in 0..EVENTS {
        let col = u16::try_from(n % 2).unwrap();
        let t = Instant::now();
        assert_eq!(
            app.send_input(id, motion(col)).unwrap(),
            InputOutcome::Consumed
        );
        slowest = slowest.max(t.elapsed());
        expected.extend(report(col));
    }
    let took = started.elapsed();
    eprintln!(
        "mouse flood: {} events in {took:?}, slowest {slowest:?}, {} bytes queued",
        2 * EVENTS,
        expected.len()
    );
    assert_nothing_written_after(&fake, &expected, &app, id);
}

#[test]
fn a_flood_of_motion_reports_only_cell_changes() {
    flood();
}

/// A report that does not fit in the pending input fails with
/// `WouldBlock` at once and leaves no trace: a press refused is not held,
/// so its release is not sent, and motion refused does not move the
/// baseline.
#[test]
fn a_report_refused_for_room_leaves_no_trace() {
    let app = app();
    let events = observe(&app);
    let (id, fake) = attach_fake(&app, false);
    output(&fake, &events, b"\x1b[?1002h\x1b[?1006h");
    fake.would_block.store(true, Ordering::SeqCst);
    // Fill the pending input to 3 bytes below its bound.
    let fill = "x".repeat(session::PENDING_INPUT_BYTES - 3);
    assert_eq!(app.paste(id, &fill).unwrap(), InputOutcome::Consumed);
    let press = input::MouseEvent::new(MouseEventKind::Press, MouseButton::Left, 2, 0);
    let refused = |result: Result<InputOutcome>| matches!(result, Err(Error::Write { source, .. }) if source.kind() == io::ErrorKind::WouldBlock);
    let started = Instant::now();
    assert!(refused(app.send_input(id, InputEvent::Mouse(press))));
    assert!(
        started.elapsed() < TIMEOUT,
        "a refused report returns at once"
    );
    let session = app.session(id).unwrap();
    assert!(!session.mouse_reports().holds(MouseButton::Left));
    let release = input::MouseEvent::new(MouseEventKind::Release, MouseButton::Left, 2, 0);
    assert_eq!(
        app.send_input(id, InputEvent::Mouse(release)).unwrap(),
        InputOutcome::Ignored
    );
    assert_eq!(session.mouse_reports(), mouse::ReportState::default());
    // Let the queue drain; the same press now reports and is held.
    fake.would_block.store(false, Ordering::SeqCst);
    written_eventually(&fake, fill.as_bytes());
    assert_eq!(
        app.send_input(id, InputEvent::Mouse(press)).unwrap(),
        InputOutcome::Consumed
    );
    assert!(session.mouse_reports().holds(MouseButton::Left));
    let expected = [fill.as_bytes(), b"\x1b[<0;3;1M"].concat();
    assert_nothing_written_after(&fake, &expected, &app, id);
}

/// SGR-pixels (1016) through a real view: the pixel is counted from the
/// text area (the view's padding taken off), one-based, and a drag out of
/// the view is clamped to the text area's edge.
#[test]
fn sgr_pixel_reports_count_from_the_text_area() {
    let app = app();
    let events = observe(&app);
    let (id, fake) = attach_fake(&app, false);
    if !attach_offscreen(&app, id, PhysicalSize::new(800, 600), 2.0) {
        return;
    }
    let metrics = app.view_metrics(id).unwrap();
    output(&fake, &events, b"\x1b[?1002h\x1b[?1016h");
    #[expect(clippy::cast_precision_loss, reason = "small pixel counts")]
    let pad = metrics.padding_px as f32;
    let width = u32::from(metrics.cols) * metrics.cell.width_px;
    let height = u32::from(metrics.rows) * metrics.cell.height_px;
    let mouse = |kind, x: f32, y: f32| {
        InputEvent::Mouse(input::MouseEvent::new(kind, MouseButton::Left, 0, 0).with_pixels(x, y))
    };
    let consumed = |event| app.send_input(id, event).unwrap() == InputOutcome::Consumed;
    assert!(consumed(mouse(
        MouseEventKind::Press,
        pad + 35.5,
        pad + 70.0
    )));
    assert!(consumed(mouse(MouseEventKind::Motion, -40.0, 1e6)));
    // Clamped to the same pixel: suppressed.
    assert!(!consumed(mouse(MouseEventKind::Motion, -1.0, 2e6)));
    assert!(consumed(mouse(MouseEventKind::Release, 1e6, pad)));
    let expected = format!("\x1b[<0;36;71M\x1b[<32;1;{height}M\x1b[<0;{width};1m");
    assert_nothing_written_after(&fake, expected.as_bytes(), &app, id);
}

/// The wheel in SGR-pixels mode reports the last mouse position when it
/// lies in the wheel's cell, else the cell's top-left pixel.
#[test]
fn sgr_pixel_wheel_reports_use_the_pointer_in_the_cell() {
    let app = app();
    let events = observe(&app);
    let (id, fake) = attach_fake(&app, false);
    output(&fake, &events, b"\x1b[?1000h\x1b[?1016h");
    // No view: no padding, unknown cell size.
    let motion = input::MouseEvent::new(MouseEventKind::Motion, MouseButton::None, 4, 4)
        .with_pixels(35.0, 70.0);
    assert_eq!(
        app.send_input(id, InputEvent::Mouse(motion)).unwrap(),
        InputOutcome::Ignored
    );
    let wheel = |col, row| {
        let event = ScrollEvent::new(1.0).with_cell(col, row);
        app.send_input(id, InputEvent::Scroll(event)).unwrap()
    };
    assert_eq!(wheel(4, 4), InputOutcome::Consumed);
    // Another cell, and no cell size known: its origin is pixel 0.
    assert_eq!(wheel(5, 4), InputOutcome::Consumed);
    assert_nothing_written_after(&fake, b"\x1b[<64;36;71M\x1b[<64;1;1M", &app, id);
}
