//! The same bytes fed to `tessera_term::Terminal` whole and in chunks give
//! the same terminal: `Terminal::advance` promises that splitting the
//! output at any byte boundary gives the same result as one call (it holds
//! back an incomplete UTF-8 character because `vte` 0.15 mishandles one
//! completed across two calls).
//!
//! Input layout: `[rows] [cols] [seed; 4] bytes...`. The grid is small
//! (`rows` modulo 25, `cols` modulo 81, zero raised to one by the terminal)
//! so wrapping, scrolling and the margins are reached quickly; the chunk
//! lengths (1 to 16 bytes) come from a xorshift generator seeded by `seed`.
//!
//! Compared: every visible and scrollback line (cells, styles, clusters,
//! wrap flags), the cursor, pending wrap, pen, scroll region, the modes,
//! the keyboard modes, the conformance level and the requests. Hidden state
//! is compared by probing: after the first comparison both terminals get
//! the same probes, each followed by another comparison: REP (`CSI 3 b`,
//! the last printed character), DECRC (`ESC 8`, the saved cursor), three
//! tabs (the tab stops), a DECSET 1049 reset (back to the stashed primary
//! screen and its cursor), DECRC and tabs there (the primary's saved cursor
//! and stops), and DECSET 1049 with DECRC (the alternate screen's saved
//! cursor slot, kept between visits).

#![no_main]

use std::fmt::Write as _;

use libfuzzer_sys::fuzz_target;
use tessera_grid::{CellFlags, Grid};
use tessera_term::Terminal;

/// The `render_screen` text of `tessera-term`'s tests, for failure messages:
/// one line per row, the cursor as `|`, a soft wrap as a trailing `\`.
fn render(grid: &Grid) -> String {
    let mut out = String::new();
    let cursor = grid.cursor();
    for (row, line) in grid.visible_lines().iter().enumerate() {
        let mut text = String::new();
        for (col, cell) in line.cells.iter().enumerate() {
            if !cell.flags.contains(CellFlags::SPACER) {
                match line.cluster(col) {
                    Some(cluster) => text.push_str(cluster),
                    None => text.push(cell.ch),
                }
            }
            if usize::from(cursor.row) == row && usize::from(cursor.col) == col {
                text.push('|');
            }
        }
        let _ = writeln!(
            out,
            "{}{}",
            text.trim_end_matches(' '),
            if line.wrapped { "\\" } else { "" }
        );
    }
    out
}

#[track_caller]
fn assert_same(whole: &mut Terminal, chunked: &mut Terminal, stage: &str) {
    let (a, b) = (whole.grid(), chunked.grid());
    let same_lines =
        a.visible_lines() == b.visible_lines() && a.logical_lines().eq(b.logical_lines());
    assert!(
        same_lines
            && a.cursor() == b.cursor()
            && a.pending_wrap() == b.pending_wrap()
            && a.pen() == b.pen()
            && a.scroll_region() == b.scroll_region()
            && a.is_alt_screen() == b.is_alt_screen(),
        "{stage}: whole and chunked grids differ\nwhole ({:?}, wrap {}):\n{}chunked ({:?}, wrap {}):\n{}",
        a.cursor(),
        a.pending_wrap(),
        render(a),
        b.cursor(),
        b.pending_wrap(),
        render(b),
    );
    assert_eq!(whole.modes(), chunked.modes(), "{stage}: modes differ");
    assert_eq!(
        whole.keyboard_modes(),
        chunked.keyboard_modes(),
        "{stage}: keyboard modes differ"
    );
    assert_eq!(
        whole.conformance_level(),
        chunked.conformance_level(),
        "{stage}"
    );
    assert_eq!(
        whole.c1_8bit_replies(),
        chunked.c1_8bit_replies(),
        "{stage}"
    );
    assert_eq!(
        whole.take_requests(),
        chunked.take_requests(),
        "{stage}: requests differ"
    );
}

/// Sequences that expose hidden state, fed to both terminals in order after
/// the input, each followed by a comparison (module docs).
const PROBES: [(&[u8], &str); 6] = [
    // REP first: any other sequence forgets the character it repeats.
    (b"\x1b[3b", "after REP"),
    (b"\x1b8", "after DECRC"),
    (b"\t\t\t", "after tabs on the active screen"),
    (b"\x1b[?1049l", "after leaving the alternate screen"),
    (b"\x1b8\t\t\t", "after DECRC and tabs on the primary screen"),
    (b"\x1b[?1049h\x1b8", "after DECRC on the alternate screen"),
];

fn probe(whole: &mut Terminal, chunked: &mut Terminal, bytes: &[u8], stage: &str) {
    whole.advance(bytes);
    chunked.advance(bytes);
    assert_same(whole, chunked, stage);
}

fuzz_target!(|data: &[u8]| {
    let [rows, cols, s0, s1, s2, s3, rest @ ..] = data else {
        return;
    };
    let (rows, cols) = (u16::from(*rows) % 25, u16::from(*cols) % 81);
    let mut whole = Terminal::new(rows, cols);
    let mut chunked = Terminal::new(rows, cols);

    whole.advance(rest);

    // xorshift32; a zero state would stay zero, so mix in a constant.
    let mut state = u32::from_le_bytes([*s0, *s1, *s2, *s3]) ^ 0x9E37_79B9;
    if state == 0 {
        state = 1;
    }
    let mut offset = 0;
    while offset < rest.len() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let len = (state % 16) as usize + 1;
        let end = (offset + len).min(rest.len());
        chunked.advance(&rest[offset..end]);
        offset = end;
    }

    assert_same(&mut whole, &mut chunked, "after the input");
    for (bytes, stage) in PROBES {
        probe(&mut whole, &mut chunked, bytes, stage);
    }
});
