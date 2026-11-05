//! OSC 8 hyperlinks in the grid (ADR 0026): which cells carry a link, how
//! the link follows its cell through every edit, reflow and history, and
//! the bounds on the number of live links.
//!
//! The VT-level rows are in `tessera-term/tests/hyperlink.rs`.

use proptest::prelude::*;
use tessera_grid::{
    EraseDisplay, EraseLine, Grid, Hyperlink, LinkRefused, MAX_ID_BYTES, MAX_LINK_TABLE_BYTES,
    MAX_LIVE_LINKS, MAX_URI_BYTES,
};

/// The link URI of each cell of row `row`.
fn uris(grid: &Grid, row: u16) -> Vec<Option<String>> {
    let line = grid.line(row).expect("row");
    (0..line.cells.len())
        .map(|col| line.hyperlink(col).map(|l| l.uri().to_owned()))
        .collect()
}

fn linked(grid: &Grid, row: u16) -> Vec<usize> {
    uris(grid, row)
        .iter()
        .enumerate()
        .filter_map(|(i, u)| u.as_ref().map(|_| i))
        .collect()
}

fn grid_with_link(cols: u16, text: &str) -> Grid {
    let mut grid = Grid::new(4, cols);
    grid.begin_hyperlink("https://a.bc/", None).unwrap();
    grid.put_str(text);
    grid.end_hyperlink();
    grid.check_invariants();
    grid
}

#[test]
fn only_cells_written_while_a_link_is_open_carry_it() {
    let mut grid = Grid::new(2, 10);
    grid.put_str("ab");
    grid.begin_hyperlink("https://a.bc/", Some("k")).unwrap();
    grid.put_str("cd");
    grid.end_hyperlink();
    grid.put_str("ef");
    assert_eq!(linked(&grid, 0), [2, 3]);
    let link = grid.line(0).unwrap().hyperlink(2).unwrap();
    assert_eq!((link.uri(), link.id()), ("https://a.bc/", Some("k")));
    assert!(link.same_link(grid.line(0).unwrap().hyperlink(3).unwrap()));
    assert!(grid.hyperlink().is_none());
    assert_eq!(grid.hyperlink_at(0, 2), Some(link));
    assert_eq!(grid.hyperlink_at(0, 4), None);
    grid.check_invariants();
}

#[test]
fn overwriting_and_erasing_drop_the_link_but_not_the_open_one() {
    let mut grid = grid_with_link(10, "abcdef");
    grid.goto(0, 0);
    grid.put_char('X');
    assert_eq!(linked(&grid, 0), [1, 2, 3, 4, 5]);
    grid.goto(0, 1);
    grid.erase_chars(2);
    assert_eq!(linked(&grid, 0), [3, 4, 5]);
    grid.goto(0, 4);
    grid.erase_in_line(EraseLine::ToEnd);
    assert_eq!(linked(&grid, 0), [3]);
    grid.erase_in_line(EraseLine::All);
    assert_eq!(linked(&grid, 0), []);
    assert_eq!(
        grid.live_hyperlinks(),
        0,
        "the last cell took the link with it"
    );
    grid.check_invariants();
}

#[test]
fn erased_blanks_stay_unlinked_while_a_link_is_open_and_new_text_is_linked() {
    let mut grid = Grid::new(2, 10);
    grid.begin_hyperlink("https://a.bc/", None).unwrap();
    grid.put_str("abcd");
    grid.goto(0, 0);
    grid.erase_chars(2);
    grid.erase_in_display(EraseDisplay::Below);
    assert_eq!(linked(&grid, 0), []);
    assert!(grid.hyperlink().is_some());
    grid.goto(0, 0);
    grid.put_char('c');
    assert_eq!(linked(&grid, 0), [0]);
    grid.check_invariants();
}

#[test]
fn insert_and_delete_move_links_with_their_cells() {
    let mut grid = grid_with_link(10, "abcd");
    grid.goto(0, 1);
    grid.insert_chars(2);
    assert_eq!(linked(&grid, 0), [0, 3, 4, 5]);
    grid.delete_chars(3);
    assert_eq!(linked(&grid, 0), [0, 1, 2]);
    // Past the right edge they fall off.
    let mut grid = grid_with_link(5, "abcde");
    grid.goto(0, 0);
    grid.insert_chars(2);
    assert_eq!(linked(&grid, 0), [2, 3, 4]);
    grid.check_invariants();
}

