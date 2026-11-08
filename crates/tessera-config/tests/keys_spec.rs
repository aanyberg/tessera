//! The spec fixture for issue #43 (`tests/fixtures/keys-grammar-cases.toml`)
//! run against the chord and value grammar and the `[keys]` table.
//!
//! `[[chord]]` and `[[value]]` cases are iterated; `[[table]]` cases are
//! prose and implemented by `id`. Cases the command registry decides
//! (`registry = ...`, and the table cases about default chords) are run by
//! `tessera-app-core/tests/keybindings.rs` over the same file.

use std::fmt::Write as _;
use std::path::Path;

use proptest::prelude::*;
use tessera_config::{
    Chord, ChordError, Config, KeyAction, Keys, MAX_CHORD_LEN, MAX_KEY_BINDINGS, MAX_SEND_TEXT_LEN,
    Warning,
};

fn spec() -> toml::Table {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keys-grammar-cases.toml");
    toml::from_str(&std::fs::read_to_string(path).unwrap()).expect("the fixture is valid TOML")
}

fn rows<'a>(spec: &'a toml::Table, table: &str) -> Vec<&'a toml::Table> {
    spec[table]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row.as_table().unwrap())
        .collect()
}

fn s<'a>(row: &'a toml::Table, key: &str) -> Option<&'a str> {
    row.get(key).map(|value| value.as_str().unwrap())
}

/// The variant name of an error, from its `Debug` form.
fn variant(error: &impl std::fmt::Debug) -> String {
    format!("{error:?}")
        .split(['(', ' ', '{'])
        .next()
        .unwrap()
        .to_owned()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        write!(out, "{b:02x}").unwrap();
        out
    })
}

/// The value text of a `[[value]]` row (long ones are written as a
/// repeated unit).
fn value_input(row: &toml::Table) -> String {
    if let Some(input) = s(row, "input") {
        return input.to_owned();
    }
    let count = usize::try_from(row["count"].as_integer().unwrap()).unwrap();
    format!(
        "{}{}",
        s(row, "prefix").unwrap(),
        s(row, "repeat").unwrap().repeat(count)
    )
}

#[test]
fn every_chord_case() {
    let spec = spec();
    let chords = rows(&spec, "chord");
    assert_eq!(chords.len(), 79);
    for row in chords {
        let input = s(row, "input").unwrap();
        let parsed = Chord::parse(input);
        match (s(row, "canonical"), s(row, "error")) {
            (Some(canonical), None) => {
                let chord = parsed.unwrap_or_else(|e| panic!("{input:?}: {e}"));
                assert_eq!(chord.to_string(), canonical, "{input:?}");
                // The canonical form is a fixed point.
                assert_eq!(Chord::parse(canonical), Ok(chord), "{canonical:?}");
                assert_eq!(
                    chord.shadows_typing(),
                    row["shadows"].as_bool().unwrap(),
                    "{input:?} shadows"
                );
            }
            (None, Some(error)) => {
                let e = parsed.expect_err(input);
                assert_eq!(variant(&e), error, "{input:?}: {e}");
            }
            _ => panic!("bad row {row:?}"),
        }
    }
}

#[test]
fn every_value_case() {
    let spec = spec();
    let values = rows(&spec, "value");
    assert_eq!(values.len(), 32);
    for row in values {
        let input = value_input(row);
        let shown = &input[..input.len().min(40)];
        let parsed = KeyAction::parse(&input);
        if let Some(error) = s(row, "error") {
            let e = parsed.expect_err(shown);
            assert_eq!(variant(&e), error, "{shown:?}: {e}");
            continue;
        }
        let action = parsed.unwrap_or_else(|e| panic!("{shown:?}: {e}"));
        if s(row, "registry").is_some() {
            // Syntactically a command; the registry rejects it.
            assert!(matches!(action, KeyAction::Command { .. }), "{shown:?}");
            continue;
        }
        match (s(row, "action").unwrap(), action) {
            ("command", KeyAction::Command { name, parameter }) => {
                assert_eq!(name, s(row, "name").unwrap());
                assert_eq!(parameter.unwrap_or_default(), s(row, "parameter").unwrap());
            }
            ("text", KeyAction::SendText(bytes)) => match s(row, "bytes") {
                Some(expected) => assert_eq!(hex(&bytes), expected, "{shown:?}"),
                None => assert_eq!(
                    bytes.len(),
                    usize::try_from(row["bytes_len"].as_integer().unwrap()).unwrap()
                ),
            },
            ("unbound", KeyAction::Unbound) | ("ignore", KeyAction::Ignore) => {}
            (expected, action) => panic!("{shown:?}: {action:?}, expected {expected}"),
        }
    }
}

