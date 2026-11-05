//! Mouse reports: the bytes a mouse event sends to a program that asked for
//! mouse tracking (#12, ADR 0027).
//!
//! [`encode_mouse`] is a pure function of one [`MouseReport`], the tracking
//! mode ([`MouseTracking`], DECSET 9, 1000, 1002, 1003) and the report
//! encoding ([`MouseEncoding`], DECSET 1005, 1006, 1015, 1016), which the
//! host reads from [`Modes`](crate::Modes). It keeps no state: suppressing
//! motion within one cell, remembering which presses were reported and
//! deciding which events reach the program at all (Shift selects, Cmd
//! never reports) belong to the host (`tessera-app-core`).
//!
//! What each tracking mode reports (xterm ctlseqs "Mouse Tracking"):
//!
//! | tracking | press | release | motion with a button held | motion with none | wheel | modifiers |
//! |---|---|---|---|---|---|---|
//! | [`MouseTracking::None`] | no | no | no | no | no | - |
//! | [`MouseTracking::X10`] | left, middle, right | no | no | no | no | never |
//! | [`MouseTracking::Normal`] | yes | yes | no | no | yes | yes |
//! | [`MouseTracking::Button`] | yes | yes | yes | no | yes | yes |
//! | [`MouseTracking::AnyMotion`] | yes | yes | yes | yes | yes | yes |
//!
//! The button code is xterm's: left 0, middle 1, right 2, wheel up, down,
//! left and right 64 to 67, back 128 and forward 129; plus 4 for Shift, 8
//! for Alt (Option, xterm's Meta) and 16 for Control, and plus 32 for
//! motion. Motion with no button held is code 3 plus 32. In the default,
//! UTF-8 and urxvt encodings a release is code 3 with the modifiers kept;
//! the SGR encodings keep the button and end in `m` instead of `M`. The
//! wheel is reported as presses only, never released.
//!
//! The encodings, all with one-based coordinates (`col + 1`):
//!
//! - [`MouseEncoding::Default`]: `CSI M Cb Cx Cy`, each one byte holding
//!   the value plus 32. A press, motion or wheel event beyond column or
//!   row 223 cannot be expressed and is not reported (Alacritty, kitty and
//!   Ghostty; xterm writes a NUL there instead). A release is never
//!   dropped, since that would leave the program with a button held: its
//!   coordinates are clamped to 223.
//! - [`MouseEncoding::Utf8`] (1005): the same with values of 128 and more
//!   as two-byte UTF-8, up to column or row 2015 (xterm and Alacritty);
//!   beyond, as for the default encoding.
//! - [`MouseEncoding::Sgr`] (1006): `CSI < Cb ; Cx ; Cy M` (`m` for a
//!   release), decimal, no offset, no limit.
//! - [`MouseEncoding::Urxvt`] (1015): `CSI Cb ; Cx ; Cy M`, decimal, `Cb`
//!   with the offset of 32 and the legacy release code, no limit.
//! - [`MouseEncoding::SgrPixels`] (1016): SGR with the pointer's pixel
//!   position in the text area instead of the cell, one-based like every
//!   other value of the format (xterm prints `pixel + 1`; kitty and
//!   Ghostty print the raw pixel). See [`MouseReport::with_pixels`].
//!
//! Reports always start with the 7-bit `ESC [`. In 8-bit mode
//! ([`Terminal::c1_8bit_replies`]) the host passes each report through
//! [`to_8bit_controls`] for xterm's 8-bit CSI (`0x9B`).
//!
//! [`Terminal::c1_8bit_replies`]: crate::Terminal::c1_8bit_replies
//! [`to_8bit_controls`]: crate::to_8bit_controls

use crate::keyboard::Modifiers;
use crate::modes::{MouseEncoding, MouseTracking};

/// The largest one-based coordinate the default encoding can carry
/// (`255 - 32`).
const DEFAULT_LIMIT: u32 = 223;

/// The largest one-based coordinate the UTF-8 encoding carries
/// (`2047 - 32`, the last two-byte code point).
const UTF8_LIMIT: u32 = 2015;

