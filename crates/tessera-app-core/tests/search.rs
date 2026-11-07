//! Scrollback search through [`App`] (ADR 0038): the fixture's navigation
//! (E), live output (E10-E14), alternate screen (E15), cap (D30-D33) and
//! stepping (F01, F02) rows, on sessions without a child
//! (`App::spawn_test_session`, bytes fed with `App::feed_for_test`).

use std::fmt::Write as _;
use std::time::Duration;

use tessera_app_core::search::{MAX_MATCHES, MAX_MATCHES_TEXT_BYTES};
use tessera_app_core::{
    App, Config, ConfigPaths, Error, PtySize, QueryError, QueryOptions, ScrollTarget, SearchStatus,
    SessionId,
};
use tessera_render::{Overlay, OverlayKind};

fn app() -> App {
    App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"))
}

fn session(app: &App, rows: u16, cols: u16, scrollback: usize) -> SessionId {
    let id = app.spawn_test_session(PtySize::new(rows, cols)).unwrap();
    app.set_max_scrollback_for_test(id, scrollback).unwrap();
    id
}

const LITERAL: QueryOptions = QueryOptions {
    regex: false,
    case_sensitive: true,
};

const REGEX: QueryOptions = QueryOptions {
    regex: true,
    case_sensitive: true,
};

/// Steps the search until it reports done.
fn finish(app: &App, id: SessionId) -> SearchStatus {
    for _ in 0..1_000_000 {
        let status = app.search_step(id, Duration::from_millis(50)).unwrap();
        if status.done {
            return status;
        }
    }
    panic!("the search never finished");
}

/// The search overlays the next frame draws, as `(kind, row, start, end)`.
fn matches(app: &App, id: SessionId) -> Vec<(OverlayKind, u16, u16, u16)> {
    let (_, overlays) = app.view_decorations_for_test(id).unwrap();
    overlays
        .iter()
        .filter(|o| matches!(o.kind, OverlayKind::Match | OverlayKind::CurrentMatch))
        .map(|o: &Overlay| (o.kind, o.row, o.cols.start, o.cols.end))
        .collect()
}

/// The rows of the current match's overlays.
fn current_rows(app: &App, id: SessionId) -> Vec<u16> {
    matches(app, id)
        .into_iter()
        .filter(|m| m.0 == OverlayKind::CurrentMatch)
        .map(|m| m.1)
        .collect()
}

/// `count` lines `line <n>`, with `x` on the ones in `marked`.
fn numbered(count: usize, marked: &[usize]) -> String {
    let mut out = String::new();
    for n in 0..count {
        let mark = if marked.contains(&n) { " x" } else { "" };
        let _ = write!(out, "line {n}{mark}\r\n");
    }
    out
}

fn offset(app: &App, id: SessionId) -> usize {
    app.scroll_position(id).unwrap().offset
}

/// E01-E04: the newest match starts current; newer wraps to the oldest,
/// older steps back and wraps to the newest.
#[test]
fn e01_to_e04_navigation_wraps_both_ways() {
    let app = app();
    let id = session(&app, 5, 20, 100);
    // Lines 2, 5 and 8 match; 10 more lines push them into the history.
    app.feed_for_test(id, numbered(20, &[2, 5, 8]).as_bytes())
        .unwrap();
    app.search_start(id, "x", LITERAL).unwrap();
    let status = finish(&app, id);
    assert_eq!((status.matches, status.current), (3, Some(2)), "E01");
    assert!(!status.truncated);

    let status = app.search_newer(id).unwrap();
    assert_eq!(status.current, Some(0), "E02: newer wraps to the oldest");
    let shown = current_rows(&app, id);
    assert_eq!(shown.len(), 1, "E02: the view scrolled to line 2");

    let status = app.search_older(id).unwrap();
    assert_eq!(status.current, Some(2), "E03: older wraps to the newest");

    let status = app.search_older(id).unwrap();
    assert_eq!(status.current, Some(1), "E04: older steps toward older");
}

