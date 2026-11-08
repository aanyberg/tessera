//! The `[keys]` table: chords bound to commands or to text (#43, ADR 0036).
//!
//! This module owns the notation only: the chord grammar
//! ([`Chord::parse`]), the value grammar ([`KeyAction::parse`]), document
//! order, the caps (docs/security.md S5) and the warnings for entries that
//! break them. Which commands exist, their default chords and what a
//! binding does belong to the command registry in `tessera-app-core`,
//! which reports unknown commands.

use std::fmt;
use std::ops::BitOr;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::load::{MAX_WARNING_PATH_LEN, Warning, escaped};

/// Most `[keys]` entries read; later entries are dropped with one
/// [`Warning::TooManyKeyBindings`] (docs/security.md S5).
pub const MAX_KEY_BINDINGS: usize = 512;

/// Longest chord, in bytes; checked before the grammar.
pub const MAX_CHORD_LEN: usize = 64;

/// Most bytes a `send_text:` value may write, after unescaping.
pub const MAX_SEND_TEXT_LEN: usize = 4096;

/// Longest command name or parameter, in bytes. No registry name comes
/// close; the cap only bounds what an entry may keep.
pub const MAX_COMMAND_NAME_LEN: usize = 64;

/// The modifiers of a [`Chord`]. The bit values are those of
/// `tessera_term::Modifiers` (Shift 1, Alt 2, Ctrl 4, Super 8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChordModifiers(u8);

impl ChordModifiers {
    /// No modifier.
    pub const NONE: Self = Self(0);
    /// Shift.
    pub const SHIFT: Self = Self(1);
    /// Alt / Option.
    pub const ALT: Self = Self(2);
    /// Control.
    pub const CTRL: Self = Self(4);
    /// Super / Command (⌘).
    pub const SUPER: Self = Self(8);

    /// The raw bits, as `tessera_term::Modifiers::from_bits` takes them.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every modifier of `other` is in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// `self` without the modifiers of `other`.
    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Whether no modifier is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for ChordModifiers {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// The key of a [`Chord`]: a character key, always the *unshifted*
/// character (`'/'`, not `'?'`), or a named key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ChordKey {
    /// A key that types a character (`space` is `Char(' ')`).
    Char(char),
    /// Escape (`escape`, `esc`).
    Escape,
    /// Return (`enter`, `return`).
    Enter,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Forward delete.
    Delete,
    /// Insert / Help.
    Insert,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up (`page_up`).
    PageUp,
    /// Page Down (`page_down`).
    PageDown,
    /// A function key, `f1` through `f35`.
    F(u8),
    /// A keypad digit, `kp_0` through `kp_9`.
    KpDigit(u8),
    /// Keypad `.` (`kp_decimal`).
    KpDecimal,
    /// Keypad `/` (`kp_divide`).
    KpDivide,
    /// Keypad `*` (`kp_multiply`).
    KpMultiply,
    /// Keypad `-` (`kp_subtract`).
    KpSubtract,
    /// Keypad `+` (`kp_add`).
    KpAdd,
    /// Keypad Enter (`kp_enter`).
    KpEnter,
    /// Keypad `=` (`kp_equal`).
    KpEqual,
}

impl ChordKey {
    /// Whether this is a keypad key, which a menu key equivalent cannot
    /// tell from the main-row key.
    #[must_use]
    pub const fn is_keypad(self) -> bool {
        matches!(
            self,
            Self::KpDigit(_)
                | Self::KpDecimal
                | Self::KpDivide
                | Self::KpMultiply
                | Self::KpSubtract
                | Self::KpAdd
                | Self::KpEnter
                | Self::KpEqual
        )
    }
}

/// The US-layout shifted symbols and the key they are typed with, the
/// sugar of rule R7: `cmd+?` is `cmd+shift+/`.
const US_SHIFTED: [(char, char); 21] = [
    ('?', '/'),
    ('{', '['),
    ('}', ']'),
    ('+', '='),
    ('~', '`'),
    ('!', '1'),
    ('@', '2'),
    ('#', '3'),
    ('$', '4'),
    ('%', '5'),
    ('^', '6'),
    ('&', '7'),
    ('*', '8'),
    ('(', '9'),
    (')', '0'),
    ('_', '-'),
    (':', ';'),
    ('"', '\''),
    ('<', ','),
    ('>', '.'),
    ('|', '\\'),
];

/// The key `symbol` is typed with on a US layout, with Shift.
fn us_unshifted(symbol: char) -> Option<char> {
    US_SHIFTED
        .iter()
        .find(|&&(shifted, _)| shifted == symbol)
        .map(|&(_, base)| base)
}

/// The symbol `base` types on a US layout with Shift.
fn us_shifted(base: char) -> Option<char> {
    US_SHIFTED
        .iter()
        .find(|&&(_, unshifted)| unshifted == base)
        .map(|&(shifted, _)| shifted)
}

/// Why a chord does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ChordError {
    /// The empty string.
    #[error("empty chord")]
    Empty,
    /// Longer than [`MAX_CHORD_LEN`] bytes.
    #[error("chord longer than {MAX_CHORD_LEN} bytes")]
    TooLong,
    /// Whitespace anywhere (the chord is not trimmed).
    #[error("whitespace in chord")]
    Whitespace,
    /// A control character.
    #[error("control character in chord")]
    Control,
    /// `>`: key sequences (leader keys) are future work.
    #[error("key sequences are not supported yet")]
    Sequence,
    /// A `global:`-style prefix.
    #[error("prefix `{0}:` is not supported")]
    Prefix(String),
    /// `cmd++`: the `+` key is written `plus`.
    #[error("write `plus` for the + key")]
    UsePlus,
    /// `+t`, `cmd++t`.
    #[error("empty segment (write `plus` for the + key)")]
    EmptySegment,
    /// Only modifiers, or a trailing `+`.
    #[error("missing key")]
    MissingKey,
    /// A modifier this notation does not have (`hyper`, `fn`).
    #[error("unsupported modifier `{0}`")]
    UnsupportedModifier(String),
    /// A modifier given twice (aliases count).
    #[error("duplicate modifier `{0}`")]
    DuplicateModifier(String),
    /// An uppercase or titlecase key: Shift is written out.
    #[error("uppercase key `{0}` (write shift+ and the lowercase key)")]
    Uppercase(char),
    /// A function-key character from U+F700..U+F8FF.
    #[error("private-use function-key character; use the key's name")]
    FunctionKeyCharacter,
    /// A key that shows nothing: a bidirectional formatting, zero-width or
    /// other default-ignorable character (U+200B, U+202E, U+FEFF, a
    /// variation selector), which a menu or the Keys pane would show as a
    /// blank or let reorder the text around it. Printed as its `\u{..}`
    /// escape.
    #[error("invisible key `{}` (a bidi, zero-width or other default-ignorable character)", .0.escape_unicode())]
    InvisibleKey(char),
    /// Neither one character nor a key name. The segment is escaped as
    /// warnings are (control, bidi and other default-ignorable characters
    /// as their `\u{..}` escapes) and capped, so it can be shown as is.
    #[error("unknown key `{0}`")]
    UnknownKey(String),
}

