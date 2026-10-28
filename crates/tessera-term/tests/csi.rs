//! CSI cursor movement, erase and edit sequences and the ESC sequences
//! that go with them (DECSC/DECRC, IND, NEL, RI, HTS, DECALN, RIS).
//!
//! Expected behaviour comes from part 1 of `tests/fixtures/csi_spec.txt`,
//! an xterm-semantics fixture that cites its sources per case. Three layers
//! check it:
//!
//! - [`fixture_cases_hold`] parses the fixture and checks every case's
//!   screen, cursor, pending-wrap flag, requests and scrollback directly.
//! - One snapshot test per `tests/scripts/csi/*.txt` script (one script per
//!   fixture case, see [`every_fixture_case_has_a_script`]) pins the full
//!   rendered state, fed both in one call and byte by byte.
//! - Direct tests and property tests cover what single cases cannot:
//!   unknown and overflowing sequences leave the grid alone, and random
//!   sequences never panic.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;
#[allow(dead_code)] // Shared with tests/vttest.rs.
mod spec;

use std::path::{Path, PathBuf};

use common::{Chunking, Script, render_state};
use proptest::prelude::*;
use tessera_grid::{Cell, Grid, MAX_CLUSTER_SCALARS, Point};
use tessera_term::Terminal;

// ------------------------------------------------------------------ scripts

fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/csi")
}

/// Runs `tests/scripts/csi/<name>.txt` whole and byte by byte, requires
/// both to agree and snapshots the result as `csi__<name>.snap`.
fn snapshot_script(name: &str) {
    let path = scripts_dir().join(format!("{name}.txt"));
    let script = Script::load(&path);
    let whole = script.run_rendered(Chunking::Whole);
    let bytewise = script.run_rendered(Chunking::Bytewise);
    assert_eq!(
        whole, bytewise,
        "{name}: feeding byte by byte changed the result"
    );
    insta::with_settings!({ input_file => &path, omit_expression => true }, {
        insta::assert_snapshot!(name, whole);
    });
}

macro_rules! script_tests {
    ($($name:ident),* $(,)?) => {
        /// Every script with a snapshot test, by file stem.
        const SCRIPTS: &[&str] = &[$(stringify!($name)),*];
        $(
            #[test]
            fn $name() {
                snapshot_script(stringify!($name));
            }
        )*
    };
}

