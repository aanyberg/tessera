//! A fixture table run inside one test, with a `TESSERA_TABLE_ROW` row
//! filter.
//!
//! The same contract as `check_table` in `tessera-term`'s
//! `tests/common/mod.rs` (#143), copied rather than shared so neither test
//! crate depends on the other's helpers.

use std::fmt::Display;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// Runs `check` on every row of a fixture table in this one test, so the
/// table costs one test process instead of one per row.
///
/// A row fails when `check` panics (an `assert!`, a failed `insta`
/// snapshot, ...). The panic is caught and the next row runs, so one run
/// reports every failing row: each row's own panic message is printed as it
/// happens, and the final panic lists the ids of all failing rows.
///
/// To debug one row, set `TESSERA_TABLE_ROW` to its id (the row id's
/// `Display`, matched exactly) or to a comma-separated list of ids and run
/// the one test that holds the table, for example
/// `TESSERA_TABLE_ROW=a01_wrapped_line_split_and_rejoined cargo nextest run
/// -p tessera-grid --test reflow every_script_matches_its_snapshot`. Only
/// the matching rows run, and the row count check becomes "at least one row
/// matched": a filter that matches no row of the table fails the test, so a
/// typo cannot pass. The variable applies to every table in the run, so
/// name one test. An id in a list that matches no row while another does is
/// printed on stderr (`TESSERA_TABLE_ROW: no row <id> in <table>`), not
/// failed: the other ids may be another table's.
///
/// # Panics
///
/// When `expected_rows` is zero (an empty table with an empty fixture would
/// pass vacuously), when any row failed, or, unfiltered, when the table did
/// not have exactly `expected_rows` rows (so a row lost from a table cannot
/// pass silently).
pub fn check_table<I: Display, R>(
    table: &str,
    expected_rows: usize,
    rows: impl IntoIterator<Item = (I, R)>,
    check: impl FnMut(R),
) {
    let filter = std::env::var("TESSERA_TABLE_ROW").ok();
    check_table_filtered(table, expected_rows, rows, check, filter.as_deref());
}

/// [`check_table`] with the `TESSERA_TABLE_ROW` value passed in (`None`
/// when unset), so the filter is testable without touching the environment.
/// Returns the listed ids that matched no row, the ones printed on stderr
/// (none when unfiltered).
pub fn check_table_filtered<I: Display, R>(
    table: &str,
    expected_rows: usize,
    rows: impl IntoIterator<Item = (I, R)>,
    mut check: impl FnMut(R),
    filter: Option<&str>,
) -> Vec<String> {
    assert!(
        expected_rows > 0,
        "{table}: a table needs at least one row (expected_rows is 0)"
    );
    let wanted: Option<Vec<&str>> = filter.map(|filter| filter.split(',').collect());
    let mut ran = 0;
    let mut failed = Vec::new();
    let mut matched = Vec::new();
    for (id, row) in rows {
        let id = id.to_string();
        if wanted
            .as_ref()
            .is_some_and(|wanted| !wanted.contains(&id.as_str()))
        {
            continue;
        }
        if wanted.is_some() {
            matched.push(id.clone());
        }
        ran += 1;
        if catch_unwind(AssertUnwindSafe(|| check(row))).is_err() {
            failed.push(id);
        }
    }
    assert!(
        failed.is_empty(),
        "{table}: {} of {ran} rows failed (messages above): {}",
        failed.len(),
        failed.join(", ")
    );
    match filter {
        None => assert_eq!(
            ran, expected_rows,
            "{table}: rows run vs rows in the fixture"
        ),
        Some(filter) => assert!(
            ran > 0,
            "{table}: TESSERA_TABLE_ROW={filter} matched no row"
        ),
    }
    let unmatched: Vec<String> = wanted
        .unwrap_or_default()
        .into_iter()
        .filter(|id| !matched.iter().any(|m| m == id))
        .map(str::to_owned)
        .collect();
    for id in &unmatched {
        eprintln!("TESSERA_TABLE_ROW: no row {id} in {table}");
    }
    unmatched
}