/// E05/E06: one match stays current; no match is no current and no
/// scroll.
#[test]
fn e05_e06_one_match_and_none() {
    let app = app();
    let id = session(&app, 5, 20, 100);
    app.feed_for_test(id, b"only x here").unwrap();
    app.search_start(id, "x", LITERAL).unwrap();
    finish(&app, id);
    for _ in 0..2 {
        assert_eq!(app.search_newer(id).unwrap().current, Some(0));
        assert_eq!(app.search_older(id).unwrap().current, Some(0));
    }
    assert_eq!(offset(&app, id), 0, "E05: a visible match does not scroll");

    app.search_start(id, "zzz", LITERAL).unwrap();
    let status = finish(&app, id);
    assert_eq!((status.matches, status.current), (0, None));
    assert_eq!(app.search_newer(id).unwrap().current, None);
    assert_eq!(app.search_older(id).unwrap().current, None);
    assert_eq!(offset(&app, id), 0, "E06: no scroll");
    assert!(status.active, "E06: no match is not an error");
}

/// E07: moving to a match outside the view scrolls the view so it shows
/// it; E08: both rows of a match across a soft wrap.
#[test]
fn e07_e08_moving_scrolls_the_match_into_view() {
    let app = app();
    let id = session(&app, 5, 10, 20_000);
    let mut text = String::from("hit\r\n");
    text.push_str(&numbered(10_000, &[]));
    app.feed_for_test(id, text.as_bytes()).unwrap();
    app.search_start(id, "hit", LITERAL).unwrap();
    let status = finish(&app, id);
    assert_eq!(status.matches, 1);
    assert!(current_rows(&app, id).is_empty(), "the match is far above");
    app.search_newer(id).unwrap();
    assert_eq!(
        current_rows(&app, id).len(),
        1,
        "E07: scrolled to the match"
    );
    let scrolled = offset(&app, id);
    assert!(scrolled > 9_990, "{scrolled}");
    // Already in view: no scroll.
    app.search_newer(id).unwrap();
    assert_eq!(offset(&app, id), scrolled);

    // E08: `abcdefghij` + `klm` wraps after `j`; `ijkl` spans both rows.
    let id = session(&app, 5, 10, 20_000);
    let mut text = String::from("abcdefghijklm\r\n");
    text.push_str(&numbered(100, &[]));
    app.feed_for_test(id, text.as_bytes()).unwrap();
    app.search_start(id, "ijkl", LITERAL).unwrap();
    finish(&app, id);
    // Show only the second row of the match: its line is at the top.
    let lines = app.scroll_position(id).unwrap().history;
    app.scroll_to(id, ScrollTarget::Offset(lines - 1)).unwrap();
    app.search_newer(id).unwrap();
    let rows = current_rows(&app, id);
    assert_eq!(rows.len(), 2, "E08: both rows in view: {rows:?}");
    assert_eq!(
        matches(&app, id),
        vec![
            (OverlayKind::CurrentMatch, rows[0], 8, 10),
            (OverlayKind::CurrentMatch, rows[0] + 1, 0, 2),
        ]
    );
}

/// E09: the current match gets `CurrentMatch` on each row, the others
/// Match; a wide character covers two columns; matches outside the view
/// draw nothing.
#[test]
fn e09_overlays_mark_the_current_match() {
    let app = app();
    let id = session(&app, 3, 10, 100);
    app.feed_for_test(id, "中 a\r\n中 b\r\n\r\n中 c".as_bytes())
        .unwrap();
    app.search_start(id, "中", LITERAL).unwrap();
    let status = finish(&app, id);
    assert_eq!((status.matches, status.current), (3, Some(2)));
    assert_eq!(
        matches(&app, id),
        vec![
            (OverlayKind::Match, 0, 0, 2),
            (OverlayKind::CurrentMatch, 2, 0, 2),
        ],
        "the first match is in the history, out of view"
    );
    // A frame after a move or a new query draws the new state, not the
    // overlays the last frame reused.
    app.search_older(id).unwrap();
    assert_eq!(
        matches(&app, id),
        vec![
            (OverlayKind::CurrentMatch, 0, 0, 2),
            (OverlayKind::Match, 2, 0, 2),
        ]
    );
    app.search_start(id, "b", LITERAL).unwrap();
    assert_eq!(matches(&app, id), vec![], "nothing found before a step");
    finish(&app, id);
    assert_eq!(
        matches(&app, id),
        vec![(OverlayKind::CurrentMatch, 0, 3, 4)]
    );
}

