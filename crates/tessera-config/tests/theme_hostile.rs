//! Hostile and arbitrary input for the theme parser and both importers:
//! nothing panics, overflows the stack or allocates without bound, and the
//! fast paths agree with their definitions.

use std::time::{Duration, Instant};

use proptest::prelude::*;
use tessera_config::{
    Color, ImportError, MAX_IMPORT_LEN, Theme, import_base16, import_itermcolors, slug,
};

/// Asserts `f` finishes within `limit`; hostile inputs must stay linear.
fn within<T>(limit: Duration, what: &str, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let out = f();
    let took = start.elapsed();
    assert!(took < limit, "{what} took {took:?}");
    out
}

#[test]
fn deeply_nested_xml_plist_does_not_overflow_the_stack() {
    let depth = 100_000;
    let mut xml = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0">"#);
    xml.push_str(&"<array>".repeat(depth));
    xml.push_str(&"</array>".repeat(depth));
    xml.push_str("</plist>");
    let bytes = xml.into_bytes();
    assert!(bytes.len() < MAX_IMPORT_LEN * 2);
    // Larger than the cap: refused before parsing.
    let refused = import_itermcolors(&bytes, "t");
    assert!(
        matches!(refused, Err(ImportError::TooLarge(_))),
        "{refused:?}"
    );
    // Within the cap: still deep enough to blow a naive recursive parser.
    let depth = 60_000;
    let mut xml = String::from("<plist version=\"1.0\">");
    xml.push_str(&"<array>".repeat(depth));
    xml.push_str(&"</array>".repeat(depth));
    xml.push_str("</plist>");
    assert!(xml.len() <= MAX_IMPORT_LEN);
    let result = within(Duration::from_secs(10), "deep plist", || {
        // Run on a thread with the default 2 MiB stack, as the devtool and
        // any frontend thread would.
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || import_itermcolors(xml.as_bytes(), "t").map(|_| ()))
            .unwrap()
            .join()
    });
    let result = result.expect("no stack overflow or panic");
    assert!(result.is_err());
}

#[test]
fn xml_entities_are_not_expanded() {
    let laughs = r#"<?xml version="1.0"?>
<!DOCTYPE plist [
 <!ENTITY a "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa">
 <!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">
 <!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;">
 <!ENTITY d "&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;">
 <!ENTITY e "&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;">
]>
<plist version="1.0"><dict><key>&e;</key><string>&e;</string></dict></plist>"#;
    let result = within(Duration::from_secs(10), "entity expansion", || {
        import_itermcolors(laughs.as_bytes(), "t")
    });
    assert!(result.is_err(), "{result:?}");
}

#[test]
fn binary_plist_with_huge_counts_is_rejected_quickly() {
    // A bplist00 whose single object claims 2^32 array elements and whose
    // offset table points at itself.
    let mut b = b"bplist00".to_vec();
    b.extend_from_slice(&[0xaf, 0x12, 0xff, 0xff, 0xff, 0xff]); // array, count 2^32-1
    let offset_table = b.len();
    b.push(8); // object 0 at offset 8
    let mut trailer = [0u8; 32];
    trailer[6] = 1; // offset int size
    trailer[7] = 1; // object ref size
    trailer[8..16].copy_from_slice(&1u64.to_be_bytes()); // num objects
    trailer[16..24].copy_from_slice(&0u64.to_be_bytes()); // top object
    trailer[24..32].copy_from_slice(&(offset_table as u64).to_be_bytes());
    b.extend_from_slice(&trailer);
    let result = within(Duration::from_secs(10), "huge bplist", || {
        import_itermcolors(&b, "t")
    });
    assert!(result.is_err(), "{result:?}");
}

