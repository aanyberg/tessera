//! Terminal modes: DECSET/DECRST, SM/RM, DECKPAM/DECKPNM, XTMODKEYS and
//! the kitty keyboard flag stack.
//!
//! Expected behaviour comes from `tests/fixtures/modes_spec.txt`, a
//! fixture in the `csi_spec.txt` layout that cites xterm's `dpmodes`,
//! `CursorSet`/`CursorSave`, `ReallyReset`, esctest2 and the kitty keyboard
//! protocol per case. Each case is checked whole and byte by byte with the
//! shared [`spec::check_case`] (screen, cursor, pending wrap, requests,
//! scrollback) and then on the rows the shared reader ignores because the
//! screen cannot show them:
//!
//! - `modes:` the fields of [`Terminal::modes`] that differ from
//!   [`Modes::default`] (`default` for none);
//! - `kbd:` the same for [`Terminal::keyboard_modes`];
//! - `region:` the scroll region, `full` or `top..=bottom`;
//! - `screen_is:` `main` or `alt`;
//! - `main:` the primary screen's rows while the alternate one is active;
//! - `size_after:` the grid size after the script (DECCOLM).
//!
//! [`every_case_holds`] runs every case in one test, listed by the mode it
//! pins; [`every_case_is_tested_once`] keeps the lists and the fixture in
//! step. The fixture's tables (the kitty stack, bracketed paste and focus,
//! which app-core tests) are not cases: the kitty table is checked by the
//! direct tests at the end.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;
#[allow(dead_code)] // Shared with tests/csi.rs and tests/vttest.rs.
mod spec;

use std::collections::HashSet;

use common::Chunking;
use spec::{Case, check_case, parse_fixture};
use tessera_grid::ScrollRegion;
use tessera_term::{
    KITTY_STACK_DEPTH, KeyboardModes, KittyFlags, Modes, ModifyOtherKeys, MouseEncoding,
    MouseTracking, Request, Terminal,
};

const FIXTURE: &str = include_str!("fixtures/modes_spec.txt");
const FIXTURE_NAME: &str = "fixtures/modes_spec.txt";

// ------------------------------------------------------------- extra rows

/// The rows of one case that [`spec::parse_fixture`] ignores.
#[derive(Debug, Default)]
struct Extra {
    modes: Option<String>,
    kbd: Option<String>,
    region: Option<String>,
    screen_is: Option<String>,
    main: Option<String>,
    size_after: Option<(u16, u16)>,
}

/// The extra rows of case `id`, read straight from the fixture text.
fn extra(id: &str) -> Extra {
    let mut lines = FIXTURE
        .lines()
        .skip_while(|line| line.strip_prefix("### ") != Some(id))
        .peekable();
    assert!(lines.next().is_some(), "{id}: no such case");
    let mut extra = Extra::default();
    while let Some(line) = lines.next_if(|l| !l.starts_with("### ") && !l.starts_with("# --")) {
        let (key, raw) = line
            .split_once(": ")
            .unwrap_or((line.trim_end_matches(':'), ""));
        let raw = raw.split_once('#').map_or(raw, |(v, _)| v).trim();
        match key {
            "modes" => extra.modes = Some(raw.to_owned()),
            "kbd" => extra.kbd = Some(raw.to_owned()),
            "region" => extra.region = Some(raw.to_owned()),
            "screen_is" => extra.screen_is = Some(raw.to_owned()),
            "size_after" => {
                let (rows, cols) = raw.split_once('x').expect("size_after is `RxC`");
                extra.size_after = Some((
                    rows.parse().expect("size_after rows"),
                    cols.parse().expect("size_after cols"),
                ));
            }
            "main" => {
                let mut main = String::new();
                while let Some(row) = lines.next_if(|l| l.starts_with('>')) {
                    main.push_str(row[1..].trim_end());
                    main.push('\n');
                }
                extra.main = Some(main);
            }
            _ => {}
        }
    }
    extra
}

fn parse_bool(field: &str, value: &str) -> bool {
    match value {
        "true" => true,
        "false" => false,
        other => panic!("{field}: `{other}` is not true or false"),
    }
}