/// E10: new output adds matches and keeps the current one.
#[test]
fn e10_new_output_is_searched_and_current_kept() {
    let app = app();
    let id = session(&app, 5, 20, 100);
    app.feed_for_test(id, b"err 1\r\nok\r\nerr 2\r\n").unwrap();
    app.search_start(id, "err", LITERAL).unwrap();
    let status = finish(&app, id);
    assert_eq!((status.matches, status.current), (2, Some(1)));
    app.search_older(id).unwrap();
    app.feed_for_test(id, numbered(10, &[]).as_bytes()).unwrap();
    app.feed_for_test(id, b"err 3").unwrap();
    let status = finish(&app, id);
    assert_eq!(status.matches, 3, "E10");
    assert_eq!(status.current, Some(0), "E10: current is the same match");
}

/// E11: an evicted line takes its matches along; the current match moves
/// to the next one in document order; no overlay is drawn where the line
/// was.
#[test]
fn e11_eviction_drops_matches() {
    let app = app();
    let id = session(&app, 3, 20, 10);
    app.feed_for_test(id, b"x old\r\n").unwrap();
    app.feed_for_test(id, numbered(4, &[]).as_bytes()).unwrap();
    app.feed_for_test(id, b"x mid\r\n").unwrap();
    app.feed_for_test(id, numbered(4, &[]).as_bytes()).unwrap();
    app.search_start(id, "x", LITERAL).unwrap();
    let status = finish(&app, id);
    assert_eq!(status.matches, 2);
    let status = app.search_newer(id).unwrap();
    assert_eq!(status.current, Some(0), "the oldest is current");
    // Ten more lines evict `x old`.
    app.feed_for_test(id, numbered(3, &[]).as_bytes()).unwrap();
    let status = finish(&app, id);
    assert_eq!((status.matches, status.current), (1, Some(0)), "E11");
    app.scroll_to(id, ScrollTarget::Top).unwrap();
    let shown = matches(&app, id);
    assert!(
        shown.iter().all(|m| m.0 == OverlayKind::CurrentMatch),
        "only `x mid` remains: {shown:?}"
    );
}

/// E11, a wrapped line cut by eviction: what is left of it is searched
/// again as a line of its own, so the search agrees with a fresh one. The
/// match `aaab` loses its first row; the rest, `ab `, matches `ab`.
#[test]
fn e11_eviction_into_a_wrapped_line_searches_its_rest_again() {
    let app = app();
    let id = session(&app, 2, 3, 3);
    // Rows ` aa`, `ab ` (one wrapped line), then two that push it into
    // the history, where it is scanned.
    app.feed_for_test(id, b" aaab\r\nc\r\nc").unwrap();
    app.search_start(id, "a[ab]*b", REGEX).unwrap();
    finish(&app, id);
    assert_eq!(app.search_matches_text(id).unwrap().text, "aaab");
    // Two more rows evict ` aa`.
    app.feed_for_test(id, b"\r\nc\r\nc").unwrap();
    app.with_terminal(id, |term| {
        let grid = term.grid();
        let first = grid.scrollback_line(0).unwrap();
        let text: String = first.cells.iter().map(|cell| cell.ch).collect();
        assert_eq!(text, "ab ", "` aa` was evicted");
    })
    .unwrap();
    let status = finish(&app, id);
    let live = (status.matches, app.search_matches_text(id).unwrap().text);
    app.search_start(id, "a[ab]", REGEX).unwrap();
    app.search_start(id, "a[ab]*b", REGEX).unwrap();
    let status = finish(&app, id);
    let fresh = (status.matches, app.search_matches_text(id).unwrap().text);
    assert_eq!(fresh, (1, "ab".to_owned()));
    assert_eq!(live, fresh);
}