/// The button code a legacy-encoded release carries.
const RELEASE_CODE: u32 = 3;

/// Added to the button code of motion.
const MOTION_BIT: u32 = 32;

/// Room for the longest report any encoding writes: with the largest code
/// (189) and two ten-digit coordinates an SGR report is 29 bytes
/// (`ESC[<189;4294967295;4294967295M`) and a urxvt one 28.
const MAX_REPORT_LEN: usize = 32;

/// The mouse button a report is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MouseReportButton {
    /// No button: motion with every button up. Not valid for a press or a
    /// release.
    #[default]
    None,
    /// The primary (left) button, code 0.
    Left,
    /// The middle button, code 1.
    Middle,
    /// The secondary (right) button, code 2.
    Right,
    /// The wheel turned up (towards older output), code 64. Pressed only.
    WheelUp,
    /// The wheel turned down, code 65. Pressed only.
    WheelDown,
    /// The wheel tilted left (X11 button 6), code 66. Pressed only.
    WheelLeft,
    /// The wheel tilted right (X11 button 7), code 67. Pressed only.
    WheelRight,
    /// The "back" side button (X11 button 8), code 128.
    Back,
    /// The "forward" side button (X11 button 9), code 129.
    Forward,
}

impl MouseReportButton {
    /// xterm's button code, before modifiers and the motion bit; `None`
    /// for [`MouseReportButton::None`].
    const fn code(self) -> Option<u32> {
        match self {
            Self::None => None,
            Self::Left => Some(0),
            Self::Middle => Some(1),
            Self::Right => Some(2),
            Self::WheelUp => Some(64),
            Self::WheelDown => Some(65),
            Self::WheelLeft => Some(66),
            Self::WheelRight => Some(67),
            Self::Back => Some(128),
            Self::Forward => Some(129),
        }
    }

    /// Whether this is one of the four wheel directions.
    const fn is_wheel(self) -> bool {
        matches!(
            self,
            Self::WheelUp | Self::WheelDown | Self::WheelLeft | Self::WheelRight
        )
    }

    /// Whether X10 mode (DECSET 9) reports a press of this button: left,
    /// middle and right only.
    const fn in_x10(self) -> bool {
        matches!(self, Self::Left | Self::Middle | Self::Right)
    }
}

/// What happened to the button of a [`MouseReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MouseReportKind {
    /// The button went down; for a wheel button, the wheel turned one
    /// step.
    #[default]
    Press,
    /// The button went up.
    Release,
    /// The pointer moved with the button held, or with no button
    /// ([`MouseReportButton::None`]).
    Motion,
}

/// One mouse event to report, as [`encode_mouse`] takes it.
///
/// Built with [`MouseReport::new`] and the `with_*` methods, so fields can
/// be added without breaking callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct MouseReport {
    /// Press, release or motion.
    pub kind: MouseReportKind,
    /// The button that changed (press, release) or is held (motion).
    pub button: MouseReportButton,
    /// Zero-based column of the cell under the pointer.
    pub col: u32,
    /// Zero-based row of the cell under the pointer.
    pub row: u32,
    /// Zero-based pixel column of the pointer in the text area, for
    /// [`MouseEncoding::SgrPixels`]; see [`MouseReport::with_pixels`].
    pub x_px: u32,
    /// Zero-based pixel row of the pointer in the text area.
    pub y_px: u32,
    /// Modifiers held. Shift, Alt and Control are reported; every other
    /// modifier (Super, the lock keys) is not.
    pub modifiers: Modifiers,
}

impl MouseReport {
    /// A `kind` event of `button` over cell (`col`, `row`), zero-based, at
    /// pixel (0, 0) and with no modifiers.
    #[must_use]
    pub fn new(kind: MouseReportKind, button: MouseReportButton, col: u32, row: u32) -> Self {
        Self {
            kind,
            button,
            col,
            row,
            ..Self::default()
        }
    }

