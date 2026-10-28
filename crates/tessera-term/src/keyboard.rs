//! Keyboard input encoding.
//!
//! Turns a frontend-neutral [`KeyEvent`] into the bytes a terminal writes to
//! the PTY, honouring the classic xterm encodings (DECCKM cursor-key mode,
//! DECKPAM application keypad, `modifyOtherKeys`) and the
//! [kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
//! with all five progressive-enhancement flags.
//!
//! The module is self-contained: the mode state it needs is carried in
//! [`KeyboardModes`], which the terminal mode machinery (DECSET 1 / 66,
//! `CSI > 4 ; n m`, `CSI = flags ; mode u`) is expected to keep up to date.
//! Nothing here depends on the parser or the grid.
//!
//! # Encoding precedence
//!
//! 1. If any kitty flag is set, the kitty protocol rules apply.
//! 2. Otherwise, if `modifyOtherKeys` is at level 1 or 2 and the combination
//!    qualifies, xterm's `CSI 27 ; modifiers ; code ~` form is used.
//! 3. Otherwise the legacy encoding is used, which is what xterm produces by
//!    default and what every terminfo entry describes.
//!
//! # Frontend contract
//!
//! The frontend supplies the *unshifted* key ([`Key::Char`] holds the lower
//! case / base character of the key), the modifier state, and, when the OS
//! has already composed text for the event (keyboard layout, dead keys, IME),
//! that text in [`KeyEvent::text`]. Whether the macOS Option key counts as
//! `alt` or as a composition key is a frontend policy: pass `ALT` to get an
//! `ESC`-prefixed byte, or pass the composed text with no `ALT` to send it
//! verbatim.
//!
//! Control code points are never text: a [`Key::Char`] holding one, or a
//! [`KeyEvent::text`] containing one, encodes to nothing rather than injecting
//! raw C0 bytes, and a [`KeyEvent::shifted_key`] or
//! [`KeyEvent::base_layout_key`] holding one is ignored.
//!
//! When no text is supplied the module derives it from the key: the shifted
//! form under Shift (from [`KeyEvent::shifted_key`], else a US layout), with
//! Caps Lock inverting the case of letters. Frontends should still pass the
//! OS text whenever the event produced any; the derived text is a fallback
//! for synthetic events and non-composing frontends.
//!
//! # Where legacy mode follows xterm rather than kitty
//!
//! The legacy text-key algorithm is kitty's, with two deliberate differences
//! that make a plain readline session behave like xterm:
//!
//! * **Ctrl+Shift+letter** sends the C0 byte of the letter (`0x03` for
//!   Ctrl+Shift+C), as xterm, iTerm2 and Terminal.app do. kitty reports it as
//!   `CSI u` instead, which prints `;6u` noise in programs that do not speak
//!   the protocol. Ctrl+Shift on a symbol key applies the ctrl table to the
//!   *shifted* character in both (`Ctrl+Shift+-` is `Ctrl+_`, `0x1f`).
//! * **Non-Latin layouts**: when Ctrl or Alt is held and the key is not an
//!   ASCII key, the [`KeyEvent::base_layout_key`] is used instead when it is
//!   one, so Ctrl+С on a Cyrillic layout still sends `0x03`. This matches
//!   kitty and what X11 keysym translation gives xterm.
//! * **Ctrl on `` ` ``, `{`, `|` and `}`** masks the byte with `0x1f`, as
//!   libX11's `XkbToControl` does for every byte in `@`..`~`: Ctrl+`` ` `` is
//!   `NUL`, Ctrl+Shift+`[` is `ESC`, Ctrl+Shift+`\` is `0x1c` and
//!   Ctrl+Shift+`]` is `0x1d`. The kitty table leaves these four untouched.
//!   With the mask the ctrl table covers all of `@`..`~`, so at
//!   `modifyOtherKeys` level 1 these combinations keep their legacy byte,
//!   exactly as xterm's `ModifyOtherKeys()` decides.
//! * **Shift on a keypad character key** is consumed by the key: Shift+KP 0
//!   sends `0`, never `)`. The keypad has no shifted symbols (kitty does the
//!   same; xterm sees a different keysym with Num Lock off instead).
//!
//! One place where legacy mode follows kitty rather than xterm:
//!
//! * **Keys without a terminfo entry** (media keys, Print Screen, ...) are
//!   sent as `CSI u` even in legacy mode. F13 through F35 are the exception:
//!   they have entries in the shipped terminfo (`kf13` through `kf35`), which
//!   describe xterm's PC-style aliases (F13 is Shift+F1, F25 is Ctrl+F1), so
//!   they are encoded as those aliases rather than as `CSI u`.
//!
//! A text-only event ([`Key::Unidentified`], as produced by an IME commit) is
//! always delivered as its text, except under *report all keys* together with
//! *report associated text*, where the spec's `CSI 0 ; ; text u` form carries
//! it. kitty's input layer hands IME commits straight to the child; this
//! module has no such side channel, so the text is sent verbatim rather than
//! dropped.

use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign, Sub};

const ESC: u8 = 0x1b;
const CSI: &[u8] = b"\x1b[";
const SS3: &[u8] = b"\x1bO";

/// Modifier keys active for a key event, as a bit set.
///
/// The bit values are those of the kitty keyboard protocol, which are a
/// superset of xterm's.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    /// No modifiers.
    pub const NONE: Self = Self(0);
    /// Shift.
    pub const SHIFT: Self = Self(1);
    /// Alt (Option on Apple keyboards).
    pub const ALT: Self = Self(2);
    /// Control.
    pub const CTRL: Self = Self(4);
    /// Super (Command on Apple keyboards, the Windows/Linux key elsewhere).
    pub const SUPER: Self = Self(8);
    /// Hyper (X11/Wayland with special XKB rules).
    pub const HYPER: Self = Self(16);
    /// Meta (X11/Wayland with special XKB rules).
    pub const META: Self = Self(32);
    /// Caps Lock is engaged.
    pub const CAPS_LOCK: Self = Self(64);
    /// Num Lock is engaged.
    pub const NUM_LOCK: Self = Self(128);

    /// The two lock modifiers, which legacy encodings cannot express.
    pub const LOCKS: Self = Self(Self::CAPS_LOCK.0 | Self::NUM_LOCK.0);

    /// Builds a modifier set from its raw bits.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The raw bits.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether no modifier is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every modifier in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any modifier in `other` is set in `self`.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// The set without the lock modifiers.
    #[must_use]
    pub const fn without_locks(self) -> Self {
        Self(self.0 & !Self::LOCKS.0)
    }

    /// The modifier parameter as used in escape sequences (`1 + bits`).
    fn param(self) -> u32 {
        u32::from(self.0) + 1
    }
}

