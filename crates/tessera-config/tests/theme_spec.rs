//! The spec fixture for issue #41 (`tests/fixtures/theme-spec-cases.toml`)
//! run against the theme format, the bundled themes, the lookup and both
//! importers.
//!
//! Tables with machine-readable expectations are iterated; tables whose
//! `input` is prose are implemented case by case, keyed by `id`, and a
//! check at the end of each such test fails if the fixture gains an id the
//! test does not handle.

#![allow(
    clippy::too_many_lines,
    clippy::format_collect,
    reason = "one block per fixture case; small fixture strings"
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use tessera_config::{
    Color, ConfigPaths, ConfigStore, ImportError, MAX_THEME_FILE_LEN, Theme, ThemeError,
    ThemeSource, Warning, import_base16, import_itermcolors, slug,
};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn spec() -> toml::Table {
    let text = std::fs::read_to_string(fixtures().join("theme-spec-cases.toml")).unwrap();
    toml::from_str(&text).expect("the spec fixture is valid TOML")
}

fn rows<'a>(spec: &'a toml::Table, table: &str) -> Vec<&'a toml::Table> {
    spec[table]
        .as_array()
        .unwrap_or_else(|| panic!("[[{table}]]"))
        .iter()
        .map(|v| v.as_table().unwrap())
        .collect()
}

fn s<'a>(row: &'a toml::Table, key: &str) -> &'a str {
    row[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} in {row:?}"))
}

fn hex(text: &str) -> Color {
    Color::from_hex(text).unwrap_or_else(|| panic!("fixture colour {text}"))
}

/// The 20 slots in fixture order: ansi0..ansi15, foreground, background,
/// cursor, selection.
fn slots(theme: &Theme) -> Vec<(String, Color)> {
    let mut out: Vec<(String, Color)> = theme
        .ansi
        .iter()
        .enumerate()
        .map(|(i, c)| (format!("ansi{i}"), *c))
        .collect();
    out.extend([
        ("foreground".into(), theme.foreground),
        ("background".into(), theme.background),
        ("cursor".into(), theme.cursor),
        ("selection".into(), theme.selection),
    ]);
    out
}

/// `got` equals `want` within `tolerance` per colour channel; alpha exact.
fn assert_close(context: &str, got: Color, want: Color, tolerance: u8) {
    let ok = got.r.abs_diff(want.r) <= tolerance
        && got.g.abs_diff(want.g) <= tolerance
        && got.b.abs_diff(want.b) <= tolerance
        && got.a == want.a;
    assert!(ok, "{context}: got {got}, expected {want} (±{tolerance})");
}

fn assert_expected_table(context: &str, theme: &Theme, expected: &toml::Table, tolerance: u8) {
    let got = slots(theme);
    assert_eq!(expected.len(), 20, "{context}: fixture has 20 slots");
    for (slot, colour) in got {
        let want = hex(s(expected, &slot));
        assert_close(&format!("{context} {slot}"), colour, want, tolerance);
    }
}

fn tolerance(row: &toml::Table) -> u8 {
    u8::try_from(row["tolerance"].as_integer().unwrap()).unwrap()
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(fixtures().join(name)).unwrap()
}

fn assert_ids_handled(spec: &toml::Table, table: &str, handled: &BTreeSet<String>) {
    let ids: BTreeSet<String> = rows(spec, table)
        .iter()
        .map(|row| s(row, "id").to_owned())
        .collect();
    let missing: Vec<_> = ids.difference(handled).collect();
    assert!(
        missing.is_empty(),
        "[[{table}]] ids not handled: {missing:?}"
    );
}

// --- 2. .itermcolors, three real files ------------------------------------

#[test]
fn itermcolors_real_files_match_all_20_slots() {
    let spec = spec();
    for row in rows(&spec, "itermcolors") {
        let file = s(row, "file");
        let theme = import_itermcolors(&read(file), s(row, "name")).unwrap();
        assert_eq!(theme.name, s(row, "name"));
        assert_expected_table(
            file,
            &theme,
            row["expected"].as_table().unwrap(),
            tolerance(row),
        );
    }
}

// --- 3. synthetic colours --------------------------------------------------

fn dracula_plist() -> plist::Dictionary {
    plist::Value::from_reader(std::io::Cursor::new(read("dracula.itermcolors")))
        .unwrap()
        .into_dictionary()
        .unwrap()
}

fn to_xml(dict: plist::Dictionary) -> Vec<u8> {
    let mut out = Vec::new();
    plist::Value::Dictionary(dict)
        .to_writer_xml(&mut out)
        .unwrap();
    out
}

fn real(v: &toml::Value) -> plist::Value {
    plist::Value::Real(
        v.as_float()
            .or_else(|| v.as_integer().map(|i| f64::from(i32::try_from(i).unwrap())))
            .unwrap(),
    )
}

fn slot_of(theme: &Theme, key: &str) -> Color {
    match key {
        "Foreground Color" => theme.foreground,
        "Background Color" => theme.background,
        "Cursor Color" => theme.cursor,
        "Selection Color" => theme.selection,
        other => {
            let i: usize = other
                .strip_prefix("Ansi ")
                .and_then(|r| r.strip_suffix(" Color"))
                .unwrap()
                .parse()
                .unwrap();
            theme.ansi[i]
        }
    }
}

#[test]
fn itermcolors_synthetic_colours() {
    let spec = spec();
    for row in rows(&spec, "itermcolors_colour") {
        let id = s(row, "id");
        let slot = s(row, "slot");
        let mut colour = plist::Dictionary::new();
        colour.insert("Red Component".into(), real(&row["red"]));
        colour.insert("Green Component".into(), real(&row["green"]));
        colour.insert("Blue Component".into(), real(&row["blue"]));
        if row["alpha"].as_str() != Some("<absent>") {
            colour.insert("Alpha Component".into(), real(&row["alpha"]));
        }
        let space = s(row, "color_space");
        if space != "<absent>" {
            colour.insert("Color Space".into(), plist::Value::String(space.into()));
        }
        let mut dict = dracula_plist();
        dict.insert(slot.into(), plist::Value::Dictionary(colour));
        let theme = import_itermcolors(&to_xml(dict), "t").unwrap();
        assert_close(
            id,
            slot_of(&theme, slot),
            hex(s(row, "expect")),
            tolerance(row),
        );
    }
}

