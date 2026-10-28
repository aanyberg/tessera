//! Table-driven tests for key encoding.
//!
//! The tables are transcribed from the kitty keyboard protocol specification
//! (<https://sw.kovidgoyal.net/kitty/keyboard-protocol/>): the quickstart
//! forms, the documented examples, "Legacy functional encoding", "C0
//! controls", "Example encodings", "Functional key codes" and "Legacy ctrl
//! mapping of ASCII keys". Legacy mode is additionally checked against what
//! xterm sends for printable, Ctrl and Alt combinations.

use std::fmt::Write as _;

use proptest::prelude::*;
use tessera_term::{
    Key, KeyEvent, KeyEventKind, KeyboardModes, KittyFlags, KittyFlagsMode, Modifiers,
    ModifyOtherKeys, encode_key,
};

const NONE: Modifiers = Modifiers::NONE;
const SHIFT: Modifiers = Modifiers::SHIFT;
const ALT: Modifiers = Modifiers::ALT;
const CTRL: Modifiers = Modifiers::CTRL;
const SUPER: Modifiers = Modifiers::SUPER;

fn legacy() -> KeyboardModes {
    KeyboardModes::default()
}

fn kitty(flags: KittyFlags) -> KeyboardModes {
    KeyboardModes {
        kitty_flags: flags,
        ..KeyboardModes::default()
    }
}

fn enc(key: Key, modifiers: Modifiers, modes: KeyboardModes) -> Vec<u8> {
    encode_key(&KeyEvent::new(key, modifiers), &modes)
}

/// Renders bytes the way the spec writes them: `CSI`, `SS3`, `ESC`, printable
/// characters verbatim and other bytes as hex.
fn show(bytes: &[u8]) -> String {
    let mut s = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i..] {
            [0x1b, b'[', _, ..] => {
                s.push_str("CSI ");
                i += 2;
            }
            [0x1b, b'O', _, ..] => {
                s.push_str("SS3 ");
                i += 2;
            }
            [0x1b, ..] => {
                s.push_str("ESC ");
                i += 1;
            }
            [b, ..] if b.is_ascii_graphic() => {
                s.push(char::from(b));
                i += 1;
            }
            [b, ..] => {
                let _ = write!(s, "0x{b:02x} ");
                i += 1;
            }
            [] => unreachable!(),
        }
    }
    s.trim_end().to_owned()
}

#[track_caller]
fn check(key: Key, modifiers: Modifiers, modes: KeyboardModes, expected: &[u8]) {
    let got = enc(key, modifiers, modes);
    assert_eq!(
        got,
        expected,
        "{key:?} + {modifiers:?}: got `{}`, expected `{}`",
        show(&got),
        show(expected)
    );
}

// ---------------------------------------------------------------------------
// Spec: Legacy functional encoding
// ---------------------------------------------------------------------------

#[test]
fn legacy_functional_encoding_table() {
    let table: &[(Key, &[u8])] = &[
        (Key::Insert, b"\x1b[2~"),
        (Key::Delete, b"\x1b[3~"),
        (Key::PageUp, b"\x1b[5~"),
        (Key::PageDown, b"\x1b[6~"),
        (Key::Up, b"\x1b[A"),
        (Key::Down, b"\x1b[B"),
        (Key::Right, b"\x1b[C"),
        (Key::Left, b"\x1b[D"),
        (Key::Home, b"\x1b[H"),
        (Key::End, b"\x1b[F"),
        (Key::F(1), b"\x1bOP"),
        (Key::F(2), b"\x1bOQ"),
        (Key::F(3), b"\x1bOR"),
        (Key::F(4), b"\x1bOS"),
        (Key::F(5), b"\x1b[15~"),
        (Key::F(6), b"\x1b[17~"),
        (Key::F(7), b"\x1b[18~"),
        (Key::F(8), b"\x1b[19~"),
        (Key::F(9), b"\x1b[20~"),
        (Key::F(10), b"\x1b[21~"),
        (Key::F(11), b"\x1b[23~"),
        (Key::F(12), b"\x1b[24~"),
        (Key::Menu, b"\x1b[29~"),
    ];
    for &(key, expected) in table {
        check(key, NONE, legacy(), expected);
    }
}

#[test]
fn legacy_cursor_key_mode_uses_ss3_only_without_modifiers() {
    let modes = KeyboardModes {
        application_cursor_keys: true,
        ..legacy()
    };
    let table: &[(Key, &[u8])] = &[
        (Key::Up, b"\x1bOA"),
        (Key::Down, b"\x1bOB"),
        (Key::Right, b"\x1bOC"),
        (Key::Left, b"\x1bOD"),
        (Key::Home, b"\x1bOH"),
        (Key::End, b"\x1bOF"),
        (Key::KpBegin, b"\x1bOE"),
    ];
    for &(key, expected) in table {
        check(key, NONE, modes, expected);
    }
    // With modifiers the second form is used regardless of DECCKM.
    check(Key::Up, SHIFT, modes, b"\x1b[1;2A");
    check(Key::Left, CTRL | ALT, modes, b"\x1b[1;7D");
    check(Key::Up, SHIFT, legacy(), b"\x1b[1;2A");
}

#[test]
fn legacy_functional_keys_with_modifiers() {
    // "The third form becomes the second form when modifiers are present."
    check(Key::F(1), SHIFT, legacy(), b"\x1b[1;2P");
    check(Key::F(3), CTRL, legacy(), b"\x1b[1;5R");
    check(Key::F(5), ALT, legacy(), b"\x1b[15;3~");
    check(Key::Insert, SHIFT | CTRL, legacy(), b"\x1b[2;6~");
    // Super, Hyper and Meta are all xterm's Meta bit (8), as in
    // modifyOtherKeys, so every sequence is one xterm could send.
    check(Key::Delete, SUPER, legacy(), b"\x1b[3;9~");
    check(Key::Delete, Modifiers::HYPER, legacy(), b"\x1b[3;9~");
    check(Key::Up, Modifiers::META | CTRL, legacy(), b"\x1b[1;13A");
    check(Key::F(1), Modifiers::HYPER | SHIFT, legacy(), b"\x1b[1;10P");
    // Lock modifiers are not encoded in legacy mode.
    check(Key::Up, Modifiers::CAPS_LOCK, legacy(), b"\x1b[A");
    check(Key::Up, Modifiers::NUM_LOCK | SHIFT, legacy(), b"\x1b[1;2A");
}

/// `kf13` through `kf35` as declared in
/// `shell-integration/terminfo/tessera.terminfo` (and identically in
/// xterm-256color): xterm's PC-style aliases, F13 = Shift+F1, F25 = Ctrl+F1.
/// ADR 0005 makes the terminfo entry the source of truth, so this table must
/// be kept in sync with it.
const TERMINFO_F13_TO_F35: &[(u8, &[u8])] = &[
    (13, b"\x1b[1;2P"),
    (14, b"\x1b[1;2Q"),
    (15, b"\x1b[1;2R"),
    (16, b"\x1b[1;2S"),
    (17, b"\x1b[15;2~"),
    (18, b"\x1b[17;2~"),
    (19, b"\x1b[18;2~"),
    (20, b"\x1b[19;2~"),
    (21, b"\x1b[20;2~"),
    (22, b"\x1b[21;2~"),
    (23, b"\x1b[23;2~"),
    (24, b"\x1b[24;2~"),
    (25, b"\x1b[1;5P"),
    (26, b"\x1b[1;5Q"),
    (27, b"\x1b[1;5R"),
    (28, b"\x1b[1;5S"),
    (29, b"\x1b[15;5~"),
    (30, b"\x1b[17;5~"),
    (31, b"\x1b[18;5~"),
    (32, b"\x1b[19;5~"),
    (33, b"\x1b[20;5~"),
    (34, b"\x1b[21;5~"),
    (35, b"\x1b[23;5~"),
];

#[test]
fn legacy_f13_to_f35_match_terminfo() {
    for &(n, expected) in TERMINFO_F13_TO_F35 {
        check(Key::F(n), NONE, legacy(), expected);
    }
    // Modifiers held with F13+ fold into the same parameter, as in xterm and
    // the terminfo entry's higher numbers (`kf37=\E[1;6P` is Ctrl+F13,
    // `kf61=\E[1;4P` is Alt+F13).
    check(Key::F(13), CTRL, legacy(), b"\x1b[1;6P");
    check(Key::F(13), ALT, legacy(), b"\x1b[1;4P");
    check(Key::F(35), SHIFT, legacy(), b"\x1b[23;6~");
    check(Key::F(24), SHIFT, legacy(), b"\x1b[24;2~");
    // In kitty mode they keep their own key codes.
    let modes = kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES);
    check(Key::F(13), NONE, modes, b"\x1b[57376u");
    check(Key::F(35), SHIFT, modes, b"\x1b[57398;2u");
}

#[test]
fn legacy_keys_without_terminfo_entry_use_csi_u() {
    // "kitty chooses to encode these using CSI u encoding even in legacy mode"
    check(Key::MediaPlay, NONE, legacy(), b"\x1b[57428u");
    check(Key::MuteVolume, NONE, legacy(), b"\x1b[57440u");
    check(Key::PrintScreen, NONE, legacy(), b"\x1b[57361u");
    // Bare modifier keys and releases produce nothing in legacy mode.
    check(Key::LeftShift, SHIFT, legacy(), b"");
    let release = KeyEvent::new(Key::Up, NONE).with_kind(KeyEventKind::Release);
    assert_eq!(encode_key(&release, &legacy()), b"");
    let repeat = KeyEvent::new(Key::Up, NONE).with_kind(KeyEventKind::Repeat);
    assert_eq!(encode_key(&repeat, &legacy()), b"\x1b[A");
    // Unknown function keys encode to nothing.
    check(Key::F(0), NONE, legacy(), b"");
    check(Key::F(36), NONE, legacy(), b"");
}

#[test]
fn legacy_keypad_keys_report_as_their_equivalents() {
    // "All keypad keys are reported as their equivalent non-keypad keys."
    check(Key::Kp7, NONE, legacy(), b"7");
    check(Key::KpAdd, NONE, legacy(), b"+");
    check(Key::KpDecimal, NONE, legacy(), b".");
    check(Key::KpEnter, NONE, legacy(), b"\r");
    check(Key::KpEnter, ALT, legacy(), b"\x1b\r");
    check(Key::KpLeft, NONE, legacy(), b"\x1b[D");
    check(Key::KpHome, SHIFT, legacy(), b"\x1b[1;2H");
    check(Key::KpDelete, NONE, legacy(), b"\x1b[3~");
    check(Key::KpBegin, NONE, legacy(), b"\x1b[E");
    check(Key::Kp3, CTRL, legacy(), b"\x1b");
}

