//! `Grid::reset` (RIS) works in place (#107): once warmed up, a reset of an
//! unchanged size allocates nothing, so a flood of `ESC c` holds the
//! terminal lock for microseconds rather than rebuilding the grid each
//! time.
//!
//! Allocations are counted per thread, so the test harness's own threads
//! do not disturb the count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as StdCell;

use tessera_grid::{Grid, Mark, Style};

/// The system allocator, counting the current thread's allocations.
struct Counting;

thread_local! {
    static ALLOCATIONS: StdCell<u64> = const { StdCell::new(0) };
}

// SAFETY: every call is forwarded to `System` unchanged; the counter is a
// const-initialised thread local that never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System` with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(StdCell::get)
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
