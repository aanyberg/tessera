//! An over-long OSC 8 URI is refused from its piece lengths, before the
//! pieces are joined (#224, ADR 0026): refusing it allocates no more than
//! an ignored OSC with the same payload.
//!
//! The counting allocator is process-wide, so it lives in this binary of
//! its own rather than in `hyperlink.rs`. Allocations are counted per
//! thread, so the test harness's own threads do not disturb the count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as StdCell;

use tessera_term::Terminal;

thread_local! {
    /// Bytes this thread has allocated (growth only for a `realloc`).
    static ALLOCATED: StdCell<usize> = const { StdCell::new(0) };
}

/// Counts each thread's allocated bytes, so a test sees only its own.
struct Counting;

fn count(bytes: usize) {
    // `try_with`: the thread-local may be gone while its thread exits.
    let _ = ALLOCATED.try_with(|c| c.set(c.get() + bytes));
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
        count(new_size.saturating_sub(layout.size()));
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Bytes `f` allocated on this thread.
fn allocated_by(f: impl FnOnce()) -> usize {
    let before = ALLOCATED.with(StdCell::get);
    f();
    ALLOCATED.with(StdCell::get) - before
}

fn open(uri: &str) -> Vec<u8> {
    format!("\x1b]8;;{uri}\x1b\\").into_bytes()
}

#[test]
fn an_over_long_uri_is_refused_from_its_piece_lengths_without_a_copy() {
    // 12 pieces of 8 KiB: 96 KiB of URI, refused before it is joined. An
    // ignored OSC with the same payload costs the parser the same; OSC 8
    // must not cost a URI's worth more.
    let piece = "u".repeat(8 * 1024);
    let payload = [piece.as_str(); 12].join(";");
    let osc = |command: &str| format!("\x1b]{command};;{payload}\x1b\\").into_bytes();
    let (link, ignored) = (osc("8"), osc("9999"));
    let mut term = Terminal::new(2, 10);
    term.advance(&open("https://a.bc/"));
    term.advance(&link);
    assert!(term.grid().hyperlink().is_none(), "refused, and closes");
    let cost = |bytes: &[u8], term: &mut Terminal| {
        (0..3)
            .map(|_| allocated_by(|| term.advance(bytes)))
            .min()
            .unwrap()
    };
    let (for_link, for_ignored) = (cost(&link, &mut term), cost(&ignored, &mut term));
    assert!(
        for_link < for_ignored + payload.len() / 2,
        "OSC 8 allocated {for_link} bytes, an ignored OSC {for_ignored}"
    );
}

#[test]
fn overwriting_a_links_last_cell_allocates_nothing() {
    // The last cell of an entity drops it, and an entity with an `id`
    // leaves the pool's index (#277: `get` and `remove`, never `entry`,
    // which can reserve).
    for id in ["", "id=k"] {
        let mut term = Terminal::new(2, 10);
        // Warm up: the line's side table and the index exist already.
        term.advance(format!("\x1b]8;{id};https://a.bc/w\x1b\\w\x1b]8;;\x1b\\").as_bytes());
        term.advance(format!("\x1b]8;{id};https://a.bc/x\x1b\\x\x1b]8;;\x1b\\").as_bytes());
        assert_eq!(term.grid().live_hyperlinks(), 2, "{id:?}");
        // Only the print is counted: the cursor move before it can allocate
        // in the parser.
        term.advance(b"\x1b[1;2H");
        let cost = allocated_by(|| term.advance(b"y"));
        assert_eq!(term.grid().live_hyperlinks(), 1, "{id:?}: the x link died");
        assert_eq!(cost, 0, "{id:?}: the drop allocated {cost} bytes");
    }
}
