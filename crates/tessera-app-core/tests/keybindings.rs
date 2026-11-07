//! The command registry and the key map (#43, ADR 0036) against the spec
//! fixtures: `fixtures/default-chords.csv` (the spec-researcher's default
//! chord table, which reproduces the key equivalents the menus shipped
//! with, so "no shortcut changes" is a table test) and the registry rows
//! of `tessera-config`'s `keys-grammar-cases.toml`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fmt::Write as _;
use std::path::Path;

use tessera_app_core::Config;
use tessera_app_core::command::{
    COMMANDS, CommandId, CommandOwner, Keymap, MenuShortcut, menu_shortcut,
};
use tessera_app_core::input::{Key, KeyEvent, Modifiers};
use tessera_config::{Chord, ChordModifiers, Keys, MAX_KEY_BINDINGS, Warning};

/// Counts this thread's allocations.
struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to `System`; the counter is a thread-local `Cell`,
// which allocates nothing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// One row of the default-chord table.
struct Row {
    command: String,
    chord: String,
    alt_chords: String,
    key_equivalent: String,
    mask: String,
    owner: String,
}

/// The fields of one CSV line; a field in double quotes may hold commas.
fn fields(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                out.last_mut().unwrap().push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(String::new()),
            c => out.last_mut().unwrap().push(c),
        }
    }
    out
}

fn rows() -> Vec<Row> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/default-chords.csv");
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines().filter(|line| !line.starts_with('#'));
    let header = fields(lines.next().unwrap());
    let column = |name: &str| header.iter().position(|h| h == name).unwrap();
    let [command, chord, alt, key, mask, owner] = [
        "command",
        "chord",
        "alt_chords",
        "appkit_keyEquivalent",
        "appkit_mask",
        "owner",
    ]
    .map(column);
    lines
        .map(|line| {
            let f = fields(line);
            Row {
                command: f[command].clone(),
                chord: f[chord].clone(),
                alt_chords: f[alt].clone(),
                key_equivalent: f[key].clone(),
                mask: f[mask].clone(),
                // The free-text columns before `owner` are not always
                // quoted (`cmd+1 (S-TERM,unverified: …)`): take the first
                // owner-shaped field from its column on.
                owner: f[owner..]
                    .iter()
                    .find(|field| {
                        matches!(field.as_str(), "system" | "app") || field.starts_with("later:")
                    })
                    .unwrap()
                    .clone(),
            }
        })
        .collect()
}

/// `name[:parameter]` as the registry knows it.
fn command(text: &str) -> CommandId {
    let (name, parameter) = match text.split_once(':') {
        Some((name, parameter)) => (name, Some(parameter)),
        None => (text, None),
    };
    CommandId::find(name, parameter).unwrap_or_else(|error| panic!("{text}: {error}"))
}

/// The table's `appkit_keyEquivalent`: a literal character, or `F:Name`
/// for an `NS…FunctionKey` character (`F:\r` is Return).
fn key_equivalent(text: &str) -> char {
    let Some(name) = text.strip_prefix("F:") else {
        let mut chars = text.chars();
        let c = chars.next().unwrap();
        assert_eq!(chars.next(), None, "{text}");
        return c;
    };
    let offset = match name {
        "UpArrow" => 0x00,
        "DownArrow" => 0x01,
        "LeftArrow" => 0x02,
        "RightArrow" => 0x03,
        "Home" => 0x29,
        "End" => 0x2B,
        "PageUp" => 0x2C,
        "PageDown" => 0x2D,
        "\\r" => return '\r',
        _ => panic!("unknown function key {name}"),
    };
    char::from_u32(0xF700 + offset).unwrap()
}

/// The table's `appkit_mask` (`cmd+shift`, `cmd (shift implied by '{')`).
fn mask(text: &str) -> ChordModifiers {
    let text = text.split(" (").next().unwrap();
    text.split('+')
        .map(|name| match name {
            "cmd" => ChordModifiers::SUPER,
            "shift" => ChordModifiers::SHIFT,
            "alt" => ChordModifiers::ALT,
            "ctrl" => ChordModifiers::CTRL,
            _ => panic!("unknown modifier {name}"),
        })
        .fold(ChordModifiers::NONE, |all, one| all | one)
}

