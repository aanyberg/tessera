//! Printing and C0 controls: auto-wrap (DECAWM) with pending-wrap
//! semantics, insert mode (IRM), BS, HT, LF/VT/FF, CR and BEL.
//!
//! Expected behaviour comes from `tests/fixtures/print_c0_spec.txt`, an
//! xterm-semantics fixture that cites its sources per case. Three layers
//! check it:
//!
//! - [`fixture_cases_hold`] parses the fixture and checks every case's
//!   screen, cursor, pending-wrap flag, requests and scrollback directly.
//! - [`every_script_matches_its_snapshot`] runs every
//!   `tests/scripts/print/*.txt` script (one script per fixture case) and
//!   pins its full rendered state as a snapshot, fed both in one call and
//!   byte by byte.
//! - Property tests cover what single cases cannot: bell counting, ignored
//!   controls and sequences, DECAWM off and chunking independence.

mod common;

use std::path::{Path, PathBuf};

use common::{Chunking, Script, Step, decode, render_screen, run_script};
use proptest::prelude::*;
use tessera_grid::Point;
use tessera_term::{Request, Terminal};

// ------------------------------------------------------------------ scripts

fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/print")
}

/// Runs `tests/scripts/print/<name>.txt` whole and byte by byte, requires
/// both to agree and snapshots the result as `print__<name>.snap`.
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

/// Lists the scripts [`every_script_matches_its_snapshot`] runs, by file
/// stem.
macro_rules! script_table {
    ($($name:ident),* $(,)?) => {
        /// Every script with a snapshot, by file stem.
        const SCRIPTS: &[&str] = &[$(stringify!($name)),*];
    };
}

script_table! {
    a01_wrap_81_chars_80x3,
    a02_exactly_80_chars_pending_80x3,
    a03_80_chars_then_cr_80x3,
    a04_80_chars_then_lf_then_x_80x3,
    a05_80_chars_then_bs_then_x_80x3,
    a06_80_chars_then_ht_then_x_80x3,
    a07_wrap_scrolls_at_bottom_3x5,
    a08_fill_screen_exactly_no_scroll_2x5,
    a09_one_more_after_fill_scrolls_2x5,
    a10_wrap_flag_only_on_the_wrapped_row_3x5,
    a11_one_column_grid_3x1,
    b01_overwrite_last_col_80x3,
    b02_overwrite_small_1x5,
    b03_toggle_off_while_pending_3x5,
    b04_off_then_ht_to_last_col_then_print_3x20,
    b05_off_does_not_wrap_on_lf_or_cr_3x5,
    c01_bs_at_col0_does_not_wrap_back_2x5,
    c02_bs_moves_left_without_erasing_1x8,
    c03_bs_on_empty_screen_1x8,
    c04_bs_with_one_column_clears_pending_3x1,
    d01_default_stops_every_8_1x20,
    d02_ht_from_col7_and_col8_1x24,
    d03_ht_on_a_stop_goes_to_next_1x24,
    d04_ht_does_not_erase_or_write_1x20,
    d05_ht_clamps_to_last_column_80x1,
    d06_extra_ht_at_last_col_are_noops_80x1,
    d07_ht_narrower_than_a_stop_1x5,
    d08_ht_then_bs_1x20,
    e01_lf_keeps_column_3x6,
    e02_crlf_pairs_3x6,
    e03_vt_behaves_as_lf_3x6,
    e04_ff_behaves_as_lf_3x6,
    e05_lf_at_bottom_scrolls_3x6,
    e06_trailing_crlf_at_bottom_3x6,
    e07_bare_lf_at_bottom_row_keeps_column_3x6,
    e08_cr_overwrites_from_col1_1x6,
    e09_cr_at_col0_is_noop_1x6,
    e10_lf_does_not_set_wrapped_2x6,
    e11_many_lf_pin_to_bottom_3x4,
    f01_single_bel_one_request_2x6,
    f02_bel_between_text_2x6,
    f03_take_requests_drains_2x6,
    f04_bel_as_osc_terminator_is_not_a_bell_2x6,
    f05_bel_does_not_move_cursor_or_clear_pending_2x5,
    g01_nul_soh_enq_ack_ignored_1x8,
    g02_so_si_are_visible_noops_1x8,
    g03_remaining_c0_ignored_1x8,
    g04_can_sub_decision_1x8,
    g05_del_print_is_dropped_1x8,
    g06_c1_controls_ignored_in_5_1x8,
    g07_csi_is_ignored_and_consumed_2x8,
    g08_esc_osc_dcs_ignored_2x8,
    h01_shift_right_and_drop_last_1x5,
    h02_two_inserts_1x5,
    h03_insert_mid_line_1x10,
    h04_insert_off_again_overwrites_1x6,
    h05_insert_in_pending_state_wraps_first_3x5,
    h06_insert_at_last_column_just_replaces_3x5,
    h07_insert_with_decawm_off_at_last_col_3x5,
    h08_insert_default_off_1x5,
    h09_insert_does_not_affect_controls_1x6,
    h10_insert_scrolling_wrap_2x4,
    i01_byte_by_byte_equals_one_shot_3x5,
    i02_non_ascii_single_width_1x6,
}