// --- 3. structure / error / ok cases ---------------------------------------

fn without(key: &str) -> Vec<u8> {
    let mut dict = dracula_plist();
    assert!(dict.remove(key).is_some(), "{key}");
    to_xml(dict)
}

fn missing_key(result: Result<Theme, ImportError>) -> String {
    match result {
        Err(ImportError::MissingKey(key)) => key,
        other => panic!("expected MissingKey, got {other:?}"),
    }
}

#[test]
fn itermcolors_structure_cases() {
    let spec = spec();
    let dracula = import_itermcolors(&read("dracula.itermcolors"), "d").unwrap();
    let mut handled = BTreeSet::new();
    let mut case = |id: &str| {
        handled.insert(id.to_owned());
    };

    case("missing-ansi-slot");
    assert_eq!(
        missing_key(import_itermcolors(&without("Ansi 7 Color"), "t")),
        "Ansi 7 Color"
    );
    case("missing-foreground");
    assert_eq!(
        missing_key(import_itermcolors(&without("Foreground Color"), "t")),
        "Foreground Color"
    );
    case("missing-background");
    assert_eq!(
        missing_key(import_itermcolors(&without("Background Color"), "t")),
        "Background Color"
    );
    case("missing-cursor-falls-back-to-foreground");
    let theme = import_itermcolors(&without("Cursor Color"), "t").unwrap();
    assert_eq!(theme.cursor, hex("#f8f8f2"));
    case("missing-selection-derived-from-foreground");
    let theme = import_itermcolors(&without("Selection Color"), "t").unwrap();
    assert_eq!(theme.selection, hex("#f8f8f240"));
    assert_eq!(theme.selection.to_string(), "#f8f8f240");

    case("extra-keys-ignored");
    let nord_dict = plist::Value::from_reader(std::io::Cursor::new(read("nord.itermcolors")))
        .unwrap()
        .into_dictionary()
        .unwrap();
    assert!(
        nord_dict
            .keys()
            .any(|k| k.starts_with("Badge") || k.starts_with("Link")),
        "the nord fixture carries extra keys"
    );
    let row = rows(&spec, "itermcolors")
        .into_iter()
        .find(|r| s(r, "file") == "nord.itermcolors")
        .unwrap();
    let nord = import_itermcolors(&read("nord.itermcolors"), "Nord").unwrap();
    assert_expected_table("nord", &nord, row["expected"].as_table().unwrap(), 0);

    case("component-missing");
    let mut dict = dracula_plist();
    dict.get_mut("Ansi 0 Color")
        .unwrap()
        .as_dictionary_mut()
        .unwrap()
        .remove("Green Component");
    let key = missing_key(import_itermcolors(&to_xml(dict), "t"));
    assert!(
        key.contains("Ansi 0 Color") && key.contains("Green Component"),
        "{key}"
    );

    case("component-not-a-number");
    let mut dict = dracula_plist();
    dict.get_mut("Ansi 3 Color")
        .unwrap()
        .as_dictionary_mut()
        .unwrap()
        .insert("Red Component".into(), plist::Value::String("abc".into()));
    match import_itermcolors(&to_xml(dict), "t") {
        Err(ImportError::BadComponent { key }) => assert!(key.contains("Ansi 3 Color"), "{key}"),
        other => panic!("{other:?}"),
    }

    case("integer-component");
    let mut dict = dracula_plist();
    dict.get_mut("Ansi 1 Color")
        .unwrap()
        .as_dictionary_mut()
        .unwrap()
        .insert("Red Component".into(), plist::Value::Integer(1.into()));
    assert_eq!(
        import_itermcolors(&to_xml(dict), "t").unwrap().ansi[1].r,
        255
    );

    case("not-a-plist");
    assert!(matches!(
        import_itermcolors(b"hello", "t"),
        Err(ImportError::Plist(_))
    ));
    case("plist-root-not-dict");
    let array = br#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><array/></plist>"#;
    assert!(matches!(
        import_itermcolors(array, "t"),
        Err(ImportError::NotADictionary | ImportError::Plist(_))
    ));

    case("binary-plist");
    let mut binary = Vec::new();
    plist::Value::Dictionary(dracula_plist())
        .to_writer_binary(&mut binary)
        .unwrap();
    assert!(binary.starts_with(b"bplist00"));
    let mut from_binary = import_itermcolors(&binary, "d").unwrap();
    from_binary.name = dracula.name.clone();
    assert_eq!(from_binary, dracula);

    case("light-dark-variants-ignored");
    let mut dict = dracula_plist();
    let mut odd = plist::Dictionary::new();
    for c in ["Red", "Green", "Blue"] {
        odd.insert(format!("{c} Component"), plist::Value::Real(0.5));
    }
    odd.insert("Color Space".into(), plist::Value::String("sRGB".into()));
    dict.insert(
        "Ansi 0 Color (Light)".into(),
        plist::Value::Dictionary(odd.clone()),
    );
    dict.insert("Ansi 0 Color (Dark)".into(), plist::Value::Dictionary(odd));
    let theme = import_itermcolors(&to_xml(dict.clone()), "t").unwrap();
    assert_eq!(theme.ansi[0], dracula.ansi[0], "plain key wins");
    dict.remove("Ansi 0 Color");
    assert_eq!(
        missing_key(import_itermcolors(&to_xml(dict), "t")),
        "Ansi 0 Color"
    );

    let mut all = BTreeSet::new();
    for table in ["itermcolors_error", "itermcolors_ok"] {
        for row in rows(&spec, table) {
            all.insert(s(row, "id").to_owned());
        }
    }
    let missing: Vec<_> = all.difference(&handled).collect();
    assert!(missing.is_empty(), "ids not handled: {missing:?}");
}

// --- 4. base16 ---------------------------------------------------------------

#[test]
fn base16_real_files_match_all_20_slots() {
    let spec = spec();
    for row in rows(&spec, "base16") {
        let file = s(row, "file");
        let text = String::from_utf8(read(file)).unwrap();
        let theme = import_base16(&text, "fallback").unwrap();
        assert_eq!(theme.name, s(row, "name"), "{file}");
        assert_expected_table(
            file,
            &theme,
            row["expected"].as_table().unwrap(),
            tolerance(row),
        );
    }
}

