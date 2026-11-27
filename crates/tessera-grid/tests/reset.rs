//! `Grid::reset` (RIS) works in place (#107): once warmed up, a reset of an
//! unchanged size allocates nothing, so a flood of `ESC c` holds the
//! terminal lock for microseconds rather than rebuilding the grid each
//! time.
//!
//! The same counting allocator also bars the heap of a line's OSC 8 link
//! table (#619): printing the corpus under one link or a link per
//! character costs the runs' room and no more, and ICH/DCH and scrolls
//! between margins over linked rows do not grow it.
//!
//! It also bars the heap a scrollback line costs once history is packed
//! (#51, ADR 0051): full 80-column lines of plain ASCII and of a coloured,
//! wide-character mix.
//!
//! Allocations and live bytes are counted per thread, so the test
//! harness's own threads do not disturb the count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as StdCell;

use tessera_grid::{Grid, Hyperlink, Mark, Style};

/// The system allocator, counting the current thread's allocations.
struct Counting;

thread_local! {
    static ALLOCATIONS: StdCell<u64> = const { StdCell::new(0) };
    static LIVE_BYTES: StdCell<isize> = const { StdCell::new(0) };
}

/// Adds `delta` to the current thread's live heap bytes.
fn count_bytes(delta: isize) {
    LIVE_BYTES.with(|c| c.set(c.get() + delta));
}

/// `size` as a signed byte count (an allocation never exceeds
/// `isize::MAX` bytes).
fn signed(size: usize) -> isize {
    isize::try_from(size).unwrap_or(isize::MAX)
}

// SAFETY: every call is forwarded to `System` unchanged; the counter is a
// const-initialised thread local that never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        count_bytes(signed(layout.size()));
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count_bytes(-signed(layout.size()));
        // SAFETY: `ptr` came from `System` with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        count_bytes(signed(new_size) - signed(layout.size()));
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(StdCell::get)
}

/// The current thread's live heap bytes, net of frees.
fn live_bytes() -> isize {
    LIVE_BYTES.with(StdCell::get)
}

/// The net heap bytes `f` leaves allocated.
fn heap_of(f: impl FnOnce()) -> isize {
    let before = live_bytes();
    f();
    live_bytes() - before
}

/// Fills every row with text in a coloured pen and leaves the state RIS
/// resets (region, saved cursor, tab stops) changed.
fn scribble(grid: &mut Grid) {
    let mut pen = Style::DEFAULT;
    pen.bg = tessera_grid::Color::Indexed(4);
    grid.set_pen(pen);
    for row in 0..grid.rows() {
        grid.goto(row, 0);
        grid.put_str("text that is not blank ");
    }
    grid.set_scroll_region(2, 9);
    grid.save_cursor();
    grid.clear_all_tab_stops();
}

#[test]
fn a_thousand_resets_of_a_96x430_grid_allocate_nothing() {
    let mut grid = Grid::with_scrollback(96, 430, 10_000);
    // Warm-up: history, then one reset of a written screen.
    grid.goto(95, 0);
    for _ in 0..200 {
        grid.line_feed();
    }
    scribble(&mut grid);
    grid.reset();

    let before = allocations();
    for i in 0..1000 {
        if i % 100 == 0 {
            // A reset after a full screen of output takes the slow path,
            // which reuses every row's allocation too.
            scribble(&mut grid);
        }
        grid.reset();
    }
    let after = allocations();
    assert_eq!(after - before, 0, "allocations across 1000 resets");
    assert_eq!(
        grid.scrollback_len(),
        200,
        "RIS keeps the grid's scrollback"
    );
    assert!(grid.visible_lines().iter().all(|l| l.text().is_empty()));
}

#[test]
fn a_reset_of_the_alternate_screen_or_with_marks_allocates_nothing_new() {
    let mut grid = Grid::with_scrollback(24, 80, 100);
    grid.enter_alt_screen();
    grid.put_str("full-screen app");
    let before = allocations();
    // Leaving the alternate screen by reset frees the stashed primary
    // screen; it allocates nothing.
    grid.reset();
    assert_eq!(allocations() - before, 0);
    assert!(!grid.is_alt_screen());

    if let Some(line) = grid.line_mut(3) {
        line.marks.push(Mark::PromptStart);
    }
    let before = allocations();
    grid.reset();
    assert_eq!(allocations() - before, 0, "clearing marks keeps the vector");
    assert!(grid.visible_lines().iter().all(|l| l.marks.is_empty()));
}

