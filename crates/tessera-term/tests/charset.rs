//! Character sets: SCS, SO/SI, the locking and single shifts and DECNRCM
//! (#112, ADR 0033).
//!
//! Expected behaviour comes from `tests/fixtures/charset_spec.txt`: every
//! row of it runs whole, byte by byte and split in two at every offset of
//! every feed step, in the layout of `csi_spec.txt` (read by
//! `tests/spec/mod.rs`). Its header cites xterm's `charsets.c` and
//! `charproc.c` and lists the decisions the rows encode; the notes file
//! beside it lists where the sources disagree.
//!
//! The terminfo strings the rows quote are checked once against the local
//! `infocmp` too ([`smacs_rmacs_from_infocmp_draw_a_box`]).

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;
#[allow(dead_code)] // Shared with tests/csi.rs.
mod spec;

use common::{Step, render_screen};
use spec::Case;
use tessera_term::Terminal;

const FIXTURE: &str = include_str!("fixtures/charset_spec.txt");
const FIXTURE_NAME: &str = "fixtures/charset_spec.txt";

/// `###` rows in the fixture, so a row lost from it cannot pass silently.
const ROWS: usize = 123;

/// How the feed steps of a case reach [`Terminal::advance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Split {
    /// Every step in one call.
    Whole,
    /// One byte per call.
    Bytewise,
    /// Step `step` in two calls, cut after `at` bytes; the others whole.
    At { step: usize, at: usize },
}

/// Runs the steps of `case` on a fresh terminal.
fn run(case: &Case, split: Split) -> Terminal {
    let mut term = Terminal::new(case.script.rows, case.script.cols);
    for (index, step) in case.script.steps.iter().enumerate() {
        let Step::Feed(bytes) = step else {
            panic!("{}: charset rows only feed bytes", case.id);
        };
        match split {
            Split::Bytewise => bytes
                .iter()
                .for_each(|byte| term.advance(std::slice::from_ref(byte))),
            Split::At { step, at } if step == index => {
                term.advance(&bytes[..at]);
                term.advance(&bytes[at..]);
            }
            Split::Whole | Split::At { .. } => term.advance(bytes),
        }
    }
    term
}

/// Every split `case` runs with.
fn splits(case: &Case) -> Vec<Split> {
    let mut all = vec![Split::Whole, Split::Bytewise];
    for (step, item) in case.script.steps.iter().enumerate() {
        if let Step::Feed(bytes) = item {
            all.extend((1..bytes.len()).map(|at| Split::At { step, at }));
        }
    }
    all
}

/// Every mismatch between `case` and the terminal after `split`.
fn check(case: &Case, split: Split) -> Vec<String> {
    let mut term = run(case, split);
    let mut errors = Vec::new();
    let mut fail = |what: &str, detail: String| {
        errors.push(format!("{} [{split:?}] {what}: {detail}", case.id));
    };
    let screen = render_screen(&term);
    if screen != case.screen {
        fail(
            "screen",
            format!("\nexpected:\n{}got:\n{screen}", case.screen),
        );
    }
    let requests = term.take_requests();
    let grid = term.grid();
    if grid.cursor() != case.cursor {
        fail(
            "cursor",
            format!("expected {:?}, got {:?}", case.cursor, grid.cursor()),
        );
    }
    if grid.pending_wrap() != case.pending_wrap {
        fail("pending_wrap", format!("expected {}", case.pending_wrap));
    }
    if requests != case.requests {
        fail(
            "requests",
            format!("expected {:?}, got {requests:?}", case.requests),
        );
    }
    if let Some((len, _)) = &case.scrollback
        && grid.scrollback_len() != *len
    {
        fail(
            "scrollback",
            format!("expected {len}, got {}", grid.scrollback_len()),
        );
    }
    errors
}

/// Every row of the fixture, whole, byte by byte and at every two-way split.
#[test]
fn every_row() {
    let cases = spec::parse_fixture(FIXTURE);
    let headings = FIXTURE.lines().filter(|l| l.starts_with("### ")).count();
    assert_eq!(headings, ROWS, "rows in the fixture");
    common::check_table(
        FIXTURE_NAME,
        headings,
        cases.iter().map(|case| (case.id.clone(), case)),
        |case| {
            let errors: Vec<String> = splits(case)
                .into_iter()
                .flat_map(|split| check(case, split))
                .collect();
            let shown: Vec<&str> = errors.iter().take(3).map(String::as_str).collect();
            assert!(
                errors.is_empty(),
                "{} mismatches, the first:\n{}",
                errors.len(),
                shown.join("\n")
            );
        },
    );
}

/// The row ids are unique.
#[test]
fn row_ids_are_unique() {
    let cases = spec::parse_fixture(FIXTURE);
    let mut ids: Vec<&str> = cases.iter().map(|case| case.id.as_str()).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), before, "duplicate row ids");
}

/// Parses one terminfo capability out of `infocmp -1` output (`\E` is ESC).
fn capability(infocmp: &str, name: &str) -> Option<Vec<u8>> {
    let line = infocmp
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(&format!("{name}=")))?;
    let line = line.trim_end_matches(',');
    let mut bytes = Vec::new();
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        match (ch, chars.clone().next()) {
            ('\\', Some('E')) => {
                bytes.push(0x1B);
                chars.next();
            }
            ('\\', Some(next)) => panic!("unhandled terminfo escape `\\{next}`"),
            _ => bytes.push(u8::try_from(ch).expect("ASCII capability")),
        }
    }
    Some(bytes)
}