/// A `[keys]` table of `n` distinct chords, `"cmd+ctrl+<CJK char>" =
/// "none"`, in that order.
fn table_of(n: usize) -> String {
    let mut text = String::from("[keys]\n");
    for i in 0..n {
        writeln!(text, "\"{}\" = \"none\"", key(i)).unwrap();
    }
    text
}

/// The key of entry `i` of [`table_of`].
fn key(i: usize) -> String {
    let c = char::from_u32(0x4E00 + u32::try_from(i).unwrap()).unwrap();
    format!("cmd+ctrl+{c}")
}

#[test]
fn table_cases() {
    let spec = spec();
    let mut seen = Vec::new();
    for row in rows(&spec, "table") {
        let id = row["id"].as_integer().unwrap();
        seen.push(id);
        match id {
            1 => {
                // 512 entries all kept. Distinct chords only.
                let loaded = Config::from_toml_lenient(&table_of(MAX_KEY_BINDINGS)).unwrap();
                assert_eq!(loaded.config.keys.bindings().len(), MAX_KEY_BINDINGS);
                assert_eq!(loaded.warnings, []);
            }
            2 => {
                // 513 and later dropped with ONE warning, whatever the count.
                for n in [MAX_KEY_BINDINGS + 1, 4 * MAX_KEY_BINDINGS] {
                    let loaded = Config::from_toml_lenient(&table_of(n)).unwrap();
                    assert_eq!(loaded.config.keys.bindings().len(), MAX_KEY_BINDINGS);
                    assert_eq!(
                        loaded.warnings,
                        [Warning::TooManyKeyBindings {
                            dropped: n - MAX_KEY_BINDINGS
                        }]
                    );
                    // The first 512 *written* are kept, not the first 512
                    // by sort order.
                    let last = loaded.config.keys.bindings().last().unwrap();
                    assert_eq!(last.key, key(MAX_KEY_BINDINGS - 1));
                }
            }
            3 => {
                let error =
                    Config::from_toml_lenient("[keys]\n'cmd+t' = 'new_tab'\n'cmd+t' = 'copy'\n");
                assert!(error.is_err(), "TOML rejects a duplicate key itself");
            }
            4 => {
                let loaded = Config::from_toml_lenient(
                    "[keys]\n'cmd+shift+d' = 'split_down'\n'shift+cmd+d' = 'split_down'\n",
                )
                .unwrap();
                assert_eq!(loaded.config.keys.bindings().len(), 1);
                assert_eq!(loaded.config.keys.bindings()[0].key, "cmd+shift+d");
                assert_eq!(loaded.warnings.len(), 1);
                let text = loaded.warnings[0].to_string();
                assert!(
                    text.contains("shift+cmd+d") && text.contains("same chord as `cmd+shift+d`"),
                    "{text}"
                );
            }
            // Default chords: the registry's (tessera-app-core).
            5..=10 => {}
            other => panic!("table case {other} is not handled"),
        }
    }
    assert_eq!(seen, (1..=10).collect::<Vec<_>>());
}

/// Every malformed entry is one warning naming its key; valid entries
/// around it are kept (the "Done when" bar).
#[test]
fn one_warning_per_bad_entry_naming_the_key() {
    let loaded = Config::from_toml_lenient(
        "[keys]\n'cmd+t' = 'new_tab'\n'cmd+hyper+x' = 'copy'\n'cmd+y' = ''\n'cmd+u' = 'send_text:\\q'\n'cmd+i' = 'paste'\n",
    )
    .unwrap();
    let kept: Vec<&str> = loaded
        .config
        .keys
        .bindings()
        .iter()
        .map(|b| b.key.as_str())
        .collect();
    assert_eq!(kept, ["cmd+t", "cmd+i"]);
    let named: Vec<String> = loaded.warnings.iter().map(ToString::to_string).collect();
    assert_eq!(named.len(), 3, "{named:?}");
    for (warning, key) in named.iter().zip(["cmd+hyper+x", "cmd+y", "cmd+u"]) {
        assert!(warning.contains(&format!("\"{key}\"")), "{warning}");
        assert!(warning.ends_with("ignored"), "{warning}");
    }
}