/// The corpus lines of the grid write bars (#619): (name, text).
const CORPUS: [(&str, &str); 11] = [
    (
        "ascii",
        "The quick brown fox jumps over the lazy dog 0123456789 ABCDEFGHIJKLMNOPQRS",
    ),
    (
        "latin",
        "\u{c7}a \u{e9}t\u{e9} tr\u{e8}s fa\u{e7}ade na\u{ef}ve co\u{f6}peration \u{c5}ngstr\u{f6}m \u{153}uvre stra\u{df}e",
    ),
    (
        "cjk",
        "\u{6f22}\u{5b57}\u{4eee}\u{540d}\u{4ea4}\u{3058}\u{308a}\u{6587}\u{d55c}\u{ad6d}\u{c5b4}\u{4e2d}\u{6587}\u{6587}\u{672c}",
    ),
    (
        "box",
        "\u{250c}\u{2500}\u{2500}\u{252c}\u{2500}\u{2510}\u{2502}  \u{2502}\u{251c}\u{2500}\u{253c}\u{2524}\u{2514}\u{2534}\u{2518}\u{2550}\u{2551}\u{2554}\u{2557}",
    ),
    (
        "hebrew",
        "\u{5e9}\u{5dc}\u{5d5}\u{5dd} \u{5e2}\u{5d5}\u{5dc}\u{5dd} \u{5d6}\u{5d4}\u{5d5} \u{5d8}\u{5e7}\u{5e1}\u{5d8}",
    ),
    (
        "arabic",
        "\u{645}\u{631}\u{62d}\u{628}\u{627} \u{628}\u{627}\u{644}\u{639}\u{627}\u{644}\u{645} \u{647}\u{630}\u{627} \u{646}\u{635}",
    ),
    (
        "punctuation",
        "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~!\"#$%&'()*+,-./:;<=>?@",
    ),
    (
        "braille",
        "\u{2801}\u{2803}\u{2809}\u{2819}\u{2811}\u{280b}\u{281b}\u{2813}\u{280a}\u{281a}\u{2805}\u{2807}",
    ),
    (
        "emoji",
        "\u{1f600}\u{1f603}\u{1f604}\u{1f44d}\u{1f3fd}\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{1f1f8}\u{1f1ea}\u{1f680}",
    ),
    (
        "sgr",
        "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ",
    ),
    (
        "combining",
        "e\u{301}a\u{308}o\u{302}u\u{300}i\u{303}n\u{303}c\u{327}a\u{30a}e\u{301}\u{302}o\u{308}\u{304}",
    ),
];

/// How a corpus line is linked.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Linked {
    Plain,
    One,
    PerChar,
}

/// Prints `text` on every row of `grid`, linked as `how`; the SGR line
/// changes its pen every 3 cells.
fn print(grid: &mut Grid, name: &str, text: &str, how: Linked) {
    for row in 0..grid.rows() {
        grid.goto(row, 0);
        if how == Linked::One {
            let id = link_id(row, 0, how);
            grid.begin_hyperlink("https://example.com/corpus", Some(id.as_str()))
                .unwrap();
        }
        for (i, ch) in text.chars().enumerate() {
            if how == Linked::PerChar {
                let id = link_id(row, i, how);
                grid.begin_hyperlink("https://example.com/corpus", Some(id.as_str()))
                    .unwrap();
            }
            if name == "sgr" && i % 3 == 0 {
                let mut pen = Style::DEFAULT;
                pen.fg = tessera_grid::Color::Indexed(u8::try_from(i % 7).unwrap() + 1);
                grid.set_pen(pen);
            }
            let mut buf = [0; 4];
            grid.put_str(ch.encode_utf8(&mut buf));
        }
        grid.end_hyperlink();
        grid.set_pen(Style::DEFAULT);
    }
}

/// The link id of character `i` of `row` printed as `how`.
fn link_id(row: u16, i: usize, how: Linked) -> String {
    match how {
        Linked::PerChar => format!("{row}-{i}"),
        _ => format!("{row}"),
    }
}

/// A 4 x 80 grid with `text` printed on each row as `how`, and the heap
/// the printing left allocated. The links are opened first and kept
/// alive, so the pool's entries are not counted: the heap is the cells'
/// clusters and the link tables.
fn printed_heap(name: &str, text: &str, how: Linked) -> (Grid, Vec<Option<Hyperlink>>, isize) {
    let mut grid = Grid::new(4, 80);
    let mut opened = Vec::new();
    if how != Linked::Plain {
        for row in 0..grid.rows() {
            for i in 0..text.chars().count() {
                let id = link_id(row, i, how);
                grid.begin_hyperlink("https://example.com/corpus", Some(id.as_str()))
                    .unwrap();
                opened.push(grid.hyperlink().cloned());
            }
        }
        grid.end_hyperlink();
    }
    let heap = heap_of(|| print(&mut grid, name, text, how));
    (grid, opened, heap)
}

