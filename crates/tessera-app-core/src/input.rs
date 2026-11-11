//! Input from the frontend: keys, mouse, scroll, paste and focus.
//!
//! The frontend turns its native events into [`InputEvent`]s and hands them
//! to [`App::send_input`](crate::App::send_input), which decides what each
//! one means: a key the key map binds ([`crate::command::Keymap`], ADR
//! 0036) to text writes it, to `none` is swallowed, and to a command is
//! reported to the frontend, which runs it
//! ([`App::run_key_binding`](crate::App::run_key_binding)); any other key
//! is encoded for the session's child and queued for its PTY. The returned
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
//!   the session's last mouse position. While the program tracks the mouse
//!   the event is reported to it ([`tessera_term::encode_mouse`]) and is
//!   [`InputOutcome::Consumed`]; Shift bypasses reporting and Cmd is never
//!   reported, so the frontend selects or opens a link when the outcome is
//!   [`InputOutcome::Ignored`] (ADR 0027, and
//!   [`App::send_input`](crate::App::send_input) for every rule).
//! * **Scroll**: while the program tracks the mouse (not in X10 mode),
//!   one wheel report per whole line, on either screen, unless Shift (the
//!   scrollback or cursor keys below) or Cmd (nothing) is held. Otherwise,
//!   on the primary screen, moves the session's scrollback view (see
//!   [`App::scroll_offset`](crate::App::scroll_offset)), accumulating
//!   fractional lines. On the alternate screen, which has no scrollback,
//!   sends one cursor key per whole line instead, Up for older output and
//!   Down for newer, in the form DECCKM selects (xterm's alternate
//!   scroll).
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

use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tessera_config::Config;

use crate::command::{ASCII_FILTER_WORDS, Binding, CommandId, Keymap, may_match};
use crate::session::lock;

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
    /// Pointer position in physical pixels from the left edge of the view
    /// (padding included); may be negative or beyond the view during a
    /// drag. Kept as given; SGR-pixels reports (DECSET 1016) take the
    /// view's padding off and clamp it to the text area.
    pub x_px: f32,
    /// Pointer position in physical pixels from the top edge of the view;
    /// see [`MouseEvent::x_px`].
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
    /// Horizontal distance in pixels as the frontend measured it. Not used:
    /// the scrollback view is vertical, and horizontal wheel reports
    /// (buttons 66 and 67) wait for this field to have a defined sign and
    /// unit (ADR 0027).
    pub delta_x_px: f32,
    /// Vertical distance in pixels as the frontend measured it. Not used;
    /// [`ScrollEvent::delta_lines`] drives the view and wheel reports.
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
    /// The key is bound to this command (`[keys]`, or a default chord);
    /// the frontend runs it. Nothing was sent to the PTY.
    Command(CommandId),
}

/// The compiled key map, shared by the [`App`](crate::App) and the
/// configuration watch, which replaces it when a configuration with other
/// `[keys]` is applied.
#[derive(Debug)]
pub(crate) struct Bindings {
    map: Mutex<Arc<Keymap>>,
    /// [`Keymap::modifier_mask`] of `map`, read without the lock so a key
    /// with modifiers no chord has (any plain or Ctrl key, by default)
    /// costs one atomic load.
    mask: AtomicU16,
    /// [`Keymap::ascii_filter`] of `map`, read without the lock so an
    /// unbound Command letter (⌘J) costs a few atomic loads. Stored word
    /// by word after the map: a key pressed while a configuration applies
    /// may meet either map's filter, as it may either map.
    ascii: [AtomicU64; ASCII_FILTER_WORDS],
    /// Counts the configurations applied ([`App::config_generation`](crate::App::config_generation)).
    generation: AtomicU64,
}

impl Bindings {
    /// The key map of `config`, at generation 0.
    pub(crate) fn new(config: &Config) -> Self {
        let map = Keymap::compile(&config.keys);
        Self {
            mask: AtomicU16::new(map.modifier_mask()),
            ascii: map.ascii_filter().map(AtomicU64::new),
            map: Mutex::new(Arc::new(map)),
            generation: AtomicU64::new(0),
        }
    }

    /// The key map in effect.
    pub(crate) fn keymap(&self) -> Arc<Keymap> {
        Arc::clone(&lock(&self.map))
    }

    /// The key map in effect and the generation of the configuration it
    /// came from, read under one lock: [`Bindings::applied`] replaces the
    /// map and counts the generation under the same lock, so the two
    /// always belong together (#317).
    pub(crate) fn keymap_and_generation(&self) -> (Arc<Keymap>, u64) {
        let guard = lock(&self.map);
        let map = Arc::clone(&guard);
        #[cfg(test)]
        tests::between_reads();
        (map, self.generation.load(Ordering::Relaxed))
    }

