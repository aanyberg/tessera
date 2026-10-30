//! Input: keys, text, mouse, scroll, paste and focus.
//!
//! The frontend translates its native events into the plain structs below
//! and hands them to `tessera_session_send_key`, `tessera_session_send_mouse`,
//! `tessera_session_send_scroll`, `tessera_session_paste` and
//! `tessera_session_set_focus`. The core decides what each event means (an
//! application key binding, bytes for the child's PTY, a scrollback move)
//! and answers with a `TesseraInputOutcome`, so the frontend knows whether
//! to handle an event itself, for instance as a menu key equivalent.
//!
//! # Conventions shared by every input function
//!
//! * **Thread**: call from the host's UI (main) thread only, like every
//!   `tessera_session_*` function except `tessera_session_write`.
//! * **Ownership**: every pointer passed in (the event struct, and the text
//!   it points to) is borrowed for the duration of the call only. The core
//!   copies what it keeps; the caller may free or reuse the memory as soon
//!   as the call returns.
//! * **Enumerations** travel as fixed-width integers (`uint32_t`,
//!   `uint8_t`) holding one of the named `TesseraKey`, `TesseraKeyEventKind`,
//!   `TesseraMouseButton` or `TesseraMouseEventKind` values. Any other
//!   value, invalid UTF-8, an invalid Unicode scalar, or a coordinate or
//!   delta that is not finite makes the whole event malformed: the call
//!   returns `TESSERA_ERROR_INVALID_ARGUMENT` and the session is not
//!   touched.
//! * **Outcome**: `*out` is set to `TESSERA_INPUT_OUTCOME_IGNORED` first, so
//!   it reads "ignored" after any failure.
//! * **Never blocking**: keys and pastes join the session's *pending input*
//!   (at most 16 MiB of input bytes, on top of the PTY's own 1 MiB write
//!   queue; the bound counts payload, not memory, and each queued input
//!   costs about 40 bytes of bookkeeping, so many one-byte keys use more)
//!   and a core thread writes it to the child in order as the child reads. Input that does not fit in the free pending space is
//!   refused whole with `TESSERA_ERROR_IO`; closing the session drops what
//!   is still pending. `tessera_session_write` bypasses the pending input
//!   and may overtake or interleave with it.

use std::ffi::c_char;
use std::slice;
use std::str;

use tessera_app_core::SessionId;
use tessera_app_core::input::{
    FocusEvent, InputEvent, InputOutcome as CoreOutcome, Key as CoreKey, KeyEvent as CoreKeyEvent,
    KeyEventKind as CoreKeyEventKind, Modifiers as CoreModifiers, MouseButton as CoreMouseButton,
    MouseEvent as CoreMouseEvent, MouseEventKind as CoreMouseEventKind,
    ScrollEvent as CoreScrollEvent,
};

use crate::app::App;
use crate::error::{self, Error};
use crate::panic::ffi_guard;

/// A key, independent of the frontend's key codes; the C mirror of the
/// core's key type.
///
/// Values are stable: new keys are only ever appended. A key event names
/// one in `TesseraKeyEvent::key`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A text-only event with no identifiable key, such as an IME commit:
    /// only `text` is sent. Zero, so a zeroed event sends nothing.
    Unidentified = 0,
    /// A key that produces a character; `codepoint` holds its *unshifted*
    /// character in the active layout (`'a'`, not `'A'`; `'4'`, not `'$'`).
    Char = 1,
    /// Escape.
    Escape = 2,
    /// Enter / Return.
    Enter = 3,
    /// Tab.
    Tab = 4,
    /// Backspace.
    Backspace = 5,
    /// Insert.
    Insert = 6,
    /// Delete (forward delete).
    Delete = 7,
    /// Left arrow.
    Left = 8,
    /// Right arrow.
    Right = 9,
    /// Up arrow.
    Up = 10,
    /// Down arrow.
    Down = 11,
    /// Page Up.
    PageUp = 12,
    /// Page Down.
    PageDown = 13,
    /// Home.
    Home = 14,
    /// End.
    End = 15,
    /// Caps Lock.
    CapsLock = 16,
    /// Scroll Lock.
    ScrollLock = 17,
    /// Num Lock.
    NumLock = 18,
    /// Print Screen.
    PrintScreen = 19,
    /// Pause.
    Pause = 20,
    /// Menu.
    Menu = 21,
    /// Function key F1.
    F1 = 22,
    /// Function key F2.
    F2 = 23,
    /// Function key F3.
    F3 = 24,
    /// Function key F4.
    F4 = 25,
    /// Function key F5.
    F5 = 26,
    /// Function key F6.
    F6 = 27,
    /// Function key F7.
    F7 = 28,
    /// Function key F8.
    F8 = 29,
    /// Function key F9.
    F9 = 30,
    /// Function key F10.
    F10 = 31,
    /// Function key F11.
    F11 = 32,
    /// Function key F12.
    F12 = 33,
    /// Function key F13.
    F13 = 34,
    /// Function key F14.
    F14 = 35,
    /// Function key F15.
    F15 = 36,
    /// Function key F16.
    F16 = 37,
    /// Function key F17.
    F17 = 38,
    /// Function key F18.
    F18 = 39,
    /// Function key F19.
    F19 = 40,
    /// Function key F20.
    F20 = 41,
    /// Function key F21.
    F21 = 42,
    /// Function key F22.
    F22 = 43,
    /// Function key F23.
    F23 = 44,
    /// Function key F24.
    F24 = 45,
    /// Function key F25.
    F25 = 46,
    /// Function key F26.
    F26 = 47,
    /// Function key F27.
    F27 = 48,
    /// Function key F28.
    F28 = 49,
    /// Function key F29.
    F29 = 50,
    /// Function key F30.
    F30 = 51,
    /// Function key F31.
    F31 = 52,
    /// Function key F32.
    F32 = 53,
    /// Function key F33.
    F33 = 54,
    /// Function key F34.
    F34 = 55,
    /// Function key F35.
    F35 = 56,
    /// Keypad 0.
    Kp0 = 57,
    /// Keypad 1.
    Kp1 = 58,
    /// Keypad 2.
    Kp2 = 59,
    /// Keypad 3.
    Kp3 = 60,
    /// Keypad 4.
    Kp4 = 61,
    /// Keypad 5.
    Kp5 = 62,
    /// Keypad 6.
    Kp6 = 63,
    /// Keypad 7.
    Kp7 = 64,
    /// Keypad 8.
    Kp8 = 65,
    /// Keypad 9.
    Kp9 = 66,
    /// Keypad decimal point.
    KpDecimal = 67,
    /// Keypad `/`.
    KpDivide = 68,
    /// Keypad `*`.
    KpMultiply = 69,
    /// Keypad `-`.
    KpSubtract = 70,
    /// Keypad `+`.
    KpAdd = 71,
    /// Keypad Enter.
    KpEnter = 72,
    /// Keypad `=`.
    KpEqual = 73,
    /// Keypad `,`.
    KpSeparator = 74,
    /// Keypad Left (Num Lock off).
    KpLeft = 75,
    /// Keypad Right (Num Lock off).
    KpRight = 76,
    /// Keypad Up (Num Lock off).
    KpUp = 77,
    /// Keypad Down (Num Lock off).
    KpDown = 78,
    /// Keypad Page Up (Num Lock off).
    KpPageUp = 79,
    /// Keypad Page Down (Num Lock off).
    KpPageDown = 80,
    /// Keypad Home (Num Lock off).
    KpHome = 81,
    /// Keypad End (Num Lock off).
    KpEnd = 82,
    /// Keypad Insert (Num Lock off).
    KpInsert = 83,
    /// Keypad Delete (Num Lock off).
    KpDelete = 84,
    /// Keypad Begin (the `5` key with Num Lock off).
    KpBegin = 85,
    /// Media: play.
    MediaPlay = 86,
    /// Media: pause.
    MediaPause = 87,
    /// Media: play/pause toggle.
    MediaPlayPause = 88,
    /// Media: reverse.
    MediaReverse = 89,
    /// Media: stop.
    MediaStop = 90,
    /// Media: fast forward.
    MediaFastForward = 91,
    /// Media: rewind.
    MediaRewind = 92,
    /// Media: next track.
    MediaTrackNext = 93,
    /// Media: previous track.
    MediaTrackPrevious = 94,
    /// Media: record.
    MediaRecord = 95,
    /// Volume down.
    LowerVolume = 96,
    /// Volume up.
    RaiseVolume = 97,
    /// Mute.
    MuteVolume = 98,
    /// Left Shift.
    LeftShift = 99,
    /// Left Control.
    LeftControl = 100,
    /// Left Alt.
    LeftAlt = 101,
    /// Left Super.
    LeftSuper = 102,
    /// Left Hyper.
    LeftHyper = 103,
    /// Left Meta.
    LeftMeta = 104,
    /// Right Shift.
    RightShift = 105,
    /// Right Control.
    RightControl = 106,
    /// Right Alt.
    RightAlt = 107,
    /// Right Super.
    RightSuper = 108,
    /// Right Hyper.
    RightHyper = 109,
    /// Right Meta.
    RightMeta = 110,
    /// ISO Level 3 Shift (`AltGr`).
    IsoLevel3Shift = 111,
    /// ISO Level 5 Shift.
    IsoLevel5Shift = 112,
}