#[test]
fn application_keypad_mode_sends_ss3() {
    let modes = KeyboardModes {
        application_keypad: true,
        ..legacy()
    };
    let table: &[(Key, u8)] = &[
        (Key::Kp0, b'p'),
        (Key::Kp1, b'q'),
        (Key::Kp2, b'r'),
        (Key::Kp3, b's'),
        (Key::Kp4, b't'),
        (Key::Kp5, b'u'),
        (Key::Kp6, b'v'),
        (Key::Kp7, b'w'),
        (Key::Kp8, b'x'),
        (Key::Kp9, b'y'),
        (Key::KpMultiply, b'j'),
        (Key::KpAdd, b'k'),
        (Key::KpSeparator, b'l'),
        (Key::KpSubtract, b'm'),
        (Key::KpDecimal, b'n'),
        (Key::KpDivide, b'o'),
        (Key::KpEnter, b'M'),
        (Key::KpEqual, b'X'),
    ];
    for &(key, byte) in table {
        check(key, NONE, modes, &[0x1b, b'O', byte]);
    }
    // Modified keypad keys fall back to their main-keyboard equivalents.
    check(Key::Kp1, ALT, modes, b"\x1b1");
    // Main keyboard digits are unaffected.
    check(Key::Char('1'), NONE, modes, b"1");
    // Keypad navigation keys still follow DECCKM, not DECKPAM.
    check(Key::KpUp, NONE, modes, b"\x1b[A");
}

// ---------------------------------------------------------------------------
// Spec: C0 controls
// ---------------------------------------------------------------------------

#[test]
fn c0_controls_table() {
    // Columns: no mods, ctrl, alt, shift, ctrl+shift, alt+shift, ctrl+alt.
    let columns = [
        NONE,
        CTRL,
        ALT,
        SHIFT,
        CTRL | SHIFT,
        ALT | SHIFT,
        CTRL | ALT,
    ];
    let rows: &[(Key, [&[u8]; 7])] = &[
        (
            Key::Enter,
            [
                b"\x0d",
                b"\x0d",
                b"\x1b\x0d",
                b"\x0d",
                b"\x0d",
                b"\x1b\x0d",
                b"\x1b\x0d",
            ],
        ),
        (
            Key::Escape,
            [
                b"\x1b",
                b"\x1b",
                b"\x1b\x1b",
                b"\x1b",
                b"\x1b",
                b"\x1b\x1b",
                b"\x1b\x1b",
            ],
        ),
        (
            Key::Backspace,
            [
                b"\x7f",
                b"\x08",
                b"\x1b\x7f",
                b"\x7f",
                b"\x08",
                b"\x1b\x7f",
                b"\x1b\x08",
            ],
        ),
        (
            Key::Tab,
            [
                b"\x09",
                b"\x09",
                b"\x1b\x09",
                b"\x1b[Z",
                b"\x1b[Z",
                b"\x1b\x1b[Z",
                b"\x1b\x09",
            ],
        ),
        (
            Key::Char(' '),
            [
                b"\x20",
                b"\x00",
                b"\x1b\x20",
                b"\x20",
                b"\x00",
                b"\x1b\x20",
                b"\x1b\x00",
            ],
        ),
    ];
    for &(key, expected) in rows {
        for (mods, expected) in columns.into_iter().zip(expected) {
            check(key, mods, legacy(), expected);
        }
    }
    // Combinations outside the table have no legacy form.
    check(Key::Enter, SUPER, legacy(), b"\x1b[13;9u");
    check(Key::Tab, CTRL | ALT | SHIFT, legacy(), b"\x1b\x1b[Z");
}

// ---------------------------------------------------------------------------
// Spec: Legacy text keys / Example encodings
// ---------------------------------------------------------------------------

#[test]
fn legacy_text_key_example_encodings() {
    // Columns: plain, shift, alt, ctrl, shift+alt, alt+ctrl, ctrl+shift.
    //
    // The ctrl+shift column deliberately differs from the spec's table
    // (`CSI 105;6u` etc.): legacy mode follows xterm, where Ctrl+Shift applies
    // the ctrl mapping to the shifted character (see the module docs).
    let columns = [
        NONE,
        SHIFT,
        ALT,
        CTRL,
        SHIFT | ALT,
        ALT | CTRL,
        CTRL | SHIFT,
    ];
    let rows: &[(char, [&[u8]; 7])] = &[
        (
            'i',
            [
                b"i",
                b"I",
                b"\x1bi",
                b"\x09",
                b"\x1bI",
                b"\x1b\x09",
                b"\x09",
            ],
        ),
        (
            '3',
            [b"3", b"#", b"\x1b3", b"\x1b", b"\x1b#", b"\x1b\x1b", b"#"],
        ),
        (';', [b";", b":", b"\x1b;", b";", b"\x1b:", b"\x1b;", b":"]),
    ];
    for &(c, expected) in rows {
        for (mods, expected) in columns.into_iter().zip(expected) {
            check(Key::Char(c), mods, legacy(), expected);
        }
    }
}

#[test]
fn legacy_text_keys_outside_the_algorithm_use_csi_u() {
    check(Key::Char('a'), SUPER, legacy(), b"\x1b[97;9u");
    check(
        Key::Char('a'),
        Modifiers::HYPER | CTRL,
        legacy(),
        b"\x1b[97;21u",
    );
    // Ctrl with a non-ASCII key (and no base-layout key) has no C0 mapping.
    check(Key::Char('é'), CTRL, legacy(), b"\x1b[233;5u");
    // ... but Alt just prefixes ESC.
    check(Key::Char('é'), ALT, legacy(), "\x1bé".as_bytes());
    check(Key::Char('é'), NONE, legacy(), "é".as_bytes());
    // Ctrl+Shift on a key whose shifted form is itself has no legacy form.
    check(Key::Char('ß'), CTRL | SHIFT, legacy(), b"\x1b[223;6u");
    // Keypad keys fall back as their main-keyboard equivalents.
    check(Key::Kp1, SUPER, legacy(), b"\x1b[49;9u");
    check(Key::KpEnter, SUPER, legacy(), b"\x1b[13;9u");
}

#[test]
fn legacy_ctrl_shift_follows_xterm() {
    // Ctrl+Shift+letter: xterm's C0 byte, not kitty's CSI u.
    check(Key::Char('a'), CTRL | SHIFT, legacy(), b"\x01");
    check(Key::Char('c'), CTRL | SHIFT, legacy(), b"\x03");
    check(Key::Char('z'), CTRL | ALT | SHIFT, legacy(), b"\x1b\x1a");
    check(Key::Char('A'), CTRL | SHIFT, legacy(), b"\x01");
    // Ctrl+Shift+symbol: the ctrl table applied to the shifted character.
    let symbols: &[(char, &[u8])] = &[
        ('-', b"\x1f"), // Ctrl+_
        ('2', b"\x00"), // Ctrl+@
        ('6', b"\x1e"), // Ctrl+^
        ('/', b"\x7f"), // Ctrl+?
        ('`', b"\x1e"), // Ctrl+~
        ('[', b"\x1b"), // Ctrl+{ : xterm masks the whole of `@`..`~`
        ('1', b"!"),
        (';', b":"),
    ];
    for &(c, expected) in symbols {
        check(Key::Char(c), CTRL | SHIFT, legacy(), expected);
        let mut with_esc = vec![0x1b];
        with_esc.extend_from_slice(expected);
        check(Key::Char(c), CTRL | ALT | SHIFT, legacy(), &with_esc);
        // The same when the frontend reports the (US) shifted key itself.
        let shifted = match c {
            '-' => '_',
            '2' => '@',
            '6' => '^',
            '/' => '?',
            '`' => '~',
            '[' => '{',
            '1' => '!',
            _ => ':',
        };
        let ev = KeyEvent::new(Key::Char(c), CTRL | SHIFT).with_shifted_key(shifted);
        assert_eq!(
            encode_key(&ev, &legacy()),
            expected,
            "{c:?} with shifted key"
        );
        let ev = KeyEvent::new(Key::Char(c), CTRL | ALT | SHIFT).with_shifted_key(shifted);
        assert_eq!(
            encode_key(&ev, &legacy()),
            with_esc,
            "{c:?} with shifted key"
        );
    }
    // A non-US shifted key that is not ASCII has no legacy form...
    let ev = KeyEvent::new(Key::Char('4'), CTRL | SHIFT).with_shifted_key('€');
    assert_eq!(encode_key(&ev, &legacy()), b"\x1b[52;6u");
    // ... unless the base layout provides one.
    let ev = KeyEvent::new(Key::Char('4'), CTRL | SHIFT)
        .with_shifted_key('€')
        .with_base_layout_key('4');
    assert_eq!(encode_key(&ev, &legacy()), b"$");
    // Keypad keys have no shifted symbol: Shift is consumed by the key.
    check(Key::Kp1, CTRL | SHIFT, legacy(), b"1");
    check(Key::KpSubtract, CTRL | SHIFT, legacy(), b"-");
    check(Key::KpDivide, CTRL | SHIFT, legacy(), b"\x1f");
    check(Key::Kp2, CTRL | SHIFT, legacy(), b"\x00");
    // A degenerate frontend that reports the shifted key equal to the key
    // still gets xterm's C0 byte for Ctrl+Shift+letter.
    let ev = KeyEvent::new(Key::Char('a'), CTRL | SHIFT).with_shifted_key('a');
    assert_eq!(encode_key(&ev, &legacy()), b"\x01");
    let ev = KeyEvent::new(Key::Char(' '), CTRL | SHIFT).with_shifted_key(' ');
    assert_eq!(encode_key(&ev, &legacy()), b"\x00");
    // ... but a key without a C0 byte of its own has no legacy form then.
    let ev = KeyEvent::new(Key::Char('1'), CTRL | SHIFT).with_shifted_key('1');
    assert_eq!(encode_key(&ev, &legacy()), b"\x1b[49;6u");
}