/// Every listed script matches its snapshot. One test runs the whole
/// table (one process instead of one per script) and reports every
/// failing script.
#[test]
fn every_script_matches_its_snapshot() {
    let fixture_cases = parse_fixture(include_str!("fixtures/print_c0_spec.txt")).len();
    common::check_table(
        "tests/scripts/print",
        fixture_cases,
        SCRIPTS.iter().map(|name| (name, *name)),
        snapshot_script,
    );
}

#[test]
fn every_script_has_a_test() {
    let mut on_disk: Vec<String> = std::fs::read_dir(scripts_dir())
        .expect("tests/scripts/print exists")
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
    assert_eq!(on_disk, listed, "add new scripts to `script_table!`");
}

// ------------------------------------------------------------------ fixture

/// What one fixture case expects.
#[derive(Debug)]
struct Case {
    id: String,
    script: Script,
    screen: String,
    cursor: Point,
    pending_wrap: bool,
    requests: Vec<Request>,
    /// `(len, first line as (text, wrapped))`; `None` when not asserted.
    scrollback: Option<(usize, Option<(String, bool)>)>,
}

/// Drops a trailing `# comment` and surrounding blanks from a field value.
fn value(raw: &str) -> &str {
    raw.split_once('#').map_or(raw, |(v, _)| v).trim()
}

fn parse_cursor(raw: &str) -> Point {
    let inner = value(raw)
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .expect("cursor is `(row,col)`");
    let (row, col) = inner.split_once(',').expect("cursor is `(row,col)`");
    Point::new(
        row.trim().parse().expect("cursor row"),
        col.trim().parse().expect("cursor col"),
    )
}

/// `[]`, or a comma list of `Bell` and `SetTitle("..")` (a title without
/// `,`, `"` or `\`: the fixture's are plain words).
fn parse_requests(raw: &str) -> Vec<Request> {
    let inner = value(raw)
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .expect("requests is `[..]`");
    inner
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(|r| {
            if r == "Bell" {
                return Request::Bell;
            }
            let title = r
                .strip_prefix("SetTitle(\"")
                .and_then(|t| t.strip_suffix("\")"))
                .unwrap_or_else(|| panic!("unexpected request `{r}` in fixture"));
            Request::SetTitle(title.to_owned())
        })
        .collect()
}

fn parse_scrollback(raw: &str) -> Option<(usize, Option<(String, bool)>)> {
    let raw = raw.trim();
    if raw.starts_with("not asserted") {
        return None;
    }
    let rest = raw.strip_prefix("len=").expect("scrollback is `len=N ...`");
    let (len, first) = rest.split_once(' ').unwrap_or((rest, ""));
    let first = first.trim().strip_prefix("first=(\"").map(|first| {
        let (text, flags) = first
            .split_once("\", wrapped=")
            .expect("first=(\"..\", wrapped=..)");
        let wrapped = flags.trim_end_matches(')') == "true";
        (text.to_owned(), wrapped)
    });
    Some((len.parse().expect("scrollback len"), first))
}