impl Key {
    /// Number of keys; every discriminant is below it.
    ///
    /// cbindgen:ignore
    pub const COUNT: u32 = 113;

    /// The key with discriminant `raw`, or `None` when no key has it.
    #[must_use]
    #[expect(clippy::too_many_lines, reason = "one arm per key of the C enum")]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::Unidentified,
            1 => Self::Char,
            2 => Self::Escape,
            3 => Self::Enter,
            4 => Self::Tab,
            5 => Self::Backspace,
            6 => Self::Insert,
            7 => Self::Delete,
            8 => Self::Left,
            9 => Self::Right,
            10 => Self::Up,
            11 => Self::Down,
            12 => Self::PageUp,
            13 => Self::PageDown,
            14 => Self::Home,
            15 => Self::End,
            16 => Self::CapsLock,
            17 => Self::ScrollLock,
            18 => Self::NumLock,
            19 => Self::PrintScreen,
            20 => Self::Pause,
            21 => Self::Menu,
            22 => Self::F1,
            23 => Self::F2,
            24 => Self::F3,
            25 => Self::F4,
            26 => Self::F5,
            27 => Self::F6,
            28 => Self::F7,
            29 => Self::F8,
            30 => Self::F9,
            31 => Self::F10,
            32 => Self::F11,
            33 => Self::F12,
            34 => Self::F13,
            35 => Self::F14,
            36 => Self::F15,
            37 => Self::F16,
            38 => Self::F17,
            39 => Self::F18,
            40 => Self::F19,
            41 => Self::F20,
            42 => Self::F21,
            43 => Self::F22,
            44 => Self::F23,
            45 => Self::F24,
            46 => Self::F25,
            47 => Self::F26,
            48 => Self::F27,
            49 => Self::F28,
            50 => Self::F29,
            51 => Self::F30,
            52 => Self::F31,
            53 => Self::F32,
            54 => Self::F33,
            55 => Self::F34,
            56 => Self::F35,
            57 => Self::Kp0,
            58 => Self::Kp1,
            59 => Self::Kp2,
            60 => Self::Kp3,
            61 => Self::Kp4,
            62 => Self::Kp5,
            63 => Self::Kp6,
            64 => Self::Kp7,
            65 => Self::Kp8,
            66 => Self::Kp9,
            67 => Self::KpDecimal,
            68 => Self::KpDivide,
            69 => Self::KpMultiply,
            70 => Self::KpSubtract,
            71 => Self::KpAdd,
            72 => Self::KpEnter,
            73 => Self::KpEqual,
            74 => Self::KpSeparator,
            75 => Self::KpLeft,
            76 => Self::KpRight,
            77 => Self::KpUp,
            78 => Self::KpDown,
            79 => Self::KpPageUp,
            80 => Self::KpPageDown,
            81 => Self::KpHome,
            82 => Self::KpEnd,
            83 => Self::KpInsert,
            84 => Self::KpDelete,
            85 => Self::KpBegin,
            86 => Self::MediaPlay,
            87 => Self::MediaPause,
            88 => Self::MediaPlayPause,
            89 => Self::MediaReverse,
            90 => Self::MediaStop,
            91 => Self::MediaFastForward,
            92 => Self::MediaRewind,
            93 => Self::MediaTrackNext,
            94 => Self::MediaTrackPrevious,
            95 => Self::MediaRecord,
            96 => Self::LowerVolume,
            97 => Self::RaiseVolume,
            98 => Self::MuteVolume,
            99 => Self::LeftShift,
            100 => Self::LeftControl,
            101 => Self::LeftAlt,
            102 => Self::LeftSuper,
            103 => Self::LeftHyper,
            104 => Self::LeftMeta,
            105 => Self::RightShift,
            106 => Self::RightControl,
            107 => Self::RightAlt,
            108 => Self::RightSuper,
            109 => Self::RightHyper,
            110 => Self::RightMeta,
            111 => Self::IsoLevel3Shift,
            112 => Self::IsoLevel5Shift,
            _ => return None,
        })
    }

    /// The core's key; `codepoint` is the character of [`Key::Char`] and is
    /// ignored for every other key.
    #[must_use]
    #[expect(clippy::too_many_lines, reason = "one arm per key of the C enum")]
    pub const fn to_core(self, codepoint: char) -> CoreKey {
        match self {
            Self::Unidentified => CoreKey::Unidentified,
            Self::Char => CoreKey::Char(codepoint),
            Self::Escape => CoreKey::Escape,
            Self::Enter => CoreKey::Enter,
            Self::Tab => CoreKey::Tab,
            Self::Backspace => CoreKey::Backspace,
            Self::Insert => CoreKey::Insert,
            Self::Delete => CoreKey::Delete,
            Self::Left => CoreKey::Left,
            Self::Right => CoreKey::Right,
            Self::Up => CoreKey::Up,
            Self::Down => CoreKey::Down,
            Self::PageUp => CoreKey::PageUp,
            Self::PageDown => CoreKey::PageDown,
            Self::Home => CoreKey::Home,
            Self::End => CoreKey::End,
            Self::CapsLock => CoreKey::CapsLock,
            Self::ScrollLock => CoreKey::ScrollLock,
            Self::NumLock => CoreKey::NumLock,
            Self::PrintScreen => CoreKey::PrintScreen,
            Self::Pause => CoreKey::Pause,
            Self::Menu => CoreKey::Menu,
            Self::F1 => CoreKey::F(1),
            Self::F2 => CoreKey::F(2),
            Self::F3 => CoreKey::F(3),
            Self::F4 => CoreKey::F(4),
            Self::F5 => CoreKey::F(5),
            Self::F6 => CoreKey::F(6),
            Self::F7 => CoreKey::F(7),
            Self::F8 => CoreKey::F(8),
            Self::F9 => CoreKey::F(9),
            Self::F10 => CoreKey::F(10),
            Self::F11 => CoreKey::F(11),
            Self::F12 => CoreKey::F(12),
            Self::F13 => CoreKey::F(13),
            Self::F14 => CoreKey::F(14),
            Self::F15 => CoreKey::F(15),
            Self::F16 => CoreKey::F(16),
            Self::F17 => CoreKey::F(17),
            Self::F18 => CoreKey::F(18),
            Self::F19 => CoreKey::F(19),
            Self::F20 => CoreKey::F(20),
            Self::F21 => CoreKey::F(21),
            Self::F22 => CoreKey::F(22),
            Self::F23 => CoreKey::F(23),
            Self::F24 => CoreKey::F(24),
            Self::F25 => CoreKey::F(25),
            Self::F26 => CoreKey::F(26),
            Self::F27 => CoreKey::F(27),
            Self::F28 => CoreKey::F(28),
            Self::F29 => CoreKey::F(29),
            Self::F30 => CoreKey::F(30),
            Self::F31 => CoreKey::F(31),
            Self::F32 => CoreKey::F(32),
            Self::F33 => CoreKey::F(33),
            Self::F34 => CoreKey::F(34),
            Self::F35 => CoreKey::F(35),
            Self::Kp0 => CoreKey::Kp0,
            Self::Kp1 => CoreKey::Kp1,
            Self::Kp2 => CoreKey::Kp2,
            Self::Kp3 => CoreKey::Kp3,
            Self::Kp4 => CoreKey::Kp4,
            Self::Kp5 => CoreKey::Kp5,
            Self::Kp6 => CoreKey::Kp6,
            Self::Kp7 => CoreKey::Kp7,
            Self::Kp8 => CoreKey::Kp8,
            Self::Kp9 => CoreKey::Kp9,
            Self::KpDecimal => CoreKey::KpDecimal,
            Self::KpDivide => CoreKey::KpDivide,
            Self::KpMultiply => CoreKey::KpMultiply,
            Self::KpSubtract => CoreKey::KpSubtract,
            Self::KpAdd => CoreKey::KpAdd,
            Self::KpEnter => CoreKey::KpEnter,
            Self::KpEqual => CoreKey::KpEqual,
            Self::KpSeparator => CoreKey::KpSeparator,
            Self::KpLeft => CoreKey::KpLeft,
            Self::KpRight => CoreKey::KpRight,
            Self::KpUp => CoreKey::KpUp,
            Self::KpDown => CoreKey::KpDown,
            Self::KpPageUp => CoreKey::KpPageUp,
            Self::KpPageDown => CoreKey::KpPageDown,
            Self::KpHome => CoreKey::KpHome,
            Self::KpEnd => CoreKey::KpEnd,
            Self::KpInsert => CoreKey::KpInsert,
            Self::KpDelete => CoreKey::KpDelete,
            Self::KpBegin => CoreKey::KpBegin,
            Self::MediaPlay => CoreKey::MediaPlay,
            Self::MediaPause => CoreKey::MediaPause,
            Self::MediaPlayPause => CoreKey::MediaPlayPause,
            Self::MediaReverse => CoreKey::MediaReverse,
            Self::MediaStop => CoreKey::MediaStop,
            Self::MediaFastForward => CoreKey::MediaFastForward,
            Self::MediaRewind => CoreKey::MediaRewind,
            Self::MediaTrackNext => CoreKey::MediaTrackNext,
            Self::MediaTrackPrevious => CoreKey::MediaTrackPrevious,
            Self::MediaRecord => CoreKey::MediaRecord,
            Self::LowerVolume => CoreKey::LowerVolume,
            Self::RaiseVolume => CoreKey::RaiseVolume,
            Self::MuteVolume => CoreKey::MuteVolume,
            Self::LeftShift => CoreKey::LeftShift,
            Self::LeftControl => CoreKey::LeftControl,
            Self::LeftAlt => CoreKey::LeftAlt,
            Self::LeftSuper => CoreKey::LeftSuper,
            Self::LeftHyper => CoreKey::LeftHyper,
            Self::LeftMeta => CoreKey::LeftMeta,
            Self::RightShift => CoreKey::RightShift,
            Self::RightControl => CoreKey::RightControl,
            Self::RightAlt => CoreKey::RightAlt,
            Self::RightSuper => CoreKey::RightSuper,
            Self::RightHyper => CoreKey::RightHyper,
            Self::RightMeta => CoreKey::RightMeta,
            Self::IsoLevel3Shift => CoreKey::IsoLevel3Shift,
            Self::IsoLevel5Shift => CoreKey::IsoLevel5Shift,
        }
    }
}