impl BitOr for Modifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for Modifiers {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl Sub for Modifiers {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 & !rhs.0)
    }
}

impl fmt::Debug for Modifiers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMES: [(Modifiers, &str); 8] = [
            (Modifiers::SHIFT, "SHIFT"),
            (Modifiers::ALT, "ALT"),
            (Modifiers::CTRL, "CTRL"),
            (Modifiers::SUPER, "SUPER"),
            (Modifiers::HYPER, "HYPER"),
            (Modifiers::META, "META"),
            (Modifiers::CAPS_LOCK, "CAPS_LOCK"),
            (Modifiers::NUM_LOCK, "NUM_LOCK"),
        ];
        if self.is_empty() {
            return f.write_str("NONE");
        }
        let mut first = true;
        for (bit, name) in NAMES {
            if self.contains(bit) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// What happened to the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeyEventKind {
    /// The key went down.
    #[default]
    Press,
    /// The key is held and auto-repeating.
    Repeat,
    /// The key went up.
    Release,
}

/// A key, identified independently of the frontend's key codes.
///
/// Names follow the functional-key table of the kitty keyboard protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Key {
    /// A key that produces a character. This is the *unshifted* character of
    /// the key in the active layout (`'a'`, not `'A'`; `'4'`, not `'$'`).
    /// An upper-case letter passed here is lowercased by the encoder when its
    /// lowercase is one scalar (`'İ'` stays `'İ'`); a titlecase letter such
    /// as `'ǅ'` is taken as the unshifted key itself.
    Char(char),
    /// A text-only event with no identifiable key (for instance IME commit).
    /// Encoded with key number 0 when it must be an escape code.
    Unidentified,
    /// Escape.
    Escape,
    /// Enter / Return.
    Enter,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Insert.
    Insert,
    /// Delete (forward delete).
    Delete,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Home.
    Home,
    /// End.
    End,
    /// Caps Lock.
    CapsLock,
    /// Scroll Lock.
    ScrollLock,
    /// Num Lock.
    NumLock,
    /// Print Screen.
    PrintScreen,
    /// Pause.
    Pause,
    /// Menu.
    Menu,
    /// A function key, `F(1)` through `F(35)`. Other values encode to nothing.
    F(u8),
    /// Keypad 0.
    Kp0,
    /// Keypad 1.
    Kp1,
    /// Keypad 2.
    Kp2,
    /// Keypad 3.
    Kp3,
    /// Keypad 4.
    Kp4,
    /// Keypad 5.
    Kp5,
    /// Keypad 6.
    Kp6,
    /// Keypad 7.
    Kp7,
    /// Keypad 8.
    Kp8,
    /// Keypad 9.
    Kp9,
    /// Keypad decimal point.
    KpDecimal,
    /// Keypad `/`.
    KpDivide,
    /// Keypad `*`.
    KpMultiply,
    /// Keypad `-`.
    KpSubtract,
    /// Keypad `+`.
    KpAdd,
    /// Keypad Enter.
    KpEnter,
    /// Keypad `=`.
    KpEqual,
    /// Keypad `,`.
    KpSeparator,
    /// Keypad Left (Num Lock off).
    KpLeft,
    /// Keypad Right (Num Lock off).
    KpRight,
    /// Keypad Up (Num Lock off).
    KpUp,
    /// Keypad Down (Num Lock off).
    KpDown,
    /// Keypad Page Up (Num Lock off).
    KpPageUp,
    /// Keypad Page Down (Num Lock off).
    KpPageDown,
    /// Keypad Home (Num Lock off).
    KpHome,
    /// Keypad End (Num Lock off).
    KpEnd,
    /// Keypad Insert (Num Lock off).
    KpInsert,
    /// Keypad Delete (Num Lock off).
    KpDelete,
    /// Keypad Begin (the `5` key with Num Lock off).
    KpBegin,
    /// Media: play.
    MediaPlay,
    /// Media: pause.
    MediaPause,
    /// Media: play/pause toggle.
    MediaPlayPause,
    /// Media: reverse.
    MediaReverse,
    /// Media: stop.
    MediaStop,
    /// Media: fast forward.
    MediaFastForward,
    /// Media: rewind.
    MediaRewind,
    /// Media: next track.
    MediaTrackNext,
    /// Media: previous track.
    MediaTrackPrevious,
    /// Media: record.
    MediaRecord,
    /// Volume down.
    LowerVolume,
    /// Volume up.
    RaiseVolume,
    /// Mute.
    MuteVolume,
    /// Left Shift.
    LeftShift,
    /// Left Control.
    LeftControl,
    /// Left Alt.
    LeftAlt,
    /// Left Super.
    LeftSuper,
    /// Left Hyper.
    LeftHyper,
    /// Left Meta.
    LeftMeta,
    /// Right Shift.
    RightShift,
    /// Right Control.
    RightControl,
    /// Right Alt.
    RightAlt,
    /// Right Super.
    RightSuper,
    /// Right Hyper.
    RightHyper,
    /// Right Meta.
    RightMeta,
    /// ISO Level 3 Shift (`AltGr`).
    IsoLevel3Shift,
    /// ISO Level 5 Shift.
    IsoLevel5Shift,
}

impl Key {
    /// Whether this key is itself a modifier key.
    pub const fn is_modifier(self) -> bool {
        matches!(
            self,
            Self::LeftShift
                | Self::LeftControl
                | Self::LeftAlt
                | Self::LeftSuper
                | Self::LeftHyper
                | Self::LeftMeta
                | Self::RightShift
                | Self::RightControl
                | Self::RightAlt
                | Self::RightSuper
                | Self::RightHyper
                | Self::RightMeta
                | Self::IsoLevel3Shift
                | Self::IsoLevel5Shift
        )
    }

    /// Whether this key normally produces text: characters, and the keypad
    /// keys that stand in for characters.
    const fn is_text_key(self) -> bool {
        matches!(self, Self::Char(_) | Self::Unidentified) || self.keypad_char().is_some()
    }

    /// The character a keypad key stands in for, if any.
    const fn keypad_char(self) -> Option<char> {
        Some(match self {
            Self::Kp0 => '0',
            Self::Kp1 => '1',
            Self::Kp2 => '2',
            Self::Kp3 => '3',
            Self::Kp4 => '4',
            Self::Kp5 => '5',
            Self::Kp6 => '6',
            Self::Kp7 => '7',
            Self::Kp8 => '8',
            Self::Kp9 => '9',
            Self::KpDecimal => '.',
            Self::KpDivide => '/',
            Self::KpMultiply => '*',
            Self::KpSubtract => '-',
            Self::KpAdd => '+',
            Self::KpEqual => '=',
            Self::KpSeparator => ',',
            _ => return None,
        })
    }

