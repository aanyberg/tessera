//! Mouse reports (#12): [`encode_mouse`] against the encoder rows of
//! `tests/fixtures/mouse_spec.txt`, whose header cites xterm's ctlseqs and
//! `button.c`, Alacritty, kitty and Ghostty and lists the decisions the rows
//! encode.
//!
//! Each encoder row is one call: `tracking:`, `encoding:`, `event:` (kind
//! and button), `mods:`, and `cell:` or `px:` with `area:` (through
//! [`MouseReport::with_pixels`]); `expect:` is hex bytes, text with `\e`
//! for ESC and the escapes of [`common::decode`], or `none`. The routing
//! rows (`D`, `W`, `G`: sequences through `App::send_input`) belong to
//! `tessera-app-core`, which reads the same fixture; here they are only
//! counted, so every row of the fixture is run by one of the two crates.
//!
//! [`round_trip`] checks every report the encoder produces with an
//! independent decoder.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;

use proptest::prelude::*;
use tessera_term::{
    Modifiers, MouseEncoding, MouseReport, MouseReportButton, MouseReportKind, MouseTracking,
    encode_mouse,
};

const FIXTURE: &str = include_str!("fixtures/mouse_spec.txt");
const FIXTURE_NAME: &str = "fixtures/mouse_spec.txt";

/// Encoder rows in the fixture.
const ENCODER_ROWS: usize = 141;
/// Routing rows in the fixture (checked by `tessera-app-core`).
const ROUTING_ROWS: usize = 43;

/// One encoder row.
#[derive(Debug)]
struct Row {
    id: String,
    tracking: MouseTracking,
    encoding: MouseEncoding,
    report: MouseReport,
    expect: Option<Vec<u8>>,
}

/// The rows of the fixture, as `(id, key-value lines)`.
fn blocks() -> Vec<(String, Vec<(String, String)>)> {
    let mut blocks: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for line in FIXTURE.lines() {
        if let Some(id) = line.strip_prefix("### ") {
            blocks.push((id.trim().to_owned(), Vec::new()));
        } else if line.starts_with('#') || line.trim().is_empty() {
        } else if let Some((key, value)) = line.split_once(": ") {
            let (_, keys) = blocks.last_mut().expect("a key before the first row");
            keys.push((key.to_owned(), value.trim().to_owned()));
        } else {
            panic!("{FIXTURE_NAME}: unexpected line {line:?}");
        }
    }
    blocks
}

fn is_encoder_row(keys: &[(String, String)]) -> bool {
    keys.iter().any(|(key, _)| key == "tracking")
}

fn parse_tracking(text: &str) -> MouseTracking {
    match text {
        "None" => MouseTracking::None,
        "X10" => MouseTracking::X10,
        "Normal" => MouseTracking::Normal,
        "Button" => MouseTracking::Button,
        "AnyMotion" => MouseTracking::AnyMotion,
        other => panic!("unknown tracking {other:?}"),
    }
}

fn parse_encoding(text: &str) -> MouseEncoding {
    match text {
        "Default" => MouseEncoding::Default,
        "Utf8" => MouseEncoding::Utf8,
        "Sgr" => MouseEncoding::Sgr,
        "Urxvt" => MouseEncoding::Urxvt,
        "SgrPixels" => MouseEncoding::SgrPixels,
        other => panic!("unknown encoding {other:?}"),
    }
}

fn parse_event(text: &str) -> (MouseReportKind, MouseReportButton) {
    let (kind, button) = text.split_once(' ').expect("`<kind> <button>`");
    let kind = match kind {
        "press" => MouseReportKind::Press,
        "release" => MouseReportKind::Release,
        "motion" => MouseReportKind::Motion,
        other => panic!("unknown kind {other:?}"),
    };
    let button = match button {
        "none" => MouseReportButton::None,
        "left" => MouseReportButton::Left,
        "middle" => MouseReportButton::Middle,
        "right" => MouseReportButton::Right,
        "back" => MouseReportButton::Back,
        "forward" => MouseReportButton::Forward,
        "wheel-up" => MouseReportButton::WheelUp,
        "wheel-down" => MouseReportButton::WheelDown,
        "wheel-left" => MouseReportButton::WheelLeft,
        "wheel-right" => MouseReportButton::WheelRight,
        other => panic!("unknown button {other:?}"),
    };
    (kind, button)
}