/// S5 flood: a key, a command name and a `send_text` far past their caps
/// each give one bounded warning, and the warning itself is capped.
#[test]
fn floods_past_each_cap_are_dropped_with_bounded_warnings() {
    let long_key = "cmd+".to_owned() + &"x".repeat(1 << 20);
    let long_name = "y".repeat(1 << 20);
    let long_text = "send_text:".to_owned() + &"\\x41".repeat(1 << 18);
    let keys = Keys::from_entries([
        (long_key.as_str(), "new_tab"),
        ("cmd+a", long_name.as_str()),
        ("cmd+b", long_text.as_str()),
        ("cmd+c", "send_text:\\u{10ffff}"),
    ]);
    assert_eq!(keys.bindings().len(), 1, "only cmd+c survives");
    let mut keys = keys;
    let warnings = keys.take_warnings();
    assert_eq!(warnings.len(), 3);
    for warning in &warnings {
        assert!(
            warning.to_string().len() < 600,
            "{} bytes",
            warning.to_string().len()
        );
    }
    assert!(
        warnings[0]
            .to_string()
            .contains(&format!("longer than {MAX_CHORD_LEN}"))
    );
    assert!(
        warnings[2]
            .to_string()
            .contains(&format!("longer than {MAX_SEND_TEXT_LEN}"))
    );
    // The 4096-byte boundary itself (the fixture has it too).
    let exact = "send_text:".to_owned() + &"\\x41".repeat(MAX_SEND_TEXT_LEN);
    assert!(
        matches!(KeyAction::parse(&exact), Ok(KeyAction::SendText(b)) if b.len() == MAX_SEND_TEXT_LEN)
    );
    // A chord of exactly 64 bytes passes the length check.
    let at_cap = format!("cmd+{}", "a".repeat(MAX_CHORD_LEN - 4));
    assert_eq!(at_cap.len(), MAX_CHORD_LEN);
    assert!(!matches!(
        Chord::parse(&at_cap),
        Err(tessera_config::ChordError::TooLong)
    ));
}

/// Characters that show as nothing: bidi formatting, zero-width,
/// variation selectors, tags and the other default-ignorables.
const INVISIBLE: [char; 16] = [
    '\u{AD}',
    '\u{34F}',
    '\u{61C}',
    '\u{115F}',
    '\u{180E}',
    '\u{200B}',
    '\u{200C}',
    '\u{200D}',
    '\u{200E}',
    '\u{202E}',
    '\u{2060}',
    '\u{2066}',
    '\u{3164}',
    '\u{FE0F}',
    '\u{FEFF}',
    '\u{E0041}',
];

/// M2 (#317): a chord whose key is invisible is refused with one warning,
/// which shows the key as its escape; visible keys beyond ASCII still
/// bind.
#[test]
fn invisible_keys_are_refused_with_a_warning() {
    for c in INVISIBLE {
        for chord in [format!("cmd+{c}"), format!("ctrl+alt+{c}"), c.to_string()] {
            let parsed = Chord::parse(&chord);
            assert_eq!(
                parsed,
                Err(ChordError::InvisibleKey(c)),
                "U+{:04X} in {chord:?}",
                u32::from(c)
            );
            let mut keys = Keys::from_entries([(chord.as_str(), "new_tab")]);
            assert!(keys.is_empty(), "U+{:04X} bound", u32::from(c));
            let warnings = keys.take_warnings();
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            let text = warnings[0].to_string();
            assert!(!text.contains(c), "raw U+{:04X} in {text:?}", u32::from(c));
            let escape: String = c.escape_unicode().collect();
            assert!(text.contains(&escape), "{text}");
            assert!(text.contains("invisible key"), "{text}");
        }
    }
    for chord in [
        "cmd+é",
        "cmd+ö",
        "cmd+ß",
        "cmd+€",
        "cmd+\u{2010}",
        "cmd+т",
        "cmd+\u{301}",
    ] {
        let keys = Keys::from_entries([(chord, "new_tab")]);
        assert_eq!(keys.bindings().len(), 1, "{chord:?} is visible");
    }
}

/// M1 (#317): an `UnknownKey` segment carrying a bidi override, a
/// zero-width character or (through the control check) a C0 control
/// reaches the warning escaped, in the reason as well as the key.
#[test]
fn unknown_key_warnings_are_escaped() {
    for (chord, raw, escape) in [
        ("cmd+a\u{202E}b", '\u{202E}', "\\u{202e}"),
        ("cmd+a\u{200B}b", '\u{200B}', "\\u{200b}"),
        ("cmd\u{202E}+t", '\u{202E}', "\\u{202e}"),
        ("cmd\u{200B}+t", '\u{200B}', "\\u{200b}"),
        ("cmd+a\u{FEFF}\u{2066}b", '\u{FEFF}', "\\u{feff}"),
    ] {
        let error = Chord::parse(chord).unwrap_err();
        assert!(
            matches!(error, ChordError::UnknownKey(_)),
            "{chord:?}: {error:?}"
        );
        assert!(!error.to_string().contains(raw), "{error}");
        assert!(error.to_string().contains(escape), "{error}");
        let mut keys = Keys::from_entries([(chord, "new_tab")]);
        let text = keys.take_warnings()[0].to_string();
        assert!(
            !text.contains(raw),
            "raw U+{:04X} in {text:?}",
            u32::from(raw)
        );
        // The key and the reason both carry the escape.
        assert_eq!(text.matches(escape).count(), 2, "{text}");
    }
    let mut keys = Keys::from_entries([("cmd+a\u{1}b", "new_tab")]);
    let text = keys.take_warnings()[0].to_string();
    assert!(
        !text.contains('\u{1}') && text.contains("\\u{1}"),
        "{text:?}"
    );
    // A value too: the command name's warning escapes its invisibles.
    let mut keys = Keys::from_entries([("cmd+y", "new\u{200B}tab")]);
    let text = keys.take_warnings()[0].to_string();
    assert!(
        !text.contains('\u{200B}') && text.contains("\\u{200b}"),
        "{text:?}"
    );
}