    /// The main-keyboard key a keypad key stands in for in legacy mode.
    const fn legacy_equivalent(self) -> Self {
        match self {
            Self::KpEnter => Self::Enter,
            Self::KpLeft => Self::Left,
            Self::KpRight => Self::Right,
            Self::KpUp => Self::Up,
            Self::KpDown => Self::Down,
            Self::KpPageUp => Self::PageUp,
            Self::KpPageDown => Self::PageDown,
            Self::KpHome => Self::Home,
            Self::KpEnd => Self::End,
            Self::KpInsert => Self::Insert,
            Self::KpDelete => Self::Delete,
            other => match other.keypad_char() {
                Some(c) => Self::Char(c),
                None => other,
            },
        }
    }

    /// The final byte of the DECKPAM (application keypad) `SS3` sequence.
    const fn application_keypad_byte(self) -> Option<u8> {
        Some(match self {
            Self::Kp0 => b'p',
            Self::Kp1 => b'q',
            Self::Kp2 => b'r',
            Self::Kp3 => b's',
            Self::Kp4 => b't',
            Self::Kp5 => b'u',
            Self::Kp6 => b'v',
            Self::Kp7 => b'w',
            Self::Kp8 => b'x',
            Self::Kp9 => b'y',
            Self::KpMultiply => b'j',
            Self::KpAdd => b'k',
            Self::KpSeparator => b'l',
            Self::KpSubtract => b'm',
            Self::KpDecimal => b'n',
            Self::KpDivide => b'o',
            Self::KpEnter => b'M',
            Self::KpEqual => b'X',
            _ => return None,
        })
    }
}

/// How a key is written in the kitty functional-key table.
#[derive(Clone, Copy)]
enum Functional {
    /// `CSI 1 ; mods <letter>`, or `CSI <letter>` without parameters.
    Letter(u8),
    /// `CSI number ; mods ~`.
    Tilde(u32),
    /// `CSI number ; mods u`.
    U(u32),
}

impl Functional {
    fn number(self) -> u32 {
        match self {
            Self::Letter(_) => 1,
            Self::Tilde(n) | Self::U(n) => n,
        }
    }

    fn terminator(self) -> u8 {
        match self {
            Self::Letter(l) => l,
            Self::Tilde(_) => b'~',
            Self::U(_) => b'u',
        }
    }
}

fn functional(key: Key) -> Option<Functional> {
    use Functional::{Letter, Tilde, U};
    Some(match key {
        Key::Escape => U(27),
        Key::Enter => U(13),
        Key::Tab => U(9),
        Key::Backspace => U(127),
        Key::Insert => Tilde(2),
        Key::Delete => Tilde(3),
        Key::Left => Letter(b'D'),
        Key::Right => Letter(b'C'),
        Key::Up => Letter(b'A'),
        Key::Down => Letter(b'B'),
        Key::PageUp => Tilde(5),
        Key::PageDown => Tilde(6),
        Key::Home => Letter(b'H'),
        Key::End => Letter(b'F'),
        Key::CapsLock => U(57_358),
        Key::ScrollLock => U(57_359),
        Key::NumLock => U(57_360),
        Key::PrintScreen => U(57_361),
        Key::Pause => U(57_362),
        Key::Menu => U(57_363),
        Key::F(1) => Letter(b'P'),
        Key::F(2) => Letter(b'Q'),
        Key::F(3) => Tilde(13),
        Key::F(4) => Letter(b'S'),
        Key::F(5) => Tilde(15),
        Key::F(6) => Tilde(17),
        Key::F(7) => Tilde(18),
        Key::F(8) => Tilde(19),
        Key::F(9) => Tilde(20),
        Key::F(10) => Tilde(21),
        Key::F(11) => Tilde(23),
        Key::F(12) => Tilde(24),
        Key::F(n @ 13..=35) => U(57_376 + u32::from(n) - 13),
        Key::Char(_) | Key::Unidentified | Key::F(_) => return None,
        Key::Kp0 => U(57_399),
        Key::Kp1 => U(57_400),
        Key::Kp2 => U(57_401),
        Key::Kp3 => U(57_402),
        Key::Kp4 => U(57_403),
        Key::Kp5 => U(57_404),
        Key::Kp6 => U(57_405),
        Key::Kp7 => U(57_406),
        Key::Kp8 => U(57_407),
        Key::Kp9 => U(57_408),
        Key::KpDecimal => U(57_409),
        Key::KpDivide => U(57_410),
        Key::KpMultiply => U(57_411),
        Key::KpSubtract => U(57_412),
        Key::KpAdd => U(57_413),
        Key::KpEnter => U(57_414),
        Key::KpEqual => U(57_415),
        Key::KpSeparator => U(57_416),
        Key::KpLeft => U(57_417),
        Key::KpRight => U(57_418),
        Key::KpUp => U(57_419),
        Key::KpDown => U(57_420),
        Key::KpPageUp => U(57_421),
        Key::KpPageDown => U(57_422),
        Key::KpHome => U(57_423),
        Key::KpEnd => U(57_424),
        Key::KpInsert => U(57_425),
        Key::KpDelete => U(57_426),
        Key::KpBegin => Letter(b'E'),
        Key::MediaPlay => U(57_428),
        Key::MediaPause => U(57_429),
        Key::MediaPlayPause => U(57_430),
        Key::MediaReverse => U(57_431),
        Key::MediaStop => U(57_432),
        Key::MediaFastForward => U(57_433),
        Key::MediaRewind => U(57_434),
        Key::MediaTrackNext => U(57_435),
        Key::MediaTrackPrevious => U(57_436),
        Key::MediaRecord => U(57_437),
        Key::LowerVolume => U(57_438),
        Key::RaiseVolume => U(57_439),
        Key::MuteVolume => U(57_440),
        Key::LeftShift => U(57_441),
        Key::LeftControl => U(57_442),
        Key::LeftAlt => U(57_443),
        Key::LeftSuper => U(57_444),
        Key::LeftHyper => U(57_445),
        Key::LeftMeta => U(57_446),
        Key::RightShift => U(57_447),
        Key::RightControl => U(57_448),
        Key::RightAlt => U(57_449),
        Key::RightSuper => U(57_450),
        Key::RightHyper => U(57_451),
        Key::RightMeta => U(57_452),
        Key::IsoLevel3Shift => U(57_453),
        Key::IsoLevel5Shift => U(57_454),
    })
}

