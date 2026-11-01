//! Mouse reporting for one session: which mouse events reach the program
//! and the state that keeps its view of the buttons consistent (#12, ADR
//! 0027).
//!
//! [`tessera_term::encode_mouse`] turns one event into bytes. What it cannot
//! know lives here, per session ([`ReportState`]):
//!
//! - which buttons the program saw go down, so a release is sent **iff**
//!   its press was reported (a Shift-click that went to selection never
//!   sends a stray release, and a reported press is always released, even
//!   with Shift or Cmd down by then), and a drag reports the lowest button
//!   the program saw held (xterm: left, middle, right, back, forward);
//! - where the last report was, so motion is reported only when it moves
//!   to another cell (another pixel in SGR-pixels mode, 1016). Every report
//!   moves this baseline: press, release, wheel and motion.
//!
//! [`route_mouse`] decides for one [`MouseEvent`]: Shift bypasses reporting
//! (the frontend selects), Cmd is never reported (Cmd-click opens links),
//! and tracking off forgets the held buttons and the baseline, so leaving
//! tracking mode mid-drag never sends a release later. The decision is
//! returned with the state to keep, which the caller stores only once the
//! report is queued: a report refused for want of room
//! ([`std::io::ErrorKind::WouldBlock`]) leaves no trace.

use tessera_term::{
    Modifiers, MouseEncoding, MouseReport, MouseReportButton, MouseReportKind, MouseTracking,
    encode_mouse,
};

use crate::input::{MouseButton, MouseEvent, MouseEventKind};

/// Where the last report put the pointer: a cell, or a pixel of the text
/// area in SGR-pixels mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Position {
    pixels: bool,
    x: u32,
    y: u32,
}

/// One session's mouse report state; starts with no button held and no
/// report made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ReportState {
    /// The buttons whose press was reported and not yet released, one bit
    /// each in the order of [`HELD_ORDER`].
    held: u8,
    /// The position of the last report, if any since tracking last
    /// started.
    baseline: Option<Position>,
}

/// The buttons a drag can report, lowest first: the bit of each is
/// `1 << index`.
const HELD_ORDER: [MouseButton; 5] = [
    MouseButton::Left,
    MouseButton::Middle,
    MouseButton::Right,
    MouseButton::Back,
    MouseButton::Forward,
];

/// The held-button bit of `button`, 0 for [`MouseButton::None`].
fn held_bit(button: MouseButton) -> u8 {
    HELD_ORDER
        .iter()
        .position(|&held| held == button)
        .map_or(0, |index| 1 << index)
}

impl ReportState {
    /// The lowest button the program saw held, or [`MouseButton::None`].
    fn lowest_held(self) -> MouseButton {
        HELD_ORDER
            .iter()
            .enumerate()
            .find(|&(index, _)| self.held & (1 << index) != 0)
            .map_or(MouseButton::None, |(_, &button)| button)
    }

    /// Whether the program saw `button` go down and not come up.
    #[cfg(test)]
    pub(crate) fn holds(self, button: MouseButton) -> bool {
        let bit = held_bit(button);
        bit != 0 && self.held & bit != 0
    }
}

/// The pointer's position in the text area in pixels, and the text area's
/// size (0 when unknown), for SGR-pixels reports; see
/// [`MouseReport::with_pixels`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct PixelPoint {
    /// Pixels from the text area's left edge (may be negative).
    pub(crate) x: f32,
    /// Pixels from the text area's top edge (may be negative).
    pub(crate) y: f32,
    /// The text area's width in pixels, 0 when unknown.
    pub(crate) width: u32,
    /// The text area's height in pixels, 0 when unknown.
    pub(crate) height: u32,
}

/// What to do with one mouse or wheel event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// Queue `bytes` for the program, then keep `next`: the event is
    /// consumed.
    Report { bytes: Vec<u8>, next: ReportState },
    /// Write nothing and keep `next` (a release forgets its button, tracking
    /// off forgets everything): the event is ignored.
    Ignore { next: ReportState },
}

/// The tracking mode and encoding the program chose, and the pointer's
/// pixel position for SGR-pixels mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Mode {
    pub(crate) tracking: MouseTracking,
    pub(crate) encoding: MouseEncoding,
    pub(crate) pixels: PixelPoint,
}

