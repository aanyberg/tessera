//! Left and right margins (#97): DECLRMM (`CSI ? 69 h`), DECSLRM
//! (`CSI Pl ; Pr s`), the VT420 column operations DECIC, DECDC, DECBI and
//! DECFI, reverse wraparound (DECSET 45 and 1045) and the reports that go
//! with them (DA1 21, DECRQM ?69/?45/?1045, DECRQSS `s`).
//!
//! Expected behaviour comes from `tests/fixtures/margins_spec.txt`, whose
//! header cites xterm's source and esctest2 and lists the milestone lead's
//! decisions (D1-D9, ADR 0028). The fixture mixes two layouts, both read
//! here:
//!
//! - grid rows in the `csi_spec.txt` layout, checked by
//!   [`spec::check_case`] whole and byte by byte;
//! - reply rows (a `reply:` line, `\e` for ESC, optionally `step: resize
//!   RxC`), checked here: the [`Request::Reply`] bytes of the whole row,
//!   concatenated, fed whole and byte by byte.
//!
//! [`every_fixture_row_holds`] runs every row through
//! [`common::check_table`], so one run reports every failing row.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;
#[allow(dead_code)] // Shared with tests/csi.rs and tests/vttest.rs.
mod spec;

use common::{Chunking, check_table, decode};
use tessera_term::{Request, Terminal};

const FIXTURE: &str = include_str!("fixtures/margins_spec.txt");

/// Rows in the fixture; a row lost from the file fails the table.
const FIXTURE_ROWS: usize = 158;

/// One `###` block of the fixture, its text with `\e` already turned into
/// `\x1b` in the `step:` and `reply:` lines.
struct Block {
    id: String,
    text: String,
}

/// Splits the fixture into its `###` blocks.
fn blocks() -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    for line in FIXTURE.lines() {
        if let Some(id) = line.strip_prefix("### ") {
            blocks.push(Block {
                id: id.to_owned(),
                text: String::new(),
            });
        }
        let Some(block) = blocks.last_mut() else {
            continue;
        };
        if line.starts_with("step:") || line.starts_with("reply:") {
            block.text.push_str(&escape_e(line));
        } else {
            block.text.push_str(line);
        }
        block.text.push('\n');
    }
    blocks
}

/// Replaces the reports fixture's `\e` with `\x1b`, leaving `\\` alone.
fn escape_e(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('e') => out.push_str("\\x1b"),
            Some(next) => {
                out.push('\\');
                out.push(next);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// One step of a reply row.
enum ReplyStep {
    Feed(Vec<u8>),
    Resize(u16, u16),
}

/// Runs a reply row and returns its concatenated replies.
fn run_reply_row(size: (u16, u16), steps: &[ReplyStep], chunking: Chunking) -> Vec<u8> {
    let mut term = Terminal::new(size.0, size.1);
    for step in steps {
        match step {
            ReplyStep::Feed(bytes) => match chunking {
                Chunking::Whole => term.advance(bytes),
                Chunking::Bytewise => {
                    for byte in bytes {
                        term.advance(std::slice::from_ref(byte));
                    }
                }
            },
            ReplyStep::Resize(rows, cols) => term.resize(*rows, *cols),
        }
    }
    let (replies, _) = common::split_replies(&term.take_requests());
    replies
}

fn parse_size(text: &str) -> (u16, u16) {
    let (rows, cols) = text.trim().split_once('x').expect("size is `RxC`");
    (
        rows.parse().expect("size rows"),
        cols.parse().expect("size cols"),
    )
}

/// Checks a reply row: every `reply:` line, concatenated, against the
/// replies of every step, whole and byte by byte.
fn check_reply_row(block: &Block) {
    let mut size = None;
    let mut steps = Vec::new();
    let mut expected = Vec::new();
    for line in block.text.lines() {
        if let Some(rest) = line.strip_prefix("size: ") {
            size = Some(parse_size(rest));
        } else if let Some(rest) = line.strip_prefix("step: feed ") {
            steps.push(ReplyStep::Feed(decode(rest)));
        } else if let Some(rest) = line.strip_prefix("step: resize ") {
            let (rows, cols) = parse_size(rest);
            steps.push(ReplyStep::Resize(rows, cols));
        } else if let Some(rest) = line.strip_prefix("reply: ") {
            expected.extend(decode(rest.trim()));
        }
    }
    let size = size.unwrap_or_else(|| panic!("{}: no size", block.id));
    for chunking in [Chunking::Whole, Chunking::Bytewise] {
        let got = run_reply_row(size, &steps, chunking);
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&expected),
            "{} ({chunking:?}): replies",
            block.id
        );
    }
}