/// A keyboard event as delivered by a frontend.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct KeyEvent {
    /// The key, unshifted.
    pub key: Key,
    /// Modifiers active for the event, including lock state.
    pub modifiers: Modifiers,
    /// Press, repeat or release.
    pub kind: KeyEventKind,
    /// The shifted variant of the key in the active layout (`'A'` for
    /// `'a'`, `'$'` for `'4'`). When absent a US layout is assumed.
    pub shifted_key: Option<char>,
    /// The key at the same physical position in the standard PC-101 layout
    /// (`'c'` when a Cyrillic layout produced `'с'`). Reported with the kitty
    /// *report alternate keys* enhancement, and used as the fallback for
    /// legacy Ctrl/Alt combinations when the key itself is not an ASCII key,
    /// so frontends should supply it whenever the layout is not US ASCII.
    pub base_layout_key: Option<char>,
    /// Text the OS composed for this event, if any. Takes precedence over the
    /// character derived from [`KeyEvent::key`] when the event is sent as
    /// text.
    pub text: Option<String>,
}

impl KeyEvent {
    /// A press of `key` with `modifiers` and no extra information.
    pub fn new(key: Key, modifiers: Modifiers) -> Self {
        Self {
            key,
            modifiers,
            kind: KeyEventKind::Press,
            shifted_key: None,
            base_layout_key: None,
            text: None,
        }
    }

    /// Sets the event kind.
    #[must_use]
    pub fn with_kind(mut self, kind: KeyEventKind) -> Self {
        self.kind = kind;
        self
    }

    /// Sets the shifted key.
    #[must_use]
    pub fn with_shifted_key(mut self, shifted: char) -> Self {
        self.shifted_key = Some(shifted);
        self
    }

    /// Sets the base-layout key.
    #[must_use]
    pub fn with_base_layout_key(mut self, base: char) -> Self {
        self.base_layout_key = Some(base);
        self
    }

    /// Sets the composed text.
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// The character of a `Char` key, normalised to its unshifted form.
    /// `None` for other keys and for control code points, which are not
    /// characters a key can legitimately produce.
    ///
    /// Only an upper-case letter ([`char::is_uppercase`]) is lowercased to
    /// find its unshifted key, and only when its lowercase is one scalar
    /// (`İ` stays `İ`). Titlecase letters (general category Lt, such as `ǅ`
    /// U+01C5 or `ᾈ` U+1F88) and uncased characters are reported as given:
    /// kitty sends the code point the layout produces and never lowercases a
    /// titlecase letter, which has no unshifted form distinct from itself.
    fn unshifted_char(&self) -> Option<char> {
        let Key::Char(c) = self.key else {
            return None;
        };
        if c.is_control() {
            return None;
        }
        if !c.is_uppercase() {
            return Some(c);
        }
        let mut lower = c.to_lowercase();
        match (lower.next(), lower.next()) {
            (Some(l), None) => Some(l),
            _ => Some(c),
        }
    }

    /// The shifted form of a `Char` key: as supplied, else from the composed
    /// text when only Shift is held (Caps Lock would invert it for letters,
    /// so it is not trusted then), else from a US layout.
    fn shifted_char(&self) -> Option<char> {
        let c = self.unshifted_char()?;
        if let Some(s) = self.shifted_key.filter(|s| !s.is_control()) {
            return Some(s);
        }
        if self.modifiers.without_locks() == Modifiers::SHIFT
            && !self.modifiers.contains(Modifiers::CAPS_LOCK)
            && let Some(text) = self.composed_text()
        {
            let mut chars = text.chars();
            if let (Some(t), None) = (chars.next(), chars.next()) {
                return Some(t);
            }
        }
        Some(us_shifted(c))
    }

    /// The base-layout key, unless it is a control code point.
    fn base_layout_char(&self) -> Option<char> {
        self.base_layout_key.filter(|b| !b.is_control())
    }

    /// The frontend's composed text, when it is non-empty and free of control
    /// characters.
    fn composed_text(&self) -> Option<&str> {
        self.text
            .as_deref()
            .filter(|t| !t.is_empty() && !t.chars().any(char::is_control))
    }

    /// Text to send for this event in legacy modes (no Ctrl/Alt involved).
    fn plain_text(&self) -> Option<String> {
        if let Some(text) = self.composed_text() {
            return Some(text.to_owned());
        }
        if let Some(c) = self.key.keypad_char() {
            return Some(c.to_string());
        }
        let shift = self.modifiers.contains(Modifiers::SHIFT);
        let c = if shift {
            self.shifted_char()?
        } else {
            self.unshifted_char()?
        };
        Some(apply_caps_lock(c, self.modifiers).to_string())
    }
}

/// Applies Caps Lock to a derived character: the case of a letter is
/// inverted while the lock is engaged, so Caps+a is `A` and Shift+Caps+a
/// is `a`. Other characters are unchanged.
///
/// A titlecase letter is not lower case, so Caps Lock lowercases it:
/// Caps+`ǅ` (U+01C5) is `ǆ` and Caps+`ᾈ` (U+1F88) is `ᾀ`. Before #73 the
/// four Latin digraphs (U+01C5, U+01C8, U+01CB, U+01F2) reached this
/// function already lowercased and came out upper case (`Ǆ`); the Greek
/// titlecase letters are unaffected, as their uppercase is two scalars.
fn apply_caps_lock(c: char, mods: Modifiers) -> char {
    if !mods.contains(Modifiers::CAPS_LOCK) || !c.is_alphabetic() {
        return c;
    }
    let swapped: String = if c.is_lowercase() {
        c.to_uppercase().collect()
    } else {
        c.to_lowercase().collect()
    };
    let mut chars = swapped.chars();
    match (chars.next(), chars.next()) {
        (Some(s), None) => s,
        _ => c,
    }
}

/// Shifted character of `c` on a US keyboard.
fn us_shifted(c: char) -> char {
    match c {
        '`' => '~',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        _ => {
            let mut upper = c.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(u), None) => u,
                _ => c,
            }
        }
    }
}

/// The byte of an ASCII character.
fn ascii_byte(c: char) -> Option<u8> {
    u8::try_from(u32::from(c)).ok().filter(u8::is_ascii)
}

/// The C0 byte produced by Ctrl and an ASCII key, per the kitty table
/// ("Legacy ctrl mapping of ASCII keys"), extended with the upper-case
/// letters so that Ctrl+Shift+letter gives xterm's C0 byte, and with `` ` ``,
/// `{`, `|` and `}`, which xterm masks like the rest of `@`..`~` (see the
/// module docs). `None` for keys the table leaves untouched by Ctrl.
fn ctrl_mapping(c: char) -> Option<u8> {
    Some(match c {
        ' ' | '2' | '@' | '`' => 0,
        '3' | '[' | '{' => 27,
        '4' | '\\' | '|' => 28,
        '5' | ']' | '}' => 29,
        '6' | '^' | '~' => 30,
        '7' | '/' | '_' => 31,
        '8' | '?' => 127,
        'a'..='z' => ascii_byte(c)? - b'a' + 1,
        'A'..='Z' => ascii_byte(c)? - b'A' + 1,
        _ => return None,
    })
}