#[test]
fn huge_base16_input_is_refused_and_long_lines_stay_linear() {
    let big = "a".repeat(MAX_IMPORT_LEN + 1);
    assert!(matches!(
        import_base16(&big, "t"),
        Err(ImportError::TooLarge(_))
    ));
    // One huge line of quotes and hashes within the cap.
    let line = format!("base00: \"{}\"", "'#\\\" ".repeat(MAX_IMPORT_LEN / 8));
    let result = within(Duration::from_secs(10), "long line", || {
        import_base16(&line, "t")
    });
    let error = result.unwrap_err().to_string();
    assert!(
        error.len() < 512,
        "error messages stay bounded: {}",
        error.len()
    );
    // Many lines: the line count is bounded by the cap, so is the work.
    let many = "x: y\n".repeat(MAX_IMPORT_LEN / 5);
    let result = within(Duration::from_secs(10), "many lines", || {
        import_base16(&many, "t")
    });
    assert!(matches!(
        result,
        Err(ImportError::Unsupported { .. } | ImportError::MissingKey(_))
    ));
}

#[test]
fn hostile_theme_files_are_errors_with_bounded_messages() {
    for text in [
        format!("foreground = \"{}\"", "#".repeat(60_000)),
        format!("ansi = [{}]", "\"#000000\",".repeat(5_000)),
        "[a]\nforeground = 1".to_owned(),
        "ansi = \"#000000\"".to_owned(),
        "name = 3\nforeground=\"#000000\"\nbackground=\"#000000\"\nansi=[]".to_owned(),
    ] {
        let error = Theme::parse(&text).unwrap_err().to_string();
        assert!(error.len() < 4096, "{}", error.len());
    }
}

fn arb_color() -> impl Strategy<Value = Color> {
    any::<[u8; 4]>().prop_map(|[r, g, b, a]| Color { r, g, b, a })
}

fn arb_theme() -> impl Strategy<Value = Theme> {
    (
        "\\PC{1,40}",
        arb_color(),
        arb_color(),
        arb_color(),
        arb_color(),
        proptest::array::uniform16(arb_color()),
    )
        .prop_filter("names are not blank", |(n, ..)| !n.trim().is_empty())
        .prop_map(
            |(name, foreground, background, cursor, selection, ansi)| Theme {
                name,
                foreground,
                background,
                cursor,
                selection,
                ansi,
            },
        )
}

proptest! {
    #[test]
    fn color_hex_round_trips(c in arb_color()) {
        let text = c.to_string();
        prop_assert_eq!(Color::from_hex(&text), Some(c));
        prop_assert_eq!(Color::from_hex(&text.to_uppercase()), Some(c));
        prop_assert_eq!(text.len(), if c.a == 0xff { 7 } else { 9 });
    }

    /// The fast byte parser agrees with a reference built on `from_str_radix`.
    #[test]
    fn color_parser_agrees_with_the_reference(text in "#?[0-9a-fA-FgG ]{0,10}|\\PC{0,10}") {
        let reference = text.strip_prefix('#').and_then(|hex| {
            if !(hex.len() == 6 || hex.len() == 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let v = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
            Some(Color { r: v(0)?, g: v(2)?, b: v(4)?, a: if hex.len() == 8 { v(6)? } else { 0xff } })
        });
        prop_assert_eq!(Color::from_hex(&text), reference);
    }

    #[test]
    fn theme_toml_round_trips(theme in arb_theme()) {
        prop_assert_eq!(Theme::parse(&theme.to_toml()).unwrap(), theme);
    }

    #[test]
    fn theme_parse_never_panics(text in "\\PC{0,200}") {
        let _ = Theme::parse(&text);
    }

    #[test]
    fn base16_never_panics(lines in proptest::collection::vec(
        prop_oneof![
            Just("palette:".to_owned()),
            Just("  base00: \"#282a36\"".to_owned()),
            Just("system: \"base16\"".to_owned()),
            "[ \t]{0,3}[a-z0-9#'\":\\- ]{0,20}",
            "\\PC{0,20}",
        ],
        0..20,
    ), crlf in any::<bool>()) {
        let text = lines.join(if crlf { "\r\n" } else { "\n" });
        let _ = import_base16(&text, "t");
    }

    #[test]
    fn itermcolors_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = import_itermcolors(&bytes, "t");
        let mut prefixed = b"bplist00".to_vec();
        prefixed.extend_from_slice(&bytes);
        let _ = import_itermcolors(&prefixed, "t");
    }

    #[test]
    fn slugs_are_safe(name in "\\PC{0,100}") {
        if let Some(s) = slug(&name) {
            prop_assert!(!s.is_empty() && s.len() <= 64);
            // Letters and digits that lowercasing leaves alone (`ℙ` has no
            // lowercase), and single dashes.
            prop_assert!(
                s.chars().all(|c| c == '-' || (c.is_alphanumeric() && c.to_lowercase().eq([c]))),
                "{s:?}"
            );
            prop_assert!(!s.starts_with('-') && !s.ends_with('-') && !s.contains("--"));
            prop_assert_eq!(slug(&s), Some(s.clone()), "idempotent");
        }
    }
}

