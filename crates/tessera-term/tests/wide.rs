//! Printing wide characters, grapheme clusters and zero-width scalars
//! through the emulator: auto-wrap (DECAWM) and insert mode (IRM) on top
//! of the grid's width-aware `put_char`.
//!
//! Expected behaviour comes from the term-layer rows of
//! `tessera-grid/tests/fixtures/wide_grapheme_spec.txt` (the grid-layer
//! rows run in `tessera-grid`). Its cell-level render and reader are
//! shared with that crate through a `#[path]` module, so a row's result
//! block means the same thing at both layers.
//!
//! - [`fixture_rows_hold`] runs every term-layer row, fed as written and
//!   byte by byte, and compares the render with the row's result block.
//! - One snapshot per `tests/scripts/wide/*.txt` script (one per row) pins
//!   the screen, the mode state and the cell-level render.
//! - A property test checks that chunking never changes the result.

mod common;
#[path = "../../tessera-grid/tests/common/mod.rs"]
mod grid_common;

use std::path::{Path, PathBuf};

use common::{Chunking, Script, run_script};
use grid_common::{Case, Layer, Step, assert_invariants, cases, render};
use proptest::prelude::*;
use tessera_term::Terminal;

// ------------------------------------------------------------------ fixture

/// Runs a term-layer row. `Bytewise` feeds every payload one byte per
/// call; `Whole` honours each step (`feed` in one call, `feed_bytewise`
/// per byte).
fn run_case(case: &Case, chunking: Chunking) -> Terminal {
    let mut term = Terminal::new(case.size.0, case.size.1);
    for step in &case.steps {
        match step {
            Step::Feed { text, .. } if chunking == Chunking::Whole => {
                term.advance(text.as_bytes());
            }
            Step::Feed { text, .. } | Step::FeedBytewise { text, .. } => {
                for byte in text.as_bytes() {
                    term.advance(std::slice::from_ref(byte));
                }
            }
            &Step::AutoWrap(on) => term.set_auto_wrap(on),
            &Step::InsertMode(on) => term.set_insert_mode(on),
            other => panic!("{}: {other:?} is a grid-layer step", case.id),
        }
    }
    term
}

fn term_cases() -> Vec<Case> {
    cases()
        .into_iter()
        .filter(|c| c.layer == Layer::Term)
        .collect()
}