#[test]
fn legacy_shift_on_keypad_keys_sends_the_keypad_character() {
    // No real keyboard produces `)` from KP 0: Shift is consumed by the key,
    // as kitty's convert_kp_key_to_normal_key does.
    let table: &[(Key, &[u8])] = &[
        (Key::Kp0, b"0"),
        (Key::Kp8, b"8"),
        (Key::KpSubtract, b"-"),
        (Key::KpDecimal, b"."),
        (Key::KpDivide, b"/"),
        (Key::KpSeparator, b","),
        (Key::KpEqual, b"="),
        (Key::KpAdd, b"+"),
        (Key::KpMultiply, b"*"),
    ];
    let deckpam = KeyboardModes {
        application_keypad: true,
        ..legacy()
    };
    for &(key, expected) in table {
        check(key, SHIFT, legacy(), expected);
        // DECKPAM sends SS3 only without modifiers; Shift falls through to
        // the plain character too.
        check(key, SHIFT, deckpam, expected);
        let mut with_esc = vec![0x1b];
        with_esc.extend_from_slice(expected);
        check(key, SHIFT | ALT, legacy(), &with_esc);
    }
    check(Key::Kp0, CTRL | SHIFT, legacy(), b"0");
    check(Key::KpSubtract, CTRL | SHIFT, deckpam, b"-");
    check(Key::KpDivide, CTRL | SHIFT, legacy(), b"\x1f");
    // A shifted key supplied by the frontend is ignored for keypad keys.
    let ev = KeyEvent::new(Key::Kp0, SHIFT).with_shifted_key(')');
    assert_eq!(encode_key(&ev, &legacy()), b"0");
    // Shift does not survive into the CSI u fallback either.
    check(Key::Kp1, SUPER | SHIFT, legacy(), b"\x1b[49;9u");
    // modifyOtherKeys has nothing to encode for Shift+keypad.
    let level2 = KeyboardModes {
        modify_other_keys: ModifyOtherKeys::Level2,
        ..legacy()
    };
    check(Key::Kp0, SHIFT, level2, b"0");
    check(Key::Kp0, CTRL | SHIFT, level2, b"\x1b[27;5;48~");
}

#[test]
fn legacy_ctrl_and_alt_fall_back_to_the_base_layout_key() {
    // Cyrillic layout: the key is `с`, the base-layout key is `c`.
    let ev = |mods| KeyEvent::new(Key::Char('с'), mods).with_base_layout_key('c');
    assert_eq!(encode_key(&ev(CTRL), &legacy()), b"\x03");
    assert_eq!(encode_key(&ev(CTRL | ALT), &legacy()), b"\x1b\x03");
    assert_eq!(encode_key(&ev(ALT), &legacy()), b"\x1bc");
    assert_eq!(encode_key(&ev(CTRL | SHIFT), &legacy()), b"\x03");
    let ev = KeyEvent::new(Key::Char('с'), CTRL | SHIFT)
        .with_shifted_key('С')
        .with_base_layout_key('c');
    assert_eq!(encode_key(&ev, &legacy()), b"\x03");
    // Without Ctrl or Alt the key's own text is sent.
    let ev = KeyEvent::new(Key::Char('с'), NONE).with_base_layout_key('c');
    assert_eq!(encode_key(&ev, &legacy()), "с".as_bytes());
    let ev = KeyEvent::new(Key::Char('с'), SHIFT).with_base_layout_key('c');
    assert_eq!(encode_key(&ev, &legacy()), "С".as_bytes());
    // Without a base-layout key the combination has no legacy form.
    check(Key::Char('с'), CTRL, legacy(), b"\x1b[1089;5u");
    check(Key::Char('с'), CTRL | ALT, legacy(), b"\x1b[1089;7u");
    check(Key::Char('с'), ALT, legacy(), "\x1bс".as_bytes());
    // A non-ASCII base-layout key does not help.
    let ev = KeyEvent::new(Key::Char('с'), CTRL).with_base_layout_key('ç');
    assert_eq!(encode_key(&ev, &legacy()), b"\x1b[1089;5u");
    // Composed text under Alt still wins over the base-layout key.
    let ev = KeyEvent::new(Key::Char('с'), ALT)
        .with_base_layout_key('c')
        .with_text("ç");
    assert_eq!(encode_key(&ev, &legacy()), "\x1bç".as_bytes());
}

#[test]
fn malformed_control_code_chars_encode_to_nothing() {
    for mods in [NONE, ALT, CTRL, SHIFT, SUPER] {
        check(Key::Char('\x03'), mods, legacy(), b"");
        check(Key::Char('\x1b'), mods, legacy(), b"");
        check(Key::Char('\x1b'), mods, kitty(KittyFlags::ALL), b"");
        check(
            Key::Char('\u{85}'),
            mods,
            kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES),
            b"",
        );
    }
    // Composed text containing a control character is ignored, not sent.
    let ev = KeyEvent::new(Key::Char('a'), NONE).with_text("\x03");
    assert_eq!(encode_key(&ev, &legacy()), b"a");
    let ev = KeyEvent::new(Key::Unidentified, NONE).with_text("a\r");
    assert_eq!(encode_key(&ev, &legacy()), b"");
    // A control code in the shifted or base-layout key is ignored too.
    let ev = KeyEvent::new(Key::Char('a'), SHIFT).with_shifted_key('\x1b');
    assert_eq!(encode_key(&ev, &legacy()), b"A");
    let ev = KeyEvent::new(Key::Char('ф'), ALT).with_base_layout_key('\x1b');
    assert_eq!(encode_key(&ev, &legacy()), "\x1bф".as_bytes());
    let ev = KeyEvent::new(Key::Char('ф'), CTRL).with_base_layout_key('\r');
    assert_eq!(encode_key(&ev, &legacy()), b"\x1b[1092;5u");
    let ev = KeyEvent::new(Key::Char('a'), CTRL | SHIFT).with_shifted_key('\0');
    assert_eq!(encode_key(&ev, &legacy()), b"\x01");
    let ev = KeyEvent::new(Key::Char('a'), SHIFT).with_shifted_key('\0');
    assert_eq!(
        encode_key(&ev, &kitty(KittyFlags::ALL)),
        b"\x1b[97:65;2;65u"
    );
    let ev = KeyEvent::new(Key::Char('a'), CTRL).with_base_layout_key('\x7f');
    assert_eq!(
        encode_key(
            &ev,
            &kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES | KittyFlags::REPORT_ALTERNATE_KEYS)
        ),
        b"\x1b[97;5u"
    );
}

#[test]
fn legacy_prefers_text_composed_by_the_frontend() {
    let ev = KeyEvent::new(Key::Char('a'), NONE).with_text("å");
    assert_eq!(encode_key(&ev, &legacy()), "å".as_bytes());
    // Shift uses the frontend's shifted key when given (non-US layouts).
    let ev = KeyEvent::new(Key::Char('4'), SHIFT).with_shifted_key('€');
    assert_eq!(encode_key(&ev, &legacy()), "€".as_bytes());
    let ev = KeyEvent::new(Key::Char('4'), SHIFT | ALT).with_shifted_key('€');
    assert_eq!(encode_key(&ev, &legacy()), "\x1b€".as_bytes());
    // Composed text under Shift alone is the shifted key.
    let ev = KeyEvent::new(Key::Char('4'), SHIFT).with_text("€");
    assert_eq!(encode_key(&ev, &legacy()), "€".as_bytes());
    // An uppercase character is normalised to its unshifted form.
    check(Key::Char('A'), CTRL, legacy(), b"\x01");
    check(Key::Char('A'), SHIFT, legacy(), b"A");
    // A pure text event is sent as-is.
    let ev = KeyEvent::new(Key::Unidentified, NONE).with_text("漢字");
    assert_eq!(encode_key(&ev, &legacy()), "漢字".as_bytes());
    let ev = KeyEvent::new(Key::Unidentified, NONE);
    assert_eq!(encode_key(&ev, &legacy()), b"");
}

// ---------------------------------------------------------------------------
// Spec: Legacy ctrl mapping of ASCII keys
// ---------------------------------------------------------------------------

#[test]
fn legacy_caps_lock_inverts_the_case_of_derived_letters() {
    check(Key::Char('a'), Modifiers::CAPS_LOCK, legacy(), b"A");
    check(Key::Char('a'), SHIFT | Modifiers::CAPS_LOCK, legacy(), b"a");
    check(
        Key::Char('a'),
        ALT | Modifiers::CAPS_LOCK,
        legacy(),
        b"\x1bA",
    );
    check(Key::Char('1'), Modifiers::CAPS_LOCK, legacy(), b"1");
    check(Key::Char('1'), SHIFT | Modifiers::CAPS_LOCK, legacy(), b"!");
    // Ctrl is unaffected, and text from the frontend always wins.
    check(
        Key::Char('c'),
        CTRL | Modifiers::CAPS_LOCK,
        legacy(),
        b"\x03",
    );
    let ev = KeyEvent::new(Key::Char('a'), Modifiers::CAPS_LOCK).with_text("a");
    assert_eq!(encode_key(&ev, &legacy()), b"a");
}

#[test]
fn legacy_ctrl_mapping_table() {
    let table: &[(char, u8)] = &[
        (' ', 0),
        ('/', 31),
        ('0', 48),
        ('1', 49),
        ('2', 0),
        ('3', 27),
        ('4', 28),
        ('5', 29),
        ('6', 30),
        ('7', 31),
        ('8', 127),
        ('9', 57),
        ('?', 127),
        ('@', 0),
        ('[', 27),
        ('\\', 28),
        (']', 29),
        ('^', 30),
        ('_', 31),
        ('`', 0),
        ('a', 1),
        ('b', 2),
        ('c', 3),
        ('d', 4),
        ('e', 5),
        ('f', 6),
        ('g', 7),
        ('h', 8),
        ('i', 9),
        ('j', 10),
        ('k', 11),
        ('l', 12),
        ('m', 13),
        ('n', 14),
        ('o', 15),
        ('p', 16),
        ('q', 17),
        ('r', 18),
        ('s', 19),
        ('t', 20),
        ('u', 21),
        ('v', 22),
        ('w', 23),
        ('x', 24),
        ('y', 25),
        ('z', 26),
        ('{', 27),
        ('|', 28),
        ('}', 29),
        ('~', 30),
    ];
    for &(c, byte) in table {
        check(Key::Char(c), CTRL, legacy(), &[byte]);
        check(Key::Char(c), CTRL | ALT, legacy(), &[0x1b, byte]);
    }
    // "Any ASCII keys not in the table must be left untouched by ctrl."
    // (`` ` ``, `{`, `|` and `}` are masked as in xterm; see module docs.)
    for c in ['-', '=', '\'', ',', '.', ';'] {
        check(Key::Char(c), CTRL, legacy(), &[c as u8]);
    }
}

// ---------------------------------------------------------------------------
// xterm legacy matrix snapshot
// ---------------------------------------------------------------------------

