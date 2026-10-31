//! DECSCL (`CSI Pl ; Pc " p`, select conformance level).
//!
//! Expected behaviour comes from `tests/fixtures/decscl_spec.txt`, a
//! fixture in the `decstr_spec.txt` layout that cites xterm's `CASE_DECSCL`
//! (the oracle), ctlseqs and DEC's VT510 manual per case. Each case is
//! checked whole and byte by byte with the shared [`spec::check_case`]
//! (screen, cursor, pending wrap, requests, scrollback), and then on the
//! rows the shared reader ignores because the screen cannot show them:
//!
//! - `pen:` the grid's pen after the script: `default` or `not default`;
//! - `cellpen:` the style of one cell, `(row,col)=default` or
//!   `(row,col)=fg=indexed(N)`;
//! - `region:` the scroll region, `full` or `top..=bottom`;
//! - `modes:` `field=bool` pairs for the [`Terminal::modes`] fields named;
//! - `level:` [`Terminal::conformance_level`];
//! - `c1:` [`Terminal::c1_8bit_replies`], `7bit` or `8bit`;
//! - `flags: control` marks a case that runs without DECSCL to prove its
//!   neighbour's setup really produces the state DECSCL resets, and
//!   `flags: negative` one that sends only near misses, which must not
//!   reset anything or change the level.
//!
//! [`every_case_holds`] runs every case in one test, listed by the
//! behaviour it pins; [`every_case_is_tested_once`] keeps the lists and the
//! fixture in step.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;
#[allow(dead_code)] // Shared with tests/csi.rs and tests/vttest.rs.
mod spec;

use std::collections::HashSet;

use common::{Chunking, Script, Step};
use spec::{Case, check_case, parse_fixture};
use tessera_grid::{Color, Point, ScrollRegion, Style};
use tessera_term::{Request, Terminal};

const FIXTURE: &str = include_str!("fixtures/decscl_spec.txt");
const FIXTURE_NAME: &str = "fixtures/decscl_spec.txt";

// ------------------------------------------------------------- extra rows

/// The rows of one case that [`spec::parse_fixture`] ignores.
#[derive(Debug, Default)]
struct Extra {
    pen: Option<String>,
    cellpen: Option<(Point, String)>,
    region: Option<String>,
    modes: Option<String>,
    level: Option<u8>,
    c1: Option<String>,
    flags: Vec<String>,
}

/// The extra rows of case `id`, read straight from the fixture text.
fn extra(id: &str) -> Extra {
    let mut lines = FIXTURE
        .lines()
        .skip_while(|line| line.strip_prefix("### ") != Some(id));
    assert!(lines.next().is_some(), "{id}: no such case");
    let mut extra = Extra::default();
    for line in lines.take_while(|l| !l.starts_with("### ") && !l.starts_with("# --")) {
        let Some((key, raw)) = line.split_once(": ") else {
            continue;
        };
        let raw = raw.trim();
        match key {
            "pen" => extra.pen = Some(raw.to_owned()),
            "cellpen" => {
                let (at, style) = raw.split_once('=').expect("cellpen is `(r,c)=style`");
                let (row, col) = at
                    .trim_start_matches('(')
                    .trim_end_matches(')')
                    .split_once(',')
                    .expect("cellpen position is `(r,c)`");
                let at = Point::new(
                    row.trim().parse().expect("cellpen row"),
                    col.trim().parse().expect("cellpen col"),
                );
                extra.cellpen = Some((at, style.trim().to_owned()));
            }
            "region" => extra.region = Some(raw.to_owned()),
            "modes" => extra.modes = Some(raw.to_owned()),
            "level" => extra.level = Some(raw.parse().expect("level is a number")),
            "c1" => extra.c1 = Some(raw.to_owned()),
            "flags" => extra.flags = raw.split_whitespace().map(str::to_owned).collect(),
            _ => {}
        }
    }
    extra
}

