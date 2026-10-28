//! winit events to [`InputEvent`]s, as pure functions.
//!
//! The development window hands every keyboard, mouse and scroll event to
//! these functions and sends what they return to
//! [`App::send_input`](tessera_app_core::App::send_input), so it drives the
//! same keyboard encoder ([`tessera_app_core::input`]) the FFI layer does.
//! Nothing here touches a window, so every mapping is unit tested.
//!
//! winit's [`KeyEvent`](winit::event::KeyEvent) cannot be built outside
//! winit (it carries private platform fields), so key mapping takes a
//! [`KeyInput`], which holds exactly the fields it reads and is built from a
//! real event with [`KeyInput::from_event`].
//!
//! # Policies
//!
//! * On macOS the left Option key is Alt: Option+`b` sends `ESC b`
//!   (readline's backward-word), not `∫`. The window asks winit for
//!   option-as-alt on the left key, so winit reports the key with Shift
//!   applied and no Option composition (Option+Shift+`,` is `<` on a US
//!   layout, `;` on a German one), which is the shifted key the encoder
//!   sends. The right Option key composes, so layouts that type `@ [ ] { }
//!   | \ $` with Option (Nordic, German) still can: winit still reports
//!   ALT for it, and [`effective_modifiers`] removes it (for the mouse
//!   too). A right-Option chord on a typing key sends the composed text
//!   (winit's `text_with_all_modifiers`) or nothing; on a key that does not
//!   type (arrows) it is Alt.
//! * A dead-key press sends nothing; the next key sends what macOS composed
//!   (winit's `text_with_all_modifiers`): `´` then `e` is `é`, `^` then
//!   Space is `^`. Under Ctrl or Alt a dead key is an ordinary key. That
//!   macOS composes across events without an IME is not yet confirmed on
//!   real typing (CONTRIBUTING lists the manual check).
//! * There is no IME, so no CJK or other input-method input. Allowing
//!   winit's IME routes every key through `interpretKeyEvents`, where
//!   macOS's press-and-hold accent menu replaces key repeat for held
//!   letters; kitty and `WezTerm` turn `ApplePressAndHoldEnabled` off for
//!   that, which needs Objective-C calls this crate may not make. Composed
//!   input belongs to the app's terminal view (#27).
//! * Lock keys (Caps, Num, Scroll Lock, and the Mac keypad's Clear,
//!   which winit reports as Num Lock) send nothing; the encoder would send
//!   them as `CSI u` in legacy mode.
//! * Known limit: with the left Option as Meta, a US key whose Option layer
//!   is a dead key (`M-u`, `M-e`, `M-n`, `M-i`, `` M-` ``) may leave a
//!   composition pending in the window server, so the next key can come
//!   out accented.
//! * Remapped modifiers (Caps Lock as Option, Karabiner) may not set the
//!   device-side bits that tell left from right Option. winit then does
//!   not rewrite the event, so Option composes while Alt is kept:
//!   Option+Shift+`,` sends `ESC` followed by the composed `¯`.
//! * Cmd+V (Ctrl+Shift+V on other platforms) is a paste request
//!   ([`KeyAction::Paste`]); the caller reads the clipboard. On a non-Latin
//!   layout the key at the V position counts (see `is_v_key`).
//! * On macOS every other Cmd chord is dropped: Cmd belongs to the
//!   application there, and the core binds no keys yet (#43). Elsewhere
//!   Super is an ordinary modifier.
//! * Every other key goes to the encoder, which drops what it has no
//!   encoding for (bare modifiers, releases in legacy mode).
//! * Under Alt and Ctrl the OS text is not sent; the encoder derives what
//!   these chords send from the key and its shifted key.
//! * winit 0.30's `ModifiersState` has no Caps Lock or Num Lock, so
//!   [`Modifiers::CAPS_LOCK`] and [`Modifiers::NUM_LOCK`] are never set;
//!   the OS text still carries Caps Lock's effect on letters.
//! * Pointer positions are physical pixels from the window's top-left
//!   corner, and cells are counted from the grid's origin inside the
//!   renderer's padding.

use std::collections::VecDeque;

use tessera_app_core::input::{
    FocusEvent, InputEvent, Key, KeyEvent, KeyEventKind, Modifiers, MouseButton, MouseEvent,
    MouseEventKind, ScrollEvent,
};
use tessera_app_core::view::CellMetrics;
use winit::event::{ElementState, MouseScrollDelta};
use winit::keyboard::{
    Key as WinitKey, KeyCode, KeyLocation, ModifiersState, NamedKey, PhysicalKey, SmolStr,
};

/// Which platform's conventions decide the paste chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Cmd+V pastes.
    MacOs,
    /// Ctrl+Shift+V pastes (Linux and other X11/Wayland desktops).
    Other,
}

impl Platform {
    /// The platform this binary was built for.
    pub const CURRENT: Self = if cfg!(target_os = "macos") {
        Self::MacOs
    } else {
        Self::Other
    };

    /// The modifiers that, with the `v` key, request a paste.
    const fn paste_modifiers(self) -> Modifiers {
        match self {
            Self::MacOs => Modifiers::SUPER,
            Self::Other => Modifiers::from_bits(Modifiers::CTRL.bits() | Modifiers::SHIFT.bits()),
        }
    }
}

/// The fields of a winit [`KeyEvent`](winit::event::KeyEvent) that key
/// mapping reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInput {
    /// The key with the active modifiers applied (`"A"` under Shift).
    pub logical: WinitKey,
    /// The key with no modifier applied (`"a"` under Shift), winit's
    /// `key_without_modifiers`.
    pub unmodified: WinitKey,
    /// The physical key, for the keypad and the base-layout key.
    pub physical: PhysicalKey,
    /// Left, right, keypad or standard.
    pub location: KeyLocation,
    /// Pressed or released.
    pub state: ElementState,
    /// Whether this press is an auto-repeat.
    pub repeat: bool,
    /// Text the event produced, ignoring Ctrl (winit's `text`; on macOS
    /// the logical key's text, so never composed by a dead key).
    pub text: Option<SmolStr>,
    /// Text the OS produced with every modifier applied (winit's
    /// `text_with_all_modifiers`): empty for a dead key, the composed
    /// character for a composing Option.
    pub text_with_all_modifiers: Option<SmolStr>,
}

impl KeyInput {
    /// The fields of `event` that [`map_key`] reads.
    pub fn from_event(event: &winit::event::KeyEvent) -> Self {
        Self {
            logical: event.logical_key.clone(),
            unmodified: unmodified_key(event),
            physical: event.physical_key,
            location: event.location,
            state: event.state,
            repeat: event.repeat,
            text: event.text.clone(),
            text_with_all_modifiers: text_with_all_modifiers(event),
        }
    }
}

/// winit's `text_with_all_modifiers` where the platform has it, else the
/// event's text.
#[cfg(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn text_with_all_modifiers(event: &winit::event::KeyEvent) -> Option<SmolStr> {
    use winit::platform::modifier_supplement::KeyEventExtModifierSupplement as _;
    event.text_with_all_modifiers().map(SmolStr::new)
}

/// winit's `text_with_all_modifiers` where the platform has it, else the
/// event's text.
#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn text_with_all_modifiers(event: &winit::event::KeyEvent) -> Option<SmolStr> {
    event.text.clone()
}

/// winit's `key_without_modifiers` where the platform has it, else the
/// logical key.
#[cfg(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn unmodified_key(event: &winit::event::KeyEvent) -> WinitKey {
    use winit::platform::modifier_supplement::KeyEventExtModifierSupplement as _;
    event.key_without_modifiers()
}

/// winit's `key_without_modifiers` where the platform has it, else the
/// logical key.
#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn unmodified_key(event: &winit::event::KeyEvent) -> WinitKey {
    event.logical_key.clone()
}