script_tests! {
    cup01_no_params,
    cup02_semicolon_only,
    cup03_row_only,
    cup04_row_and_empty_col,
    cup05_empty_row_col_only,
    cup06_explicit_zero_zero,
    cup07_zero_row_only,
    cup08_in_range,
    cup09_clamp_both,
    cup10_max_u16,
    cup11_leading_zeros,
    cup12_extra_params_ignored,
    cup13_home_from_corner,
    hvp01_no_params,
    hvp02_row_col,
    hvp03_clamp,
    hvp04_zero_zero,
    cup14_pending_wrap_cleared,
    cup15_pending_wrap_same_cell,
    cup16_1x1_grid,
    cup17_1x1_home_no_params,
    cup18_ignores_scroll_region,
    cup19_below_region,
    cuu01_default,
    cuu02_zero_is_one,
    cuu03_explicit_one,
    cuu04_two,
    cuu05_clamp_top,
    cuu06_max_u16,
    cuu07_at_top_stays,
    cuu08_pending_wrap_cleared,
    cud01_default,
    cud02_zero_is_one,
    cud03_two,
    cud04_clamp_bottom,
    cud05_max_u16,
    cud06_at_bottom_stays,
    cud07_pending_wrap_cleared,
    cuf01_default,
    cuf02_zero_is_one,
    cuf03_three,
    cuf04_clamp_right,
    cuf05_max_u16,
    cuf06_pending_wrap_cleared,
    cub01_default,
    cub02_zero_is_one,
    cub03_three,
    cub04_clamp_left,
    cub05_max_u16,
    cub06_at_col0_stays,
    cub07_pending_wrap_counts_last_col,
    cuu10_region_inside_stops_at_top_margin,
    cuu11_region_at_top_margin_stays,
    cuu12_region_above_goes_to_screen_top,
    cuu13_region_below_stops_at_top_margin,
    cud10_region_inside_stops_at_bottom_margin,
    cud11_region_at_bottom_margin_stays,
    cud12_region_above_stops_at_bottom_margin,
    cud13_region_below_goes_to_screen_bottom,
    cuu20_1x1_grid,
    cnl01_default,
    cnl02_zero_is_one,
    cnl03_two,
    cnl04_clamp_bottom,
    cnl05_pending_wrap,
    cnl06_at_bottom_row_goes_col0,
    cpl01_default,
    cpl02_zero_is_one,
    cpl03_two,
    cpl04_clamp_top,
    cpl05_pending_wrap,
    cpl06_at_top_row_goes_col0,
    cnl10_region_inside_stops_at_bottom_margin,
    cpl10_region_inside_stops_at_top_margin,
    cnl20_1x1_grid,
    cha01_default,
    cha02_zero_is_one,
    cha03_absolute,
    cha04_clamp,
    cha05_max_u16,
    hpa01_default,
    hpa02_zero_is_one,
    hpa03_absolute,
    hpa04_clamp,
    hpa05_max_u16,
    cha06_pending_wrap_cleared,
    cha07_ignores_scroll_region,
    vpa01_default,
    vpa02_zero_is_one,
    vpa03_absolute,
    vpa04_clamp,
    vpa05_max_u16,
    vpa06_pending_wrap_cleared,
    vpa07_ignores_scroll_region,
    cha08_1x1_grid,
    ed01_default_is_below,
    ed02_explicit_0,
    ed03_above_inclusive,
    ed04_all,
    ed05_saved_lines_noop_on_screen,
    ed06_unknown_mode_ignored,
    ed07_below_at_origin,
    ed08_above_at_last_cell,
    ed09_below_last_cell,
    ed10_max_u16_param,
    ed11_pending_wrap_below,
    ed12_pending_wrap_all,
    ed13_pending_wrap_unknown_mode,
    ed14_private_marker_not_ed,
    ed15_1x1_grid,
    el01_default_is_right,
    el02_explicit_0,
    el03_left_inclusive,
    el04_all,
    el05_mode3_ignored,
    el06_at_col0_right,
    el07_at_last_col_left,
    el08_at_last_col_right,
    el09_max_u16,
    el10_pending_wrap_right,
    el11_pending_wrap_left,
    el12_pending_wrap_all,
    el13_private_marker_not_el,
    el14_right_clears_wrapped_flag,
    el15_1x1_grid,
    ich01_default,
    ich02_zero_is_one,
    ich03_three,
    ich04_exceeds_line,
    ich05_max_u16,
    ich06_at_col0,
    ich07_at_last_col,
    ich08_pending_wrap_cleared,
    dch01_default,
    dch02_zero_is_one,
    dch03_three,
    dch04_exceeds_line,
    dch05_max_u16,
    dch06_at_col0,
    dch07_pending_wrap_cleared,
    ech01_default,
    ech02_zero_is_one,
    ech03_three,
    ech04_exceeds_line,
    ech05_max_u16,
    ech06_pending_wrap_cleared,
    ech07_cursor_does_not_move,
    ich09_1x1_grid,
    il01_default,
    il02_zero_is_one,
    il03_three,
    il04_exceeds_region,
    il05_max_u16,
    il06_region_inside,
    il07_region_above_ignored,
    il08_region_below_ignored,
    il09_region_bottom_row,
    il10_pending_wrap_inside_cleared,
    il11_pending_wrap_outside_kept,
    dl01_default,
    dl02_zero_is_one,
    dl03_three,
    dl04_exceeds_region,
    dl05_max_u16,
    dl06_region_inside,
    dl07_region_above_ignored,
    dl08_region_below_ignored,
    dl09_pending_wrap_inside_cleared,
    il12_1x1_grid,
    su01_default,
    su02_zero_is_one,
    su03_three,
    su04_exceeds_screen,
    su05_max_u16,
    su06_region,
    su07_region_exceeds,
    su08_pending_wrap_preserved,
    sd01_default,
    sd02_caret_form,
    sd03_zero_is_one_caret,
    sd04_three,
    sd05_exceeds_screen,
    sd06_region,
    sd07_pending_wrap_preserved,
    sd08_zero_param_t_ambiguity,
    stbm01_no_params_resets_and_homes,
    stbm02_region_homes_cursor,
    stbm03_scroll_inside_region,
    stbm04_semicolon_only_resets,
    stbm05_zero_zero_resets,
    stbm06_top_only,
    stbm07_bottom_clamped,
    stbm08_top_equals_bottom_ignored,
    stbm09_top_greater_than_bottom_ignored,
    stbm10_top_beyond_screen_ignored,
    stbm11_max_u16_ignored,
    stbm12_pending_wrap_cleared,
    stbm13_invalid_keeps_pending_wrap,
    stbm14_1x1_grid_ignored,
    stbm15_minimal_region_two_rows,
    cbt01_default,
    cbt02_zero_is_one,
    cbt03_two,
    cbt04_exceeds,
    cbt05_on_a_stop,
    cbt06_at_col0,
    cbt07_max_u16,
    cbt08_pending_wrap,
    cht01_default,
    cht02_zero_is_one,
    cht03_two,
    cht04_exceeds,
    cht05_on_a_stop,
    cht06_max_u16,
    cht07_pending_wrap_kept,
    tbc01_clear_current,
    tbc02_explicit_0,
    tbc03_clear_all,
    tbc04_mode1_noop,
    tbc05_mode2_noop,
    tbc06_unknown_mode,
    tbc07_clear_nonstop_col,
    hts01_set_stop,
    hts02_set_then_clear_all_then_set,
    hts03_set_beyond_default_between,
    hts04_pending_wrap_kept,
    tbc08_1x1_grid,
    tbc09_narrow_grid_default_stops,
    rep01_default_one,
    rep02_zero_is_one,
    rep03_five,
    rep04_wraps_with_autowrap,
    rep05_no_preceding_char,
    rep06_cleared_by_cursor_move,
    rep07_cleared_by_c0,
    rep08_cleared_by_sgr,
    rep09_repeated_rep_noop,
    rep10_cleared_by_esc,
    rep11_wide_deferred_marker,
    rep12_pending_wrap,
    rep13_auto_wrap_off,
    rep14_insert_mode,
    rep15_insert_mode_repeat,
    rep16_max_u16,
    rep17_1x1_grid,
    rep18_private_marker_not_rep,
    sc01_save_move_restore,
    sc02_restore_without_save_homes,
    sc03_pending_wrap_saved,
    sc04_restore_clears_pending_when_not_saved,
    sc05_save_twice_last_wins,
    sc06_restore_twice,
    sc07_region_not_saved,
    sc08_1x1_grid,
    sc09_ansi_csi_s_u_not_handled,
    ind01_moves_down_keeps_column,
    ind02_at_bottom_scrolls,
    ind03_region_bottom_scrolls_region,
    ind04_below_region_moves_down,
    ind05_last_row_below_region_stays,
    ind06_pending_wrap_cleared,
    nel01_down_and_col0,
    nel02_at_bottom_scrolls,
    nel03_pending_wrap,
    ri01_moves_up_keeps_column,
    ri02_at_top_scrolls_down,
    ri03_region_top_scrolls_region,
    ri04_above_region_moves_up,
    ri05_pending_wrap_cleared,
    ri06_1x1_grid,
    decaln01_fills_e_and_homes,
    decaln02_resets_region,
    decaln03_clears_pending_wrap,
    decaln04_1x1_grid,
    decaln05_hash_other_ignored,
    ris01_full_reset,
    ris02_clears_scrollback,
    ris03_restores_modes,
    ris04_pending_wrap_cleared,
    ris05_saved_cursor_reset,
    unk01_tilde_with_params,
    unk02_private_dec_mode_set,
    unk03_private_dec_mode_reset,
    unk04_private_marker_gt,
    unk05_private_marker_eq,
    unk06_intermediate_space_q,
    unk07_intermediate_space_a,
    unk08_intermediate_bang_p,
    unk09_intermediate_dollar_p,
    unk10_intermediate_dollar_r,
    unk11_intermediate_quote_h,
    unk12_unhandled_finals,
    unk13_hpr_vpr_not_in_list,
    unk14_ignore_flag_33_params,
    unk15_32_params_not_ignored,
    unk16_esc_unknown_finals,
    unk17_esc_charset_designation,
    unk18_colon_subparams_flattened,
    unk19_c0_inside_csi_executes,
    unk20_esc_inside_csi_restarts,
    unk21_can_inside_csi_aborts,
}