fn parse_fixture(text: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(id) = line.strip_prefix("### ") else {
            continue;
        };
        let mut size = None;
        let mut steps = Vec::new();
        let mut screen = String::new();
        let (mut cursor, mut pending_wrap, mut requests, mut scrollback) = (None, None, None, None);
        while let Some(line) = lines.next_if(|l| !l.starts_with("### ") && !l.starts_with("# --")) {
            let (key, raw) = line
                .split_once(": ")
                .unwrap_or((line.trim_end_matches(':'), ""));
            match key {
                "size" => {
                    let (rows, cols) = raw.split_once('x').expect("size is `RxC`");
                    size = Some((rows.parse().expect("rows"), cols.parse().expect("cols")));
                }
                "step" => steps.push(if let Some(p) = raw.strip_prefix("feed_bytewise ") {
                    Step::Feed(decode(p))
                } else if let Some(p) = raw.strip_prefix("feed ") {
                    Step::Feed(decode(p))
                } else if let Some(arg) = raw.strip_prefix("set ") {
                    Step::parse_set(arg)
                } else {
                    panic!("{id}: unknown step `{raw}`")
                }),
                "screen" => {
                    while let Some(row) = lines.next_if(|l| l.starts_with('>')) {
                        screen.push_str(row[1..].trim_end());
                        screen.push('\n');
                    }
                }
                "cursor" => cursor = Some(parse_cursor(raw)),
                "pending_wrap" => pending_wrap = Some(value(raw) == "true"),
                "requests" => requests = Some(parse_requests(raw)),
                "scrollback" => scrollback = Some(parse_scrollback(raw)),
                _ => {}
            }
        }
        let (rows, cols) = size.unwrap_or_else(|| panic!("{id}: no size"));
        cases.push(Case {
            id: id.to_owned(),
            script: Script { rows, cols, steps },
            screen,
            cursor: cursor.unwrap_or_else(|| panic!("{id}: no cursor")),
            pending_wrap: pending_wrap.unwrap_or_else(|| panic!("{id}: no pending_wrap")),
            requests: requests.unwrap_or_else(|| panic!("{id}: no requests")),
            scrollback: scrollback.unwrap_or_else(|| panic!("{id}: no scrollback")),
        });
    }
    cases
}

/// Every mismatch between a case and what the terminal did.
fn check_case(case: &Case, chunking: Chunking) -> Vec<String> {
    let (term, requests) = case.script.run(chunking);
    let grid = term.grid();
    let mut errors = Vec::new();
    let mut expect = |what: &str, ok: bool, detail: String| {
        if !ok {
            errors.push(format!("{} ({chunking:?}) {what}: {detail}", case.id));
        }
    };
    let screen = render_screen(&term);
    expect(
        "screen",
        screen == case.screen,
        format!("\nexpected:\n{}got:\n{screen}", case.screen),
    );
    expect(
        "cursor",
        grid.cursor() == case.cursor,
        format!("expected {:?}, got {:?}", case.cursor, grid.cursor()),
    );
    expect(
        "pending_wrap",
        grid.pending_wrap() == case.pending_wrap,
        format!("expected {}", case.pending_wrap),
    );
    expect(
        "requests",
        requests == case.requests,
        format!("expected {:?}, got {requests:?}", case.requests),
    );
    if let Some((len, first)) = &case.scrollback {
        expect(
            "scrollback_len",
            grid.scrollback_len() == *len,
            format!("expected {len}, got {}", grid.scrollback_len()),
        );
        if let Some((text, wrapped)) = first {
            let got = grid.scrollback_line(0).map(|l| (l.text(), l.wrapped));
            expect(
                "scrollback[0]",
                got.as_ref() == Some(&(text.clone(), *wrapped)),
                format!("expected {:?}, got {got:?}", (text, wrapped)),
            );
        }
    }
    errors
}