    /// Sets the pointer's pixel position for [`MouseEncoding::SgrPixels`]
    /// from `x` and `y`, in pixels from the top-left corner of the text
    /// area (inside any padding), clamped to the text area of `width` by
    /// `height` pixels: a drag that leaves the view reports the nearest
    /// edge, never 0 or a negative. Fractions are dropped; a value that is
    /// not a number counts as 0. A `width` or `height` of 0 means the size
    /// is unknown: that coordinate is then only kept from going negative.
    #[must_use]
    pub fn with_pixels(mut self, x: f32, y: f32, width: u32, height: u32) -> Self {
        self.x_px = clamp_pixel(x, width);
        self.y_px = clamp_pixel(y, height);
        self
    }

    /// Sets the modifiers.
    #[must_use]
    pub fn with_modifiers(mut self, modifiers: Modifiers) -> Self {
        self.modifiers = modifiers;
        self
    }
}

/// `value` floored into `0..extent` (`0..` when `extent` is 0).
fn clamp_pixel(value: f32, extent: u32) -> u32 {
    let most = if extent == 0 { u32::MAX } else { extent - 1 };
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "float-to-int `as` saturates and the value is positive"
    )]
    let floored = value.floor() as u32;
    floored.min(most)
}

/// The bytes that report `report` to a program in `tracking` mode with
/// `encoding`, or `None` when that mode does not report it: tracking is
/// off, the event is not one the mode reports (see the module
/// documentation), a press or release names no button, a wheel button is
/// released or moved, or a press or motion lies beyond what the encoding
/// can express.
#[must_use]
pub fn encode_mouse(
    report: &MouseReport,
    tracking: MouseTracking,
    encoding: MouseEncoding,
) -> Option<Vec<u8>> {
    let button = report.button;
    let x10 = match tracking {
        MouseTracking::None => return None,
        MouseTracking::X10 => {
            if report.kind != MouseReportKind::Press || !button.in_x10() {
                return None;
            }
            true
        }
        MouseTracking::Normal | MouseTracking::Button | MouseTracking::AnyMotion => false,
    };
    let release = report.kind == MouseReportKind::Release;
    let base = match report.kind {
        MouseReportKind::Press | MouseReportKind::Release => {
            if button.is_wheel() && release {
                return None;
            }
            button.code()?
        }
        MouseReportKind::Motion => {
            let reported = match button {
                MouseReportButton::None => tracking == MouseTracking::AnyMotion,
                _ => matches!(tracking, MouseTracking::Button | MouseTracking::AnyMotion),
            };
            if !reported || button.is_wheel() {
                return None;
            }
            button.code().unwrap_or(RELEASE_CODE) + MOTION_BIT
        }
    };
    let modifiers = if x10 {
        0
    } else {
        modifier_bits(report.modifiers)
    };
    let sgr = matches!(encoding, MouseEncoding::Sgr | MouseEncoding::SgrPixels);
    let code = if release && !sgr { RELEASE_CODE } else { base } + modifiers;
    let (x, y) = if encoding == MouseEncoding::SgrPixels {
        (report.x_px, report.y_px)
    } else {
        (report.col, report.row)
    };
    // One-based from here on.
    let (x, y) = (x.saturating_add(1), y.saturating_add(1));
    // Every report fits, so it is written in place with one allocation.
    let mut out = Vec::with_capacity(MAX_REPORT_LEN);
    match encoding {
        MouseEncoding::Default | MouseEncoding::Utf8 => {
            let limit = if encoding == MouseEncoding::Default {
                DEFAULT_LIMIT
            } else {
                UTF8_LIMIT
            };
            let (x, y) = if release {
                (x.min(limit), y.min(limit))
            } else if x > limit || y > limit {
                return None;
            } else {
                (x, y)
            };
            out.extend_from_slice(b"\x1b[M");
            for value in [code + 32, x + 32, y + 32] {
                if encoding == MouseEncoding::Utf8 {
                    push_utf8(&mut out, value);
                } else {
                    push_byte(&mut out, value);
                }
            }
        }
        MouseEncoding::Sgr | MouseEncoding::SgrPixels => {
            out.extend_from_slice(b"\x1b[<");
            push_decimal(&mut out, code);
            out.push(b';');
            push_decimal(&mut out, x);
            out.push(b';');
            push_decimal(&mut out, y);
            out.push(if release { b'm' } else { b'M' });
        }
        MouseEncoding::Urxvt => {
            out.extend_from_slice(b"\x1b[");
            push_decimal(&mut out, code + 32);
            out.push(b';');
            push_decimal(&mut out, x);
            out.push(b';');
            push_decimal(&mut out, y);
            out.push(b'M');
        }
    }
    debug_assert!(out.len() <= MAX_REPORT_LEN, "a report outgrew its buffer");
    Some(out)
}