/// Every printable US key against every Shift/Ctrl/Alt combination in legacy
/// mode. Each cell is what xterm sends (Ctrl applies to the shifted
/// character when Shift is held; keys the ctrl table does not cover are left
/// untouched).
#[test]
fn legacy_xterm_matrix() {
    let keys = "abcdefghijklmnopqrstuvwxyz0123456789`-=[]\\;',./ ";
    let combos = [
        ("plain", NONE),
        ("shift", SHIFT),
        ("ctrl", CTRL),
        ("alt", ALT),
        ("ctrl+alt", CTRL | ALT),
        ("alt+shift", ALT | SHIFT),
        ("ctrl+shift", CTRL | SHIFT),
        ("ctrl+alt+shift", CTRL | ALT | SHIFT),
    ];
    let mut out = String::new();
    for c in keys.chars() {
        let _ = write!(out, "{c:?}");
        for (name, mods) in combos {
            let bytes = enc(Key::Char(c), mods, legacy());
            let _ = write!(out, "  {name}=[{}]", show(&bytes));
        }
        out.push('\n');
    }
    insta::assert_snapshot!(out);
}

// ---------------------------------------------------------------------------
// xterm modifyOtherKeys
// ---------------------------------------------------------------------------

#[test]
fn modify_other_keys_level_1_keeps_well_established_sequences() {
    let modes = KeyboardModes {
        modify_other_keys: ModifyOtherKeys::Level1,
        ..legacy()
    };
    // Well-established combinations are unchanged.
    check(Key::Char('a'), NONE, modes, b"a");
    check(Key::Char('a'), SHIFT, modes, b"A");
    check(Key::Char('c'), CTRL, modes, b"\x03");
    check(Key::Char('a'), ALT, modes, b"\x1ba");
    check(Key::Char('a'), ALT | SHIFT, modes, b"\x1bA");
    check(Key::Char(' '), CTRL, modes, b"\x00");
    check(Key::Char(' '), ALT, modes, b"\x1b ");
    check(Key::Char('['), CTRL, modes, b"\x1b");
    // xterm's IsControlInput(): Ctrl on `@`..`~` is a legacy byte, so the
    // masked keys stay legacy rather than becoming CSI 27 ; 5 ; 96 ~.
    check(Key::Char('`'), CTRL, modes, b"\x00");
    check(Key::Char('['), CTRL | SHIFT, modes, b"\x1b");
    check(Key::Char('\\'), CTRL | SHIFT, modes, b"\x1c");
    check(Key::Char(']'), CTRL | SHIFT, modes, b"\x1d");
    check(Key::Tab, SHIFT, modes, b"\x1b[Z");
    check(Key::Backspace, CTRL, modes, b"\x08");
    check(Key::Escape, ALT, modes, b"\x1b\x1b");
    // xterm's allowedCharModifiers(): Ctrl+Shift stays a legacy byte when
    // the lookup already produced one.
    check(Key::Char('a'), CTRL | SHIFT, modes, b"\x01");
    check(Key::Char('6'), CTRL | SHIFT, modes, b"\x1e");
    check(Key::Char(' '), CTRL | SHIFT, modes, b"\x00");
    // Tab and Enter are encoded under any modifier other than Shift+Tab
    // (ctlseqs: "alt-Tab sends CSI 27;3;9~"); Emacs relies on these.
    check(Key::Enter, SHIFT, modes, b"\x1b[27;2;13~");
    check(Key::Enter, CTRL, modes, b"\x1b[27;5;13~");
    check(Key::Enter, ALT, modes, b"\x1b[27;3;13~");
    check(Key::Tab, CTRL, modes, b"\x1b[27;5;9~");
    check(Key::Tab, ALT, modes, b"\x1b[27;3;9~");
    check(Key::Tab, CTRL | SHIFT, modes, b"\x1b[27;6;9~");
    // Everything else is encoded as CSI 27 ; mods ; code ~ (shifted code).
    check(Key::Char('a'), CTRL | ALT, modes, b"\x1b[27;7;97~");
    check(Key::Char(';'), CTRL, modes, b"\x1b[27;5;59~");
    check(Key::Char('1'), CTRL, modes, b"\x1b[27;5;49~");
    check(Key::Char('1'), CTRL | SHIFT, modes, b"\x1b[27;6;33~");
    check(Key::Char('a'), SUPER, modes, b"\x1b[27;9;97~");
    check(Key::Char(' '), ALT | SHIFT, modes, b"\x1b[27;4;32~");
    // Lock modifiers do not count (Caps Lock only changes the letter).
    check(Key::Char('a'), Modifiers::CAPS_LOCK, modes, b"A");
}

#[test]
fn modify_other_keys_level_2_encodes_every_modified_key() {
    let modes = KeyboardModes {
        modify_other_keys: ModifyOtherKeys::Level2,
        ..legacy()
    };
    check(Key::Char('a'), NONE, modes, b"a");
    check(Key::Char('a'), SHIFT, modes, b"A");
    check(Key::Char('c'), CTRL, modes, b"\x1b[27;5;99~");
    check(Key::Char('a'), ALT, modes, b"\x1b[27;3;97~");
    check(Key::Char('a'), ALT | SHIFT, modes, b"\x1b[27;4;65~");
    check(Key::Char('a'), CTRL | SHIFT, modes, b"\x1b[27;6;65~");
    check(Key::Char(' '), CTRL, modes, b"\x1b[27;5;32~");
    check(Key::Char(' '), SHIFT, modes, b"\x1b[27;2;32~");
    check(Key::Tab, NONE, modes, b"\x09");
    check(Key::Tab, SHIFT, modes, b"\x1b[27;2;9~");
    check(Key::Enter, SHIFT, modes, b"\x1b[27;2;13~");
    check(Key::Enter, NONE, modes, b"\r");
    check(Key::Backspace, CTRL, modes, b"\x1b[27;5;127~");
    check(Key::Escape, ALT, modes, b"\x1b[27;3;27~");
    // Super, Hyper and Meta all map to xterm's Meta bit.
    check(
        Key::Char('a'),
        Modifiers::META | CTRL,
        modes,
        b"\x1b[27;13;97~",
    );
    // Function keys are unaffected.
    check(Key::F(1), CTRL, modes, b"\x1b[1;5P");
    check(Key::Up, SHIFT, modes, b"\x1b[1;2A");
}

#[test]
fn kitty_protocol_takes_precedence_over_modify_other_keys() {
    let modes = KeyboardModes {
        modify_other_keys: ModifyOtherKeys::Level2,
        kitty_flags: KittyFlags::DISAMBIGUATE_ESCAPE_CODES,
        ..legacy()
    };
    check(Key::Char('c'), CTRL, modes, b"\x1b[99;5u");
}

// ---------------------------------------------------------------------------
// Spec: Functional key codes (kitty mode)
// ---------------------------------------------------------------------------

/// Every row of the "Functional key codes" table except the modifier keys,
/// encoded as an escape code.
const FUNCTIONAL_KEY_CODES: &[(Key, &[u8])] = &[
    (Key::Escape, b"\x1b[27u"),
    (Key::Enter, b"\x1b[13u"),
    (Key::Tab, b"\x1b[9u"),
    (Key::Backspace, b"\x1b[127u"),
    (Key::Insert, b"\x1b[2~"),
    (Key::Delete, b"\x1b[3~"),
    (Key::Left, b"\x1b[D"),
    (Key::Right, b"\x1b[C"),
    (Key::Up, b"\x1b[A"),
    (Key::Down, b"\x1b[B"),
    (Key::PageUp, b"\x1b[5~"),
    (Key::PageDown, b"\x1b[6~"),
    (Key::Home, b"\x1b[H"),
    (Key::End, b"\x1b[F"),
    (Key::CapsLock, b"\x1b[57358u"),
    (Key::ScrollLock, b"\x1b[57359u"),
    (Key::NumLock, b"\x1b[57360u"),
    (Key::PrintScreen, b"\x1b[57361u"),
    (Key::Pause, b"\x1b[57362u"),
    (Key::Menu, b"\x1b[57363u"),
    (Key::F(1), b"\x1b[P"),
    (Key::F(2), b"\x1b[Q"),
    (Key::F(3), b"\x1b[13~"),
    (Key::F(4), b"\x1b[S"),
    (Key::F(5), b"\x1b[15~"),
    (Key::F(6), b"\x1b[17~"),
    (Key::F(7), b"\x1b[18~"),
    (Key::F(8), b"\x1b[19~"),
    (Key::F(9), b"\x1b[20~"),
    (Key::F(10), b"\x1b[21~"),
    (Key::F(11), b"\x1b[23~"),
    (Key::F(12), b"\x1b[24~"),
    (Key::F(13), b"\x1b[57376u"),
    (Key::F(14), b"\x1b[57377u"),
    (Key::F(15), b"\x1b[57378u"),
    (Key::F(16), b"\x1b[57379u"),
    (Key::F(17), b"\x1b[57380u"),
    (Key::F(18), b"\x1b[57381u"),
    (Key::F(19), b"\x1b[57382u"),
    (Key::F(20), b"\x1b[57383u"),
    (Key::F(21), b"\x1b[57384u"),
    (Key::F(22), b"\x1b[57385u"),
    (Key::F(23), b"\x1b[57386u"),
    (Key::F(24), b"\x1b[57387u"),
    (Key::F(25), b"\x1b[57388u"),
    (Key::F(26), b"\x1b[57389u"),
    (Key::F(27), b"\x1b[57390u"),
    (Key::F(28), b"\x1b[57391u"),
    (Key::F(29), b"\x1b[57392u"),
    (Key::F(30), b"\x1b[57393u"),
    (Key::F(31), b"\x1b[57394u"),
    (Key::F(32), b"\x1b[57395u"),
    (Key::F(33), b"\x1b[57396u"),
    (Key::F(34), b"\x1b[57397u"),
    (Key::F(35), b"\x1b[57398u"),
    (Key::Kp0, b"\x1b[57399u"),
    (Key::Kp1, b"\x1b[57400u"),
    (Key::Kp2, b"\x1b[57401u"),
    (Key::Kp3, b"\x1b[57402u"),
    (Key::Kp4, b"\x1b[57403u"),
    (Key::Kp5, b"\x1b[57404u"),
    (Key::Kp6, b"\x1b[57405u"),
    (Key::Kp7, b"\x1b[57406u"),
    (Key::Kp8, b"\x1b[57407u"),
    (Key::Kp9, b"\x1b[57408u"),
    (Key::KpDecimal, b"\x1b[57409u"),
    (Key::KpDivide, b"\x1b[57410u"),
    (Key::KpMultiply, b"\x1b[57411u"),
    (Key::KpSubtract, b"\x1b[57412u"),
    (Key::KpAdd, b"\x1b[57413u"),
    (Key::KpEnter, b"\x1b[57414u"),
    (Key::KpEqual, b"\x1b[57415u"),
    (Key::KpSeparator, b"\x1b[57416u"),
    (Key::KpLeft, b"\x1b[57417u"),
    (Key::KpRight, b"\x1b[57418u"),
    (Key::KpUp, b"\x1b[57419u"),
    (Key::KpDown, b"\x1b[57420u"),
    (Key::KpPageUp, b"\x1b[57421u"),
    (Key::KpPageDown, b"\x1b[57422u"),
    (Key::KpHome, b"\x1b[57423u"),
    (Key::KpEnd, b"\x1b[57424u"),
    (Key::KpInsert, b"\x1b[57425u"),
    (Key::KpDelete, b"\x1b[57426u"),
    (Key::KpBegin, b"\x1b[E"),
    (Key::MediaPlay, b"\x1b[57428u"),
    (Key::MediaPause, b"\x1b[57429u"),
    (Key::MediaPlayPause, b"\x1b[57430u"),
    (Key::MediaReverse, b"\x1b[57431u"),
    (Key::MediaStop, b"\x1b[57432u"),
    (Key::MediaFastForward, b"\x1b[57433u"),
    (Key::MediaRewind, b"\x1b[57434u"),
    (Key::MediaTrackNext, b"\x1b[57435u"),
    (Key::MediaTrackPrevious, b"\x1b[57436u"),
    (Key::MediaRecord, b"\x1b[57437u"),
    (Key::LowerVolume, b"\x1b[57438u"),
    (Key::RaiseVolume, b"\x1b[57439u"),
    (Key::MuteVolume, b"\x1b[57440u"),
];