#[test]
fn fixture_cases_hold() {
    let cases = parse_fixture(include_str!("fixtures/print_c0_spec.txt"));
    assert_eq!(cases.len(), 64, "fixture case count");
    let errors: Vec<String> = cases
        .iter()
        .flat_map(|case| {
            [Chunking::Whole, Chunking::Bytewise]
                .into_iter()
                .flat_map(|chunking| check_case(case, chunking))
        })
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn every_fixture_case_has_a_script() {
    let cases = parse_fixture(include_str!("fixtures/print_c0_spec.txt"));
    for case in cases {
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

#[test]
fn eighty_first_char_wraps_to_the_next_row() {
    let mut term = Terminal::new(3, 80);
    term.advance(&[b'a'; 80]);
    let grid = term.grid();
    assert_eq!(grid.cursor(), Point::new(0, 79), "cursor stays on col 80");
    assert!(grid.pending_wrap(), "80th char arms pending wrap");
    assert!(!grid.line(0).unwrap().wrapped, "no wrap before the 81st");

    term.advance(b"b");
    let grid = term.grid();
    assert!(grid.line(0).unwrap().wrapped, "row 1 is marked wrapped");
    assert!(!grid.line(1).unwrap().wrapped, "row 2 is not");
    assert_eq!(
        grid.cell(Point::new(1, 0)).unwrap().ch,
        'b',
        "81st at row 2 col 1"
    );
    assert_eq!(grid.cursor(), Point::new(1, 1), "cursor at row 2 col 2");
    assert!(!grid.pending_wrap());
}

#[test]
fn auto_wrap_off_overwrites_the_last_column() {
    let mut term = Terminal::new(3, 80);
    term.set_auto_wrap(false);
    assert!(!term.auto_wrap());
    term.advance(&[b'a'; 79]);
    term.advance(b"XYZ");
    let grid = term.grid();
    assert_eq!(grid.cell(Point::new(0, 79)).unwrap().ch, 'Z');
    assert_eq!(grid.cell(Point::new(0, 78)).unwrap().ch, 'a');
    assert_eq!(grid.cursor(), Point::new(0, 79));
    assert!(grid.visible_lines().iter().all(|line| !line.wrapped));
    assert_eq!(grid.line(1).unwrap().text(), "", "nothing reached row 2");
}

#[test]
fn defaults_are_auto_wrap_on_and_insert_off() {
    let term = Terminal::new(1, 5);
    assert!(term.auto_wrap(), "DECAWM defaults to on");
    assert!(!term.insert_mode(), "IRM defaults to replace");
}

#[test]
fn mode_setters_round_trip() {
    let mut term = Terminal::new(1, 5);
    term.set_insert_mode(true);
    assert!(term.insert_mode());
    term.set_insert_mode(false);
    assert!(!term.insert_mode());
    term.set_auto_wrap(false);
    term.set_auto_wrap(true);
    assert!(term.auto_wrap());
}

#[test]
fn take_requests_drains() {
    let mut term = Terminal::new(2, 6);
    term.advance(b"\x07");
    assert_eq!(
        term.take_requests(),
        vec![Request::Bell],
        "one bell per BEL"
    );
    assert_eq!(term.take_requests(), vec![], "second take is empty");
}

/// CSI sequences whose finals ctlseqs leaves unassigned, with and without
/// a private marker. Assigned ones are applied since #6 (`tests/csi.rs`).
#[test]
fn csi_leaves_the_grid_unchanged() {
    assert_eq!(
        run_script(b"ab\x1b[2N\x1b[5;5~\x1b[31|\x1b[?7}cd", 2, 8),
        run_script(b"abcd", 2, 8),
    );
}

/// Feeds `chunks` into a fresh `rows × cols` terminal, one
/// [`Terminal::advance`] call each, and renders the full state.
fn run_chunks(chunks: &[&[u8]], rows: u16, cols: u16) -> String {
    let mut term = Terminal::new(rows, cols);
    for chunk in chunks {
        term.advance(chunk);
    }
    let requests = term.take_requests();
    common::render_state(&term, &requests)
}

#[test]
fn read_split_inside_a_two_byte_char_keeps_the_next_char() {
    // `vte` 0.15 over-reads when it completes a character left over from
    // the previous call: it consumes the following one-byte character too.
    assert_eq!(
        run_chunks(&[b"\xC3", b"\xA9a\xC3\xA9"], 1, 10),
        run_chunks(&[b"\xC3\xA9a\xC3\xA9"], 1, 10),
    );
    assert_eq!(
        run_chunks(&[b"\xD1", b"\x82 \xD0\xBC"], 1, 10),
        run_chunks(&[b"\xD1\x82 \xD0\xBC"], 1, 10),
    );
    let mut term = Terminal::new(1, 10);
    term.advance(b"\xD1");
    term.advance(b"\x82 \xD0\xBC");
    assert_eq!(term.grid().line(0).unwrap().text(), "т м");
}

#[test]
fn read_split_inside_an_invalid_sequence_keeps_the_escape() {
    assert_eq!(
        run_chunks(&[b"\xC2", b"\x80\x1b\x80 xy"], 1, 10),
        run_chunks(&[b"\xC2\x80\x1b\x80 xy"], 1, 10),
    );
}

#[test]
fn every_split_of_mixed_script_text_matches_one_call() {
    let text = "Съешь же ещё этих мягких булок, да выпей чаю. αβ γ שלום é a 😀 b\r\n".repeat(3);
    let bytes = text.as_bytes();
    let whole = run_chunks(&[bytes], 4, 30);
    for chunk_len in 1..=8 {
        let chunks: Vec<&[u8]> = bytes.chunks(chunk_len).collect();
        assert_eq!(run_chunks(&chunks, 4, 30), whole, "chunks of {chunk_len}");
    }
    for cut in 0..=bytes.len() {
        let (head, tail) = bytes.split_at(cut);
        assert_eq!(run_chunks(&[head, tail], 4, 30), whole, "cut at {cut}");
    }
}

// --------------------------------------------------------------- properties

/// Text that cannot start an escape sequence and is not a control.
fn printable() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop_oneof![b' '..=b'~', Just(b'\r'), Just(b'\n')], 0..200)
}

/// C0 controls (and C1, UTF-8 encoded) that the emulator ignores.
fn ignored_control() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop_oneof![
            0x00..=0x06u8,
            0x0E..=0x17u8,
            Just(0x18u8),
            Just(0x19u8),
            Just(0x1Au8),
            0x1C..=0x1Fu8,
            Just(0x7Fu8),
        ]
        .prop_map(|b| vec![b]),
        (0x80..=0x9Fu32).prop_map(|c| {
            char::from_u32(c)
                .expect("C1 range is valid")
                .to_string()
                .into_bytes()
        }),
    ]
}