/// `tput smacs; printf '\x6c\x71\x6b'; tput rmacs` under
/// `TERM=xterm-256color` draws `┌─┐` (the issue's Done-when): the strings
/// come from this machine's terminfo, so a change in them shows up here.
/// Skipped where `infocmp` or the entry is not installed.
#[test]
fn smacs_rmacs_from_infocmp_draw_a_box() {
    let Ok(output) = std::process::Command::new("infocmp")
        .args(["-1", "xterm-256color"])
        .output()
    else {
        eprintln!("skipped: no infocmp");
        return;
    };
    if !output.status.success() {
        eprintln!("skipped: no xterm-256color terminfo entry");
        return;
    }
    let infocmp = String::from_utf8_lossy(&output.stdout);
    let smacs = capability(&infocmp, "smacs").expect("xterm-256color has smacs");
    let rmacs = capability(&infocmp, "rmacs").expect("xterm-256color has rmacs");
    assert_eq!(smacs, b"\x1b(0", "smacs");
    assert_eq!(rmacs, b"\x1b(B", "rmacs");
    let mut stream = smacs;
    stream.extend_from_slice(b"\x6c\x71\x6b");
    stream.extend_from_slice(&rmacs);
    stream.extend_from_slice(b"\x6c\x71\x6b");
    assert_eq!(common::run_script(&stream, 2, 10), "┌─┐lqk |\n\n");
}

/// DECNRCM is level 2 and up, off by default, kept by DECSTR and cleared by
/// RIS (xterm; ADR 0033).
#[test]
fn decnrcm_state_follows_xterm() {
    let mut term = Terminal::new(3, 10);
    assert!(!term.modes().nrcs);
    term.advance(b"\x1b[?42h");
    assert!(term.modes().nrcs);
    term.advance(b"\x1b[!p");
    assert!(term.modes().nrcs, "DECSTR keeps DECNRCM");
    term.advance(b"\x1bc");
    assert!(!term.modes().nrcs, "RIS clears it");
    term.advance(b"\x1b[61;1\"p\x1b[?42h");
    assert!(!term.modes().nrcs, "level 1 has no DECNRCM");
}

/// Splits a character-set stream at arbitrary reads: the result does not
/// change.
mod properties {
    use proptest::prelude::*;

    use super::*;

    /// Tokens that designate, shift, save, restore, reset and print.
    const TOKENS: &[&[u8]] = &[
        b"\x1b(0",
        b"\x1b(B",
        b"\x1b(A",
        b"\x1b(K",
        b"\x1b(R",
        b"\x1b(%5",
        b"\x1b(%6",
        b"\x1b(>",
        b"\x1b)0",
        b"\x1b*0",
        b"\x1b+0",
        b"\x1b-A",
        b"\x1b.A",
        b"\x1b(\"4",
        b"\x1b(&5",
        b"\x1b( ",
        b"\x0e",
        b"\x0f",
        b"\x1bn",
        b"\x1bo",
        b"\x1b~",
        b"\x1b}",
        b"\x1b|",
        b"\x1bN",
        b"\x1bO",
        b"\x1b7",
        b"\x1b8",
        b"\x1b[s",
        b"\x1b[u",
        b"\x1b[!p",
        b"\x1bc",
        b"\x1b[?42h",
        b"\x1b[?42l",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[61;1\"p",
        b"\x1b[64;1\"p",
        b"\x1b[3b",
        b"\x1b[4h",
        b"\x1b[4l",
        b"\r\n",
        b"lqkxmj",
        b"#@[\\]^_`{|}~",
        b"abc ABC 019",
        "éü£ÿ\u{a0}日".as_bytes(),
        b"\xc3",
        b"\xa9",
        b"\x1b",
    ];

    fn stream() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(proptest::sample::select(TOKENS), 0..60).prop_map(|t| t.concat())
    }

    fn outcome(bytes: &[u8], cuts: &[usize]) -> (String, tessera_grid::Point, bool) {
        let mut term = Terminal::new(5, 14);
        let mut at = 0;
        for &cut in cuts.iter().chain(std::iter::once(&bytes.len())) {
            let cut = cut.clamp(at, bytes.len());
            term.advance(&bytes[at..cut]);
            at = cut;
        }
        (
            render_screen(&term),
            term.grid().cursor(),
            term.modes().nrcs,
        )
    }

    proptest! {
        /// However the output is split into reads, the screen, the cursor
        /// and DECNRCM are those of one read.
        #[test]
        fn character_sets_do_not_depend_on_read_boundaries(
            bytes in stream(),
            mut cuts in proptest::collection::vec(0usize..400, 0..10),
        ) {
            cuts.sort_unstable();
            prop_assert_eq!(outcome(&bytes, &cuts), outcome(&bytes, &[]));
        }

        /// A stream that never designates a set prints as if the character
        /// sets did not exist: ASCII and Latin-1 text comes out unchanged.
        #[test]
        fn without_designations_text_is_unchanged(text in "[!-~\u{a1}-\u{ac}\u{ae}-\u{ff}]{0,70}") {
            let mut term = Terminal::new(3, 80);
            term.advance(text.as_bytes());
            prop_assert_eq!(render_screen(&term), format!("{text} |\n\n\n"));
        }
    }
}
