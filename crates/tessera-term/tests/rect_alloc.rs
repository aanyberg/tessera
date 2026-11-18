//! Allocation bounds of the rectangle operations (#251, ADR 0046,
//! docs/security.md): DECERA, DECFRA and DECSERA allocate nothing beyond
//! what the same CSIs cost when the operations are off (DECSCL 63), and
//! DECCRA of plain text allocates one scratch row per operation, never
//! more than the screen's width of cells. (A copy that carries clusters or
//! links also allocates their scratch lists, each at most a row's entries,
//! and one string per copied cluster: `tests/rect.rs` bounds its cost.)
//! And the command end's carry of the alternate screen (#489, ADR 0020)
//! moves the screen's rows instead of copying them: it allocates no more
//! than a command end on the primary screen, plus the list of rows.
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

/// #430 20.1 M2: an exact DECCRA self copy (destination at the source's
/// top-left corner) allocates nothing of its own, on a screen of clusters
/// and links where any other copy allocates per row and per cluster.
#[test]
fn an_exact_self_copy_allocates_nothing() {
    let screen = |level: u8| {
        let mut term = Terminal::new(ROWS, COLS);
        term.advance(format!("\x1b[{level};1\"p").as_bytes());
        term.advance(b"\x1b]8;;https://a.example/\x1b\\");
        term.advance(
            "e\u{301}"
                .repeat(usize::from(ROWS) * usize::from(COLS))
                .as_bytes(),
        );
        term.advance(b"\x1b]8;;\x1b\\\x1b[H");
        term
    };
    for op in [
        &b"\x1b[$v"[..],
        b"\x1b[1;1;50;200;1;1;1;1$v",
        b"\x1b[3;4;20;150;1;3;4;1$v",
        b"\x1b[7;9;65535;65535;9;7;9;9$v",
    ] {
        let flood = op.repeat(200);
        let mut on = screen(64);
        let before = format!("{:?}", on.grid().visible_lines());
        let ran = allocations_by(|| on.advance(&flood)).0;
        assert_eq!(
            format!("{:?}", on.grid().visible_lines()),
            before,
            "{:?} changed a cell",
            String::from_utf8_lossy(op)
        );
        let mut off = screen(63);
        let ignored = allocations_by(|| off.advance(&flood)).0;
        assert_eq!(ran, ignored, "{:?}", String::from_utf8_lossy(op));
    }
}

/// A 200 x 500 alternate screen of text on all rows but the last two (so
/// the carry, from the primary screen's second row, scrolls nothing),
/// `leave` (DECRST 1049) or not.
fn full_alternate_screen(leave: bool) -> Terminal {
    let mut term = Terminal::new(200, 500);
    term.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07tui\r\n\x1b]133;C\x07\x1b[?1049h");
    for row in 1..=198 {
        term.advance(format!("\x1b[{row};1H").as_bytes());
        term.advance(&b"0123456789".repeat(50));
    }
    if leave {
        term.advance(b"\x1b[?1049l");
    }
    term
}

/// #489 (#461 r2-M3): the command end's carry takes the alternate
/// screen's rows instead of cloning them: on a 200 x 500 screen of text
/// it allocates the list of the rows, and the mark list of the line
/// below them that takes the command end mark, and nothing more than the
/// same command end after the program left the screen itself (its mark
/// goes on the output start's line, which has a list): never one per row
/// (a copy of the rows allocated 198 rows of cells).
#[test]
fn the_command_end_carry_moves_the_alternate_rows() {
    let end = b"\x1b]133;D;0\x07";
    let mut carried = full_alternate_screen(false);
    let (allocations, _) = allocations_by(|| carried.advance(end));
    let mut left = full_alternate_screen(true);
    let (baseline, _) = allocations_by(|| left.advance(end));
    assert_eq!(
        carried.grid().visible_lines()[198].text().trim_end(),
        "0123456789".repeat(50),
        "the carry ran"
    );
    assert!(
        allocations <= baseline + 2,
        "{allocations} allocations, {baseline} without the carry"
    );
}
