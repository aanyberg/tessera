//! DECSTR (`CSI ! p`, soft terminal reset).
//!
//! Expected behaviour comes from `tests/fixtures/decstr_spec.txt`, a
//! fixture in the `csi_spec.txt` layout that cites xterm's `ReallyReset`
//! (the oracle), DEC's VT510 table 5-9, kitty and `WezTerm` per case. Each
//! case is checked whole and byte by byte with the shared
//! [`spec::check_case`] (screen, cursor, pending wrap, requests,
//! scrollback), and then on the rows the shared reader ignores because the
//! screen cannot show them:
//!
//! - `pen:` the grid's pen after the script: `default` or `not default`;
//! - `cellpen:` the style of one cell, `(row,col)=default` or a list of
//!   `bold`, `fg=indexed(N)`;
//! - `region:` the scroll region, `full` or `top..=bottom`;
//! - `modes:` `field=bool` pairs for the [`Terminal::modes`] fields
//!   named (#8);
//! - `kbd:` [`Terminal::keyboard_modes`], `default` or `not default`;
//! - `flags: control` marks a case that runs without DECSTR to prove its
//!   neighbour's setup really produces the state DECSTR resets, and
//!   `flags: negative` one that sends a near miss (another intermediate, a
//!   cancelled sequence) that must not reset anything.
//!
//! [`every_case_holds`] runs every case in one test, listed by the reset
//! item it pins; [`every_case_is_tested_once`] keeps the lists and the
//! fixture in step.
//! Rows after the fixture's `#>` marker wait on state that does not exist
//! yet and are not cases.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;
#[allow(dead_code)] // Shared with tests/csi.rs and tests/vttest.rs.
mod spec;

use std::collections::HashSet;

use common::{Chunking, Script, Step};
use spec::{Case, check_case, parse_fixture};
use tessera_grid::{Color, Point, ScrollRegion, Style};
use tessera_term::{KeyboardModes, MouseEncoding, MouseTracking, Request, Terminal};

const FIXTURE: &str = include_str!("fixtures/decstr_spec.txt");
const FIXTURE_NAME: &str = "fixtures/decstr_spec.txt";

// ------------------------------------------------------------- extra rows

/// The rows of one case that [`spec::parse_fixture`] ignores.
#[derive(Debug, Default)]
struct Extra {
    pen: Option<String>,
    cellpen: Option<(Point, String)>,
    region: Option<String>,
    modes: Option<String>,
    kbd: Option<String>,
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
            "kbd" => extra.kbd = Some(raw.to_owned()),
            "flags" => extra.flags = raw.split_whitespace().map(str::to_owned).collect(),
            _ => {}
        }
    }
    extra
}

/// A `cellpen:` style: `default`, or space-separated `bold`, `curly`,
/// `fg=indexed(N)` and `ul=rgb(R,G,B)` on top of the default.
fn parse_style(text: &str) -> Style {
    let mut style = Style::DEFAULT;
    if text == "default" {
        return style;
    }
    for word in text.split_whitespace() {
        if word == "bold" {
            style.flags |= Style::BOLD;
        } else if word == "curly" {
            style.flags |= Style::UNDERLINE | Style::CURLY_UNDERLINE;
        } else if let Some(rgb) = word
            .strip_prefix("ul=rgb(")
            .and_then(|rgb| rgb.strip_suffix(')'))
        {
            let c: Vec<u8> = rgb
                .split(',')
                .map(|c| c.parse().expect("rgb component"))
                .collect();
            style.underline = Color::Rgb(c[0], c[1], c[2]);
        } else if let Some(n) = word
            .strip_prefix("fg=indexed(")
            .and_then(|n| n.strip_suffix(')'))
        {
            style.fg = Color::Indexed(n.parse().expect("indexed colour"));
        } else {
            panic!("unknown cellpen word `{word}`");
        }
    }
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
        (Some("default"), false) => errors.push(format!(
            "{id} ({chunking:?}) pen: expected default, got {:?}",
            grid.pen()
        )),
        (Some("not default"), true) => {
            errors.push(format!("{id} ({chunking:?}) pen: expected not default"));
        }
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
            "cursor_visible" => modes.cursor_visible,
            "cursor_blink" => modes.cursor_blink,
            "synchronized_output" => modes.synchronized_output,
            "reverse_video" => modes.reverse_video,
            "linefeed_newline" => modes.linefeed_newline,
            "alt_screen" => modes.alt_screen,
            "bracketed_paste" => modes.bracketed_paste,
            "focus_events" => modes.focus_events,
            other => panic!("{id}: unknown mode field `{other}`"),
        };
        if got.to_string() != want {
            errors.push(format!(
                "{id} ({chunking:?}) modes {field}: expected {want}, got {got}"
            ));
        }
    }
    if extra
        .modes
        .iter()
        .any(|text| text.contains("bracketed_paste=true"))
        && (modes.mouse, modes.mouse_encoding) != (MouseTracking::Normal, MouseEncoding::Sgr)
    {
        errors.push(format!(
            "{id} ({chunking:?}) mouse modes not kept: {:?}",
            (modes.mouse, modes.mouse_encoding)
        ));
    }
    let kbd_default = term.keyboard_modes() == KeyboardModes::default();
    match (extra.kbd.as_deref(), kbd_default) {
        (None, _) | (Some("default"), true) | (Some("not default"), false) => {}
        (Some("default" | "not default"), _) => errors.push(format!(
            "{id} ({chunking:?}) kbd: expected {:?}, got {:?}",
            extra.kbd,
            term.keyboard_modes()
        )),
        (Some(other), _) => panic!("{id}: unknown kbd `{other}`"),
    }
    errors
}