#[test]
fn a_wide_character_carries_the_link_on_both_halves_and_dies_whole() {
    let mut grid = grid_with_link(6, "\u{4e16}a");
    assert_eq!(linked(&grid, 0), [0, 1, 2]);
    grid.goto(0, 1);
    grid.put_char('x');
    assert_eq!(linked(&grid, 0), [2], "overwriting a half blanks the other");
    grid.check_invariants();
}

#[test]
fn a_cluster_that_grows_to_two_columns_links_its_new_spacer() {
    // U+263A WHITE SMILING FACE is one column; with VS16 the cluster is two.
    let mut grid = Grid::new(2, 6);
    grid.begin_hyperlink("https://a.bc/", None).unwrap();
    grid.put_str("\u{263a}\u{fe0f}");
    assert_eq!(linked(&grid, 0), [0, 1]);
    grid.check_invariants();
}

#[test]
fn rectangular_scrolls_move_links_with_their_text() {
    for n in [1, 2] {
        let mut grid = Grid::new(4, 8);
        grid.goto(2, 0);
        grid.begin_hyperlink("https://a.bc/", None).unwrap();
        grid.put_str("cd");
        grid.end_hyperlink();
        grid.set_margins(0, 5);
        grid.scroll_up(n);
        let row = 2 - n;
        assert_eq!(linked(&grid, row), [0, 1], "scrolled up by {n}");
        assert_eq!(linked(&grid, 2), []);
        assert_eq!(grid.live_hyperlinks(), 1);
        grid.scroll_down(n);
        assert_eq!(linked(&grid, 2), [0, 1], "and back down");
        grid.reset_margins();
        grid.check_invariants();
    }
}

#[test]
fn a_margin_scroll_leaves_links_outside_the_margins_on_their_rows() {
    // Columns 0 and 7 are outside the margins (1..=6) and stay put; columns
    // 3-4 are inside and move. One row takes the rotation path of
    // `scroll_rect` when no line has links, two the copy path.
    for n in [1, 2] {
        let mut grid = Grid::new(4, 8);
        grid.goto(2, 0);
        grid.begin_hyperlink("https://out.example/", None).unwrap();
        grid.put_char('a');
        grid.goto(2, 3);
        grid.begin_hyperlink("https://in.example/", None).unwrap();
        grid.put_str("cd");
        grid.goto(2, 7);
        grid.begin_hyperlink("https://out.example/", None).unwrap();
        grid.put_char('f');
        grid.end_hyperlink();
        grid.set_margins(1, 6);
        grid.scroll_up(n);
        assert_eq!(linked(&grid, 2), [0, 7], "outside stays, n = {n}");
        assert_eq!(linked(&grid, 2 - n), [3, 4], "inside moves, n = {n}");
        grid.check_invariants();
    }
}

#[test]
fn links_scroll_into_history_and_are_freed_when_it_evicts_them() {
    let mut grid = Grid::with_scrollback(2, 6, 3);
    grid.begin_hyperlink("https://a.bc/", None).unwrap();
    grid.put_str("ab");
    grid.end_hyperlink();
    for _ in 0..2 {
        grid.line_feed();
    }
    assert_eq!(grid.scrollback_len(), 1);
    assert_eq!(
        grid.hyperlink_at(0, 1).map(Hyperlink::uri),
        Some("https://a.bc/")
    );
    assert_eq!(grid.live_hyperlinks(), 1);
    for _ in 0..6 {
        grid.line_feed();
    }
    assert_eq!(grid.scrollback_len(), 3);
    assert_eq!(grid.live_hyperlinks(), 0, "evicted with its line");
    grid.check_invariants();
}