impl From<CoreKey> for Key {
    /// The C mirror of `key`. A key the C enum cannot name (a function key
    /// beyond F35, or a key added to the core but not yet here) becomes
    /// [`Key::Unidentified`].
    #[expect(clippy::too_many_lines, reason = "one arm per key of the C enum")]
    fn from(key: CoreKey) -> Self {
        match key {
            CoreKey::Char(_) => Self::Char,
            CoreKey::Escape => Self::Escape,
            CoreKey::Enter => Self::Enter,
            CoreKey::Tab => Self::Tab,
            CoreKey::Backspace => Self::Backspace,
            CoreKey::Insert => Self::Insert,
            CoreKey::Delete => Self::Delete,
            CoreKey::Left => Self::Left,
            CoreKey::Right => Self::Right,
            CoreKey::Up => Self::Up,
            CoreKey::Down => Self::Down,
            CoreKey::PageUp => Self::PageUp,
            CoreKey::PageDown => Self::PageDown,
            CoreKey::Home => Self::Home,
            CoreKey::End => Self::End,
            CoreKey::CapsLock => Self::CapsLock,
            CoreKey::ScrollLock => Self::ScrollLock,
            CoreKey::NumLock => Self::NumLock,
            CoreKey::PrintScreen => Self::PrintScreen,
            CoreKey::Pause => Self::Pause,
            CoreKey::Menu => Self::Menu,
            CoreKey::F(1) => Self::F1,
            CoreKey::F(2) => Self::F2,
            CoreKey::F(3) => Self::F3,
            CoreKey::F(4) => Self::F4,
            CoreKey::F(5) => Self::F5,
            CoreKey::F(6) => Self::F6,
            CoreKey::F(7) => Self::F7,
            CoreKey::F(8) => Self::F8,
            CoreKey::F(9) => Self::F9,
            CoreKey::F(10) => Self::F10,
            CoreKey::F(11) => Self::F11,
            CoreKey::F(12) => Self::F12,
            CoreKey::F(13) => Self::F13,
            CoreKey::F(14) => Self::F14,
            CoreKey::F(15) => Self::F15,
            CoreKey::F(16) => Self::F16,
            CoreKey::F(17) => Self::F17,
            CoreKey::F(18) => Self::F18,
            CoreKey::F(19) => Self::F19,
            CoreKey::F(20) => Self::F20,
            CoreKey::F(21) => Self::F21,
            CoreKey::F(22) => Self::F22,
            CoreKey::F(23) => Self::F23,
            CoreKey::F(24) => Self::F24,
            CoreKey::F(25) => Self::F25,
            CoreKey::F(26) => Self::F26,
            CoreKey::F(27) => Self::F27,
            CoreKey::F(28) => Self::F28,
            CoreKey::F(29) => Self::F29,
            CoreKey::F(30) => Self::F30,
            CoreKey::F(31) => Self::F31,
            CoreKey::F(32) => Self::F32,
            CoreKey::F(33) => Self::F33,
            CoreKey::F(34) => Self::F34,
            CoreKey::F(35) => Self::F35,
            CoreKey::Kp0 => Self::Kp0,
            CoreKey::Kp1 => Self::Kp1,
            CoreKey::Kp2 => Self::Kp2,
            CoreKey::Kp3 => Self::Kp3,
            CoreKey::Kp4 => Self::Kp4,
            CoreKey::Kp5 => Self::Kp5,
            CoreKey::Kp6 => Self::Kp6,
            CoreKey::Kp7 => Self::Kp7,
            CoreKey::Kp8 => Self::Kp8,
            CoreKey::Kp9 => Self::Kp9,
            CoreKey::KpDecimal => Self::KpDecimal,
            CoreKey::KpDivide => Self::KpDivide,
            CoreKey::KpMultiply => Self::KpMultiply,
            CoreKey::KpSubtract => Self::KpSubtract,
            CoreKey::KpAdd => Self::KpAdd,
            CoreKey::KpEnter => Self::KpEnter,
            CoreKey::KpEqual => Self::KpEqual,
            CoreKey::KpSeparator => Self::KpSeparator,
            CoreKey::KpLeft => Self::KpLeft,
            CoreKey::KpRight => Self::KpRight,
            CoreKey::KpUp => Self::KpUp,
            CoreKey::KpDown => Self::KpDown,
            CoreKey::KpPageUp => Self::KpPageUp,
            CoreKey::KpPageDown => Self::KpPageDown,
            CoreKey::KpHome => Self::KpHome,
            CoreKey::KpEnd => Self::KpEnd,
            CoreKey::KpInsert => Self::KpInsert,
            CoreKey::KpDelete => Self::KpDelete,
            CoreKey::KpBegin => Self::KpBegin,
            CoreKey::MediaPlay => Self::MediaPlay,
            CoreKey::MediaPause => Self::MediaPause,
            CoreKey::MediaPlayPause => Self::MediaPlayPause,
            CoreKey::MediaReverse => Self::MediaReverse,
            CoreKey::MediaStop => Self::MediaStop,
            CoreKey::MediaFastForward => Self::MediaFastForward,
            CoreKey::MediaRewind => Self::MediaRewind,
            CoreKey::MediaTrackNext => Self::MediaTrackNext,
            CoreKey::MediaTrackPrevious => Self::MediaTrackPrevious,
            CoreKey::MediaRecord => Self::MediaRecord,
            CoreKey::LowerVolume => Self::LowerVolume,
            CoreKey::RaiseVolume => Self::RaiseVolume,
            CoreKey::MuteVolume => Self::MuteVolume,
            CoreKey::LeftShift => Self::LeftShift,
            CoreKey::LeftControl => Self::LeftControl,
            CoreKey::LeftAlt => Self::LeftAlt,
            CoreKey::LeftSuper => Self::LeftSuper,
            CoreKey::LeftHyper => Self::LeftHyper,
            CoreKey::LeftMeta => Self::LeftMeta,
            CoreKey::RightShift => Self::RightShift,
            CoreKey::RightControl => Self::RightControl,
            CoreKey::RightAlt => Self::RightAlt,
            CoreKey::RightSuper => Self::RightSuper,
            CoreKey::RightHyper => Self::RightHyper,
            CoreKey::RightMeta => Self::RightMeta,
            CoreKey::IsoLevel3Shift => Self::IsoLevel3Shift,
            CoreKey::IsoLevel5Shift => Self::IsoLevel5Shift,
            // `Unidentified` itself, function keys beyond F35 and keys the
            // core gains before this enum does.
            _ => Self::Unidentified,
        }
    }
}

