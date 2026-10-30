//! esctest subset and vttest menu 2 screen 13 (SGR test pattern).
//!
//! `tests/fixtures/esctest_spec.txt` holds the esctest2 tests whose
//! sequences tessera-term implements, in the `csi_spec.txt` layout, with
//! ids `E_<esctest class>_<test>` and a `source:` line naming the esctest
//! file and function. Expected screens come from an independent model of
//! xterm that replays every esctest assertion, not from tessera-grid.
//! esctest tests that cannot pass yet are `#>` comment blocks naming the
//! issue that will make them pass; they are never cases or skipped tests.
//!
//! Every case runs through [`spec::check_all_in`] (whole and byte by byte),
//! its `attr:` rows are checked against the cell styles, and its rendered
//! state is snapshotted as `esctest__<id>.snap`.
//!
//! `tests/fixtures/esctest_pending.txt` holds the full blocks behind some
//! `#>` lines (ids `N_*`); [`pending_cases_still_fail`] requires each to
//! fail, so the PR that makes one pass lifts it.
//!
//! vttest case ids (`V<menu>S<screen>_...`) also have a script under
//! `tests/scripts/esctest/vttest_menu<m>_screen<s>.txt` that must match the
//! case. They live there rather than under `tests/scripts/vttest/`, whose
//! contents `tests/vttest.rs` pins to its own list.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;
#[allow(dead_code)] // Shared with tests/csi.rs and tests/vttest.rs.
mod spec;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use common::{Chunking, Script, pen_text, render_pens, render_state};
use spec::{Case, parse_fixture};

const FIXTURE: &str = include_str!("fixtures/esctest_spec.txt");

/// esctest cases that fail today, in the same layout (ids `N_*`); each
/// waits for the issue its `#>` line in [`FIXTURE`] names.
const PENDING: &str = include_str!("fixtures/esctest_pending.txt");

/// The esctest classes the fixture must cover (issue #15): one or more
/// `E_<class>_*` cases each. DECSC is exercised by the `DECRC` class
/// and SCOSC by the `SCORC` class (esctest's `save_restore_cursor.py`).
const CLASSES: &[&str] = &[
    "CUP", "CUU", "CUD", "CUF", "CUB", "CHA", "VPA", "CNL", "CPL", "ED", "EL", "ECH", "ICH", "DCH",
    "IL", "DL", "SU", "SD", "REP", "HTS", "TBC", "CBT", "CHT", "DECSTBM", "DECRC", "RI", "IND",
    "NEL", "SGR", "DECALN", "HPA", "HVP", "HPR", "VPR", "SCORC",
];

fn cases() -> Vec<Case> {
    parse_fixture(FIXTURE)
}

fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/esctest")
}

/// The esctest class of an `E_<class>_<test>` id.
fn esctest_class(id: &str) -> Option<&str> {
    let (class, test) = id.strip_prefix("E_")?.split_once('_')?;
    (!class.is_empty() && !test.is_empty()).then_some(class)
}

/// The text lines of each case block, keyed by id: everything from the
/// `### <id>` heading up to the next heading or `# --` line.
fn case_blocks() -> Vec<(&'static str, Vec<&'static str>)> {
    let mut blocks: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in FIXTURE.lines() {
        if let Some(id) = line.strip_prefix("### ") {
            blocks.push((id, Vec::new()));
        } else if line.starts_with("# --") || line.starts_with("#>") {
            // A section rule or a pending block ends the open case.
            blocks.push(("", Vec::new()));
        } else if let Some((_, lines)) = blocks.last_mut() {
            lines.push(line);
        }
    }
    blocks.retain(|(id, _)| !id.is_empty());
    blocks
}

/// The pending statements of a fixture: each `#>` line that starts with a
/// test id (`#> E_...`, not indented; `E_SCORC_*` names a whole class), joined with the `#>` lines after it
/// until the text holds a `->`, so an id list that wraps onto the next
/// line stays one statement. Indented `#>` lines after the `->` are prose
/// and are not part of it.
fn pending_statements(fixture: &str) -> Vec<String> {
    let mut statements: Vec<String> = Vec::new();
    // True while the last statement has not reached its `->` yet.
    let mut open = false;
    for line in fixture.lines() {
        let Some(rest) = line.strip_prefix("#>") else {
            open = false;
            continue;
        };
        let starts_with_id = rest.starts_with(' ') && !rest.starts_with("  ");
        if open {
            let current = statements.last_mut().expect("an open statement");
            current.push('\n');
            current.push_str(rest);
        } else if starts_with_id {
            statements.push(rest.to_owned());
        } else {
            continue;
        }
        open = !statements.last().is_some_and(|s| s.contains("->"));
    }
    statements
}