fn chord(text: &str) -> Chord {
    Chord::parse(text).unwrap_or_else(|error| panic!("{text}: {error}"))
}

fn keymap(toml: &str) -> Keymap {
    let config = Config::from_toml_lenient(&format!("[keys]\n{toml}"))
        .unwrap()
        .config;
    Keymap::compile(&config.keys)
}

fn bound(map: &Keymap, event: &KeyEvent) -> Option<String> {
    map.resolve(event).map(|binding| format!("{binding:?}"))
}

fn command_of(map: &Keymap, event: &KeyEvent) -> Option<CommandId> {
    match map.resolve(event)? {
        tessera_app_core::command::Binding::Command(id) => Some(id),
        _ => None,
    }
}

fn event(key: Key, modifiers: Modifiers) -> KeyEvent {
    KeyEvent::new(key, modifiers)
}

/// The spec table, row by row: every command is in the registry with the
/// table's owner and default chords (aliases after the menu's), each
/// default resolves to it in the default map, and its menu shows the key
/// equivalent and mask the menus shipped with. The prompt commands have
/// no default chord of their own: ⌘↑ / ⌘↓ stay `scroll_page_up` / `scroll_page_down`'s
/// (N1). `send_text` is a value form, not a registry command.
#[test]
fn default_chords_match_the_spec_table() {
    let rows = rows();
    let map = Keymap::compile(&Keys::default());
    let mut covered = Vec::new();
    for row in rows.iter().filter(|row| row.command != "send_text") {
        let id = command(&row.command);
        covered.push(id);
        let spec = id.spec();
        let owner = match row.owner.as_str() {
            "system" => CommandOwner::System,
            "app" => CommandOwner::App,
            later if later.starts_with("later:") => CommandOwner::Planned,
            other => panic!("unknown owner {other}"),
        };
        assert_eq!(spec.owner, owner, "{}", row.command);
        let defaults: Vec<Chord> = spec.defaults.iter().map(|text| chord(text)).collect();
        if row.command.starts_with("prompt_") {
            assert_eq!(defaults, [], "N1: {}", row.command);
            let scroll = if row.command == "prompt_previous" {
                "scroll_page_up"
            } else {
                "scroll_page_down"
            };
            let shared = chord(&row.chord);
            assert_eq!(map.chords(command(scroll)), [shared]);
            assert!(map.is_default(shared), "{}", row.command);
            continue;
        }
        let mut expected: Vec<Chord> = Vec::new();
        if !row.chord.is_empty() {
            expected.push(chord(&row.chord));
        }
        expected.extend(
            row.alt_chords
                .split(';')
                .filter(|text| !text.is_empty())
                .map(chord),
        );
        assert_eq!(defaults, expected, "{}", row.command);
        assert_eq!(map.chords(id), expected, "{}", row.command);
        for &default in &expected {
            let key = match default.key {
                tessera_config::ChordKey::Char(c) => Key::Char(c),
                tessera_config::ChordKey::Up => Key::Up,
                tessera_config::ChordKey::Down => Key::Down,
                tessera_config::ChordKey::Left => Key::Left,
                tessera_config::ChordKey::Right => Key::Right,
                tessera_config::ChordKey::Home => Key::Home,
                tessera_config::ChordKey::End => Key::End,
                tessera_config::ChordKey::Enter => Key::Enter,
                other => panic!("{other:?} in the table"),
            };
            let modifiers = Modifiers::from_bits(default.modifiers.bits());
            assert_eq!(
                command_of(&map, &event(key, modifiers)),
                Some(id),
                "{default}"
            );
        }
        match expected.first() {
            Some(&first) => assert_eq!(
                menu_shortcut(first),
                Some(MenuShortcut {
                    key_equivalent: key_equivalent(&row.key_equivalent),
                    modifiers: mask(&row.mask),
                }),
                "{}",
                row.command
            ),
            None => assert_eq!(row.key_equivalent, "", "{}", row.command),
        }
    }
    for id in CommandId::all() {
        assert!(covered.contains(&id), "{id} is not in the spec table");
    }
    assert_eq!(covered.len(), COMMANDS.len());
}