/// E12: clearing the scrollback (CSI 3 J) leaves the screen's matches.
#[test]
fn e12_clearing_the_scrollback_keeps_the_screens_matches() {
    let app = app();
    let id = session(&app, 3, 20, 100);
    app.feed_for_test(id, numbered(20, &[1, 2, 3, 19]).as_bytes())
        .unwrap();
    app.search_start(id, "x", LITERAL).unwrap();
    assert_eq!(finish(&app, id).matches, 4);
    app.feed_for_test(id, b"\x1b[3J").unwrap();
    let status = finish(&app, id);
    assert_eq!((status.matches, status.current), (1, Some(0)), "E12");
    assert_eq!(
        matches(&app, id),
        vec![(OverlayKind::CurrentMatch, 1, 8, 9)]
    );
}

/// E13: RIS drops every match but keeps the query: later output that
/// matches is found.
#[test]
fn e13_ris_drops_matches_and_keeps_the_query() {
    let app = app();
    let id = session(&app, 3, 20, 100);
    app.feed_for_test(id, numbered(20, &[1, 2, 3, 19]).as_bytes())
        .unwrap();
    app.search_start(id, "x", LITERAL).unwrap();
    assert_eq!(finish(&app, id).matches, 4);
    app.feed_for_test(id, b"\x1bc").unwrap();
    let status = finish(&app, id);
    assert_eq!((status.matches, status.current), (0, None), "E13");
    assert!(status.active);
    assert!(matches(&app, id).is_empty(), "no stale overlay");
    app.feed_for_test(id, b"new x").unwrap();
    assert_eq!(finish(&app, id).matches, 1);
}

/// E14: a screen row overwritten loses its match and its overlay.
#[test]
fn e14_overwritten_screen_row_is_rescanned() {
    let app = app();
    let id = session(&app, 5, 20, 100);
    app.feed_for_test(id, b"a\r\nb\r\nerr\r\nd").unwrap();
    app.search_start(id, "err", LITERAL).unwrap();
    assert_eq!(finish(&app, id).matches, 1);
    assert_eq!(matches(&app, id).len(), 1);
    app.feed_for_test(id, b"\x1b[3;1Hok \x1b[K").unwrap();
    assert_eq!(finish(&app, id).matches, 0, "E14");
    assert!(matches(&app, id).is_empty(), "no stale overlay");
}

/// The match counts of a search started after `setup` and kept live while
/// `more` arrives, and of a fresh search of a session fed both.
fn live_and_fresh(
    (rows, cols): (u16, u16),
    pattern: &str,
    setup: &str,
    more: &str,
) -> (usize, usize) {
    let app = app();
    let live = session(&app, rows, cols, 1000);
    app.feed_for_test(live, setup.as_bytes()).unwrap();
    app.search_start(live, pattern, REGEX).unwrap();
    finish(&app, live);
    app.feed_for_test(live, more.as_bytes()).unwrap();
    let fresh = session(&app, rows, cols, 1000);
    app.feed_for_test(fresh, format!("{setup}{more}").as_bytes())
        .unwrap();
    app.search_start(fresh, pattern, REGEX).unwrap();
    (finish(&app, live).matches, finish(&app, fresh).matches)
}

/// #46a adversarial H1: ED 2 ends the newest history line, which wrapped
/// into the erased top row (#86). `x$` now matches at its end; the live
/// search must not keep its copy from when it wrapped.
#[test]
fn ed2_ending_the_newest_history_line_is_searched_again() {
    let setup = "xxxxxxxxxxyyy\r\n\r\n";
    assert_eq!(live_and_fresh((3, 10), "x$", setup, "\x1b[2J"), (1, 1));
}