// ------------------------------------------------------------- attributes

/// What a case's `attr:` / `attr_others:` rows say about cell styles.
struct Attrs {
    /// `(row, col0, col1, pen)`, inclusive.
    runs: Vec<(u16, u16, u16, String)>,
    /// Pen of every cell no run covers.
    others: String,
}

fn parse_point(raw: &str) -> (u16, u16) {
    let inner = raw
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or_else(|| panic!("`{raw}` is not `(row,col)`"));
    let (row, col) = inner.split_once(',').expect("`(row,col)`");
    (
        row.trim().parse().expect("attr row"),
        col.trim().parse().expect("attr col"),
    )
}

/// The `attr:` rows of every case that has them, by id.
fn attrs() -> HashMap<&'static str, Attrs> {
    let mut out = HashMap::new();
    for (id, lines) in case_blocks() {
        let mut runs = Vec::new();
        let mut others = None;
        for line in lines {
            if let Some(rest) = line.strip_prefix("attr: ") {
                let (range, pen) = rest.split_once(' ').expect("`attr: <range> <pen>`");
                let (from, to) = range.split_once('-').expect("`(r,c0)-(r,c1)`");
                let ((row, col0), (row1, col1)) = (parse_point(from), parse_point(to));
                assert_eq!(row, row1, "{id}: an attr run stays on one row");
                assert!(col0 <= col1, "{id}: attr run {range} is reversed");
                runs.push((row, col0, col1, pen.trim().to_owned()));
            } else if let Some(pen) = line.strip_prefix("attr_others: ") {
                others = Some(pen.trim().to_owned());
            }
        }
        if !runs.is_empty() || others.is_some() {
            let others = others.unwrap_or_else(|| panic!("{id}: attr rows without attr_others"));
            out.insert(id, Attrs { runs, others });
        }
    }
    out
}

/// Every cell whose style differs from what `attrs` says.
fn attr_errors(case: &Case, attrs: &Attrs, chunking: Chunking) -> Vec<String> {
    let (term, _) = case.script.run(chunking);
    let grid = term.grid();
    let mut errors = Vec::new();
    for (row, line) in (0u16..).zip(grid.visible_lines()) {
        for (col, cell) in (0u16..).zip(&line.cells) {
            let want = attrs
                .runs
                .iter()
                .find(|(r, c0, c1, _)| *r == row && (*c0..=*c1).contains(&col))
                .map_or(&attrs.others, |(.., pen)| pen);
            let got = pen_text(cell.style);
            if got != *want {
                errors.push(format!(
                    "{} ({chunking:?}) cell ({row},{col}): {got}, expected {want}",
                    case.id
                ));
            }
        }
    }
    errors
}

// ------------------------------------------------------------------ tests

/// Screen, cursor, pending wrap, requests and scrollback of every case,
/// whole and byte by byte.
#[test]
fn cases_hold() {
    let checked = spec::check_all_in(FIXTURE, |_| true);
    assert_eq!(checked, case_blocks().len(), "every `###` block is a case");
}