#[test]
fn the_open_link_ends_with_a_screen_switch_and_a_reset_but_cells_keep_theirs() {
    let mut grid = Grid::new(2, 6);
    grid.begin_hyperlink("https://a.bc/", None).unwrap();
    grid.put_char('a');
    grid.enter_alt_screen();
    assert!(grid.hyperlink().is_none());
    grid.begin_hyperlink("https://a.bc/", None).unwrap();
    grid.put_char('b');
    grid.leave_alt_screen();
    assert!(grid.hyperlink().is_none());
    assert_eq!(linked(&grid, 0), [0]);
    grid.begin_hyperlink("https://a.bc/", None).unwrap();
    grid.reset();
    assert!(grid.hyperlink().is_none());
    assert_eq!(linked(&grid, 0), [], "the reset blanked the screen");
    assert_eq!(grid.live_hyperlinks(), 0);
    grid.check_invariants();
}

#[test]
fn entering_the_alternate_screen_again_closes_the_open_link() {
    let mut grid = Grid::new(2, 6);
    grid.enter_alt_screen();
    grid.begin_hyperlink("https://a.bc/", None).unwrap();
    grid.put_char('a');
    // A repeated DECSET 47 or 1049: no switch, but the link closes (D-L5).
    grid.enter_alt_screen();
    assert!(grid.hyperlink().is_none());
    grid.put_char('b');
    assert_eq!(linked(&grid, 0), [0]);
    grid.check_invariants();
}

#[test]
fn refused_links_close_the_open_one() {
    let mut grid = Grid::new(2, 6);
    grid.begin_hyperlink("https://a.bc/", None).unwrap();
    assert_eq!(grid.begin_hyperlink("", None), Err(LinkRefused::EmptyUri));
    assert!(grid.hyperlink().is_none());
    grid.begin_hyperlink("https://a.bc/", None).unwrap();
    let long = format!("https://a.bc/{}", "a".repeat(2071));
    assert_eq!(
        grid.begin_hyperlink(&long, None),
        Err(LinkRefused::UriTooLong)
    );
    assert!(grid.hyperlink().is_none());
}

#[test]
fn a_flood_of_rewritten_links_holds_one_link_at_a_time() {
    let mut grid = Grid::new(2, 6);
    for i in 0..1_000_000 {
        // Alternate between links with and without an id.
        let id = (i % 2 == 0).then_some("k");
        grid.begin_hyperlink(&format!("https://a.bc/{i}"), id)
            .unwrap();
        grid.goto(0, 0);
        grid.put_char('x');
        // The open link and the one in the cell.
        assert!(
            grid.live_hyperlinks() <= 2,
            "{} live at {i}",
            grid.live_hyperlinks()
        );
    }
    grid.end_hyperlink();
    assert_eq!(grid.live_hyperlinks(), 1);
    grid.check_invariants();
}

#[test]
fn links_stop_at_the_live_cap_and_the_refusal_is_reported_once() {
    // MAX_LIVE_LINKS cells hold a link each (the history keeps them alive);
    // one more is refused, and so is every other until a cell lets go.
    let mut grid = Grid::with_scrollback(10, 200, 1000);
    for i in 0..MAX_LIVE_LINKS {
        grid.begin_hyperlink(&format!("https://a.bc/{i}"), None)
            .unwrap();
        grid.put_char('x');
    }
    grid.end_hyperlink();
    assert_eq!(grid.live_hyperlinks(), MAX_LIVE_LINKS);
    assert_eq!(
        grid.begin_hyperlink("https://a.bc/next", None),
        Err(LinkRefused::TableFull { first: true })
    );
    for _ in 0..3 {
        assert_eq!(
            grid.begin_hyperlink("https://a.bc/again", Some("id")),
            Err(LinkRefused::TableFull { first: false })
        );
    }
    // The refused link is not open, and the text after it has none.
    assert!(grid.hyperlink().is_none());
    grid.put_char('y');
    let cursor = grid.cursor();
    let line = grid.line(cursor.row).unwrap();
    assert!(line.hyperlink(usize::from(cursor.col) - 1).is_none());
    assert_eq!(grid.live_hyperlinks(), MAX_LIVE_LINKS);
    // A cell lets go and the table refills: the same run of refusals, not
    // reported again (the table never drained to three quarters).
    for round in 0..100 {
        grid.goto(0, 0);
        grid.erase_chars(1);
        assert_eq!(grid.live_hyperlinks(), MAX_LIVE_LINKS - 1);
        grid.begin_hyperlink(&format!("https://a.bc/refill/{round}"), None)
            .unwrap();
        grid.put_char('x');
        assert_eq!(
            grid.begin_hyperlink("https://a.bc/again", None),
            Err(LinkRefused::TableFull { first: false }),
            "round {round}"
        );
    }
    // Room again once cells let go: the next refusal is reported anew.
    grid.erase_in_display(EraseDisplay::All);
    grid.clear_scrollback();
    assert_eq!(grid.live_hyperlinks(), 0);
    grid.begin_hyperlink("https://a.bc/room", None).unwrap();
    grid.put_char('x');
    for i in 1..MAX_LIVE_LINKS {
        grid.begin_hyperlink(&format!("https://a.bc/{i}"), None)
            .unwrap();
        grid.put_char('x');
    }
    assert_eq!(
        grid.begin_hyperlink("https://a.bc/next", None),
        Err(LinkRefused::TableFull { first: true }),
        "reported anew"
    );
}