/// The byte sent for Ctrl and an ASCII key: the C0 mapping, or the key
/// itself when the table leaves it untouched. `None` for non-ASCII keys.
fn ctrl_byte(c: char) -> Option<u8> {
    ctrl_mapping(c).or_else(|| ascii_byte(c))
}

/// Progressive-enhancement flags of the kitty keyboard protocol.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct KittyFlags(u8);

impl KittyFlags {
    /// No enhancement: legacy encoding.
    pub const NONE: Self = Self(0);
    /// `0b1`: report Esc, Alt/Ctrl combinations and keypad keys as `CSI u`.
    pub const DISAMBIGUATE_ESCAPE_CODES: Self = Self(1);
    /// `0b10`: report repeat and release events.
    pub const REPORT_EVENT_TYPES: Self = Self(2);
    /// `0b100`: report shifted and base-layout keys.
    pub const REPORT_ALTERNATE_KEYS: Self = Self(4);
    /// `0b1000`: report every key, including text keys, as an escape code.
    pub const REPORT_ALL_KEYS_AS_ESCAPE_CODES: Self = Self(8);
    /// `0b10000`: embed the associated text in the escape code.
    pub const REPORT_ASSOCIATED_TEXT: Self = Self(16);
    /// Every flag.
    pub const ALL: Self = Self(31);

    /// Builds a flag set from the raw bits, ignoring undefined bits.
    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self((bits & 31) as u8)
    }

    /// The raw bits.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether no flag is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every flag in `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any flag in `other` is set.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Applies a `CSI = flags ; mode u` request.
    #[must_use]
    pub const fn apply(self, flags: Self, mode: KittyFlagsMode) -> Self {
        match mode {
            KittyFlagsMode::Replace => flags,
            KittyFlagsMode::Set => Self(self.0 | flags.0),
            KittyFlagsMode::Clear => Self(self.0 & !flags.0),
        }
    }
}

impl BitOr for KittyFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for KittyFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl fmt::Debug for KittyFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KittyFlags({:#07b})", self.0)
    }
}

/// The `mode` parameter of `CSI = flags ; mode u`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KittyFlagsMode {
    /// `1`: set bits are set, unset bits are reset.
    #[default]
    Replace,
    /// `2`: set bits are set, unset bits are left unchanged.
    Set,
    /// `3`: set bits are reset, unset bits are left unchanged.
    Clear,
}

impl KittyFlagsMode {
    /// Parses the numeric parameter; unknown values are `None`.
    pub const fn from_param(param: u32) -> Option<Self> {
        match param {
            1 => Some(Self::Replace),
            2 => Some(Self::Set),
            3 => Some(Self::Clear),
            _ => None,
        }
    }
}

/// xterm's `modifyOtherKeys` resource, set with `CSI > 4 ; level m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModifyOtherKeys {
    /// Level 0: legacy encoding only.
    #[default]
    Disabled,
    /// Level 1: modified keys are encoded as `CSI 27 ; mods ; code ~`, except
    /// combinations with a well-established legacy encoding, as in xterm:
    /// Ctrl+key and Ctrl+Shift+key when the ctrl table yields a C0 byte,
    /// Alt+key, Shift+key, Ctrl+Space, Ctrl+Shift+Space, Alt+Space, and
    /// Backspace and Escape with any modifier. Tab and Enter *are* encoded
    /// with any modifier (`alt-Tab sends CSI 27;3;9~`, per ctlseqs), except
    /// Shift+Tab alone, which stays `CSI Z`.
    Level1,
    /// Level 2: every modified key is encoded, including Tab, Enter,
    /// Backspace and Escape with Shift.
    ///
    /// Shift alone on a printable key sends the shifted character. This is a
    /// deliberate simplification: xterm encodes Shift+letter as
    /// `CSI 27 ; 2 ; code ~` at level 2 but sends `!` for Shift+1, because
    /// its test is on the keysym range rather than on the modifier. Every
    /// level-2 client accepts the plain shifted character (Vim strips the
    /// Shift modifier itself), so Tessera sends it for both.
    Level2,
}

/// The terminal mode state that influences key encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct KeyboardModes {
    /// DECCKM (`CSI ? 1 h`): cursor keys send `SS3` sequences.
    pub application_cursor_keys: bool,
    /// DECKPAM (`ESC =`): the numeric keypad sends `SS3` sequences.
    pub application_keypad: bool,
    /// xterm `modifyOtherKeys` level.
    pub modify_other_keys: ModifyOtherKeys,
    /// Active kitty keyboard protocol flags.
    pub kitty_flags: KittyFlags,
}

/// Encodes a key event into the bytes to write to the PTY.
///
/// Returns an empty vector for events that produce nothing (releases in
/// legacy mode, bare modifier keys, unknown function keys).
pub fn encode_key(event: &KeyEvent, modes: &KeyboardModes) -> Vec<u8> {
    let mut out = Vec::new();
    encode_key_into(event, modes, &mut out);
    out
}

/// Encodes a key event, appending the bytes to `out`.
pub fn encode_key_into(event: &KeyEvent, modes: &KeyboardModes, out: &mut Vec<u8>) {
    if modes.kitty_flags.is_empty() {
        Legacy { event, modes }.encode(out);
    } else {
        Kitty { event, modes }.encode(out);
    }
}

/// Appends `CSI`, the given parameters joined with `;`, and `terminator`.
fn put_csi(out: &mut Vec<u8>, params: &[String], terminator: u8) {
    out.extend_from_slice(CSI);
    out.extend_from_slice(params.join(";").as_bytes());
    out.push(terminator);
}

// ---------------------------------------------------------------------------
// Legacy (xterm) encoding
// ---------------------------------------------------------------------------

struct Legacy<'a> {
    event: &'a KeyEvent,
    modes: &'a KeyboardModes,
}

/// Shift, Alt and Ctrl: the modifiers legacy encodings can express.
const BASIC_MODIFIERS: Modifiers =
    Modifiers(Modifiers::SHIFT.0 | Modifiers::ALT.0 | Modifiers::CTRL.0);