/// The `[[value]]` rows of the grammar fixture that only the registry
/// can judge: each is left out with one warning naming the key and why.
#[test]
fn registry_value_cases_from_the_grammar_fixture() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tessera-config/tests/fixtures/keys-grammar-cases.toml");
    let text = std::fs::read_to_string(path).unwrap();
    let field = |block: &str, name: &str| {
        block
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name} = \"")))
            .map(|rest| rest.trim_end_matches('"').to_owned())
    };
    let mut seen = 0;
    for block in text.split("[[value]]").skip(1) {
        let Some(reason) = field(block, "registry") else {
            continue;
        };
        let input = field(block, "input").unwrap();
        let map = keymap(&format!("'cmd+ctrl+x' = '{input}'\n"));
        let expected = match reason.as_str() {
            "parameter out of range" => "out of range 1..=9; ignored",
            "missing parameter" => "needs a parameter; ignored",
            "takes no parameter" => "takes no parameter; ignored",
            "unknown command" => "unknown command `nope`; ignored",
            other => panic!("unknown registry reason {other}"),
        };
        match map.warnings() {
            [Warning::KeyBinding { key, reason }] => {
                assert_eq!(key, "cmd+ctrl+x");
                assert!(reason.ends_with(expected), "{input}: {reason}");
            }
            other => panic!("{input}: {other:?}"),
        }
        let x = event(Key::Char('x'), Modifiers::SUPER | Modifiers::CTRL);
        assert_eq!(bound(&map, &x), None, "{input}");
        seen += 1;
    }
    assert_eq!(seen, 5);
}

/// Table cases 5 to 10 of the grammar fixture (1 to 4 are
/// `tessera-config`'s).
#[test]
fn table_cases_over_the_defaults() {
    let cmd = |c| event(Key::Char(c), Modifiers::SUPER);
    let id = |name| command(name);
    // 5: a user chord takes it from its default command, silently.
    let map = keymap("'cmd+t' = 'new_window'\n");
    assert_eq!(command_of(&map, &cmd('t')), Some(id("new_window")));
    assert_eq!(map.chords(id("new_tab")), []);
    assert_eq!(
        map.chords(id("new_window")),
        [chord("cmd+t"), chord("cmd+n")]
    );
    assert_eq!(map.warnings(), []);
    // 6: the other way round; one command, two chords.
    let map = keymap("'cmd+n' = 'new_tab'\n");
    assert_eq!(map.chords(id("new_tab")), [chord("cmd+n"), chord("cmd+t")]);
    assert_eq!(map.chords(id("new_window")), []);
    assert_eq!(map.warnings(), []);
    // 7: `unbound` takes the chord from every command, silently.
    let map = keymap("'cmd+w' = 'unbound'\n");
    assert_eq!(map.chords(id("close_tab")), []);
    assert_eq!(bound(&map, &cmd('w')), None);
    assert_eq!(map.warnings(), []);
    // 8: text on a system command's chord is honoured with a warning.
    let map = keymap("'cmd+q' = 'send_text:x'\n");
    assert_eq!(bound(&map, &cmd('q')), Some("SendText([120])".into()));
    assert_eq!(map.chords(id("quit")), []);
    assert_eq!(
        map.warnings(),
        [Warning::KeyBinding {
            key: "cmd+q".into(),
            reason: "shadows the menu item of `quit`, which loses this chord; honoured".into()
        }]
    );
    // 9: the default written out: no warning, nothing changes.
    let map = keymap("'cmd+t' = 'new_tab'\n");
    assert_eq!(map.chords(id("new_tab")), [chord("cmd+t")]);
    assert_eq!(map.warnings(), []);
    // 10: a user's ⌘↑ is never the prompt rule's.
    let map = keymap("'cmd+up' = 'scroll_page_up'\n");
    assert!(!map.is_default(chord("cmd+up")));
    assert!(Keymap::compile(&Keys::default()).is_default(chord("cmd+up")));
}