/// A key with modifiers, as written in a `[keys]` table key:
/// `cmd+shift+d`, `ctrl+alt+left`, `f5`.
///
/// Parsed by [`Chord::parse`]; [`Display`](fmt::Display) prints the
/// canonical form (modifiers in the order `cmd+ctrl+alt+shift`, then the
/// key), which parses back to the same chord. A US shifted symbol is
/// stored as Shift plus its unshifted key (`cmd+?` is `cmd+shift+/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Chord {
    /// The modifiers held.
    pub modifiers: ChordModifiers,
    /// The key pressed.
    pub key: ChordKey,
}

impl Chord {
    /// Parses `text` with the grammar of ADR 0036: modifiers (`cmd`,
    /// `command`, `super`, `ctrl`, `control`, `alt`, `opt`, `option`,
    /// `shift`, any ASCII case) joined to one key by `+`.
    pub fn parse(text: &str) -> Result<Self, ChordError> {
        if text.len() > MAX_CHORD_LEN {
            return Err(ChordError::TooLong);
        }
        if text.is_empty() {
            return Err(ChordError::Empty);
        }
        for c in text.chars() {
            if c.is_whitespace() {
                return Err(ChordError::Whitespace);
            }
            if c.is_control() {
                return Err(ChordError::Control);
            }
        }
        if text == "+" || text.ends_with("++") {
            return Err(ChordError::UsePlus);
        }
        let segments: Vec<&str> = text.split('+').collect();
        let Some((key, modifiers)) = segments.split_last() else {
            return Err(ChordError::Empty);
        };
        if let Some((prefix, _)) = text.split_once(':')
            && !prefix.is_empty()
            && !prefix.contains('+')
            && prefix.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_')
        {
            return Err(ChordError::Prefix(prefix.to_owned()));
        }
        if modifiers.iter().any(|segment| segment.contains('>'))
            || (key.contains('>') && *key != ">")
        {
            return Err(ChordError::Sequence);
        }
        let mut held = ChordModifiers::NONE;
        for segment in modifiers {
            if segment.is_empty() {
                return Err(ChordError::EmptySegment);
            }
            let Some(modifier) = modifier(segment) else {
                if is_unsupported_modifier(segment) {
                    return Err(ChordError::UnsupportedModifier((*segment).to_owned()));
                }
                return Err(ChordError::UnknownKey(escaped_text(segment)));
            };
            if held.contains(modifier) {
                return Err(ChordError::DuplicateModifier((*segment).to_owned()));
            }
            held = held | modifier;
        }
        if key.is_empty() || modifier(key).is_some() {
            return Err(ChordError::MissingKey);
        }
        let (key, implied) = parse_key(key)?;
        Ok(Self {
            modifiers: held | implied,
            key,
        })
    }