/// Modifier keys, as bits of a `uint8_t` (`TesseraKeyEvent::modifiers`,
/// `TesseraMouseEvent::modifiers`, `TesseraScrollEvent::modifiers`). Combine
/// with bitwise OR; every bit has a meaning, so any `uint8_t` is valid.
///
/// The values are those of the kitty keyboard protocol.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modifiers {
    /// No modifier.
    None = 0,
    /// Shift.
    Shift = 1,
    /// Alt (Option on Apple keyboards). Pass it to have the key sent with an
    /// `ESC` prefix; leave it out and pass the composed text to send what
    /// Option produced instead.
    Alt = 2,
    /// Control.
    Ctrl = 4,
    /// Super (Command on Apple keyboards).
    Super = 8,
    /// Hyper.
    Hyper = 16,
    /// Meta.
    Meta = 32,
    /// Caps Lock is engaged.
    CapsLock = 64,
    /// Num Lock is engaged.
    NumLock = 128,
}

/// What happened to a key (`TesseraKeyEvent::kind`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyEventKind {
    /// The key went down.
    Press = 0,
    /// The key is held and auto-repeating.
    Repeat = 1,
    /// The key went up. Sends nothing unless the child asked for key
    /// release events.
    Release = 2,
}

impl KeyEventKind {
    /// The kind with discriminant `raw`, or `None` when no kind has it.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::Press,
            1 => Self::Repeat,
            2 => Self::Release,
            _ => return None,
        })
    }
}

