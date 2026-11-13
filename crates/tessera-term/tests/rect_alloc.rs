//! Allocation bounds of the rectangle operations (#251, ADR 0046,
//! docs/security.md): DECERA, DECFRA and DECSERA allocate nothing beyond
//! what the same CSIs cost when the operations are off (DECSCL 63), and
//! DECCRA of plain text allocates one scratch row per operation, never
//! more than the screen's width of cells. (A copy that carries clusters or
//! links also allocates their scratch lists, each at most a row's entries,
//! and one string per copied cluster: `tests/rect.rs` bounds its cost.)
//!
//! The counting allocator is process-wide, so it lives in this binary of
//! its own. It counts per thread, so the harness's own threads do not
//! disturb a test's count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as StdCell;

use tessera_term::Terminal;

thread_local! {
    /// Allocations and reallocations this thread made.
    static ALLOCATIONS: StdCell<usize> = const { StdCell::new(0) };
    /// The largest of them, in bytes.
    static LARGEST: StdCell<usize> = const { StdCell::new(0) };
}

/// Counts each thread's allocations, so a test sees only its own.
struct Counting;

fn count(size: usize) {
    // `try_with`: the thread-locals may be gone while their thread exits.
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
    let _ = LARGEST.try_with(|c| c.set(c.get().max(size)));
}

// SAFETY: every call goes to `System` with the caller's arguments.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System` with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size);
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations `f` made on this thread, and the largest one in bytes.
fn allocations_by(f: impl FnOnce()) -> (usize, usize) {
    let before = ALLOCATIONS.with(StdCell::get);
    LARGEST.with(|c| c.set(0));
    f();
    (
        ALLOCATIONS.with(StdCell::get) - before,
        LARGEST.with(StdCell::get),
    )
}

const ROWS: u16 = 50;
const COLS: u16 = 200;

/// A full screen at conformance `level`, so the flood's own writes cost
/// the same in both runs.
fn setup(level: u8) -> Terminal {
    let mut term = Terminal::new(ROWS, COLS);
    term.advance(format!("\x1b[{level};1\"p").as_bytes());
    term.advance(&b"abcdefghij".repeat(usize::from(ROWS) * usize::from(COLS) / 10));
    term.advance(b"\x1b[H");
    term
}

/// Allocations of `flood` at level 64 (the operations run) and at 63
/// (they are ignored).
fn on_and_off(flood: &[u8]) -> ((usize, usize), (usize, usize)) {
    let mut on = setup(64);
    let ran = allocations_by(|| on.advance(flood));
    let mut off = setup(63);
    let ignored = allocations_by(|| off.advance(flood));
    assert!(
        format!("{:?}", on.grid()) != format!("{:?}", off.grid()),
        "the flood changed nothing at level 64"
    );
    (ran, ignored)
}

/// DECERA, DECFRA and DECSERA, each with saturated and huge parameters,
/// allocate nothing of their own.
#[test]
fn erases_and_fills_allocate_nothing() {
    for op in [
        &b"\x1b[1;1;65535;65535$z\x1b[88;2;2;99999;99999$x"[..],
        b"\x1b[120;1;1;50;200$x",
        b"\x1b[1;1;65535;65535${\x1b[2;2;9;9$z",
    ] {
        let flood = op.repeat(2_000);
        let ((ran, _), (ignored, _)) = on_and_off(&flood);
        assert_eq!(ran, ignored, "{:?}", String::from_utf8_lossy(op));
    }
}

/// DECCRA of plain text allocates one scratch row per copy, whatever its
/// parameters say, and the row is never larger than the screen is wide.
#[test]
fn deccra_allocates_one_row_of_at_most_the_screen_width() {
    let ops = 2_000;
    for op in [
        &b"\x1b[1;1;65535;65535;1;2;2;1$v"[..],
        b"\x1b[65535;65535;65535;65535;65535;1;1;65535$v",
        b"\x1b[3;4;20;150;1;1;1;1$v",
    ] {
        let flood = op.repeat(ops);
        let ((ran, largest), (ignored, parser_largest)) = on_and_off(&flood);
        assert_eq!(ran, ignored + ops, "{:?}", String::from_utf8_lossy(op));
        // The parser's own allocations are far smaller than a row.
        assert!(parser_largest < 256, "{parser_largest}");
        assert!(largest <= usize::from(COLS) * 20, "{largest} bytes");
    }
}