#[test]
fn fixture_rows_hold() {
    let cases = term_cases();
    assert_eq!(cases.len(), 27, "term-layer rows");
    let mut errors = Vec::new();
    for case in &cases {
        for chunking in [Chunking::Whole, Chunking::Bytewise] {
            let term = run_case(case, chunking);
            let got = render(term.grid());
            if got != case.expected {
                errors.push(format!(
                    "{} ({chunking:?}):\nexpected:\n{}got:\n{got}",
                    case.id, case.expected
                ));
            }
            let broken = grid_common::invariant_errors(term.grid());
            if !broken.is_empty() {
                errors.push(format!("{} ({chunking:?}): {}", case.id, broken.join("; ")));
            }
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

// ------------------------------------------------------------------ scripts

fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/wide")
}

/// The term harness's view of a fixture row.
fn as_script(case: &Case) -> Script {
    let steps = case
        .steps
        .iter()
        .map(|step| match step {
            Step::Feed { text, .. } | Step::FeedBytewise { text, .. } => {
                common::Step::Feed(text.as_bytes().to_vec())
            }
            &Step::AutoWrap(on) => common::Step::AutoWrap(on),
            &Step::InsertMode(on) => common::Step::InsertMode(on),
            other => panic!("{}: {other:?} is a grid-layer step", case.id),
        })
        .collect();
    Script {
        rows: case.size.0,
        cols: case.size.1,
        steps,
    }
}

/// Runs `tests/scripts/wide/<name>.txt` whole and byte by byte, requires
/// both to agree and snapshots the state plus the cell-level render as
/// `wide__<name>.snap`.
fn snapshot_script(name: &str) {
    let path = scripts_dir().join(format!("{name}.txt"));
    let script = Script::load(&path);
    let rendered = |chunking| {
        let (term, _) = script.run(chunking);
        assert_invariants(name, term.grid());
        format!(
            "{}--\n{}",
            script.run_rendered(chunking),
            render(term.grid())
        )
    };
    let whole = rendered(Chunking::Whole);
    assert_eq!(
        whole,
        rendered(Chunking::Bytewise),
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
    c07_decawm_off_wide_with_one_column_left_is_dropped,
    c08_decawm_off_next_narrow_char_overwrites_last_col,
    c09_decawm_off_pending_then_wide_clears_pending_and_drops,
    c10_decawm_off_wide_that_fits_is_written,
    c11_decawm_off_wide_filling_the_line_then_narrow_splits_it,
    c13_one_column_grid_drops_wide_chars_term_with_insert_mode,
    c15_term_twin_of_c01_default_modes,
    c16_decawm_off_insert_mode_wide_with_one_column_left_changes_nothing,
    h01_insert_mode_wide_char_inserts_two_cells,
    h02_insert_mode_pushes_content_off_the_edge,
    h03_insert_mode_pending_wrap_wraps_first,
    h04_insert_mode_wide_with_one_column_left_blanks_wraps_inserts,
    h05_insert_mode_modifier_does_not_open_a_gap,
    h06_insert_mode_combining_mark_inserts_nothing,
    h07_insert_mode_mark_with_pending_wrap_does_not_wrap,
    h08_insert_mode_flag_growth_overwrites_instead_of_inserting,
    h09_insert_mode_wide_over_wide,
    h10_replace_mode_wide_over_narrow_text,
    h11_replace_mode_wide_over_the_middle_of_wide_chars,
    h12_decawm_off_mark_with_pending_attaches_to_last_cell,
    h13_decawm_off_vs16_at_the_last_column_is_dropped,
    h14_decawm_off_second_ri_at_the_last_column_overwrites,
    h15_decawm_on_flag_split_at_the_edge,
    h16_feed_bytewise_family,
    h17_feed_bytewise_flag_and_tone,
    h18_decawm_off_three_column_scalar_at_the_last_column_is_dropped,
    h19_insert_mode_three_column_scalar_inserts_two_cells,
}

#[test]
fn every_script_has_a_test() {
    let mut on_disk: Vec<String> = std::fs::read_dir(scripts_dir())
        .expect("tests/scripts/wide exists")
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

#[test]
fn every_fixture_row_has_a_script() {
    for case in term_cases() {
        let name = case.id.to_lowercase();
        let path = scripts_dir().join(format!("{name}.txt"));
        assert_eq!(
            Script::load(&path),
            as_script(&case),
            "{name}.txt does not match fixture row {}",
            case.id
        );
    }
}

// ----------------------------------------------------------- direct checks

#[test]
fn render_screen_shows_spacers_as_nothing_and_clusters_whole() {
    // 中 0..1, 文 2..3, e+U+0301 4, 👍🏽 5..6: the cursor is on blank column
    // 7, so the marker follows one space.
    assert_eq!(
        run_script("中文e\u{301}👍🏽".as_bytes(), 1, 10),
        "中文e\u{301}👍🏽 |\n"
    );
}

// ------------------------------------------------------------------ timing

/// Print throughput in ns per character on a 50x200 terminal, for the
/// workloads ADR 0008 quotes. Not a pass/fail test; run it optimised:
///
/// ```text
/// cargo test --release -p tessera-term --test wide -- --ignored --nocapture print_throughput
/// ```
#[test]
#[ignore = "timing harness; run with --release --ignored --nocapture"]
fn print_throughput() {
    let braille: String = (0..60)
        .map(|i| char::from_u32(0x2800 + (i * 37) % 256).expect("braille pattern"))
        .collect();
    let workloads = [
        (
            "ascii",
            "The quick brown fox jumps over the lazy dog 0123456789 abcdefghij",
        ),
        (
            "cjk",
            "中文字符测试日本語のテキスト한국어텍스트中文字符测试日本語のテキスト",
        ),
        (
            "latin",
            "héllo wörld café naïve résumé Ελληνικά Русский текст façade",
        ),
        (
            "box",
            "┌──────┬──────┐ │ cell │ cell │ ├──────┼──────┤ └──────┴──────┘ ▒▒",
        ),
        (
            "hebrew",
            "שלום עולם, זהו טקסט בעברית לבדיקה של המסוף והתצוגה",
        ),
        ("arabic", "مرحبا بالعالم هذا نص عربي لاختبار الطرفية"),
        (
            "vietnamese",
            "Tiếng Việt có dấu: điều này rất quan trọng đối với người dùng",
        ),
        (
            "punctuation",
            "“Quoted” and ‘single’ — dashes – and… ellipses • bullets ‘x’",
        ),
        ("braille", &braille),
        (
            "emoji",
            "Build 🚀 passed ✅ tests 🎉 deploy 🔥 ok 👍 done 😀 next ⭐ ship 🐛 fix",
        ),
        (
            "cjk+ascii",
            "ls -la 文件 目录 readme 说明.txt 日本語 test 한국어 main.rs 中文 ok",
        ),
        ("e+acute", &"e\u{301}".repeat(10)),
    ];
    for (name, line) in workloads {
        let lines = 200_000;
        let mut input = Vec::with_capacity((line.len() + 1) * lines);
        for _ in 0..lines {
            input.extend_from_slice(line.as_bytes());
            input.push(b'\r');
        }
        let chars = line.chars().count() * lines;
        let best = (0..5)
            .map(|_| {
                let mut term = Terminal::new(50, 200);
                let start = std::time::Instant::now();
                term.advance(&input);
                let elapsed = start.elapsed();
                std::hint::black_box(&term);
                elapsed
            })
            .min()
            .expect("five runs");
        #[expect(
            clippy::cast_precision_loss,
            reason = "display only; the counts are far below 2^52"
        )]
        let ns = best.as_nanos() as f64 / chars as f64;
        eprintln!("{name}: {ns:.1} ns/char");
    }
}

// --------------------------------------------------------------- properties

fn payload() -> impl Strategy<Value = String> {
    let piece = prop::sample::select(vec![
        "a", "b", " ", "\r", "\n", "\u{8}", "\t", "中", "文", "😀", "👍", "🏽", "👨", "\u{200D}",
        "👩", "🇸", "🇪", "☺", "⌚", "\u{FE0E}", "\u{FE0F}", "e", "\u{301}", "\u{302}", "\u{200B}",
        "क", "\u{93E}", "1", "\u{20E3}", "\u{1b}[", "\u{7f}",
    ]);
    prop::collection::vec(piece, 0..60).prop_map(|pieces| pieces.concat())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    /// I7: one `advance` call or one per byte gives the same grid, in
    /// every combination of DECAWM and IRM.
    #[test]
    fn chunking_never_changes_wide_output(
        rows in 1..=4u16,
        cols in 1..=9u16,
        auto_wrap in any::<bool>(),
        insert_mode in any::<bool>(),
        text in payload(),
    ) {
        let run = |bytewise: bool| {
            let mut term = Terminal::new(rows, cols);
            term.set_auto_wrap(auto_wrap);
            term.set_insert_mode(insert_mode);
            if bytewise {
                for byte in text.as_bytes() {
                    term.advance(std::slice::from_ref(byte));
                }
            } else {
                term.advance(text.as_bytes());
            }
            term
        };
        let whole = run(false);
        let errors = grid_common::invariant_errors(whole.grid());
        prop_assert!(errors.is_empty(), "{}\n{}", errors.join("; "), render(whole.grid()));
        prop_assert_eq!(render(whole.grid()), render(run(true).grid()));
    }
}
