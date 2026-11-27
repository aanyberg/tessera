//! The fixed case set of the scheduled performance job (`scripts/perf-job.sh`,
//! #612), copied over the probe's `src/cases.rs` after `cargo xtask ab init`.
//! The crates under test are `base_term`, `head_term`, `base_grid`,
//! `head_grid`, `base_search` and `head_search`.
//!
//! Instruction cases (`feed`, `scroll`, `search`, `cat`, `rewrap`,
//! `vtebench`) are timed by `cargo xtask ab bench`. Heap cases (`heap_idle`,
//! `heap_scrollback`, `heap_lines_plain`, `heap_lines_mixed`) return the live
//! heap in bytes as their checksum, counted by the global allocator below; the
//! job runs those probe processes directly and reads the checksum. The
//! resident case (`rss_1m`) is a plain run whose peak resident set the job
//! reads from `/usr/bin/time -l` (#51). Changing a corpus changes every later
//! number: record it in docs/adr/0049 and expect one step in the trend.

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
/// The history of the resident case (#51: resident memory per 1M lines).
const RSS_LINES: usize = 1_000_000;
/// What vtebench repeats each benchmark's output up to (its `--min-bytes`
/// default).
const VTEBENCH_MIN_BYTES: usize = 1 << 20;

/// Output with SGR runs, wide and combining characters and line breaks, from
/// a fixed seed: the same bytes on both sides and on every run.
fn corpus(lines: usize) -> Vec<u8> {
    let mut rng = Rng::new(0x612);
    let words = [
        "alpha",
        "needle42",
        "scroll",
        "\u{4e2d}\u{6587}",
        "e\u{301}",
        "tessera",
        "0123456789",
    ];
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

/// `cat` of a plain ASCII text file: source-like lines of 0 to 119
/// printable characters (about a third wrap at 80 columns), with the CRLF a
/// PTY's `onlcr` makes of each line feed. 256 KiB of lines repeated to at
/// least `bytes` bytes, so generating them costs little next to feeding them.
fn plain_text(bytes: usize) -> Vec<u8> {
    let mut rng = Rng::new(0x51);
    let mut out = Vec::with_capacity((256 << 10) + 128);
    while out.len() < 256 << 10 {
        let indent = 4 * rng.below(4) as usize;
        let len = rng.below(120) as usize;
        out.resize(out.len() + indent.min(len), b' ');
        for _ in indent.min(len)..len {
            out.push(match rng.below(8) {
                0 => b' ',
                1 => b"(){};,.=+-*/_<>:'\"#"[rng.below(19) as usize],
                2 => b'0' + rng.below(10) as u8,
                _ => b'a' + rng.below(26) as u8,
            });
        }
        out.extend_from_slice(b"\r\n");
    }
    out.repeat(bytes.div_ceil(out.len()))
}

/// One full 80-column line of printable ASCII (the last character never a
/// space) and its CRLF: the plain line of the bytes-per-line bar (#51).
fn plain_line(rng: &mut Rng, out: &mut Vec<u8>) {
    for col in 0..COLS {
        let ch = if col + 1 < COLS && rng.below(6) == 0 {
            b' '
        } else {
            b'!' + rng.below(94) as u8
        };
        out.push(ch);
    }
    out.extend_from_slice(b"\r\n");
}

/// One full 80-column line of coloured words, wide (CJK) characters, a
/// combining sequence and an emoji, padded with spaces to the last column:
/// the coloured/wide-character mix of the bytes-per-line case (#51).
fn mixed_line(rng: &mut Rng, out: &mut Vec<u8>) {
    // (text, columns)
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
        let (word, width) = *rng.pick(&words);
        if col + width + 1 > usize::from(COLS) {
            break;
        }
        match rng.below(4) {
            0 => out.extend_from_slice(format!("\x1b[{}m", 31 + rng.below(7)).as_bytes()),
            1 => out.extend_from_slice(format!("\x1b[1;38;5;{}m", rng.below(256)).as_bytes()),
            2 => out.extend_from_slice(b"\x1b[0m"),
            _ => {}
        }
        out.extend_from_slice(word.as_bytes());
        out.push(b' ');
        col += width + 1;
    }
    out.extend_from_slice(b"\x1b[0m");
    out.resize(out.len() + usize::from(COLS) - col, b' ');
    out.extend_from_slice(b"\r\n");
}

/// `lines` lines of one kind (`plain_line` or `mixed_line`).
fn full_lines(lines: usize, plain: bool) -> Vec<u8> {
    let mut rng = Rng::new(if plain { 0x5101 } else { 0x5102 });
    let mut out = Vec::new();
    for _ in 0..lines {
        if plain {
            plain_line(&mut rng, &mut out);
        } else {
            mixed_line(&mut rng, &mut out);
        }
    }
    out
}