/// #46a adversarial P1: the blanks at the end of a wrapped history row are
/// text only while it wraps. After `CSI H CSI 2 J` (xterm's clear) they
/// are trailing blanks, and `o\s+` no longer matches.
#[test]
fn clearing_the_screen_drops_a_match_on_the_newest_history_line() {
    let setup = "foo       bar\r\n\r\n";
    assert_eq!(
        live_and_fresh((3, 10), r"o\s+", setup, "\x1b[H\x1b[2J"),
        (0, 0)
    );
}

/// #46a variant: the newest history line ends a logical line of more than a
/// segment period, cut exactly at the screen's top row, so it was searched
/// as history, not with the screen. ED 2 ends it there.
#[test]
fn ed2_ending_a_long_line_cut_at_the_top_is_searched_again() {
    // 128 full rows wrap into "yyy"; two more rows push the screen top to
    // absolute row 128, a multiple of the period at 10 columns.
    let setup = format!("{}yyy\r\n\r\n", "x".repeat(1280));
    assert_eq!(live_and_fresh((3, 10), "x$", &setup, "\x1b[2J"), (1, 1));
}

/// E15 (core part): the alternate screen shows and counts only its own
/// matches; the primary's come back when it ends.
#[test]
fn e15_alternate_screen_keeps_the_primary_matches() {
    let app = app();
    let id = session(&app, 3, 20, 100);
    app.feed_for_test(id, numbered(10, &[1, 9]).as_bytes())
        .unwrap();
    app.search_start(id, "x", LITERAL).unwrap();
    assert_eq!(finish(&app, id).matches, 2);
    app.feed_for_test(id, b"\x1b[?1049h\x1b[2;1Halt x and x")
        .unwrap();
    let status = finish(&app, id);
    assert_eq!(
        (status.matches, status.current),
        (2, Some(1)),
        "alt matches"
    );
    assert_eq!(
        matches(&app, id),
        vec![
            (OverlayKind::Match, 1, 4, 5),
            (OverlayKind::CurrentMatch, 1, 10, 11),
        ]
    );
    app.feed_for_test(id, b"\x1b[?1049l").unwrap();
    let status = finish(&app, id);
    assert_eq!(status.matches, 2, "the primary's matches are back");
}

/// E16: clearing ends the search and its overlays.
#[test]
fn e16_clear_ends_the_search() {
    let app = app();
    let id = session(&app, 3, 20, 100);
    app.feed_for_test(id, b"x").unwrap();
    app.search_start(id, "x", LITERAL).unwrap();
    finish(&app, id);
    assert_eq!(matches(&app, id).len(), 1);
    app.search_clear(id).unwrap();
    assert_eq!(app.search_status(id).unwrap(), SearchStatus::default());
    assert_eq!(matches(&app, id), vec![]);
    // Stepping without a search does nothing.
    assert_eq!(
        app.search_step(id, Duration::ZERO).unwrap(),
        SearchStatus::default()
    );
}

/// E17: an identical query keeps the results (and the current match); a
/// different one starts over.
#[test]
fn e17_identical_query_keeps_results() {
    let app = app();
    let id = session(&app, 5, 20, 100);
    app.feed_for_test(id, b"a\r\nab\r\nabc").unwrap();
    app.search_start(id, "a", LITERAL).unwrap();
    finish(&app, id);
    app.search_newer(id).unwrap();
    app.search_start(id, "a", LITERAL).unwrap();
    assert_eq!(app.search_status(id).unwrap().current, Some(0), "kept");
    app.search_start(id, "ab", LITERAL).unwrap();
    let status = app.search_status(id).unwrap();
    assert_eq!((status.matches, status.current), (0, None), "restarted");
    let status = finish(&app, id);
    assert_eq!((status.matches, status.current), (2, Some(1)));
    // The case flag is part of the query.
    app.search_start(id, "ab", QueryOptions::default()).unwrap();
    assert_eq!(app.search_status(id).unwrap().matches, 0);
}

