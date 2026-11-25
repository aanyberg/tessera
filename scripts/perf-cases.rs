//! The fixed case set of the scheduled performance job (`scripts/perf-job.sh`,
//! #612), copied over the probe's `src/cases.rs` after `cargo xtask ab init`.
//! The crates under test are `base_term`, `head_term`, `base_grid`,
//! `head_grid`, `base_search` and `head_search`.
//!
//! Instruction cases (`feed`, `scroll`, `search`) are timed by `cargo xtask ab
//! bench`. Heap cases (`heap_idle`, `heap_scrollback`) return the live heap in
//! bytes as their checksum, counted by the global allocator below; the job
//! runs those probe processes directly and reads the checksum. Changing a
//! corpus changes every later number: record it in docs/adr/0049 and expect
//! one step in the trend.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{Rng, Side};

/// Live heap bytes of this process (every allocation, every thread).
static LIVE: AtomicUsize = AtomicUsize::new(0);

struct Counting;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

// SAFETY: every call forwards to `System` with the caller's arguments; the
// counter is a side effect that never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let block = unsafe { System.alloc(layout) };
        if !block.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let block = unsafe { System.alloc_zeroed(layout) };
        if !block.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(block, layout) }
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let moved = unsafe { System.realloc(block, layout, new_size) };
        if !moved.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            LIVE.fetch_add(new_size, Ordering::Relaxed);
        }
        moved
    }
}

const ROWS: u16 = 24;
const COLS: u16 = 80;
/// The default scrollback cap (lines).
const SCROLLBACK: usize = 10_000;

/// Output with SGR runs, wide and combining characters and line breaks, from
/// a fixed seed: the same bytes on both sides and on every run.
fn corpus(lines: usize) -> Vec<u8> {
    let mut rng = Rng::new(0x612);
    let words = ["alpha", "needle42", "scroll", "\u{4e2d}\u{6587}", "e\u{301}", "tessera", "0123456789"];
    let mut out = Vec::new();
    for _ in 0..lines {
        let count = 4 + rng.below(12);
        for _ in 0..count {
            if rng.below(5) == 0 {
                out.extend_from_slice(format!("\x1b[{}m", 31 + rng.below(7)).as_bytes());
            }
            out.extend_from_slice(rng.pick(&words).as_bytes());
            out.push(b' ');
        }
        out.extend_from_slice(b"\x1b[0m\r\n");
    }
    out
}

macro_rules! cases {
    ($term:ident, $search:ident) => {
        /// Feeds the corpus in 4 KiB chunks into a terminal that keeps no
        /// scrollback: parsing and printing alone.
        pub fn feed(lines: usize) -> u64 {
            let data = corpus(lines);
            let mut term = $term::Terminal::new(ROWS, COLS);
            term.set_max_scrollback(0);
            for chunk in data.chunks(4096) {
                term.advance(black_box(chunk));
            }
            black_box(term.grid().scrollback_len() as u64)
        }

        /// Scrolls `lines` lines of the corpus into the scrollback.
        pub fn scroll(lines: usize) -> u64 {
            black_box(filled(lines).grid().scrollback_len() as u64)
        }

        /// A terminal that has scrolled `lines` lines of the corpus.
        pub fn filled(lines: usize) -> $term::Terminal {
            let data = corpus(lines);
            let mut term = $term::Terminal::new(ROWS, COLS);
            term.set_max_scrollback(SCROLLBACK);
            for chunk in data.chunks(4096) {
                term.advance(chunk);
            }
            term
        }

        /// A literal and a regex search over the scrollback, in windows of
        /// 200 rows (a window is far under the haystack cap).
        pub fn search(lines: usize) -> u64 {
            let term = filled(lines);
            let rows: Vec<_> = (0..term.grid().scrollback_len())
                .filter_map(|i| term.grid().scrollback_line(i).cloned())
                .collect();
            let mut found = 0u64;
            for (pattern, regex) in [("needle42", false), (r"needle\d+ \S+", true)] {
                let options = $search::QueryOptions { regex, case_sensitive: true };
                let query = $search::Query::new(pattern, options).unwrap().unwrap();
                let mut haystack = $search::Haystack::new();
                for window in rows.chunks(200) {
                    haystack.fill(window, false, false);
                    let _ = query.find(&haystack, |_| {
                        found += 1;
                        ControlFlow::Continue(())
                    });
                }
            }
            black_box(found)
        }

        /// Live heap of an idle 24x80 terminal.
        pub fn heap_idle() -> u64 {
            let before = LIVE.load(Ordering::Relaxed);
            let term = black_box($term::Terminal::new(ROWS, COLS));
            let after = LIVE.load(Ordering::Relaxed);
            drop(term);
            after.saturating_sub(before) as u64
        }

        /// Live heap of a terminal holding the default scrollback, without
        /// the corpus buffer.
        pub fn heap_scrollback() -> u64 {
            let data = corpus(SCROLLBACK + 2_000);
            let before = LIVE.load(Ordering::Relaxed);
            let mut term = $term::Terminal::new(ROWS, COLS);
            term.set_max_scrollback(SCROLLBACK);
            for chunk in data.chunks(4096) {
                term.advance(chunk);
            }
            let after = LIVE.load(Ordering::Relaxed);
            black_box(&term);
            after.saturating_sub(before) as u64
        }
    };
}

mod base {
    use super::*;
    use base_search as search;
    use base_term as term;
    cases!(term, search);
}

mod head {
    use super::*;
    use head_search as search;
    use head_term as term;
    cases!(term, search);
}

/// Runs bench `case` on `side` and returns its checksum, or `None` for an
/// unknown case.
pub fn bench(case: &str, side: Side) -> Option<u64> {
    macro_rules! pick {
        ($f:ident $(, $arg:expr)?) => {
            match side {
                Side::Base => base::$f($($arg)?),
                Side::Head => head::$f($($arg)?),
            }
        };
    }
    Some(match case {
        // Instruction cases: well over a few tens of millions of instructions each.
        "feed" => pick!(feed, 60_000),
        "scroll" => pick!(scroll, 40_000),
        "search" => pick!(search, 12_000),
        "heap_idle" => pick!(heap_idle),
        "heap_scrollback" => pick!(heap_scrollback),
        _ => return None,
    })
}

/// No differential cases: the job measures cost, not behaviour.
pub fn diff(_case: &str, _seed: u64) -> Option<Result<(), String>> {
    None
}