#[test]
fn every_script_has_a_test() {
    let mut on_disk: Vec<String> = std::fs::read_dir(scripts_dir())
        .expect("tests/scripts/csi exists")
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .map(|path| {
            path.file_stem()
                .expect("script has a file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    on_disk.sort();
    let mut listed: Vec<&str> = SCRIPTS.to_vec();
    listed.sort_unstable();
    assert_eq!(on_disk, listed, "add new scripts to `script_tests!`");
}

// ------------------------------------------------------------------ fixture

#[test]
fn fixture_cases_hold() {
    let checked = spec::check_all(|case| !spec::is_vttest(&case.id));
    assert_eq!(checked, 301, "part 1 case count");
}

#[test]
fn every_fixture_case_has_a_script() {
    let cases = spec::parse_fixture(spec::FIXTURE);
    for case in cases.iter().filter(|case| !spec::is_vttest(&case.id)) {
        let name = case.id.to_lowercase();
        let path = scripts_dir().join(format!("{name}.txt"));
        assert_eq!(
            Script::load(&path),
            case.script,
            "{name}.txt does not match fixture case {}",
            case.id
        );
    }
}

// ----------------------------------------------------------- direct checks

/// A 5x10 terminal whose rows read `abcdefghij`, `klmnopqrst`, ... with
/// the cursor parked at row 3, column 5 (0-based (2,4)).
fn filled() -> Terminal {
    let mut term = Terminal::new(5, 10);
    term.advance(b"abcdefghij\r\nklmnopqrst\r\nuvwxyzABCD\r\nEFGHIJKLMN\r\nOPQRSTUVW");
    term.advance(b"\x1b[3;5H");
    term
}

fn state(term: &mut Terminal) -> String {
    let requests = term.take_requests();
    render_state(term, &requests)
}

#[test]
fn unknown_sequences_leave_the_grid_unchanged() {
    let sequences: &[&[u8]] = &[
        b"\x1b[1;2;3~",
        b"\x1b[123~",
        b"\x1b[?1J",
        b"\x1b[?5;6H",
        b"\x1b[>4;2m",
        b"\x1b[1 A",
        b"\x1b[3$r",
        b"\x1b[N",
        b"\x1b[5j",
        b"\x1b=",
        b"\x1b#3",
        b"\x1b(0",
    ];
    let before = state(&mut filled());
    for seq in sequences {
        let mut term = filled();
        term.advance(seq);
        assert_eq!(
            state(&mut term),
            before,
            "{:?} changed the terminal",
            seq.escape_ascii().to_string()
        );
    }
}

#[test]
fn overflowing_csi_is_dropped() {
    // 33 parameters overflow the parser, which flags the action `ignore`.
    // As CUP it would move the cursor to (0,0).
    let mut seq = b"\x1b[".to_vec();
    seq.extend_from_slice(["1"; 33].join(";").as_bytes());
    seq.push(b'H');
    let before = state(&mut filled());
    let mut term = filled();
    term.advance(&seq);
    assert_eq!(state(&mut term), before);
    assert_eq!(term.grid().cursor(), Point::new(2, 4));

    // 32 parameters are kept: the same sequence is a CUP to (0,0).
    let mut seq = b"\x1b[".to_vec();
    seq.extend_from_slice(["1"; 32].join(";").as_bytes());
    seq.push(b'H');
    term.advance(&seq);
    assert_eq!(term.grid().cursor(), Point::new(0, 0));
}

#[test]
fn rep_repeats_only_right_after_a_print() {
    let mut term = Terminal::new(1, 10);
    term.advance(b"A\x1b[3b");
    assert_eq!(term.grid().line(0).unwrap().text(), "AAAA");
    // DEL arrives as a printable but is not graphic: it clears the char.
    term.advance(b"\rB\x7f\x1b[3b");
    assert_eq!(term.grid().line(0).unwrap().text(), "BAAA");
}

#[test]
fn rep_repeats_with_auto_wrap_off() {
    let mut term = Terminal::new(1, 5);
    term.set_auto_wrap(false);
    term.advance(b"ab\x1b[3b");
    assert_eq!(term.grid().line(0).unwrap().text(), "abbbb");
    assert_eq!(term.grid().cursor(), Point::new(0, 4));
    // Past the margin the repeats overwrite the last column.
    term.advance(b"\rc\x1b[9b");
    assert_eq!(term.grid().line(0).unwrap().text(), "ccccc");
    assert_eq!(term.grid().cursor(), Point::new(0, 4));
    assert!(!term.grid().line(0).unwrap().wrapped);
}

/// Renders the full state after feeding `bytes` to a fresh `rows x cols`
/// terminal, optionally with DECAWM off.
fn run_state(bytes: &[u8], rows: u16, cols: u16, auto_wrap: bool) -> String {
    let mut term = Terminal::new(rows, cols);
    term.set_auto_wrap(auto_wrap);
    term.advance(bytes);
    state(&mut term)
}

#[test]
fn rep_ignores_a_character_that_joined_a_cluster() {
    for text in ["e\u{301}", "👍🏽", "🇸🇪", "👩\u{200d}💻", "❤\u{fe0f}"] {
        assert_eq!(
            run_state(format!("{text}\x1b[3b").as_bytes(), 2, 10, true),
            run_state(text.as_bytes(), 2, 10, true),
            "REP after {text:?} must not repeat anything",
        );
    }
}

#[test]
fn rep_repeats_a_wide_character_as_wide() {
    let mut term = Terminal::new(2, 10);
    term.advance("中\x1b[2b".as_bytes());
    assert_eq!(term.grid().cursor(), Point::new(0, 6));
    assert_eq!(
        state(&mut term),
        run_state("中中中".as_bytes(), 2, 10, true)
    );
}

#[test]
fn rep_repeats_the_clamped_three_column_scalar_as_wide() {
    // U+17D8 is the one scalar unicode-width calls three columns wide; the
    // grid clamps it to two.
    let mut term = Terminal::new(2, 10);
    term.advance("\u{17d8}\x1b[b".as_bytes());
    assert_eq!(term.grid().cursor(), Point::new(0, 4));
    assert_eq!(
        state(&mut term),
        run_state("\u{17d8}\u{17d8}".as_bytes(), 2, 10, true)
    );
}

#[test]
fn rep_after_a_dropped_character_does_nothing() {
    // Without DECAWM a wide character on the last column is dropped.
    assert_eq!(
        run_state("abcd中\x1b[2b".as_bytes(), 1, 5, false),
        run_state(b"abcd", 1, 5, false),
    );
    // A wide character never fits a one-column grid.
    assert_eq!(
        run_state("a\r中\x1b[2b".as_bytes(), 2, 1, true),
        run_state(b"a\r", 2, 1, true),
    );
    // A zero-width scalar with nothing to join is dropped too.
    assert_eq!(
        run_state("\u{301}\x1b[2b".as_bytes(), 1, 5, true),
        run_state(b"", 1, 5, true),
    );
}

#[test]
fn csi_t_with_several_parameters_is_not_sd() {
    // `CSI Ps ; Ps ; Ps ; Ps ; Ps T` is xterm's mouse highlight tracking.
    let before = state(&mut filled());
    let mut term = filled();
    term.advance(b"\x1b[1;2;3;4;5T");
    assert_eq!(state(&mut term), before, "highlight tracking ignored");
    term.advance(b"\x1b[1;2T");
    assert_eq!(state(&mut term), before, "two parameters ignored too");
    term.advance(b"\x1b[2T");
    assert_eq!(
        term.grid().line(2).unwrap().text(),
        "abcdefghij",
        "one parameter is SD 2"
    );
}

#[test]
fn ed_3_clears_scrollback_and_keeps_the_pending_wrap() {
    let mut term = Terminal::new(2, 5);
    term.advance(b"one\r\ntwo\r\nthree\r\nfours");
    assert_eq!(term.grid().scrollback_len(), 2);
    assert!(term.grid().pending_wrap());
    term.advance(b"\x1b[3J");
    assert_eq!(term.grid().scrollback_len(), 0, "saved lines discarded");
    assert!(term.grid().pending_wrap(), "pending wrap survives ED 3");
    assert_eq!(term.grid().line(1).unwrap().text(), "fours", "screen kept");
}

#[test]
fn decaln_keeps_the_pen() {
    let mut term = Terminal::new(2, 3);
    let pen = term.grid().pen();
    term.advance(b"\x1b#8");
    assert_eq!(term.grid().pen(), pen);
    assert!(
        term.grid()
            .visible_lines()
            .iter()
            .all(|line| line.text() == "EEE" && !line.wrapped)
    );
}

// --------------------------------------------------------------- properties

/// A CSI sequence with random private marker, parameters (huge ones
/// included), intermediate and final byte, or one of the handled finals.
fn any_csi() -> impl Strategy<Value = Vec<u8>> {
    (
        prop::option::of(prop_oneof![Just(b'?'), Just(b'>'), Just(b'='), Just(b'<')]),
        prop::collection::vec(
            prop_oneof![
                Just(None),
                (0u32..100).prop_map(Some),
                any::<u32>().prop_map(Some)
            ],
            0..6,
        ),
        prop::option::weighted(0.1, 0x20..=0x2Fu8),
        prop_oneof![
            0x40..=0x7Eu8,
            prop::sample::select(b"@ABCDEFGHIJKLMPSTXZ^`bdfgr".to_vec()),
        ],
    )
        .prop_map(|(private, params, intermediate, final_byte)| {
            let mut out = b"\x1b[".to_vec();
            out.extend(private);
            let params: Vec<String> = params
                .iter()
                .map(|p| p.map(|n| n.to_string()).unwrap_or_default())
                .collect();
            out.extend_from_slice(params.join(";").as_bytes());
            out.extend(intermediate);
            out.push(final_byte);
            out
        })
}

/// One of the handled ESC sequences, or a random one.
fn any_esc() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::sample::select(vec![
            b"\x1b7".to_vec(),
            b"\x1b8".to_vec(),
            b"\x1bD".to_vec(),
            b"\x1bE".to_vec(),
            b"\x1bH".to_vec(),
            b"\x1bM".to_vec(),
            b"\x1bc".to_vec(),
            b"\x1b#8".to_vec(),
        ]),
        (0x30..=0x7Eu8).prop_map(|b| vec![0x1b, b]),
    ]
}

/// Scalars that exercise the grid's width and cluster handling: ASCII,
/// wide CJK and Hangul, a combining mark, regional indicators, an emoji
/// with a skin-tone modifier, ZWJ, VS16 and the clamped three-column
/// U+17D8.
const SCALARS: &[char] = &[
    'a', 'e', 'Z', ' ', '中', '한', '\u{301}', '🇸', '🇪', '👍', '🏽', '\u{200d}', '👩', '💻', '❤',
    '\u{fe0f}', '\u{17d8}',
];

/// A run of [`SCALARS`], UTF-8 encoded.
fn mixed_text() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop::sample::select(SCALARS), 1..8)
        .prop_map(|chars| chars.into_iter().collect::<String>().into_bytes())
}