/// The code point runs of vtebench 0.3.1's `benchmarks/unicode/symbols`
/// first line; its second is U+1F600..=U+1F64F.
const VTEBENCH_SYMBOLS: [(u32, u32); 21] = [
    (0xa1, 0x304),
    (0x370, 0x487),
    (0x48a, 0x595),
    (0x5be, 0x604),
    (0x606, 0x614),
    (0x61b, 0x64f),
    (0x660, 0x6da),
    (0x6de, 0x6e3),
    (0x6e5, 0x734),
    (0x74b, 0x7aa),
    (0x910, 0x945),
    (0x949, 0x955),
    (0x958, 0xa36),
    (0x10a1, 0x1164),
    (0x1200, 0x167d),
    (0x1d00, 0x1dc4),
    (0x1dfa, 0x206a),
    (0x2070, 0x20d4),
    (0x20f1, 0x28ff),
    (0x2e80, 0x9fef),
    (0xac00, 0xd7fb),
];

/// The bytes vtebench 0.3.1 writes for each of its ten benchmarks in an
/// 80x24 terminal (`tput cols` and `tput lines`), as (name, setup,
/// benchmark), the benchmark repeated to at least `VTEBENCH_MIN_BYTES` as
/// vtebench does. Each sample starts with RIS (`ESC c`). The shell scripts
/// are reproduced byte for byte, a line feed as the CRLF of `onlcr`.
fn vtebench_streams() -> Vec<(&'static str, Vec<u8>, Vec<u8>)> {
    let (cols, lines) = (u32::from(COLS), u32::from(ROWS));
    let alphabet = b'A'..=b'Z';
    let mut dense = Vec::new();
    for (offset, ch) in alphabet.clone().enumerate() {
        dense.extend_from_slice(b"\x1b[H");
        for line in 1..=lines {
            for column in 1..=cols {
                let index = line + column + offset as u32;
                let fg = index % 156 + 100;
                let bg = 255 - index % 156 + 100;
                dense.extend_from_slice(format!("\x1b[38;5;{fg};48;5;{bg};1;3;4m").as_bytes());
                dense.push(ch);
            }
        }
    }
    let mut light = Vec::new();
    for ch in alphabet.clone() {
        light.extend_from_slice(b"\x1b[H");
        light.resize(light.len() + (cols * lines) as usize, ch);
    }
    let mut motion = Vec::new();
    for ch in alphabet.clone() {
        let (mut c0, mut c1, mut l0, mut l1) = (1, cols, 1, lines);
        loop {
            let (mut column, mut line) = (c0, l0);
            let mut at = |line: u32, column: u32| {
                motion.extend_from_slice(format!("\x1b[{line};{column}H").as_bytes());
                motion.push(ch);
            };
            while column < c1 {
                at(line, column);
                column += 1;
            }
            while line < l1 {
                at(line, column);
                line += 1;
            }
            while column > c0 {
                at(line, column);
                column -= 1;
            }
            while line > l0 {
                at(line, column);
                line -= 1;
            }
            c0 += 1;
            l0 += 1;
            c1 -= 1;
            l1 -= 1;
            if c0 > c1 || l0 > l1 {
                break;
            }
        }
    }
    let mut fullscreen = Vec::new();
    for ch in alphabet {
        fullscreen.resize(fullscreen.len() + cols as usize, ch);
        fullscreen.extend_from_slice(b"\r\n");
    }
    let mut symbols = String::new();
    for (first, last) in VTEBENCH_SYMBOLS {
        symbols.extend((first..=last).filter_map(char::from_u32));
    }
    symbols.push_str("\r\n");
    symbols.extend((0x1f600..=0x1f64f).filter_map(char::from_u32));
    symbols.push_str("\r\n");
    let alt = b"\x1b[?1049h".to_vec();
    let region = |top: u32, bottom: u32| format!("\x1b[?1049h\x1b[{top};{bottom}r").into_bytes();
    let y = b"y\r\n".to_vec();
    let streams = vec![
        ("cursor_motion", Vec::new(), motion),
        ("dense_cells", alt.clone(), dense),
        ("light_cells", alt.clone(), light),
        ("scrolling", Vec::new(), y.clone()),
        ("scrolling_bottom_region", region(1, lines - 1), y.clone()),
        (
            "scrolling_bottom_small_region",
            region(1, lines / 2),
            y.clone(),
        ),
        ("scrolling_fullscreen", Vec::new(), fullscreen),
        ("scrolling_top_region", region(2, lines), y.clone()),
        ("scrolling_top_small_region", region(lines / 2, lines), y),
        ("unicode", alt, symbols.into_bytes()),
    ];
    streams
        .into_iter()
        .map(|(name, setup, once)| {
            let count = (VTEBENCH_MIN_BYTES - 1) / once.len() + 1;
            (name, setup, once.repeat(count))
        })
        .collect()
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
                let options = $search::QueryOptions {
                    regex,
                    case_sensitive: true,
                };
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

        /// `cat` of an 8 MiB plain ASCII file into a terminal with the
        /// default scrollback, in 4 KiB reads: plain-text throughput (#51).
        pub fn cat() -> u64 {
            let data = plain_text(8 << 20);
            let mut term = $term::Terminal::new(ROWS, COLS);
            term.set_max_scrollback(SCROLLBACK);
            for chunk in data.chunks(4096) {
                term.advance(black_box(chunk));
            }
            black_box(term.grid().scrollback_len() as u64)
        }

        /// Re-wraps a full default scrollback of the mixed corpus: twenty
        /// width changes between 80 and 100 columns, the bar of
        /// `docs/config.md` (`scrollback_lines`) that #51 must keep.
        pub fn rewrap() -> u64 {
            let mut term = filled(SCROLLBACK + 2_000);
            for i in 0..20 {
                term.resize(ROWS, if i % 2 == 0 { 100 } else { COLS });
            }
            black_box(term.grid().scrollback_len() as u64)
        }

        /// vtebench 0.3.1's ten benchmarks (`vtebench_streams`), each run
        /// once after a RIS, as `vtebench --max-samples 1 --warmup 0` writes
        /// them to its terminal.
        pub fn vtebench() -> u64 {
            let mut term = $term::Terminal::new(ROWS, COLS);
            term.set_max_scrollback(SCROLLBACK);
            let mut sum = 0u64;
            for (_, setup, bytes) in vtebench_streams() {
                term.advance(b"\x1bc");
                term.advance(&setup);
                for chunk in bytes.chunks(4096) {
                    term.advance(black_box(chunk));
                }
                sum += term.grid().scrollback_len() as u64;
            }
            black_box(sum)
        }

        /// Heap bytes that 10 000 lines of scrollback cost, of full 80-column
        /// lines: plain ASCII (`plain`) or the coloured/wide-character mix.
        /// The difference between a terminal keeping the default scrollback
        /// and one keeping none, fed the same lines; divide by 10 000 for the
        /// bytes per line of the #51 bar.
        pub fn heap_lines(plain: bool) -> u64 {
            let data = full_lines(SCROLLBACK + usize::from(ROWS), plain);
            let heap = |cap: usize| {
                let before = LIVE.load(Ordering::Relaxed);
                let mut term = $term::Terminal::new(ROWS, COLS);
                term.set_max_scrollback(cap);
                for chunk in data.chunks(4096) {
                    term.advance(chunk);
                }
                assert_eq!(
                    term.grid().scrollback_len(),
                    cap,
                    "the corpus fills the history"
                );
                let after = LIVE.load(Ordering::Relaxed);
                black_box(&term);
                drop(term);
                after.saturating_sub(before)
            };
            let none = heap(0);
            black_box(heap(SCROLLBACK).saturating_sub(none) as u64)
        }

        /// A terminal holding 1 000 000 lines of plain 80-column scrollback,
        /// fed in 4 KiB reads that are generated as it goes (no corpus held):
        /// the job reads the process's peak resident set (#51).
        pub fn rss_1m() -> u64 {
            let mut rng = Rng::new(0x5103);
            let mut term = $term::Terminal::new(ROWS, COLS);
            term.set_max_scrollback(RSS_LINES);
            let mut chunk = Vec::with_capacity(8192);
            for _ in 0..RSS_LINES + usize::from(ROWS) {
                plain_line(&mut rng, &mut chunk);
                if chunk.len() >= 4096 {
                    term.advance(&chunk);
                    chunk.clear();
                }
            }
            term.advance(&chunk);
            black_box(term.grid().scrollback_len() as u64)
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
        // Instruction cases: each must clear ab's floor of 20x the empty probe
        // (about 320M); search is the closest, at about 1.3x the floor.
        "feed" => pick!(feed, 60_000),
        "scroll" => pick!(scroll, 40_000),
        "search" => pick!(search, 12_000),
        "cat" => pick!(cat),
        "rewrap" => pick!(rewrap),
        "vtebench" => pick!(vtebench),
        "heap_idle" => pick!(heap_idle),
        "heap_scrollback" => pick!(heap_scrollback),
        "heap_lines_plain" => pick!(heap_lines, true),
        "heap_lines_mixed" => pick!(heap_lines, false),
        "rss_1m" => pick!(rss_1m),
        _ => return None,
    })
}

/// No differential cases: the job measures cost, not behaviour.
pub fn diff(_case: &str, _seed: u64) -> Option<Result<(), String>> {
    None
}