/// Final bytes that ctlseqs assigns to nothing without an intermediate,
/// so a CSI ending in one stays a no-op as more sequences are wired up.
const UNASSIGNED_CSI_FINALS: &[u8] = b"NOQUVY[\\]_jko{|}~";

/// A well-formed CSI sequence with random parameters and an unassigned
/// final byte.
fn csi() -> impl Strategy<Value = Vec<u8>> {
    (
        prop::option::of(prop_oneof![Just(b'?'), Just(b'>'), Just(b'=')]),
        prop::collection::vec(0u16..1000, 0..4),
        prop::sample::select(UNASSIGNED_CSI_FINALS),
    )
        .prop_map(|(private, params, final_byte)| {
            let mut out = b"\x1b[".to_vec();
            out.extend(private);
            let params: Vec<String> = params.iter().map(u16::to_string).collect();
            out.extend_from_slice(params.join(";").as_bytes());
            out.push(final_byte);
            out
        })
}

/// Splits `text` into pieces and inserts one `noise` before each piece.
fn interleave(text: &[u8], cuts: &[usize], noise: &[Vec<u8>]) -> Vec<u8> {
    let mut cuts: Vec<usize> = cuts.iter().map(|c| c % (text.len() + 1)).collect();
    cuts.sort_unstable();
    let mut out = Vec::new();
    let mut start = 0;
    for (cut, noise) in cuts.into_iter().zip(noise) {
        out.extend_from_slice(&text[start..cut]);
        out.extend_from_slice(noise);
        start = cut;
    }
    out.extend_from_slice(&text[start..]);
    out
}