#[test]
fn functional_key_codes_table() {
    // Keypad text keys are only reported as escape codes under "report all
    // keys", so that is the mode the whole table is checked in.
    let all = kitty(KittyFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);
    for &(key, expected) in FUNCTIONAL_KEY_CODES {
        check(key, NONE, all, expected);
    }
    // With disambiguation only, the non-text keys use the same codes; the
    // keypad keys that stand for characters produce text unless modified.
    let modes = kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES);
    for &(key, expected) in FUNCTIONAL_KEY_CODES {
        match key {
            Key::Enter => check(key, NONE, modes, b"\x0d"),
            Key::Tab => check(key, NONE, modes, b"\x09"),
            Key::Backspace => check(key, NONE, modes, b"\x7f"),
            Key::Kp0 => check(key, NONE, modes, b"0"),
            Key::Kp9 => check(key, NONE, modes, b"9"),
            Key::KpDecimal => check(key, NONE, modes, b"."),
            Key::KpDivide => check(key, NONE, modes, b"/"),
            Key::KpMultiply => check(key, NONE, modes, b"*"),
            Key::KpSubtract => check(key, NONE, modes, b"-"),
            Key::KpAdd => check(key, NONE, modes, b"+"),
            Key::KpEqual => check(key, NONE, modes, b"="),
            Key::KpSeparator => check(key, NONE, modes, b","),
            Key::Kp1
            | Key::Kp2
            | Key::Kp3
            | Key::Kp4
            | Key::Kp5
            | Key::Kp6
            | Key::Kp7
            | Key::Kp8 => {}
            _ => check(key, NONE, modes, expected),
        }
    }
    check(Key::Kp5, CTRL, modes, b"\x1b[57404;5u");
    check(Key::KpAdd, ALT, modes, b"\x1b[57413;3u");
}

#[test]
fn functional_key_codes_table_modifier_keys() {
    let modifier_keys: &[(Key, Modifiers, &[u8])] = &[
        (Key::LeftShift, SHIFT, b"\x1b[57441;2u"),
        (Key::LeftControl, CTRL, b"\x1b[57442;5u"),
        (Key::LeftAlt, ALT, b"\x1b[57443;3u"),
        (Key::LeftSuper, SUPER, b"\x1b[57444;9u"),
        (Key::LeftHyper, Modifiers::HYPER, b"\x1b[57445;17u"),
        (Key::LeftMeta, Modifiers::META, b"\x1b[57446;33u"),
        (Key::RightShift, SHIFT, b"\x1b[57447;2u"),
        (Key::RightControl, CTRL, b"\x1b[57448;5u"),
        (Key::RightAlt, ALT, b"\x1b[57449;3u"),
        (Key::RightSuper, SUPER, b"\x1b[57450;9u"),
        (Key::RightHyper, Modifiers::HYPER, b"\x1b[57451;17u"),
        (Key::RightMeta, Modifiers::META, b"\x1b[57452;33u"),
        (Key::IsoLevel3Shift, NONE, b"\x1b[57453u"),
        (Key::IsoLevel5Shift, NONE, b"\x1b[57454u"),
    ];
    let all = kitty(KittyFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);
    let modes = kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES);
    for &(key, mods, expected) in modifier_keys {
        check(key, mods, all, expected);
        // Without "report all keys", modifier keys are not reported.
        check(key, mods, modes, b"");
    }
}

#[test]
fn kitty_modified_functional_keys_use_the_parameter_forms() {
    let modes = kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES);
    check(Key::Up, SHIFT, modes, b"\x1b[1;2A");
    check(Key::F(1), CTRL, modes, b"\x1b[1;5P");
    check(Key::F(3), SHIFT, modes, b"\x1b[13;2~");
    check(Key::Delete, ALT, modes, b"\x1b[3;3~");
    check(Key::F(13), SHIFT | CTRL, modes, b"\x1b[57376;6u");
    check(Key::Escape, CTRL, modes, b"\x1b[27;5u");
    // Kitty mode never uses SS3, even under DECCKM.
    let deckm = KeyboardModes {
        application_cursor_keys: true,
        ..modes
    };
    check(Key::Up, NONE, deckm, b"\x1b[A");
    check(Key::F(1), NONE, deckm, b"\x1b[P");
    // Non-text functional keys report lock modifiers.
    check(Key::Up, Modifiers::CAPS_LOCK, modes, b"\x1b[1;65A");
}

// ---------------------------------------------------------------------------
// Spec: Disambiguate escape codes
// ---------------------------------------------------------------------------

#[test]
fn disambiguate_reports_modified_text_keys_as_csi_u() {
    let modes = kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES);
    // Text keys still produce text when unmodified or shifted.
    check(Key::Char('a'), NONE, modes, b"a");
    check(Key::Char('a'), SHIFT, modes, b"A");
    check(Key::Char('a'), Modifiers::CAPS_LOCK, modes, b"A");
    // Esc, alt+key, ctrl+key, ctrl+alt+key, shift+alt+key are CSI u.
    check(Key::Escape, NONE, modes, b"\x1b[27u");
    check(Key::Char('a'), ALT, modes, b"\x1b[97;3u");
    check(Key::Char('c'), CTRL, modes, b"\x1b[99;5u");
    check(Key::Char('c'), CTRL | ALT, modes, b"\x1b[99;7u");
    check(Key::Char('a'), SHIFT | ALT, modes, b"\x1b[97;4u");
    check(Key::Char('a'), CTRL | SHIFT, modes, b"\x1b[97;6u");
    check(Key::Char(' '), CTRL, modes, b"\x1b[32;5u");
    check(Key::Char('['), ALT, modes, b"\x1b[91;3u");
    // The key code is always the unshifted key.
    check(Key::Char('A'), CTRL | SHIFT, modes, b"\x1b[97;6u");
    // Lock modifiers are not reported for text-producing keys.
    check(
        Key::Char('a'),
        CTRL | Modifiers::CAPS_LOCK,
        modes,
        b"\x1b[97;5u",
    );
    // Enter, Tab and Backspace keep their legacy bytes without modifiers...
    check(Key::Enter, NONE, modes, b"\r");
    check(Key::Tab, NONE, modes, b"\t");
    check(Key::Backspace, NONE, modes, b"\x7f");
    check(Key::Backspace, Modifiers::NUM_LOCK, modes, b"\x7f");
    // ... and become CSI u with them.
    check(Key::Enter, SHIFT, modes, b"\x1b[13;2u");
    check(Key::Tab, SHIFT, modes, b"\x1b[9;2u");
    check(Key::Tab, CTRL | SHIFT, modes, b"\x1b[9;6u");
    check(Key::Backspace, CTRL, modes, b"\x1b[127;5u");
    check(Key::Enter, ALT, modes, b"\x1b[13;3u");
    // Non-text keypad keys get their own codes; text ones stay text.
    check(Key::Kp1, NONE, modes, b"1");
    check(Key::Kp1, CTRL, modes, b"\x1b[57400;5u");
    check(Key::KpEnter, NONE, modes, b"\x1b[57414u");
    check(Key::KpLeft, NONE, modes, b"\x1b[57417u");
}

// ---------------------------------------------------------------------------
// Spec: Report event types
// ---------------------------------------------------------------------------

#[test]
fn report_event_types() {
    let modes = kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES | KittyFlags::REPORT_EVENT_TYPES);
    let ev = |key, mods, kind| KeyEvent::new(key, mods).with_kind(kind);
    // CSI key-code;modifier:2 for repeat, :3 for release; 1 for press.
    assert_eq!(
        encode_key(&ev(Key::Char('c'), CTRL, KeyEventKind::Press), &modes),
        b"\x1b[99;5u"
    );
    assert_eq!(
        encode_key(&ev(Key::Char('c'), CTRL, KeyEventKind::Repeat), &modes),
        b"\x1b[99;5:2u"
    );
    assert_eq!(
        encode_key(&ev(Key::Char('c'), CTRL, KeyEventKind::Release), &modes),
        b"\x1b[99;5:3u"
    );
    // "If no modifiers are present, the modifiers field must have the value 1."
    assert_eq!(
        encode_key(&ev(Key::Up, NONE, KeyEventKind::Release), &modes),
        b"\x1b[1;1:3A"
    );
    assert_eq!(
        encode_key(&ev(Key::Escape, NONE, KeyEventKind::Repeat), &modes),
        b"\x1b[27;1:2u"
    );
    assert_eq!(
        encode_key(&ev(Key::F(5), NONE, KeyEventKind::Release), &modes),
        b"\x1b[15;1:3~"
    );
    // Text keys are sent as text, so no release for them...
    assert_eq!(
        encode_key(&ev(Key::Char('a'), NONE, KeyEventKind::Release), &modes),
        b""
    );
    assert_eq!(
        encode_key(&ev(Key::Char('a'), NONE, KeyEventKind::Repeat), &modes),
        b"a"
    );
    // ... nor for Enter/Tab/Backspace without "report all keys".
    assert_eq!(
        encode_key(&ev(Key::Enter, NONE, KeyEventKind::Release), &modes),
        b""
    );
    // Without the flag, releases are dropped entirely.
    let without = kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES);
    assert_eq!(
        encode_key(&ev(Key::Up, NONE, KeyEventKind::Release), &without),
        b""
    );
    assert_eq!(
        encode_key(&ev(Key::Up, NONE, KeyEventKind::Repeat), &without),
        b"\x1b[A"
    );
}