    /// The symbol this chord's key types with Shift on a US layout, when
    /// Shift is held and the key has one: `'?'` for `cmd+shift+/`. The key
    /// map also matches an event that reports this symbol as its key with
    /// Shift not held, and a menu shows it as its key equivalent.
    #[must_use]
    pub fn shifted_symbol(&self) -> Option<char> {
        match self.key {
            ChordKey::Char(base) if self.modifiers.contains(ChordModifiers::SHIFT) => {
                us_shifted(base)
            }
            _ => None,
        }
    }

    /// Whether the chord takes a key away from plain typing (rule R10):
    /// no modifier but Shift and Alt, on a character, Escape, Return, Tab
    /// or Backspace. Such a binding is honoured with a warning.
    #[must_use]
    pub fn shadows_typing(&self) -> bool {
        let typing = self
            .modifiers
            .without(ChordModifiers::SHIFT | ChordModifiers::ALT)
            .is_empty();
        typing
            && matches!(
                self.key,
                ChordKey::Char(_)
                    | ChordKey::Escape
                    | ChordKey::Enter
                    | ChordKey::Tab
                    | ChordKey::Backspace
            )
    }
}

impl std::str::FromStr for Chord {
    type Err = ChordError;

    fn from_str(text: &str) -> Result<Self, ChordError> {
        Self::parse(text)
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (modifier, name) in [
            (ChordModifiers::SUPER, "cmd"),
            (ChordModifiers::CTRL, "ctrl"),
            (ChordModifiers::ALT, "alt"),
            (ChordModifiers::SHIFT, "shift"),
        ] {
            if self.modifiers.contains(modifier) {
                write!(f, "{name}+")?;
            }
        }
        let name = match self.key {
            ChordKey::Char(' ') => "space",
            ChordKey::Char(c) => return write!(f, "{c}"),
            ChordKey::Escape => "escape",
            ChordKey::Enter => "enter",
            ChordKey::Tab => "tab",
            ChordKey::Backspace => "backspace",
            ChordKey::Delete => "delete",
            ChordKey::Insert => "insert",
            ChordKey::Left => "left",
            ChordKey::Right => "right",
            ChordKey::Up => "up",
            ChordKey::Down => "down",
            ChordKey::Home => "home",
            ChordKey::End => "end",
            ChordKey::PageUp => "page_up",
            ChordKey::PageDown => "page_down",
            ChordKey::F(n) => return write!(f, "f{n}"),
            ChordKey::KpDigit(n) => return write!(f, "kp_{n}"),
            ChordKey::KpDecimal => "kp_decimal",
            ChordKey::KpDivide => "kp_divide",
            ChordKey::KpMultiply => "kp_multiply",
            ChordKey::KpSubtract => "kp_subtract",
            ChordKey::KpAdd => "kp_add",
            ChordKey::KpEnter => "kp_enter",
            ChordKey::KpEqual => "kp_equal",
        };
        f.write_str(name)
    }
}

/// The modifier `segment` names, any ASCII case.
fn modifier(segment: &str) -> Option<ChordModifiers> {
    let lower = segment.to_ascii_lowercase();
    Some(match lower.as_str() {
        "cmd" | "command" | "super" => ChordModifiers::SUPER,
        "ctrl" | "control" => ChordModifiers::CTRL,
        "alt" | "opt" | "option" => ChordModifiers::ALT,
        "shift" => ChordModifiers::SHIFT,
        _ => return None,
    })
}

/// Modifiers other notations have and this one rejects (rule R3).
fn is_unsupported_modifier(segment: &str) -> bool {
    matches!(
        segment.to_ascii_lowercase().as_str(),
        "hyper" | "meta" | "fn" | "globe" | "caps_lock" | "num_lock"
    )
}