    /// Runs `with` on the binding of `event`, if any, and the keymap it
    /// is in; one atomic load when no chord has the event's modifiers, a
    /// few when no chord has its character key with them ([`may_match`]).
    /// The lock is held only to take the map: `with` runs on that map
    /// after it is released (#317), so it may queue `send_text` bytes
    /// (the PTY and outbox locks), read the map again or apply a
    /// configuration without waiting on a key press, or a key press on
    /// it. A configuration applied meanwhile replaces the map for the
    /// next key; this one finishes with the map it resolved in.
    pub(crate) fn with_binding<R>(
        &self,
        event: &KeyEvent,
        with: impl FnOnce(&Keymap, Binding<'_>) -> R,
    ) -> Option<R> {
        let mask = self.mask.load(Ordering::Relaxed);
        if !may_match(event, mask, |word| self.ascii[word].load(Ordering::Relaxed)) {
            return None;
        }
        // The guard is a temporary: dropped at the end of this statement.
        let map = Arc::clone(&lock(&self.map));
        map.resolve(event).map(|binding| with(&map, binding))
    }

    /// Records that `config` was applied: compiles its `[keys]` again when
    /// they changed, and returns the new generation.
    pub(crate) fn applied(&self, config: &Config) -> u64 {
        let mut map = lock(&self.map);
        if map.keys() != config.keys.bindings() {
            let compiled = Keymap::compile(&config.keys);
            self.mask.store(compiled.modifier_mask(), Ordering::Relaxed);
            for (word, bits) in self.ascii.iter().zip(compiled.ascii_filter()) {
                word.store(bits, Ordering::Relaxed);
            }
            *map = Arc::new(compiled);
        }
        // Under the map lock, so generations and maps go up together.
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The number of configurations applied since the app started.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

/// The cursor keys the wheel sends on the alternate screen for `lines`
/// whole lines (positive: towards older output, the wheel turned up): that
/// many Up keys, or `-lines` Down keys, `CSI A` / `CSI B`, or `SS3 A` /
/// `SS3 B` when `application_cursor_keys` (DECCKM) is set, each in its
/// 8-bit form (`0x9B A`, `0x8F A`) when `eight_bit` (S8C1T). Empty for 0.
///
/// These are the unmodified legacy cursor keys whatever keyboard protocol
/// the application turned on, as xterm sends for alternate scroll.
pub(crate) fn alternate_scroll_keys(
    lines: i32,
    application_cursor_keys: bool,
    eight_bit: bool,
) -> Vec<u8> {
    let key: &[u8] = match (lines > 0, application_cursor_keys) {
        (true, false) => b"\x1b[A",
        (true, true) => b"\x1bOA",
        (false, false) => b"\x1b[B",
        (false, true) => b"\x1bOB",
    };
    let mut key = key.to_vec();
    if eight_bit {
        tessera_term::to_8bit_controls(&mut key);
    }
    key.repeat(usize::try_from(lines.unsigned_abs()).unwrap_or(0))
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
    use crate::command::admits;

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
    fn alternate_scroll_sends_one_cursor_key_per_line_in_the_decckm_form() {
        assert_eq!(alternate_scroll_keys(0, false, false), b"");
        assert_eq!(alternate_scroll_keys(0, true, false), b"");
        assert_eq!(alternate_scroll_keys(1, false, false), b"\x1b[A");
        assert_eq!(
            alternate_scroll_keys(3, false, false),
            b"\x1b[A\x1b[A\x1b[A"
        );
        assert_eq!(alternate_scroll_keys(-2, false, false), b"\x1b[B\x1b[B");
        assert_eq!(alternate_scroll_keys(2, true, false), b"\x1bOA\x1bOA");
        assert_eq!(alternate_scroll_keys(-1, true, false), b"\x1bOB");
    }

    /// In 8-bit mode (S8C1T) every key of the run is 8-bit, not only the
    /// first (#96).
    #[test]
    fn alternate_scroll_keys_are_8bit_in_8bit_mode() {
        assert_eq!(alternate_scroll_keys(0, false, true), b"");
        assert_eq!(alternate_scroll_keys(3, false, true), b"\x9bA\x9bA\x9bA");
        assert_eq!(alternate_scroll_keys(-2, false, true), b"\x9bB\x9bB");
        assert_eq!(alternate_scroll_keys(2, true, true), b"\x8fA\x8fA");
        assert_eq!(alternate_scroll_keys(-1, true, true), b"\x8fB");
    }

    #[test]
    fn a_plain_key_skips_the_lock_and_cmd_t_is_new_tab() {
        let bindings = Bindings::new(&Config::default());
        let plain = KeyEvent::new(Key::Char('t'), Modifiers::NONE);
        assert!(!admits(
            bindings.mask.load(Ordering::Relaxed),
            plain.modifiers
        ));
        assert_eq!(bindings.with_binding(&plain, |_, b| format!("{b:?}")), None);
        let cmd_t = KeyEvent::new(Key::Char('t'), Modifiers::SUPER);
        let id = CommandId::find("new_tab", None).unwrap();
        assert_eq!(
            bindings.with_binding(&cmd_t, |_, b| b == Binding::Command(id)),
            Some(true)
        );
    }

    /// #317: `with` runs after the map lock is released, so a binding
    /// whose work reads the map again or applies a configuration (as
    /// `send_text` takes the PTY and outbox locks) does not deadlock. On
    /// another thread with a deadline: a held lock would hang it for good.
    #[test]
    fn a_binding_that_reenters_the_map_does_not_deadlock() {
        let bindings = Arc::new(Bindings::new(&Config::default()));
        let loaded = Config::from_toml_lenient("[keys]\n'cmd+u' = 'send_text:x'\n").unwrap();
        bindings.applied(&loaded.config);
        let (done, finished) = std::sync::mpsc::channel();
        let worker = Arc::clone(&bindings);
        std::thread::spawn(move || {
            let cmd_u = KeyEvent::new(Key::Char('u'), Modifiers::SUPER);
            let outcome = worker.with_binding(&cmd_u, |map, binding| {
                let Binding::SendText(bytes) = binding else {
                    return None;
                };
                // Re-entered: the map, the map with its generation, and a
                // new configuration, each of which takes the map's lock.
                let again = worker.keymap();
                let (_, generation) = worker.keymap_and_generation();
                let applied = worker.applied(&Config::default());
                Some((
                    bytes.to_vec(),
                    std::ptr::eq(map, Arc::as_ptr(&again)),
                    generation,
                    applied,
                ))
            });
            let _ = done.send(outcome);
        });
        let outcome = finished
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("the binding re-entered the map's lock and hung");
        assert_eq!(outcome, Some(Some((b"x".to_vec(), true, 1, 2))));
        // The map in effect is the one applied from inside the binding.
        let cmd_u = KeyEvent::new(Key::Char('u'), Modifiers::SUPER);
        assert_eq!(bindings.with_binding(&cmd_u, |_, _| ()), None);
    }

    /// The map and its generation come from the same apply.
    #[test]
    fn the_map_and_its_generation_are_read_together() {
        let bindings = Bindings::new(&Config::default());
        let (map, generation) = bindings.keymap_and_generation();
        assert_eq!(generation, 0);
        assert!(Arc::ptr_eq(&map, &bindings.keymap()));
        let loaded = Config::from_toml_lenient("[keys]\n'cmd+u' = 'new_tab'\n").unwrap();
        assert_eq!(bindings.applied(&loaded.config), 1);
        let (map, generation) = bindings.keymap_and_generation();
        assert_eq!(generation, 1);
        let new_tab = CommandId::find("new_tab", None).unwrap();
        assert_eq!(map.chords(new_tab).len(), 2, "cmd+u and cmd+t");
    }

    thread_local! {
        /// Run once by [`Bindings::keymap_and_generation`] on this thread,
        /// after it took the map and before it reads the generation.
        static BETWEEN_READS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The pause point of [`Bindings::keymap_and_generation`].
    pub(super) fn between_reads() {
        if let Some(hook) = BETWEEN_READS.with(|cell| cell.borrow_mut().take()) {
            hook();
        }
    }

    /// M4 (#317), staged: an apply started between taking the map and
    /// reading the generation cannot finish there (it waits for the lock),
    /// so the pair is the old map with the old generation. Read with the
    /// lock released, it was the old map with the new generation.
    #[test]
    fn an_apply_cannot_land_between_the_map_and_its_generation() {
        let bindings = Arc::new(Bindings::new(&Config::default()));
        let (start, started) = std::sync::mpsc::channel::<()>();
        let (finish, finished) = std::sync::mpsc::channel::<u64>();
        let applier = {
            let bindings = Arc::clone(&bindings);
            std::thread::spawn(move || {
                let bound = Config::from_toml_lenient("[keys]\n'cmd+u' = 'new_tab'\n")
                    .unwrap()
                    .config;
                started.recv().unwrap();
                let generation = bindings.applied(&bound);
                // The reader has stopped listening when the apply waited
                // for the lock, as it should; nothing to report then.
                let _ = finish.send(generation);
            })
        };
        let landed = Arc::new(Mutex::new(None));
        let hook_landed = Arc::clone(&landed);
        BETWEEN_READS.with(|cell| {
            *cell.borrow_mut() = Some(Box::new(move || {
                start.send(()).unwrap();
                // Long enough for an unblocked apply even under load; a
                // blocked one never finishes here.
                let outcome = finished.recv_timeout(std::time::Duration::from_millis(500));
                *hook_landed.lock().unwrap() = outcome.ok();
            }));
        });
        let (map, generation) = bindings.keymap_and_generation();
        let new_tab = CommandId::find("new_tab", None).unwrap();
        assert_eq!(
            (generation, map.chords(new_tab).len()),
            (0, 1),
            "an apply landed between the reads: {:?}",
            landed.lock().unwrap()
        );
        applier.join().unwrap();
        assert_eq!(bindings.keymap_and_generation().0.chords(new_tab).len(), 2);
    }

    /// M4 (#317) at the source: 20,000 applies alternating between two
    /// `[keys]` tables against a tight loop of reads; every map is the
    /// one of its generation. (Unstaged, this rarely reaches the few
    /// nanoseconds between the reads that a torn version left open; the
    /// staged test above is the one that catches it.) Halfway, the applier
    /// waits until a read has started after that apply, so the reads
    /// overlap the applies however the threads are scheduled (#386: a
    /// stalled reader used to wake after the last apply).
    #[test]
    fn the_map_and_its_generation_never_tear_under_applies() {
        const APPLIES: u64 = 20_000;
        const HALFWAY: u64 = APPLIES / 2;
        let bindings = Arc::new(Bindings::new(&Config::default()));
        let bound = Config::from_toml_lenient("[keys]\n'cmd+u' = 'new_tab'\n")
            .unwrap()
            .config;
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The reads finished so far.
        let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let applier = {
            let bindings = Arc::clone(&bindings);
            let done = Arc::clone(&done);
            let reads = Arc::clone(&reads);
            std::thread::spawn(move || {
                let defaults = Config::default();
                for generation in 1..=APPLIES {
                    let config = if generation % 2 == 1 {
                        &bound
                    } else {
                        &defaults
                    };
                    assert_eq!(bindings.applied(config), generation);
                    // No apply until a read that started after this one
                    // finishes: two more finished reads, as one may have
                    // been under way. It counts reads, not the generation
                    // they report, so a torn read fails the test rather
                    // than hanging it; the reader loops until `done`.
                    if generation == HALFWAY {
                        let before = reads.load(Ordering::Acquire);
                        while reads.load(Ordering::Acquire) < before + 2 {
                            std::thread::yield_now();
                        }
                    }
                }
                done.store(true, Ordering::Release);
            })
        };
        let new_tab = CommandId::find("new_tab", None).unwrap();
        let mut torn = 0_u64;
        // An applier that panics never sets `done`: `join` reports it.
        while !done.load(Ordering::Acquire) && !applier.is_finished() {
            let (map, generation) = bindings.keymap_and_generation();
            let expected = if generation % 2 == 1 { 2 } else { 1 };
            torn += u64::from(map.chords(new_tab).len() != expected);
            reads.fetch_add(1, Ordering::Release);
        }
        applier.join().unwrap();
        let reads = reads.load(Ordering::Relaxed);
        assert_eq!(torn, 0, "{torn} torn reads in {reads}");
    }

    /// An unbound Command letter stops at the ASCII filter; a configuration
    /// that binds it updates the filter with the map.
    #[test]
    fn an_unbound_command_letter_skips_the_lock_until_a_config_binds_it() {
        let bindings = Bindings::new(&Config::default());
        let filter = |bindings: &Bindings, event: &KeyEvent| {
            may_match(event, bindings.mask.load(Ordering::Relaxed), |word| {
                bindings.ascii[word].load(Ordering::Relaxed)
            })
        };
        let cmd_j = KeyEvent::new(Key::Char('j'), Modifiers::SUPER);
        assert!(admits(
            bindings.mask.load(Ordering::Relaxed),
            cmd_j.modifiers
        ));
        assert!(!filter(&bindings, &cmd_j), "⌘J is no default chord");
        assert!(filter(
            &bindings,
            &KeyEvent::new(Key::Char('T'), Modifiers::SUPER)
        ));
        let loaded = Config::from_toml_lenient("[keys]\n'cmd+j' = 'new_tab'\n").unwrap();
        bindings.applied(&loaded.config);
        assert!(filter(&bindings, &cmd_j));
        let id = CommandId::find("new_tab", None).unwrap();
        assert_eq!(
            bindings.with_binding(&cmd_j, |_, b| b == Binding::Command(id)),
            Some(true)
        );
    }
}