/// Text or `none` on an app command's chord, any command on a system
/// command's chord, and the chords macOS uses each warn and are honoured;
/// a command on another app command's chord is a plain override.
#[test]
fn shadowing_and_reserved_chords_warn_and_are_honoured() {
    let map = keymap(
        "'cmd+k' = 'none'\n'cmd+h' = 'new_tab'\n'cmd+c' = 'paste'\n'ctrl+tab' = 'tab_next'\n\
         'cmd+ctrl+f' = 'send_text:f'\n",
    );
    let reasons: Vec<String> = map.warnings().iter().map(ToString::to_string).collect();
    assert_eq!(
        reasons,
        [
            "`keys.\"cmd+k\"`: shadows the menu item of `clear_scrollback`, which loses this chord; honoured",
            "`keys.\"cmd+h\"`: shadows the menu item of `hide`, which loses this chord; honoured",
            "`keys.\"ctrl+tab\"`: macOS uses this chord for Show Next Tab; honoured",
            "`keys.\"cmd+ctrl+f\"`: macOS uses this chord for Enter Full Screen; honoured",
        ]
    );
    let tab = event(Key::Tab, Modifiers::CTRL);
    assert_eq!(command_of(&map, &tab), Some(command("tab_next")));
    assert_eq!(
        command_of(&map, &event(Key::Char('h'), Modifiers::SUPER)),
        Some(command("new_tab"))
    );
    assert_eq!(
        bound(&map, &event(Key::Char('k'), Modifiers::SUPER)),
        Some("Ignore".into())
    );
}

/// A key matches by its key and modifiers; with Shift, by the shifted
/// character without Shift (`cmd+?` on any layout); by the key the US
/// layout has in its place (`cmd+t` on a Cyrillic layout). Lock keys do
/// not matter; Hyper or Meta matches nothing.
#[test]
fn events_match_by_key_shifted_symbol_or_base_layout_key() {
    let map = Keymap::compile(&Keys::default());
    let help = Some(command("help"));
    let shift_cmd = Modifiers::SUPER | Modifiers::SHIFT;
    assert_eq!(command_of(&map, &event(Key::Char('/'), shift_cmd)), help);
    let mut shifted = event(Key::Char('ß'), shift_cmd);
    shifted.shifted_key = Some('?');
    assert_eq!(command_of(&map, &shifted), help);
    // A layout that types `?` without Shift: the menu's `cmd+?`.
    assert_eq!(
        command_of(&map, &event(Key::Char('?'), Modifiers::SUPER)),
        help
    );
    let bigger = Some(command("font_bigger"));
    assert_eq!(
        command_of(&map, &event(Key::Char('='), Modifiers::SUPER)),
        bigger
    );
    let mut plus = event(Key::Char('='), shift_cmd);
    plus.shifted_key = Some('+');
    assert_eq!(command_of(&map, &plus), bigger);
    let mut cyrillic = event(Key::Char('е'), Modifiers::SUPER);
    cyrillic.base_layout_key = Some('t');
    assert_eq!(command_of(&map, &cyrillic), Some(command("new_tab")));
    let caps = Modifiers::SUPER | Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK;
    assert_eq!(
        command_of(&map, &event(Key::Char('t'), caps)),
        Some(command("new_tab"))
    );
    assert_eq!(
        command_of(&map, &event(Key::Char('T'), Modifiers::SUPER)),
        Some(command("new_tab"))
    );
    for extra in [Modifiers::HYPER, Modifiers::META] {
        assert_eq!(
            bound(&map, &event(Key::Char('t'), Modifiers::SUPER | extra)),
            None
        );
    }
    assert_eq!(bound(&map, &event(Key::Char('t'), Modifiers::NONE)), None);
    assert_eq!(
        bound(&map, &event(Key::Kp1, Modifiers::SUPER)),
        None,
        "keypad is its own key"
    );
}