/// A binary plist of `depth` arrays, each holding the next.
fn nested_bplist(depth: u32) -> Vec<u8> {
    let mut b = b"bplist00".to_vec();
    let mut offsets = Vec::new();
    for i in 0..depth {
        offsets.push(u32::try_from(b.len()).unwrap());
        if i + 1 == depth {
            b.push(0xa0); // empty array
        } else {
            b.push(0xa1); // array of one
            b.extend_from_slice(&(i + 1).to_be_bytes());
        }
    }
    let table = b.len();
    for o in offsets {
        b.extend_from_slice(&o.to_be_bytes());
    }
    let mut trailer = [0u8; 32];
    trailer[6] = 4;
    trailer[7] = 4;
    trailer[8..16].copy_from_slice(&u64::from(depth).to_be_bytes());
    trailer[24..32].copy_from_slice(&(table as u64).to_be_bytes());
    b.extend_from_slice(&trailer);
    b
}

#[test]
fn deeply_nested_binary_plist_does_not_overflow_the_stack() {
    let bytes = nested_bplist(100_000);
    assert!(bytes.len() <= MAX_IMPORT_LEN);
    let result = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || import_itermcolors(&bytes, "t").map(|_| ()))
        .unwrap()
        .join()
        .expect("no stack overflow or panic");
    assert!(result.is_err());
}

/// A themes directory full of broken files that all match the requested
/// name by slug (`dracula`, `dracula_`, `dracula__`, ...) and of unrelated
/// files: one lookup reads at most `MAX_THEME_FILES` of them, reports at
/// most that many warnings, and stays fast.
#[test]
fn a_flood_of_matching_broken_theme_files_is_bounded() {
    use tessera_config::{MAX_THEME_FILES, ThemeSource, Warning};

    let dir = tempfile::tempdir().unwrap();
    // File names stop at 255 bytes, so vary the separators too.
    for i in 0..600 {
        let stem = format!("{}dracula{}", "-".repeat(i / 200), "_".repeat(i % 200));
        std::fs::write(dir.path().join(format!("{stem}.toml")), "foreground = ").unwrap();
    }
    for i in 0..600 {
        std::fs::write(dir.path().join(format!("other-{i}.toml")), "x = 1").unwrap();
    }
    let start = Instant::now();
    let resolved = Theme::resolve(dir.path(), "Dracula");
    let took = start.elapsed();
    eprintln!(
        "lookup over 1200 files: {took:?}, {} warnings",
        resolved.warnings.len()
    );
    assert_eq!(resolved.source, ThemeSource::Bundled);
    assert_eq!(resolved.warnings.len(), MAX_THEME_FILES);
    assert!(
        resolved
            .warnings
            .iter()
            .all(|w| matches!(w, Warning::ThemeInvalid { .. }))
    );
    let bytes: usize = resolved.warnings.iter().map(|w| w.to_string().len()).sum();
    assert!(bytes < 1 << 20, "{bytes} bytes of warnings");
    assert!(took < Duration::from_secs(10), "{took:?}");

    // An unknown name reads up to the same budget of `name` keys.
    let start = Instant::now();
    let resolved = Theme::resolve(dir.path(), "nope");
    assert_eq!(resolved.source, ThemeSource::Default);
    assert!(start.elapsed() < Duration::from_secs(10));
}