fn solarized_legacy() -> String {
    String::from_utf8(read("base16-solarized-dark-legacy.yaml")).unwrap()
}

/// The solarized legacy file with the line for `key` replaced by `line`.
fn with_line(key: &str, line: &str) -> String {
    solarized_legacy()
        .lines()
        .map(|l| {
            if l.starts_with(&format!("{key}:")) {
                line.to_owned()
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn unsupported_line(result: Result<Theme, ImportError>) -> usize {
    match result {
        Err(ImportError::Unsupported { line, .. }) => line,
        other => panic!("expected Unsupported, got {other:?}"),
    }
}

fn line_of(text: &str, needle: &str) -> usize {
    text.lines().position(|l| l.contains(needle)).unwrap() + 1
}

#[test]
fn base16_synthetic_cases() {
    let spec = spec();
    let solarized = import_base16(&solarized_legacy(), "x").unwrap();
    let mut handled = BTreeSet::new();
    let mut case = |id: &str| {
        handled.insert(id.to_owned());
    };
    let bg = |text: &str| import_base16(text, "x").unwrap().background;

    case("bare-unquoted-values");
    assert_eq!(bg(&with_line("base00", "base00: 002b36")), hex("#002b36"));

    case("all-digit-hex-stays-string");
    let text = with_line("base00", "base00: 000000");
    let text = text.replace("base01: \"073642\"", "base01: 123456");
    let text = text.replace("base02: \"586e75\"", "base02: 100e10");
    let theme = import_base16(&text, "x").unwrap();
    assert_eq!(theme.background, hex("#000000"));
    assert_eq!(theme.selection, hex("#100e10"), "base02 is the selection");

    case("single-quoted");
    assert_eq!(
        bg(&with_line("base00", "base00: '#002b36'")),
        hex("#002b36")
    );
    case("trailing-comment-after-quoted");
    assert_eq!(
        bg(&with_line(
            "base00",
            "base00: \"#282a36\"  # Default Background"
        )),
        hex("#282a36")
    );
    case("hash-inside-quotes-is-not-a-comment");
    assert_eq!(
        bg(&with_line("base00", "base00: \"#282a36\"")),
        hex("#282a36")
    );

    case("unquoted-hash-value-is-a-comment-in-yaml");
    match import_base16(&with_line("base00", "base00: #282a36"), "x") {
        Err(ImportError::BadHex { key, value }) => {
            assert_eq!((key.as_str(), value.as_str()), ("base00", ""));
        }
        other => panic!("{other:?}"),
    }

    case("crlf-and-bom");
    let crlf = format!("\u{feff}{}", solarized_legacy().replace('\n', "\r\n"));
    assert_eq!(import_base16(&crlf, "x").unwrap(), solarized);

    case("key-order-irrelevant");
    let mut lines: Vec<&str> = Vec::new();
    let legacy = solarized_legacy();
    lines.extend(legacy.lines().rev());
    assert_eq!(import_base16(&lines.join("\n"), "x").unwrap(), solarized);

    case("key-case");
    let text = solarized_legacy().replace("base0A:", "base0a:");
    assert_eq!(missing_key(import_base16(&text, "x")), "base0A");

    case("missing-key");
    let text: Vec<&str> = legacy
        .lines()
        .filter(|l| !l.starts_with("base0C"))
        .collect();
    assert_eq!(missing_key(import_base16(&text.join("\n"), "x")), "base0C");

    case("duplicate-key");
    let text = format!("{legacy}\nbase00: \"000000\"\n");
    assert_eq!(
        unsupported_line(import_base16(&text, "x")),
        text.lines().count()
    );

    case("bad-hex-short");
    match import_base16(&with_line("base00", "base00: \"fff\""), "x") {
        Err(ImportError::BadHex { key, value }) => {
            assert_eq!((key.as_str(), value.as_str()), ("base00", "fff"));
        }
        other => panic!("{other:?}"),
    }
    case("bad-hex-nonhex");
    assert!(matches!(
        import_base16(&with_line("base00", "base00: \"gg0000\""), "x"),
        Err(ImportError::BadHex { .. })
    ));

    case("unsupported-nesting-beyond-palette");
    let nested = String::from_utf8(read("base16-dracula-nested.yaml")).unwrap();
    let text = format!("{nested}\n  extra:\n    nested: 1\n");
    assert_eq!(
        unsupported_line(import_base16(&text, "x")),
        line_of(&text, "nested: 1")
    );

    case("unsupported-list");
    let text = format!("{legacy}\ntags:\n  - a\n  - b\n");
    assert_eq!(
        unsupported_line(import_base16(&text, "x")),
        line_of(&text, "- a")
    );

    case("base24-file");
    let text = nested.replace("system: \"base16\"", "system: \"base24\"");
    let line = unsupported_line(import_base16(&text, "x"));
    assert_eq!(line, line_of(&text, "system:"));
    let Err(error) = import_base16(&text, "x") else {
        unreachable!()
    };
    assert!(error.to_string().contains("base24"), "{error}");

    case("name-fallback");
    let text: Vec<&str> = legacy
        .lines()
        .filter(|l| !l.starts_with("scheme"))
        .collect();
    assert_eq!(
        import_base16(&text.join("\n"), "my-scheme").unwrap().name,
        "my-scheme"
    );

    case("name-precedence");
    let text = format!(
        "name: \"A\"\n{}",
        legacy.replace("scheme: \"Solarized Dark\"", "scheme: \"B\"")
    );
    assert_eq!(import_base16(&text, "x").unwrap().name, "A");

    case("light-scheme-same-mapping");
    let light = "system: \"base16\"\nname: \"Solarized Light\"\nvariant: \"light\"\npalette:\n  \
        base00: \"#fdf6e3\"\n  base01: \"#eee8d5\"\n  base02: \"#93a1a1\"\n  base03: \"#839496\"\n  \
        base04: \"#657b83\"\n  base05: \"#586e75\"\n  base06: \"#073642\"\n  base07: \"#002b36\"\n  \
        base08: \"#dc322f\"\n  base09: \"#cb4b16\"\n  base0A: \"#b58900\"\n  base0B: \"#859900\"\n  \
        base0C: \"#2aa198\"\n  base0D: \"#268bd2\"\n  base0E: \"#6c71c4\"\n  base0F: \"#d33682\"\n";
    let theme = import_base16(light, "x").unwrap();
    assert_eq!(theme.background, hex("#fdf6e3"));
    assert_eq!(theme.ansi[0], hex("#fdf6e3"));
    assert_eq!(theme.foreground, hex("#586e75"));

    assert_ids_handled(&spec, "base16_case", &handled);
}

// --- 5. theme file format ----------------------------------------------------

const BLACKS: &str = "ansi = [\"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \
    \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \"#000000\", \
    \"#000000\", \"#000000\", \"#000000\", \"#000000\"]";

#[test]
fn color_strings() {
    let spec = spec();
    for row in rows(&spec, "color_string") {
        let id = s(row, "id");
        let input = s(row, "input");
        let text =
            format!("name = \"t\"\nbackground = \"#000000\"\n{BLACKS}\nforeground = {input}\n");
        let expect = s(row, "expect");
        let result = Theme::parse(&text);
        if let Some(args) = expect.strip_prefix("Color(") {
            let args = args.strip_suffix(')').unwrap();
            let bytes: Vec<u8> = args
                .split(',')
                .map(|a| {
                    let a = a.trim();
                    a.strip_prefix("0x").map_or_else(
                        || a.parse().unwrap(),
                        |h| u8::from_str_radix(h, 16).unwrap(),
                    )
                })
                .collect();
            let theme = result.unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(
                theme.foreground,
                Color::rgb(bytes[0], bytes[1], bytes[2]).with_alpha(bytes[3]),
                "{id}"
            );
            // Serialised lowercase, alpha digits only when not opaque.
            let shown = theme.foreground.to_string();
            assert_eq!(shown, shown.to_lowercase(), "{id}");
            assert_eq!(shown.len(), if bytes[3] == 0xff { 7 } else { 9 }, "{id}");
        } else {
            assert!(expect.starts_with("ERROR"), "{id}: {expect}");
            let error = result.expect_err(id);
            assert!(
                error.to_string().contains("foreground"),
                "{id}: the error names the key: {error}"
            );
        }
    }
}

fn dracula_text() -> String {
    Theme::bundled("Dracula").unwrap().to_toml()
}

#[test]
fn theme_parse_cases() {
    let spec = spec();
    let dracula = Theme::bundled("Dracula").unwrap().clone();
    let mut handled = BTreeSet::new();
    let mut case = |id: &str| {
        handled.insert(id.to_owned());
    };
    let error_text = |text: &str| Theme::parse(text).unwrap_err().to_string();
    let replace_ansi = |n: usize| {
        let items = vec!["\"#000000\""; n].join(", ");
        dracula_text().split("ansi = [").next().unwrap().to_owned() + &format!("ansi = [{items}]\n")
    };

    case("minimal-valid");
    assert_eq!(Theme::parse(&dracula_text()).unwrap(), dracula);

    case("name-absent");
    let dir = tempfile::tempdir().unwrap();
    let nameless: String = dracula_text()
        .lines()
        .filter(|l| !l.starts_with("name"))
        .map(|l| format!("{l}\n"))
        .collect();
    let path = dir.path().join("my-theme.toml");
    std::fs::write(&path, &nameless).unwrap();
    assert_eq!(Theme::load(&path).unwrap().name, "my-theme");
    assert!(matches!(
        Theme::parse(&nameless),
        Err(ThemeError::MissingKey("name"))
    ));

    case("ansi-15-entries");
    let e = error_text(&replace_ansi(15));
    assert!(e.contains("ansi") && e.contains("15"), "{e}");
    case("ansi-17-entries");
    let e = error_text(&replace_ansi(17));
    assert!(e.contains("ansi") && e.contains("17"), "{e}");
    case("ansi-bad-element");
    let bad = dracula_text().replacen("\"#f1fa8c\"", "\"#zzzzzz\"", 1);
    let e = error_text(&bad);
    assert!(e.contains("ansi[3]"), "{e}");

    case("missing-foreground");
    let text: String = dracula_text()
        .lines()
        .filter(|l| !l.starts_with("foreground"))
        .map(|l| format!("{l}\n"))
        .collect();
    assert!(error_text(&text).contains("foreground"));
    case("missing-cursor");
    let text: String = dracula_text()
        .lines()
        .filter(|l| !l.starts_with("cursor"))
        .map(|l| format!("{l}\n"))
        .collect();
    let theme = Theme::parse(&text).unwrap();
    assert_eq!(theme.cursor, theme.foreground);
    case("missing-selection");
    let text: String = dracula_text()
        .lines()
        .filter(|l| !l.starts_with("selection"))
        .map(|l| format!("{l}\n"))
        .collect();
    let theme = Theme::parse(&text).unwrap();
    assert_eq!(theme.selection, theme.foreground.with_alpha(0x40));

    case("unknown-key-ignored");
    let text = format!("author = \"x\"\n{}\n[extra]\nk = 1\n", dracula_text());
    assert_eq!(Theme::parse(&text).unwrap(), dracula);

    case("appearance-table-not-a-theme");
    assert!(error_text("[appearance]\ntheme = \"x\"\n").contains("foreground"));

    case("toml-syntax-error");
    match Theme::parse("foreground = ") {
        Err(ThemeError::Load(tessera_config::Error::Parse(e))) => {
            assert!(e.span().is_some(), "{e}");
        }
        other => panic!("{other:?}"),
    }

    case("roundtrip");
    for name in Theme::bundled_names() {
        let theme = Theme::bundled(name).unwrap();
        assert_eq!(&Theme::parse(&theme.to_toml()).unwrap(), theme, "{name}");
    }
    for (file, is_iterm) in [
        ("dracula.itermcolors", true),
        ("nord.itermcolors", true),
        ("solarized-dark-patched.itermcolors", true),
        ("base16-nord-legacy.yaml", false),
        ("base16-dracula-nested.yaml", false),
        ("base16-solarized-dark-legacy.yaml", false),
    ] {
        let theme = if is_iterm {
            import_itermcolors(&read(file), "Name \"quoted\" \\ back")
        } else {
            import_base16(&String::from_utf8(read(file)).unwrap(), "x")
        }
        .unwrap();
        assert_eq!(Theme::parse(&theme.to_toml()).unwrap(), theme, "{file}");
    }

    case("huge-file");
    let path = dir.path().join("huge.toml");
    let padding = "#".repeat(usize::try_from(MAX_THEME_FILE_LEN).unwrap());
    std::fs::write(&path, format!("{}\n{padding}\n", dracula_text())).unwrap();
    assert!(matches!(
        Theme::load(&path),
        Err(ThemeError::Load(tessera_config::Error::Io(_)))
    ));

    assert_ids_handled(&spec, "theme_parse", &handled);
}

// --- 6. lookup ---------------------------------------------------------------

fn write_theme(dir: &Path, file: &str, name: Option<&str>, background: &str) {
    let mut theme = Theme::bundled("Dracula").unwrap().clone();
    theme.background = hex(background);
    let text: String = theme
        .to_toml()
        .lines()
        .filter(|l| !l.starts_with("name"))
        .map(|l| format!("{l}\n"))
        .collect();
    let text = match name {
        Some(name) => format!("name = \"{name}\"\n{text}"),
        None => text,
    };
    std::fs::write(dir.join(file), text).unwrap();
}

fn not_found(warnings: &[Warning]) -> bool {
    warnings
        .iter()
        .any(|w| matches!(w, Warning::ThemeNotFound { .. }))
}

#[test]
fn lookup_cases() {
    let spec = spec();
    let mut handled = BTreeSet::new();
    let mut case = |id: &str| {
        handled.insert(id.to_owned());
    };
    let empty = tempfile::tempdir().unwrap();
    let bundled = |name: &str| {
        let r = Theme::resolve(empty.path(), name);
        (r.theme.name.clone(), r.source.clone(), r.warnings.clone())
    };

    case("exact-bundled");
    assert_eq!(
        bundled("Dracula"),
        ("Dracula".into(), ThemeSource::Bundled, vec![])
    );
    case("case-insensitive-bundled");
    assert_eq!(bundled("dRaCuLa").0, "Dracula");
    assert_eq!(bundled("dRaCuLa").2, [] as [tessera_config::Warning; 0]);
    case("bundled-with-spaces");
    assert_eq!(bundled("Solarized Dark").0, "Solarized Dark");
    case("slug-form-matches-bundled");
    assert_eq!(bundled("solarized-dark").0, "Solarized Dark");
    case("default-name");
    assert_eq!(
        bundled("Tessera Dark"),
        ("Tessera Dark".into(), ThemeSource::Bundled, vec![])
    );
    case("unknown");
    let (name, source, warnings) = bundled("Nope");
    assert_eq!(
        (name.as_str(), source),
        ("Tessera Dark", ThemeSource::Default)
    );
    assert_eq!(
        warnings,
        vec![Warning::ThemeNotFound {
            name: "Nope".into()
        }]
    );
    case("empty-string");
    let (name, _, warnings) = bundled("");
    assert_eq!(name, "Tessera Dark");
    assert!(not_found(&warnings));

    let dir = tempfile::tempdir().unwrap();
    write_theme(dir.path(), "mine.toml", Some("Pretty"), "#010203");
    case("user-stem-match");
    let r = Theme::resolve(dir.path(), "mine");
    assert_eq!(r.theme.background, hex("#010203"));
    assert_eq!(r.source, ThemeSource::User(dir.path().join("mine.toml")));
    assert_eq!(r.theme.name, "Pretty");
    case("user-name-key-match");
    assert_eq!(
        Theme::resolve(dir.path(), "pretty").theme.background,
        hex("#010203")
    );

    case("user-shadows-bundled");
    write_theme(dir.path(), "dracula.toml", None, "#040506");
    let r = Theme::resolve(dir.path(), "Dracula");
    assert_eq!(r.theme.background, hex("#040506"));
    assert_eq!(r.theme.name, "dracula", "named after its stem");
    case("user-shadows-by-name-key");
    write_theme(dir.path(), "x.toml", Some("Nord"), "#070809");
    assert_eq!(
        Theme::resolve(dir.path(), "nord").theme.background,
        hex("#070809")
    );

    case("user-invalid-falls-through");
    let broken = tempfile::tempdir().unwrap();
    std::fs::write(broken.path().join("dracula.toml"), "foreground = ").unwrap();
    let r = Theme::resolve(broken.path(), "Dracula");
    assert_eq!(&r.theme, Theme::bundled("Dracula").unwrap());
    assert_eq!(r.source, ThemeSource::Bundled);
    assert!(
        matches!(&r.warnings[..], [Warning::ThemeInvalid { path, .. }] if path.ends_with("dracula.toml")),
        "{:?}",
        r.warnings
    );

    case("non-toml-file-ignored");
    let other = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("dracula.itermcolors"),
        other.path().join("dracula.itermcolors"),
    )
    .unwrap();
    assert_eq!(
        Theme::resolve(other.path(), "Dracula").source,
        ThemeSource::Bundled
    );
    case("hidden-file-ignored");
    write_theme(other.path(), ".dracula.toml", Some("Dracula"), "#0a0b0c");
    write_theme(other.path(), "dracula.toml~", Some("Dracula"), "#0a0b0c");
    write_theme(other.path(), ".#dracula.toml", Some("Dracula"), "#0a0b0c");
    assert_eq!(
        Theme::resolve(other.path(), "Dracula").source,
        ThemeSource::Bundled
    );

    case("path-escape");
    let nested = tempfile::tempdir().unwrap();
    let themes = nested.path().join("themes");
    std::fs::create_dir(&themes).unwrap();
    write_theme(nested.path(), "config.toml", Some("Escaped"), "#0d0e0f");
    // Names are compared, never joined onto a path: nothing outside the
    // themes directory is found.
    for name in ["../config", "/etc/passwd", "a/b", "..\\config", "escaped"] {
        let r = Theme::resolve(&themes, name);
        assert_eq!(r.source, ThemeSource::Default, "{name}");
        assert!(not_found(&r.warnings), "{name}");
    }
    // A path-like name is still a name: its slug can match.
    assert_eq!(
        Theme::resolve(&themes, "../dracula").source,
        ThemeSource::Bundled
    );
    write_theme(&themes, "a-b.toml", None, "#0d0e0f");
    assert_eq!(
        Theme::resolve(&themes, "a/b").source,
        ThemeSource::User(themes.join("a-b.toml"))
    );

    case("themes-dir-missing");
    let gone = dir.path().join("does-not-exist");
    assert_eq!(Theme::resolve(&gone, "Nord").source, ThemeSource::Bundled);
    assert_eq!(Theme::resolve(&gone, "Nope").source, ThemeSource::Default);

    case("two-user-files-same-name");
    let two = tempfile::tempdir().unwrap();
    write_theme(two.path(), "b.toml", Some("X"), "#0000bb");
    write_theme(two.path(), "a.toml", Some("X"), "#0000aa");
    assert_eq!(
        Theme::resolve(two.path(), "x").theme.background,
        hex("#0000aa")
    );

    case("live-retheme");
    // The fixture says the watcher already watches `themes/`; it does not
    // (watch.rs follows `config.toml` only, and its semantics are out of
    // scope here). What holds: `ConfigStore::theme` reads the file anew
    // on every call, so the next `Changed` event (or any later call) sees
    // an edited theme file.
    let root = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::new(root.path());
    let store = ConfigStore::open(paths.clone()).unwrap();
    std::fs::write(paths.config_file(), "[appearance]\ntheme = \"mine\"\n").unwrap();
    store.reload().unwrap();
    write_theme(&paths.themes_dir(), "mine.toml", None, "#111111");
    assert_eq!(store.theme().background, hex("#111111"));
    write_theme(&paths.themes_dir(), "mine.toml", None, "#222222");
    assert_eq!(store.theme().background, hex("#222222"));

    case("bundled_names-stable");
    let names = rows(&spec, "lookup")
        .into_iter()
        .find(|r| s(r, "id") == "bundled_names-stable")
        .map(|r| s(r, "expect").to_owned())
        .unwrap();
    let listed = names.split('[').nth(1).unwrap().split(']').next().unwrap();
    let listed: Vec<&str> = listed.split(", ").map(|n| n.trim_matches('"')).collect();
    assert_eq!(Theme::bundled_names(), &listed[..]);
    case("bundled-lookup-unknown");
    assert!(Theme::bundled("nope").is_none());
    case("bundled-lookup-ci");
    assert_eq!(Theme::bundled("NORD").unwrap().name, "Nord");

    assert_ids_handled(&spec, "lookup", &handled);
}

#[test]
fn store_reports_theme_warnings_through_last_warnings() {
    let root = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::new(root.path());
    let store = ConfigStore::open(paths.clone()).unwrap();
    assert_eq!(store.theme().name, "Tessera Dark");
    assert_eq!(store.last_warnings(), [] as [tessera_config::Warning; 0]);

    std::fs::write(
        paths.config_file(),
        "[appearance]\ntheme = \"Nope\"\nbogus = 1\n",
    )
    .unwrap();
    store.reload().unwrap();
    assert_eq!(store.theme(), Theme::default());
    assert_eq!(
        store.last_warnings(),
        vec![
            Warning::UnknownKey {
                path: "appearance.bogus".into()
            },
            Warning::ThemeNotFound {
                name: "Nope".into()
            },
        ]
    );
    assert_eq!(
        store.warnings().len(),
        1,
        "warnings() is the config load's only"
    );

    std::fs::write(paths.themes_dir().join("nope.toml"), "foreground = 1").unwrap();
    let theme = store.theme();
    assert_eq!(theme, Theme::default());
    let warnings = store.last_warnings();
    assert!(
        matches!(
            &warnings[1..],
            [Warning::ThemeInvalid { .. }, Warning::ThemeNotFound { .. }]
        ),
        "{warnings:?}"
    );
    let shown = warnings[1].to_string();
    assert!(
        shown.contains("nope.toml") && shown.contains("foreground"),
        "{shown}"
    );

    std::fs::write(paths.config_file(), "[appearance]\ntheme = \"one dark\"\n").unwrap();
    store.reload().unwrap();
    assert_eq!(store.theme().name, "One Dark");
    assert!(
        store.last_warnings().is_empty(),
        "the next lookup replaces them"
    );
}

// --- 7. bundled themes ---------------------------------------------------------

#[test]
fn bundled_themes_match_the_fixture() {
    let spec = spec();
    let rows = rows(&spec, "bundled");
    assert_eq!(rows.len(), Theme::bundled_names().len());
    for (row, name) in rows.iter().zip(Theme::bundled_names()) {
        assert_eq!(s(row, "name"), *name, "order");
        let theme = Theme::bundled(name).unwrap();
        assert_eq!(theme.name, *name);
        assert_eq!(theme.foreground, hex(s(row, "foreground")), "{name}");
        assert_eq!(theme.background, hex(s(row, "background")), "{name}");
        assert_eq!(theme.cursor, hex(s(row, "cursor")), "{name}");
        assert_eq!(theme.selection, hex(s(row, "selection")), "{name}");
        let ansi: Vec<Color> = row["ansi"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| hex(v.as_str().unwrap()))
            .collect();
        assert_eq!(theme.ansi.to_vec(), ansi, "{name}");
        // The file the fixture names is the one compiled in, and it starts
        // with its attribution header.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(s(row, "file"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(&Theme::parse(&text).unwrap(), theme, "{}", path.display());
        assert!(
            text.lines().take(3).any(|l| l.starts_with("# Licence:")),
            "{name} names its licence"
        );
    }
    // Names are unique case-insensitively and by slug.
    let slugs: BTreeSet<_> = Theme::bundled_names()
        .iter()
        .map(|n| slug(n).unwrap())
        .collect();
    assert_eq!(slugs.len(), Theme::bundled_names().len());
    assert_eq!(Theme::default().name, Theme::DEFAULT_NAME);
}

#[test]
fn bundled_dracula_equals_the_imported_itermcolors() {
    let mut imported = import_itermcolors(&read("dracula.itermcolors"), "Dracula").unwrap();
    imported.name = "Dracula".into();
    assert_eq!(&imported, Theme::bundled("Dracula").unwrap());
}

#[test]
fn bundled_solarized_is_close_to_the_calibrated_import() {
    let imported = import_itermcolors(&read("solarized-dark-patched.itermcolors"), "x").unwrap();
    let bundled = Theme::bundled("Solarized Dark").unwrap();
    for i in [1, 2, 3, 4, 5, 6, 9] {
        assert_close(&format!("ansi{i}"), imported.ansi[i], bundled.ansi[i], 2);
    }
}

#[test]
fn tessera_light_meets_wcag_aa() {
    fn luminance(c: Color) -> f64 {
        let lin = |b: u8| {
            let v = f64::from(b) / 255.0;
            if v <= 0.040_45 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
    }
    let light = Theme::bundled("Tessera Light").unwrap();
    let bg = luminance(light.background);
    // Every text colour except ANSI 7 and 15 (white and bright white are
    // light by definition; the same holds in Solarized Light).
    for (what, c) in [("foreground", light.foreground), ("cursor", light.cursor)]
        .into_iter()
        .chain((0..=6).chain(8..=14).map(|i| ("ansi", light.ansi[i])))
    {
        let fg = luminance(c);
        let ratio = (fg.max(bg) + 0.05) / (fg.min(bg) + 0.05);
        assert!(ratio >= 4.5, "{what} {c}: {ratio:.2}");
    }
}

// --- 8. slugs ------------------------------------------------------------------

#[test]
fn slugs() {
    // Lead decision (adversarial cycle 1): letters of any script are kept,
    // lowercased, instead of dropped, so these fixture rows change.
    let overrides = [("日本語", Some("日本語")), ("ÅÄÖ test", Some("åäö-test"))];
    let spec = spec();
    for row in rows(&spec, "slug") {
        let input = s(row, "input");
        let expect = s(row, "expect");
        if let Some((_, want)) = overrides.iter().find(|(i, _)| *i == input) {
            assert_eq!(slug(input).as_deref(), *want, "{input:?}");
        } else if expect.starts_with("ERROR") {
            assert_eq!(slug(input), None, "{input:?}");
        } else {
            let want = expect.split(" (").next().unwrap();
            assert_eq!(slug(input).as_deref(), Some(want), "{input:?}");
        }
    }
    for (input, want) in [
        ("Rosé Pine", Some("rosé-pine")),
        ("Dracula+", Some("dracula-plus")),
        ("C++ Dark", Some("c-plus-plus-dark")),
        ("+", Some("plus")),
        ("İstanbul", Some("istanbul")),
        ("ΣΊΣΥΦΟΣ", Some("σίσυφοσ")),
        // NFD: the combining accent is not a letter, so it separates.
        ("Rose\u{301} Pine", Some("rose-pine")),
        ("\u{301}", None),
    ] {
        assert_eq!(slug(input).as_deref(), want, "{input:?}");
    }
}

/// NFC and NFD spellings of one name are different names (no Unicode
/// normalisation, a documented limit); each matches its own spelling.
#[test]
fn unicode_names_match_their_own_spelling() {
    let dir = tempfile::tempdir().unwrap();
    write_theme(dir.path(), "rosé-pine.toml", Some("Rosé Pine"), "#191724");
    let nfc = Theme::resolve(dir.path(), "Rosé Pine");
    assert_eq!(nfc.theme.background, hex("#191724"));
    assert_eq!(
        Theme::resolve(dir.path(), "ROSÉ PINE").theme.background,
        hex("#191724")
    );
    let nfd = Theme::resolve(dir.path(), "Rose\u{301} Pine");
    assert_eq!(nfd.source, ThemeSource::Default);
}

/// `Dracula+` is its own theme: it neither shadows nor is shadowed by the
/// bundled Dracula.
#[test]
fn plus_is_part_of_the_name() {
    let dir = tempfile::tempdir().unwrap();
    write_theme(dir.path(), "dracula-plus.toml", Some("Dracula+"), "#212121");
    assert_eq!(
        Theme::resolve(dir.path(), "Dracula").source,
        ThemeSource::Bundled
    );
    let plus = Theme::resolve(dir.path(), "Dracula+");
    assert_eq!(plus.theme.background, hex("#212121"));
    assert_eq!(
        Theme::resolve(dir.path(), "dracula-plus").theme.background,
        hex("#212121")
    );
}

/// A name longer than any theme's is not looked up at all, so a huge
/// `appearance.theme` costs nothing per lookup.
#[test]
fn huge_lookup_names_are_not_compared() {
    use std::time::{Duration, Instant};

    use tessera_config::MAX_THEME_NAME_LEN;

    let dir = tempfile::tempdir().unwrap();
    for i in 0..50 {
        write_theme(dir.path(), &format!("t{i}.toml"), Some("x"), "#000000");
    }
    let huge = "a".repeat(16 << 20);
    let start = Instant::now();
    let resolved = Theme::resolve(dir.path(), &huge);
    let took = start.elapsed();
    assert_eq!(resolved.source, ThemeSource::Default);
    let [Warning::ThemeNotFound { name }] = &resolved.warnings[..] else {
        panic!("{:?}", resolved.warnings.len());
    };
    assert!(name.len() <= tessera_config::MAX_WARNING_PATH_LEN);
    assert!(took < Duration::from_secs(1), "{took:?}");
    // The longest name still looked up.
    let longest = "b".repeat(MAX_THEME_NAME_LEN);
    write_theme(dir.path(), "long.toml", Some(&longest), "#0000ff");
    assert_eq!(
        Theme::resolve(dir.path(), &longest).theme.background,
        hex("#0000ff")
    );
    let too_long = "b".repeat(MAX_THEME_NAME_LEN + 1);
    assert_eq!(
        Theme::resolve(dir.path(), &too_long).source,
        ThemeSource::Default
    );

    // A hand-written theme whose `name` is longer than the cap cannot be
    // selected by that full name, but by its file stem, or by any name
    // within the cap that has the same slug (the slug itself, for one).
    let hand = tempfile::tempdir().unwrap();
    let long_name = format!("Very {}", "Long ".repeat(80));
    assert!(long_name.len() > MAX_THEME_NAME_LEN);
    write_theme(hand.path(), "mine.toml", Some(long_name.trim()), "#00ff00");
    let by = |name: &str| Theme::resolve(hand.path(), name).source;
    assert_eq!(by(long_name.trim()), ThemeSource::Default);
    let user = ThemeSource::User(hand.path().join("mine.toml"));
    assert_eq!(by("mine"), user);
    let long_slug = slug(&long_name).unwrap();
    assert!(long_slug.len() <= 64);
    assert_eq!(by(&long_slug), user);
    assert_eq!(
        by(&long_name[..MAX_THEME_NAME_LEN]),
        user,
        "a prefix within the cap"
    );
}

// --- snapshots -----------------------------------------------------------------

#[test]
fn bundled_names_snapshot() {
    insta::assert_debug_snapshot!(Theme::bundled_names());
}

#[test]
fn imported_toml_snapshots() {
    for file in [
        "dracula.itermcolors",
        "nord.itermcolors",
        "solarized-dark-patched.itermcolors",
    ] {
        let stem = file.strip_suffix(".itermcolors").unwrap();
        let theme = import_itermcolors(&read(file), stem).unwrap();
        insta::assert_snapshot!(format!("itermcolors_{stem}"), theme.to_toml());
    }
    for file in [
        "base16-solarized-dark-legacy.yaml",
        "base16-nord-legacy.yaml",
        "base16-dracula-nested.yaml",
    ] {
        let stem = file.strip_suffix(".yaml").unwrap();
        let theme = import_base16(&String::from_utf8(read(file)).unwrap(), stem).unwrap();
        insta::assert_snapshot!(stem.replace('-', "_"), theme.to_toml());
    }
}

/// Extra colours beyond the fixture, ground truth from `AppKit` on macOS 26
/// (`NSColor(calibratedRed:…)` / `NSColor(displayP3Red:…)` converted with
/// `usingColorSpace(.sRGB)`). Unlike the fixture's P3 rows, these have
/// unsaturated channels next to saturated ones, so every matrix entry
/// moves the result by more than the tolerance.
#[test]
fn colour_spaces_match_appkit_on_mixed_colours() {
    let cases = [
        ([0.5, 1.0, 0.5], "#8dfb92", "#36ff6d"),
        ([0.2, 0.8, 0.4], "#38d279", "#00d058"),
        ([0.9, 0.6, 0.1], "#eca91f", "#f39500"),
        ([0.3, 0.7, 0.2], "#5bbc42", "#00b500"),
        ([0.1, 0.2, 0.9], "#2250eb", "#0f34ef"),
    ];
    for ([r, g, b], calibrated, p3) in cases {
        for (space, want) in [("Calibrated", calibrated), ("P3", p3)] {
            let mut colour = plist::Dictionary::new();
            colour.insert("Red Component".into(), plist::Value::Real(r));
            colour.insert("Green Component".into(), plist::Value::Real(g));
            colour.insert("Blue Component".into(), plist::Value::Real(b));
            colour.insert("Color Space".into(), plist::Value::String(space.into()));
            let mut dict = dracula_plist();
            dict.insert("Ansi 2 Color".into(), plist::Value::Dictionary(colour));
            let theme = import_itermcolors(&to_xml(dict), "t").unwrap();
            assert_close(&format!("{space} {r} {g} {b}"), theme.ansi[2], hex(want), 1);
        }
    }
}

/// Out-of-range components are clamped before the colour-space conversion
/// (rule R7), in every colour space: 1.2 is 1.0 and -0.5 is 0.0.
#[test]
fn components_clamp_before_conversion() {
    let colour = |r: f64, g: f64, space: &str| {
        let mut colour = plist::Dictionary::new();
        colour.insert("Red Component".into(), plist::Value::Real(r));
        colour.insert("Green Component".into(), plist::Value::Real(g));
        colour.insert("Blue Component".into(), plist::Value::Real(0.0));
        colour.insert("Color Space".into(), plist::Value::String(space.into()));
        let mut dict = dracula_plist();
        dict.insert("Ansi 1 Color".into(), plist::Value::Dictionary(colour));
        import_itermcolors(&to_xml(dict), "t").unwrap().ansi[1]
    };
    for space in ["sRGB", "Calibrated", "P3"] {
        assert_eq!(colour(1.2, 0.0, space), colour(1.0, 0.0, space), "{space}");
        assert_eq!(colour(1.0, -0.5, space), colour(1.0, 0.0, space), "{space}");
    }
    assert_eq!(colour(1.2, 0.0, "Calibrated"), hex("#ff2600"));
}

/// Block scalars (`description: |` in tinted-theming's ascendancy.yaml, the
/// one published scheme that has one) are skipped for keys the importer
/// ignores and refused for keys it reads.
#[test]
fn base16_block_scalars() {
    let text = String::from_utf8(read("base16-ascendancy-block-scalar.yaml")).unwrap();
    let theme = import_base16(&text, "x").unwrap();
    assert_eq!(theme.name, "Ascendancy");
    assert_eq!(theme.background, hex("#282828"));
    assert_eq!(theme.ansi[1], hex("#d05000"));
    assert_eq!(theme.ansi[15], hex("#fbf1c7"));

    let legacy = solarized_legacy();
    for header in ["|", ">", "|-", ">+", "|2", "|-2", ">2+"] {
        // Content with colons, quotes, hashes, list markers and blank lines,
        // ending at the next key.
        let text =
            format!("description: {header}\n  a: b 'c \"d # e\n\n  - f\n    g: [h\n{legacy}");
        assert_eq!(
            import_base16(&text, "x").unwrap(),
            import_base16(&legacy, "x").unwrap(),
            "{header}"
        );
        // At the end of the file, and inside `palette:`.
        let nested = String::from_utf8(read("base16-dracula-nested.yaml")).unwrap();
        let tail = format!("{nested}\n  notes: {header}\n    one\n    two: 3\n");
        assert_eq!(
            import_base16(&tail, "x").unwrap().name,
            "Dracula",
            "{header}"
        );
    }
    // Keys the importer reads may not be block scalars.
    for key in ["name", "scheme", "base00", "system"] {
        let text = format!("{key}: |\n  value\n{legacy}");
        assert!(
            matches!(
                import_base16(&text, "x"),
                Err(ImportError::Unsupported { line: 1, .. })
            ),
            "{key}"
        );
    }
    // Not block headers: these stay errors.
    for value in ["|x", ">>", "|++", "|12"] {
        let text = format!("description: {value}\n{legacy}");
        assert!(import_base16(&text, "x").is_err(), "{value}");
    }
}