/// Parses the key segment: a name or one character. Returns the key and
/// the modifiers it implies (Shift for a US shifted symbol, rule R7).
fn parse_key(segment: &str) -> Result<(ChordKey, ChordModifiers), ChordError> {
    let mut chars = segment.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if ('\u{F700}'..='\u{F8FF}').contains(&c) {
            return Err(ChordError::FunctionKeyCharacter);
        }
        if is_invisible(c) {
            return Err(ChordError::InvisibleKey(c));
        }
        let mut lower = c.to_lowercase();
        if lower.next() != Some(c) || lower.next().is_some() {
            return Err(ChordError::Uppercase(c));
        }
        return Ok(match us_unshifted(c) {
            Some(base) => (ChordKey::Char(base), ChordModifiers::SHIFT),
            None => (ChordKey::Char(c), ChordModifiers::NONE),
        });
    }
    let lower = segment.to_ascii_lowercase();
    let char_key = |c| Ok((ChordKey::Char(c), ChordModifiers::NONE));
    let named = |key| Ok((key, ChordModifiers::NONE));
    match lower.as_str() {
        "escape" | "esc" => named(ChordKey::Escape),
        "enter" | "return" => named(ChordKey::Enter),
        "tab" => named(ChordKey::Tab),
        "space" => char_key(' '),
        "backspace" => named(ChordKey::Backspace),
        "delete" => named(ChordKey::Delete),
        "insert" => named(ChordKey::Insert),
        "left" => named(ChordKey::Left),
        "right" => named(ChordKey::Right),
        "up" => named(ChordKey::Up),
        "down" => named(ChordKey::Down),
        "home" => named(ChordKey::Home),
        "end" => named(ChordKey::End),
        "page_up" => named(ChordKey::PageUp),
        "page_down" => named(ChordKey::PageDown),
        "plus" => Ok((ChordKey::Char('='), ChordModifiers::SHIFT)),
        "minus" => char_key('-'),
        "equal" => char_key('='),
        "comma" => char_key(','),
        "period" => char_key('.'),
        "slash" => char_key('/'),
        "backslash" => char_key('\\'),
        "semicolon" => char_key(';'),
        "apostrophe" => char_key('\''),
        "grave_accent" => char_key('`'),
        "left_bracket" => char_key('['),
        "right_bracket" => char_key(']'),
        "kp_decimal" => named(ChordKey::KpDecimal),
        "kp_divide" => named(ChordKey::KpDivide),
        "kp_multiply" => named(ChordKey::KpMultiply),
        "kp_subtract" => named(ChordKey::KpSubtract),
        "kp_add" => named(ChordKey::KpAdd),
        "kp_enter" => named(ChordKey::KpEnter),
        "kp_equal" => named(ChordKey::KpEqual),
        other => {
            if let Some(n) = numbered(other, "f")
                && (1..=35).contains(&n)
            {
                return named(ChordKey::F(n));
            }
            if let Some(n) = numbered(other, "kp_")
                && n <= 9
            {
                return named(ChordKey::KpDigit(n));
            }
            Err(ChordError::UnknownKey(escaped_text(segment)))
        }
    }
}

/// The Unicode `Default_Ignorable_Code_Point` ranges (`DerivedCoreProperties`,
/// Unicode 16), inclusive and sorted: the characters a renderer shows as
/// nothing, the bidirectional formatting characters among them (U+061C,
/// U+200E, U+200F, U+202A..=U+202E, U+2066..=U+2069).
const DEFAULT_IGNORABLE: [(u32, u32); 17] = [
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x061C, 0x061C),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFF8),
    (0x1_BCA0, 0x1_BCA3),
    (0x1_D173, 0x1_D17A),
    (0xE_0000, 0xE_0FFF),
];

/// Whether `c` shows nothing: a default-ignorable character
/// ([`DEFAULT_IGNORABLE`]), the bidi, zero-width and variation-selector
/// characters included. Control and white space characters are refused
/// before a chord gets this far.
fn is_invisible(c: char) -> bool {
    let c = u32::from(c);
    let i = DEFAULT_IGNORABLE.partition_point(|&(_, last)| last < c);
    DEFAULT_IGNORABLE
        .get(i)
        .is_some_and(|&(first, _)| first <= c)
}

/// `text` for a `[keys]` warning: as [`escaped`] writes it (control, line
/// separator and bidi characters as escapes, cut to
/// [`MAX_WARNING_PATH_LEN`] bytes with a marker), with every other
/// invisible character ([`is_invisible`]: zero-width, variation selectors,
/// the other default-ignorables) written as its `\u{..}` escape too, so a
/// warning never carries a character that shows as nothing. Only the
/// first [`MAX_WARNING_PATH_LEN`] bytes or so are looked at, so a huge key
/// costs no more than a short one.
pub(crate) fn escaped_text(text: &str) -> String {
    // One byte past the cap, rounded up to a character boundary: a longer
    // `text` still ends up longer than the cap, so `escaped` cuts it and
    // adds its marker.
    let mut end = text.len().min(MAX_WARNING_PATH_LEN + 1);
    while !text.is_char_boundary(end) {
        end += 1;
    }
    let mut head = String::with_capacity(end);
    for c in text[..end].chars() {
        if is_invisible(c) {
            head.extend(c.escape_unicode());
        } else {
            head.push(c);
        }
    }
    escaped(&head, MAX_WARNING_PATH_LEN)
}