/// The worst lookup: `MAX_THEME_FILES` valid, near-limit files that match
/// nothing, all parsed for their `name` key.
#[test]
fn the_largest_name_key_scan_stays_under_a_second_in_release() {
    use tessera_config::{MAX_THEME_FILE_LEN, MAX_THEME_FILES, ThemeSource};

    let dir = tempfile::tempdir().unwrap();
    let theme = Theme::bundled("Nord")
        .unwrap()
        .to_toml()
        .replace("name = \"Nord\"", "name = \"x\"");
    let mut padded = theme.clone();
    while padded.len() + 40 < usize::try_from(MAX_THEME_FILE_LEN).unwrap() {
        padded.push_str(
            "pad = \"0123456789012345678901234567890\"\n"
                .replacen("pad", &format!("p{}", padded.len()), 1)
                .as_str(),
        );
    }
    for i in 0..MAX_THEME_FILES {
        std::fs::write(dir.path().join(format!("t{i:03}.toml")), &padded).unwrap();
    }
    let start = Instant::now();
    let resolved = Theme::resolve(dir.path(), "nope");
    let took = start.elapsed();
    eprintln!(
        "name-key scan of {MAX_THEME_FILES} x {} bytes: {took:?}",
        padded.len()
    );
    assert_eq!(resolved.source, ThemeSource::Default);
    // Debug builds parse TOML about 15x slower and the wave runs many
    // gates at once (5-13 s seen under load); release stays strict, so a
    // release run on a heavily loaded machine can exceed 1 s (about 0.1 s
    // idle). CI and the gate run debug tests only.
    let limit = if cfg!(debug_assertions) { 120 } else { 1 };
    assert!(took < Duration::from_secs(limit), "{took:?}");
}

// --- amplification through shared references (binary plists) -------------

/// A binary-plist builder: objects in order, refs and offsets 4 bytes.
struct Bplist {
    body: Vec<u8>,
    offsets: Vec<u32>,
}

impl Bplist {
    fn new() -> Self {
        Self {
            body: b"bplist00".to_vec(),
            offsets: Vec::new(),
        }
    }

    fn marker(&mut self, kind: u8, count: usize) {
        if count < 15 {
            self.body.push(kind | u8::try_from(count).unwrap());
        } else {
            self.body.extend_from_slice(&[kind | 0x0f, 0x12]);
            self.body
                .extend_from_slice(&u32::try_from(count).unwrap().to_be_bytes());
        }
    }

    fn begin(&mut self) {
        self.offsets.push(u32::try_from(self.body.len()).unwrap());
    }

    fn refs(&mut self, refs: impl IntoIterator<Item = u32>) {
        for r in refs {
            self.body.extend_from_slice(&r.to_be_bytes());
        }
    }

    /// An array of `count` references to object `child`.
    fn array(&mut self, count: usize, child: u32) {
        self.begin();
        self.marker(0xa0, count);
        self.refs(std::iter::repeat_n(child, count));
    }

    /// A dictionary of `keys` (objects) to `values` (objects).
    fn dict(&mut self, keys: &[u32], values: &[u32]) {
        self.begin();
        self.marker(0xd0, keys.len());
        self.refs(keys.iter().copied());
        self.refs(values.iter().copied());
    }

    fn string(&mut self, text: &[u8]) {
        self.begin();
        self.marker(0x50, text.len());
        self.body.extend_from_slice(text);
    }