/// Checks a grid row with the shared `csi_spec.txt` reader, then the
/// grid's structural invariants after it.
fn check_grid_row(block: &Block) {
    let cases = spec::parse_fixture(&block.text);
    assert_eq!(cases.len(), 1, "{}: one case per block", block.id);
    let errors: Vec<String> = [Chunking::Whole, Chunking::Bytewise]
        .into_iter()
        .flat_map(|chunking| spec::check_case(&cases[0], chunking))
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
    // What the screen cannot show: no padding flag on a blank left at an
    // inner margin, wide pairs and clusters intact.
    let (term, _) = cases[0].script.run(Chunking::Whole);
    term.grid().check_invariants();
}

#[test]
fn every_fixture_row_holds() {
    let rows = blocks().into_iter().map(|block| (block.id.clone(), block));
    check_table("margins_spec.txt", FIXTURE_ROWS, rows, |block| {
        if block.text.contains("\nreply:") {
            check_reply_row(&block);
        } else {
            check_grid_row(&block);
        }
    });
}

/// A reply row that carries a resize, the one step `check_reply_row` adds
/// to the shared harness, really resizes: the replies differ without it.
#[test]
fn reply_rows_resize() {
    let steps = [
        ReplyStep::Feed(decode("\\x1b[?69h\\x1b[10;40s")),
        ReplyStep::Resize(24, 100),
        ReplyStep::Feed(decode("\\x1bP$qs\\x1b\\\\")),
    ];
    let got = run_reply_row((24, 80), &steps, Chunking::Whole);
    assert_eq!(got, b"\x1bP1$r1;100s\x1b\\");
}

/// No margin sequence emits a request: a flood of them leaves the queue
/// empty, so the host never has a per-byte backlog from #97.
#[test]
fn margin_sequences_emit_no_requests() {
    let mut term = Terminal::new(24, 80);
    let chunk = b"\x1b[?69h\x1b[5;70s\x1b[?45h\x1b[?1045hab\x08\x08\x08\x1b6\x1b9\x1b[2'}\x1b[2'~\x1b[3D\r\n\x1b[?69l\x1b[s";
    for _ in 0..10_000 {
        term.advance(chunk);
    }
    let requests: Vec<Request> = term.take_requests();
    assert!(requests.is_empty(), "{} requests", requests.len());
    term.grid().check_invariants();
}

/// CUB with extended reverse wrap walks at most a lap of the region, a
/// row at a time, not up to 65535 single steps as xterm's `CursorBack`
/// does: a flood of `CSI 65535 D` between two-column margins finishes far
/// inside the bound (about 0.1 s), which the step-by-step walk misses by
/// four times in a debug build (measured 2026-11-01: 8.3 s).
/// The other sequences whose cost grows with a parameter (DECIC, DECDC,
/// margin scrolls) cost one pass over the region and run here too.
#[test]
fn worst_case_floods_stay_cheap() {
    let mut term = Terminal::new(100, 200);
    term.advance(b"\x1b[?1045h\x1b[?69h\x1b[1;2s\x1b[1;100r");
    let flood = b"\x1b[65535D".repeat(20_000);
    let start = std::time::Instant::now();
    term.advance(&flood);
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "{} bytes of CUB 65535 took {elapsed:?}",
        flood.len()
    );
    term.advance(&b"\x1b[65535'}\x1b[65535'~\x1b[100;1H\n\x1b[1;1H\x1bM\x08".repeat(2_000));
    term.grid().check_invariants();
    assert_eq!(term.take_requests(), Vec::<Request>::new());
}