#[test]
fn a_corpus_line_s_link_table_costs_its_runs_room_and_edits_do_not_grow_it() {
    // Memory bar (#619): per corpus line, the link table of a row under
    // one link is one table header and the minimum room (4 runs of 16
    // bytes); under a link per character it is at most twice its runs'
    // 16 bytes. ICH/DCH 8 cells in and SU/SD between margins over the
    // linked rows allocate no more than over the plain rows.
    const SIDE: isize = 48;
    const RUN: isize = 16;
    for (name, text) in CORPUS {
        let (mut plain, _, plain_heap) = printed_heap(name, text, Linked::Plain);
        for how in [Linked::One, Linked::PerChar] {
            let (mut grid, _links, heap) = printed_heap(name, text, how);
            let runs = if how == Linked::One {
                1
            } else {
                isize::try_from(text.chars().count()).unwrap()
            };
            let rows = isize::try_from(grid.rows()).unwrap();
            let table = (heap - plain_heap) / rows;
            let bound = SIDE + RUN * (2 * runs).max(4);
            assert!(
                table <= bound,
                "{name} {how:?}: {table} bytes of link table per row, bound {bound}"
            );
            assert!(table >= RUN, "{name} {how:?}: {table} bytes per row");
            for (edit, f) in [
                ("ICH/DCH", edits as fn(&mut Grid)),
                ("SU/SD between margins", margin_scrolls),
            ] {
                let linked = heap_of(|| f(&mut grid));
                let unlinked = heap_of(|| f(&mut plain));
                assert!(
                    linked <= unlinked,
                    "{name} {how:?} {edit}: {linked} bytes over linked rows, {unlinked} plain"
                );
            }
            grid.check_invariants();
        }
    }
}

/// ICH and DCH 8 cells into every row.
fn edits(grid: &mut Grid) {
    for row in 0..grid.rows() {
        grid.goto(row, 8);
        grid.delete_chars(8);
        grid.insert_chars(8);
        grid.insert_chars(3);
        grid.delete_chars(2);
    }
}

/// SU and SD between margins 8..72, every row in the region.
fn margin_scrolls(grid: &mut Grid) {
    grid.set_margins(8, 71);
    for _ in 0..10 {
        grid.scroll_up(1);
        grid.scroll_down(2);
        grid.scroll_up(1);
    }
    grid.reset_margins();
}

/// Prints `links` links of `width` cells each from column 0 of `row`, ids
/// `{tag}-{row}-{k}`.
fn print_links(grid: &mut Grid, row: u16, tag: &str, links: usize, width: usize) {
    grid.goto(row, 0);
    for k in 0..links {
        let id = format!("{tag}-{row}-{k}");
        grid.begin_hyperlink("https://example.com/runs", Some(id.as_str()))
            .unwrap();
        grid.put_str(&"y".repeat(width));
    }
    grid.end_hyperlink();
}

#[test]
fn a_scroll_between_margins_over_full_link_tables_allocates_nothing() {
    // #619 M1: a row whose table is full (eight 10-cell links, room for
    // eight runs) that takes in one link between the margins ends with
    // three runs, so it needs no more room; cutting its edge runs before
    // the copy went to ten runs and doubled its table. Odd rows get the
    // even rows' eight pieces, nine runs, so they are given room for 16
    // runs first.
    let mut grid = Grid::new(24, 80);
    let mut kept = Vec::new();
    for row in 0..grid.rows() {
        if row % 2 == 0 {
            print_links(&mut grid, row, "multi", 8, 10);
        } else {
            print_links(&mut grid, row, "room", 16, 5);
            print_links(&mut grid, row, "one", 1, 80);
        }
        kept.push(grid.hyperlink().cloned());
    }
    grid.set_margins(5, 74);
    let before = allocations();
    let heap = heap_of(|| grid.scroll_up(1));
    assert_eq!(allocations() - before, 0, "the scroll allocated");
    assert!(heap <= 0, "the scroll left {heap} more bytes allocated");
    grid.check_invariants();
}

#[test]
fn margin_scrolls_over_plain_rows_allocate_nothing() {
    // #619 M3: copying a span without links onto a row without a link
    // table makes none.
    for (name, text) in CORPUS {
        let (mut grid, _, _) = printed_heap(name, text, Linked::Plain);
        let before = allocations();
        let heap = heap_of(|| margin_scrolls(&mut grid));
        // Copying a cluster of several scalars allocates its text, as
        // before #619; the other lines have no cluster table at all.
        if !matches!(name, "emoji" | "combining") {
            assert_eq!(allocations() - before, 0, "{name}: the scrolls allocated");
        }
        assert!(
            heap <= 0,
            "{name}: the scrolls left {heap} more bytes allocated"
        );
    }
}