    fn int0(&mut self) {
        self.int(0);
    }

    fn int(&mut self, value: u8) {
        self.begin();
        self.body.extend_from_slice(&[0x10, value]);
    }

    fn finish(mut self) -> Vec<u8> {
        let table = self.body.len() as u64;
        let count = self.offsets.len() as u64;
        for o in std::mem::take(&mut self.offsets) {
            self.body.extend_from_slice(&o.to_be_bytes());
        }
        self.body.extend_from_slice(&[0; 6]);
        self.body.extend_from_slice(&[4, 4]);
        self.body.extend_from_slice(&count.to_be_bytes());
        self.body.extend_from_slice(&0u64.to_be_bytes());
        self.body.extend_from_slice(&table.to_be_bytes());
        self.body
    }
}

/// Resident set size of this process in bytes (`ps`; macOS reports KiB).
fn rss() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap()
        * 1024
}

/// Imports `bytes` on a fresh thread, asserting it fails with an error
/// `expected` accepts (so each case shows which defence stopped it) within
/// 100 ms (1 s in a debug build) and grows RSS by less than 64 MiB.
fn assert_refused_quickly(what: &str, bytes: Vec<u8>, expected: fn(&ImportError) -> bool) {
    let limit = if cfg!(debug_assertions) {
        Duration::from_secs(1)
    } else {
        Duration::from_millis(100)
    };
    let before = rss();
    let start = Instant::now();
    // Wait at most twice the limit, so an import that never ends fails the
    // test instead of hanging it (the process exits with the thread).
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(import_itermcolors(&bytes, "t").map(|_| ()));
    });
    let result = rx
        .recv_timeout(limit * 2)
        .unwrap_or_else(|_| panic!("{what}: no result within {:?}", limit * 2));
    let took = start.elapsed();
    let grew = rss().saturating_sub(before);
    eprintln!("{what}: {took:?}, RSS +{} KiB, {result:?}", grew / 1024);
    let error = result.expect_err(what);
    assert!(
        expected(&error),
        "{what}: stopped by the wrong defence: {error:?}"
    );
    assert!(took < limit, "{what} took {took:?}");
    assert!(grew < 64 << 20, "{what} grew RSS by {grew} bytes");
}

#[test]
fn a_cube_of_shared_array_references_is_refused_quickly() {
    // Root array of N refs to one array of N refs to one array of N refs to
    // an integer: N^3 visits from a few kilobytes.
    let n = 400;
    let mut b = Bplist::new();
    b.array(n, 1);
    b.array(n, 2);
    b.array(n, 3);
    b.int0();
    let bytes = b.finish();
    assert!(bytes.len() < 8 * 1024);
    // Refused structurally: a colour scheme's root is a dictionary.
    assert_refused_quickly("array cube", bytes, |e| {
        matches!(e, ImportError::NotADictionary)
    });
}

#[test]
fn a_cube_of_shared_dictionary_references_is_refused_quickly() {
    // Root dict of N entries "k" -> slot dict of N entries "k" -> leaf dict
    // of N entries "k" -> 0.
    let n = 400;
    let mut b = Bplist::new();
    let key = 4;
    b.dict(&vec![key; n], &vec![1; n]);
    b.dict(&vec![key; n], &vec![2; n]);
    b.dict(&vec![key; n], &vec![3; n]);
    b.int0();
    b.string(b"k");
    let bytes = b.finish();
    assert!(bytes.len() < 16 * 1024);
    // Refused structurally: a colour component is never a collection.
    assert_refused_quickly("dict cube", bytes, |e| matches!(e, ImportError::Plist(_)));

    // The same with distinct keys at the root, so the root dictionary is
    // honest and only the colour dictionaries amplify.
    let n = 2000;
    let mut b = Bplist::new();
    let keys: Vec<u32> = (0..n).map(|i| 2 + i).collect();
    b.dict(&keys, &vec![1; keys.len()]);
    b.dict(&vec![2; 400], &vec![2 + n; 400]);
    for i in 0..n {
        b.string(format!("Ansi {i} Color").as_bytes());
    }
    b.int0();
    assert_refused_quickly("wide root over a shared slot", b.finish(), |e| {
        matches!(e, ImportError::TooComplex)
    });
}