/// D13: a refused query ends the earlier search; the next valid one
/// works. The error does not quote the query (S8).
#[test]
fn d13_refused_query_clears_the_search() {
    let app = app();
    let id = session(&app, 3, 10, 100);
    app.feed_for_test(id, b"abc").unwrap();
    app.search_start(id, "b", REGEX).unwrap();
    assert_eq!(finish(&app, id).matches, 1);
    let error = app.search_start(id, "(secret", REGEX).unwrap_err();
    assert!(
        matches!(error, Error::Query(QueryError::Syntax(_))),
        "{error:?}"
    );
    assert!(!error.to_string().contains("secret"), "S8: {error}");
    assert_eq!(app.search_status(id).unwrap(), SearchStatus::default());
    assert_eq!(matches(&app, id), vec![]);
    app.search_start(id, "b", REGEX).unwrap();
    assert_eq!(finish(&app, id).matches, 1);
    let long = "z".repeat(1025);
    assert!(matches!(
        app.search_start(id, &long, LITERAL),
        Err(Error::Query(QueryError::TooLong { bytes: 1025 }))
    ));
}

/// An empty query is no search.
#[test]
fn empty_query_is_idle() {
    let app = app();
    let id = session(&app, 3, 10, 100);
    app.search_start(id, "", LITERAL).unwrap();
    assert!(!app.search_status(id).unwrap().active);
}

/// D30: past [`MAX_MATCHES`] the oldest give way; exactly the cap is not
/// truncated.
#[test]
fn d30_match_cap_keeps_the_newest() {
    let app = app();
    let id = session(&app, 24, 100, 120_000);
    let mut text = String::new();
    for n in 0..110_000 {
        let _ = write!(text, "a{n:06}\r\n");
    }
    app.feed_for_test(id, text.as_bytes()).unwrap();
    app.search_start(id, r"a\d+", REGEX).unwrap();
    let status = finish(&app, id);
    assert_eq!(status.matches, MAX_MATCHES);
    assert!(status.truncated);
    let text = app.search_matches_text(id).unwrap();
    assert!(!text.truncated);
    let first = text.text.lines().next().unwrap();
    assert_eq!(first, "a010000", "the oldest 10 000 lines have none");
    assert_eq!(text.text.lines().last().unwrap(), "a109999");

    // New output past the cap: the oldest give way to the newest.
    let mut more = String::new();
    for n in 110_000..115_000 {
        let _ = write!(more, "a{n:06}\r\n");
    }
    app.feed_for_test(id, more.as_bytes()).unwrap();
    let status = finish(&app, id);
    assert_eq!(status.matches, MAX_MATCHES);
    assert!(status.truncated);
    let text = app.search_matches_text(id).unwrap();
    assert_eq!(text.text.lines().next().unwrap(), "a015000");
    assert_eq!(text.text.lines().last().unwrap(), "a114999");

    // Exactly the cap: a010000 to a109999.
    app.search_start(id, r"a(0[1-9]|10)\d{4}", REGEX).unwrap();
    let status = finish(&app, id);
    assert_eq!(status.matches, MAX_MATCHES);
    assert!(!status.truncated, "the cap triggers on the next match");
}

/// D31: one logical line flooding matches is cut at the cap inside the
/// line.
#[test]
fn d31_single_line_flood_is_capped() {
    let app = app();
    let id = session(&app, 24, 2048, 1_000);
    app.feed_for_test(id, "a".repeat(2048 * 100).as_bytes())
        .unwrap();
    app.search_start(id, "a", LITERAL).unwrap();
    let status = finish(&app, id);
    assert_eq!(status.matches, MAX_MATCHES);
    assert!(status.truncated);
}

/// D32: [`App::search_matches_text`] stops before the match that would
/// pass [`MAX_MATCHES_TEXT_BYTES`].
#[test]
fn d32_matches_text_is_capped_on_a_match_boundary() {
    let app = app();
    let id = session(&app, 24, 100, 120_000);
    let line = format!("{}\r\n", "x".repeat(100));
    app.feed_for_test(id, line.repeat(20_000).as_bytes())
        .unwrap();
    app.search_start(id, "x+", REGEX).unwrap();
    let status = finish(&app, id);
    assert_eq!(status.matches, 20_000);
    let text = app.search_matches_text(id).unwrap();
    assert!(text.truncated);
    assert!(text.text.len() <= MAX_MATCHES_TEXT_BYTES);
    assert_eq!((text.text.len() + 1) % 101, 0, "whole matches only");
    assert!(text.text.len() + 101 > MAX_MATCHES_TEXT_BYTES);
}

