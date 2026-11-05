//! Protected areas and selective erase (#95, ADR 0034): SPA/EPA (ISO
//! protection), DECSCA (DEC protection), DECSED and DECSEL, and the ED, EL
//! and ECH that honour ISO protection.
//!
//! Expected behaviour comes from `tests/fixtures/protect_spec.txt`, rows
//! hand-derived from xterm's source and esctest2. Rows with a `screen:` are
//! in the `csi_spec.txt` layout and run through the shared
//! [`spec::check_case`] (screen, cursor, pending wrap, requests,
//! scrollback), whole and byte by byte. Every row is then checked, whole
//! and byte by byte, on the keys that reader ignores:
//!
//! - `reply:` the [`Request::Reply`] bytes of the whole script,
//!   concatenated (`\e` is ESC; the escapes of [`common::decode`]);
//! - `pstate: <mode>/<pen>`: [`Terminal::protection_mode`] and whether the
//!   pen carries [`Style::PROTECTED`] after the script.
//!
//! Protection is invisible on the screen, so every row proves it by what
//! an erase leaves behind; the `flags: control` rows run the same erase
//! without protection and prove the setups are not vacuous. Rows after the
//! fixture's `#>` marker wait for DECSERA (#251) and are not cases.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;
#[allow(dead_code)] // Shared with tests/csi.rs and tests/vttest.rs.
mod spec;

use common::{Chunking, Script, Step, decode, render_screen, split_replies};
use tessera_grid::{Cell, CellFlags, Point, Style};
use tessera_term::Terminal;

const FIXTURE: &str = include_str!("fixtures/protect_spec.txt");
const FIXTURE_NAME: &str = "fixtures/protect_spec.txt";

/// Cases in the fixture: a row lost from it fails the table.
const CASES: usize = 73;