/// `n` when `text` is `prefix` followed by `n` in decimal without a
/// leading zero.
fn numbered(text: &str, prefix: &str) -> Option<u8> {
    let digits = text.strip_prefix(prefix)?;
    let n: u8 = digits.parse().ok()?;
    (n.to_string() == digits).then_some(n)
}

/// What a `[keys]` entry binds its chord to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyAction {
    /// A command of the registry, with its parameter (`select_tab:3`).
    /// Whether the command exists is checked by the registry.
    Command {
        /// The command name, `[a-z][a-z0-9_]*`.
        name: String,
        /// The text after `:`, if any.
        parameter: Option<String>,
    },
    /// `send_text:<escaped>`: these bytes are written to the PTY. Trusted
    /// configuration: they are not sanitised (docs/security.md S4).
    SendText(Vec<u8>),
    /// `unbound`: the chord does nothing it did by default; the key is
    /// typed as if it were not bound.
    Unbound,
    /// `none`: the key is swallowed, nothing is written.
    Ignore,
}

/// Why a `[keys]` value does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyActionError {
    /// The empty string.
    #[error("empty value (use `unbound` or `none`)")]
    Empty,
    /// Not `[a-z][a-z0-9_]*`, or longer than [`MAX_COMMAND_NAME_LEN`].
    #[error("unknown command `{0}`")]
    InvalidCommand(String),
    /// A parameter that is empty, too long or not `[a-z0-9_]`.
    #[error("invalid parameter `{0}`")]
    InvalidParameter(String),
    /// `send_text:` with nothing after it.
    #[error("empty send_text (use `none` to swallow a key)")]
    EmptySendText,
    /// More than [`MAX_SEND_TEXT_LEN`] bytes after unescaping.
    #[error("send_text longer than {MAX_SEND_TEXT_LEN} bytes")]
    SendTextTooLong,
    /// A malformed escape in a `send_text:` value.
    #[error("{0}")]
    Escape(&'static str),
}

impl KeyAction {
    /// Parses a `[keys]` value: a command name with an optional
    /// `:parameter`, `send_text:<escaped>`, `unbound` or `none`.
    pub fn parse(text: &str) -> Result<Self, KeyActionError> {
        match text {
            "" => return Err(KeyActionError::Empty),
            "unbound" => return Ok(Self::Unbound),
            "none" => return Ok(Self::Ignore),
            _ => {}
        }
        if let Some(escaped) = text.strip_prefix("send_text:") {
            if escaped.is_empty() {
                return Err(KeyActionError::EmptySendText);
            }
            return unescape(escaped).map(Self::SendText);
        }
        let (name, parameter) = match text.split_once(':') {
            Some((name, parameter)) => (name, Some(parameter)),
            None => (text, None),
        };
        let valid_name = name.len() <= MAX_COMMAND_NAME_LEN
            && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !valid_name {
            return Err(KeyActionError::InvalidCommand(escaped_text(name)));
        }
        if let Some(parameter) = parameter {
            let valid = !parameter.is_empty()
                && parameter.len() <= MAX_COMMAND_NAME_LEN
                && parameter
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if !valid {
                return Err(KeyActionError::InvalidParameter(escaped_text(parameter)));
            }
        }
        Ok(Self::Command {
            name: name.to_owned(),
            parameter: parameter.map(str::to_owned),
        })
    }
}

/// Decodes the escapes of a `send_text:` value (rule R13), failing as soon
/// as the result would pass [`MAX_SEND_TEXT_LEN`] bytes.
fn unescape(text: &str) -> Result<Vec<u8>, KeyActionError> {
    fn push(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), KeyActionError> {
        if out.len() + bytes.len() > MAX_SEND_TEXT_LEN {
            return Err(KeyActionError::SendTextTooLong);
        }
        out.extend_from_slice(bytes);
        Ok(())
    }
    fn hex(digits: &str) -> Option<u32> {
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(digits, 16).ok()
    }
    fn scalar(out: &mut Vec<u8>, value: u32) -> Result<(), KeyActionError> {
        if (0xD800..=0xDFFF).contains(&value) {
            return Err(KeyActionError::Escape("surrogate in \\u escape"));
        }
        let c = char::from_u32(value).ok_or(KeyActionError::Escape("\\u escape out of range"))?;
        push(out, c.encode_utf8(&mut [0; 4]).as_bytes())
    }

    let mut out = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        if c != '\\' {
            push(&mut out, c.encode_utf8(&mut [0; 4]).as_bytes())?;
            continue;
        }
        let Some(escape) = rest.chars().next() else {
            return Err(KeyActionError::Escape("trailing backslash"));
        };
        rest = &rest[escape.len_utf8()..];
        let byte = match escape {
            'a' => 0x07,
            'b' => 0x08,
            'e' => 0x1b,
            'f' => 0x0c,
            'n' => b'\n',
            'r' => b'\r',
            't' => b'\t',
            'v' => 0x0b,
            '\\' => b'\\',
            '"' => b'"',
            '\'' => b'\'',
            '0'..='7' => {
                return Err(KeyActionError::Escape(
                    "octal escapes are not supported (write \\x00)",
                ));
            }
            'x' => {
                let digits = rest
                    .get(..2)
                    .ok_or(KeyActionError::Escape("truncated \\x escape"))?;
                let value = hex(digits).ok_or(KeyActionError::Escape("bad hex in \\x escape"))?;
                rest = &rest[2..];
                push(&mut out, &[u8::try_from(value).expect("two hex digits")])?;
                continue;
            }
            'u' => {
                if let Some(braced) = rest.strip_prefix('{') {
                    let end = braced
                        .find('}')
                        .ok_or(KeyActionError::Escape("truncated \\u{} escape"))?;
                    let digits = &braced[..end];
                    if digits.len() > 6 {
                        return Err(KeyActionError::Escape("\\u escape out of range"));
                    }
                    let value =
                        hex(digits).ok_or(KeyActionError::Escape("bad hex in \\u{} escape"))?;
                    rest = &braced[end + 1..];
                    scalar(&mut out, value)?;
                } else {
                    let digits = rest
                        .get(..4)
                        .filter(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))
                        .ok_or(KeyActionError::Escape("truncated \\u escape"))?;
                    let value = hex(digits).expect("four hex digits");
                    rest = &rest[4..];
                    scalar(&mut out, value)?;
                }
                continue;
            }
            _ => return Err(KeyActionError::Escape("unknown escape")),
        };
        push(&mut out, &[byte])?;
    }
    Ok(out)
}