/// Random text (ASCII and [`SCALARS`]), controls and sequences.
fn stream() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            4 => any_csi(),
            1 => any_esc(),
            2 => prop::collection::vec(b' '..=b'~', 1..12),
            2 => mixed_text(),
            1 => prop::sample::select(b"\r\n\t\x08".to_vec()).prop_map(|b| vec![b]),
        ],
        0..60,
    )
    .prop_map(|chunks| chunks.concat())
}

/// Every violation of the grid's cell invariants: a wide cell is followed
/// by its spacer and a spacer preceded by a wide cell, a spacer is a bare
/// `' '`, the cluster flag matches the line's grapheme table, a cluster
/// starts with its cell's character and has 2..=MAX scalars, lines have
/// `cols` cells and the cursor is on screen. A local copy of the checks in
/// `tessera-grid`'s tests, which this crate cannot import.
fn invariant_errors(grid: &Grid) -> Vec<String> {
    let mut errors = Vec::new();
    let cols = usize::from(grid.cols());
    let scrollback = (0..grid.scrollback_len()).filter_map(|i| grid.scrollback_line(i));
    for (i, line) in grid.visible_lines().iter().chain(scrollback).enumerate() {
        let cells = &line.cells;
        if cells.len() != cols {
            errors.push(format!("line {i}: {} cells", cells.len()));
        }
        for (col, cell) in cells.iter().enumerate() {
            if cell.is_wide() && !cells.get(col + 1).is_some_and(Cell::is_spacer) {
                errors.push(format!("line {i} col {col}: wide without a spacer"));
            }
            if cell.is_spacer() {
                if col == 0 || !cells[col - 1].is_wide() {
                    errors.push(format!("line {i} col {col}: spacer without a wide cell"));
                }
                if cell.ch != ' ' || cell.is_wide() || cell.is_cluster() {
                    errors.push(format!("line {i} col {col}: spacer is not a bare ' '"));
                }
            }
            match (cell.is_cluster(), line.cluster(col)) {
                (true, None) => errors.push(format!("line {i} col {col}: cluster without text")),
                (false, Some(_)) => {
                    errors.push(format!("line {i} col {col}: text without cluster"));
                }
                (true, Some(text)) => {
                    let scalars = text.chars().count();
                    if !(2..=MAX_CLUSTER_SCALARS).contains(&scalars) || !text.starts_with(cell.ch) {
                        errors.push(format!("line {i} col {col}: bad cluster {text:?}"));
                    }
                }
                (false, None) => {}
            }
        }
        if line.graphemes.iter().any(|(col, _)| col >= cells.len()) {
            errors.push(format!("line {i}: cluster past the end"));
        }
    }
    let cursor = grid.cursor();
    if cursor.row >= grid.rows() || cursor.col >= grid.cols() {
        errors.push(format!("cursor {cursor:?} off screen"));
    }
    if grid.pending_wrap() && cursor.col + 1 != grid.cols() {
        errors.push(format!("pending wrap off the last column at {cursor:?}"));
    }
    let region = grid.scroll_region();
    if region.top > region.bottom || region.bottom >= grid.rows() {
        errors.push(format!("scroll region {region:?} invalid"));
    }
    errors
}