/// The shifted character of a letter is the letter's uppercase, never the
/// chord of the letter without Shift: ⌘⇧T is not ⌘T, ⌃⇧K is not ⌃K, and
/// `A` is not a binding on `a` (#43 round 1, R1). The app fills
/// `shifted_key` for every key.
#[test]
fn a_shifted_letter_is_not_the_chord_of_its_lowercase() {
    let map = Keymap::compile(&Keys::default());
    let shift_cmd = Modifiers::SUPER | Modifiers::SHIFT;
    for letter in ['a', 'f', 'h', 'k', 'm', 'n', 'q', 't', 'v'] {
        let mut chord = event(Key::Char(letter), shift_cmd);
        chord.shifted_key = Some(letter.to_ascii_uppercase());
        assert_eq!(bound(&map, &chord), None, "cmd+shift+{letter}");
    }
    let map = keymap("'ctrl+k' = 'send_text:k'\n'a' = 'send_text:a'\n");
    let mut ctrl_shift_k = event(Key::Char('k'), Modifiers::CTRL | Modifiers::SHIFT);
    ctrl_shift_k.shifted_key = Some('K');
    assert_eq!(bound(&map, &ctrl_shift_k), None, "ctrl+shift+k");
    let mut capital = event(Key::Char('a'), Modifiers::SHIFT);
    capital.shifted_key = Some('A');
    assert_eq!(bound(&map, &capital), None, "shift+a");
    // The symbol rule still holds: ⌘⇧/ types `?`, which is `cmd+?`.
    let mut help = event(Key::Char('/'), shift_cmd);
    help.shifted_key = Some('?');
    assert_eq!(
        command_of(&Keymap::compile(&Keys::default()), &help),
        Some(command("help"))
    );
}

/// The base-layout key stands in only for a key that is not ASCII, as in
/// tessera-term's legacy encoding: ⌘е on a Cyrillic layout is ⌘T, but a
/// Latin layout's own ASCII key is never replaced by the US key in its
/// place (#43 adversarial B1). French ⌘Z is not ⌘W (Close Tab), Dvorak ⌘'
/// is not ⌘Q, German Ctrl-Z is not `ctrl+y`.
#[test]
fn the_base_layout_key_stands_in_only_for_a_non_ascii_key() {
    let map = Keymap::compile(&Keys::default());
    let shift_cmd = Modifiers::SUPER | Modifiers::SHIFT;
    // (layout, typed key, shifted, US key in its place, modifiers)
    let phantoms = [
        ("French", 'z', 'Z', 'w', Modifiers::SUPER),
        ("French", 'z', 'Z', 'w', shift_cmd),
        ("Dvorak", '\'', '"', 'q', Modifiers::SUPER),
        ("Dvorak", 'j', 'J', 'c', Modifiers::SUPER),
        ("Dvorak", 'y', 'Y', 't', Modifiers::SUPER),
        ("Dvorak", 'b', 'B', 'n', Modifiers::SUPER),
        ("Turkish-F", 's', 'S', 'm', Modifiers::SUPER),
        ("Colemak", 'e', 'E', 'k', Modifiers::SUPER),
        ("Swedish", '-', '_', '/', shift_cmd),
    ];
    for (layout, key, shifted, base, modifiers) in phantoms {
        let mut chord = event(Key::Char(key), modifiers);
        chord.shifted_key = Some(shifted);
        chord.base_layout_key = Some(base);
        assert_eq!(bound(&map, &chord), None, "{layout} {modifiers:?} {key}");
    }
    // Russian: the own keys are Cyrillic, so the US key stands in.
    for (key, base, want) in [('е', 't', "new_tab"), ('ц', 'w', "close_tab")] {
        let mut chord = event(Key::Char(key), Modifiers::SUPER);
        chord.shifted_key = Some(key.to_uppercase().next().unwrap());
        chord.base_layout_key = Some(base);
        assert_eq!(
            command_of(&map, &chord),
            Some(command(want)),
            "Russian {key}"
        );
    }
    let mut one = event(Key::Char('1'), Modifiers::SUPER);
    one.base_layout_key = Some('1');
    assert_eq!(command_of(&map, &one), Some(command("select_tab:1")));
    // German: Z is where US has Y, and the user bound `ctrl+y` and `alt+y`.
    let map = keymap("'ctrl+y' = 'send_text:CY'\n'alt+y' = 'send_text:AY'\n");
    for modifiers in [Modifiers::CTRL, Modifiers::ALT] {
        let mut z = event(Key::Char('z'), modifiers);
        z.shifted_key = Some('Z');
        z.base_layout_key = Some('y');
        assert_eq!(bound(&map, &z), None, "German {modifiers:?}-Z");
        let mut y = event(Key::Char('y'), modifiers);
        y.base_layout_key = Some('z');
        assert!(bound(&map, &y).is_some(), "German {modifiers:?}-Y");
    }
}