/// What a key event asks the window to do.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyAction {
    /// Send this event to the session.
    Send(InputEvent),
    /// Read the clipboard and send it as [`InputEvent::Paste`].
    Paste,
}

/// The core's modifier set for winit's.
pub fn modifiers(state: ModifiersState) -> Modifiers {
    let mut out = Modifiers::NONE;
    for (winit, core) in [
        (ModifiersState::SHIFT, Modifiers::SHIFT),
        (ModifiersState::CONTROL, Modifiers::CTRL),
        (ModifiersState::ALT, Modifiers::ALT),
        (ModifiersState::SUPER, Modifiers::SUPER),
    ] {
        if state.contains(winit) {
            out |= core;
        }
    }
    out
}

/// The modifiers held for key and mouse events, after the right-Option
/// policy ([`effective_modifiers`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyMods {
    /// The modifiers that count, without Alt when the right Option
    /// composes.
    pub held: ModifiersState,
    /// Alt was held only through the right Option key on macOS, which
    /// composes text rather than being Alt.
    pub option_composes: bool,
}

impl From<ModifiersState> for KeyMods {
    /// `held` as is, with no composing Option.
    fn from(held: ModifiersState) -> Self {
        Self {
            held,
            option_composes: false,
        }
    }
}

/// The modifiers key and mouse events count as held: `state` as winit
/// reports it, except that on macOS Alt held only through the right Option
/// key (`right_alt` without `left_alt`) is removed and marked as composing,
/// because that key composes text instead of being Alt. When winit cannot
/// tell the sides apart (neither is known to be pressed) Alt stays.
pub fn effective_modifiers(
    state: ModifiersState,
    left_alt: bool,
    right_alt: bool,
    platform: Platform,
) -> KeyMods {
    if platform == Platform::MacOs && right_alt && !left_alt && state.alt_key() {
        KeyMods {
            held: state - ModifiersState::ALT,
            option_composes: true,
        }
    } else {
        state.into()
    }
}

/// What key event `input`, with `mods` held, means on `platform`: a paste
/// request, a key for the session, or nothing (a dead key, a key the
/// terminal has no name for, the release of the paste chord, a Cmd chord on
/// macOS).
pub fn map_key(
    input: &KeyInput,
    mods: impl Into<KeyMods>,
    platform: Platform,
) -> Option<KeyAction> {
    let mods = mods.into();
    let mut modifiers = modifiers(mods.held);
    // A dead key types nothing itself; the window server composes it into
    // the next key's characters. winit's unmodified key for it is the bare
    // accent, never `Dead`, so the logical key decides. Under Ctrl or Alt
    // it is an ordinary key (German Ctrl+`^` is `0x1e`), named by that
    // unmodified accent.
    if let WinitKey::Dead(_) = input.logical
        && !modifiers.intersects(Modifiers::CTRL | Modifiers::ALT)
    {
        return None;
    }
    let mut key = core_key(input)?;
    if matches!(key, Key::NumLock | Key::CapsLock | Key::ScrollLock) {
        // The encoder reports lock keys as `CSI u` even in legacy mode, the
        // only mode the core encodes with until it tracks the terminal's
        // keyboard modes (#8); the Mac keypad's Clear key arrives as Num
        // Lock. Lock state still reaches the OS text.
        return None;
    }
    let mut text = input.text.as_deref();
    let composed = input
        .text_with_all_modifiers
        .as_deref()
        .filter(|t| !t.is_empty());
    let mut typed_composed = false;
    if mods.option_composes {
        match key {
            // The right Option composes: send what it composed, or nothing
            // (winit's `text` would be the bare letter).
            Key::Char(_) | Key::Unidentified => {
                text = Some(composed?);
                typed_composed = true;
            }
            // Keys that do not type keep Option as Alt (word motion).
            _ => modifiers |= Modifiers::ALT,
        }
    } else if !modifiers.intersects(Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER)
        && composed.is_some()
    {
        // With only Shift held this is what the key typed, including a
        // dead-key composition that winit's `text` (the logical key's)
        // misses: `^` then Space types `^`.
        text = composed;
        typed_composed = true;
    }
    if typed_composed && key == Key::Char(' ') && text != Some(" ") {
        // The encoder sends Space as a C0 key whatever its text; what the
        // composition typed (`^`, a right-Option no-break space) is a
        // text-only event.
        key = Key::Unidentified;
    }
    if modifiers == platform.paste_modifiers() && is_v_key(key, input.physical) {
        return (input.state == ElementState::Pressed).then_some(KeyAction::Paste);
    }
    if platform == Platform::MacOs && modifiers.contains(Modifiers::SUPER) {
        // Cmd chords belong to the application, as in every macOS
        // terminal; the encoder would send them as `CSI u` noise.
        return None;
    }
    let kind = match (input.state, input.repeat) {
        (ElementState::Released, _) => KeyEventKind::Release,
        (ElementState::Pressed, true) => KeyEventKind::Repeat,
        (ElementState::Pressed, false) => KeyEventKind::Press,
    };
    let mut event = KeyEvent::new(key, modifiers).with_kind(kind);
    if let Key::Char(c) = key {
        if let Some(shifted) = single_char(&input.logical).filter(|&s| s != c) {
            event = event.with_shifted_key(shifted);
        }
        if let Some(base) = base_layout_char(input.physical).filter(|&b| b != c) {
            event = event.with_base_layout_key(base);
        }
    }
    // The OS text is the key's text on press. Under Alt it is what macOS
    // composed for Option (`∫` for Option+b), and under Ctrl it is the
    // plain letter, which kitty's associated-text report must not carry;
    // the encoder derives what these chords send from the key.
    if let Some(text) = text.filter(|t| !t.is_empty())
        && input.state == ElementState::Pressed
        && !modifiers.intersects(Modifiers::ALT | Modifiers::CTRL)
    {
        event = event.with_text(text);
    }
    Some(KeyAction::Send(InputEvent::Key(event)))
}

/// Whether `key` (at `physical`) is the V key for the paste chord: the key
/// that types `v` in the active layout, or, when the layout's key is not an
/// ASCII letter (Cyrillic `м`), the key at the V position. A Latin layout
/// that moves V (Dvorak) pastes from where it types `v`, as macOS does.
fn is_v_key(key: Key, physical: PhysicalKey) -> bool {
    match key {
        Key::Char(c) if c.is_ascii_alphabetic() => c.eq_ignore_ascii_case(&'v'),
        Key::Char(_) | Key::Unidentified => {
            base_layout_char(physical).is_some_and(|b| b.eq_ignore_ascii_case(&'v'))
        }
        _ => false,
    }
}

/// The core's name for the key of `input`, or `None` for dead keys and keys
/// it has no name for.
fn core_key(input: &KeyInput) -> Option<Key> {
    if input.location == KeyLocation::Numpad
        && let Some(key) = keypad_key(input)
    {
        return Some(key);
    }
    match &input.unmodified {
        WinitKey::Named(named) => named_key(*named, input.location),
        WinitKey::Character(s) => Some(match single_char_of(s) {
            Some(c) => Key::Char(c),
            None => Key::Unidentified,
        }),
        WinitKey::Unidentified(_) => input
            .text
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(|_| Key::Unidentified),
        WinitKey::Dead(_) => None,
    }
}