/// A URI of exactly `len` bytes, distinct per `i`.
fn long_uri(i: usize, len: usize) -> String {
    let head = format!("https://a.bc/{i}/");
    format!("{head}{}", "u".repeat(len - head.len()))
}

#[test]
fn links_with_long_uris_and_ids_stop_at_the_table_byte_cap() {
    // 65 536 maximal links, each in a cell of its own: the byte cap, not
    // the count, stops them; every one past it is refused and counted, and
    // the first refusal is the reported one.
    let mut grid = Grid::with_scrollback(10, 200, 1000);
    let size = MAX_URI_BYTES + MAX_ID_BYTES;
    let (mut opened, mut refused) = (0, 0);
    for i in 0..MAX_LIVE_LINKS {
        let id = format!("{i:0>MAX_ID_BYTES$}");
        match grid.begin_hyperlink(&long_uri(i, MAX_URI_BYTES), Some(&id)) {
            Ok(()) => opened += 1,
            Err(LinkRefused::TableFull { first }) => {
                assert_eq!(first, refused == 0, "link {i}");
                refused += 1;
            }
            Err(other) => panic!("link {i}: {other:?}"),
        }
        grid.put_char('x');
        assert!(grid.hyperlink_bytes() <= MAX_LINK_TABLE_BYTES);
    }
    grid.end_hyperlink();
    assert_eq!(opened, MAX_LINK_TABLE_BYTES / size);
    assert_eq!(refused, MAX_LIVE_LINKS - opened);
    assert_eq!(grid.live_hyperlinks(), opened);
    assert_eq!(grid.hyperlink_bytes(), opened * size);
    // Short links fit in what is left, until it is gone.
    let left = MAX_LINK_TABLE_BYTES - grid.hyperlink_bytes();
    grid.begin_hyperlink(&"u".repeat(left), None).unwrap();
    grid.put_char('x');
    assert_eq!(grid.hyperlink_bytes(), MAX_LINK_TABLE_BYTES);
    assert!(matches!(
        grid.begin_hyperlink("u", None),
        Err(LinkRefused::TableFull { first: false })
    ));
    grid.erase_in_display(EraseDisplay::All);
    grid.clear_scrollback();
    assert_eq!((grid.live_hyperlinks(), grid.hyperlink_bytes()), (0, 0));
    grid.check_invariants();
}