/// The whole grid, clusters and flags included, for exact comparisons.
fn grid_state(term: &Terminal) -> String {
    format!("{:?}", term.grid())
}

proptest! {
    #[test]
    fn random_sequences_never_panic_and_keep_the_grid_consistent(
        bytes in stream(),
        rows in 1u16..6,
        cols in 1u16..12,
        auto_wrap in any::<bool>(),
        insert in any::<bool>(),
    ) {
        let mut term = Terminal::new(rows, cols);
        term.set_auto_wrap(auto_wrap);
        term.set_insert_mode(insert);
        term.advance(&bytes);
        prop_assert_eq!(term.grid().visible_lines().len(), usize::from(rows));
        let errors = invariant_errors(term.grid());
        prop_assert!(errors.is_empty(), "{}", errors.join("; "));
    }

    #[test]
    fn rep_equals_printing_again_or_nothing(
        prefix in stream(),
        ch in prop::sample::select(SCALARS),
        n in 1u16..6,
        rows in 1u16..4,
        cols in 1u16..8,
        auto_wrap in any::<bool>(),
        insert in any::<bool>(),
    ) {
        let start = || {
            let mut term = Terminal::new(rows, cols);
            term.set_auto_wrap(auto_wrap);
            term.set_insert_mode(insert);
            term.advance(&prefix);
            // CAN returns the parser to ground from inside any sequence or
            // string. Without it a prefix ending in an unterminated SOS, PM
            // or APC (`ESC X`, `ESC ^`, `ESC _`) would swallow `ch`, and
            // those strings yield no action, so REP would still see the
            // character printed before them.
            term.advance(b"\x18");
            term
        };
        let run = |tail: &[u8]| {
            let mut term = start();
            term.advance(tail);
            let errors = invariant_errors(term.grid());
            prop_assert!(errors.is_empty(), "after {tail:?}: {}", errors.join("; "));
            Ok(grid_state(&term))
        };
        // Whether `ch` will join the previous cluster or be dropped, decided
        // from the public API before printing it. DECAWM is read from the
        // terminal because the prefix may contain RIS.
        let term = start();
        let grid = term.grid();
        let last = grid.cols() - 1;
        let col = if grid.pending_wrap() { last } else { grid.cursor().col };
        let width = tessera_grid::char_width(ch);
        let dropped = width == Some(2) && (grid.cols() < 2 || (!term.auto_wrap() && col == last));
        let joins = grid.would_attach(ch) || width == Some(0) || dropped;

        let once = ch.to_string();
        let rep = run(format!("{once}\x1b[{n}b").as_bytes())?;
        if joins {
            let nothing = run(once.as_bytes())?;
            prop_assert_eq!(rep, nothing, "{:?} joined or was dropped: REP must do nothing", ch);
        } else {
            let again = run(once.repeat(usize::from(n) + 1).as_bytes())?;
            prop_assert_eq!(rep, again, "{:?} took a cell: REP must print it {} more times", ch, n);
        }
    }

    #[test]
    fn chunking_csi_streams_does_not_change_the_result(
        bytes in stream(),
        split in any::<prop::sample::Index>(),
    ) {
        let run = |chunks: &[&[u8]]| {
            let mut term = Terminal::new(4, 9);
            for chunk in chunks {
                term.advance(chunk);
            }
            state(&mut term)
        };
        let (head, tail) = bytes.split_at(split.index(bytes.len() + 1));
        prop_assert_eq!(run(&[&bytes]), run(&[head, tail]));
    }
}