#[test]
fn a_span_copy_grows_a_link_table_as_writing_its_runs_one_by_one_would() {
    // #619 F1: a copy that puts 30 runs into a table with room for 4 grew
    // it to exactly 31, so the next link doubled it to 62 where writing
    // the runs one by one stops at 32 and has room for that link.
    let mut grid = Grid::new(2, 80);
    grid.goto(0, 1);
    grid.begin_hyperlink("https://o/", None).unwrap();
    grid.put_str("o");
    grid.end_hyperlink();
    grid.set_margins(9, 79);
    grid.goto(1, 10);
    for i in 0..30 {
        let uri = format!("https://p/{i}");
        grid.begin_hyperlink(&uri, None).unwrap();
        grid.put_str("x");
    }
    grid.end_hyperlink();
    // Row 0 takes in row 1's 30 runs: room for 32, 448 bytes more.
    let scrolled = heap_of(|| grid.scroll_up(1));
    assert!(
        scrolled <= 16 * (32 - 4),
        "the scroll left {scrolled} more bytes"
    );
    // A 32nd run fits that room.
    grid.reset_margins();
    grid.goto(0, 3);
    grid.begin_hyperlink("https://q/", None).unwrap();
    let before = allocations();
    let written = heap_of(|| grid.put_str("q"));
    assert_eq!(allocations() - before, 0, "the 32nd run grew the table");
    assert!(written <= 0, "the 32nd run left {written} more bytes");
    grid.end_hyperlink();
    grid.check_invariants();
}

/// Lines of history the heap-per-line bars measure.
const HISTORY_LINES: usize = 10_000;

/// A small deterministic generator (xorshift), so the corpus is fixed.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Writes one full 80-column line and its CR LF: printable ASCII with
/// some spaces (`plain`), or coloured words, wide characters, a combining
/// sequence and an emoji, padded with spaces, as the `heap_lines_*`
/// cases of `scripts/perf-cases.rs` do.
fn history_line(grid: &mut Grid, rng: &mut Rng, plain: bool) {
    if plain {
        let line: String = (0..80u64)
            .map(|col| {
                if col < 79 && rng.below(6) == 0 {
                    ' '
                } else {
                    char::from(b'!' + u8::try_from(rng.below(94)).unwrap())
                }
            })
            .collect();
        grid.put_str(&line);
    } else {
        let words: [(&str, usize); 8] = [
            ("alpha", 5),
            ("needle42", 8),
            ("\u{4e2d}\u{6587}", 4),
            ("\u{65e5}\u{672c}\u{8a9e}", 6),
            ("e\u{301}te\u{301}", 4),
            ("\u{1f600}", 2),
            ("tessera", 7),
            ("0123456789", 10),
        ];
        let mut col = 0;
        loop {
            let (word, width) = words[usize::try_from(rng.below(8)).unwrap()];
            if col + width + 1 > 80 {
                break;
            }
            match rng.below(4) {
                0 => grid.set_pen(Style {
                    fg: tessera_grid::Color::Indexed(u8::try_from(1 + rng.below(7)).unwrap()),
                    ..Style::DEFAULT
                }),
                1 => grid.set_pen(
                    Style {
                        fg: tessera_grid::Color::Indexed(u8::try_from(rng.below(256)).unwrap()),
                        ..Style::DEFAULT
                    }
                    .with(Style::BOLD),
                ),
                2 => grid.set_pen(Style::DEFAULT),
                _ => {}
            }
            grid.put_str(word);
            grid.put_str(" ");
            col += width + 1;
        }
        grid.set_pen(Style::DEFAULT);
        while col < 80 {
            grid.put_str(" ");
            col += 1;
        }
    }
    grid.carriage_return();
    grid.line_feed();
}

/// Heap bytes per history line: a grid keeping [`HISTORY_LINES`] lines
/// against one keeping none, fed the same lines.
fn heap_per_line(plain: bool) -> isize {
    let fill = |cap: usize| {
        let before = live_bytes();
        let mut rng = Rng(0x5103);
        let mut grid = Grid::with_scrollback(24, 80, cap);
        for _ in 0..HISTORY_LINES + 24 {
            history_line(&mut grid, &mut rng, plain);
        }
        assert_eq!(grid.scrollback_len(), cap, "the corpus fills the history");
        // Measured while the grid is alive, then released.
        let live = live_bytes() - before;
        drop(grid);
        live
    };
    let none = fill(0);
    (fill(HISTORY_LINES) - none) / isize::try_from(HISTORY_LINES).unwrap()
}

/// A plain 80-column history line costs at most 256 heap bytes once
/// packed (#51; 1 664 bytes as `Line`s).
#[test]
fn a_plain_history_line_costs_at_most_256_bytes() {
    let per_line = heap_per_line(true);
    assert!(per_line <= 256, "{per_line} bytes per plain line");
}

/// A coloured, wide-character history line costs at most 400 heap bytes
/// once packed (#51; 1 781 bytes as `Line`s).
#[test]
fn a_mixed_history_line_costs_at_most_400_bytes() {
    let per_line = heap_per_line(false);
    assert!(per_line <= 400, "{per_line} bytes per mixed line");
}