// ----------------------------------------------------------------- driver

/// Whether the script's bytes, all feeds joined, contain DECSTR: `ESC [`,
/// digits and `;` only, then `! p`.
fn sends_decstr(script: &Script) -> bool {
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
        let params = rest
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b';')
            .count();
        rest[params..].starts_with(b"!p")
    })
}

/// Every case in the fixture.
fn cases() -> Vec<Case> {
    parse_fixture(FIXTURE)
}

/// Checks case `id` whole and byte by byte, every row, and panics with
/// all its mismatches at once. A `control` or `negative` case must not
/// send DECSTR and every other case must, so a control never silently
/// tests DECSTR and a DECSTR case never silently tests nothing.
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
        sends_decstr(&case.script),
        without,
        "{id}: control and negative cases run without DECSTR, every other case with it"
    );
    for chunking in [Chunking::Whole, Chunking::Bytewise] {
        errors.extend(check_case(case, chunking));
        let (term, _) = case.script.run(chunking);
        errors.extend(check_extra(case, &extra, &term, chunking));
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// Lists the cases [`every_case_holds`] runs, grouped by the reset item they
/// pin: `group: [ids]`.
macro_rules! decstr_tests {
    ($($name:ident: [$($id:literal),* $(,)?]),* $(,)?) => {
        /// Every case, as `(group, ids)`.
        const GROUPS: &[(&str, &[&str])] = &[$((stringify!($name), &[$($id),*])),*];
    };
}

decstr_tests! {
    pen_resets_to_default: [
        "PEN01_all_sgr_state_reset",
        "PEN02_control_pen_set_without_decstr",
        "PEN03_faint_strike_double_underline",
        "PEN04_inverse_blink_hidden_italic",
        "PEN05_256_color_pen",
        "PEN06_underline_colour_and_shape",
        "PEN07_control_underline_colour_without_decstr",
    ],
    insert_mode_turns_off: [
        "IRM01_insert_mode_off",
        "IRM02_control_insert_mode_stays_on",
    ],
    auto_wrap_turns_on: [
        "AWM01_autowrap_restored_on",
        "AWM02_control_autowrap_off_overwrites",
        "AWM03_autowrap_already_on_stays_on",
    ],
    scroll_region_becomes_full_screen: [
        "REGION01_scroll_region_full",
        "REGION02_control_region_survives_without_decstr",
    ],
    saved_cursor_becomes_home_with_default_pen: [
        "SAVED01_decrc_after_decstr_homes_and_resets_pen",
        "SAVED02_control_decrc_without_decstr",
        "SAVED03_saved_pending_wrap_cleared",
        "SAVED04_decrc_without_prior_decsc_same_as_after_decstr",
    ],
    cursor_position_unchanged: [
        "CURSOR01_position_unchanged",
        "REGION03_cursor_not_homed",
    ],
    screen_text_and_scrollback_unchanged: [
        "CONTENT01_screen_and_scrollback_unchanged",
    ],
    pending_wrap_preserved: [
        "WRAP01_pending_wrap_survives",
        "WRAP02_next_glyph_wraps_after_decstr",
    ],
    tab_stops_unchanged: [
        "TAB01_custom_tab_stops_kept",
        "TAB02_cleared_tab_stops_stay_cleared",
    ],
    rep_has_nothing_to_repeat_after_decstr: [
        "REP01_decstr_clears_last_printed",
        "REP02_control_rep_without_decstr",
    ],
    other_intermediates_are_ignored: [
        "INTER01_other_intermediates_ignored_pen",
        "INTER02_other_intermediates_ignored_irm",
    ],
    parameters_are_ignored: [
        "INTER03_bang_with_params_still_resets",
        "INTER04_zero_param_also_resets",
    ],
    origin_mode_turns_off: [
        "DECOM01_origin_mode_off_cup_absolute",
        "DECOM02_control_origin_mode_stays_on",
        "DECOM03_saved_origin_mode_reset_too",
    ],
    cursor_becomes_visible: [
        "TCEM01_cursor_visible_again",
        "TCEM02_control_cursor_stays_hidden",
    ],
    cursor_blink_and_synchronized_output_turn_off: [
        "BLINK01_cursor_blink_and_sync_off",
        "BLINK02_control_blink_and_sync_stay_on",
    ],
    keyboard_modes_reset: [
        "KBD01_keyboard_modes_reset",
        "KBD02_control_keyboard_modes_stay_set",
    ],
    paste_focus_mouse_reverse_video_lnm_and_alt_screen_kept: [
        "KEEP01_paste_focus_mouse_reverse_lnm_alt_kept",
    ],
    split_and_cancelled_sequences: [
        "SPLIT01_decstr_split_across_feeds",
        "INTERRUPT01_can_aborts_decstr",
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

/// The control cases' pens are exactly what their SGR strings set, so the
/// reset cases start from known fields, not just "something". The brief's
/// PEN01 string sets no underline colour (`58:2::9` is too short, and the
/// `;9;9` after it are SGR 9); PEN06's all-colon string does.
#[test]
fn the_pen_controls_set_what_they_claim() {
    let mut term = Terminal::new(1, 5);
    term.advance(b"\x1b[1;4:3;31;48;2;1;2;3;58:2::9;9;9m");
    let pen = term.grid().pen();
    assert_eq!(pen.fg, Color::Indexed(1));
    assert_eq!(pen.bg, Color::Rgb(1, 2, 3));
    assert_eq!(pen.underline, Color::Default);
    assert_eq!(
        pen.flags,
        Style::BOLD | Style::UNDERLINE | Style::CURLY_UNDERLINE | Style::STRIKETHROUGH
    );

    let mut term = Terminal::new(1, 5);
    term.advance(b"\x1b[4:3;58:2::9:9:9m");
    let pen = term.grid().pen();
    assert_eq!(pen.underline, Color::Rgb(9, 9, 9));
    assert_eq!(pen.flags, Style::UNDERLINE | Style::CURLY_UNDERLINE);

    let mut term = Terminal::new(1, 5);
    term.advance(b"\x1b[2;9;21m");
    assert_eq!(
        term.grid().pen().flags,
        Style::DIM | Style::STRIKETHROUGH | Style::UNDERLINE | Style::DOUBLE_UNDERLINE,
        "PEN03 starts from faint, strikeout and double underline"
    );
}

/// DECSTR resets the modes the terminal exposes and leaves the grid's
/// contents alone; RIS (`ESC c`) is the reset that clears them.
#[test]
fn modes_reset_and_contents_kept() {
    let mut term = Terminal::new(3, 5);
    term.set_auto_wrap(false);
    term.set_insert_mode(true);
    term.advance(b"ab");
    term.advance(b"\x1b[!p");
    assert!(term.auto_wrap());
    assert!(!term.insert_mode());
    assert_eq!(
        term.grid().line(0).map(tessera_grid::Line::text).as_deref(),
        Some("ab")
    );
    assert_eq!(term.grid().cursor(), Point::new(0, 2));
}

/// A flood of DECSTR costs O(1) per sequence and emits nothing.
#[test]
fn a_flood_of_decstr_requests_nothing() {
    let mut term = Terminal::new(24, 80);
    term.advance(b"text");
    term.advance(&b"\x1b[31m\x1b[!p".repeat(100_000));
    assert_eq!(term.take_requests(), Vec::<Request>::new());
    assert_eq!(term.grid().pen(), Style::DEFAULT);
    assert_eq!(term.grid().cursor(), Point::new(0, 4));
}