#[test]
fn churning_id_links_over_one_cell_gives_their_bytes_back() {
    // The table nearly full of history, and a fresh maximal `id` link
    // written over one screen cell, again and again: each dead link's
    // bytes come back, so none is refused and the table stays in its cap.
    let mut grid = Grid::with_scrollback(10, 200, 1000);
    let size = MAX_URI_BYTES + MAX_ID_BYTES;
    let id = |i: usize| format!("{i:0>MAX_ID_BYTES$}");
    let held = MAX_LINK_TABLE_BYTES / size - 2;
    for i in 0..held {
        grid.begin_hyperlink(&long_uri(i, MAX_URI_BYTES), Some(&id(i)))
            .unwrap();
        grid.put_char('x');
    }
    for i in held..held + 20_000 {
        grid.begin_hyperlink(&long_uri(i, MAX_URI_BYTES), Some(&id(i)))
            .expect("the dead links gave their bytes back");
        // Past the end of the filled cells on the last row.
        grid.goto(9, 199);
        grid.put_char('x');
        assert!(grid.hyperlink_bytes() <= MAX_LINK_TABLE_BYTES);
        assert_eq!(grid.live_hyperlinks(), held + 1);
    }
    grid.check_invariants();
}

// ------------------------------------------------------------------ reflow

/// `(character, URI)` of every cell with text in the logical buffer, in
/// order: what a resize must keep. The spacer half of a wide character is
/// listed as `'\0'` with its own link, so a spacer that loses its link
/// shows.
fn text_and_links(grid: &Grid) -> Vec<(char, Option<String>)> {
    let mut out = Vec::new();
    for line in grid.logical_lines() {
        for (col, cell) in line.cells.iter().enumerate() {
            if cell.is_pad() || (cell.ch == ' ' && !cell.is_spacer()) {
                continue;
            }
            let ch = if cell.is_spacer() { '\0' } else { cell.ch };
            out.push((ch, line.hyperlink(col).map(|l| l.uri().to_owned())));
        }
    }
    out
}

#[test]
fn the_spacer_half_keeps_its_link_through_a_reflow() {
    // `中` lands on the next row at 3 columns and back at 9.
    let mut grid = Grid::new(4, 9);
    grid.put_str("ab");
    grid.begin_hyperlink("https://a.bc/", None).unwrap();
    grid.put_str("\u{4e2d}");
    grid.end_hyperlink();
    for cols in [3, 2, 9] {
        grid.resize(4, cols);
        grid.check_invariants();
        let line = grid
            .logical_lines()
            .find(|line| line.cells.iter().any(|c| c.ch == '\u{4e2d}'))
            .expect("the wide character");
        let col = line.cells.iter().position(|c| c.ch == '\u{4e2d}').unwrap();
        let (base, spacer) = (line.hyperlink(col), line.hyperlink(col + 1));
        assert!(line.cells[col + 1].is_spacer() && line.cells[col + 1].is_link());
        assert!(
            base.zip(spacer).is_some_and(|(b, s)| b.same_link(s)),
            "{cols} columns: {base:?} / {spacer:?}"
        );
    }
}

/// The text of the logical buffer, history first, one entry per logical
/// line (rows joined across soft wraps): what Select All copies.
fn logical_text(grid: &Grid) -> Vec<String> {
    let mut out = vec![String::new()];
    for line in grid.logical_lines() {
        out.last_mut().expect("one").push_str(&line.text());
        if !line.wrapped {
            out.push(String::new());
        }
    }
    out
}

#[test]
fn a_trailing_linked_space_does_not_add_a_row_when_narrowing() {
    for linked_space in [false, true] {
        let mut grid = Grid::new(4, 10);
        grid.begin_hyperlink("https://a.bc/", None).unwrap();
        grid.put_str("ab");
        if !linked_space {
            grid.end_hyperlink();
        }
        grid.put_str("  ");
        grid.end_hyperlink();
        grid.carriage_return();
        grid.line_feed();
        grid.put_str("cd");
        grid.resize(4, 2);
        grid.check_invariants();
        // `ab` stays one row: its trailing spaces are dropped, linked or
        // not, so `cd` is the next row.
        let rows: Vec<String> = grid.logical_lines().map(tessera_grid::Line::text).collect();
        assert_eq!(rows[..2], ["ab", "cd"], "linked spaces: {linked_space}");
        assert_eq!(logical_text(&grid)[..2], ["ab", "cd"]);
    }
}