/// D33: a match across a soft wrap is one line of text; a wide character
/// appears once.
#[test]
fn d33_matches_text_joins_wraps_and_wide_cells() {
    let app = app();
    let id = session(&app, 5, 5, 100);
    app.feed_for_test(id, "hello 中文\r\nhello".as_bytes())
        .unwrap();
    app.search_start(id, "o 中", LITERAL).unwrap();
    finish(&app, id);
    let text = app.search_matches_text(id).unwrap();
    assert_eq!(text.text, "o 中");
    assert!(!text.truncated);
    app.search_start(id, "hello", LITERAL).unwrap();
    finish(&app, id);
    assert_eq!(app.search_matches_text(id).unwrap().text, "hello\nhello");
}

/// F01: wrapped lines on both sides of every chunk boundary are found
/// once: 600 three-row lines, each with a match across its first wrap.
#[test]
fn f01_no_match_lost_or_doubled_at_chunk_boundaries() {
    let app = app();
    let id = session(&app, 24, 80, 10_000);
    let line = format!("{}abcd{}\r\n", " ".repeat(78), "-".repeat(160));
    app.feed_for_test(id, line.repeat(600).as_bytes()).unwrap();
    app.search_start(id, "abcd", LITERAL).unwrap();
    // One chunk per step.
    let mut status = app.search_step(id, Duration::ZERO).unwrap();
    let mut steps = 1;
    while !status.done {
        status = app.search_step(id, Duration::ZERO).unwrap();
        steps += 1;
    }
    assert_eq!(status.matches, 600);
    assert!(steps > 4, "the scan took {steps} chunks");
    let text = app.search_matches_text(id).unwrap();
    assert!(text.text.lines().all(|l| l == "abcd"));
}

/// F02 and the reflow row: a resize starts the search over in the new
/// layout; no overlay of the old width is drawn.
#[test]
fn f02_resize_starts_over() {
    let app = app();
    let id = session(&app, 5, 10, 100);
    app.feed_for_test(id, b"0123456789abc\r\n").unwrap();
    app.search_start(id, "9a", LITERAL).unwrap();
    finish(&app, id);
    assert_eq!(
        matches(&app, id),
        vec![
            (OverlayKind::CurrentMatch, 0, 9, 10),
            (OverlayKind::CurrentMatch, 1, 0, 1),
        ]
    );
    app.resize(id, PtySize::new(5, 20)).unwrap();
    assert!(matches(&app, id).is_empty(), "no overlay of the old width");
    let status = finish(&app, id);
    assert_eq!((status.matches, status.current), (1, Some(0)));
    assert_eq!(
        matches(&app, id),
        vec![(OverlayKind::CurrentMatch, 0, 9, 11)],
        "found again in one row"
    );
}

/// Step budgets: a zero budget does one chunk; the count grows as the
/// scan goes on (E18).
#[test]
fn e18_count_is_live_while_scanning() {
    let app = app();
    let id = session(&app, 24, 80, 20_000);
    app.feed_for_test(
        id,
        numbered(10_000, &(0..10_000).collect::<Vec<_>>()).as_bytes(),
    )
    .unwrap();
    app.search_start(id, "x", LITERAL).unwrap();
    let first = app.search_step(id, Duration::ZERO).unwrap();
    assert!(!first.done);
    assert!(first.matches > 0 && first.matches < 10_000, "{first:?}");
    let second = app.search_step(id, Duration::ZERO).unwrap();
    assert!(second.matches > first.matches);
    assert_eq!(finish(&app, id).matches, 10_000);
}