impl Mode {
    /// The report of `kind` for `button` over `event`'s cell and pixel, with
    /// `modifiers`, and where it puts the baseline.
    fn report(
        &self,
        kind: MouseReportKind,
        button: MouseReportButton,
        (col, row): (u16, u16),
        modifiers: Modifiers,
    ) -> (MouseReport, Position) {
        let pixels = self.pixels;
        let report = MouseReport::new(kind, button, u32::from(col), u32::from(row))
            .with_pixels(pixels.x, pixels.y, pixels.width, pixels.height)
            .with_modifiers(modifiers);
        let position = if self.encoding == MouseEncoding::SgrPixels {
            Position {
                pixels: true,
                x: report.x_px,
                y: report.y_px,
            }
        } else {
            Position {
                pixels: false,
                x: report.col,
                y: report.row,
            }
        };
        (report, position)
    }
}

/// The encoder's name for a button.
fn report_button(button: MouseButton) -> MouseReportButton {
    match button {
        MouseButton::None => MouseReportButton::None,
        MouseButton::Left => MouseReportButton::Left,
        MouseButton::Middle => MouseReportButton::Middle,
        MouseButton::Right => MouseReportButton::Right,
        MouseButton::Back => MouseReportButton::Back,
        MouseButton::Forward => MouseReportButton::Forward,
    }
}

/// Decides what `event` (its cell already clamped to the grid) sends in
/// `mode`, given the session's `state`:
///
/// - tracking off: nothing, and the state is forgotten;
/// - a press: reported unless Shift or Cmd is held or the encoding cannot
///   express it (X10's buttons, the default encoding's 223 limit); the
///   program then holds the button;
/// - a release: reported iff the program holds the button, whatever the
///   modifiers (Shift and Cmd are not encoded); the button is no longer
///   held either way;
/// - motion: reported when neither Shift nor Cmd is held, the position
///   differs from the last report's, and the mode reports it. A drag
///   carries the lowest button the program holds (no button when it holds
///   none: the press went to selection); motion with no button carries
///   none.
pub(crate) fn route_mouse(state: ReportState, event: &MouseEvent, mode: &Mode) -> Route {
    if mode.tracking == MouseTracking::None {
        return Route::Ignore {
            next: ReportState::default(),
        };
    }
    let cell = (event.col, event.row);
    let bypass = event
        .modifiers
        .intersects(Modifiers::SHIFT | Modifiers::SUPER);
    // Neither Shift nor Cmd is ever reported.
    let modifiers = event.modifiers - (Modifiers::SHIFT | Modifiers::SUPER);
    let ignore = Route::Ignore { next: state };
    match event.kind {
        MouseEventKind::Press => {
            let bit = held_bit(event.button);
            if bypass || bit == 0 {
                return ignore;
            }
            let (report, position) = mode.report(
                MouseReportKind::Press,
                report_button(event.button),
                cell,
                modifiers,
            );
            report_with(&report, mode, state, |next| ReportState {
                held: next.held | bit,
                baseline: Some(position),
            })
        }
        MouseEventKind::Release => {
            let bit = held_bit(event.button);
            if bit == 0 || state.held & bit == 0 {
                return ignore;
            }
            let released = ReportState {
                held: state.held & !bit,
                ..state
            };
            let (report, position) = mode.report(
                MouseReportKind::Release,
                report_button(event.button),
                cell,
                modifiers,
            );
            match encode_mouse(&report, mode.tracking, mode.encoding) {
                Some(bytes) => Route::Report {
                    bytes,
                    next: ReportState {
                        baseline: Some(position),
                        ..released
                    },
                },
                None => Route::Ignore { next: released },
            }
        }
        MouseEventKind::Motion => {
            if bypass {
                return ignore;
            }
            // A drag reports the lowest button the program saw go down;
            // motion the frontend sends with no button is plain motion.
            let held = if event.button == MouseButton::None {
                MouseButton::None
            } else {
                state.lowest_held()
            };
            let button = report_button(held);
            let (report, position) = mode.report(MouseReportKind::Motion, button, cell, modifiers);
            if state.baseline == Some(position) {
                return ignore;
            }
            report_with(&report, mode, state, |next| ReportState {
                baseline: Some(position),
                ..next
            })
        }
    }
}