/// One accepted `[keys]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    /// The table key as written (`"shift+cmd+d"`).
    pub key: String,
    /// The parsed chord.
    pub chord: Chord,
    /// The value as written (`"split_down"`), for writing the table back.
    pub value: String,
    /// The parsed value.
    pub action: KeyAction,
}

/// The `[keys]` table: chords bound to commands or text, in document
/// order. Built by deserializing; entries that break the grammar or the
/// caps are left out and reported by [`Keys::take_warnings`].
#[derive(Debug, Clone, Default)]
pub struct Keys {
    bindings: Vec<KeyBinding>,
    /// Warnings with the byte offset of their key in the document, for
    /// [`Config::from_toml_lenient`](crate::Config::from_toml_lenient)
    /// to sort; not part of the value.
    warnings: Vec<(usize, Warning)>,
    /// The entries as deserialized (a value that is not a string is
    /// `None`), kept only until [`Keys::put_in_document_order`] rebuilds
    /// the table from them and drops them: the TOML parser hands a table
    /// over sorted by key, and only the loader knows where each key was.
    /// Empty after a load, so a table at the file cap keeps no more than
    /// its [`MAX_KEY_BINDINGS`] entries (docs/security.md S5). Not part of
    /// the value.
    source: Vec<(String, Option<String>)>,
}

/// Equal when both bind the same entries in the same order: the order
/// decides which chord a command's menu item shows (its first), so a
/// configuration that only reorders the table is a change.
impl PartialEq for Keys {
    fn eq(&self, other: &Self) -> bool {
        self.bindings == other.bindings
    }
}

