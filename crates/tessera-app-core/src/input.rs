//! Input from the frontend: keys, mouse, scroll, paste and focus.
//!
//! The frontend turns its native events into [`InputEvent`]s and hands them
//! to [`App::send_input`](crate::App::send_input), which decides what each
//! one means: a key bound to an application command runs that command (no
//! key is bound until the configurable key map, #43); any other key is
//! encoded for the session's child and queued for its PTY. The returned
//! [`InputOutcome`] tells the frontend whether the core used the event, so
//! an event the terminal has no use for (a bare modifier, a key release in
//! legacy mode, `Cmd`+`Q`) can still go to the frontend's own handling,
//! such as menu key equivalents.
//!
//! What each event does today:
//!
//! * **Key**: encoded with [`tessera_term::encode_key`] and queued for the
//!   session's writer thread without blocking. Until the terminal tracks
//!   its keyboard modes (#8) the encoding uses
//!   [`KeyboardModes::default()`](tessera_term::KeyboardModes): normal
//!   cursor keys, numeric keypad, no `modifyOtherKeys`, no kitty flags.
//! * **Mouse**: the position is clamped to the session's grid and kept as
//!   the session's last mouse position; nothing is written. Mouse reporting
//!   (#12) and selection (#30) build on it.
//! * **Scroll**: moves the session's scrollback view (see
//!   [`App::scroll_offset`](crate::App::scroll_offset)), accumulating
//!   fractional lines.
//! * **Paste**: sanitised with [`sanitize_paste`] and queued like a key,
//!   in order with the keys around it.
//! * **Focus**: recorded (see [`App::is_focused`](crate::App::is_focused));
//!   nothing is written until focus reporting (DECSET 1004, #8) exists.

use std::convert::Infallible;

pub use tessera_term::{Key, KeyEvent, KeyEventKind, Modifiers};

/// One event from the frontend, routed by
/// [`App::send_input`](crate::App::send_input).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum InputEvent {
    /// A key was pressed, repeated or released.
    Key(KeyEvent),
    /// The mouse moved, or a button went down or up, over the session.
    Mouse(MouseEvent),
    /// The scroll wheel or trackpad scrolled over the session.
    Scroll(ScrollEvent),
    /// The user pasted text into the session.
    Paste(String),
    /// The session's view gained or lost keyboard focus.
    Focus(FocusEvent),
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MouseButton {
    /// No button: plain motion with every button up.
    #[default]
    None,
    /// The primary (left) button.
    Left,
    /// The middle button (or wheel click).
    Middle,
    /// The secondary (right) button.
    Right,
    /// The "back" side button (button 4 in X11 numbering after the wheel).
    Back,
    /// The "forward" side button.
    Forward,
}

/// What happened to the mouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum MouseEventKind {
    /// A button went down.
    #[default]
    Press,
    /// A button went up.
    Release,
    /// The pointer moved, with [`MouseEvent::button`] held (a drag) or
    /// [`MouseButton::None`].
    Motion,
}

/// A mouse event over a session's view.
///
/// Built with [`MouseEvent::new`] and the `with_*` methods, so fields can be
/// added without breaking frontends.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub struct MouseEvent {
    /// Press, release or motion.
    pub kind: MouseEventKind,
    /// The button that changed (press, release) or is held (motion).
    pub button: MouseButton,
    /// Zero-based column of the cell under the pointer. Clamped to the
    /// session's grid when the event is handled.
    pub col: u16,
    /// Zero-based row of the cell under the pointer, counted from the top of
    /// the visible screen. Clamped like [`MouseEvent::col`].
    pub row: u16,
    /// Pointer position in pixels from the left edge of the view; may be
    /// negative or beyond the view during a drag. Kept as given.
    pub x_px: f32,
    /// Pointer position in pixels from the top edge of the view; kept as
    /// given.
    pub y_px: f32,
    /// Modifiers held during the event.
    pub modifiers: Modifiers,
}

/// A scroll gesture over a session's view.
///
/// Built with [`ScrollEvent::new`] and the `with_*` methods, so fields can
/// be added without breaking frontends.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub struct ScrollEvent {
    /// Distance in lines. Positive moves the view towards older output
    /// (into the scrollback); fractions accumulate across events. A value
    /// that is not finite is ignored.
    pub delta_lines: f32,
    /// Horizontal distance in pixels as the frontend measured it, for
    /// pixel-precise consumers (mouse reporting, #12). Not used for the
    /// scrollback view.
    pub delta_x_px: f32,
    /// Vertical distance in pixels as the frontend measured it; see
    /// [`ScrollEvent::delta_x_px`].
    pub delta_y_px: f32,
    /// Whether the deltas come from a precise device (trackpad, Magic
    /// Mouse) rather than a notched wheel.
    pub precise: bool,
    /// Zero-based column of the cell under the pointer.
    pub col: u16,
    /// Zero-based row of the cell under the pointer.
    pub row: u16,
    /// Modifiers held during the gesture.
    pub modifiers: Modifiers,
}

impl MouseEvent {
    /// A `kind` event of `button` over cell (`col`, `row`), at pixel
    /// (0, 0) with no modifiers.
    #[must_use]
    pub fn new(kind: MouseEventKind, button: MouseButton, col: u16, row: u16) -> Self {
        Self {
            kind,
            button,
            col,
            row,
            ..Self::default()
        }
    }