/// The keypad key of an event on the numeric keypad: its digit or operator
/// key from the physical key, or, with Num Lock off, its navigation key from
/// the logical key.
fn keypad_key(input: &KeyInput) -> Option<Key> {
    if let WinitKey::Named(named) = input.logical {
        let nav = match named {
            NamedKey::ArrowLeft => Some(Key::KpLeft),
            NamedKey::ArrowRight => Some(Key::KpRight),
            NamedKey::ArrowUp => Some(Key::KpUp),
            NamedKey::ArrowDown => Some(Key::KpDown),
            NamedKey::PageUp => Some(Key::KpPageUp),
            NamedKey::PageDown => Some(Key::KpPageDown),
            NamedKey::Home => Some(Key::KpHome),
            NamedKey::End => Some(Key::KpEnd),
            NamedKey::Insert => Some(Key::KpInsert),
            NamedKey::Delete => Some(Key::KpDelete),
            NamedKey::Clear => Some(Key::KpBegin),
            _ => None,
        };
        if nav.is_some() {
            return nav;
        }
    }
    let PhysicalKey::Code(code) = input.physical else {
        return None;
    };
    Some(match code {
        KeyCode::Numpad0 => Key::Kp0,
        KeyCode::Numpad1 => Key::Kp1,
        KeyCode::Numpad2 => Key::Kp2,
        KeyCode::Numpad3 => Key::Kp3,
        KeyCode::Numpad4 => Key::Kp4,
        KeyCode::Numpad5 => Key::Kp5,
        KeyCode::Numpad6 => Key::Kp6,
        KeyCode::Numpad7 => Key::Kp7,
        KeyCode::Numpad8 => Key::Kp8,
        KeyCode::Numpad9 => Key::Kp9,
        KeyCode::NumpadDecimal => Key::KpDecimal,
        KeyCode::NumpadDivide => Key::KpDivide,
        KeyCode::NumpadMultiply | KeyCode::NumpadStar => Key::KpMultiply,
        KeyCode::NumpadSubtract => Key::KpSubtract,
        KeyCode::NumpadAdd => Key::KpAdd,
        KeyCode::NumpadEnter => Key::KpEnter,
        KeyCode::NumpadEqual => Key::KpEqual,
        KeyCode::NumpadComma => Key::KpSeparator,
        _ => return None,
    })
}

/// The core's key for a winit named key at `location`.
fn named_key(named: NamedKey, location: KeyLocation) -> Option<Key> {
    let right = location == KeyLocation::Right;
    let side = |left: Key, right_key: Key| if right { right_key } else { left };
    Some(match named {
        NamedKey::Enter => Key::Enter,
        NamedKey::Tab => Key::Tab,
        NamedKey::Space => Key::Char(' '),
        NamedKey::Backspace => Key::Backspace,
        NamedKey::Escape => Key::Escape,
        NamedKey::Insert => Key::Insert,
        NamedKey::Delete => Key::Delete,
        NamedKey::ArrowLeft => Key::Left,
        NamedKey::ArrowRight => Key::Right,
        NamedKey::ArrowUp => Key::Up,
        NamedKey::ArrowDown => Key::Down,
        NamedKey::PageUp => Key::PageUp,
        NamedKey::PageDown => Key::PageDown,
        NamedKey::Home => Key::Home,
        NamedKey::End => Key::End,
        NamedKey::CapsLock => Key::CapsLock,
        NamedKey::ScrollLock => Key::ScrollLock,
        NamedKey::NumLock => Key::NumLock,
        NamedKey::PrintScreen => Key::PrintScreen,
        NamedKey::Pause => Key::Pause,
        NamedKey::ContextMenu => Key::Menu,
        NamedKey::MediaPlay => Key::MediaPlay,
        NamedKey::MediaPause => Key::MediaPause,
        NamedKey::MediaPlayPause => Key::MediaPlayPause,
        NamedKey::MediaStop => Key::MediaStop,
        NamedKey::MediaFastForward => Key::MediaFastForward,
        NamedKey::MediaRewind => Key::MediaRewind,
        NamedKey::MediaTrackNext => Key::MediaTrackNext,
        NamedKey::MediaTrackPrevious => Key::MediaTrackPrevious,
        NamedKey::MediaRecord => Key::MediaRecord,
        NamedKey::AudioVolumeDown => Key::LowerVolume,
        NamedKey::AudioVolumeUp => Key::RaiseVolume,
        NamedKey::AudioVolumeMute => Key::MuteVolume,
        NamedKey::Shift => side(Key::LeftShift, Key::RightShift),
        NamedKey::Control => side(Key::LeftControl, Key::RightControl),
        NamedKey::Alt => side(Key::LeftAlt, Key::RightAlt),
        NamedKey::Super => side(Key::LeftSuper, Key::RightSuper),
        NamedKey::Hyper => side(Key::LeftHyper, Key::RightHyper),
        NamedKey::Meta => side(Key::LeftMeta, Key::RightMeta),
        NamedKey::AltGraph => Key::IsoLevel3Shift,
        other => return function_key(other).map(Key::F),
    })
}

/// The number of a function key, `F1` through `F35`.
fn function_key(named: NamedKey) -> Option<u8> {
    const KEYS: [NamedKey; 35] = [
        NamedKey::F1,
        NamedKey::F2,
        NamedKey::F3,
        NamedKey::F4,
        NamedKey::F5,
        NamedKey::F6,
        NamedKey::F7,
        NamedKey::F8,
        NamedKey::F9,
        NamedKey::F10,
        NamedKey::F11,
        NamedKey::F12,
        NamedKey::F13,
        NamedKey::F14,
        NamedKey::F15,
        NamedKey::F16,
        NamedKey::F17,
        NamedKey::F18,
        NamedKey::F19,
        NamedKey::F20,
        NamedKey::F21,
        NamedKey::F22,
        NamedKey::F23,
        NamedKey::F24,
        NamedKey::F25,
        NamedKey::F26,
        NamedKey::F27,
        NamedKey::F28,
        NamedKey::F29,
        NamedKey::F30,
        NamedKey::F31,
        NamedKey::F32,
        NamedKey::F33,
        NamedKey::F34,
        NamedKey::F35,
    ];
    let index = KEYS.iter().position(|&key| key == named)?;
    u8::try_from(index + 1).ok()
}

/// The one character of a character key, if it has exactly one.
fn single_char(key: &WinitKey) -> Option<char> {
    match key {
        WinitKey::Character(s) => single_char_of(s),
        _ => None,
    }
}

/// The one character of `s`, if it has exactly one.
fn single_char_of(s: &str) -> Option<char> {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// The character the key at `physical` produces on a US layout without
/// modifiers.
fn base_layout_char(physical: PhysicalKey) -> Option<char> {
    const LETTERS: [KeyCode; 26] = [
        KeyCode::KeyA,
        KeyCode::KeyB,
        KeyCode::KeyC,
        KeyCode::KeyD,
        KeyCode::KeyE,
        KeyCode::KeyF,
        KeyCode::KeyG,
        KeyCode::KeyH,
        KeyCode::KeyI,
        KeyCode::KeyJ,
        KeyCode::KeyK,
        KeyCode::KeyL,
        KeyCode::KeyM,
        KeyCode::KeyN,
        KeyCode::KeyO,
        KeyCode::KeyP,
        KeyCode::KeyQ,
        KeyCode::KeyR,
        KeyCode::KeyS,
        KeyCode::KeyT,
        KeyCode::KeyU,
        KeyCode::KeyV,
        KeyCode::KeyW,
        KeyCode::KeyX,
        KeyCode::KeyY,
        KeyCode::KeyZ,
    ];
    const DIGITS: [KeyCode; 10] = [
        KeyCode::Digit0,
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];
    let PhysicalKey::Code(code) = physical else {
        return None;
    };
    if let Some(i) = LETTERS.iter().position(|&k| k == code) {
        return char::from_u32(u32::from(b'a') + u32::try_from(i).ok()?);
    }
    if let Some(i) = DIGITS.iter().position(|&k| k == code) {
        return char::from_digit(u32::try_from(i).ok()?, 10);
    }
    Some(match code {
        KeyCode::Minus => '-',
        KeyCode::Equal => '=',
        KeyCode::BracketLeft => '[',
        KeyCode::BracketRight => ']',
        KeyCode::Backslash => '\\',
        KeyCode::Semicolon => ';',
        KeyCode::Quote => '\'',
        KeyCode::Backquote => '`',
        KeyCode::Comma => ',',
        KeyCode::Period => '.',
        KeyCode::Slash => '/',
        _ => return None,
    })
}

/// Where the grid's cells sit in the window. Every field is in physical
/// pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellGeometry {
    /// Pixels of padding before the first column and row.
    pub padding: f64,
    /// Width of one cell.
    pub cell_width: f64,
    /// Height of one cell.
    pub cell_height: f64,
}