/// A chord without Command is never a menu item's key equivalent: `AppKit`
/// would match ⌃⇧C to Copy before the terminal saw it (#43 round 1, S2).
#[test]
fn only_command_chords_are_menu_shortcuts() {
    for text in ["ctrl+shift+c", "ctrl+e", "alt+t", "shift+f2", "f5"] {
        let chord = Chord::parse(text).expect(text);
        assert_eq!(menu_shortcut(chord), None, "{text}");
    }
    assert!(menu_shortcut(Chord::parse("cmd+e").expect("cmd+e")).is_some());
}

/// Resolving a key allocates nothing, bound or not.
#[test]
fn resolve_allocates_nothing() {
    let map = keymap("'ctrl+shift+k' = 'send_text:abc'\n'cmd+alt+x' = 'none'\n");
    let mut events = vec![
        event(Key::Char('a'), Modifiers::NONE),
        event(Key::Char('c'), Modifiers::CTRL),
        event(Key::Char('k'), Modifiers::CTRL | Modifiers::SHIFT),
        event(Key::Char('t'), Modifiers::SUPER),
        event(Key::Char('x'), Modifiers::SUPER | Modifiers::ALT),
        event(Key::Up, Modifiers::SUPER),
    ];
    let mut shifted = event(Key::Char('/'), Modifiers::SUPER | Modifiers::SHIFT);
    shifted.shifted_key = Some('?');
    shifted.base_layout_key = Some('/');
    events.push(shifted);
    let before = ALLOCATIONS.with(Cell::get);
    let mut found = 0;
    for event in &events {
        found += usize::from(map.resolve(event).is_some());
    }
    assert_eq!(ALLOCATIONS.with(Cell::get), before);
    assert_eq!(found, 5);
}

/// S5: a table past every cap compiles to at most `MAX_KEY_BINDINGS`
/// entries and as many registry warnings, each bounded, however many
/// entries and however long their names.
#[test]
fn a_flood_of_bad_entries_is_bounded() {
    let mut toml = String::new();
    for i in 0..20_000 {
        let c = char::from_u32(0x4E00 + i).unwrap();
        writeln!(toml, "'cmd+ctrl+{c}' = '{}{i}'", "x".repeat(60)).unwrap();
    }
    let loaded = Config::from_toml_lenient(&format!("[keys]\n{toml}")).unwrap();
    assert_eq!(loaded.config.keys.bindings().len(), MAX_KEY_BINDINGS);
    let map = Keymap::compile(&loaded.config.keys);
    assert_eq!(map.warnings().len(), MAX_KEY_BINDINGS);
    for warning in map.warnings() {
        assert!(warning.to_string().len() < 512, "{warning}");
    }
    // 512 send_text entries of the longest text fit too.
    let long = "\\x41".repeat(tessera_config::MAX_SEND_TEXT_LEN);
    let mut toml = String::new();
    for i in 0..MAX_KEY_BINDINGS {
        let c = char::from_u32(0x4E00 + u32::try_from(i).unwrap()).unwrap();
        writeln!(toml, "'cmd+ctrl+{c}' = 'send_text:{long}'").unwrap();
    }
    let map = keymap(&toml);
    assert_eq!(map.warnings(), []);
    let first = event(Key::Char('\u{4E00}'), Modifiers::SUPER | Modifiers::CTRL);
    assert_eq!(
        bound(&map, &first).map(|text| text.len()),
        Some("SendText([])".len() + tessera_config::MAX_SEND_TEXT_LEN * 4 - 2)
    );
}

/// docs/config.md (from `Config::keys`'s doc comment) names every command
/// of the registry, the planned ones after "for features still to come".
#[test]
fn the_config_reference_lists_every_command() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/config.md");
    let docs = std::fs::read_to_string(path).unwrap();
    let row = docs
        .lines()
        .find(|line| line.starts_with("| `keys` |"))
        .expect("a `keys` row");
    let (current, planned) = row
        .split_once("for features still to come")
        .expect("planned commands listed apart");
    for id in CommandId::all() {
        let spec = id.spec();
        let written = match spec.parameter {
            Some(_) => format!("`{}:", spec.name),
            None => format!("`{}`", spec.name),
        };
        let part = if spec.owner == CommandOwner::Planned {
            planned
        } else {
            current
        };
        assert!(
            part.contains(&written),
            "{written} missing ({:?})",
            spec.owner
        );
    }
}