impl From<KeyEventKind> for CoreKeyEventKind {
    fn from(kind: KeyEventKind) -> Self {
        match kind {
            KeyEventKind::Press => Self::Press,
            KeyEventKind::Repeat => Self::Repeat,
            KeyEventKind::Release => Self::Release,
        }
    }
}

impl From<CoreKeyEventKind> for KeyEventKind {
    fn from(kind: CoreKeyEventKind) -> Self {
        match kind {
            CoreKeyEventKind::Press => Self::Press,
            CoreKeyEventKind::Repeat => Self::Repeat,
            CoreKeyEventKind::Release => Self::Release,
        }
    }
}

/// A mouse button (`TesseraMouseEvent::button`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// No button: plain motion with every button up.
    None = 0,
    /// The primary (left) button.
    Left = 1,
    /// The middle button.
    Middle = 2,
    /// The secondary (right) button.
    Right = 3,
    /// The "back" side button.
    Back = 4,
    /// The "forward" side button.
    Forward = 5,
}

impl MouseButton {
    /// The button with discriminant `raw`, or `None` when no button has it.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::None,
            1 => Self::Left,
            2 => Self::Middle,
            3 => Self::Right,
            4 => Self::Back,
            5 => Self::Forward,
            _ => return None,
        })
    }
}

impl From<MouseButton> for CoreMouseButton {
    fn from(button: MouseButton) -> Self {
        match button {
            MouseButton::None => Self::None,
            MouseButton::Left => Self::Left,
            MouseButton::Middle => Self::Middle,
            MouseButton::Right => Self::Right,
            MouseButton::Back => Self::Back,
            MouseButton::Forward => Self::Forward,
        }
    }
}

impl From<CoreMouseButton> for MouseButton {
    /// The C mirror of `button`; a button the C enum cannot name yet
    /// becomes [`MouseButton::None`].
    fn from(button: CoreMouseButton) -> Self {
        match button {
            CoreMouseButton::Left => Self::Left,
            CoreMouseButton::Middle => Self::Middle,
            CoreMouseButton::Right => Self::Right,
            CoreMouseButton::Back => Self::Back,
            CoreMouseButton::Forward => Self::Forward,
            _ => Self::None,
        }
    }
}

/// What happened to the mouse (`TesseraMouseEvent::kind`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseEventKind {
    /// A button went down.
    Press = 0,
    /// A button went up.
    Release = 1,
    /// The pointer moved, with `button` held (a drag) or
    /// `TESSERA_MOUSE_BUTTON_NONE`.
    Motion = 2,
}

impl MouseEventKind {
    /// The kind with discriminant `raw`, or `None` when no kind has it.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::Press,
            1 => Self::Release,
            2 => Self::Motion,
            _ => return None,
        })
    }
}

impl From<MouseEventKind> for CoreMouseEventKind {
    fn from(kind: MouseEventKind) -> Self {
        match kind {
            MouseEventKind::Press => Self::Press,
            MouseEventKind::Release => Self::Release,
            MouseEventKind::Motion => Self::Motion,
        }
    }
}

impl From<CoreMouseEventKind> for MouseEventKind {
    /// The C mirror of `kind`; a kind the C enum cannot name yet becomes
    /// [`MouseEventKind::Motion`].
    fn from(kind: CoreMouseEventKind) -> Self {
        match kind {
            CoreMouseEventKind::Press => Self::Press,
            CoreMouseEventKind::Release => Self::Release,
            _ => Self::Motion,
        }
    }
}

/// Whether the core used an input event, reported through the `out`
/// parameter of the `tessera_session_send_*` functions.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputOutcome {
    /// The core had no use for the event; the frontend may handle it itself
    /// (for example as a menu key equivalent). Also what `*out` holds after
    /// a failed call.
    Ignored = 0,
    /// The core acted on the event; the frontend must not handle it again.
    Consumed = 1,
}

impl From<CoreOutcome> for InputOutcome {
    fn from(outcome: CoreOutcome) -> Self {
        match outcome {
            CoreOutcome::Consumed => Self::Consumed,
            CoreOutcome::Ignored => Self::Ignored,
        }
    }
}

/// A keyboard event for `tessera_session_send_key`.
///
/// Ownership: the struct and the `text` bytes are borrowed for the duration
/// of the call only; the core copies what it needs and keeps no pointer.
///
/// The frontend supplies the *unshifted* key, the modifier state and, when
/// the OS composed text for the event (keyboard layout, dead keys, IME
/// commit), that text. Composed text takes precedence over the character of
/// the key when the event is sent as text.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    /// One of the `TesseraKey` values.
    pub key: u32,
    /// For `TESSERA_KEY_CHAR`: the key's unshifted character as a Unicode
    /// scalar value (`'a'` for the A key, also under Shift). Ignored for
    /// every other key; pass 0.
    pub codepoint: u32,
    /// The key's shifted character in the active layout (`'A'` for `'a'`,
    /// `'$'` for `'4'`) as a Unicode scalar value, or 0 when unknown (a US
    /// layout is then assumed).
    pub shifted: u32,
    /// The key at the same physical position in the US PC-101 layout (`'c'`
    /// when a Cyrillic layout produced `'с'`) as a Unicode scalar value, or
    /// 0 when unknown or the same as the key. Lets Ctrl and Alt combinations
    /// work on non-Latin layouts.
    pub base_layout: u32,
    /// Bitwise OR of `TesseraModifiers` values held during the event,
    /// including the lock state.
    pub modifiers: u8,
    /// One of the `TesseraKeyEventKind` values.
    pub kind: u32,
    /// Text the OS composed for the event, as UTF-8 that is *not*
    /// NUL-terminated: `text_len` bytes are read. Borrowed for the call
    /// only. May be `NULL` only when `text_len` is 0, which means "no
    /// text". Text containing a control character is ignored: a
    /// `TESSERA_KEY_CHAR` event then sends its `codepoint` instead (`text`
    /// `"\x1b[31m"` with `'a'` sends `a`), any other key sends what it
    /// sends without text, and only a `TESSERA_KEY_UNIDENTIFIED` event
    /// sends nothing.
    pub text: *const c_char,
    /// Length of `text` in bytes.
    pub text_len: usize,
}

/// A mouse event for `tessera_session_send_mouse`.
///
/// Ownership: borrowed for the duration of the call only; the struct holds
/// no pointers.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MouseEvent {
    /// One of the `TesseraMouseEventKind` values.
    pub kind: u32,
    /// One of the `TesseraMouseButton` values: the button that changed
    /// (press, release) or is held (motion).
    pub button: u32,
    /// Zero-based column of the cell under the pointer. Values beyond the
    /// grid are clamped to its last column.
    pub col: u16,
    /// Zero-based row of the cell under the pointer, from the top of the
    /// visible screen. Values beyond the grid are clamped to its last row.
    pub row: u16,
    /// Pointer position in pixels from the view's left edge; may be
    /// negative or beyond the view during a drag. Must be finite.
    pub x_px: f32,
    /// Pointer position in pixels from the view's top edge. Must be finite.
    pub y_px: f32,
    /// Bitwise OR of `TesseraModifiers` values held during the event.
    pub modifiers: u8,
}

