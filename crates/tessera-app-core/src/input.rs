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
//! * **Key**: encoded with [`tessera_term::encode_key`] in the session
//!   terminal's own keyboard modes
//!   ([`Terminal::keyboard_modes`](tessera_term::Terminal::keyboard_modes):
//!   DECCKM, DECKPAM, `modifyOtherKeys`, kitty flags, copied under the
//!   terminal lock once per key) and queued for the session's writer thread
//!   without blocking.
//! * **Mouse**: the position is clamped to the session's grid and kept as
//!   the session's last mouse position; nothing is written. Mouse reporting
//!   (#12) and selection (#30) build on it.
//! * **Scroll**: moves the session's scrollback view (see
//!   [`App::scroll_offset`](crate::App::scroll_offset)), accumulating
//!   fractional lines.
//! * **Paste**: sanitised with [`sanitize_paste_into`] straight into the
//!   buffer that is queued, like a key, in order with the keys around it.
//!   In bracketed-paste mode (DECSET 2004) it is wrapped in `ESC [ 200 ~`
//!   and `ESC [ 201 ~` and sanitised with [`sanitize_bracketed_paste_into`]
//!   instead, which keeps the line breaks as they are. Its room is checked
//!   on the raw length first, so a paste that cannot fit is refused before
//!   anything is copied. A paste that sanitises to nothing writes nothing,
//!   brackets included.
//! * **Focus**: recorded (see [`App::is_focused`](crate::App::is_focused));
//!   in focus-event mode (DECSET 1004) a change is reported as `ESC [ I`
//!   (gained) or `ESC [ O` (lost). An event that repeats the current state
//!   reports nothing, and setting the mode does not report the current
//!   state (xterm).

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
/// [`sanitize_paste_into`] does the same without allocating a new string.
#[must_use]
pub fn sanitize_paste(text: &str) -> String {
    let mut out = Vec::with_capacity(text.len());
    sanitize_paste_into(text, &mut out);
    // Only whole characters and ASCII line breaks are copied, so the
    // result is UTF-8; checking it costs one pass, never a panic.
    String::from_utf8(out)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

/// Appends `text`, sanitised as [`sanitize_paste`] describes, to `out` in
/// one pass.
///
/// Sanitising never lengthens text, so at most `text.len()` bytes are
/// appended: a caller that reserves that much copies the paste exactly
/// once. Runs of kept characters are copied as slices.
pub fn sanitize_paste_into(text: &str, out: &mut Vec<u8>) {
    sanitize_into(text, out, true);
}

/// Appends `text`, sanitised for a bracketed paste (DECSET 2004), to `out`
/// in one pass: as [`sanitize_paste_into`], except that line breaks
/// (`"\n"`, `"\r"`, `"\r\n"`) are kept as they are, as xterm, kitty,
/// Alacritty and `WezTerm` keep them inside the brackets, where the
/// application reads them as text rather than as Return. `ESC` is still
/// removed, so the text cannot end the paste early with its own
/// `ESC [ 201 ~` (what remains of one is the plain text `[201~`).
pub fn sanitize_bracketed_paste_into(text: &str, out: &mut Vec<u8>) {
    sanitize_into(text, out, false);
}

/// [`sanitize_paste_into`] (`normalize_breaks`: line breaks become `CR`)
/// or [`sanitize_bracketed_paste_into`] (line breaks are kept).
fn sanitize_into(text: &str, out: &mut Vec<u8>, normalize_breaks: bool) {
    let bytes = text.as_bytes();
    let mut run = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        if ch == '\t' || !ch.is_control() || (!normalize_breaks && matches!(ch, '\r' | '\n')) {
            continue;
        }
        out.extend_from_slice(&bytes[run..at]);
        match ch {
            '\r' => {
                chars.next_if(|&(_, next)| next == '\n');
                out.push(b'\r');
            }
            '\n' => out.push(b'\r'),
            _ => {}
        }
        run = chars.peek().map_or(bytes.len(), |&(next, _)| next);
    }
    out.extend_from_slice(&bytes[run..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bracketed(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        sanitize_bracketed_paste_into(text, &mut out);
        out
    }

    #[test]
    fn a_bracketed_paste_keeps_line_breaks_and_drops_other_controls() {
        assert_eq!(bracketed("a\nb\r\nc\rd\te"), b"a\nb\r\nc\rd\te");
        assert_eq!(
            bracketed("x\x1b[201~y\u{9b}201~z\x03\x7f"),
            b"x[201~y201~z",
            "the end marker cannot be forged"
        );
        assert_eq!(bracketed("\x1b\x07"), b"");
        let mut plain = Vec::new();
        sanitize_paste_into("a\nb\r\nc", &mut plain);
        assert_eq!(plain, b"a\rb\rc", "unbracketed pastes still send CR");
    }

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

    /// The character-by-character sanitiser `sanitize_paste` used to be;
    /// the slice-copying one must agree with it.
    fn reference_sanitize(text: &str) -> String {
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

    #[test]
    fn sanitising_into_a_buffer_matches_the_reference_on_every_short_string() {
        // Every string of up to five characters over an alphabet with each
        // kind of character: line breaks, tab, C0, DEL, C1, ASCII and
        // multi-byte text (9^5 + ... = 66 430 strings).
        const ALPHABET: [char; 9] = ['\r', '\n', '\t', '\x1b', '\x7f', '\u{85}', 'a', 'é', '🦀'];
        let mut text = String::new();
        let mut out = Vec::new();
        let mut indices = Vec::new();
        loop {
            text.clear();
            text.extend(indices.iter().map(|&i: &usize| ALPHABET[i]));
            out.clear();
            out.extend_from_slice(b"prefix");
            sanitize_paste_into(&text, &mut out);
            let expected = reference_sanitize(&text);
            assert_eq!(&out[..6], b"prefix", "{text:?}");
            assert_eq!(&out[6..], expected.as_bytes(), "{text:?}");
            assert!(out.len() - 6 <= text.len(), "{text:?} grew");
            assert_eq!(sanitize_paste(&text), expected, "{text:?}");
            // Next string in length-then-lexicographic order.
            let mut digit = 0;
            loop {
                if digit == indices.len() {
                    indices.push(0);
                    break;
                }
                indices[digit] += 1;
                if indices[digit] < ALPHABET.len() {
                    break;
                }
                indices[digit] = 0;
                digit += 1;
            }
            if indices.len() > 5 {
                break;
            }
        }
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