/// The `attr:` rows match the cell styles, whole and byte by byte.
#[test]
fn attributes_hold() {
    let attrs = attrs();
    assert!(
        attrs.contains_key("E_SGR_bold") && attrs.contains_key("V2S13_sgr_pattern"),
        "the SGR cases carry attr rows"
    );
    let cases = cases();
    let mut errors = Vec::new();
    for (id, attrs) in &attrs {
        let case = cases
            .iter()
            .find(|case| case.id == *id)
            .unwrap_or_else(|| panic!("attr rows for unknown case {id}"));
        for chunking in [Chunking::Whole, Chunking::Bytewise] {
            errors.extend(attr_errors(case, attrs, chunking));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// The rendered state ([`render_state`] plus [`render_pens`]) of every
/// case, identical whole and byte by byte, as `esctest__<id>.snap`. Every
/// case runs even when an earlier one fails, and the test reports every
/// failing id.
#[test]
fn snapshots() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/esctest_spec.txt");
    let rows = cases().into_iter().map(|case| (case.id.clone(), case));
    common::check_table("esctest_spec.txt", case_blocks().len(), rows, |case| {
        let render = |chunking| {
            let (term, requests) = case.script.run(chunking);
            format!(
                "{}-- pens\n{}",
                render_state(&term, &requests),
                render_pens(&term)
            )
        };
        let whole = render(Chunking::Whole);
        assert_eq!(
            whole,
            render(Chunking::Bytewise),
            "{}: feeding byte by byte changed the result",
            case.id
        );
        insta::with_settings!({
            input_file => &fixture,
            omit_expression => true,
            description => case.id.as_str(),
        }, {
            insta::assert_snapshot!(case.id.as_str(), whole);
        });
    });
}

/// Every script under `tests/scripts/esctest/` is
/// `vttest_menu<m>_screen<s>.txt` and equals the fixture's `V<m>S<s>_*`
/// case, and every vttest case has its script.
#[test]
fn vttest_scripts_match_their_cases() {
    let mut on_disk: Vec<String> = std::fs::read_dir(scripts_dir())
        .expect("tests/scripts/esctest exists")
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .map(|path| {
            path.file_stem()
                .expect("file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    on_disk.sort();
    let mut expected = Vec::new();
    for case in cases().iter().filter(|case| spec::is_vttest(&case.id)) {
        let (menu, screen) = spec::vttest_screen(&case.id).expect("a vttest id");
        let name = format!("vttest_menu{menu}_screen{screen}");
        let script = Script::load(&scripts_dir().join(format!("{name}.txt")));
        assert_eq!(script, case.script, "{name}.txt does not match {}", case.id);
        expected.push(name);
    }
    expected.sort();
    assert_eq!(on_disk, expected, "one script per vttest case and no other");
}

/// The fixture's shape: at least 60 esctest cases over every listed class,
/// unique ids, a `source:` on every case, and every `#>` block naming the
/// issue that will make it pass.
#[test]
fn fixture_is_well_formed() {
    let blocks = case_blocks();
    let ids: Vec<&str> = blocks.iter().map(|(id, _)| *id).collect();
    let unique: HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len(), "duplicate case ids");

    for (id, lines) in &blocks {
        assert!(
            esctest_class(id).is_some() || spec::is_vttest(id),
            "{id}: ids are `E_<class>_<test>` or `V<menu>S<screen>_<name>`"
        );
        let source = lines
            .iter()
            .find_map(|line| line.strip_prefix("source: "))
            .unwrap_or_else(|| panic!("{id}: no `source:` line"));
        if esctest_class(id).is_some() {
            assert!(
                source.contains("esctest/tests/") && source.contains(" test_"),
                "{id}: source must name the esctest file and function: {source}"
            );
        }
    }

    let classes: HashSet<&str> = ids.iter().filter_map(|id| esctest_class(id)).collect();
    let esctest_cases = ids.iter().filter(|id| esctest_class(id).is_some()).count();
    assert!(esctest_cases >= 60, "only {esctest_cases} esctest cases");
    for class in CLASSES {
        assert!(classes.contains(class), "no E_{class}_* case");
    }

    let statements = pending_statements(FIXTURE);
    assert!(!statements.is_empty(), "no pending `#>` lines");
    for text in &statements {
        // The ids come before the first `->`, the issue right after it;
        // prose further on may mention live cases or other arrows.
        let (listed, target) = text
            .split_once("->")
            .unwrap_or_else(|| panic!("`#>` line has no `-> #N`:\n{text}"));
        let names_issue = target
            .trim_start()
            .strip_prefix('#')
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
        assert!(names_issue, "`#>` line names no issue (`-> #N`):\n{text}");
        let words: Vec<&str> = listed
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|word| !word.is_empty())
            .collect();
        assert!(
            words
                .iter()
                .any(|word| word.starts_with("E_") || spec::is_vttest(word)),
            "`#>` line lists no test id:\n{text}"
        );
        for word in words {
            assert!(
                !unique.contains(word),
                "{word} is both a case and pending:\n{text}"
            );
        }
    }
    let placeholder = FIXTURE
        .lines()
        .filter(|line| line.starts_with("#>"))
        .find(|line| line.contains("NEW ISSUE"));
    assert!(
        placeholder.is_none(),
        "placeholder left in: {placeholder:?}"
    );
}

/// Every `N_*` case in `esctest_pending.txt` still fails, whole and byte
/// by byte, so the PR that makes one pass has to lift it into
/// `esctest_spec.txt`. A file without cases passes: nothing is pending.
#[test]
fn pending_cases_still_fail() {
    let cases = parse_fixture(PENDING);
    // An empty list must mean "nothing pending", not "a block the parser
    // skipped": every `###` heading has to be a parsed case.
    let headings = PENDING
        .lines()
        .filter(|line| line.starts_with("###"))
        .count();
    assert_eq!(cases.len(), headings, "a pending block is not `### <id>`");
    for case in &cases {
        assert!(
            case.id.starts_with("N_"),
            "{}: pending ids are N_*",
            case.id
        );
        for chunking in [Chunking::Whole, Chunking::Bytewise] {
            assert!(
                !spec::check_case(case, chunking).is_empty(),
                "{} now passes ({chunking:?}): lift it into tests/fixtures/esctest_spec.txt as a \
                 live E_* case, replace its `#>` line there, and delete it from \
                 esctest_pending.txt",
                case.id
            );
        }
    }
}