/// xterm's modifier bits: Shift 4, Alt (Meta) 8, Control 16.
fn modifier_bits(modifiers: Modifiers) -> u32 {
    [
        (Modifiers::SHIFT, 4),
        (Modifiers::ALT, 8),
        (Modifiers::CTRL, 16),
    ]
    .into_iter()
    .filter(|&(modifier, _)| modifiers.contains(modifier))
    .map(|(_, bit)| bit)
    .sum()
}

/// Pushes the decimal digits of `value`, as `write!` would, without the
/// formatting machinery or a temporary string.
fn push_decimal(out: &mut Vec<u8>, value: u32) {
    // u32::MAX has 10 digits.
    let mut digits = [0u8; 10];
    let mut start = digits.len();
    let mut rest = value;
    loop {
        start -= 1;
        digits[start] = b'0' + u8::try_from(rest % 10).expect("a digit");
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[start..]);
}

/// Pushes `value` as one raw byte: the default encoding's limit and the
/// largest button code (220 with every bit) keep it below 256.
fn push_byte(out: &mut Vec<u8>, value: u32) {
    out.push(u8::try_from(value).expect("bounded by the limit and the code range"));
}

/// Pushes `value` (below 2048) as xterm's 1005 encoding does: one byte
/// below 128, otherwise the two-byte UTF-8 form.
fn push_utf8(out: &mut Vec<u8>, value: u32) {
    let byte = |v: u32| u8::try_from(v).expect("masked to a byte");
    if value < 0x80 {
        out.push(byte(value));
    } else {
        out.push(byte(0xC0 | (value >> 6)));
        out.push(byte(0x80 | (value & 0x3F)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixels_are_floored_and_clamped_to_the_text_area() {
        assert_eq!(clamp_pixel(-5.0, 640), 0);
        assert_eq!(clamp_pixel(f32::NAN, 640), 0);
        assert_eq!(clamp_pixel(35.9, 640), 35);
        assert_eq!(clamp_pixel(700.0, 640), 639);
        assert_eq!(clamp_pixel(1e12, 0), u32::MAX);
        assert_eq!(clamp_pixel(f32::INFINITY, 0), u32::MAX);
    }

    #[test]
    fn decimals_match_the_formatter() {
        let decimal = |value| {
            let mut out = Vec::new();
            push_decimal(&mut out, value);
            String::from_utf8(out).expect("ASCII digits")
        };
        for value in [
            0,
            1,
            9,
            10,
            99,
            100,
            223,
            999,
            1000,
            65_535,
            u32::MAX - 1,
            u32::MAX,
        ] {
            assert_eq!(decimal(value), value.to_string());
        }
    }

    #[test]
    fn utf8_values_are_one_or_two_bytes() {
        let utf8 = |value| {
            let mut out = Vec::new();
            push_utf8(&mut out, value);
            out
        };
        assert_eq!(utf8(127), [0x7f]);
        assert_eq!(utf8(128), [0xc2, 0x80]);
        assert_eq!(utf8(2047), [0xdf, 0xbf]);
        for value in 128..2048 {
            let encoded = utf8(value);
            let decoded = std::str::from_utf8(&encoded).expect("valid UTF-8");
            assert_eq!(decoded.chars().next().map(u32::from), Some(value));
        }
    }
}