/// A scroll gesture for `tessera_session_send_scroll`.
///
/// Ownership: borrowed for the duration of the call only; the struct holds
/// no pointers.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ScrollEvent {
    /// Distance in lines; positive moves the view towards older output
    /// (into the scrollback), negative towards the live screen. Fractions
    /// accumulate across calls, so a trackpad can report small steps. Must
    /// be finite.
    pub delta_lines: f32,
    /// Horizontal distance in pixels as measured by the frontend, kept for
    /// pixel-precise consumers. Must be finite; 0 when unknown.
    pub delta_x_px: f32,
    /// Vertical distance in pixels as measured by the frontend. Must be
    /// finite; 0 when unknown.
    pub delta_y_px: f32,
    /// 0 for a notched wheel; any other value for a precise device
    /// (trackpad, Magic Mouse).
    pub precise: u8,
    /// Zero-based column of the cell under the pointer.
    pub col: u16,
    /// Zero-based row of the cell under the pointer.
    pub row: u16,
    /// Bitwise OR of `TesseraModifiers` values held during the gesture.
    pub modifiers: u8,
}

/// The Unicode scalar `raw`, rejecting surrogates and values past U+10FFFF.
fn scalar(raw: u32) -> Result<char, Error> {
    char::from_u32(raw).ok_or(Error::InvalidArgument)
}

/// An optional scalar where 0 means "none".
fn optional_scalar(raw: u32) -> Result<Option<char>, Error> {
    if raw == 0 {
        Ok(None)
    } else {
        scalar(raw).map(Some)
    }
}

/// Fails unless every value is finite.
fn finite(values: &[f32]) -> Result<(), Error> {
    if values.iter().all(|value| value.is_finite()) {
        Ok(())
    } else {
        Err(Error::InvalidArgument)
    }
}

/// Borrows `len` bytes at `ptr` as UTF-8.
///
/// Returns `Ok(None)` for no text (`len` 0, whatever `ptr` is),
/// `Err(null)` for a `NULL` pointer with a non-zero length,
/// `Err(InvalidArgument)` for a `len` above `isize::MAX` (no buffer is that
/// large) and for bytes that are not UTF-8.
///
/// # Safety
///
/// When `len` is not 0 and `ptr` is not `NULL`, `ptr` must point to `len`
/// readable bytes that outlive `'a`.
unsafe fn utf8<'a>(ptr: *const c_char, len: usize, null: Error) -> Result<Option<&'a str>, Error> {
    if len == 0 {
        return Ok(None);
    }
    if ptr.is_null() {
        return Err(null);
    }
    // No allocation can be larger, and `slice::from_raw_parts` requires it.
    if isize::try_from(len).is_err() {
        return Err(Error::InvalidArgument);
    }
    // SAFETY: non-null and pointing to `len` readable bytes per the
    // contract.
    let bytes = unsafe { slice::from_raw_parts(ptr.cast::<u8>(), len) };
    str::from_utf8(bytes)
        .map(Some)
        .map_err(|_| Error::InvalidArgument)
}

impl KeyEvent {
    /// Validates the C event and converts it to the core's.
    ///
    /// # Safety
    ///
    /// `text` must satisfy the contract documented on the field.
    pub(crate) unsafe fn to_core(self) -> Result<CoreKeyEvent, Error> {
        let key = Key::from_raw(self.key).ok_or(Error::InvalidArgument)?;
        let kind = KeyEventKind::from_raw(self.kind).ok_or(Error::InvalidArgument)?;
        let codepoint = if key == Key::Char {
            scalar(self.codepoint)?
        } else {
            '\0'
        };
        let shifted = optional_scalar(self.shifted)?;
        let base_layout = optional_scalar(self.base_layout)?;
        // A NULL `text` with a length is a malformed event, not a NULL
        // argument.
        // SAFETY: the caller guarantees the field contract.
        let text = unsafe { utf8(self.text, self.text_len, Error::InvalidArgument) }?;
        let mut event = CoreKeyEvent::new(
            key.to_core(codepoint),
            CoreModifiers::from_bits(self.modifiers),
        )
        .with_kind(kind.into());
        if let Some(shifted) = shifted {
            event = event.with_shifted_key(shifted);
        }
        if let Some(base) = base_layout {
            event = event.with_base_layout_key(base);
        }
        if let Some(text) = text {
            event = event.with_text(text);
        }
        Ok(event)
    }
}

impl MouseEvent {
    /// Validates the C event and converts it to the core's.
    pub(crate) fn to_core(self) -> Result<CoreMouseEvent, Error> {
        let kind = MouseEventKind::from_raw(self.kind).ok_or(Error::InvalidArgument)?;
        let button = MouseButton::from_raw(self.button).ok_or(Error::InvalidArgument)?;
        finite(&[self.x_px, self.y_px])?;
        Ok(
            CoreMouseEvent::new(kind.into(), button.into(), self.col, self.row)
                .with_pixels(self.x_px, self.y_px)
                .with_modifiers(CoreModifiers::from_bits(self.modifiers)),
        )
    }
}

impl ScrollEvent {
    /// Validates the C event and converts it to the core's.
    pub(crate) fn to_core(self) -> Result<CoreScrollEvent, Error> {
        finite(&[self.delta_lines, self.delta_x_px, self.delta_y_px])?;
        Ok(CoreScrollEvent::new(self.delta_lines)
            .with_pixels(self.delta_x_px, self.delta_y_px)
            .with_precise(self.precise != 0)
            .with_cell(self.col, self.row)
            .with_modifiers(CoreModifiers::from_bits(self.modifiers)))
    }
}

/// The body shared by the `tessera_session_send_*` functions: checks `out`
/// and `app`, builds the event with `event` and routes it.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle; `out` must be `NULL` or point to a
/// writable `TesseraInputOutcome`.
unsafe fn send(
    function: &'static str,
    app: *mut App,
    session_id: u64,
    out: *mut InputOutcome,
    event: impl FnOnce() -> Result<InputEvent, Error>,
) -> Error {
    if out.is_null() {
        return Error::NullPointer;
    }
    // SAFETY: non-null and writable per the contract.
    unsafe { out.write(InputOutcome::Ignored) };
    // SAFETY: the caller passes NULL or a live handle.
    let Some(app) = (unsafe { App::from_ptr(app) }) else {
        return Error::NullPointer;
    };
    let event = match event() {
        Ok(event) => event,
        Err(code) => return code,
    };
    match app.core.send_input(SessionId::from_raw(session_id), event) {
        Ok(outcome) => {
            // SAFETY: as above.
            unsafe { out.write(outcome.into()) };
            Error::Ok
        }
        Err(error) => error::report(function, &error),
    }
}