#[test]
fn narrowing_keeps_the_oldest_history_row_with_trailing_linked_spaces() {
    // 4 000 random buffers, each written twice: once with its trailing
    // spaces linked, once without. Narrowed to two columns both must keep
    // the same rows: a linked space that counted as text would add rows
    // and push the oldest history row out of the capped scrollback.
    let mut seed: u64 = 0x2224;
    let mut next = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let mut losses = 0;
    for case in 0..4000 {
        let lines: Vec<(String, usize)> = (0..2 + next(8))
            .map(|_| {
                let text: String = (0..=next(6))
                    .map(|_| char::from(b'a' + u8::try_from(next(26)).unwrap()))
                    .collect();
                (text, usize::try_from(next(4)).unwrap())
            })
            .collect();
        let write = |link_spaces: bool| {
            let mut grid = Grid::with_scrollback(3, 10, 6);
            for (i, (text, spaces)) in lines.iter().enumerate() {
                grid.begin_hyperlink(&format!("https://a.bc/{i}"), None)
                    .unwrap();
                grid.put_str(text);
                if !link_spaces {
                    grid.end_hyperlink();
                }
                grid.put_str(&" ".repeat(*spaces));
                grid.end_hyperlink();
                grid.carriage_return();
                grid.line_feed();
            }
            grid.resize(3, 2);
            grid.check_invariants();
            grid
        };
        let (linked, plain) = (write(true), write(false));
        if logical_text(&linked) != logical_text(&plain)
            || linked.scrollback_len() != plain.scrollback_len()
        {
            losses += 1;
            if losses == 1 {
                eprintln!(
                    "case {case}: {lines:?}\nlinked {:?}\nplain  {:?}",
                    logical_text(&linked),
                    logical_text(&plain)
                );
            }
        }
    }
    assert_eq!(losses, 0, "of 4000 cases");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Every linked cell keeps its URI through any chain of resizes, wide
    /// characters included, and the grid stays consistent.
    #[test]
    fn reflow_keeps_every_linked_cell_attached_to_its_text(
        runs in proptest::collection::vec(
            (0u8..3, "[a-z\u{4e16}\u{754c}]{1,12}", any::<bool>()), 1..8),
        sizes in proptest::collection::vec((1u16..8, 2u16..14), 1..5),
    ) {
        let mut grid = Grid::new(4, 9);
        for (which, text, newline) in &runs {
            match which {
                0 => grid.end_hyperlink(),
                n => {
                    grid.begin_hyperlink(&format!("https://a.bc/{n}"), None).unwrap();
                }
            }
            grid.put_str(text);
            if *newline {
                grid.carriage_return();
                grid.line_feed();
            }
        }
        grid.check_invariants();
        let before = text_and_links(&grid);
        for (rows, cols) in sizes {
            grid.resize(rows, cols);
            grid.check_invariants();
        }
        let after = text_and_links(&grid);
        prop_assert_eq!(after, before);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Random edits with links open and closed keep the cell flags and the
    /// link table consistent.
    #[test]
    fn edits_keep_flags_and_table_consistent(
        ops in proptest::collection::vec((0u8..16, 0u16..1000), 1..120),
    ) {
        let mut grid = Grid::with_scrollback(5, 12, 6);
        for (op, arg) in ops {
            match op {
                0 => {
                    let id = (arg % 2 == 0).then_some("i");
                    let _ = grid.begin_hyperlink(&format!("u{}", arg % 3), id);
                }
                1 => grid.end_hyperlink(),
                2..=4 => grid.put_str("ab\u{4e16}\u{301}e\u{301}"),
                5 => grid.goto(arg % 5, arg % 12),
                6 => grid.erase_chars(1 + arg % 5),
                7 => grid.insert_chars(1 + arg % 5),
                8 => grid.delete_chars(1 + arg % 5),
                9 => grid.erase_in_line(EraseLine::ToStart),
                10 => grid.line_feed(),
                11 => grid.insert_lines(1 + arg % 2),
                12 => grid.delete_lines(1 + arg % 2),
                13 => grid.set_margins(arg % 4, 4 + arg % 8),
                14 => grid.resize(2 + arg % 5, 3 + arg % 12),
                _ => grid.enter_alt_screen(),
            }
            grid.check_invariants();
        }
    }
}