proptest! {
    #[test]
    fn one_bell_per_bel(
        bytes in prop::collection::vec(
            prop_oneof![b' '..=b'~', Just(0x07u8), Just(b'\r'), Just(b'\n'), Just(b'\t'), Just(0x08u8)],
            0..300,
        ),
        split in any::<prop::sample::Index>(),
    ) {
        let mut term = Terminal::new(4, 10);
        let (head, tail) = bytes.split_at(split.index(bytes.len() + 1));
        term.advance(head);
        term.advance(tail);
        let bels: usize = bytes.iter().map(|&b| usize::from(b == 0x07)).sum();
        prop_assert_eq!(term.take_requests(), vec![Request::Bell; bels]);
    }

    #[test]
    fn ignored_controls_leave_the_grid_unchanged(
        text in printable(),
        cuts in prop::collection::vec(any::<usize>(), 0..20),
        noise in prop::collection::vec(ignored_control(), 20),
    ) {
        let noisy = interleave(&text, &cuts, &noise);
        prop_assert_eq!(run_script(&noisy, 4, 10), run_script(&text, 4, 10));
        let mut term = Terminal::new(4, 10);
        term.advance(&noisy);
        prop_assert_eq!(term.take_requests(), vec![]);
    }

    #[test]
    fn csi_sequences_leave_the_grid_unchanged(
        text in printable(),
        cuts in prop::collection::vec(any::<usize>(), 0..10),
        noise in prop::collection::vec(csi(), 10),
    ) {
        let noisy = interleave(&text, &cuts, &noise);
        prop_assert_eq!(run_script(&noisy, 4, 10), run_script(&text, 4, 10));
    }

    #[test]
    fn auto_wrap_off_never_marks_a_line_wrapped(
        bytes in prop::collection::vec(
            prop_oneof![b' '..=b'~', Just(b'\r'), Just(b'\n'), Just(b'\t'), Just(0x08u8)],
            0..400,
        ),
        insert in any::<bool>(),
    ) {
        let mut term = Terminal::new(3, 7);
        term.set_auto_wrap(false);
        term.set_insert_mode(insert);
        term.advance(&bytes);
        let grid = term.grid();
        prop_assert!(grid.visible_lines().iter().all(|line| !line.wrapped));
        prop_assert!((0..grid.scrollback_len())
            .filter_map(|i| grid.scrollback_line(i))
            .all(|line| !line.wrapped));
    }

    #[test]
    fn chunking_does_not_change_the_result(
        bytes in prop::collection::vec(
            prop_oneof![
                b' '..=b'~',
                Just(0x07u8), Just(0x08u8), Just(b'\t'), Just(b'\n'), Just(b'\r'),
                Just(0x0Bu8), Just(0x0Cu8),
            ],
            0..300,
        ),
        split in any::<prop::sample::Index>(),
        insert in any::<bool>(),
        auto_wrap in any::<bool>(),
    ) {
        let run = |chunks: &[&[u8]]| {
            let mut term = Terminal::new(4, 9);
            term.set_insert_mode(insert);
            term.set_auto_wrap(auto_wrap);
            for chunk in chunks {
                term.advance(chunk);
            }
            let requests = term.take_requests();
            common::render_state(&term, &requests)
        };
        let (head, tail) = bytes.split_at(split.index(bytes.len() + 1));
        prop_assert_eq!(run(&[&bytes]), run(&[head, tail]));
    }

    #[test]
    fn chunking_non_ascii_text_does_not_change_the_result(
        chars in prop::collection::vec(
            prop_oneof![
                3 => (b' '..=b'~').prop_map(char::from),
                2 => any::<char>(),
                1 => Just('\n'), 1 => Just('\r'), 1 => Just('\u{7}'),
            ],
            0..200,
        ),
        sizes in prop::collection::vec(1usize..8, 1..64),
    ) {
        let text: String = chars.into_iter().collect();
        let bytes = text.as_bytes();
        let mut chunks = Vec::new();
        let mut rest = bytes;
        for size in sizes.iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (chunk, tail) = rest.split_at((*size).min(rest.len()));
            chunks.push(chunk);
            rest = tail;
        }
        prop_assert_eq!(run_chunks(&chunks, 4, 9), run_chunks(&[bytes], 4, 9));
    }

    #[test]
    fn chunking_arbitrary_bytes_does_not_change_the_result(
        bytes in prop::collection::vec(any::<u8>(), 0..300),
        cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..6),
    ) {
        let mut cuts: Vec<usize> = cuts.iter().map(|c| c.index(bytes.len() + 1)).collect();
        cuts.sort_unstable();
        let mut chunks = Vec::new();
        let mut start = 0;
        for cut in cuts {
            chunks.push(&bytes[start..cut]);
            start = cut;
        }
        chunks.push(&bytes[start..]);
        prop_assert_eq!(run_chunks(&chunks, 4, 9), run_chunks(&[&bytes], 4, 9));
    }
}

// ------------------------------------------------------------- the harness

#[test]
#[should_panic(expected = "need two hex digits")]
fn decode_rejects_a_signed_hex_byte() {
    decode("\\x+4");
}

#[test]
#[should_panic(expected = "bad `\\u{..}` escape")]
fn decode_rejects_a_signed_unicode_escape() {
    decode("\\u{+41}");
}