#[test]
fn event_types_without_disambiguation_keep_legacy_forms() {
    // Only 0b10: unmodified keys and Ctrl/Alt text keys keep their legacy
    // form on press, as kitty's encoder does, but modified Esc, Enter, Tab
    // and Backspace are CSI u, and every reportable release is.
    let modes = kitty(KittyFlags::REPORT_EVENT_TYPES);
    check(Key::Char('c'), CTRL, modes, b"\x03");
    check(Key::Char('a'), ALT, modes, b"\x1ba");
    check(Key::Escape, NONE, modes, b"\x1b");
    check(Key::Escape, CTRL, modes, b"\x1b[27;5u");
    check(Key::Escape, ALT, modes, b"\x1b[27;3u");
    check(Key::Enter, NONE, modes, b"\r");
    check(Key::Enter, ALT, modes, b"\x1b[13;3u");
    check(Key::Tab, SHIFT, modes, b"\x1b[9;2u");
    check(Key::Kp1, NONE, modes, b"1");
    let release = KeyEvent::new(Key::Up, NONE).with_kind(KeyEventKind::Release);
    assert_eq!(encode_key(&release, &modes), b"\x1b[1;1:3A");
    let release = KeyEvent::new(Key::Char('c'), CTRL).with_kind(KeyEventKind::Release);
    assert_eq!(encode_key(&release, &modes), b"\x1b[99;5:3u");
    let release = KeyEvent::new(Key::Char('a'), NONE).with_kind(KeyEventKind::Release);
    assert_eq!(encode_key(&release, &modes), b"");
    let release = KeyEvent::new(Key::Escape, NONE).with_kind(KeyEventKind::Release);
    assert_eq!(encode_key(&release, &modes), b"");
    // The same split applies with only *report associated text*.
    let text_only = kitty(KittyFlags::REPORT_ASSOCIATED_TEXT);
    check(Key::Char('c'), CTRL, text_only, b"\x1b[99;5u");
    check(Key::Escape, ALT, text_only, b"\x1b[27;3u");
    check(Key::Escape, NONE, text_only, b"\x1b");
    // The legacy fallback still honours DECCKM.
    let deckm = KeyboardModes {
        application_cursor_keys: true,
        ..modes
    };
    check(Key::Up, NONE, deckm, b"\x1b[A");
    let legacy_deckm = KeyboardModes {
        application_cursor_keys: true,
        ..legacy()
    };
    check(Key::Up, NONE, legacy_deckm, b"\x1bOA");
}

// ---------------------------------------------------------------------------
// Spec: Report alternate keys
// ---------------------------------------------------------------------------

#[test]
fn report_alternate_keys() {
    let modes = kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES | KittyFlags::REPORT_ALTERNATE_KEYS);
    // "ctrl+shift+a": shifted key present only when shift is present.
    let ev = KeyEvent::new(Key::Char('a'), CTRL | SHIFT);
    assert_eq!(encode_key(&ev, &modes), b"\x1b[97:65;6u");
    let ev = KeyEvent::new(Key::Char('a'), CTRL);
    assert_eq!(encode_key(&ev, &modes), b"\x1b[97;5u");
    // "ctrl+plus" vs "ctrl+shift+equal": the layout decides the shifted key.
    let ev = KeyEvent::new(Key::Char('='), CTRL | SHIFT).with_shifted_key('+');
    assert_eq!(encode_key(&ev, &modes), b"\x1b[61:43;6u");
    // Cyrillic layout: ctrl+С carries base-layout key c.
    let ev = KeyEvent::new(Key::Char('с'), CTRL).with_base_layout_key('c');
    assert_eq!(encode_key(&ev, &modes), b"\x1b[1089::99;5u");
    let ev = KeyEvent::new(Key::Char('с'), CTRL | SHIFT)
        .with_shifted_key('С')
        .with_base_layout_key('c');
    assert_eq!(encode_key(&ev, &modes), b"\x1b[1089:1057:99;6u");
    // Alternates are a pure enhancement: text keys still produce text.
    let ev = KeyEvent::new(Key::Char('a'), SHIFT).with_base_layout_key('q');
    assert_eq!(encode_key(&ev, &modes), b"A");
    // A base key equal to the key is not repeated.
    let ev = KeyEvent::new(Key::Char('c'), CTRL).with_base_layout_key('c');
    assert_eq!(encode_key(&ev, &modes), b"\x1b[99;5u");
    // Functional keys have no alternates.
    check(Key::Up, SHIFT, modes, b"\x1b[1;2A");
    check(Key::F(13), SHIFT, modes, b"\x1b[57376;2u");
}

// ---------------------------------------------------------------------------
// Spec: Report all keys as escape codes / Report associated text
// ---------------------------------------------------------------------------

#[test]
fn report_all_keys_as_escape_codes() {
    let modes = kitty(KittyFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);
    check(Key::Char('a'), NONE, modes, b"\x1b[97u");
    check(Key::Char('a'), SHIFT, modes, b"\x1b[97;2u");
    check(Key::Char(' '), NONE, modes, b"\x1b[32u");
    check(Key::Enter, NONE, modes, b"\x1b[13u");
    check(Key::Tab, NONE, modes, b"\x1b[9u");
    check(Key::Backspace, NONE, modes, b"\x1b[127u");
    check(Key::Escape, NONE, modes, b"\x1b[27u");
    check(Key::Kp1, NONE, modes, b"\x1b[57400u");
    // Lock modifiers are reported for all keys.
    check(Key::Char('a'), Modifiers::CAPS_LOCK, modes, b"\x1b[97;65u");
    check(
        Key::Char('a'),
        Modifiers::NUM_LOCK | SHIFT,
        modes,
        b"\x1b[97;130u",
    );
    // Modifier key presses are reported, with their own bit set.
    check(Key::LeftShift, SHIFT, modes, b"\x1b[57441;2u");
    // Text is not sent without the associated-text flag.
    let ev = KeyEvent::new(Key::Char('a'), SHIFT).with_text("A");
    assert_eq!(encode_key(&ev, &modes), b"\x1b[97;2u");
    // Releases need the event-type flag.
    let ev = KeyEvent::new(Key::Enter, NONE).with_kind(KeyEventKind::Release);
    assert_eq!(encode_key(&ev, &modes), b"");
    let both = kitty(KittyFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES | KittyFlags::REPORT_EVENT_TYPES);
    assert_eq!(encode_key(&ev, &both), b"\x1b[13;1:3u");
    let ev = KeyEvent::new(Key::LeftShift, NONE).with_kind(KeyEventKind::Release);
    assert_eq!(encode_key(&ev, &both), b"\x1b[57441;1:3u");
}

#[test]
fn report_associated_text_examples() {
    let modes =
        kitty(KittyFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES | KittyFlags::REPORT_ASSOCIATED_TEXT);
    // shift+a -> CSI 97 ; 2 ; 65 u
    let ev = KeyEvent::new(Key::Char('a'), SHIFT).with_text("A");
    assert_eq!(encode_key(&ev, &modes), b"\x1b[97;2;65u");
    // The same without text from the frontend, derived from the layout.
    let ev = KeyEvent::new(Key::Char('a'), SHIFT);
    assert_eq!(encode_key(&ev, &modes), b"\x1b[97;2;65u");
    // alt+a -> CSI 0 ; ; 229 u (alt consumed by the OS, key unknown)
    let ev = KeyEvent::new(Key::Unidentified, NONE).with_text("å");
    assert_eq!(encode_key(&ev, &modes), b"\x1b[0;;229u");
    // Multiple code points are separated by colons.
    let ev = KeyEvent::new(Key::Unidentified, NONE).with_text("漢字");
    assert_eq!(encode_key(&ev, &modes), b"\x1b[0;;28450:23383u");
    // Plain keys carry their text too.
    let ev = KeyEvent::new(Key::Char('a'), NONE);
    assert_eq!(encode_key(&ev, &modes), b"\x1b[97;;97u");
    // Control characters are never sent as text.
    let ev = KeyEvent::new(Key::Char('c'), CTRL).with_text("\x03");
    assert_eq!(encode_key(&ev, &modes), b"\x1b[99;5u");
    // Text is never fabricated for combinations that produce none: Ctrl,
    // Super, Alt (and Hyper/Meta) without text from the frontend.
    check(Key::Char('c'), CTRL, modes, b"\x1b[99;5u");
    check(Key::Char('c'), SUPER, modes, b"\x1b[99;9u");
    check(Key::Char('c'), ALT, modes, b"\x1b[99;3u");
    check(Key::Char('c'), CTRL | SHIFT, modes, b"\x1b[99;6u");
    check(Key::Char('c'), Modifiers::HYPER, modes, b"\x1b[99;17u");
    // ... but text the frontend composed under Alt is the event's text.
    let ev = KeyEvent::new(Key::Char('c'), ALT).with_text("ç");
    assert_eq!(encode_key(&ev, &modes), b"\x1b[99;3;231u");
    // Lock modifiers do not stop the key's own text from being derived, and
    // Caps Lock is applied to it (the key produced "C").
    check(
        Key::Char('c'),
        Modifiers::CAPS_LOCK,
        modes,
        b"\x1b[99;65;67u",
    );
    check(
        Key::Char('c'),
        SHIFT | Modifiers::CAPS_LOCK,
        modes,
        b"\x1b[99;66;99u",
    );
    check(Key::Kp1, Modifiers::NUM_LOCK, modes, b"\x1b[57400;129;49u");
    // Functional keys carry no text.
    let ev = KeyEvent::new(Key::Enter, NONE).with_text("\r");
    assert_eq!(encode_key(&ev, &modes), b"\x1b[13u");
    // Releases carry no text; repeats do.
    let all = kitty(KittyFlags::ALL);
    let ev = KeyEvent::new(Key::Char('a'), SHIFT).with_kind(KeyEventKind::Release);
    assert_eq!(encode_key(&ev, &all), b"\x1b[97:65;2:3u");
    let ev = KeyEvent::new(Key::Char('a'), SHIFT).with_kind(KeyEventKind::Repeat);
    assert_eq!(encode_key(&ev, &all), b"\x1b[97:65;2:2;65u");
}