impl CellGeometry {
    /// The geometry of a view drawing `metrics`-sized cells at backing
    /// `scale` with `padding_pt` points of padding, rounded to whole pixels
    /// as [`tessera_app_core::view::grid_size_for`] rounds it.
    pub fn new(metrics: CellMetrics, scale: f32, padding_pt: f32) -> Self {
        let padding = f64::from(padding_pt) * f64::from(scale);
        Self {
            padding: if padding.is_finite() {
                padding.round().max(0.0)
            } else {
                0.0
            },
            cell_width: f64::from(metrics.width_px.max(1)),
            cell_height: f64::from(metrics.height_px.max(1)),
        }
    }

    /// The zero-based `(col, row)` of the cell under pixel (`x`, `y`).
    /// Positions in the padding or beyond the window count as the nearest
    /// cell; the core clamps the far edge to the grid.
    pub fn cell_at(&self, x: f64, y: f64) -> (u16, u16) {
        let index = |offset: f64, cell: f64| {
            let cells = ((offset - self.padding) / cell).floor();
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "clamped to the u16 range first; NaN becomes 0"
            )]
            let cells = cells.clamp(0.0, f64::from(u16::MAX)) as u16;
            cells
        };
        (index(x, self.cell_width), index(y, self.cell_height))
    }
}

impl Default for CellGeometry {
    /// One-pixel cells and no padding, for events before the view exists.
    fn default() -> Self {
        Self {
            padding: 0.0,
            cell_width: 1.0,
            cell_height: 1.0,
        }
    }
}

/// The core's button for a winit mouse button; `None` for buttons it has
/// no name for.
pub fn mouse_button(button: winit::event::MouseButton) -> Option<MouseButton> {
    use winit::event::MouseButton as W;
    Some(match button {
        W::Left => MouseButton::Left,
        W::Middle => MouseButton::Middle,
        W::Right => MouseButton::Right,
        W::Back => MouseButton::Back,
        W::Forward => MouseButton::Forward,
        W::Other(_) => return None,
    })
}

/// The pointer position as `f32` pixels for [`MouseEvent::x_px`].
#[expect(
    clippy::cast_possible_truncation,
    reason = "window coordinates are far inside the f32 range"
)]
fn px(v: f64) -> f32 {
    v as f32
}

/// A button press or release at `position` (physical pixels), or `None`
/// for a button the core has no name for.
pub fn mouse_input(
    state: ElementState,
    button: winit::event::MouseButton,
    position: (f64, f64),
    geometry: CellGeometry,
    mods: ModifiersState,
) -> Option<InputEvent> {
    let button = mouse_button(button)?;
    let kind = match state {
        ElementState::Pressed => MouseEventKind::Press,
        ElementState::Released => MouseEventKind::Release,
    };
    let (col, row) = geometry.cell_at(position.0, position.1);
    Some(InputEvent::Mouse(
        MouseEvent::new(kind, button, col, row)
            .with_pixels(px(position.0), px(position.1))
            .with_modifiers(modifiers(mods)),
    ))
}

/// Pointer motion to `position` (physical pixels) with `held` down
/// ([`MouseButton::None`] for none).
pub fn mouse_motion(
    position: (f64, f64),
    held: MouseButton,
    geometry: CellGeometry,
    mods: ModifiersState,
) -> InputEvent {
    let (col, row) = geometry.cell_at(position.0, position.1);
    InputEvent::Mouse(
        MouseEvent::new(MouseEventKind::Motion, held, col, row)
            .with_pixels(px(position.0), px(position.1))
            .with_modifiers(modifiers(mods)),
    )
}

/// A wheel or trackpad scroll at `position` (physical pixels).
///
/// winit's positive vertical delta scrolls up, towards older output, which
/// is the core's positive [`ScrollEvent::delta_lines`]. A notched wheel
/// reports lines; a trackpad reports pixels, which become lines of
/// `geometry`'s cell height and are marked precise.
pub fn scroll(
    delta: MouseScrollDelta,
    position: (f64, f64),
    geometry: CellGeometry,
    mods: ModifiersState,
) -> InputEvent {
    let (col, row) = geometry.cell_at(position.0, position.1);
    let event = match delta {
        MouseScrollDelta::LineDelta(_, lines) => ScrollEvent::new(lines),
        MouseScrollDelta::PixelDelta(pixels) => {
            ScrollEvent::new(px(pixels.y / geometry.cell_height))
                .with_pixels(px(pixels.x), px(pixels.y))
                .with_precise(true)
        }
    };
    InputEvent::Scroll(event.with_cell(col, row).with_modifiers(modifiers(mods)))
}

/// The window gained (`true`) or lost keyboard focus.
pub fn focus(focused: bool) -> InputEvent {
    InputEvent::Focus(if focused {
        FocusEvent::Gained
    } else {
        FocusEvent::Lost
    })
}

/// Input held back while a paste's clipboard read runs, so that "Cmd-V,
/// Return" cannot submit the line before the pasted text arrives.
///
/// Keys, mouse buttons and scrolls are kept in order, at most
/// [`InputHold::CAPACITY`] of them; pointer motions collapse into the
/// latest one since the last button or scroll. Events past the capacity are dropped and counted.
/// Focus changes are not held: they describe the window, not the line.
#[derive(Debug, Default)]
pub struct InputHold {
    events: VecDeque<InputEvent>,
    dropped: usize,
}

impl InputHold {
    /// Most events held; a clipboard read lasts at most two seconds.
    pub const CAPACITY: usize = 256;

    /// Whether `event` is held while a paste is pending.
    pub fn holds(event: &InputEvent) -> bool {
        matches!(
            event,
            InputEvent::Key(_) | InputEvent::Mouse(_) | InputEvent::Scroll(_)
        )
    }