/// A `modes:` row: `default`, or `field=value` pairs on top of the
/// default.
fn parse_modes(text: &str) -> Modes {
    let mut modes = Modes::default();
    if text == "default" {
        return modes;
    }
    for pair in text.split_whitespace() {
        let (field, value) = pair.split_once('=').expect("modes is `field=value ...`");
        match field {
            "origin" => modes.origin = parse_bool(field, value),
            "auto_wrap" => modes.auto_wrap = parse_bool(field, value),
            "cursor_visible" => modes.cursor_visible = parse_bool(field, value),
            "cursor_blink" => modes.cursor_blink = parse_bool(field, value),
            "reverse_video" => modes.reverse_video = parse_bool(field, value),
            "insert" => modes.insert = parse_bool(field, value),
            "linefeed_newline" => modes.linefeed_newline = parse_bool(field, value),
            "allow_column_change" => modes.allow_column_change = parse_bool(field, value),
            "columns_132" => modes.columns_132 = parse_bool(field, value),
            "alt_screen" => modes.alt_screen = parse_bool(field, value),
            "bracketed_paste" => modes.bracketed_paste = parse_bool(field, value),
            "focus_events" => modes.focus_events = parse_bool(field, value),
            "synchronized_output" => modes.synchronized_output = parse_bool(field, value),
            "mouse" => {
                modes.mouse = match value {
                    "None" => MouseTracking::None,
                    "X10" => MouseTracking::X10,
                    "Normal" => MouseTracking::Normal,
                    "Button" => MouseTracking::Button,
                    "AnyMotion" => MouseTracking::AnyMotion,
                    other => panic!("unknown mouse tracking `{other}`"),
                }
            }
            "mouse_encoding" => {
                modes.mouse_encoding = match value {
                    "Default" => MouseEncoding::Default,
                    "Utf8" => MouseEncoding::Utf8,
                    "Sgr" => MouseEncoding::Sgr,
                    "Urxvt" => MouseEncoding::Urxvt,
                    "SgrPixels" => MouseEncoding::SgrPixels,
                    other => panic!("unknown mouse encoding `{other}`"),
                }
            }
            other => panic!("unknown mode field `{other}`"),
        }
    }
    modes
}