#[test]
fn caps_lock_does_not_pass_text_off_as_the_shifted_key() {
    // Caps Lock inverts Shift for letters: Shift+Caps+a produces "a", which
    // is not the shifted key. The alternate-key field still reports `A`.
    let all = kitty(KittyFlags::ALL);
    let ev = KeyEvent::new(Key::Char('a'), SHIFT | Modifiers::CAPS_LOCK).with_text("a");
    assert_eq!(encode_key(&ev, &all), b"\x1b[97:65;66;97u");
    // Without Caps Lock, text under Shift alone is the shifted key.
    let ev = KeyEvent::new(Key::Char('4'), SHIFT).with_text("€");
    assert_eq!(encode_key(&ev, &all), "\x1b[52:8364;2;8364u".as_bytes());
    // An explicit shifted key always wins.
    let ev = KeyEvent::new(Key::Char('a'), SHIFT | Modifiers::CAPS_LOCK)
        .with_shifted_key('A')
        .with_text("a");
    assert_eq!(encode_key(&ev, &all), b"\x1b[97:65;66;97u");
}

#[test]
fn text_only_events_are_delivered_as_text_unless_both_text_flags_apply() {
    let ime = KeyEvent::new(Key::Unidentified, NONE).with_text("漢字");
    // Report-all without associated text: the text is sent verbatim rather
    // than being dropped behind `CSI 0 u`.
    let report_all = kitty(KittyFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);
    assert_eq!(encode_key(&ime, &report_all), "漢字".as_bytes());
    // Associated text without report-all: still verbatim.
    let text_only = kitty(KittyFlags::REPORT_ASSOCIATED_TEXT);
    assert_eq!(encode_key(&ime, &text_only), "漢字".as_bytes());
    // Disambiguation does not turn an IME commit into `CSI 0 ; mods u`, even
    // when the frontend reports a modifier the OS consumed.
    let disambiguate = kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES);
    let ev = KeyEvent::new(Key::Unidentified, ALT).with_text("å");
    assert_eq!(encode_key(&ev, &disambiguate), "å".as_bytes());
    // Both flags: the spec's `CSI 0 ; ; text u` form.
    let both =
        kitty(KittyFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES | KittyFlags::REPORT_ASSOCIATED_TEXT);
    assert_eq!(encode_key(&ime, &both), b"\x1b[0;;28450:23383u");
    // An Unidentified event without text has nothing to say in any mode.
    for modes in [legacy(), report_all, both, kitty(KittyFlags::ALL)] {
        check(Key::Unidentified, NONE, modes, b"");
        let release = KeyEvent::new(Key::Unidentified, NONE)
            .with_text("x")
            .with_kind(KeyEventKind::Release);
        assert_eq!(encode_key(&release, &modes), b"");
    }
}

// ---------------------------------------------------------------------------
// Quickstart: forms produced under CSI > 1 u
// ---------------------------------------------------------------------------

#[test]
fn quickstart_forms() {
    let modes = kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES);
    let outputs = [
        enc(Key::Char('a'), NONE, modes),
        enc(Key::Char('a'), CTRL, modes),
        enc(Key::Char('x'), ALT | SHIFT, modes),
        enc(Key::Escape, NONE, modes),
        enc(Key::Up, NONE, modes),
        enc(Key::Up, CTRL, modes),
        enc(Key::Home, SHIFT, modes),
        enc(Key::F(1), NONE, modes),
        enc(Key::F(5), ALT, modes),
        enc(Key::F(20), NONE, modes),
        enc(Key::Enter, NONE, modes),
        enc(Key::Backspace, NONE, modes),
        enc(Key::Tab, NONE, modes),
    ];
    for bytes in outputs {
        assert!(
            matches_quickstart_form(&bytes),
            "`{}` is not a quickstart form",
            show(&bytes)
        );
    }
}

/// Plain UTF-8 text, `CSI number ; modifiers [u~]`, `CSI 1 ; modifiers
/// [ABCDEFHPQS]` (parameters optional), or the Enter/Backspace/Tab bytes.
fn matches_quickstart_form(bytes: &[u8]) -> bool {
    if matches!(bytes, [0x0d | 0x7f | 0x08 | 0x09]) {
        return true;
    }
    if let Some(rest) = bytes.strip_prefix(b"\x1b[") {
        let Some((terminator, params)) = rest.split_last() else {
            return false;
        };
        let fields: Vec<&[u8]> = params.split(|b| *b == b';').collect();
        let numeric = |f: &[u8]| !f.is_empty() && f.iter().all(u8::is_ascii_digit);
        return match terminator {
            b'u' | b'~' => {
                (fields.len() == 1 || fields.len() == 2) && fields.iter().all(|f| numeric(f))
            }
            b'A' | b'B' | b'C' | b'D' | b'E' | b'F' | b'H' | b'P' | b'Q' | b'S' => {
                params.is_empty() || (fields.len() == 2 && fields[0] == b"1" && numeric(fields[1]))
            }
            _ => false,
        };
    }
    std::str::from_utf8(bytes).is_ok_and(|s| !s.chars().any(char::is_control))
}

// ---------------------------------------------------------------------------
// Progressive enhancement flag arithmetic
// ---------------------------------------------------------------------------

#[test]
fn kitty_flags_apply_modes() {
    let current = KittyFlags::DISAMBIGUATE_ESCAPE_CODES | KittyFlags::REPORT_EVENT_TYPES;
    let incoming = KittyFlags::REPORT_EVENT_TYPES | KittyFlags::REPORT_ALTERNATE_KEYS;
    assert_eq!(current.apply(incoming, KittyFlagsMode::Replace), incoming);
    assert_eq!(
        current.apply(incoming, KittyFlagsMode::Set),
        KittyFlags::from_bits_truncate(0b111)
    );
    assert_eq!(
        current.apply(incoming, KittyFlagsMode::Clear),
        KittyFlags::DISAMBIGUATE_ESCAPE_CODES
    );
    assert_eq!(KittyFlags::from_bits_truncate(0xffff), KittyFlags::ALL);
    assert_eq!(KittyFlagsMode::from_param(1), Some(KittyFlagsMode::Replace));
    assert_eq!(KittyFlagsMode::from_param(2), Some(KittyFlagsMode::Set));
    assert_eq!(KittyFlagsMode::from_param(3), Some(KittyFlagsMode::Clear));
    assert_eq!(KittyFlagsMode::from_param(0), None);
    assert_eq!(format!("{:?}", KittyFlags::ALL), "KittyFlags(0b11111)");
    assert_eq!(
        format!("{:?}", CTRL | SHIFT | Modifiers::NUM_LOCK),
        "SHIFT|CTRL|NUM_LOCK"
    );
    assert_eq!(format!("{NONE:?}"), "NONE");
}

// ---------------------------------------------------------------------------
// Titlecase letters (issue #73)
// ---------------------------------------------------------------------------

/// The spec-researcher fixture for titlecase (Lt) letters: its Table A lists
/// every character the encoder used to rewrite, its Table B the expected
/// encodings and its Table C the regression guards.
const TITLECASE_FIXTURE: &str = include_str!("fixtures/keyboard_titlecase.txt");

/// The characters of Table A in [`TITLECASE_FIXTURE`]: the `#   ` lines
/// between `# TABLE A` and `# Columns:`, each a list of hex scalars or
/// `FIRST..LAST` ranges, with an optional trailing `#` comment.
fn titlecase_fixture_chars() -> Vec<char> {
    let hex = |s: &str| {
        u32::from_str_radix(s, 16)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or_else(|| panic!("bad scalar {s:?} in Table A"))
    };
    let table = TITLECASE_FIXTURE
        .split_once("# TABLE A")
        .and_then(|(_, rest)| rest.split_once("# Columns:"))
        .expect("Table A in the fixture")
        .0;
    let mut chars = Vec::new();
    for line in table.lines().filter_map(|l| l.strip_prefix("#   ")) {
        let scalars = line.split('#').next().unwrap_or_default();
        for token in scalars.split_whitespace() {
            match token.split_once("..") {
                Some((first, last)) => chars.extend(hex(first)..=hex(last)),
                None => chars.push(hex(token)),
            }
        }
    }
    chars
}

/// The single-scalar mapping of an iterator of case-mapped characters.
fn single(mut mapped: impl Iterator<Item = char>) -> Option<char> {
    match (mapped.next(), mapped.next()) {
        (Some(m), None) => Some(m),
        _ => None,
    }
}

/// Encodes a press of `c` with `mods` under every kitty enhancement and
/// parses the CSI u result.
fn kitty_all(c: char, mods: Modifiers) -> CsiU {
    let bytes = enc(Key::Char(c), mods, kitty(KittyFlags::ALL));
    parse_csi_u(&bytes).unwrap_or_else(|| panic!("{c:?} {mods:?}: not CSI u: {}", show(&bytes)))
}

#[test]
fn titlecase_fixture_lists_every_titlecase_letter_with_a_simple_lowercase() {
    let listed = titlecase_fixture_chars();
    assert_eq!(listed.len(), 31, "Table A count");
    // Table A is the set of characters `arb_char` admits whose lowercase is
    // another single scalar: exactly the characters the old encoder
    // lowercased although they are not upper case.
    let scanned: Vec<char> = (char::MIN..=char::MAX)
        .filter(|&c| {
            !c.is_control() && !c.is_uppercase() && single(c.to_lowercase()).is_some_and(|l| l != c)
        })
        .collect();
    assert_eq!(listed, scanned);
}

/// Table B: a titlecase letter is its own unshifted key. Kitty reports the
/// scalar as given in the key code and the text; legacy mode sends its UTF-8.
/// Caps Lock leaves the kitty key code alone and, as for any letter that is
/// not lower case, lowercases the legacy text (U+1F88 -> U+1F80).
#[test]
fn titlecase_letters_are_their_own_unshifted_key() {
    let caps = Modifiers::CAPS_LOCK;
    for c in titlecase_fixture_chars() {
        let code = u32::from(c);
        let lower = single(c.to_lowercase()).expect("simple lowercase");
        // Shift: the single-scalar uppercase (Dž -> Ǆ), else the letter itself
        // (ᾈ's uppercase expands to two scalars).
        let shifted = single(c.to_uppercase()).unwrap_or(c);

        let plain = kitty_all(c, NONE);
        assert_eq!(plain.key, code, "{c:?} key");
        assert_eq!(plain.shifted, None, "{c:?} shifted");
        assert_eq!(plain.mods, 0, "{c:?} mods");
        assert_eq!(plain.text, [code], "{c:?} text");

        let shift = kitty_all(c, SHIFT);
        assert_eq!(shift.key, code, "{c:?}+Shift key");
        assert_eq!(
            shift.shifted,
            (shifted != c).then_some(u32::from(shifted)),
            "{c:?}+Shift shifted"
        );
        assert_eq!(shift.text, [u32::from(shifted)], "{c:?}+Shift text");

        let locked = kitty_all(c, caps);
        assert_eq!(locked.key, code, "{c:?}+Caps key");
        assert_eq!(locked.mods, caps.bits(), "{c:?}+Caps mods");
        assert_eq!(locked.text.len(), 1, "{c:?}+Caps text");

        let utf8 = |c: char| c.to_string().into_bytes();
        assert_eq!(enc(Key::Char(c), NONE, legacy()), utf8(c), "{c:?} legacy");
        assert_eq!(
            enc(Key::Char(c), Modifiers::NUM_LOCK, legacy()),
            utf8(c),
            "{c:?}+NumLock legacy"
        );
        assert_eq!(
            enc(Key::Char(c), SHIFT, legacy()),
            utf8(shifted),
            "{c:?}+Shift legacy"
        );
        assert_eq!(
            enc(Key::Char(c), caps, legacy()),
            utf8(lower),
            "{c:?}+Caps legacy"
        );
    }
}

