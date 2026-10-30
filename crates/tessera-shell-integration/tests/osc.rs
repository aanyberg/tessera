//! The OSC 7 and OSC 1337 decoders: the decoder rows of the spec fixture
//! (`tests/fixtures/osc_sequences.txt`, section 5, rows D1-D19 and E1-E7),
//! and property tests.

use proptest::prelude::*;
use tessera_shell_integration::osc7::MAX_OSC7_LEN;
use tessera_shell_integration::{
    Cwd, ShellIntegrationVersion, parse_osc7, parse_osc133, parse_osc1337,
};

#[expect(
    clippy::unnecessary_wraps,
    reason = "the rows compare with what parse_osc7 returns"
)]
fn cwd(host: &str, path: &str) -> Option<Cwd> {
    Some(Cwd {
        host: host.into(),
        path: path.into(),
    })
}

#[test]
fn fixture_decoder_rows() {
    let rows: &[(&str, &[u8], Option<Cwd>)] = &[
        ("D1", b"file:///tmp", cwd("", "/tmp")),
        ("D2", b"file://Mac.home/tmp", cwd("Mac.home", "/tmp")),
        (
            "D3",
            b"file://Mac.home/tmp/a%20b",
            cwd("Mac.home", "/tmp/a b"),
        ),
        ("D4", b"file://h/tmp/caf%C3%A9", cwd("h", "/tmp/café")),
        ("D5", b"file://h/tmp/%F0%9F%90%B1", cwd("h", "/tmp/🐱")),
        ("D6", b"file://h/tmp/100%25", cwd("h", "/tmp/100%")),
        // Lead decision 5: an encoded `/`, malformed escapes, control bytes
        // and invalid UTF-8 are refused.
        ("D7", b"file://h/tmp/a%2fb", None),
        ("D7", b"file://h/tmp/a%2Fb", None),
        ("D8", b"file://h/tmp/%2e%2e/etc", None),
        ("D8", b"file://h/tmp/../etc", None),
        ("D8", b"file://h/..", None),
        ("D9", b"file://h/tmp/%zz", None),
        ("D9", b"file://h/tmp/%4", None),
        ("D10", b"file://h/tmp/%00", None),
        ("D10", b"file://h/tmp/a%0Ab", None),
        ("D10", b"file://h/tmp/a\nb", None),
        ("D10", b"file://h/tmp/%7F", None),
        ("D11", b"file://h/tmp/%FF", None),
        ("D11", b"file://h/tmp/\xff", None),
        // iTerm2's form: userinfo and query.
        ("D12", b"file://user@h/tmp?machineID=1:ab", cwd("h", "/tmp")),
        ("D13", b"file://h/tmp#frag", cwd("h", "/tmp")),
        ("D14", b"file://h", None),
        ("D15", b"tmp/x", None),
        ("D15", b"/tmp", None),
        ("D16", b"http://h/tmp", None),
        // Lead decision 4: kitty's scheme, with a raw path.
        ("D16", b"kitty-shell-cwd://h/tmp", cwd("h", "/tmp")),
        (
            "D16",
            b"kitty-shell-cwd://h/tmp/100%/a#b?c",
            cwd("h", "/tmp/100%/a#b?c"),
        ),
        ("D16", b"kitty-shell-cwd://h/tmp/../x", None),
        ("D17", b"file://[::1]/tmp", cwd("[::1]", "/tmp")),
        ("D18", b"FILE://h/tmp", cwd("h", "/tmp")),
        ("D18", b"File://h/tmp", cwd("h", "/tmp")),
        // `.` segments and empty segments are not `..`.
        ("-", b"file://h/./a//b/", cwd("h", "/./a//b/")),
        ("-", b"file://h/a..b/...", cwd("h", "/a..b/...")),
        // A host with a control character.
        ("-", b"file://h\x07x/tmp", None),
    ];
    let mut failures = Vec::new();
    for (id, payload, expected) in rows {
        let got = parse_osc7(payload);
        if &got != expected {
            failures.push(format!(
                "{id} {:?}: expected {expected:?}, got {got:?}",
                String::from_utf8_lossy(payload)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn fixture_row_d19_caps_the_payload() {
    let fits = format!("file://h/{}", "a".repeat(MAX_OSC7_LEN - "file://h/".len()));
    assert_eq!(fits.len(), MAX_OSC7_LEN);
    assert!(parse_osc7(fits.as_bytes()).is_some());
    let over = format!("{fits}a");
    assert_eq!(parse_osc7(over.as_bytes()), None);
}

/// Terminal.app's encoder (fixture P1): every byte outside
/// `[/._~A-Za-z0-9-]` as `%XX`.
fn encode(path: &str) -> String {
    path.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"/._~-".contains(&b) {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[test]
fn fixture_encoder_rows_round_trip() {
    let rows = [
        ("E1", "/tmp/a b", "/tmp/a%20b"),
        ("E2", "/tmp/100%", "/tmp/100%25"),
        ("E3", "/tmp/a#b?c", "/tmp/a%23b%3Fc"),
        ("E4", "/tmp/café", "/tmp/caf%C3%A9"),
        ("E5", "/tmp/🐱", "/tmp/%F0%9F%90%B1"),
        ("E6", "/tmp/a+b,c;d=e:f@g", "/tmp/a%2Bb%2Cc%3Bd%3De%3Af%40g"),
        ("E7", "/tmp/a'b\"c", "/tmp/a%27b%22c"),
    ];
    for (id, path, encoded) in rows {
        assert_eq!(encode(path), encoded, "{id}");
        assert_eq!(
            parse_osc7(format!("file://h{encoded}").as_bytes()),
            cwd("h", path),
            "{id}"
        );
    }
    // E6: the RFC 3986 sub-delimiters are accepted raw as well.
    assert_eq!(
        parse_osc7(b"file://h/tmp/a+b,c;d=e:f@g"),
        cwd("h", "/tmp/a+b,c;d=e:f@g")
    );
}

#[test]
fn handshake() {
    assert_eq!(
        parse_osc1337(b"ShellIntegrationVersion=1;shell=zsh"),
        Some(ShellIntegrationVersion {
            version: 1,
            shell: "zsh".into()
        })
    );
    // iTerm2's own script (fixture S2), and extra parameters in any order.
    assert_eq!(
        parse_osc1337(b"ShellIntegrationVersion=24;x=y;shell=bash"),
        Some(ShellIntegrationVersion {
            version: 24,
            shell: "bash".into()
        })
    );
    for payload in [
        &b"SetUserVar=foo=YmFy"[..],
        b"CurrentDir=/tmp",
        b"ShellIntegrationVersion=1",
        b"ShellIntegrationVersion=;shell=zsh",
        b"ShellIntegrationVersion=-1;shell=zsh",
        b"ShellIntegrationVersion=+1;shell=zsh",
        b"ShellIntegrationVersion=4294967296;shell=zsh",
        b"ShellIntegrationVersion=1;shell=",
        b"ShellIntegrationVersion=1;shell=z sh",
        b"ShellIntegrationVersion=1;shell=z\x1bsh",
        b"ShellIntegrationVersion=1;shell=abcdefghijklmnopqrstuvwxyz0123456",
        b"shell=zsh;ShellIntegrationVersion=1",
        b"",
    ] {
        assert_eq!(
            parse_osc1337(payload),
            None,
            "{:?}",
            String::from_utf8_lossy(payload)
        );
    }
}

/// The `layer: parse` rows P7-01..P7-26 and P1337-01..P1337-12 of
/// `crates/tessera-term/tests/fixtures/osc_spec.txt` (#9), by row id: what
/// app-core gets from the payloads the terminal forwards raw.
#[test]
fn osc_spec_parse_rows() {
    let path = |path: &str| cwd("h", path);
    let long = |extra: usize| format!("file://h/{}", "a".repeat(4087 + extra));
    let fits = long(0);
    let over = long(1);
    let rows: Vec<(&str, &[u8], Option<Cwd>)> = vec![
        ("P7-01", b"file:///tmp", cwd("", "/tmp")),
        ("P7-02", b"file://Mac.home/tmp", cwd("Mac.home", "/tmp")),
        ("P7-03", b"file://h/tmp/a%20b", path("/tmp/a b")),
        ("P7-04", b"file://h/caf%C3%A9", path("/caf\u{e9}")),
        ("P7-05", b"file://h/a%00b", None),
        ("P7-06", b"file://h/a%0Ab", None),
        ("P7-07", b"file://h/a%1Bb", None),
        ("P7-08", b"file://h/a%7Fb", None),
        ("P7-09", b"file://h/a%2Fb", None),
        ("P7-10", b"file://h/tmp/../etc", None),
        ("P7-11", b"file://h/tmp/%2e%2e/etc", None),
        ("P7-12", b"file://h/tmp/..foo", path("/tmp/..foo")),
        ("P7-13", b"file://h", None),
        ("P7-14", b"file://h/", path("/")),
        ("P7-15", b"file://h/a%zz", None),
        ("P7-16", b"file://h/a%4", None),
        ("P7-17", b"file://h/a%FF", None),
        ("P7-18", b"file://h/%C0%AF", None),
        ("P7-19", b"file://u@h/a?x=1#f", path("/a")),
        ("P7-20", b"FILE://h/a", path("/a")),
        ("P7-21", b"http://h/a", None),
        ("P7-22", b"kitty-shell-cwd://h/a%b?c", path("/a%b?c")),
        ("P7-23", b"file://h/a%E2%80%AEb", path("/a\u{202e}b")),
        ("P7-24", b"file://h/a;b", path("/a;b")),
        ("P7-25", fits.as_bytes(), path(&fits["file://h".len()..])),
        ("P7-26", over.as_bytes(), None),
    ];
    assert_eq!(fits.len(), MAX_OSC7_LEN, "P7-25 is 4096 bytes");
    let mut failures = Vec::new();
    for (id, payload, expected) in &rows {
        let got = parse_osc7(payload);
        if &got != expected {
            failures.push(format!("{id}: expected {expected:?}, got {got:?}"));
        }
    }
    let version = |version: u32, shell: &str| {
        Some(ShellIntegrationVersion {
            version,
            shell: shell.into(),
        })
    };
    let shell_33 = format!("ShellIntegrationVersion=1;shell={}", "a".repeat(33));
    let handshakes: Vec<(&str, &[u8], Option<ShellIntegrationVersion>)> = vec![
        (
            "P1337-01",
            b"ShellIntegrationVersion=1;shell=zsh",
            version(1, "zsh"),
        ),
        (
            "P1337-02",
            b"ShellIntegrationVersion=2;shell=bash",
            version(2, "bash"),
        ),
        (
            "P1337-03",
            b"ShellIntegrationVersion=18;shell=zsh",
            version(18, "zsh"),
        ),
        ("P1337-04", b"ShellIntegrationVersion=1", None),
        ("P1337-05", b"ShellIntegrationVersion=;shell=zsh", None),
        ("P1337-06", b"ShellIntegrationVersion=1x;shell=zsh", None),
        ("P1337-07", b"ShellIntegrationVersion=-1;shell=zsh", None),
        (
            "P1337-08",
            b"ShellIntegrationVersion=99999999999;shell=zsh",
            None,
        ),
        ("P1337-09", b"ShellIntegrationVersion=1;shell=z sh", None),
        (
            "P1337-10",
            b"ShellIntegrationVersion=1;other=x;shell=zsh",
            version(1, "zsh"),
        ),
        ("P1337-11", shell_33.as_bytes(), None),
        ("P1337-12", b"SetUserVar=a=b", None),
    ];
    for (id, payload, expected) in &handshakes {
        let got = parse_osc1337(payload);
        if &got != expected {
            failures.push(format!("{id}: expected {expected:?}, got {got:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The `layer: parse` rows P133-01..P133-18 through the forwarder (the
/// parser's own test is next to `Mark::from_osc133` in tessera-grid).
#[test]
fn parse_osc133_forwards_to_the_grid_parser() {
    use tessera_grid::Mark;

    for payload in [
        &b"A"[..],
        b"B",
        b"C",
        b"D",
        b"D;130",
        b"D;-1",
        b"D;+3",
        b"D;0;aid=1",
        b"A;aid=1;cl=line",
        b"A;k=s",
        b"A;k=c",
        b"A;k=r",
        b"Afoo",
        b"AB",
        b"D0",
        b"a",
        b"",
        b"L",
        b"N",
        b"P",
        b"I",
        b"Z",
    ] {
        assert_eq!(
            parse_osc133(payload),
            Mark::from_osc133(payload),
            "{payload:?}"
        );
    }
    assert_eq!(parse_osc133(b"Afoo"), None, "P133-13");
    assert_eq!(parse_osc133(b"A;k=s"), None, "P133-10");
}

/// A path segment: any characters but `/` and controls, never `..`.
fn segment() -> impl Strategy<Value = String> {
    "[^/\\x00-\\x1f\\x7f-\\u{9f}]{0,12}".prop_filter("not ..", |s| s != "..")
}

proptest! {
    #[test]
    fn decoders_never_panic(payload in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = parse_osc7(&payload);
        let _ = parse_osc1337(&payload);
        let _ = parse_osc133(&payload);
    }

    #[test]
    fn decoders_never_panic_on_near_misses(
        prefix in prop_oneof![
            Just("file://".to_owned()),
            Just("FILE://h/".to_owned()),
            Just("kitty-shell-cwd://".to_owned()),
            Just("ShellIntegrationVersion=".to_owned()),
        ],
        rest in "[%/@?#;=a-fA-F0-9.\\x00-\\x20\\u{80}-\\u{ff}]{0,64}",
    ) {
        let payload = format!("{prefix}{rest}");
        let _ = parse_osc7(payload.as_bytes());
        let _ = parse_osc1337(payload.as_bytes());
    }

    #[test]
    fn encoded_paths_round_trip(
        host in "[A-Za-z0-9._-]{0,16}",
        segments in proptest::collection::vec(segment(), 1..6),
    ) {
        let path = format!("/{}", segments.join("/"));
        let payload = format!("file://{host}{}", encode(&path));
        prop_assert_eq!(parse_osc7(payload.as_bytes()), cwd(&host, &path));
    }

    #[test]
    fn a_decoded_path_is_absolute_text_without_dot_dot(
        payload in proptest::collection::vec(any::<u8>(), 0..128),
    ) {
        let mut with_scheme = b"file://h".to_vec();
        with_scheme.extend_from_slice(&payload);
        if let Some(cwd) = parse_osc7(&with_scheme) {
            prop_assert!(cwd.path.starts_with('/'));
            prop_assert!(!cwd.path.split('/').any(|segment| segment == ".."));
            prop_assert!(!cwd.path.chars().any(char::is_control));
        }
    }
}