/// Replaces `\e` with `\x1b` in a fixture payload, leaving `\\` alone, so
/// [`common::decode`] reads it.
fn expand_esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('e') => out.push_str("\\x1b"),
            Some(next) => {
                out.push('\\');
                out.push(next);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// One fixture row.
#[derive(Debug)]
struct Row {
    id: String,
    /// The row's text with `\e` expanded, from its `###` heading on.
    block: String,
    script: Script,
    reply: Option<Vec<u8>>,
    pstate: Option<(String, bool)>,
    flags: Vec<String>,
    has_screen: bool,
}

/// Every row of the fixture, in order, up to the `#>` pending blocks.
fn rows() -> Vec<Row> {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in FIXTURE.lines() {
        if line.starts_with("### ") {
            blocks.push(vec![line]);
        } else if line.starts_with("# --") || line.starts_with("#>") {
            blocks.push(Vec::new());
        } else if let Some(block) = blocks.last_mut() {
            block.push(line);
        }
    }
    blocks
        .into_iter()
        .filter(|block| block.first().is_some_and(|l| l.starts_with("### ")))
        .map(|block| {
            let block = expand_esc(&block.join("\n"));
            let id = block.lines().next().expect("heading")[4..].to_owned();
            let field = |key: &str| {
                block
                    .lines()
                    .find_map(|l| l.strip_prefix(key).map(str::trim).map(str::to_owned))
            };
            let (rows, cols) = field("size: ")
                .and_then(|s| {
                    let (r, c) = s.split_once('x')?;
                    Some((r.parse().ok()?, c.parse().ok()?))
                })
                .unwrap_or_else(|| panic!("{id}: no size"));
            let steps = block
                .lines()
                .filter_map(|l| l.strip_prefix("step: feed "))
                .map(|p| Step::Feed(decode(p)))
                .collect();
            let pstate = field("pstate: ").map(|p| {
                let (mode, pen) = p.split_once('/').expect("pstate is mode/pen");
                (mode.to_owned(), pen == "1")
            });
            Row {
                reply: field("reply: ").map(|r| decode(&r)),
                pstate,
                flags: field("flags: ")
                    .map(|f| f.split_whitespace().map(str::to_owned).collect())
                    .unwrap_or_default(),
                has_screen: block.lines().any(|l| l == "screen:"),
                script: Script { rows, cols, steps },
                block,
                id,
            }
        })
        .collect()
}

/// Every mismatch of `row`, run with `chunking`.
fn check_row(row: &Row, chunking: Chunking) -> Vec<String> {
    let mut errors = Vec::new();
    if row.has_screen {
        let cases = spec::parse_fixture(&row.block);
        assert_eq!(cases.len(), 1, "{}: one case per block", row.id);
        errors.extend(spec::check_case(&cases[0], chunking));
    }
    let (term, requests) = row.script.run(chunking);
    if let Some(expected) = &row.reply {
        let (replies, _) = split_replies(&requests);
        if &replies != expected {
            errors.push(format!(
                "{} ({chunking:?}) reply: expected {:?}, got {:?}",
                row.id,
                String::from_utf8_lossy(expected),
                String::from_utf8_lossy(&replies)
            ));
        }
    }
    if let Some((mode, pen)) = &row.pstate {
        let got = (
            term.protection_mode(),
            term.grid().pen().has(Style::PROTECTED),
        );
        if got != (mode.as_str(), *pen) {
            errors.push(format!(
                "{} ({chunking:?}) pstate: expected {mode}/{}, got {}/{}",
                row.id,
                u8::from(*pen),
                got.0,
                u8::from(got.1)
            ));
        }
    }
    term.grid().check_invariants();
    errors
}

/// Every row holds, whole and byte by byte.
#[test]
fn every_row_holds() {
    let rows = rows();
    common::check_table(
        FIXTURE_NAME,
        CASES,
        rows.iter().map(|row| (row.id.clone(), row)),
        |row| {
            let errors: Vec<String> = [Chunking::Whole, Chunking::Bytewise]
                .into_iter()
                .flat_map(|chunking| check_row(row, chunking))
                .collect();
            assert!(errors.is_empty(), "{}", errors.join("\n"));
        },
    );
}

/// Each row asserts something: a screen, a reply or a protection state;
/// and the table has controls, so its setups are proven not vacuous.
#[test]
fn every_row_asserts_and_controls_exist() {
    let rows = rows();
    for row in &rows {
        assert!(
            row.has_screen || row.reply.is_some(),
            "{}: neither a screen nor a reply",
            row.id
        );
    }
    let controls = rows
        .iter()
        .filter(|r| r.flags.iter().any(|f| f == "control"))
        .count();
    assert!(controls >= 5, "only {controls} control rows");
}

// ------------------------------------------------------------ direct tests

fn feed(rows: u16, cols: u16, bytes: &[u8]) -> Terminal {
    let mut term = Terminal::new(rows, cols);
    term.advance(bytes);
    term
}

fn reply(term: &mut Terminal) -> String {
    let (bytes, _) = split_replies(&term.take_requests());
    String::from_utf8_lossy(&bytes).into_owned()
}

/// DECRQSS `m` reports the renditions only: the pen's protection is not
/// one of them, so the answer after DECSCA 1 (or SPA) is the answer
/// without it.
#[test]
fn decrqss_sgr_never_reports_protection() {
    for setup in [&b"\x1b[1;4;31m"[..], b"", b"\x1b[0m\x1b[7m"] {
        let mut plain = feed(2, 10, setup);
        plain.advance(b"\x1bP$qm\x1b\\");
        let expected = reply(&mut plain);
        for protect in [&b"\x1b[1\"q"[..], b"\x1bV"] {
            let mut term = feed(2, 10, protect);
            term.advance(setup);
            assert!(term.grid().pen().has(Style::PROTECTED));
            term.advance(b"\x1bP$qm\x1b\\");
            assert_eq!(reply(&mut term), expected, "{protect:?} {setup:?}");
        }
    }
}

/// SGR 0 and every other SGR keep the pen's protection; the cells printed
/// carry it, the blanks an erase leaves never do.
#[test]
fn sgr_keeps_protection_and_blanks_never_carry_it() {
    let mut term = feed(2, 10, b"\x1b[1\"q\x1b[0;1;7;38;5;3;48;2;1;2;3mab\x1b[m");
    assert!(term.grid().pen().has(Style::PROTECTED));
    let cell = *term.grid().cell(Point::new(0, 0)).expect("cell");
    assert!(cell.is_protected() && cell.style.has(Style::BOLD | Style::INVERSE));
    // DEC mode: ED ignores the bit; the blanks are plain.
    term.advance(b"\x1b[41m\x1b[2J");
    for col in 0..10 {
        let cell = term.grid().cell(Point::new(0, col)).expect("cell");
        assert!(!cell.is_protected(), "col {col}");
    }
    assert!(term.grid().pen().has(Style::PROTECTED), "ED keeps the pen");
}

/// A protected wide character is atomic: an erase that covers either half
/// keeps both, and one that covers only an unprotected neighbour leaves it
/// whole (xterm `ClearInLine2`). Checked for ECH and EL in ISO mode and
/// DECSEL in DEC mode, on each half.
#[test]
fn protected_wide_characters_are_never_split() {
    for (setup, erase) in [
        (&b"\x1bV\xe4\xb8\xad\x1bWab"[..], &b"\x1b[1;2H\x1b[X"[..]),
        (b"\x1bV\xe4\xb8\xad\x1bWab", b"\x1b[1;2H\x1b[1K"),
        (b"\x1bV\xe4\xb8\xad\x1bWab", b"\x1b[1;1H\x1b[K"),
        (b"\x1b[1\"q\xe4\xb8\xad\x1b[0\"qab", b"\x1b[1;2H\x1b[?1K"),
        (b"\x1b[1\"q\xe4\xb8\xad\x1b[0\"qab", b"\x1b[1;2H\x1b[?0K"),
        (b"ab\x1bV\xe4\xb8\xad\x1bW", b"\x1b[1;3H\x1b[2X"),
    ] {
        let mut term = feed(2, 10, setup);
        term.advance(erase);
        let line = term.grid().line(0).expect("row");
        let wide = line.cells.iter().position(Cell::is_wide).expect("kept");
        assert!(line.cells[wide + 1].is_spacer(), "{setup:?} {erase:?}");
        assert!(line.cells[wide].is_protected() && line.cells[wide + 1].is_protected());
        assert!(line.text().contains('中'), "{setup:?} {erase:?}");
        term.grid().check_invariants();
    }
}

/// An unprotected wide character next to a protected cell is erased whole
/// when the range covers one half of it.
#[test]
fn an_unprotected_wide_character_cut_by_the_range_goes_whole() {
    let mut term = feed(2, 10, b"\x1bVa\x1bW\xe4\xb8\xad\x1bVb\x1bW");
    // ECH 1 on the spacer (col 2): the wide character at 1..=2 goes.
    term.advance(b"\x1b[1;3H\x1b[X");
    let line = term.grid().line(0).expect("row");
    assert_eq!(line.text(), "a  b");
    assert!(line.cells.iter().all(|c| !c.is_wide() && !c.is_spacer()));
    term.grid().check_invariants();
}

/// Protected cells keep their cluster text and hyperlink through a
/// selective erase; the erased ones lose theirs.
#[test]
fn clusters_and_links_of_kept_cells_survive() {
    let mut term = feed(
        2,
        10,
        b"\x1b]8;;https://a.example/\x1b\\x\x1bVe\xcc\x81\x1bWy\x1b]8;;\x1b\\\x1b[1;1H\x1b[K",
    );
    let line = term.grid().line(0).expect("row");
    assert_eq!(line.text(), " e\u{301}");
    assert!(line.cells[1].flags.contains(CellFlags::CLUSTER));
    assert!(line.hyperlink(1).is_some(), "the kept cell keeps its link");
    assert!(line.hyperlink(0).is_none() && line.hyperlink(2).is_none());
    term.advance(b"\x1b[2J");
    term.grid().check_invariants();
}

/// Protection travels with the text through a re-wrap: a resize that
/// joins and splits a soft-wrapped line keeps the bit on each cell, and an
/// ISO-mode erase after it still honours them.
#[test]
fn protection_survives_reflow() {
    let mut term = feed(3, 4, b"ab\x1bVcdef\x1bWgh");
    term.resize(3, 10);
    assert_eq!(term.grid().line(0).expect("row").text(), "abcdefgh");
    term.advance(b"\x1b[1;1H\x1b[2K");
    assert_eq!(term.grid().line(0).expect("row").text(), "  cdef");
    term.resize(3, 3);
    term.advance(b"\x1b[2J");
    let text: Vec<String> = term
        .grid()
        .visible_lines()
        .iter()
        .map(tessera_grid::Line::text)
        .collect();
    assert_eq!(text.concat(), "  cdef");
    term.grid().check_invariants();
}

/// The fast path never hides a protected cell: after an ED 2 found none
/// (the flag says the screen is clean), every way a protected cell can
/// reach the screen again makes the next ISO erase honour it.
#[test]
fn the_clean_screen_flag_never_hides_a_protected_cell() {
    let cases: &[(&str, &[u8])] = &[
        // SPA again after the clean ED 2.
        ("spa", b"\x1bV\x1bW\x1b[2J\x1bVp\x1bW"),
        // DECRC restores a protecting pen saved before the clean ED 2.
        ("decrc", b"\x1bV\x1b7\x1bW\x1b[2J\x1b8p\x1bW\x1b[1;1H"),
        // SCORC, the same.
        ("scorc", b"\x1bV\x1b[s\x1bW\x1b[2J\x1b[up\x1bW\x1b[1;1H"),
        // DECSET 1048 off, the same.
        (
            "1048",
            b"\x1bV\x1b[?1048h\x1bW\x1b[2J\x1b[?1048lp\x1bW\x1b[1;1H",
        ),
        // DECSET 1049 off restores the protecting pen the primary screen
        // had on entry, after the alternate screen was cleaned; the next
        // visit prints protected cells on a screen known clean.
        (
            "1049",
            b"\x1bV\x1b[?1049h\x1bW\x1b[2J\x1b[?1049l\x1b[?1049hp\x1bW\x1b[1;1H",
        ),
        // The alternate screen was cleaned; the primary still has one.
        (
            "primary kept",
            b"\x1bVp\x1bW\x1b[?1049h\x1b[2J\x1b[?1049l\x1b[1;1H",
        ),
        // DECSCA 1 (DEC mode), then SPA switches to ISO mode.
        (
            "decsca",
            b"\x1bV\x1bW\x1b[2J\x1b[1\"qp\x1b[0\"q\x1bV\x1bW\x1b[1;1H",
        ),
    ];
    for (name, setup) in cases {
        let mut term = feed(3, 10, setup);
        assert_eq!(term.protection_mode(), "iso", "{name}");
        term.advance(b"\x1b[1;1H\x1b[2K\x1b[X\x1b[J");
        let line = term.grid().line(0).expect("row");
        assert_eq!(
            line.text().trim(),
            "p",
            "{name}: the protected p was erased"
        );
    }
}

/// A resize can pull protected cells back from the scrollback onto a
/// screen an ED 2 found clean.
#[test]
fn a_resize_brings_back_protected_history() {
    let mut term = feed(2, 10, b"\x1bVp\x1bW\r\n\r\n\r\n");
    assert_eq!(term.grid().scrollback_len(), 2);
    term.advance(b"\x1b[2J");
    term.resize(6, 10);
    let pulled = term
        .grid()
        .visible_lines()
        .iter()
        .any(|l| l.cells.iter().any(Cell::is_protected));
    assert!(pulled, "the taller screen shows the history again");
    term.advance(b"\x1b[2J");
    let kept = term
        .grid()
        .visible_lines()
        .iter()
        .any(|l| l.text().contains('p'));
    assert!(kept, "ED 2 after the resize erased a protected cell");
}

/// A selective erase of a whole line drops its shell-integration marks
/// even when protected cells stay on it, as the plain erase does.
#[test]
fn a_selective_full_line_erase_drops_the_marks() {
    for (setup, erase) in [
        (&b"\x1bVp\x1bW"[..], &b"\x1b[2J"[..]),
        (b"\x1b[1\"qp\x1b[0\"q", b"\x1b[?2J"),
    ] {
        let mut term = feed(3, 10, b"\x1b]133;A\x1b\\");
        term.advance(setup);
        assert_eq!(term.grid().line(0).expect("row").marks.len(), 1);
        term.advance(erase);
        let line = term.grid().line(0).expect("row");
        assert_eq!(line.text(), "p", "{erase:?}");
        assert!(line.marks.is_empty(), "{erase:?} kept the mark");
    }
}

/// DECSED 3 is ED 3: the scrollback goes, the screen and a pending wrap
/// stay (xterm's DECSED shares `do_erase_display`).
#[test]
fn decsed_3_clears_the_scrollback_like_ed_3() {
    let mut term = feed(2, 4, b"1\r\n2\r\n3\r\nabcd");
    assert_eq!(term.grid().scrollback_len(), 2);
    assert!(term.grid().pending_wrap());
    term.advance(b"\x1b[?3J");
    assert_eq!(term.grid().scrollback_len(), 0);
    assert!(term.grid().pending_wrap());
    assert_eq!(render_screen(&term), "3\nabcd|\n");
}

/// DECSCA's colon form is ignored as a whole, as every mode sequence's is
/// (ADR 0020).
#[test]
fn decsca_with_sub_parameters_is_ignored() {
    let term = feed(2, 10, b"\x1b[1:2\"q");
    assert_eq!(term.protection_mode(), "off");
    assert!(!term.grid().pen().has(Style::PROTECTED));
}

/// S6: DECSED 2 over a full screen of protected cells costs O(cells), no
/// worse than ED 2 by more than a constant: a flood of each over the same
/// screen is timed (release builds only, where the timing means
/// something).
#[test]
#[cfg_attr(debug_assertions, ignore = "timing: release only")]
fn decsed_flood_over_protected_cells_is_linear() {
    use std::time::Instant;
    let (rows, cols) = (50u16, 200u16);
    let fill: Vec<u8> = std::iter::repeat_n(b'x', usize::from(rows) * usize::from(cols)).collect();
    let time = |setup: &[u8], erase: &[u8], n: usize| {
        let mut term = feed(rows, cols, setup);
        term.advance(&fill);
        let flood = erase.repeat(n);
        let start = Instant::now();
        term.advance(&flood);
        start.elapsed()
    };
    let n = 2_000;
    let plain = time(b"", b"\x1b[2J", n);
    let protected = time(b"\x1b[1\"q", b"\x1b[?2J", n);
    // Both are one pass over the screen per erase; the protected one runs
    // fill per run of unprotected cells (none here) after a scan.
    assert!(
        protected < plain * 8 + std::time::Duration::from_millis(50),
        "DECSED 2 over protected cells {protected:?} vs ED 2 {plain:?}"
    );
}

/// S6 with the worst shape for a selective erase: every other cell of a
/// full screen protected, each one a cluster in a hyperlink, so every row
/// has `cols / 2` runs and a full grapheme and link table. DECSED 2 and an
/// ISO-mode ED 2 repeated over it must stay within a constant of ED 2
/// repeated over the same screen printed unprotected: one pass over the
/// cells and one over the table per row, not one table edit per run. The
/// bound is a ratio of best-of-five times measured back to back in this
/// process, so machine load cancels out (release builds only).
#[test]
#[cfg_attr(debug_assertions, ignore = "timing: release only")]
fn selective_erase_over_interleaved_clusters_is_like_ed_2() {
    use std::time::{Duration, Instant};
    let (rows, cols) = (50u16, 200u16);
    let screen = |protect: bool| {
        let (on, off): (&[u8], &[u8]) = if protect {
            (b"\x1bV", b"\x1bW")
        } else {
            (b"", b"")
        };
        let mut out = b"\x1b]8;;https://a.example/\x1b\\".to_vec();
        for _ in 0..usize::from(rows) * usize::from(cols) / 2 {
            out.extend_from_slice(on);
            out.extend_from_slice("e\u{301}".as_bytes());
            out.extend_from_slice(off);
            out.extend_from_slice("e\u{301}".as_bytes());
        }
        out.extend_from_slice(b"\x1b]8;;\x1b\\");
        out
    };
    let best = |setup: &[u8], erase: &[u8]| {
        let flood = erase.repeat(200);
        (0..5)
            .map(|_| {
                let mut term = feed(rows, cols, setup);
                // The first erase drops the unprotected text; time the rest.
                term.advance(erase);
                let start = Instant::now();
                term.advance(&flood);
                start.elapsed()
            })
            .min()
            .expect("five runs")
    };
    let plain = best(&screen(false), b"\x1b[2J");
    let mut dec = screen(true);
    dec.extend_from_slice(b"\x1b[0\"q");
    let decsed = best(&dec, b"\x1b[?2J");
    let iso = best(&screen(true), b"\x1b[2J");
    for (name, took) in [("DECSED 2", decsed), ("ISO ED 2", iso)] {
        eprintln!("{name}: {took:?} for 200, ED 2 {plain:?}");
        assert!(
            took < plain * 6 + Duration::from_millis(2),
            "{name} over interleaved protected clusters {took:?} vs ED 2 {plain:?}"
        );
    }
}

// ------------------------------------------------------------- properties

mod properties {
    use proptest::prelude::*;
    use tessera_term::Terminal;

    /// The ISO-mode erases checked: the whole screen, the top row twice.
    const ISO_ERASES: &[&[u8]] = &[b"\x1b[2J", b"\x1b[H\x1b[2K", b"\x1b[H\x1b[99X"];

    /// Input pieces that print, protect, move, erase, save and restore,
    /// switch screens and reset.
    const PIECES: &[&[u8]] = &[
        b"a",
        b"bc",
        b" ",
        b"\xe4\xb8\xad",
        b"e\xcc\x81",
        b"\r\n",
        b"\r",
        b"\n",
        b"\x08",
        b"\x1bV",
        b"\x1bW",
        b"\x96",
        b"\x97",
        b"\x1b G",
        b"\x1b F",
        b"\x1b[1\"q",
        b"\x1b[0\"q",
        b"\x1b[2\"q",
        b"\x1b[7\"q",
        b"\x1b[J",
        b"\x1b[1J",
        b"\x1b[2J",
        b"\x1b[3J",
        b"\x1b[K",
        b"\x1b[1K",
        b"\x1b[2K",
        b"\x1b[X",
        b"\x1b[3X",
        b"\x1b[?J",
        b"\x1b[?1J",
        b"\x1b[?2J",
        b"\x1b[?K",
        b"\x1b[?1K",
        b"\x1b[?2K",
        b"\x1b[@",
        b"\x1b[P",
        b"\x1b[L",
        b"\x1b[M",
        b"\x1b[S",
        b"\x1b[T",
        b"\x1b[H",
        b"\x1b[2;3H",
        b"\x1b[1;9H",
        b"\x1b7",
        b"\x1b8",
        b"\x1b[s",
        b"\x1b[u",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[?47h",
        b"\x1b[?47l",
        b"\x1b[?1048h",
        b"\x1b[?1048l",
        b"\x1b[!p",
        b"\x1b[64;1\"p",
        b"\x1bc",
        b"\x1b[0m",
        b"\x1b[41m",
        b"\x1b#8",
    ];

    #[derive(Debug, Clone)]
    enum Op {
        Feed(Vec<u8>),
        Resize(u16, u16),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            12 => prop::collection::vec(prop::sample::select(PIECES), 1..6)
                .prop_map(|pieces| Op::Feed(pieces.concat())),
            1 => (1u16..7, 1u16..12).prop_map(|(rows, cols)| Op::Resize(rows, cols)),
        ]
    }

    /// The protected cells of the active screen, by position.
    fn protected(term: &Terminal) -> Vec<(usize, usize, char)> {
        let mut out = Vec::new();
        for (row, line) in term.grid().visible_lines().iter().enumerate() {
            for (col, cell) in line.cells.iter().enumerate() {
                if cell.is_protected() {
                    out.push((row, col, cell.ch));
                }
            }
        }
        out
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1024))]

        /// The "screen holds no protected cell" flag never lies: after any
        /// history, DECSCA 0 (DEC mode, the pen not protecting, which does
        /// not reset the flag) and DECSED 2 keep exactly the protected cells
        /// the screen held and blank every other one (the reference: a
        /// selective erase that always scans).
        #[test]
        fn decsed_after_any_history_keeps_every_protected_cell(
            ops in prop::collection::vec(op(), 1..30),
        ) {
            let mut term = Terminal::new(4, 8);
            for op in &ops {
                match op {
                    Op::Feed(bytes) => term.advance(bytes),
                    Op::Resize(rows, cols) => term.resize(*rows, *cols),
                }
            }
            term.advance(b"\x1b[0\"q");
            let before = protected(&term);
            term.advance(b"\x1b[?2J");
            prop_assert_eq!(protected(&term), before.clone());
            for (row, line) in term.grid().visible_lines().iter().enumerate() {
                for (col, cell) in line.cells.iter().enumerate() {
                    let kept = before.iter().any(|&(r, c, _)| (r, c) == (row, col));
                    prop_assert!(
                        kept || cell.ch == ' ' && !cell.is_wide(),
                        "({}, {}) {:?} survived unprotected", row, col, cell
                    );
                }
            }
            term.grid().check_invariants();
        }
    }

    /// Generated histories that run [`iso_erase_keeps_every_protected_cell`],
    /// and the share of them that must end in ISO mode (at least 1 in 2).
    const ISO_CASES: u32 = 1024;

    /// The ISO-mode half of the DECSED property: for a history that ends
    /// in ISO mode, EPA (which does not reset the flag) stops the pen
    /// protecting, and ED 2, EL 2 or ECH keep exactly the protected cells
    /// they cover. Returns whether the history ended in ISO mode (the
    /// others check nothing here: the DECSED property covers DEC mode).
    fn iso_erase_keeps_every_protected_cell(
        ops: &[Op],
        spa_last: bool,
        erase: &[u8],
    ) -> Result<bool, TestCaseError> {
        let mut term = Terminal::new(4, 8);
        for op in ops {
            match op {
                Op::Feed(bytes) => term.advance(bytes),
                Op::Resize(rows, cols) => term.resize(*rows, *cols),
            }
        }
        // A trailing SPA switches to ISO mode without printing a cell, so
        // the screen and the flag are still the ones the history left.
        if spa_last {
            term.advance(b"\x1bV");
        }
        if term.protection_mode() != "iso" {
            return Ok(false);
        }
        term.advance(b"\x1bW");
        let covered = |term: &Terminal| -> Vec<_> {
            protected(term)
                .into_iter()
                .filter(|&(r, _, _)| erase == b"\x1b[2J" || r == 0)
                .collect()
        };
        let before = covered(&term);
        term.advance(erase);
        prop_assert_eq!(covered(&term), before);
        Ok(true)
    }

    /// [`iso_erase_keeps_every_protected_cell`] over [`ISO_CASES`]
    /// histories, of which at least half must reach ISO mode (#257: a
    /// history ends in ISO mode by itself only about once in seven, so
    /// three in four end with an SPA).
    #[test]
    fn iso_erases_after_any_history_keep_every_protected_cell() {
        let strategy = (
            prop::collection::vec(op(), 1..30),
            prop::bool::weighted(0.75),
            prop::sample::select(ISO_ERASES),
        );
        let iso = std::cell::Cell::new(0u32);
        let mut runner =
            proptest::test_runner::TestRunner::new(ProptestConfig::with_cases(ISO_CASES));
        runner
            .run(&strategy, |(ops, spa_last, erase)| {
                if iso_erase_keeps_every_protected_cell(&ops, spa_last, erase)? {
                    iso.set(iso.get() + 1);
                }
                Ok(())
            })
            .unwrap_or_else(|failure| panic!("{failure}"));
        assert!(
            iso.get() * 2 >= ISO_CASES,
            "only {} of {ISO_CASES} histories reached ISO mode",
            iso.get()
        );
    }
}