fn parse_mods(text: &str) -> Modifiers {
    if text == "-" {
        return Modifiers::NONE;
    }
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

fn pair<T: std::str::FromStr>(text: &str, sep: char) -> (T, T) {
    let (a, b) = text.split_once(sep).expect("a pair");
    let parse = |s: &str| s.trim().parse().ok().expect("a number");
    (parse(a), parse(b))
}

fn parse_expect(text: &str) -> Option<Vec<u8>> {
    if text == "none" {
        return None;
    }
    if text.starts_with("\\e") {
        return Some(common::decode(&text.replace("\\e", "\\x1b")));
    }
    Some(
        text.split_whitespace()
            .map(|hex| u8::from_str_radix(hex, 16).expect("a hex byte"))
            .collect(),
    )
}

fn row(id: &str, keys: &[(String, String)]) -> Row {
    let get = |name: &str| {
        keys.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let need = |name: &str| get(name).unwrap_or_else(|| panic!("{id}: no `{name}:`"));
    let (kind, button) = parse_event(need("event"));
    let mut report = MouseReport::new(kind, button, 0, 0).with_modifiers(parse_mods(need("mods")));
    if let Some(cell) = get("cell") {
        let (col, row) = pair(cell, ',');
        report.col = col;
        report.row = row;
    }
    if let Some(px) = get("px") {
        let (x, y): (f32, f32) = pair(px, ',');
        let (width, height) = pair(need("area"), 'x');
        report = report.with_pixels(x, y, width, height);
    }
    Row {
        id: id.to_owned(),
        tracking: parse_tracking(need("tracking")),
        encoding: parse_encoding(need("encoding")),
        report,
        expect: parse_expect(need("expect")),
    }
}

/// Every encoder row, through [`common::check_table`]; the routing rows
/// are counted so that a row added to the fixture is run by one crate.
#[test]
fn every_encoder_row() {
    let blocks = blocks();
    let (encoder, routing): (Vec<_>, Vec<_>) =
        blocks.iter().partition(|(_, keys)| is_encoder_row(keys));
    assert_eq!(routing.len(), ROUTING_ROWS, "routing rows in the fixture");
    let rows: Vec<Row> = encoder.iter().map(|(id, keys)| row(id, keys)).collect();
    common::check_table(
        FIXTURE_NAME,
        ENCODER_ROWS,
        rows.iter().map(|row| (row.id.clone(), row)),
        |row| {
            let got = encode_mouse(&row.report, row.tracking, row.encoding);
            assert_eq!(
                got.as_deref().map(String::from_utf8_lossy),
                row.expect.as_deref().map(String::from_utf8_lossy),
                "{}: {:?} in {:?} with {:?}",
                row.id,
                row.report,
                row.tracking,
                row.encoding
            );
            assert_eq!(got, row.expect, "{}: exact bytes", row.id);
        },
    );
}

/// Tracking off reports nothing, whatever the event and encoding.
#[test]
fn tracking_off_reports_nothing() {
    for encoding in ENCODINGS {
        for kind in KINDS {
            for button in BUTTONS {
                let report = MouseReport::new(kind, button, 3, 4);
                assert_eq!(encode_mouse(&report, MouseTracking::None, encoding), None);
            }
        }
    }
}

const ENCODINGS: [MouseEncoding; 5] = [
    MouseEncoding::Default,
    MouseEncoding::Utf8,
    MouseEncoding::Sgr,
    MouseEncoding::Urxvt,
    MouseEncoding::SgrPixels,
];

const TRACKINGS: [MouseTracking; 5] = [
    MouseTracking::None,
    MouseTracking::X10,
    MouseTracking::Normal,
    MouseTracking::Button,
    MouseTracking::AnyMotion,
];

const KINDS: [MouseReportKind; 3] = [
    MouseReportKind::Press,
    MouseReportKind::Release,
    MouseReportKind::Motion,
];

const BUTTONS: [MouseReportButton; 10] = [
    MouseReportButton::None,
    MouseReportButton::Left,
    MouseReportButton::Middle,
    MouseReportButton::Right,
    MouseReportButton::WheelUp,
    MouseReportButton::WheelDown,
    MouseReportButton::WheelLeft,
    MouseReportButton::WheelRight,
    MouseReportButton::Back,
    MouseReportButton::Forward,
];

/// A report decoded back from the bytes: the button code with its
/// modifier and motion bits, the one-based position and whether it is an
/// SGR release.
#[derive(Debug, PartialEq, Eq)]
struct Decoded {
    code: u32,
    x: u32,
    y: u32,
    sgr_release: bool,
}

/// An independent decoder for every encoding (xterm ctlseqs), so the
/// property below does not trust the encoder's own arithmetic.
fn decode_report(bytes: &[u8], encoding: MouseEncoding) -> Decoded {
    match encoding {
        MouseEncoding::Default => {
            assert_eq!(&bytes[..3], b"\x1b[M");
            assert_eq!(bytes.len(), 6);
            let value = |b: u8| u32::from(b) - 32;
            Decoded {
                code: value(bytes[3]),
                x: value(bytes[4]),
                y: value(bytes[5]),
                sgr_release: false,
            }
        }
        MouseEncoding::Utf8 => {
            assert_eq!(&bytes[..3], b"\x1b[M");
            let text = std::str::from_utf8(&bytes[3..]).expect("UTF-8 values");
            let values: Vec<u32> = text.chars().map(|c| u32::from(c) - 32).collect();
            assert_eq!(values.len(), 3);
            Decoded {
                code: values[0],
                x: values[1],
                y: values[2],
                sgr_release: false,
            }
        }
        MouseEncoding::Sgr | MouseEncoding::SgrPixels | MouseEncoding::Urxvt => {
            let text = std::str::from_utf8(bytes).expect("ASCII");
            let sgr = encoding != MouseEncoding::Urxvt;
            let body = text
                .strip_prefix(if sgr { "\x1b[<" } else { "\x1b[" })
                .expect("the CSI prefix");
            let (body, last) = body.split_at(body.len() - 1);
            assert!(last == "M" || (sgr && last == "m"), "final {last:?}");
            let values: Vec<u32> = body
                .split(';')
                .map(|v| v.parse().expect("decimal"))
                .collect();
            assert_eq!(values.len(), 3);
            Decoded {
                code: if sgr { values[0] } else { values[0] - 32 },
                x: values[1],
                y: values[2],
                sgr_release: last == "m",
            }
        }
        other => panic!("unknown encoding {other:?}"),
    }
}

fn any_report() -> impl Strategy<Value = MouseReport> {
    (
        0..KINDS.len(),
        0..BUTTONS.len(),
        0u32..3000,
        0u32..3000,
        0u32..5000,
        0u32..5000,
        any::<u8>(),
    )
        .prop_map(|(kind, button, col, row, x, y, mods)| {
            let mut report = MouseReport::new(KINDS[kind], BUTTONS[button], col, row)
                .with_modifiers(Modifiers::from_bits(mods));
            report.x_px = x;
            report.y_px = y;
            report
        })
}

proptest! {
    /// Every report the encoder writes decodes, by ctlseqs' rules, back to
    /// the button code xterm's `BtnCode` gives (button, release 3 in the
    /// legacy encodings, +4/+8/+16 for Shift/Alt/Control except in X10,
    /// +32 for motion) and to the one-based position, clamped to the limit
    /// only for a legacy release.
    #[test]
    fn round_trip(
        report in any_report(),
        tracking in 0..TRACKINGS.len(),
        encoding in 0..ENCODINGS.len(),
    ) {
        let (tracking, encoding) = (TRACKINGS[tracking], ENCODINGS[encoding]);
        let Some(bytes) = encode_mouse(&report, tracking, encoding) else {
            return Ok(());
        };
        let got = decode_report(&bytes, encoding);
        let release = report.kind == MouseReportKind::Release;
        let sgr = matches!(encoding, MouseEncoding::Sgr | MouseEncoding::SgrPixels);
        let base = match report.button {
            MouseReportButton::None => 3,
            MouseReportButton::Left => 0,
            MouseReportButton::Middle => 1,
            MouseReportButton::Right => 2,
            MouseReportButton::WheelUp => 64,
            MouseReportButton::WheelDown => 65,
            MouseReportButton::WheelLeft => 66,
            MouseReportButton::WheelRight => 67,
            MouseReportButton::Back => 128,
            MouseReportButton::Forward => 129,
            other => panic!("{other:?}"),
        };
        let mut code = if release && !sgr { 3 } else { base };
        if tracking != MouseTracking::X10 {
            let mods = report.modifiers;
            code += [(Modifiers::SHIFT, 4), (Modifiers::ALT, 8), (Modifiers::CTRL, 16)]
                .into_iter()
                .filter(|&(m, _)| mods.contains(m))
                .map(|(_, bit)| bit)
                .sum::<u32>();
        }
        if report.kind == MouseReportKind::Motion {
            code += 32;
        }
        let (x, y) = if encoding == MouseEncoding::SgrPixels {
            (report.x_px + 1, report.y_px + 1)
        } else {
            (report.col + 1, report.row + 1)
        };
        let limit = match encoding {
            MouseEncoding::Default => 223,
            MouseEncoding::Utf8 => 2015,
            _ => u32::MAX,
        };
        prop_assert!(release || (x <= limit && y <= limit), "{x},{y} beyond {limit}");
        let want = Decoded { code, x: x.min(limit), y: y.min(limit), sgr_release: sgr && release };
        prop_assert_eq!(got, want);
        // What each tracking mode reports at all.
        let wheel = matches!(
            report.button,
            MouseReportButton::WheelUp
                | MouseReportButton::WheelDown
                | MouseReportButton::WheelLeft
                | MouseReportButton::WheelRight
        );
        prop_assert!(!(wheel && report.kind != MouseReportKind::Press));
        prop_assert!(report.button != MouseReportButton::None || report.kind == MouseReportKind::Motion);
        match tracking {
            MouseTracking::X10 => prop_assert!(
                report.kind == MouseReportKind::Press
                    && matches!(
                        report.button,
                        MouseReportButton::Left | MouseReportButton::Middle | MouseReportButton::Right
                    )
            ),
            MouseTracking::Normal => prop_assert!(report.kind != MouseReportKind::Motion),
            MouseTracking::Button => prop_assert!(
                report.kind != MouseReportKind::Motion || report.button != MouseReportButton::None
            ),
            _ => {}
        }
    }

    /// The SGR and urxvt digits, written without the formatter (#194.10),
    /// are exactly what `format!` writes, over the whole coordinate range.
    #[test]
    fn sgr_and_urxvt_digits_match_the_formatter(
        report in any_report(),
        col in any::<u32>(),
        row in any::<u32>(),
        x in any::<u32>(),
        y in any::<u32>(),
        encoding in prop::sample::select(vec![
            MouseEncoding::Sgr,
            MouseEncoding::SgrPixels,
            MouseEncoding::Urxvt,
        ]),
    ) {
        let mut report = report;
        report.col = col;
        report.row = row;
        report.x_px = x;
        report.y_px = y;
        let Some(bytes) = encode_mouse(&report, MouseTracking::AnyMotion, encoding) else {
            return Ok(());
        };
        let got = decode_report(&bytes, encoding);
        let (x, y) = if encoding == MouseEncoding::SgrPixels { (x, y) } else { (col, row) };
        prop_assert_eq!((got.x, got.y), (x.saturating_add(1), y.saturating_add(1)));
        let want = if encoding == MouseEncoding::Urxvt {
            format!("\x1b[{};{};{}M", got.code + 32, got.x, got.y)
        } else {
            let last = if got.sgr_release { 'm' } else { 'M' };
            format!("\x1b[<{};{};{}{last}", got.code, got.x, got.y)
        };
        prop_assert_eq!(String::from_utf8_lossy(&bytes), want);
    }

    /// A press, motion or wheel event the legacy encodings cannot express is
    /// dropped, but a release never is.
    #[test]
    fn a_release_is_never_dropped(
        col in 0u32..100_000,
        row in 0u32..100_000,
        button in 1..BUTTONS.len(),
        tracking in 2..TRACKINGS.len(),
        encoding in 0..ENCODINGS.len(),
    ) {
        let button = BUTTONS[button];
        let report = MouseReport::new(MouseReportKind::Release, button, col, row);
        let got = encode_mouse(&report, TRACKINGS[tracking], ENCODINGS[encoding]);
        let wheel = matches!(
            button,
            MouseReportButton::WheelUp
                | MouseReportButton::WheelDown
                | MouseReportButton::WheelLeft
                | MouseReportButton::WheelRight
        );
        prop_assert_eq!(got.is_some(), !wheel);
    }
}