/// A `cellpen:` style: `default` or `fg=indexed(N)` on top of it.
fn parse_style(text: &str) -> Style {
    let mut style = Style::DEFAULT;
    if text == "default" {
        return style;
    }
    let n = text
        .strip_prefix("fg=indexed(")
        .and_then(|n| n.strip_suffix(')'))
        .unwrap_or_else(|| panic!("unknown cellpen `{text}`"));
    style.fg = Color::Indexed(n.parse().expect("indexed colour"));
    style
}

/// A `region:` row: `full` or `top..=bottom`, 0-based.
fn parse_region(text: &str, rows: u16) -> ScrollRegion {
    if text == "full" {
        return ScrollRegion {
            top: 0,
            bottom: rows - 1,
        };
    }
    let (top, bottom) = text.split_once("..=").expect("region is `a..=b`");
    ScrollRegion {
        top: top.parse().expect("region top"),
        bottom: bottom.parse().expect("region bottom"),
    }
}

/// Mismatches on the extra rows.
fn check_extra(case: &Case, extra: &Extra, term: &Terminal, chunking: Chunking) -> Vec<String> {
    let grid = term.grid();
    let mut errors = Vec::new();
    let id = &case.id;
    match (extra.pen.as_deref(), grid.pen() == Style::DEFAULT) {
        (None, _) | (Some("default"), true) | (Some("not default"), false) => {}
        (Some("default" | "not default"), _) => errors.push(format!(
            "{id} ({chunking:?}) pen: expected {:?}, got {:?}",
            extra.pen,
            grid.pen()
        )),
        (Some(other), _) => panic!("{id}: unknown pen `{other}`"),
    }
    if let Some((at, style)) = &extra.cellpen {
        let want = parse_style(style);
        let got = grid
            .line(at.row)
            .and_then(|line| line.cells.get(usize::from(at.col)))
            .map(|cell| cell.style);
        if got != Some(want) {
            errors.push(format!(
                "{id} ({chunking:?}) cellpen {at:?}: expected {want:?}, got {got:?}"
            ));
        }
    }
    if let Some(region) = &extra.region {
        let want = parse_region(region, grid.rows());
        if grid.scroll_region() != want {
            errors.push(format!(
                "{id} ({chunking:?}) region: expected {want:?}, got {:?}",
                grid.scroll_region()
            ));
        }
    }
    let modes = term.modes();
    for pair in extra.modes.iter().flat_map(|text| text.split_whitespace()) {
        let (field, want) = pair.split_once('=').expect("modes is `field=bool ...`");
        let got = match field {
            "origin" => modes.origin,
            "insert" => modes.insert,
            "auto_wrap" => modes.auto_wrap,
            "cursor_visible" => modes.cursor_visible,
            other => panic!("{id}: unknown mode field `{other}`"),
        };
        if got.to_string() != want {
            errors.push(format!(
                "{id} ({chunking:?}) modes {field}: expected {want}, got {got}"
            ));
        }
    }
    let level = extra
        .level
        .unwrap_or_else(|| panic!("{id}: every case states its `level:`"));
    if term.conformance_level() != level {
        errors.push(format!(
            "{id} ({chunking:?}) level: expected {level}, got {}",
            term.conformance_level()
        ));
    }
    let eight_bit = match extra.c1.as_deref() {
        Some("8bit") => true,
        Some("7bit") => false,
        other => panic!("{id}: `c1:` must be 7bit or 8bit, got {other:?}"),
    };
    if term.c1_8bit_replies() != eight_bit {
        errors.push(format!(
            "{id} ({chunking:?}) c1: expected 8-bit {eight_bit}, got {}",
            term.c1_8bit_replies()
        ));
    }
    errors
}

// ----------------------------------------------------------------- driver