/// Unknown sessions are errors.
#[test]
fn unknown_session() {
    let app = app();
    let id = SessionId::from_raw(999);
    assert!(matches!(
        app.search_start(id, "x", LITERAL),
        Err(Error::SessionNotFound(_))
    ));
    assert!(app.search_step(id, Duration::ZERO).is_err());
    assert!(app.search_older(id).is_err());
    assert!(app.search_newer(id).is_err());
    assert!(app.search_clear(id).is_err());
    assert!(app.search_status(id).is_err());
    assert!(app.search_matches_text(id).is_err());
}

/// One thing that happens to a session while a search runs.
#[derive(Debug, Clone)]
enum Op {
    /// Output: text, line ends, wide characters and screen controls.
    Feed(String),
    /// A step of the search with a zero budget (one chunk).
    Step,
    /// A resize to these rows and columns.
    Resize(u16, u16),
    /// A frame's look at the overlays (which the next look may reuse).
    Look,
    /// A move of the current match, older or newer.
    Move(bool),
}

fn op() -> impl proptest::strategy::Strategy<Value = Op> {
    use proptest::prelude::*;
    let piece = prop_oneof![
        8 => "[ab ]{1,12}",
        4 => Just("\r\n".to_owned()),
        1 => Just("\u{4e00}".to_owned()),
        1 => Just("e\u{301}".to_owned()),
        1 => Just("\x1b[2J".to_owned()),
        1 => Just("\x1b[3J".to_owned()),
        1 => Just("\x1b[H".to_owned()),
        1 => Just("\x1b[2;3H".to_owned()),
        1 => Just("\x1b[K".to_owned()),
        1 => Just("\x1b[?1049h".to_owned()),
        1 => Just("\x1b[?1049l".to_owned()),
        1 => Just("\x1bc".to_owned()),
    ];
    prop_oneof![
        6 => proptest::collection::vec(piece, 1..12).prop_map(|p| Op::Feed(p.concat())),
        4 => Just(Op::Step),
        1 => (2u16..6, 3u16..10).prop_map(|(rows, cols)| Op::Resize(rows, cols)),
        2 => Just(Op::Look),
        1 => proptest::bool::ANY.prop_map(Op::Move),
    ]
}

/// What a search shows: the count, the text of every match, and the
/// visible matches' cells (current or not).
fn outcome(app: &App, id: SessionId) -> (usize, String, Vec<(u16, u16, u16)>) {
    let status = finish(app, id);
    let text = app.search_matches_text(id).unwrap().text;
    let mut cells: Vec<_> = matches(app, id)
        .into_iter()
        .map(|(_, row, start, end)| (row, start, end))
        .collect();
    cells.sort_unstable();
    (status.matches, text, cells)
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(96))]

    /// The live search (stepped one chunk at a time while output, clears,
    /// the alternate screen, RIS and resizes arrive, then finished) finds
    /// what a fresh search of the final state finds: the same count, the
    /// same match texts and the same highlighted cells.
    #[test]
    fn a_live_search_agrees_with_a_fresh_one(
        ops in proptest::collection::vec(op(), 1..40),
        regex in proptest::bool::ANY,
        scrollback in 0usize..40,
    ) {
        let app = app();
        let id = session(&app, 4, 6, scrollback);
        let (pattern, options) = if regex {
            ("a[ab]*b|\u{4e00}", REGEX)
        } else {
            ("ab", LITERAL)
        };
        app.search_start(id, pattern, options).unwrap();
        for op in &ops {
            match op {
                Op::Feed(text) => app.feed_for_test(id, text.as_bytes()).unwrap(),
                Op::Step => {
                    app.search_step(id, Duration::ZERO).unwrap();
                }
                Op::Resize(rows, cols) => app.resize(id, PtySize::new(*rows, *cols)).unwrap(),
                Op::Look => {
                    matches(&app, id);
                }
                Op::Move(older) => {
                    if *older {
                        app.search_older(id).unwrap();
                    } else {
                        app.search_newer(id).unwrap();
                    }
                }
            }
        }
        let live = outcome(&app, id);
        app.search_clear(id).unwrap();
        app.search_start(id, pattern, options).unwrap();
        let fresh = outcome(&app, id);
        proptest::prop_assert_eq!(live, fresh);
    }
}