/// Table B's pinned rows for one Latin digraph and two Greek letters.
#[test]
fn titlecase_fixture_table_b_samples() {
    let caps = Modifiers::CAPS_LOCK;
    for (c, shift_text, caps_legacy) in [
        ('\u{1F88}', '\u{1F88}', "\u{1F80}"),
        ('\u{01C5}', '\u{01C4}', "\u{01C6}"),
        ('\u{1FBC}', '\u{1FBC}', "\u{1FB3}"),
    ] {
        let code = u32::from(c);
        let plain = kitty_all(c, NONE);
        assert_eq!((plain.key, plain.text), (code, vec![code]), "{c:?}");
        let shift = kitty_all(c, SHIFT);
        assert_eq!(shift.key, code, "{c:?}+Shift");
        assert_eq!(shift.text, [u32::from(shift_text)], "{c:?}+Shift");
        let expected_shifted = (c == '\u{01C5}').then_some(0x01C4);
        assert_eq!(shift.shifted, expected_shifted, "{c:?}+Shift");
        let locked = kitty_all(c, caps);
        assert_eq!((locked.key, locked.mods), (code, 64), "{c:?}+Caps");
        assert_eq!(enc(Key::Char(c), NONE, legacy()), c.to_string().as_bytes());
        assert_eq!(
            enc(Key::Char(c), SHIFT, legacy()),
            shift_text.to_string().as_bytes()
        );
        assert_eq!(enc(Key::Char(c), caps, legacy()), caps_legacy.as_bytes());
    }
}

/// Table C: upper-case letters still report their lowercase as the key, and
/// lowercase or uncased letters are reported as given.
#[test]
fn titlecase_fix_keeps_cased_letters_unchanged() {
    for (c, key) in [
        ('A', 'a'),
        ('ß', 'ß'),
        ('\u{0130}', '\u{0130}'),
        ('\u{01C4}', '\u{01C6}'),
        ('\u{01C6}', '\u{01C6}'),
        ('\u{1F80}', '\u{1F80}'),
    ] {
        assert_eq!(kitty_all(c, NONE).key, u32::from(key), "{c:?}");
    }
    let shift_a = kitty_all('a', SHIFT);
    assert_eq!(
        (shift_a.key, shift_a.shifted, shift_a.text),
        (0x61, Some(0x41), vec![0x41])
    );
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

/// A parsed `CSI key[:shifted[:base]] ; mods[:event] ; text u` sequence.
#[derive(Debug, PartialEq, Eq)]
struct CsiU {
    key: u32,
    shifted: Option<u32>,
    base: Option<u32>,
    mods: u8,
    event: u8,
    text: Vec<u32>,
}

fn parse_csi_u(bytes: &[u8]) -> Option<CsiU> {
    let body = bytes.strip_prefix(b"\x1b[")?.strip_suffix(b"u")?;
    let body = std::str::from_utf8(body).ok()?;
    let mut fields = body.split(';');
    let mut keys = fields.next()?.split(':');
    let key = keys.next()?.parse().ok()?;
    let shifted = match keys.next().filter(|s| !s.is_empty()) {
        None => None,
        Some(s) => Some(s.parse().ok()?),
    };
    let base = match keys.next() {
        None => None,
        Some(s) => Some(s.parse().ok()?),
    };
    let (mods, event) = match fields.next() {
        None | Some("") => (1, 1),
        Some(m) => {
            let mut sub = m.split(':');
            let mods: u32 = sub.next()?.parse().ok()?;
            let event = sub.next().map_or(Some(1), |e| e.parse().ok())?;
            (mods, event)
        }
    };
    let text = match fields.next() {
        None => Vec::new(),
        Some(t) => t
            .split(':')
            .map(|c| c.parse().ok())
            .collect::<Option<_>>()?,
    };
    if fields.next().is_some() {
        return None;
    }
    Some(CsiU {
        key,
        shifted,
        base,
        mods: u8::try_from(mods.checked_sub(1)?).ok()?,
        event,
        text,
    })
}

fn arb_kind() -> impl Strategy<Value = KeyEventKind> {
    prop_oneof![
        Just(KeyEventKind::Press),
        Just(KeyEventKind::Repeat),
        Just(KeyEventKind::Release),
    ]
}

fn arb_char() -> impl Strategy<Value = char> {
    // Already unshifted, so the key code is the character itself: no
    // upper-case letters (the encoder lowercases those), nothing whose
    // lowercase is several scalars, and no controls. Titlecase (Lt) letters
    // such as `ǅ` and `ᾈ` are kept on purpose: no keyboard layout produces
    // one, but they can arrive in a `Key::Char` from composed or synthetic
    // events, and they exercise the encoder's rule that only `is_uppercase`
    // letters are lowercased, so a titlecase letter is its own unshifted
    // key (#73).
    any::<char>().prop_filter("unshifted, printable", |c| {
        !c.is_control() && !c.is_uppercase() && c.to_lowercase().count() == 1
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// With every enhancement on, a `Char` event round-trips through a CSI u
    /// parser: key code, modifiers, event type and text all survive.
    #[test]
    fn full_kitty_mode_round_trips(
        c in arb_char(),
        mods in any::<u8>(),
        kind in arb_kind(),
        base in proptest::option::of(arb_char()),
    ) {
        let modes = kitty(KittyFlags::ALL);
        let mods = Modifiers::from_bits(mods);
        let mut ev = KeyEvent::new(Key::Char(c), mods).with_kind(kind);
        if let Some(b) = base {
            ev = ev.with_base_layout_key(b);
        }
        let bytes = encode_key(&ev, &modes);
        let parsed = parse_csi_u(&bytes).expect("CSI u");
        prop_assert_eq!(parsed.key, u32::from(c));
        prop_assert_eq!(parsed.mods, mods.bits());
        let expected_event = match kind {
            KeyEventKind::Press => 1,
            KeyEventKind::Repeat => 2,
            KeyEventKind::Release => 3,
        };
        prop_assert_eq!(parsed.event, expected_event);
        prop_assert_eq!(parsed.base, base.filter(|b| *b != c).map(u32::from));
        if mods.contains(SHIFT) {
            // The shifted key is reported only when it differs.
            prop_assert!(parsed.shifted.is_none_or(|s| s != u32::from(c)));
        } else {
            prop_assert_eq!(parsed.shifted, None);
        }
        // Text is present exactly when the key could have produced it: a
        // press or repeat with nothing but Shift and locks held. It is then a
        // single, non-control code point.
        let produces_text = kind != KeyEventKind::Release && SHIFT.contains(mods.without_locks());
        if produces_text {
            prop_assert_eq!(parsed.text.len(), 1);
            prop_assert!(char::from_u32(parsed.text[0]).is_some_and(|t| !t.is_control()));
        } else {
            prop_assert!(parsed.text.is_empty(), "{}", show(&bytes));
        }
    }

    /// Unmodified presses of a character in legacy mode are the character's
    /// UTF-8, with Num Lock ignored and Caps Lock only inverting the case of
    /// a letter.
    #[test]
    fn legacy_plain_char_is_utf8(c in arb_char(), locks in 0u8..4) {
        let mods = Modifiers::from_bits(locks << 6);
        let bytes = encode_key(&KeyEvent::new(Key::Char(c), mods), &legacy());
        let expected = if mods.contains(Modifiers::CAPS_LOCK) && c.is_alphabetic() {
            let swapped: String = if c.is_lowercase() {
                c.to_uppercase().collect()
            } else {
                c.to_lowercase().collect()
            };
            if swapped.chars().count() == 1 { swapped } else { c.to_string() }
        } else {
            c.to_string()
        };
        prop_assert_eq!(bytes, expected.into_bytes());
    }

    /// Legacy output for a character is plain UTF-8 text when only Shift is
    /// involved, a single ASCII byte or an escape sequence under Ctrl (with
    /// or without Shift), and always starts with ESC under
    /// Alt/Super/Hyper/Meta.
    #[test]
    fn legacy_output_is_text_or_escape(c in arb_char(), mods in 0u8..64) {
        let mods = Modifiers::from_bits(mods);
        let bytes = encode_key(&KeyEvent::new(Key::Char(c), mods), &legacy());
        prop_assert!(!bytes.is_empty(), "{c:?} {mods:?}");
        if mods.intersects(ALT | SUPER | Modifiers::HYPER | Modifiers::META) {
            prop_assert_eq!(bytes[0], 0x1b, "{}", show(&bytes));
        } else if mods.contains(CTRL) {
            prop_assert!(bytes[0] == 0x1b || (bytes.len() == 1 && bytes[0].is_ascii()),
                "{}", show(&bytes));
        } else {
            prop_assert!(std::str::from_utf8(&bytes).is_ok());
        }
    }

    /// Disambiguated kitty output for a modified character is always a
    /// well-formed CSI u carrying the unshifted key.
    #[test]
    fn disambiguate_output_parses(c in arb_char(), mods in 1u8..64) {
        let mods = Modifiers::from_bits(mods);
        prop_assume!(!Modifiers::SHIFT.contains(mods));
        let modes = kitty(KittyFlags::DISAMBIGUATE_ESCAPE_CODES);
        let bytes = encode_key(&KeyEvent::new(Key::Char(c), mods), &modes);
        let parsed = parse_csi_u(&bytes).expect("CSI u");
        prop_assert_eq!(parsed.key, u32::from(c));
        prop_assert_eq!(parsed.mods, mods.bits());
        prop_assert_eq!(parsed.event, 1);
    }
}