impl Legacy<'_> {
    fn encode(&self, out: &mut Vec<u8>) {
        let event = self.event;
        if event.kind == KeyEventKind::Release || event.key.is_modifier() {
            return;
        }
        let mods = event.modifiers.without_locks();

        if mods.is_empty()
            && self.modes.application_keypad
            && let Some(byte) = event.key.application_keypad_byte()
        {
            out.extend_from_slice(SS3);
            out.push(byte);
            return;
        }

        // Keypad keys are their main-keyboard equivalents from here on, in
        // every form including the CSI u fallback. A keypad character key
        // has no shifted symbol: Shift is consumed by it (see module docs).
        let key = event.key.legacy_equivalent();
        let is_keypad_char = event.key.keypad_char().is_some();
        let mods = if is_keypad_char {
            mods - Modifiers::SHIFT
        } else {
            mods
        };
        let equivalent;
        let event = if key == event.key {
            event
        } else {
            equivalent = KeyEvent {
                key,
                modifiers: if is_keypad_char {
                    event.modifiers - Modifiers::SHIFT
                } else {
                    event.modifiers
                },
                shifted_key: None,
                ..event.clone()
            };
            &equivalent
        };

        if self.modes.modify_other_keys != ModifyOtherKeys::Disabled
            && let Some(code) = self.modify_other_keys_code(event, mods)
        {
            let params = [
                27.to_string(),
                xterm_modifier_param(mods).to_string(),
                code.to_string(),
            ];
            put_csi(out, &params, b'~');
            return;
        }

        match key {
            Key::Enter | Key::Escape | Key::Backspace | Key::Tab | Key::Char(' ') => {
                Self::encode_c0_key(out, event, mods);
            }
            Key::Char(_) | Key::Unidentified => Self::encode_text_key(out, event, mods),
            _ => self.encode_functional_key(out, event, mods),
        }
    }

    /// The `code` parameter of `CSI 27 ; mods ; code ~` when `modifyOtherKeys`
    /// applies to this combination, else `None`.
    fn modify_other_keys_code(&self, event: &KeyEvent, mods: Modifiers) -> Option<u32> {
        if mods.is_empty() {
            return None;
        }
        let level2 = self.modes.modify_other_keys == ModifyOtherKeys::Level2;
        let shift_only = mods == Modifiers::SHIFT;
        // xterm's ModifyOtherKeys(): at level 1 Return and Tab are encoded
        // under any modifier, Backspace never, and the rest only when the
        // combination has no legacy byte of its own.
        let ctrl_maybe_shift = mods - Modifiers::SHIFT == Modifiers::CTRL;
        match event.key {
            Key::Enter => Some(13),
            Key::Tab => (level2 || !shift_only).then_some(9),
            Key::Backspace => level2.then_some(127),
            Key::Escape => level2.then_some(27),
            Key::Char(' ') => {
                let well_known = shift_only || ctrl_maybe_shift || mods == Modifiers::ALT;
                (level2 || !well_known).then_some(32)
            }
            Key::Char(_) => {
                if shift_only {
                    return None;
                }
                let code = if mods.contains(Modifiers::SHIFT) {
                    event.shifted_char()?
                } else {
                    event.unshifted_char()?
                };
                let alt_only = mods - Modifiers::SHIFT == Modifiers::ALT;
                // Ctrl(+Shift) is a legacy byte when the (shifted) key has a
                // C0 mapping, as xterm's allowedCharModifiers() decides.
                let well_known_ctrl = ctrl_maybe_shift && ctrl_mapping(code).is_some();
                let well_known = alt_only || well_known_ctrl;
                (level2 || !well_known).then_some(u32::from(code))
            }
            _ => None,
        }
    }

    /// Enter, Escape, Backspace, Tab, Space: the kitty "C0 controls" table.
    fn encode_c0_key(out: &mut Vec<u8>, event: &KeyEvent, mods: Modifiers) {
        if !BASIC_MODIFIERS.contains(mods) {
            Kitty::csi_u_press(event, mods, out);
            return;
        }
        let ctrl = mods.contains(Modifiers::CTRL);
        let shift = mods.contains(Modifiers::SHIFT);
        if mods.contains(Modifiers::ALT) {
            out.push(ESC);
        }
        match event.key {
            Key::Enter => out.push(0x0d),
            Key::Escape => out.push(ESC),
            Key::Backspace => out.push(if ctrl { 0x08 } else { 0x7f }),
            Key::Tab if shift => out.extend_from_slice(b"\x1b[Z"),
            Key::Tab => out.push(0x09),
            _ => out.push(if ctrl { 0x00 } else { 0x20 }),
        }
    }

    /// `Char` keys: text, or the "Legacy text keys" algorithm (see the module
    /// docs for where it follows xterm rather than kitty).
    fn encode_text_key(out: &mut Vec<u8>, event: &KeyEvent, mods: Modifiers) {
        if event.key == Key::Unidentified {
            // Pure text events carry no key to modify.
            if let Some(text) = event.plain_text() {
                out.extend_from_slice(text.as_bytes());
            }
            return;
        }
        if !BASIC_MODIFIERS.contains(mods) {
            // Super/Hyper/Meta combinations have no legacy form; kitty (and
            // we) fall back to CSI u even in legacy mode.
            Kitty::csi_u_press(event, mods, out);
            return;
        }
        let ctrl = mods.contains(Modifiers::CTRL);
        let alt = mods.contains(Modifiers::ALT);
        let shift = mods.contains(Modifiers::SHIFT);

        // A control-code Char is rejected by `unshifted_char`.
        let Some(unshifted) = event.unshifted_char() else {
            return;
        };

        if !ctrl && let Some(text) = event.composed_text() {
            // Plain or Alt with text the frontend composed: send it as is.
            if alt {
                out.push(ESC);
            }
            out.extend_from_slice(text.as_bytes());
            return;
        }

        // Shift is consumed by the character it produces; when it produces
        // nothing different (Space, letters without case) it stays pending.
        let shifted = shift
            .then(|| event.shifted_char())
            .flatten()
            .filter(|s| *s != unshifted);
        let c = shifted.unwrap_or(unshifted);
        let shift_pending = shift && shifted.is_none();

        // Under Ctrl or Alt, non-ASCII keys fall back to the key at the same
        // position in the base (PC-101) layout, as kitty does, so Ctrl+С is
        // still ^C and Alt+ф is still ESC a.
        let ascii = ascii_byte(c).map(|_| c).or_else(|| {
            let base = event
                .base_layout_char()
                .filter(|b| ascii_byte(*b).is_some())?;
            Some(if shift { us_shifted(base) } else { base })
        });

        if !ctrl {
            if alt {
                out.push(ESC);
            }
            let text = if alt { ascii.unwrap_or(c) } else { c };
            let text = apply_caps_lock(text, event.modifiers);
            out.extend_from_slice(text.to_string().as_bytes());
            return;
        }
        // Ctrl+Shift with nothing to shift has no legacy form, unless the
        // key has a C0 byte of its own (letters, Space): xterm sends that
        // byte whatever Shift did to the character.
        let byte = if shift_pending {
            ascii.and_then(ctrl_mapping)
        } else {
            ascii.and_then(ctrl_byte)
        };
        let Some(byte) = byte else {
            Kitty::csi_u_press(event, mods, out);
            return;
        };
        if alt {
            out.push(ESC);
        }
        out.push(byte);
    }

    /// Everything with a terminfo entry, plus CSI u for the rest.
    fn encode_functional_key(&self, out: &mut Vec<u8>, event: &KeyEvent, mods: Modifiers) {
        // F13..F35 are the PC-style aliases the terminfo entry declares
        // (`kf13=\E[1;2P` ... `kf35=\E[23;5~`): F13..F24 are Shift+F1..F12
        // and F25..F35 are Ctrl+F1..F11. Modifiers held with them fold into
        // the same parameter, as xterm does (`kf37=\E[1;6P` is Ctrl+F13).
        let (key, mods) = match event.key {
            Key::F(n @ 13..=35) => {
                let implied = if n <= 24 {
                    Modifiers::SHIFT
                } else {
                    Modifiers::CTRL
                };
                (Key::F((n - 13) % 12 + 1), mods | implied)
            }
            key => (key, mods),
        };
        let Some(form) = legacy_functional(key) else {
            if functional(key).is_some() {
                Kitty::csi_u_press(event, mods, out);
            }
            return;
        };
        // xterm's modifier parameter, which folds Super/Hyper/Meta onto Meta,
        // so that every sequence sent here is one xterm itself could send.
        let param = xterm_modifier_param(mods).to_string();
        match form {
            LegacyForm::Cursor(letter) => {
                if mods.is_empty() {
                    out.extend_from_slice(if self.modes.application_cursor_keys {
                        SS3
                    } else {
                        CSI
                    });
                    out.push(letter);
                } else {
                    put_csi(out, &["1".into(), param], letter);
                }
            }
            LegacyForm::Ss3(letter) => {
                if mods.is_empty() {
                    out.extend_from_slice(SS3);
                    out.push(letter);
                } else {
                    put_csi(out, &["1".into(), param], letter);
                }
            }
            LegacyForm::Tilde(n) => {
                if mods.is_empty() {
                    put_csi(out, &[n.to_string()], b'~');
                } else {
                    put_csi(out, &[n.to_string(), param], b'~');
                }
            }
        }
    }
}