    /// Keeps `event` for [`InputHold::release`], or drops it when full.
    pub fn push(&mut self, event: InputEvent) {
        let is_motion = |event: &InputEvent| {
            matches!(
                event,
                InputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Motion,
                    ..
                })
            )
        };
        if is_motion(&event) {
            // Only the latest position matters, but a motion before a
            // button or scroll must stay before it (they are where the
            // pointer was then); keys do not care where it is.
            let pointer_barrier = |event: &InputEvent| {
                matches!(event, InputEvent::Mouse(_) | InputEvent::Scroll(_)) && !is_motion(event)
            };
            if let Some(index) = self
                .events
                .iter()
                .rposition(|held| is_motion(held) || pointer_barrier(held))
                .filter(|&index| is_motion(&self.events[index]))
            {
                self.events.remove(index);
            }
        }
        if self.events.len() < Self::CAPACITY {
            self.events.push_back(event);
        } else {
            self.dropped += 1;
        }
    }

    /// The held events in arrival order and how many were dropped; the
    /// hold is empty afterwards.
    pub fn release(&mut self) -> (Vec<InputEvent>, usize) {
        (
            self.events.drain(..).collect(),
            std::mem::take(&mut self.dropped),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;

    fn key(c: char) -> InputEvent {
        InputEvent::Key(KeyEvent::new(Key::Char(c), Modifiers::NONE))
    }

    fn motion(col: u16) -> InputEvent {
        InputEvent::Mouse(MouseEvent::new(
            MouseEventKind::Motion,
            MouseButton::None,
            col,
            0,
        ))
    }

    #[test]
    fn the_hold_releases_in_order_and_empties() {
        let mut hold = InputHold::default();
        let enter = InputEvent::Key(KeyEvent::new(Key::Enter, Modifiers::NONE));
        let click = InputEvent::Mouse(MouseEvent::new(
            MouseEventKind::Press,
            MouseButton::Left,
            1,
            1,
        ));
        let wheel = InputEvent::Scroll(ScrollEvent::new(1.0));
        for event in [key('a'), click.clone(), wheel.clone(), enter.clone()] {
            assert!(InputHold::holds(&event));
            hold.push(event);
        }
        assert!(!InputHold::holds(&focus(true)));
        assert!(!InputHold::holds(&InputEvent::Paste("x".into())));
        assert_eq!(hold.release(), (vec![key('a'), click, wheel, enter], 0));
        assert_eq!(hold.release(), (vec![], 0));
    }

    #[test]
    fn motions_collapse_into_the_latest() {
        let mut hold = InputHold::default();
        for col in 0..1000 {
            hold.push(motion(col));
        }
        hold.push(key('x'));
        hold.push(motion(1));
        hold.push(key('y'));
        hold.push(motion(2));
        assert_eq!(hold.release(), (vec![key('x'), key('y'), motion(2)], 0));
        // Typing while the mouse moves cannot fill the hold with motions.
        for i in 0..InputHold::CAPACITY {
            hold.push(motion(u16::try_from(i).unwrap()));
            hold.push(key('k'));
        }
        let (events, dropped) = hold.release();
        assert_eq!(dropped, 1, "only the last key is over the capacity");
        let motions: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, InputEvent::Mouse(_)))
            .collect();
        let last = u16::try_from(InputHold::CAPACITY - 1).unwrap();
        assert_eq!(motions, [&motion(last)], "one motion, the latest");
    }

    #[test]
    fn motions_do_not_cross_buttons_or_scrolls() {
        let mut hold = InputHold::default();
        let press = InputEvent::Mouse(MouseEvent::new(
            MouseEventKind::Press,
            MouseButton::Left,
            5,
            0,
        ));
        let wheel = InputEvent::Scroll(ScrollEvent::new(1.0));
        for event in [
            motion(1),
            motion(2),
            press.clone(),
            motion(3),
            key('a'),
            motion(4),
            wheel.clone(),
            motion(5),
        ] {
            hold.push(event);
        }
        assert_eq!(
            hold.release(),
            (
                vec![motion(2), press, key('a'), motion(4), wheel, motion(5)],
                0
            )
        );
    }

    #[test]
    fn the_hold_is_bounded() {
        let mut hold = InputHold::default();
        for i in 0..InputHold::CAPACITY + 10 {
            hold.push(key(char::from(b'a' + u8::try_from(i % 26).unwrap())));
        }
        let (events, dropped) = hold.release();
        assert_eq!(events.len(), InputHold::CAPACITY);
        assert_eq!(dropped, 10);
        assert_eq!(events[0], key('a'), "the oldest are kept");
        assert_eq!(hold.release(), (vec![], 0), "the drop count resets");
    }

    fn char_key(unmodified: &str, logical: &str, code: KeyCode) -> KeyInput {
        KeyInput {
            logical: WinitKey::Character(logical.into()),
            unmodified: WinitKey::Character(unmodified.into()),
            physical: PhysicalKey::Code(code),
            location: KeyLocation::Standard,
            state: ElementState::Pressed,
            repeat: false,
            text: Some(logical.into()),
            text_with_all_modifiers: Some(logical.into()),
        }
    }

    fn named(key: NamedKey, code: KeyCode) -> KeyInput {
        KeyInput {
            logical: WinitKey::Named(key),
            unmodified: WinitKey::Named(key),
            physical: PhysicalKey::Code(code),
            location: KeyLocation::Standard,
            state: ElementState::Pressed,
            repeat: false,
            text: None,
            text_with_all_modifiers: None,
        }
    }

    fn sent(input: &KeyInput, mods: impl Into<KeyMods>) -> KeyEvent {
        match map_key(input, mods, Platform::MacOs) {
            Some(KeyAction::Send(InputEvent::Key(event))) => event,
            other => panic!("expected a key, got {other:?}"),
        }
    }

    /// The bytes the core's encoder writes for `input` with `mods`.
    fn bytes(input: &KeyInput, mods: impl Into<KeyMods>) -> Vec<u8> {
        tessera_term::encode_key(&sent(input, mods), &tessera_term::KeyboardModes::default())
    }

    #[test]
    fn arrows_reach_the_encoder_as_cursor_keys() {
        let none = ModifiersState::empty();
        for (named_key, code, key, seq) in [
            (NamedKey::ArrowUp, KeyCode::ArrowUp, Key::Up, b"\x1b[A"),
            (
                NamedKey::ArrowDown,
                KeyCode::ArrowDown,
                Key::Down,
                b"\x1b[B",
            ),
            (
                NamedKey::ArrowRight,
                KeyCode::ArrowRight,
                Key::Right,
                b"\x1b[C",
            ),
            (
                NamedKey::ArrowLeft,
                KeyCode::ArrowLeft,
                Key::Left,
                b"\x1b[D",
            ),
        ] {
            let input = named(named_key, code);
            assert_eq!(sent(&input, none).key, key);
            assert_eq!(bytes(&input, none), seq);
        }
    }

    #[test]
    fn modified_arrows_carry_the_modifier_parameter() {
        let input = named(NamedKey::ArrowLeft, KeyCode::ArrowLeft);
        assert_eq!(bytes(&input, ModifiersState::CONTROL), b"\x1b[1;5D");
        assert_eq!(
            bytes(&input, ModifiersState::SHIFT | ModifiersState::ALT),
            b"\x1b[1;4D"
        );
    }

    #[test]
    fn function_keys_map_one_to_thirty_five() {
        assert_eq!(function_key(NamedKey::F1), Some(1));
        assert_eq!(function_key(NamedKey::F12), Some(12));
        assert_eq!(function_key(NamedKey::F35), Some(35));
        assert_eq!(function_key(NamedKey::Enter), None);
        let none = ModifiersState::empty();
        assert_eq!(bytes(&named(NamedKey::F1, KeyCode::F1), none), b"\x1bOP");
        assert_eq!(bytes(&named(NamedKey::F5, KeyCode::F5), none), b"\x1b[15~");
        assert_eq!(
            bytes(&named(NamedKey::F12, KeyCode::F12), ModifiersState::SHIFT),
            b"\x1b[24;2~"
        );
    }

    #[test]
    fn editing_keys_map_to_their_core_names() {
        let none = ModifiersState::empty();
        for (named_key, key) in [
            (NamedKey::Enter, Key::Enter),
            (NamedKey::Tab, Key::Tab),
            (NamedKey::Backspace, Key::Backspace),
            (NamedKey::Escape, Key::Escape),
            (NamedKey::Delete, Key::Delete),
            (NamedKey::Home, Key::Home),
            (NamedKey::PageDown, Key::PageDown),
            (NamedKey::Space, Key::Char(' ')),
        ] {
            assert_eq!(sent(&named(named_key, KeyCode::Fn), none).key, key);
        }
        assert_eq!(bytes(&named(NamedKey::Enter, KeyCode::Enter), none), b"\r");
        assert_eq!(
            bytes(&named(NamedKey::Backspace, KeyCode::Backspace), none),
            b"\x7f"
        );
    }

    #[test]
    fn modifiers_map_bit_for_bit() {
        assert_eq!(modifiers(ModifiersState::empty()), Modifiers::NONE);
        assert_eq!(modifiers(ModifiersState::SHIFT), Modifiers::SHIFT);
        assert_eq!(modifiers(ModifiersState::CONTROL), Modifiers::CTRL);
        assert_eq!(modifiers(ModifiersState::ALT), Modifiers::ALT);
        assert_eq!(modifiers(ModifiersState::SUPER), Modifiers::SUPER);
        assert_eq!(
            modifiers(ModifiersState::all()),
            Modifiers::SHIFT | Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER
        );
    }

    #[test]
    fn ctrl_c_interrupts() {
        // winit's `text` ignores Ctrl, so Ctrl+C arrives as "c".
        let input = char_key("c", "c", KeyCode::KeyC);
        let event = sent(&input, ModifiersState::CONTROL);
        assert_eq!(event.key, Key::Char('c'));
        assert_eq!(event.modifiers, Modifiers::CTRL);
        assert_eq!(bytes(&input, ModifiersState::CONTROL), b"\x03");
    }

    #[test]
    fn shift_sends_the_layout_text_and_reports_the_shifted_key() {
        let input = char_key("4", "$", KeyCode::Digit4);
        let event = sent(&input, ModifiersState::SHIFT);
        assert_eq!(event.key, Key::Char('4'));
        assert_eq!(event.shifted_key, Some('$'));
        assert_eq!(event.base_layout_key, None, "same as the key on US");
        assert_eq!(bytes(&input, ModifiersState::SHIFT), b"$");
    }

    /// The bytes for `input` with `mods` in kitty mode with
    /// disambiguate + report-alternate-keys.
    fn kitty_bytes(input: &KeyInput, mods: impl Into<KeyMods>) -> Vec<u8> {
        let kitty = tessera_term::KeyboardModes {
            kitty_flags: tessera_term::KittyFlags::DISAMBIGUATE_ESCAPE_CODES
                | tessera_term::KittyFlags::REPORT_ALTERNATE_KEYS,
            ..tessera_term::KeyboardModes::default()
        };
        tessera_term::encode_key(&sent(input, mods), &kitty)
    }

    /// A key as winit 0.30 reports it on macOS with the left Option key as
    /// Alt: the event is rebuilt from `charactersIgnoringModifiers`, so
    /// the logical key and text have Shift applied but no Option
    /// composition.
    fn left_option_key(unmodified: &str, shifted: &str, code: KeyCode) -> KeyInput {
        char_key(unmodified, shifted, code)
    }

    #[test]
    fn left_option_shift_chords_send_the_layouts_shifted_key() {
        let alt_shift = ModifiersState::ALT | ModifiersState::SHIFT;
        let cases = [
            (
                "US ,",
                left_option_key(",", "<", KeyCode::Comma),
                b"\x1b<".as_slice(),
            ),
            (
                "German ,",
                left_option_key(",", ";", KeyCode::Comma),
                b"\x1b;",
            ),
            (
                "ISO <",
                left_option_key("<", ">", KeyCode::IntlBackslash),
                b"\x1b>",
            ),
            (
                "Swedish 7",
                left_option_key("7", "/", KeyCode::Digit7),
                b"\x1b/",
            ),
            (
                "AZERTY 1",
                left_option_key("&", "1", KeyCode::Digit1),
                b"\x1b1",
            ),
            ("US b", left_option_key("b", "B", KeyCode::KeyB), b"\x1bB"),
        ];
        for (name, input, expected) in cases {
            assert_eq!(bytes(&input, alt_shift), expected, "{name}");
        }
        let comma = left_option_key(",", "<", KeyCode::Comma);
        assert_eq!(sent(&comma, alt_shift).shifted_key, Some('<'));
        assert_eq!(kitty_bytes(&comma, alt_shift), b"\x1b[44:60;4u");
        let german = left_option_key(",", ";", KeyCode::Comma);
        assert_eq!(kitty_bytes(&german, alt_shift), b"\x1b[44:59;4u");
    }

    /// The modifiers of the right Option key alone (plus `extra`) on macOS.
    fn right_option(extra: ModifiersState) -> KeyMods {
        effective_modifiers(ModifiersState::ALT | extra, false, true, Platform::MacOs)
    }

    #[test]
    fn right_option_composes_on_macos() {
        // Swedish: right Option+2 types "@"; winit leaves the event alone
        // (only the left Option is Alt) but still reports ALT.
        let at = char_key("2", "@", KeyCode::Digit2);
        let mods = right_option(ModifiersState::empty());
        assert_eq!(
            mods,
            KeyMods {
                held: ModifiersState::empty(),
                option_composes: true
            }
        );
        assert_eq!(bytes(&at, mods), b"@");
        let brace = char_key("8", "{", KeyCode::Digit8);
        let mods = right_option(ModifiersState::SHIFT);
        assert_eq!(mods.held, ModifiersState::SHIFT);
        assert_eq!(bytes(&brace, mods), b"{");
        // The composed text is winit's `text_with_all_modifiers`; its
        // `text` is the logical key's.
        let at = KeyInput {
            text: Some("2".into()),
            ..char_key("2", "@", KeyCode::Digit2)
        };
        assert_eq!(bytes(&at, right_option(ModifiersState::empty())), b"@");
        // The left Option (alone or with the right one) stays Alt.
        for (left, right) in [(true, false), (true, true)] {
            assert_eq!(
                effective_modifiers(ModifiersState::ALT, left, right, Platform::MacOs),
                ModifiersState::ALT.into()
            );
        }
        // Side unknown: Alt.
        assert_eq!(
            effective_modifiers(ModifiersState::ALT, false, false, Platform::MacOs),
            ModifiersState::ALT.into()
        );
        // Elsewhere both Alt keys are Alt (AltGr is its own key).
        assert_eq!(
            effective_modifiers(ModifiersState::ALT, false, true, Platform::Other),
            ModifiersState::ALT.into()
        );
        // A stale right-Option state without ALT composes nothing.
        assert_eq!(
            effective_modifiers(ModifiersState::SHIFT, false, true, Platform::MacOs),
            ModifiersState::SHIFT.into()
        );
    }

    #[test]
    fn right_option_without_text_sends_nothing() {
        // US right Option+E outside a composition: macOS produced no
        // characters, and winit's logical key is the bare "e".
        let e = KeyInput {
            text_with_all_modifiers: None,
            ..char_key("e", "e", KeyCode::KeyE)
        };
        assert_eq!(
            map_key(&e, right_option(ModifiersState::empty()), Platform::MacOs),
            None
        );
        let empty = KeyInput {
            text_with_all_modifiers: Some("".into()),
            ..e
        };
        assert_eq!(
            map_key(
                &empty,
                right_option(ModifiersState::empty()),
                Platform::MacOs
            ),
            None
        );
    }

    #[test]
    fn right_option_on_keys_that_do_not_type_is_alt() {
        let left = named(NamedKey::ArrowLeft, KeyCode::ArrowLeft);
        assert_eq!(
            bytes(&left, right_option(ModifiersState::empty())),
            b"\x1b[1;3D"
        );
        let backspace = named(NamedKey::Backspace, KeyCode::Backspace);
        assert_eq!(
            bytes(&backspace, right_option(ModifiersState::empty())),
            b"\x1b\x7f"
        );
    }

    #[test]
    fn ctrl_and_alt_drop_the_os_text() {
        let a = char_key("a", "a", KeyCode::KeyA);
        assert_eq!(sent(&a, ModifiersState::empty()).text.as_deref(), Some("a"));
        assert_eq!(sent(&a, ModifiersState::CONTROL).text, None);
        assert_eq!(sent(&a, ModifiersState::ALT).text, None);
        assert_eq!(sent(&a, ModifiersState::SHIFT).text.as_deref(), Some("a"));
    }

    #[test]
    fn paste_follows_the_physical_v_key() {
        // Russian layout: the V key types "м"; Cmd on it still pastes.
        let em = char_key("м", "м", KeyCode::KeyV);
        assert_eq!(
            map_key(&em, ModifiersState::SUPER, Platform::MacOs),
            Some(KeyAction::Paste)
        );
        // An upper-case report of the key (Caps Lock) pastes too.
        let upper = char_key("V", "V", KeyCode::KeyV);
        assert_eq!(
            map_key(&upper, ModifiersState::SUPER, Platform::MacOs),
            Some(KeyAction::Paste)
        );
        let chord = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(map_key(&em, chord, Platform::Other), Some(KeyAction::Paste));
        // Dvorak: the physical V key types "k", which is not a paste; the
        // key that types "v" is.
        let k = char_key("k", "k", KeyCode::KeyV);
        assert_eq!(map_key(&k, ModifiersState::SUPER, Platform::MacOs), None);
        let dvorak_v = char_key("v", "v", KeyCode::Period);
        assert_eq!(
            map_key(&dvorak_v, ModifiersState::SUPER, Platform::MacOs),
            Some(KeyAction::Paste)
        );
    }

    #[test]
    fn option_is_alt() {
        // With option-as-alt winit reports the left Option+b as "b"; were
        // the OS text the composed "∫", it is still not sent under Alt.
        assert_eq!(
            bytes(&char_key("b", "b", KeyCode::KeyB), ModifiersState::ALT),
            b"\x1bb"
        );
        let input = char_key("b", "∫", KeyCode::KeyB);
        assert_eq!(bytes(&input, ModifiersState::ALT), b"\x1bb");
    }

    #[test]
    fn non_us_layouts_carry_the_base_layout_key() {
        let input = char_key("с", "с", KeyCode::KeyC); // Cyrillic es
        let event = sent(&input, ModifiersState::empty());
        assert_eq!(event.base_layout_key, Some('c'));
        assert_eq!(bytes(&input, ModifiersState::empty()), "с".as_bytes());
        assert_eq!(bytes(&input, ModifiersState::CONTROL), b"\x03");
    }

    #[test]
    fn cmd_v_is_a_paste_request_on_macos() {
        let v = char_key("v", "v", KeyCode::KeyV);
        assert_eq!(
            map_key(&v, ModifiersState::SUPER, Platform::MacOs),
            Some(KeyAction::Paste)
        );
        let released = KeyInput {
            state: ElementState::Released,
            ..v.clone()
        };
        assert_eq!(
            map_key(&released, ModifiersState::SUPER, Platform::MacOs),
            None
        );
        // Ctrl+V is the terminal's literal-next, not a paste.
        assert!(matches!(
            map_key(&v, ModifiersState::CONTROL, Platform::MacOs),
            Some(KeyAction::Send(_))
        ));
        // Cmd+Shift+V is another chord, not a paste.
        assert_eq!(
            map_key(
                &v,
                ModifiersState::SUPER | ModifiersState::SHIFT,
                Platform::MacOs
            ),
            None
        );
    }

    #[test]
    fn ctrl_shift_v_is_a_paste_request_elsewhere() {
        let v = char_key("v", "V", KeyCode::KeyV);
        let chord = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(map_key(&v, chord, Platform::Other), Some(KeyAction::Paste));
        assert!(matches!(
            map_key(&v, ModifiersState::SUPER, Platform::Other),
            Some(KeyAction::Send(_))
        ));
        assert!(matches!(
            map_key(&v, chord, Platform::MacOs),
            Some(KeyAction::Send(_))
        ));
    }

    #[test]
    fn other_cmd_chords_are_not_sent_on_macos() {
        let q = char_key("q", "q", KeyCode::KeyQ);
        assert_eq!(map_key(&q, ModifiersState::SUPER, Platform::MacOs), None);
        let left = named(NamedKey::ArrowLeft, KeyCode::ArrowLeft);
        assert_eq!(
            map_key(
                &left,
                ModifiersState::SUPER | ModifiersState::SHIFT,
                Platform::MacOs
            ),
            None
        );
        // Elsewhere Super is an ordinary modifier the encoder reports.
        assert!(matches!(
            map_key(&q, ModifiersState::SUPER, Platform::Other),
            Some(KeyAction::Send(_))
        ));
    }

    #[test]
    fn repeats_and_releases_keep_their_kind() {
        let mut input = char_key("a", "a", KeyCode::KeyA);
        input.repeat = true;
        assert_eq!(
            sent(&input, ModifiersState::empty()).kind,
            KeyEventKind::Repeat
        );
        assert_eq!(bytes(&input, ModifiersState::empty()), b"a");
        input.repeat = false;
        input.state = ElementState::Released;
        input.text = None;
        assert_eq!(
            sent(&input, ModifiersState::empty()).kind,
            KeyEventKind::Release
        );
        assert_eq!(bytes(&input, ModifiersState::empty()), b"");
    }

    #[test]
    fn modifier_keys_know_their_side() {
        let mut input = named(NamedKey::Shift, KeyCode::ShiftRight);
        input.location = KeyLocation::Right;
        assert_eq!(sent(&input, ModifiersState::SHIFT).key, Key::RightShift);
        input.location = KeyLocation::Left;
        assert_eq!(sent(&input, ModifiersState::SHIFT).key, Key::LeftShift);
        assert_eq!(bytes(&input, ModifiersState::SHIFT), b"");
    }

    #[test]
    fn keypad_keys_come_from_the_physical_key() {
        let mut input = char_key("5", "5", KeyCode::Numpad5);
        input.location = KeyLocation::Numpad;
        assert_eq!(sent(&input, ModifiersState::empty()).key, Key::Kp5);
        assert_eq!(bytes(&input, ModifiersState::empty()), b"5");

        let mut enter = named(NamedKey::Enter, KeyCode::NumpadEnter);
        enter.location = KeyLocation::Numpad;
        assert_eq!(sent(&enter, ModifiersState::empty()).key, Key::KpEnter);

        // Num Lock off: the keypad 4 is a left arrow.
        let mut left = named(NamedKey::ArrowLeft, KeyCode::Numpad4);
        left.location = KeyLocation::Numpad;
        assert_eq!(sent(&left, ModifiersState::empty()).key, Key::KpLeft);
        assert_eq!(bytes(&left, ModifiersState::empty()), b"\x1b[D");
    }

    #[test]
    fn dead_keys_and_unnamed_keys_send_nothing() {
        // What winit 0.30 reports on macOS for German `´` (a dead key): the
        // logical key is `Dead`, the unmodified key the bare accent (it
        // translates with dead keys off), and there is no text.
        let dead = KeyInput {
            logical: WinitKey::Dead(Some('´')),
            unmodified: WinitKey::Character("´".into()),
            text: None,
            text_with_all_modifiers: None,
            ..char_key("´", "´", KeyCode::Equal)
        };
        for mods in [ModifiersState::empty(), ModifiersState::SHIFT] {
            assert_eq!(map_key(&dead, mods, Platform::MacOs), None);
        }
        assert_eq!(
            map_key(
                &dead,
                right_option(ModifiersState::empty()),
                Platform::MacOs
            ),
            None
        );
        let released = KeyInput {
            state: ElementState::Released,
            ..dead
        };
        assert_eq!(
            map_key(&released, ModifiersState::empty(), Platform::MacOs),
            None
        );
        assert_eq!(
            map_key(
                &named(NamedKey::BrowserBack, KeyCode::BrowserBack),
                ModifiersState::empty(),
                Platform::MacOs
            ),
            None
        );
    }

    #[test]
    fn dead_keys_under_ctrl_or_alt_are_ordinary_keys() {
        // German `^` (ISO key) and `´` as winit reports them under Ctrl or
        // the left Option: the logical key is still `Dead` (it comes from
        // `charactersIgnoringModifiers`, empty for a base-layer dead key).
        let dead = |accent: &str, code: KeyCode| KeyInput {
            logical: WinitKey::Dead(accent.chars().next()),
            unmodified: WinitKey::Character(accent.into()),
            text: None,
            text_with_all_modifiers: None,
            ..char_key(accent, accent, code)
        };
        let caret = dead("^", KeyCode::IntlBackslash);
        let acute = dead("´", KeyCode::Equal);
        assert_eq!(bytes(&caret, ModifiersState::CONTROL), b"\x1e");
        assert_eq!(bytes(&caret, ModifiersState::ALT), b"\x1b^");
        assert_eq!(bytes(&acute, ModifiersState::ALT), b"\x1b=");
        // Shift alone, or a composing right Option, still composes.
        assert_eq!(
            map_key(&caret, ModifiersState::SHIFT, Platform::MacOs),
            None
        );
        assert_eq!(
            map_key(
                &caret,
                right_option(ModifiersState::empty()),
                Platform::MacOs
            ),
            None
        );
    }

    #[test]
    fn right_option_space_sends_its_composed_text() {
        let space = |composed: &str| KeyInput {
            text: Some(" ".into()),
            text_with_all_modifiers: Some(composed.into()),
            ..named(NamedKey::Space, KeyCode::Space)
        };
        let mods = right_option(ModifiersState::empty());
        assert_eq!(bytes(&space("\u{a0}"), mods), "\u{a0}".as_bytes());
        assert_eq!(bytes(&space("~"), mods), b"~");
        assert_eq!(map_key(&space(""), mods, Platform::MacOs), None);
    }

    #[test]
    fn lock_keys_send_nothing() {
        for (named_key, code) in [
            (NamedKey::NumLock, KeyCode::NumLock),
            (NamedKey::CapsLock, KeyCode::CapsLock),
            (NamedKey::ScrollLock, KeyCode::ScrollLock),
        ] {
            let input = named(named_key, code);
            assert_eq!(
                map_key(&input, ModifiersState::empty(), Platform::MacOs),
                None
            );
            assert_eq!(
                map_key(&input, ModifiersState::empty(), Platform::Other),
                None
            );
        }
    }

    #[test]
    fn dead_key_compositions_arrive_on_the_next_key() {
        // What winit 0.30 reports on macOS after a dead key: the window
        // server composes into the next key's characters, which are its
        // `text_with_all_modifiers` (and, for a typing key, its logical
        // key and `text`).
        let none = ModifiersState::empty();
        // German `´` then `e`.
        assert_eq!(
            bytes(&char_key("e", "é", KeyCode::KeyE), none),
            "é".as_bytes()
        );
        // `^` then Space: Space's logical key is Space, its text " ",
        // and only `text_with_all_modifiers` holds the accent.
        let space_after = |composed: &str| KeyInput {
            text: Some(" ".into()),
            text_with_all_modifiers: Some(composed.into()),
            ..named(NamedKey::Space, KeyCode::Space)
        };
        assert_eq!(bytes(&space_after("^"), none), b"^");
        assert_eq!(bytes(&space_after("~"), none), b"~");
        assert_eq!(bytes(&space_after("`"), none), b"`");
        // A dead key followed by a key it does not combine with: both.
        assert_eq!(
            bytes(&char_key("x", "´x", KeyCode::KeyX), none),
            "´x".as_bytes()
        );
        // A plain Space is a space.
        assert_eq!(bytes(&space_after(" "), none), b" ");
        // Under Ctrl the composed text is not used.
        assert_eq!(bytes(&space_after("^"), ModifiersState::CONTROL), b"\x00");
    }

    fn geometry() -> CellGeometry {
        CellGeometry::new(
            CellMetrics {
                width_px: 10,
                height_px: 20,
                baseline_px: 15.0,
            },
            2.0,
            4.0,
        )
    }

    #[test]
    fn cells_count_from_inside_the_padding() {
        let g = geometry();
        assert!((g.padding - 8.0).abs() < f64::EPSILON);
        assert_eq!(g.cell_at(8.0, 8.0), (0, 0));
        assert_eq!(g.cell_at(17.9, 27.9), (0, 0));
        assert_eq!(g.cell_at(18.0, 28.0), (1, 1));
        assert_eq!(
            g.cell_at(0.0, 0.0),
            (0, 0),
            "padding counts as the first cell"
        );
        assert_eq!(g.cell_at(-50.0, -1.0), (0, 0));
        assert_eq!(g.cell_at(1e12, 1e12), (u16::MAX, u16::MAX));
        assert_eq!(g.cell_at(f64::NAN, f64::INFINITY), (0, u16::MAX));
    }

    #[test]
    fn mouse_buttons_map_and_carry_their_cell() {
        let event = mouse_input(
            ElementState::Pressed,
            winit::event::MouseButton::Left,
            (38.0, 48.0),
            geometry(),
            ModifiersState::SHIFT,
        );
        let expected = MouseEvent::new(MouseEventKind::Press, MouseButton::Left, 3, 2)
            .with_pixels(38.0, 48.0)
            .with_modifiers(Modifiers::SHIFT);
        assert_eq!(event, Some(InputEvent::Mouse(expected)));
        let released = mouse_input(
            ElementState::Released,
            winit::event::MouseButton::Right,
            (0.0, 0.0),
            geometry(),
            ModifiersState::empty(),
        );
        assert!(matches!(
            released,
            Some(InputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Release,
                button: MouseButton::Right,
                ..
            }))
        ));
        assert_eq!(
            mouse_button(winit::event::MouseButton::Back),
            Some(MouseButton::Back)
        );
        assert_eq!(mouse_button(winit::event::MouseButton::Other(9)), None);
        assert_eq!(
            mouse_input(
                ElementState::Pressed,
                winit::event::MouseButton::Other(9),
                (0.0, 0.0),
                geometry(),
                ModifiersState::empty()
            ),
            None
        );
    }

    #[test]
    fn motion_reports_the_held_button() {
        let event = mouse_motion(
            (28.0, 8.0),
            MouseButton::Left,
            geometry(),
            ModifiersState::empty(),
        );
        let expected =
            MouseEvent::new(MouseEventKind::Motion, MouseButton::Left, 2, 0).with_pixels(28.0, 8.0);
        assert_eq!(event, InputEvent::Mouse(expected));
    }

    #[test]
    fn wheel_lines_scroll_into_the_scrollback() {
        let up = scroll(
            MouseScrollDelta::LineDelta(0.0, 3.0),
            (18.0, 28.0),
            geometry(),
            ModifiersState::empty(),
        );
        assert_eq!(
            up,
            InputEvent::Scroll(ScrollEvent::new(3.0).with_cell(1, 1))
        );
        let down = scroll(
            MouseScrollDelta::LineDelta(0.0, -1.5),
            (0.0, 0.0),
            geometry(),
            ModifiersState::empty(),
        );
        assert_eq!(down, InputEvent::Scroll(ScrollEvent::new(-1.5)));
    }

    #[test]
    fn trackpad_pixels_become_precise_lines() {
        let event = scroll(
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(4.0, 50.0)),
            (0.0, 0.0),
            geometry(),
            ModifiersState::CONTROL,
        );
        let expected = ScrollEvent::new(2.5)
            .with_pixels(4.0, 50.0)
            .with_precise(true)
            .with_modifiers(Modifiers::CTRL);
        assert_eq!(event, InputEvent::Scroll(expected));
    }

    #[test]
    fn focus_maps_both_ways() {
        assert_eq!(focus(true), InputEvent::Focus(FocusEvent::Gained));
        assert_eq!(focus(false), InputEvent::Focus(FocusEvent::Lost));
    }
}