impl Keys {
    /// Builds a table from `(key, value)` entries in order, as if they had
    /// been written in a `[keys]` table: the same checks, caps and
    /// warnings as a loaded file.
    pub fn from_entries<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let source: Vec<(String, Option<String>)> = entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), Some(value.to_owned())))
            .collect();
        let mut keys = Self::default();
        keys.build(&source, |_| None);
        keys
    }

    /// Rebuilds the table from the entries it was deserialized from, in
    /// the order of `at`: the byte offset of each key in the document
    /// (`None` sorts last). Duplicates and the cap are decided in that
    /// order, so the first entry *written* wins. The entries are dropped
    /// then, so a second call (or a table not deserialized) does nothing.
    pub(crate) fn put_in_document_order(&mut self, at: impl Fn(&str) -> Option<usize>) {
        let source = std::mem::take(&mut self.source);
        if !source.is_empty() {
            self.build(&source, at);
        }
    }

    /// Checks `source` in the order of `at` (stable), keeping at most
    /// [`MAX_KEY_BINDINGS`] entries. Keeps nothing of `source` but the
    /// entries it accepts.
    fn build(&mut self, source: &[(String, Option<String>)], at: impl Fn(&str) -> Option<usize>) {
        let mut order: Vec<(usize, usize)> = source
            .iter()
            .enumerate()
            .map(|(index, (key, _))| (at(key).unwrap_or(usize::MAX), index))
            .collect();
        order.sort_by_key(|&(at, _)| at);
        self.bindings.clear();
        self.warnings.clear();
        let dropped = order.len().saturating_sub(MAX_KEY_BINDINGS);
        for &(at, index) in order.iter().take(MAX_KEY_BINDINGS) {
            let (key, value) = &source[index];
            self.add(at, key, value.as_deref().ok_or(()));
        }
        if dropped > 0 {
            let at = order[MAX_KEY_BINDINGS].0;
            self.warnings
                .push((at, Warning::TooManyKeyBindings { dropped }));
        }
        self.warnings.sort_by_key(|&(at, _)| at);
    }

    /// The accepted entries, in document order. No two have the same
    /// chord, and there are at most [`MAX_KEY_BINDINGS`].
    #[must_use]
    pub fn bindings(&self) -> &[KeyBinding] {
        &self.bindings
    }

    /// Whether the table has no accepted entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// The warnings of the entries left out (and of those honoured with a
    /// caveat), in document order; empties the list.
    pub fn take_warnings(&mut self) -> Vec<Warning> {
        std::mem::take(&mut self.warnings)
            .into_iter()
            .map(|(_, warning)| warning)
            .collect()
    }

    /// Like [`Keys::take_warnings`], with each warning's byte offset in
    /// the document.
    pub(crate) fn take_placed_warnings(&mut self) -> Vec<(usize, Warning)> {
        std::mem::take(&mut self.warnings)
    }

    /// Checks one entry found at byte offset `at` (or index, for
    /// [`Keys::from_entries`]); `value` is `Err` when it is not a string.
    fn add(&mut self, at: usize, key: &str, value: Result<&str, ()>) {
        let warn = |reason: String| {
            (
                at,
                Warning::KeyBinding {
                    key: escaped_text(key),
                    reason,
                },
            )
        };
        let chord = match Chord::parse(key) {
            Ok(chord) => chord,
            Err(error) => {
                self.warnings.push(warn(format!("{error}; ignored")));
                return;
            }
        };
        let Ok(value) = value else {
            self.warnings
                .push(warn("value must be a string; ignored".into()));
            return;
        };
        let action = match KeyAction::parse(value) {
            Ok(action) => action,
            Err(error) => {
                self.warnings.push(warn(format!("{error}; ignored")));
                return;
            }
        };
        if let Some(first) = self.bindings.iter().find(|b| b.chord == chord) {
            let first = escaped_text(&first.key);
            self.warnings.push(warn(format!(
                "same chord as `{first}` (the first entry wins); ignored"
            )));
            return;
        }
        if chord.shadows_typing() && action != KeyAction::Unbound {
            self.warnings.push(warn(
                "shadows plain typing of this key; honoured".to_owned(),
            ));
        }
        self.bindings.push(KeyBinding {
            key: key.to_owned(),
            chord,
            value: value.to_owned(),
            action,
        });
    }
}