#[test]
fn a_shared_big_string_is_refused_quickly() {
    // One colour dictionary of N distinct keys all pointing at one big
    // string: N * S bytes of strings from N * 8 + S bytes of file.
    let n: u32 = 4000;
    let size = 256 * 1024;
    let mut b = Bplist::new();
    b.dict(&[3], &[1]);
    let keys: Vec<u32> = (0..n).map(|i| 4 + i).collect();
    b.dict(&keys, &vec![2; keys.len()]);
    b.string(&vec![b'x'; size]);
    b.string(b"Ansi 0 Color");
    for i in 0..n {
        b.string(format!("{i:x}").as_bytes());
    }
    let bytes = b.finish();
    assert!(bytes.len() <= tessera_config::MAX_IMPORT_LEN);
    assert_refused_quickly("shared big string", bytes, |e| {
        matches!(e, ImportError::TooComplex)
    });

    // The big string as the `Color Space` of every slot.
    let mut b = Bplist::new();
    let slots: Vec<u32> = (0..20).map(|i| 4 + i).collect();
    b.dict(&slots, &vec![1; slots.len()]);
    b.dict(&[2], &[3]);
    b.string(b"Color Space");
    b.string(&vec![b'x'; size]);
    for i in 0..20 {
        b.string(format!("Ansi {i} Color").as_bytes());
    }
    assert_refused_quickly("big shared colour space", b.finish(), |e| {
        matches!(e, ImportError::TooComplex)
    });
}

#[test]
fn imported_names_are_clipped() {
    use tessera_config::MAX_THEME_NAME_LEN;

    let long = "é".repeat(MAX_THEME_NAME_LEN);
    let dracula = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dracula.itermcolors"),
    )
    .unwrap();
    let theme = import_itermcolors(&dracula, &long).unwrap();
    assert!(theme.name.len() <= MAX_THEME_NAME_LEN && long.starts_with(&theme.name));
    let nord = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/base16-nord-legacy.yaml"),
    )
    .unwrap()
    .replace("scheme: \"Nord\"", &format!("scheme: \"{long}\""));
    let theme = import_base16(&nord, "x").unwrap();
    assert!(theme.name.len() <= MAX_THEME_NAME_LEN && long.starts_with(&theme.name));
    // The caller's default name is the caller's to bound.
    assert_eq!(
        import_base16(&nord.replace(&format!("scheme: \"{long}\""), ""), "dflt")
            .unwrap()
            .name,
        "dflt"
    );
}

/// Integer dictionary keys cost no text bytes, so only the value budget
/// stops this: a root dictionary of n entries `1 -> slot`, where the one
/// shared slot dictionary has n entries `1 -> 0`. Without the value budget
/// that is n^2 visits (about 7 s at n = 20 000, release).
#[test]
fn integer_keyed_shared_dictionaries_hit_the_value_budget() {
    // 4-byte references: 16 bytes per entry pair across both
    // dictionaries, so n = 60 000 stays under the input cap.
    let n = 60_000;
    let mut b = Bplist::new();
    b.dict(&vec![2; n], &vec![1; n]);
    b.dict(&vec![2; n], &vec![3; n]);
    b.int(1);
    b.int0();
    let bytes = b.finish();
    assert!(bytes.len() <= tessera_config::MAX_IMPORT_LEN);
    assert_refused_quickly("integer-keyed shared slot", bytes, |e| {
        matches!(e, ImportError::TooComplex)
    });
}