/// The wheel turned `lines` whole lines (positive: up, towards older
/// output) over `cell` with `modifiers`, while the program tracks the
/// mouse: one wheel-up (64) or wheel-down (65) press per line, Shift and
/// Cmd never encoded. Nothing for 0 lines, in X10 mode (which has no
/// wheel) or beyond what the encoding can express.
pub(crate) fn route_wheel(
    state: ReportState,
    lines: i32,
    cell: (u16, u16),
    modifiers: Modifiers,
    mode: &Mode,
) -> Route {
    let button = if lines > 0 {
        MouseReportButton::WheelUp
    } else {
        MouseReportButton::WheelDown
    };
    let modifiers = modifiers - (Modifiers::SHIFT | Modifiers::SUPER);
    let (report, position) = mode.report(MouseReportKind::Press, button, cell, modifiers);
    let count = usize::try_from(lines.unsigned_abs()).unwrap_or(0);
    match encode_mouse(&report, mode.tracking, mode.encoding) {
        Some(one) if count > 0 => Route::Report {
            bytes: one.repeat(count),
            next: ReportState {
                baseline: Some(position),
                ..state
            },
        },
        _ => Route::Ignore { next: state },
    }
}

/// [`Route::Report`] with the state `update` makes of `state` when the
/// mode reports `report`, else [`Route::Ignore`] with `state`.
fn report_with(
    report: &MouseReport,
    mode: &Mode,
    state: ReportState,
    update: impl FnOnce(ReportState) -> ReportState,
) -> Route {
    match encode_mouse(report, mode.tracking, mode.encoding) {
        Some(bytes) => Route::Report {
            bytes,
            next: update(state),
        },
        None => Route::Ignore { next: state },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(tracking: MouseTracking) -> Mode {
        Mode {
            tracking,
            encoding: MouseEncoding::Sgr,
            pixels: PixelPoint::default(),
        }
    }

    fn event(kind: MouseEventKind, button: MouseButton, col: u16) -> MouseEvent {
        MouseEvent::new(kind, button, col, 0)
    }

    /// Routes `event` from `state`, keeps the state it leads to and
    /// returns the bytes.
    fn step(state: &mut ReportState, event: &MouseEvent, mode: &Mode) -> Option<Vec<u8>> {
        match route_mouse(*state, event, mode) {
            Route::Report { bytes, next } => {
                *state = next;
                Some(bytes)
            }
            Route::Ignore { next } => {
                *state = next;
                None
            }
        }
    }

    #[test]
    fn held_buttons_follow_reported_presses_and_releases() {
        let button = mode(MouseTracking::Button);
        let mut state = ReportState::default();
        let press = event(MouseEventKind::Press, MouseButton::Right, 0);
        assert!(step(&mut state, &press, &button).is_some());
        assert!(state.holds(MouseButton::Right));
        assert_eq!(state.lowest_held(), MouseButton::Right);
        let press = event(MouseEventKind::Press, MouseButton::Middle, 0);
        assert!(step(&mut state, &press, &button).is_some());
        assert_eq!(state.lowest_held(), MouseButton::Middle);
        let release = event(MouseEventKind::Release, MouseButton::Middle, 0);
        assert!(step(&mut state, &release, &button).is_some());
        assert!(!state.holds(MouseButton::Middle));
        assert_eq!(state.lowest_held(), MouseButton::Right);
        // Tracking off forgets everything.
        let off = mode(MouseTracking::None);
        let motion = event(MouseEventKind::Motion, MouseButton::Right, 4);
        assert_eq!(step(&mut state, &motion, &off), None);
        assert_eq!(state, ReportState::default());
    }

    #[test]
    fn an_x10_release_forgets_the_button_without_a_report() {
        let mode = mode(MouseTracking::X10);
        let mut state = ReportState::default();
        let press = event(MouseEventKind::Press, MouseButton::Left, 0);
        assert!(step(&mut state, &press, &mode).is_some());
        assert!(state.holds(MouseButton::Left));
        let release = event(MouseEventKind::Release, MouseButton::Left, 0);
        assert_eq!(step(&mut state, &release, &mode), None);
        assert!(!state.holds(MouseButton::Left));
    }

    #[test]
    fn the_wheel_sends_one_press_per_line_and_nothing_in_x10() {
        let state = ReportState::default();
        let normal = mode(MouseTracking::Normal);
        let Route::Report { bytes, next } =
            route_wheel(state, -2, (1, 0), Modifiers::CTRL, &normal)
        else {
            panic!("not reported");
        };
        assert_eq!(bytes, b"\x1b[<81;2;1M\x1b[<81;2;1M");
        assert_eq!(next.held, 0);
        let x10 = mode(MouseTracking::X10);
        assert_eq!(
            route_wheel(state, 3, (0, 0), Modifiers::NONE, &x10),
            Route::Ignore { next: state }
        );
        assert_eq!(
            route_wheel(state, 0, (0, 0), Modifiers::NONE, &normal),
            Route::Ignore { next: state }
        );
    }
}