impl Serialize for Keys {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.bindings.len()))?;
        for binding in &self.bindings {
            map.serialize_entry(&binding.key, &binding.value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Keys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeysVisitor;

        impl<'de> Visitor<'de> for KeysVisitor {
            type Value = Keys;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a table of chords")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Keys, A::Error> {
                // In the parser's order, which is sorted by key rather
                // than the document's: the loader puts it right
                // (`Keys::put_in_document_order`).
                let mut source = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    let value: toml::Value = map.next_value()?;
                    source.push((key, value.as_str().map(str::to_owned)));
                }
                let mut keys = Keys::default();
                keys.build(&source, |_| None);
                keys.source = source;
                Ok(keys)
            }
        }

        deserializer.deserialize_map(KeysVisitor)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for Keys {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Keys".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "object",
            "additionalProperties": { "type": "string" }
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_bits_match_the_terminal() {
        // tessera_term::Modifiers: SHIFT 1, ALT 2, CTRL 4, SUPER 8.
        assert_eq!(ChordModifiers::SHIFT.bits(), 1);
        assert_eq!(ChordModifiers::ALT.bits(), 2);
        assert_eq!(ChordModifiers::CTRL.bits(), 4);
        assert_eq!(ChordModifiers::SUPER.bits(), 8);
    }

    #[test]
    fn shifted_symbols_round_trip() {
        for (symbol, base) in US_SHIFTED {
            if symbol == '+' {
                // Written `plus`: `cmd++` is an error.
                continue;
            }
            let chord = Chord::parse(&format!("cmd+{symbol}")).unwrap();
            assert_eq!(chord.key, ChordKey::Char(base), "{symbol}");
            assert_eq!(chord.shifted_symbol(), Some(symbol));
        }
        assert_eq!(Chord::parse("cmd+=").unwrap().shifted_symbol(), None);
        assert_eq!(Chord::parse("cmd+shift+a").unwrap().shifted_symbol(), None);
    }

    #[test]
    fn document_order_survives_the_sorted_table() {
        let loaded = crate::Config::from_toml_lenient(
            "[keys]\n'cmd+z' = 'new_tab'\n'cmd+a' = 'new_window'\n'shift+cmd+z' = 'copy'\n'cmd+shift+z' = 'paste'\n",
        )
        .unwrap();
        let keys: Vec<&str> = loaded
            .config
            .keys
            .bindings()
            .iter()
            .map(|b| b.key.as_str())
            .collect();
        assert_eq!(keys, ["cmd+z", "cmd+a", "shift+cmd+z"]);
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert!(
            loaded.warnings[0].to_string().contains("cmd+shift+z"),
            "{}",
            loaded.warnings[0]
        );
    }

    #[test]
    fn a_value_that_is_not_a_string_is_one_warning() {
        let loaded = crate::Config::from_toml_lenient("[keys]\n'cmd+t' = 5\n").unwrap();
        assert!(loaded.config.keys.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].to_string().contains("cmd+t"));
    }

    #[test]
    fn the_invisible_table_is_sorted_and_matches_known_characters() {
        for pair in DEFAULT_IGNORABLE.windows(2) {
            assert!(pair[0].0 <= pair[0].1 && pair[0].1 < pair[1].0, "{pair:X?}");
        }
        for c in [
            '\u{AD}',
            '\u{34F}',
            '\u{61C}',
            '\u{115F}',
            '\u{180E}',
            '\u{200B}',
            '\u{200D}',
            '\u{200E}',
            '\u{202E}',
            '\u{2060}',
            '\u{2066}',
            '\u{2069}',
            '\u{3164}',
            '\u{FE0F}',
            '\u{FEFF}',
            '\u{1D173}',
            '\u{E0001}',
            '\u{E0FFF}',
        ] {
            assert!(is_invisible(c), "U+{:04X}", u32::from(c));
        }
        for c in [
            'a',
            '-',
            '\u{A0}',
            '\u{301}',
            '\u{2010}',
            '\u{2028}',
            '\u{2070}',
            '\u{FE10}',
            '\u{FFF9}',
            '\u{E1000}',
            'é',
            'т',
        ] {
            assert!(!is_invisible(c), "U+{:04X}", u32::from(c));
        }
    }

    /// Warning text escapes what `escaped` does and every invisible
    /// character too, and stays bounded however long the key.
    #[test]
    fn warning_text_escapes_invisible_characters_and_is_bounded() {
        assert_eq!(
            escaped_text("a\u{200B}b\u{202E}c\u{1}d\u{FEFF}"),
            "a\\u{200b}b\\u{202e}c\\u{1}d\\u{feff}"
        );
        assert_eq!(escaped_text("cmd+t"), "cmd+t");
        for long in [
            "\u{200B}".repeat(1 << 20),
            "x".repeat(1 << 20),
            "é".repeat(1 << 20),
        ] {
            let out = escaped_text(&long);
            assert!(out.len() <= MAX_WARNING_PATH_LEN + 8, "{}", out.len());
            assert!(!out.contains('\u{200B}'));
        }
    }

    /// M3: a `[keys]` table filling the config file cap keeps only the
    /// accepted entries once loaded: the deserialized entries are dropped
    /// (none left in the table or in a clone), whatever the count.
    #[test]
    fn a_table_at_the_file_cap_keeps_no_source_entries() {
        /// A `[keys]` table of `'k<n>' = 'none'` lines, `len` bytes at most.
        fn table(len: usize) -> String {
            let mut text = String::with_capacity(len);
            text.push_str("[keys]\n");
            for i in 0.. {
                let line = format!("'k{i}' = 'none'\n");
                if text.len() + line.len() > len {
                    break;
                }
                text.push_str(&line);
            }
            text
        }
        let full = table(usize::try_from(crate::MAX_CONFIG_FILE_LEN).unwrap());
        assert!(full.lines().count() > 500_000);
        // The strict loader at a smaller size: the file cap costs seconds
        // in a debug build.
        let small = table(1 << 20);
        for keys in [
            crate::Config::from_toml_lenient(&full).unwrap().config.keys,
            crate::Config::from_toml(&small).unwrap().keys,
        ] {
            assert_eq!(keys.source.len(), 0);
            assert_eq!(keys.source.capacity(), 0, "the entries are freed");
            assert_eq!(keys.clone().source.capacity(), 0);
            assert!(keys.bindings.is_empty(), "every `k<n>` is an unknown key");
            assert!(keys.warnings.len() <= MAX_KEY_BINDINGS + 1);
        }
    }

    #[test]
    fn keys_round_trip_through_toml() {
        let config = crate::Config {
            keys: Keys::from_entries([("cmd+shift+d", "split_down"), ("cmd+a", "send_text:\\e[A")]),
            ..crate::Config::default()
        };
        let text = config.to_toml().unwrap();
        assert_eq!(crate::Config::from_toml(&text).unwrap(), config, "{text}");
    }
}
