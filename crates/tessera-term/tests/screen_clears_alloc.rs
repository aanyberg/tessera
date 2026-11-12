//! B13 of the screen-clear count (#377, ADR 0039): a flood of clears
//! counts each and allocates nothing beyond what the same CSIs cost when
//! they do not count. The rest of rows B01 to B15 is `screen_clears.rs`.
//!
//! The counting allocator is process-wide, so it lives in this binary of
//! its own. It counts per thread, so the harness's own threads do not
//! disturb the flood's count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as StdCell;

use tessera_grid::Point;

mod screen_clears_common;

use screen_clears_common::{PUSHED, expect_clears, setup};

thread_local! {
    /// Allocations and reallocations this thread made.
    static ALLOCATIONS: StdCell<usize> = const { StdCell::new(0) };
}

/// Counts each thread's allocations, so a test sees only its own.
struct Counting;

fn count() {
    // `try_with`: the thread-local may be gone while its thread exits.
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: every call goes to `System` with the caller's arguments.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System` with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations `f` made on this thread.
fn allocations_by<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let before = ALLOCATIONS.with(StdCell::get);
    let result = f();
    (result, ALLOCATIONS.with(StdCell::get) - before)
}

/// S6: a flood of clears costs a counter bump each, allocates nothing
/// beyond what the same CSIs cost when they do not count (the parser
/// hands each CSI's parameters over in an allocation of their own, as
/// before #377) and reports nothing.
#[test]
fn b13_a_flood_of_clears_counts_each_and_allocates_nothing() {
    let clear = b"\x1b[H\x1b[2J";
    let reps = (1 << 20) / clear.len();
    let mut flood = clear.repeat(reps);
    flood.push(b'x');
    let mut term = setup();
    let ((), allocations) = allocations_by(|| term.advance(&flood));
    let mut uncounted = b"\x1b[H\x1b[1J".repeat(reps);
    uncounted.push(b'x');
    let mut other = setup();
    let ((), baseline) = allocations_by(|| other.advance(&uncounted));
    assert_eq!(other.screen_clears(), 1);
    assert_eq!(allocations, baseline, "counting the clears allocated");
    assert!(
        baseline <= 2 * reps,
        "{baseline} allocations for {reps} pairs"
    );
    assert_eq!(reps, 149_796);
    expect_clears(&mut term, 1, reps as u64, Some(PUSHED));
    assert!(term.take_requests().is_empty(), "nothing reported");
    let grid = term.grid();
    assert_eq!(grid.line(0).unwrap().cells[0].ch, 'x');
    assert_eq!(grid.cursor(), Point::new(0, 1));
}