/// Whether the script's bytes, all feeds joined, contain a valid DECSCL:
/// `ESC [`, a level 61..=65, then digits and `;` only, then `" p`.
fn sends_decscl(script: &Script) -> bool {
    let bytes: Vec<u8> = script
        .steps
        .iter()
        .filter_map(|step| match step {
            Step::Feed(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    (0..bytes.len()).any(|start| {
        let Some(rest) = bytes[start..].strip_prefix(b"\x1b[") else {
            return false;
        };
        let level_len = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        let level_ok = std::str::from_utf8(&rest[..level_len])
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
            .is_some_and(|level| (61..=65).contains(&level));
        let params = rest
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b';')
            .count();
        level_ok && rest[params..].starts_with(b"\"p")
    })
}

/// Every case in the fixture.
fn cases() -> Vec<Case> {
    parse_fixture(FIXTURE)
}

/// Checks case `id` whole and byte by byte, every row, and panics with
/// all its mismatches at once. A `control` or `negative` case must not
/// send a valid DECSCL and every other case must, so a control never
/// silently tests DECSCL and a DECSCL case never silently tests nothing.
fn check(cases: &[Case], id: &str) {
    let mut errors = Vec::new();
    let case = cases
        .iter()
        .find(|case| case.id == id)
        .unwrap_or_else(|| panic!("{id}: no such case"));
    let extra = extra(id);
    let without = extra
        .flags
        .iter()
        .any(|flag| flag == "control" || flag == "negative");
    assert_ne!(
        sends_decscl(&case.script),
        without,
        "{id}: control and negative cases run without DECSCL, every other case with it"
    );
    assert!(
        case.requests.is_empty(),
        "{id}: DECSCL is silent (lead decision D3), every case expects `requests: []`"
    );
    for chunking in [Chunking::Whole, Chunking::Bytewise] {
        errors.extend(check_case(case, chunking));
        let (term, requests) = case.script.run(chunking);
        errors.extend(check_extra(case, &extra, &term, chunking));
        // DECSCL itself replies nothing; the only query among the cases is
        // NEAR01's `CSI 62 $ p`, a DECRQM of the unknown ANSI mode 62 (#10).
        let (replies, _) = common::split_replies(&requests);
        let want: &[u8] = if id.starts_with("NEAR01_") {
            b"\x1b[62;0$y"
        } else {
            b""
        };
        if replies != want {
            errors.push(format!(
                "{id} ({chunking:?}) replies: expected {}, got {}",
                want.escape_ascii(),
                replies.escape_ascii()
            ));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// Lists the cases [`every_case_holds`] runs, grouped by the behaviour they
/// pin: `group: [ids]`.
macro_rules! decscl_tests {
    ($($name:ident: [$($id:literal),* $(,)?]),* $(,)?) => {
        /// Every case, as `(group, ids)`.
        const GROUPS: &[(&str, &[&str])] = &[$((stringify!($name), &[$($id),*])),*];
    };
}

decscl_tests! {
    a_new_terminal_is_level_4_with_7bit_replies: [
        "DEFAULT01_new_terminal_is_level_4_7bit",
    ],
    level_1_forces_7bit_replies: [
        "LEVEL61_no_pc",
        "LEVEL61_pc_2_still_7bit",
        "LEVEL61_pc_0_still_7bit",
    ],
    levels_2_to_5_take_pc: [
        "LEVEL62_no_pc_is_8bit",
        "LEVEL62_pc_1_is_7bit",
        "LEVEL62_pc_0_is_8bit",
        "LEVEL62_pc_2_is_8bit",
        "LEVEL63_no_pc_is_8bit",
        "LEVEL63_pc_1_is_7bit",
        "LEVEL64_no_pc_is_8bit",
        "LEVEL64_pc_1_is_7bit",
        "LEVEL65_no_pc_is_8bit",
        "LEVEL65_pc_1_is_7bit",
    ],
    unknown_pc_keeps_the_c1_mode: [
        "PC01_pc_3_keeps_8bit",
        "PC02_pc_99_keeps_7bit",
    ],
    parameters_after_pc_are_ignored: [
        "EXTRA01_parameter_after_pc_ignored",
    ],
    decscl_runs_the_soft_reset: [
        "RESET01_pen_region_modes_reset",
        "RESET02_control_setup_without_decscl",
        "RESET03_autowrap_back_on",
        "RESET04_screen_scrollback_and_position_kept",
        "SAVED01_decrc_after_decscl_homes",
        "SAVED02_control_decrc_without_decscl",
        "REP01_nothing_to_repeat_after_decscl",
    ],
    without_a_valid_level_nothing_happens: [
        "NOLEVEL01_bare_csi_quote_p_does_nothing",
        "NOLEVEL02_levels_outside_61_65_do_nothing",
    ],
    near_misses_are_not_decscl: [
        "NEAR01_other_intermediates_finals_and_subparams",
        "NEAR02_decstr_with_level_param_keeps_level",
        "CANCEL01_can_aborts_decscl",
    ],
    the_level_persists: [
        "KEEP01_decstr_keeps_level_and_c1",
        "KEEP02_ris_keeps_level_and_turns_8bit_off",
        "KEEP03_last_decscl_wins",
    ],
}

/// Every listed case holds. One test runs the whole table (one process
/// instead of one per group) and reports every failing case as
/// `group/id`.
#[test]
fn every_case_holds() {
    let cases = cases();
    let rows = GROUPS
        .iter()
        .flat_map(|(group, ids)| ids.iter().map(move |id| (format!("{group}/{id}"), *id)));
    common::check_table(FIXTURE_NAME, cases.len(), rows, |id| check(&cases, id));
}

#[test]
fn every_case_is_tested_once() {
    let ids: Vec<String> = cases().into_iter().map(|case| case.id).collect();
    let tested: Vec<&str> = GROUPS
        .iter()
        .flat_map(|(_, ids)| ids.iter().copied())
        .collect();
    let unique: HashSet<&str> = tested.iter().copied().collect();
    assert_eq!(unique.len(), tested.len(), "a case is listed twice");
    let fixture: HashSet<&str> = ids.iter().map(String::as_str).collect();
    assert_eq!(unique, fixture, "test lists and fixture cases differ");
}

// ----------------------------------------------------------- direct tests

/// Every level byte DECSCL can store: `CSI 6n " p` for n in 1..=5 gives n,
/// and every other first parameter up to 1000 leaves the level alone.
#[test]
fn every_first_parameter_up_to_1000() {
    for first in 0..=1000_u16 {
        let mut term = Terminal::new(1, 5);
        term.advance(b"\x1b[31m");
        term.advance(format!("\x1b[{first};2\"p").as_bytes());
        let valid = (61..=65).contains(&first);
        let want = if valid { first - 60 } else { 4 };
        assert_eq!(
            u16::from(term.conformance_level()),
            want,
            "CSI {first};2\"p"
        );
        assert_eq!(
            term.c1_8bit_replies(),
            valid && first != 61,
            "CSI {first};2\"p"
        );
        assert_eq!(
            term.grid().pen() == Style::DEFAULT,
            valid,
            "CSI {first};2\"p"
        );
    }
}

/// A flood of 200 000 DECSCL emits nothing and leaves only the last one's
/// effect: no request, nothing new on the screen, the cursor where it
/// was, the last level and C1 mode. (Its cost is the bench case
/// `quote_p_noop`'s concern, not this test's.)
#[test]
fn a_flood_of_decscl_requests_nothing() {
    let mut term = Terminal::new(24, 80);
    term.advance(b"text");
    term.advance(&b"\x1b[31m\x1b[62;2\"p\x1b[63;1\"p".repeat(100_000));
    assert_eq!(term.take_requests(), Vec::<Request>::new());
    assert_eq!(term.grid().pen(), Style::DEFAULT);
    assert_eq!(term.grid().cursor(), Point::new(0, 4));
    assert_eq!(term.conformance_level(), 3);
    assert!(!term.c1_8bit_replies());
}