/// A table far past the cap costs work for the first entries only: the
/// rest are not parsed (they are one warning), so loading stays linear.
#[test]
fn a_huge_table_loads_with_one_warning() {
    let text = table_of(20_000);
    let loaded = Config::from_toml_lenient(&text).unwrap();
    assert_eq!(loaded.config.keys.bindings().len(), MAX_KEY_BINDINGS);
    let too_many = loaded
        .warnings
        .iter()
        .filter(|w| matches!(w, Warning::TooManyKeyBindings { .. }))
        .count();
    assert_eq!(too_many, 1);
    assert_eq!(loaded.warnings.len(), 1);
}

/// Keys that may appear in a chord: modifiers, names, and characters
/// (including ones the grammar rejects).
fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::sample::select(vec![
            "cmd",
            "Cmd",
            "command",
            "super",
            "ctrl",
            "control",
            "alt",
            "opt",
            "option",
            "shift",
            "hyper",
            "fn",
            "esc",
            "enter",
            "return",
            "space",
            "page_up",
            "f1",
            "f35",
            "f36",
            "kp_0",
            "kp_enter",
            "plus",
            "minus",
            "left_bracket",
            "",
            ">",
            ":",
        ])
        .prop_map(str::to_owned),
        any::<char>().prop_map(|c| c.to_string()),
        prop::sample::select(INVISIBLE.to_vec()).prop_map(|c| c.to_string()),
        "[a-z+]{0,3}",
    ]
}

proptest! {
    /// Any text parses or fails without panicking; what parses prints a
    /// canonical form that parses back to the same chord and is itself
    /// within the cap.
    #[test]
    fn chords_round_trip_through_their_canonical_form(
        parts in prop::collection::vec(segment(), 0..6),
    ) {
        let text = parts.join("+");
        if let Ok(chord) = Chord::parse(&text) {
            let canonical = chord.to_string();
            prop_assert_eq!(Chord::parse(&canonical), Ok(chord), "{:?} -> {:?}", text, canonical);
            prop_assert!(canonical.len() <= MAX_CHORD_LEN + 16);
            prop_assert_eq!(chord.to_string(), canonical);
        }
        // Accepted or not, nothing invisible is shown: not in the chord a
        // menu prints, not in the warning.
        let mut keys = Keys::from_entries([(text.as_str(), "new_tab")]);
        let mut shown: Vec<String> = keys
            .bindings()
            .iter()
            .map(|binding| binding.chord.to_string())
            .collect();
        shown.extend(keys.take_warnings().iter().map(ToString::to_string));
        for shown in shown {
            prop_assert!(!shown.contains(INVISIBLE), "{:?} shows {:?}", text, shown);
        }
    }

    /// Unescaping never panics and never yields more than the cap.
    #[test]
    fn send_text_is_bounded(body in ".{0,64}", reps in 0usize..200) {
        let text = format!("send_text:{}", body.repeat(reps));
        if let Ok(KeyAction::SendText(bytes)) = KeyAction::parse(&text) {
            prop_assert!(bytes.len() <= MAX_SEND_TEXT_LEN);
            prop_assert!(!bytes.is_empty());
        }
    }

    /// Escaping arbitrary bytes as `\xHH` and parsing gives them back.
    #[test]
    fn hex_escapes_round_trip(bytes in prop::collection::vec(any::<u8>(), 1..64)) {
        let text = bytes.iter().fold(String::new(), |mut out, b| {
            write!(out, "\\x{b:02x}").unwrap();
            out
        });
        prop_assert_eq!(KeyAction::parse(&format!("send_text:{text}")), Ok(KeyAction::SendText(bytes)));
    }
}