/// Sends a key event to session `session_id` and stores in `*out` whether
/// the core used it.
///
/// A key bound to an application command runs that command (no key is
/// bound yet). Any other key is encoded for the child (xterm and kitty
/// keyboard protocol rules, in the session terminal's keyboard modes:
/// DECCKM, DECKPAM, `modifyOtherKeys` and the kitty flags, see ADR 0020)
/// and queued
/// **without blocking** behind the session's pending input (16 MiB at
/// most; a core thread writes it to the child in order, so a key sent after
/// a paste arrives after the paste), and the session's view returns to the
/// live screen: `TESSERA_INPUT_OUTCOME_CONSUMED`. A key that encodes to
/// nothing (a bare modifier, a release, a control character as text) is
/// `TESSERA_INPUT_OUTCOME_IGNORED`, and the frontend may handle it itself.
///
/// Call from the UI (main) thread only.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app`, `event` or `out` is
/// `NULL`; `TESSERA_ERROR_INVALID_ARGUMENT` if the event is malformed (an
/// unknown `key` or `kind`, a `codepoint`, `shifted` or `base_layout` that
/// is not a Unicode scalar value, `text` that is not UTF-8 or is `NULL`
/// with a non-zero `text_len`); `TESSERA_ERROR_SESSION_NOT_FOUND` if the id
/// is not a live session; `TESSERA_ERROR_IO` if the child has exited, the
/// session is closing, or the key's bytes do not fit in the free space of
/// the session's pending input because the child stopped reading (the key
/// is then dropped whole). On failure
/// nothing is sent and `*out` is `TESSERA_INPUT_OUTCOME_IGNORED` (when
/// `out` is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `event` must be `NULL` or point
/// to a readable `TesseraKeyEvent` whose `text` satisfies the contract on
/// that field; the event is borrowed for the call only. `out` must be
/// `NULL` or point to a writable `TesseraInputOutcome`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_send_key(
    app: *mut App,
    session_id: u64,
    event: *const KeyEvent,
    out: *mut InputOutcome,
) -> Error {
    ffi_guard!("tessera_session_send_key", {
        let event = || {
            // SAFETY: `event` is NULL or readable per the contract.
            let event = unsafe { event.as_ref() }.ok_or(Error::NullPointer)?;
            // SAFETY: `text` satisfies the field contract.
            Ok(InputEvent::Key(unsafe { event.to_core() }?))
        };
        // SAFETY: forwarded caller contract for `app` and `out`.
        unsafe { send("tessera_session_send_key", app, session_id, out, event) }
    })
}

/// Sends a mouse event to session `session_id` and stores in `*out`
/// whether the core used it.
///
/// The event's cell is clamped to the session's grid and recorded as the
/// session's last mouse position; nothing is sent to the child yet (mouse
/// reporting and selection come later), so the outcome is
/// `TESSERA_INPUT_OUTCOME_IGNORED` and the frontend keeps handling the
/// mouse itself.
///
/// Call from the UI (main) thread only.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app`, `event` or `out` is
/// `NULL`; `TESSERA_ERROR_INVALID_ARGUMENT` for an unknown `kind` or
/// `button` or a pixel position that is not finite;
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// failure nothing is recorded and `*out` is
/// `TESSERA_INPUT_OUTCOME_IGNORED` (when `out` is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `event` must be `NULL` or point
/// to a readable `TesseraMouseEvent`, borrowed for the call only. `out`
/// must be `NULL` or point to a writable `TesseraInputOutcome`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_send_mouse(
    app: *mut App,
    session_id: u64,
    event: *const MouseEvent,
    out: *mut InputOutcome,
) -> Error {
    ffi_guard!("tessera_session_send_mouse", {
        let event = || {
            // SAFETY: `event` is NULL or readable per the contract.
            let event = unsafe { event.as_ref() }.ok_or(Error::NullPointer)?;
            Ok(InputEvent::Mouse(event.to_core()?))
        };
        // SAFETY: forwarded caller contract for `app` and `out`.
        unsafe { send("tessera_session_send_mouse", app, session_id, out, event) }
    })
}

/// Sends a scroll gesture to session `session_id` and stores in `*out`
/// whether the core used it.
///
/// Moves the session's view `delta_lines` into the scrollback (or back
/// towards the live screen), accumulating fractions across calls and
/// clamping between the live screen and the oldest scrollback line. The
/// outcome is `TESSERA_INPUT_OUTCOME_CONSUMED` when the view moved by at
/// least one line, `TESSERA_INPUT_OUTCOME_IGNORED` otherwise (including at
/// either end).
///
/// On the alternate screen (a full-screen application), which has no
/// scrollback, the view does not move: each whole line sends a cursor key
/// to the child instead, Up for positive deltas and Down for negative ones
/// (`ESC [ A` / `ESC [ B`, or `ESC O A` / `ESC O B` in application
/// cursor-key mode), at most the screen's height per call, and the outcome
/// is `TESSERA_INPUT_OUTCOME_CONSUMED` when a key was sent. While that
/// application asked for mouse reports the gesture sends nothing and is
/// `TESSERA_INPUT_OUTCOME_IGNORED`. The keys are queued like typed keys and
/// can fail the same way (`TESSERA_ERROR_IO`).
///
/// Call from the UI (main) thread only.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app`, `event` or `out` is
/// `NULL`; `TESSERA_ERROR_INVALID_ARGUMENT` if a delta is not finite;
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// failure the view does not move and `*out` is
/// `TESSERA_INPUT_OUTCOME_IGNORED` (when `out` is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `event` must be `NULL` or point
/// to a readable `TesseraScrollEvent`, borrowed for the call only. `out`
/// must be `NULL` or point to a writable `TesseraInputOutcome`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_send_scroll(
    app: *mut App,
    session_id: u64,
    event: *const ScrollEvent,
    out: *mut InputOutcome,
) -> Error {
    ffi_guard!("tessera_session_send_scroll", {
        let event = || {
            // SAFETY: `event` is NULL or readable per the contract.
            let event = unsafe { event.as_ref() }.ok_or(Error::NullPointer)?;
            Ok(InputEvent::Scroll(event.to_core()?))
        };
        // SAFETY: forwarded caller contract for `app` and `out`.
        unsafe { send("tessera_session_send_scroll", app, session_id, out, event) }
    })
}