/// A `kbd:` row: `default`, or `field=value` pairs on top of the default.
fn parse_kbd(text: &str) -> KeyboardModes {
    let mut kbd = KeyboardModes::default();
    if text == "default" {
        return kbd;
    }
    for pair in text.split_whitespace() {
        let (field, value) = pair.split_once('=').expect("kbd is `field=value ...`");
        match field {
            "application_cursor_keys" => kbd.application_cursor_keys = parse_bool(field, value),
            "application_keypad" => kbd.application_keypad = parse_bool(field, value),
            "modify_other_keys" => {
                kbd.modify_other_keys = match value {
                    "Disabled" => ModifyOtherKeys::Disabled,
                    "Level1" => ModifyOtherKeys::Level1,
                    "Level2" => ModifyOtherKeys::Level2,
                    other => panic!("unknown modifyOtherKeys level `{other}`"),
                }
            }
            "kitty_flags" => {
                kbd.kitty_flags =
                    KittyFlags::from_bits_truncate(value.parse().expect("kitty_flags bits"));
            }
            other => panic!("unknown kbd field `{other}`"),
        }
    }
    kbd
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

/// The primary screen's rows, trimmed, while the alternate one is active.
fn render_main(term: &Terminal) -> String {
    let mut grid = term.grid().clone();
    grid.leave_alt_screen();
    let mut main = String::new();
    for line in grid.visible_lines() {
        main.push_str(line.text().trim_end());
        main.push('\n');
    }
    main
}

/// Mismatches on the extra rows.
fn check_extra(case: &Case, extra: &Extra, term: &Terminal, chunking: Chunking) -> Vec<String> {
    let grid = term.grid();
    let id = &case.id;
    let mut errors = Vec::new();
    let mut expect = |what: &str, ok: bool, detail: String| {
        if !ok {
            errors.push(format!("{id} ({chunking:?}) {what}: {detail}"));
        }
    };
    let modes = extra.modes.as_deref().map(parse_modes);
    let modes = modes.unwrap_or_else(|| panic!("{id}: every case states its modes"));
    expect(
        "modes",
        term.modes() == modes,
        format!("expected {modes:?}\n got {:?}", term.modes()),
    );
    let kbd = parse_kbd(extra.kbd.as_deref().unwrap_or("default"));
    expect(
        "kbd",
        term.keyboard_modes() == kbd,
        format!("expected {kbd:?}, got {:?}", term.keyboard_modes()),
    );
    if let Some(region) = &extra.region {
        let want = parse_region(region, grid.rows());
        expect(
            "region",
            grid.scroll_region() == want,
            format!("expected {want:?}, got {:?}", grid.scroll_region()),
        );
    }
    if let Some(screen) = &extra.screen_is {
        let alt = match screen.as_str() {
            "alt" => true,
            "main" => false,
            other => panic!("{id}: unknown screen `{other}`"),
        };
        expect(
            "screen_is",
            grid.is_alt_screen() == alt,
            format!("expected {screen}"),
        );
    }
    if let Some(main) = &extra.main {
        let got = render_main(term);
        expect(
            "main",
            got == *main,
            format!("\nexpected:\n{main}got:\n{got}"),
        );
    }
    let size = (grid.rows(), grid.cols());
    let want = extra
        .size_after
        .unwrap_or((case.script.rows, case.script.cols));
    expect(
        "size",
        size == want,
        format!("expected {want:?}, got {size:?}"),
    );
    errors
}

// ----------------------------------------------------------------- driver

fn cases() -> Vec<Case> {
    parse_fixture(FIXTURE)
}

/// Checks case `id` whole and byte by byte, every row, and panics with
/// all its mismatches at once.
fn check(cases: &[Case], id: &str) {
    let mut errors = Vec::new();
    let case = cases
        .iter()
        .find(|case| case.id == id)
        .unwrap_or_else(|| panic!("{id}: no such case"));
    let extra = extra(id);
    for chunking in [Chunking::Whole, Chunking::Bytewise] {
        errors.extend(check_case(case, chunking));
        let (term, _) = case.script.run(chunking);
        errors.extend(check_extra(case, &extra, &term, chunking));
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// Lists the cases [`every_case_holds`] runs, grouped by the mode they
/// pin: `group: [ids]`.
macro_rules! mode_tests {
    ($($name:ident: [$($id:literal),* $(,)?]),* $(,)?) => {
        /// Every case, as `(group, ids)`.
        const GROUPS: &[(&str, &[&str])] = &[$((stringify!($name), &[$($id),*])),*];
    };
}

mode_tests! {
    origin_mode_homes_on_set_and_reset: [
        "DECOM01_set_homes_to_region_top",
        "DECOM02_set_full_screen_region_homes_to_screen_origin",
        "DECOM03_reset_homes_to_screen_origin",
        "DECOM04_set_clears_pending_wrap",
        "DECOM05_reset_clears_pending_wrap",
        "DECOM06_repeat_set_rehomes",
    ],
    origin_mode_positions_inside_the_region: [
        "DECOM07_cup_relative_to_region",
        "DECOM08_cup_past_bottom_clamps_to_bottom_margin",
        "DECOM09_control_cup_past_bottom_without_decom",
        "DECOM10_hvp_same_as_cup",
        "DECOM11_vpa_is_origin_relative",
        "DECOM12_vpa_past_bottom_clamps",
        "DECOM13_hpa_cha_unaffected_without_left_margin",
        "DECOM14_cuu_cud_clamp_to_region_regardless_of_decom",
        "DECOM15_cuu_cud_same_with_decom_on",
        "DECOM16_decstbm_homes_to_region_top_when_set",
        "DECOM17_decstbm_invalid_region_ignored_keeps_cursor",
        "DECOM18_ind_ri_nel_unaffected_by_origin",
        "DECOM19_erase_in_display_ignores_origin",
        "DECOM30_vpr_is_origin_relative_and_stops_at_the_bottom_margin",
    ],
    origin_mode_is_saved_with_the_cursor: [
        "DECOM20_decsc_saves_origin_decrc_restores_into_region",
        "DECOM21_decrc_restores_origin_off",
        "DECOM22_decrc_never_saved_clears_origin",
        "DECOM23_decrc_restores_pending_wrap_after_cursorset",
        "DECOM24_decsc_not_saving_autowrap_or_insert",
        "DECOM25_decsc_not_saving_insert_mode",
        "DECOM26_decstr_resets_origin_before_saved_cursor",
        "DECOM27_ris_resets_origin",
        "DECOM28_control_decrc_without_decom_after_decsc_in_region",
        "DECOM29_decrc_clamps_the_saved_row_to_a_shrunk_region",
    ],
    auto_wrap: [
        "AWM01_decrst7_overwrites_last_column",
        "AWM02_decset7_wraps",
        "AWM03_repeat_and_unchanged_other_state",
        "AWM04_toggle_does_not_touch_pending_wrap",
        "AWM05_ris_and_decstr_set_autowrap_on",
    ],
    cursor_visibility: [
        "TCEM01_reset_hides_set_shows_nothing_else",
        "TCEM02_decstr_shows_cursor",
        "TCEM03_ris_shows_cursor",
        "TCEM04_control_hidden_until_reset",
    ],
    several_and_unknown_modes: [
        "MULTI01_three_modes_in_one_sequence",
        "MULTI02_unknown_mode_in_the_middle_is_skipped",
        "MULTI03_unknown_mode_alone_changes_nothing",
        "MULTI04_empty_and_zero_parameters_are_unknown",
        "MULTI05_sm_rm_without_question_mark_are_ansi_modes",
        "MULTI06_private_marker_gt_eq_lt_on_h_l_ignored",
        "MULTI07_colon_subparameters_reject_decset",
        "MULTI08_colon_subparameters_reject_sm",
        "PRIV01_private_4_is_decsclm_not_irm",
        "PRIV02_ansi_4_is_not_decsclm",
    ],
    alternate_screen: [
        "ALT01_1049_set_enters_alt_blank_cursor_kept",
        "ALT02_1049_reset_restores_saved_cursor_and_main",
        "ALT03_1049_set_again_on_alt_clears_again",
        "ALT04_1049_reset_on_main_without_enter",
        "ALT05_1047_reset_clears_alt_and_does_not_restore_cursor",
        "ALT06_1047_set_after_clear_is_blank",
        "ALT07_47_reset_keeps_alt_contents_in_xterm",
        "ALT08_scroll_region_survives_screen_switches",
        "ALT09_1048_saves_and_restores_cursor",
        "ALT10_each_screen_has_its_own_saved_cursor",
        "ALT11_alt_screen_has_no_scrollback",
        "ALT12_decstr_stays_on_alt",
        "ALT13_ris_leaves_alt_and_clears",
        "ALT14_1049_reset_with_origin_clamps_into_a_shrunk_region",
        "ALT15_1047_reset_disarms_a_pending_wrap",
    ],
    column_mode: [
        "COL01_decset3_ignored_without_40",
        "COL02_decrst3_ignored_without_40",
        "COL03_decset40_stored",
        "COL04_decset3_with_40_resizes_clears_homes_resets_region",
        "COL05_decrst3_with_40_back_to_80",
        "COL06_decset3_when_already_132_still_clears_no_request",
        "COL07_decset3_clears_pending_wrap_keeps_decom",
        "COL08_decset3_after_40_reset_is_ignored_again",
        "COL09_ris_with_132_restores_80_request",
    ],
    insert_mode: [
        "IRM01_sm4_inserts",
        "IRM02_rm4_replaces",
        "IRM03_sm_multi_param_4_and_20",
        "IRM04_unsupported_ansi_modes_ignored",
        "IRM05_decstr_resets_irm_ris_resets_irm",
    ],
    linefeed_newline: [
        "LNM01_lf_does_cr_when_set",
        "LNM02_control_lf_keeps_column_when_reset",
        "LNM03_vt_and_ff_also_cr",
        "LNM04_ind_and_nel_unaffected",
        "LNM05_rm20_resets",
        "LNM06_decstr_does_not_reset_lnm",
        "LNM07_ris_resets_lnm",
    ],
    stored_modes: [
        "MODE01_set_each_stored_mode_then_reset",
        "MODE02_decstr_resets_blink_and_keeps_paste_focus_mouse_reverse",
        "MODE03_ris_resets_every_mode",
    ],
    mouse_modes: [
        "MOUSE01_tracking_modes_are_exclusive",
        "MOUSE02_any_tracking_reset_turns_tracking_off",
        "MOUSE03_encodings_exclusive_reset_matches_only_active",
        "MOUSE04_encoding_reset_of_active_returns_default",
        "MOUSE05_encoding_independent_of_tracking",
    ],
    keyboard_modes: [
        "KBD01_deckpam_deckpnm",
        "KBD02_decckm",
        "KBD03_decstr_resets_cursor_keys_keypad_mok_kitty",
        "KBD04_modify_other_keys_levels",
        "KBD05_modify_other_keys_reset_forms",
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

// ------------------------------------------------- kitty keyboard stack

/// The kitty flags after feeding `bytes` to a fresh 3x10 terminal.
fn kitty_after(bytes: &[u8]) -> u8 {
    let mut term = Terminal::new(3, 10);
    term.advance(bytes);
    term.keyboard_modes().kitty_flags.bits()
}

/// The fixture's kitty table, KITTY01..KITTY13 and KITTY19..KITTY20.
#[test]
fn kitty_stack_push_pop_set() {
    let rows: &[(&str, &[u8], u8)] = &[
        ("KITTY01_push", b"\x1b[>1u", 1),
        ("KITTY02_push_default", b"\x1b[>u", 0),
        ("KITTY03_push_push_pop", b"\x1b[>1u\x1b[>3u\x1b[<u", 1),
        ("KITTY04_pop_empty", b"\x1b[<u", 0),
        ("KITTY05_pop_99", b"\x1b[>1u\x1b[>2u\x1b[<99u", 0),
        // kitty pops nothing for 0; the parser reports `CSI < 0 u` exactly
        // like `CSI < u`, so Tessera pops one (ADR 0020).
        ("KITTY06_pop_0", b"\x1b[>1u\x1b[>2u\x1b[<0u", 1),
        ("KITTY07_set_replace", b"\x1b[=5u", 5),
        ("KITTY08_set_or", b"\x1b[=5u\x1b[=2;2u", 7),
        ("KITTY09_set_andnot", b"\x1b[=7u\x1b[=2;3u", 5),
        ("KITTY10_set_no_params", b"\x1b[=7u\x1b[=u", 0),
        ("KITTY11_set_bad_mode", b"\x1b[=7u\x1b[=1;4u", 7),
        ("KITTY12_set_modifies_top", b"\x1b[>1u\x1b[=3;2u\x1b[<u", 0),
        ("KITTY13_mask_bits", b"\x1b[>255u", 31),
        ("KITTY19_query_not_here", b"\x1b[>3u\x1b[?u", 3),
        // Plain `CSI u` is SCORC and `CSI 5 u` nothing: neither pops.
        ("KITTY20_private_markers", b"\x1b[>3u\x1b[u\x1b[5u", 3),
        // Colon sub-parameters reject the sequence (xterm), so none of
        // these changes the stack.
        (
            "KITTY21_colon_rejected",
            b"\x1b[>3u\x1b[>1:2u\x1b[<1:1u\x1b[=5:1u",
            3,
        ),
    ];
    let mut errors = Vec::new();
    for (id, bytes, want) in rows {
        let whole = kitty_after(bytes);
        let mut term = Terminal::new(3, 10);
        for byte in *bytes {
            term.advance(std::slice::from_ref(byte));
        }
        let bytewise = term.keyboard_modes().kitty_flags.bits();
        if (whole, bytewise) != (*want, *want) {
            errors.push(format!(
                "{id}: expected {want}, got {whole} whole, {bytewise} bytewise"
            ));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// KITTY14: a push beyond the depth evicts the oldest entry, so popping
/// the whole stack ends at the second push, not the first.
#[test]
fn kitty_stack_depth_cap_evicts_the_oldest() {
    let mut term = Terminal::new(3, 10);
    let pushes = u8::try_from(KITTY_STACK_DEPTH + 1).expect("small depth");
    for flags in 1..=pushes {
        term.advance(format!("\x1b[>{flags}u").as_bytes());
    }
    assert_eq!(term.keyboard_modes().kitty_flags.bits(), pushes);
    for expected in (2..pushes).rev() {
        term.advance(b"\x1b[<u");
        assert_eq!(term.keyboard_modes().kitty_flags.bits(), expected);
    }
    term.advance(b"\x1b[<u");
    assert_eq!(
        term.keyboard_modes().kitty_flags.bits(),
        0,
        "the first push was evicted: {KITTY_STACK_DEPTH} entries only"
    );
}

/// KITTY15/16: the main and alternate screens have their own stacks, and
/// the alternate one is kept across visits.
#[test]
fn kitty_stacks_are_per_screen() {
    let mut term = Terminal::new(3, 10);
    term.advance(b"\x1b[>1u\x1b[?1049h");
    assert_eq!(
        term.keyboard_modes().kitty_flags.bits(),
        0,
        "alt: own stack"
    );
    term.advance(b"\x1b[>2u");
    assert_eq!(term.keyboard_modes().kitty_flags.bits(), 2);
    term.advance(b"\x1b[?1049l");
    assert_eq!(term.keyboard_modes().kitty_flags.bits(), 1, "main kept");
    term.advance(b"\x1b[?1049h");
    assert_eq!(term.keyboard_modes().kitty_flags.bits(), 2, "alt kept");
    term.advance(b"\x1b[?47l\x1b[?47h");
    assert_eq!(term.keyboard_modes().kitty_flags.bits(), 2, "47 too");
}

/// KITTY17/18: DECSTR and RIS empty both stacks.
#[test]
fn decstr_and_ris_empty_both_kitty_stacks() {
    for reset in [&b"\x1b[!p"[..], b"\x1bc"] {
        let mut term = Terminal::new(3, 10);
        term.advance(b"\x1b[>3u\x1b[?1049h\x1b[>4u");
        term.advance(reset);
        assert_eq!(term.keyboard_modes(), KeyboardModes::default(), "{reset:?}");
        term.advance(b"\x1b[?1049l");
        assert_eq!(
            term.keyboard_modes(),
            KeyboardModes::default(),
            "{reset:?}: main stack"
        );
    }
}

/// `CSI > 4 ; 3 m` (no such level) and other XTMODKEYS resources leave
/// `modifyOtherKeys` alone; `CSI > m` resets it.
#[test]
fn xtmodkeys_ignores_other_resources_and_levels() {
    let mut term = Terminal::new(3, 10);
    term.advance(b"\x1b[>4;2m\x1b[>4;3m\x1b[>1;2m\x1b[>2;1m");
    assert_eq!(
        term.keyboard_modes().modify_other_keys,
        ModifyOtherKeys::Level2
    );
    term.advance(b"\x1b[>m");
    assert_eq!(
        term.keyboard_modes().modify_other_keys,
        ModifyOtherKeys::Disabled
    );
    // An SGR with the same parameters must not be mistaken for it, and
    // XTMODKEYS must not reach SGR.
    term.advance(b"\x1b[4;2m");
    assert_eq!(
        term.keyboard_modes().modify_other_keys,
        ModifyOtherKeys::Disabled
    );
    term.advance(b"\x1b[0m\x1b[>4;1m");
    assert_eq!(term.grid().pen(), tessera_grid::Style::DEFAULT);
}

/// `ESC =` / `ESC >` and DECSET 66 are one flag.
#[test]
fn deckpam_and_decnkm_share_the_keypad_flag() {
    let mut term = Terminal::new(3, 10);
    term.advance(b"\x1b=");
    assert!(term.keyboard_modes().application_keypad);
    term.advance(b"\x1b[?66l");
    assert!(!term.keyboard_modes().application_keypad);
    term.advance(b"\x1b[?66h");
    assert!(term.keyboard_modes().application_keypad);
    term.advance(b"\x1b>");
    assert!(!term.keyboard_modes().application_keypad);
}

// ------------------------------------------------------------ the seams

/// The `@set` harness seam and the escape sequences write the same state.
#[test]
fn setters_and_sequences_share_the_state() {
    let mut term = Terminal::new(3, 10);
    term.set_auto_wrap(false);
    term.set_insert_mode(true);
    assert!(!term.modes().auto_wrap);
    assert!(term.modes().insert);
    term.advance(b"\x1b[?7h\x1b[4l");
    assert!(term.auto_wrap());
    assert!(!term.insert_mode());
}

/// The defaults are xterm's.
#[test]
fn defaults_are_xterm_s() {
    let modes = Terminal::new(3, 10).modes();
    assert_eq!(modes, Modes::default());
    assert!(modes.auto_wrap && modes.cursor_visible);
    assert!(!modes.cursor_blink && !modes.origin && !modes.alt_screen);
    assert_eq!(modes.mouse, MouseTracking::None);
    assert_eq!(modes.mouse_encoding, MouseEncoding::Default);
}

// ---------------------------------------------------------------- floods

/// A flood of mode sequences costs O(1) each and queues nothing; the
/// screen switches and DECCOLM toggles do not accumulate state.
#[test]
fn a_flood_of_mode_sequences_requests_nothing() {
    let mut term = Terminal::new(24, 80);
    term.advance(b"text");
    let one = b"\x1b[?25l\x1b[?25h\x1b[?1049h\x1b[?1049l\x1b[?1047h\x1b[?1047l\x1b[?6h\x1b[?6l\
\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[4h\x1b[4l\x1b[>1u\x1b[<u\x1b[>4;2m\x1b=\x1b>\x1b[?3h";
    term.advance(&one.repeat(2_000));
    assert_eq!(term.take_requests(), Vec::<Request>::new());
    assert!(!term.grid().is_alt_screen());
    assert_eq!(term.keyboard_modes().kitty_flags.bits(), 0);
    assert_eq!(term.grid().cols(), 80, "DECCOLM without DECSET 40");
}

/// A flood of kitty pushes never grows the stack past its depth.
#[test]
fn a_flood_of_kitty_pushes_stays_bounded() {
    let mut term = Terminal::new(24, 80);
    term.advance(&b"\x1b[>1u".repeat(100_000));
    term.advance(&b"\x1b[<u".repeat(KITTY_STACK_DEPTH));
    assert_eq!(term.keyboard_modes().kitty_flags.bits(), 0);
}

/// With DECSET 40, a DECCOLM flood queues one request per change of the
/// mode to a width the grid does not have, and never resizes the grid.
#[test]
fn a_deccolm_flood_queues_one_request_per_change() {
    let mut term = Terminal::new(24, 80);
    term.advance(b"\x1b[?40h");
    term.advance(&b"\x1b[?3h\x1b[?3h\x1b[?3l".repeat(1_000));
    let requests = term.take_requests();
    // `?3l` asks for 80 columns, which the grid already has.
    assert_eq!(
        requests,
        vec![
            Request::Resize {
                rows: 24,
                cols: 132
            };
            1_000
        ]
    );
    assert_eq!((term.grid().rows(), term.grid().cols()), (24, 80));
}

/// D2': DECCOLM never changes the grid's width, whatever the window's
/// width; it asks for the width with a request instead.
#[test]
fn deccolm_never_resizes_the_grid() {
    let mut term = Terminal::new(30, 100);
    term.advance(b"some text\x1b[?40h\x1b[?3h");
    assert_eq!((term.grid().rows(), term.grid().cols()), (30, 100));
    assert!(term.modes().columns_132);
    assert_eq!(
        term.take_requests(),
        [Request::Resize {
            rows: 30,
            cols: 132
        }]
    );
    term.advance(b"\x1b[?3l");
    assert_eq!((term.grid().rows(), term.grid().cols()), (30, 100));
    assert!(!term.modes().columns_132);
    assert_eq!(
        term.take_requests(),
        [Request::Resize { rows: 30, cols: 80 }]
    );
    // The clear and the home still happen.
    assert_eq!(term.grid().cursor(), tessera_grid::Point::new(0, 0));
    assert!(
        term.grid()
            .visible_lines()
            .iter()
            .all(|line| line.text().trim().is_empty())
    );
}

/// The adversarial repro: vttest sets DECSET 40 and never resets it, and
/// `reset` / `tput reset` send RIS, DECSTR and terminfo's `CSI ? 3 ; 4 l`.
/// None of them may shrink a 100-column grid to 80.
#[test]
fn reset_after_vttest_keeps_the_window_width() {
    let mut term = Terminal::new(30, 100);
    term.advance(b"\x1b[?40h\x1b[?3h\x1b[?3l");
    term.take_requests();
    term.advance(b"\x1bc\x1b[!p\x1b[?3;4l");
    assert_eq!((term.grid().rows(), term.grid().cols()), (30, 100));
    assert!(term.modes().allow_column_change, "40 survives RIS (xterm)");
    assert!(!term.modes().columns_132);
    assert_eq!(
        term.take_requests(),
        Vec::<Request>::new(),
        "never in 132 mode"
    );
    // RIS leaving 132-column mode asks for 80 columns, still without
    // resizing.
    term.advance(b"\x1b[?3h");
    term.take_requests();
    term.advance(b"\x1bc");
    assert_eq!(
        term.take_requests(),
        [Request::Resize { rows: 30, cols: 80 }]
    );
    assert_eq!((term.grid().rows(), term.grid().cols()), (30, 100));
    assert!(!term.modes().columns_132);
}