    /// Sets the pointer position in pixels.
    #[must_use]
    pub fn with_pixels(mut self, x_px: f32, y_px: f32) -> Self {
        self.x_px = x_px;
        self.y_px = y_px;
        self
    }

    /// Sets the modifiers.
    #[must_use]
    pub fn with_modifiers(mut self, modifiers: Modifiers) -> Self {
        self.modifiers = modifiers;
        self
    }
}

impl ScrollEvent {
    /// A notched-wheel scroll of `delta_lines` over cell (0, 0), with no
    /// pixel deltas and no modifiers.
    #[must_use]
    pub fn new(delta_lines: f32) -> Self {
        Self {
            delta_lines,
            ..Self::default()
        }
    }

    /// Sets the pixel deltas ([`ScrollEvent::delta_x_px`],
    /// [`ScrollEvent::delta_y_px`]).
    #[must_use]
    pub fn with_pixels(mut self, horizontal: f32, vertical: f32) -> Self {
        self.delta_x_px = horizontal;
        self.delta_y_px = vertical;
        self
    }

    /// Marks the deltas as coming from a precise device (or not).
    #[must_use]
    pub fn with_precise(mut self, precise: bool) -> Self {
        self.precise = precise;
        self
    }

    /// Sets the cell under the pointer.
    #[must_use]
    pub fn with_cell(mut self, col: u16, row: u16) -> Self {
        self.col = col;
        self.row = row;
        self
    }

    /// Sets the modifiers.
    #[must_use]
    pub fn with_modifiers(mut self, modifiers: Modifiers) -> Self {
        self.modifiers = modifiers;
        self
    }
}

/// A change of keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusEvent {
    /// The session's view became the key view.
    Gained,
    /// The session's view stopped being the key view.
    Lost,
}

/// Whether the core used an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub enum InputOutcome {
    /// The core acted on the event (wrote to the PTY, ran a binding, moved
    /// the view, recorded focus); the frontend should not handle it again.
    Consumed,
    /// The core had no use for the event; the frontend may handle it
    /// itself (for example as a menu key equivalent).
    Ignored,
}

/// An application command a key can be bound to.
///
/// Has no variants yet: the command registry and the configurable key map
/// (#43) do not exist, so no key is ever bound. The type keeps the routing
/// in [`App::send_input`](crate::App::send_input) in place for them.
pub(crate) type Binding = Infallible;

/// Resolves key events to application commands before they reach the PTY.
#[derive(Debug, Default)]
pub(crate) struct Bindings {}

impl Bindings {
    /// The command bound to `event`, if any. Always `None` until #43 adds
    /// configurable bindings.
    #[expect(
        clippy::unused_self,
        reason = "the key map will live in `self` once #43 adds it"
    )]
    pub(crate) fn resolve(&self, event: &KeyEvent) -> Option<Binding> {
        let _ = event;
        None
    }
}

/// Makes pasted text safe to write to a PTY.
///
/// * Line breaks become carriage returns, which is what the Return key
///   sends: `"\r\n"` and `"\n"` each become `"\r"`, and a lone `"\r"` stays.
/// * Every other C0 control except tab (`0x00`..=`0x1f`, including `ESC`),
///   `DEL` (`0x7f`) and every C1 control (`U+0080`..=`U+009F`) is removed, so
///   pasted text cannot carry escape sequences, signals (`^C`, `^Z`), line
///   editing (`^U`, `^W`, `DEL`) or an early end of a bracketed paste.
///
/// Everything else, including all printable Unicode, is kept unchanged.
pub fn sanitize_paste(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\r');
            }
            '\n' => out.push('\r'),
            '\t' => out.push('\t'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_maps_line_breaks_to_carriage_returns() {
        assert_eq!(sanitize_paste("a\nb\r\nc\rd"), "a\rb\rc\rd");
        assert_eq!(sanitize_paste("\r\n\r\n"), "\r\r");
        assert_eq!(sanitize_paste("\n\r"), "\r\r");
    }

    #[test]
    fn paste_strips_controls_but_keeps_tab_and_text() {
        assert_eq!(sanitize_paste("a\nb\x1b[31mc"), "a\rb[31mc");
        assert_eq!(sanitize_paste("x\ty\u{3}\u{1a}\u{7f}z"), "x\tyz");
        assert_eq!(sanitize_paste("\u{80}\u{9b}31m\u{9f}"), "31m");
        assert_eq!(sanitize_paste("\u{a0}é漢字🦀"), "\u{a0}é漢字🦀");
        assert_eq!(sanitize_paste("\x1b[201~evil"), "[201~evil");
    }

    #[test]
    fn paste_keeps_nothing_from_pure_controls() {
        assert_eq!(sanitize_paste(""), "");
        assert_eq!(sanitize_paste("\x00\x1b\x07\u{85}"), "");
    }

    #[test]
    fn nothing_is_bound_yet() {
        let bindings = Bindings::default();
        let event = KeyEvent::new(Key::Char('q'), Modifiers::SUPER);
        assert!(bindings.resolve(&event).is_none());
    }
}