/// Pastes `len` bytes of UTF-8 text from `text` into session `session_id`.
///
/// The text is sanitised before it is sent: `"\r\n"` and `"\n"` become
/// `"\r"` (what Return sends); every other C0 control except tab (including
/// `ESC`), `DEL` and every C1 control (U+0080 to U+009F) is removed, so a
/// paste cannot carry escape sequences or signals. The remaining characters
/// are sent unchanged (`"a\nb\x1b[31mc"` sends `"a\rb[31mc"`). When the
/// child turned on bracketed paste (DECSET 2004) the text is wrapped in
/// `ESC [ 200 ~` and `ESC [ 201 ~` and its line breaks are kept as they
/// are (`ESC` is still removed, so the text cannot end the paste itself).
/// The view returns to the live screen. Text that sanitises to nothing
/// sends nothing, brackets included, and succeeds.
///
/// **Never blocks.** The sanitised text joins the session's pending input
/// (16 MiB at most, shared with keys) and a core thread writes it to the
/// child in order as the child reads; keys sent afterwards reach the child
/// after the paste. A paste whose length `len` (before sanitising) does not
/// fit in the free pending space is refused whole with `TESSERA_ERROR_IO`
/// before its bytes are read, validated or copied, so one paste longer than
/// 16 MiB always fails, at once and without allocating. In bracketed-paste
/// mode the 12 bytes of brackets count too: a paste for which `len` fits
/// but `len + 12` does not is refused with `TESSERA_ERROR_IO` after it was
/// validated, still without anything being sent. A paste that fits is
/// copied once. Closing the session drops whatever is still pending.
///
/// Call from the UI (main) thread only.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL` or `text` is
/// `NULL` with a non-zero `len`; `TESSERA_ERROR_INVALID_ARGUMENT` if the
/// bytes are not UTF-8 (nothing is sent); `TESSERA_ERROR_SESSION_NOT_FOUND`
/// if the id is not a live session; `TESSERA_ERROR_IO` if the child has
/// exited, the session is closing or `len` does not fit in the free
/// pending space (nothing is sent). These session errors are reported
/// before the text is checked for UTF-8.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `text` must point to `len`
/// readable bytes (it need not be NUL-terminated; a NUL byte is a C0
/// control and is removed); it may be `NULL` when `len` is 0. The bytes are
/// borrowed for the call only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_paste(
    app: *mut App,
    session_id: u64,
    text: *const c_char,
    len: usize,
) -> Error {
    ffi_guard!("tessera_session_paste", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        let id = SessionId::from_raw(session_id);
        if len > 0 && text.is_null() {
            return Error::NullPointer;
        }
        // Refuse a paste that cannot fit before validating or copying it;
        // sanitising never lengthens text, so the raw length bounds it.
        if len > 0
            && let Err(error) = app.core.check_input_room(id, len)
        {
            return error::report("tessera_session_paste", &error);
        }
        // SAFETY: `text` points to `len` readable bytes per the contract.
        let text = match unsafe { utf8(text, len, Error::NullPointer) } {
            Ok(text) => text.unwrap_or_default(),
            Err(code) => return code,
        };
        // Borrowed: the text is sanitised straight into the queued buffer.
        match app.core.paste(id, text) {
            Ok(_) => Error::Ok,
            Err(error) => error::report("tessera_session_paste", &error),
        }
    })
}

/// Tells session `session_id` whether its view has keyboard focus.
///
/// The core records the state (a new session starts unfocused). When the
/// child turned on focus reporting (DECSET 1004), a change of focus also
/// queues `ESC [ I` (gained) or `ESC [ O` (lost) for it, without blocking;
/// a call repeating the recorded state sends nothing (see ADR 0020).
///
/// Call from the UI (main) thread only: the check and the update of the
/// recorded state are not one atomic step, so calls from two threads could
/// both report the same change.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session,
/// `TESSERA_ERROR_IO` if a focus report could not be queued (the child has
/// exited, the session is closing or its pending input is full); the focus
/// state is recorded all the same.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_set_focus(
    app: *mut App,
    session_id: u64,
    focused: bool,
) -> Error {
    ffi_guard!("tessera_session_set_focus", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        let focus = if focused {
            FocusEvent::Gained
        } else {
            FocusEvent::Lost
        };
        match app
            .core
            .send_input(SessionId::from_raw(session_id), InputEvent::Focus(focus))
        {
            Ok(_) => Error::Ok,
            Err(error) => error::report("tessera_session_set_focus", &error),
        }
    })
}

/// Runs `f` with the core application behind `app`, so tests can observe
/// state the C API does not expose (the screen, the scrollback view,
/// focus). Returns `None` for `NULL`.
///
/// Test-only (feature `test-hooks`), not exported to C and absent from the
/// header.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle that is not freed while `f` runs.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub unsafe fn tessera_test_with_core<R>(
    app: *const App,
    f: impl FnOnce(&tessera_app_core::App) -> R,
) -> Option<R> {
    // SAFETY: guaranteed by the caller.
    unsafe { App::from_ptr(app) }.map(|app| f(&app.core))
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;

    #[test]
    fn modifier_bits_match_the_core() {
        let pairs = [
            (Modifiers::None, CoreModifiers::NONE),
            (Modifiers::Shift, CoreModifiers::SHIFT),
            (Modifiers::Alt, CoreModifiers::ALT),
            (Modifiers::Ctrl, CoreModifiers::CTRL),
            (Modifiers::Super, CoreModifiers::SUPER),
            (Modifiers::Hyper, CoreModifiers::HYPER),
            (Modifiers::Meta, CoreModifiers::META),
            (Modifiers::CapsLock, CoreModifiers::CAPS_LOCK),
            (Modifiers::NumLock, CoreModifiers::NUM_LOCK),
        ];
        for (c, core) in pairs {
            assert_eq!(c as u32, u32::from(core.bits()), "{c:?}");
        }
    }

    #[test]
    fn the_documented_pending_input_limit_is_the_core_s() {
        // The header states 16 MiB; update it together with the core.
        assert_eq!(
            tessera_app_core::session::PENDING_INPUT_BYTES,
            16 * 1024 * 1024
        );
    }

    #[test]
    fn utf8_borrows_text() {
        // SAFETY: NULL with zero length is never read.
        assert_eq!(
            unsafe { utf8(ptr::null(), 0, Error::NullPointer) },
            Ok(None)
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { utf8(ptr::null(), 3, Error::NullPointer) },
            Err(Error::NullPointer)
        );
        let bad = [0xff_u8, 0xfe];
        // SAFETY: `bad` holds two readable bytes.
        assert_eq!(
            unsafe { utf8(bad.as_ptr().cast(), 2, Error::NullPointer) },
            Err(Error::InvalidArgument)
        );
        let huge = usize::MAX;
        // SAFETY: rejected before the pointer is read.
        assert_eq!(
            unsafe {
                utf8(
                    ptr::NonNull::<c_char>::dangling().as_ptr(),
                    huge,
                    Error::NullPointer,
                )
            },
            Err(Error::InvalidArgument)
        );
        // SAFETY: never read; the documented NULL error wins.
        assert_eq!(
            unsafe { utf8(ptr::null(), huge, Error::NullPointer) },
            Err(Error::NullPointer)
        );
        let good = "é!";
        // SAFETY: `good` holds `len` readable bytes.
        assert_eq!(
            unsafe { utf8(good.as_ptr().cast(), good.len(), Error::NullPointer) },
            Ok(Some("é!"))
        );
    }
}