/// xterm's four-bit modifier parameter: Shift 1, Alt 2, Ctrl 4, Meta 8.
///
/// Super, Hyper and Meta all map onto xterm's Meta bit.
fn xterm_modifier_param(mods: Modifiers) -> u32 {
    let mut bits = mods.bits() & 0b111;
    if mods.intersects(Modifiers::SUPER | Modifiers::HYPER | Modifiers::META) {
        bits |= 0b1000;
    }
    u32::from(bits) + 1
}

/// How a key is encoded in legacy mode.
#[derive(Clone, Copy)]
enum LegacyForm {
    /// `CSI <letter>` (or `SS3 <letter>` under DECCKM), `CSI 1 ; mods <letter>`.
    Cursor(u8),
    /// `SS3 <letter>`, `CSI 1 ; mods <letter>`.
    Ss3(u8),
    /// `CSI n ~`, `CSI n ; mods ~`.
    Tilde(u32),
}

fn legacy_functional(key: Key) -> Option<LegacyForm> {
    Some(match key {
        Key::Insert => LegacyForm::Tilde(2),
        Key::Delete => LegacyForm::Tilde(3),
        Key::PageUp => LegacyForm::Tilde(5),
        Key::PageDown => LegacyForm::Tilde(6),
        Key::Up => LegacyForm::Cursor(b'A'),
        Key::Down => LegacyForm::Cursor(b'B'),
        Key::Right => LegacyForm::Cursor(b'C'),
        Key::Left => LegacyForm::Cursor(b'D'),
        Key::KpBegin => LegacyForm::Cursor(b'E'),
        Key::Home => LegacyForm::Cursor(b'H'),
        Key::End => LegacyForm::Cursor(b'F'),
        Key::F(1) => LegacyForm::Ss3(b'P'),
        Key::F(2) => LegacyForm::Ss3(b'Q'),
        Key::F(3) => LegacyForm::Ss3(b'R'),
        Key::F(4) => LegacyForm::Ss3(b'S'),
        Key::F(5) => LegacyForm::Tilde(15),
        Key::F(6) => LegacyForm::Tilde(17),
        Key::F(7) => LegacyForm::Tilde(18),
        Key::F(8) => LegacyForm::Tilde(19),
        Key::F(9) => LegacyForm::Tilde(20),
        Key::F(10) => LegacyForm::Tilde(21),
        Key::F(11) => LegacyForm::Tilde(23),
        Key::F(12) => LegacyForm::Tilde(24),
        Key::Menu => LegacyForm::Tilde(29),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Kitty keyboard protocol
// ---------------------------------------------------------------------------

struct Kitty<'a> {
    event: &'a KeyEvent,
    modes: &'a KeyboardModes,
}

impl Kitty<'_> {
    fn flags(&self) -> KittyFlags {
        self.modes.kitty_flags
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let event = self.event;
        let flags = self.flags();
        let report_all = flags.contains(KittyFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);
        let disambiguate = report_all || flags.contains(KittyFlags::DISAMBIGUATE_ESCAPE_CODES);
        let release = event.kind == KeyEventKind::Release;
        // kitty's encoder only uses the legacy forms of *modified* keys when
        // no flag at all asks for richer reports: with just *report event
        // types* (or *report associated text*), Ctrl+Esc is `CSI 27;5u` and
        // Shift+Tab is `CSI 9;2u`. A Ctrl/Alt text key keeps its legacy
        // byte on press, but its release can only be an escape code.
        let modified_as_csi_u = disambiguate
            || flags
                .intersects(KittyFlags::REPORT_EVENT_TYPES | KittyFlags::REPORT_ASSOCIATED_TEXT);
        let text_press_as_csi_u =
            disambiguate || flags.contains(KittyFlags::REPORT_ASSOCIATED_TEXT);

        if release && !flags.contains(KittyFlags::REPORT_EVENT_TYPES) {
            return;
        }
        if event.key.is_modifier() && !report_all {
            return;
        }
        if event.key == Key::Unidentified {
            // A text-only event: delivered as text unless both flags that
            // define the `CSI 0 ; ; text u` form are set (see module docs).
            let as_escape = report_all && flags.contains(KittyFlags::REPORT_ASSOCIATED_TEXT);
            if release || event.composed_text().is_none() {
                return;
            }
            if as_escape {
                self.csi_u(out, event.modifiers);
            } else if let Some(text) = event.plain_text() {
                out.extend_from_slice(text.as_bytes());
            }
            return;
        }
        if report_all {
            self.csi_u(out, event.modifiers);
            return;
        }

        let key = if disambiguate {
            event.key
        } else {
            event.key.legacy_equivalent()
        };

        if key.is_text_key() {
            // Lock modifiers are not reported for text-producing keys.
            let mods = event.modifiers.without_locks();
            if Modifiers::SHIFT.contains(mods) {
                if !release && let Some(text) = event.plain_text() {
                    out.extend_from_slice(text.as_bytes());
                }
            } else if text_press_as_csi_u || release {
                self.csi_u(out, mods);
            } else {
                self.legacy_fallback(out);
            }
            return;
        }

        match key {
            Key::Enter | Key::Tab | Key::Backspace => {
                let mods = event.modifiers.without_locks();
                if mods.is_empty() {
                    if !release {
                        out.push(match key {
                            Key::Enter => 0x0d,
                            Key::Tab => 0x09,
                            _ => 0x7f,
                        });
                    }
                } else if modified_as_csi_u {
                    self.csi_u(out, mods);
                } else {
                    self.legacy_fallback(out);
                }
            }
            Key::Escape if !disambiguate => {
                let mods = event.modifiers.without_locks();
                if mods.is_empty() {
                    // Bare Escape keeps its legacy byte; like other keys sent
                    // in legacy form, its release is not reported.
                    self.legacy_fallback(out);
                } else if modified_as_csi_u {
                    self.csi_u(out, mods);
                } else {
                    self.legacy_fallback(out);
                }
            }
            _ => self.csi_u(out, event.modifiers),
        }
    }

    /// Legacy bytes for a key that has a legacy form, for flag sets that do
    /// not disambiguate. Releases produce nothing.
    fn legacy_fallback(&self, out: &mut Vec<u8>) {
        if self.event.kind == KeyEventKind::Release {
            return;
        }
        let modes = KeyboardModes {
            kitty_flags: KittyFlags::NONE,
            modify_other_keys: ModifyOtherKeys::Disabled,
            ..*self.modes
        };
        Legacy {
            event: self.event,
            modes: &modes,
        }
        .encode(out);
    }

    /// A press encoded as CSI u without any enhancement (used by the legacy
    /// encoder for combinations that have no legacy form).
    fn csi_u_press(event: &KeyEvent, mods: Modifiers, out: &mut Vec<u8>) {
        let press = KeyEvent {
            kind: KeyEventKind::Press,
            ..event.clone()
        };
        Kitty {
            event: &press,
            modes: &KeyboardModes::default(),
        }
        .csi_u(out, mods);
    }

    /// The central `CSI key[:shifted[:base]] ; mods[:event] ; text` form, or
    /// the `CSI 1 ; mods <letter>` / `CSI n ; mods ~` forms for functional
    /// keys that have them.
    fn csi_u(&self, out: &mut Vec<u8>, mods: Modifiers) {
        let event = self.event;
        let form = match event.key {
            // A control-code Char is malformed and encodes to nothing.
            Key::Char(_) => match event.unshifted_char() {
                Some(c) => Functional::U(u32::from(c)),
                None => return,
            },
            Key::Unidentified => Functional::U(0),
            key => match functional(key) {
                Some(form) => form,
                None => return,
            },
        };

        let mut key_field = form.number().to_string();
        if self.flags().contains(KittyFlags::REPORT_ALTERNATE_KEYS)
            && matches!(form, Functional::U(_))
            && matches!(event.key, Key::Char(_))
        {
            let unshifted = event.unshifted_char();
            let shifted = if mods.contains(Modifiers::SHIFT) {
                event.shifted_char().filter(|s| Some(*s) != unshifted)
            } else {
                None
            };
            let base = event.base_layout_char().filter(|b| Some(*b) != unshifted);
            if shifted.is_some() || base.is_some() {
                key_field.push(':');
                if let Some(s) = shifted {
                    key_field.push_str(&u32::from(s).to_string());
                }
                if let Some(b) = base {
                    key_field.push(':');
                    key_field.push_str(&u32::from(b).to_string());
                }
            }
        }

        let text_field = self.text_codepoints();
        let report_events = self.flags().contains(KittyFlags::REPORT_EVENT_TYPES);
        let event_type = match event.kind {
            KeyEventKind::Repeat if report_events => Some(2),
            KeyEventKind::Release if report_events => Some(3),
            _ => None,
        };

        if matches!(form, Functional::Letter(_))
            && mods.is_empty()
            && event_type.is_none()
            && text_field.is_none()
        {
            // `CSI 1 <letter>` omits the 1 when there are no parameters.
            put_csi(out, &[], form.terminator());
            return;
        }

        let mut params = vec![key_field];
        if !mods.is_empty() || event_type.is_some() || text_field.is_some() {
            let mut mods_field = String::new();
            if !mods.is_empty() || event_type.is_some() {
                mods_field.push_str(&mods.param().to_string());
                if let Some(t) = event_type {
                    mods_field.push(':');
                    mods_field.push_str(&t.to_string());
                }
            }
            params.push(mods_field);
        }
        if let Some(text) = text_field {
            params.push(text);
        }
        put_csi(out, &params, form.terminator());
    }

    /// The associated-text field: code points joined with `:`.
    ///
    /// The field carries the text the event actually produced: what the
    /// frontend composed, or, when it composed nothing, the character the key
    /// stands for when only Shift (and locks) are held. Ctrl, Alt, Super,
    /// Hyper and Meta combinations produce no text of their own, so none is
    /// fabricated for them (Ctrl+C is `CSI 99 ; 5 u`, never `CSI 99;5;99u`).
    fn text_codepoints(&self) -> Option<String> {
        let event = self.event;
        if !self.flags().contains(KittyFlags::REPORT_ASSOCIATED_TEXT)
            || event.kind == KeyEventKind::Release
            || !event.key.is_text_key()
        {
            return None;
        }
        let text = if event.composed_text().is_some()
            || Modifiers::SHIFT.contains(event.modifiers.without_locks())
        {
            event.plain_text()?
        } else {
            return None;
        };
        let field = text
            .chars()
            .map(|c| u32::from(c).to_string())
            .collect::<Vec<_>>()
            .join(":");
        (!field.is_empty()).then_some(field)
    }
}
