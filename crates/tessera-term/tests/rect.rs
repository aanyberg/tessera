//! Rectangular area operations (#251, ADR 0046): DECERA, DECFRA, DECCRA
//! and DECSERA.
//!
//! Expected behaviour comes from `tests/fixtures/rect_spec.txt`, rows
//! derived from a model of xterm's source with the #251 decisions applied.
//! Every row runs through the shared [`spec::check_case`] (screen, cursor,
//! pending wrap, requests, scrollback), whole and byte by byte, and is then
//! checked, whole and byte by byte, on the keys that reader ignores:
//!
//! - `pstate: <mode>/<pen>`: [`Terminal::protection_mode`] and whether the
//!   pen carries [`Style::PROTECTED`] after the script;
//! - `attr: (row,c0)-(row,c1) <pen>`: the style of every cell of the run,
//!   in [`common::pen_text`] notation, [`Style::PROTECTED`] left out (the
//!   `pstate` and the screen after an erase prove that bit).
//!
//! `xterm_screen:` and `xterm_attr:` record what stock xterm does where it
//! differs; they are documentation, not checked. The direct tests below
//! cover what a screen cannot show: hyperlinks, damage, the state an
//! operation must leave alone, the bounds of docs/security.md, and a
//! property test over hostile parameters.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;
#[allow(dead_code)] // Shared with tests/csi.rs and tests/vttest.rs.
mod spec;

use common::{Chunking, Script, Step, decode, pen_text};
use tessera_grid::{Cell, DamageRows, Point, Style};
use tessera_term::Terminal;

const FIXTURE: &str = include_str!("fixtures/rect_spec.txt");
const FIXTURE_NAME: &str = "fixtures/rect_spec.txt";

/// Cases in the fixture: a row lost from it fails the table.
const CASES: usize = 132;

/// Replaces `\e` with `\x1b` in a fixture payload, leaving `\\` alone, so
/// [`common::decode`] reads it.
fn expand_esc(text: &str) -> String {
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

/// One `attr:` run: row, first and last column, pen text.
type Run = (u16, u16, u16, String);

/// One fixture row.
#[derive(Debug)]
struct Row {
    id: String,
    /// The row's text with `\e` expanded, from its `###` heading on.
    block: String,
    script: Script,
    pstate: (String, bool),
    attrs: Vec<Run>,
    flags: Vec<String>,
}

fn parse_point(raw: &str) -> (u16, u16) {
    let inner = raw
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or_else(|| panic!("`{raw}` is not `(row,col)`"));
    let (row, col) = inner.split_once(',').expect("`(row,col)`");
    (
        row.trim().parse().expect("attr row"),
        col.trim().parse().expect("attr col"),
    )
}

/// Every row of the fixture, in order.
fn rows() -> Vec<Row> {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in FIXTURE.lines() {
        if line.starts_with("### ") {
            blocks.push(vec![line]);
        } else if line.starts_with("# ") {
            // A section heading or the closing notes end the row.
            blocks.push(Vec::new());
        } else if let Some(block) = blocks.last_mut() {
            block.push(line);
        }
    }
    blocks
        .into_iter()
        .filter(|block| block.first().is_some_and(|l| l.starts_with("### ")))
        .map(|block| {
            let block = expand_esc(&block.join("\n"));
            let id = block.lines().next().expect("heading")[4..].to_owned();
            let field = |key: &str| {
                block
                    .lines()
                    .find_map(|l| l.strip_prefix(key).map(str::trim).map(str::to_owned))
            };
            let (rows, cols) = field("size: ")
                .and_then(|s| {
                    let (r, c) = s.split_once('x')?;
                    Some((r.parse().ok()?, c.parse().ok()?))
                })
                .unwrap_or_else(|| panic!("{id}: no size"));
            let steps = block
                .lines()
                .filter_map(|l| l.strip_prefix("step: feed "))
                .map(|p| Step::Feed(decode(p)))
                .collect();
            let pstate = field("pstate: ").map_or_else(
                || panic!("{id}: no pstate"),
                |p| {
                    let (mode, pen) = p.split_once('/').expect("pstate is mode/pen");
                    (mode.to_owned(), pen == "1")
                },
            );
            let attrs = block
                .lines()
                .filter_map(|l| l.strip_prefix("attr: "))
                .map(|rest| {
                    let (range, pen) = rest.split_once(' ').expect("`attr: <range> <pen>`");
                    let (from, to) = range.split_once('-').expect("`(r,c0)-(r,c1)`");
                    let ((at, start), (end_at, end)) = (parse_point(from), parse_point(to));
                    assert_eq!(at, end_at, "{id}: an attr run stays on one row");
                    assert!(start <= end, "{id}: attr run {range} is reversed");
                    (at, start, end, pen.trim().to_owned())
                })
                .collect();
            Row {
                pstate,
                attrs,
                flags: field("flags: ")
                    .map_or_default(|f| f.split_whitespace().map(str::to_owned).collect()),
                script: Script { rows, cols, steps },
                block,
                id,
            }
        })
        .collect()
}

/// A cell's style as the `attr:` rows write it: without the protection bit.
fn attr_text(cell: &Cell) -> String {
    let mut style = cell.style;
    style.flags &= !Style::PROTECTED;
    pen_text(style)
}

/// Every mismatch of `row`, run with `chunking`.
fn check_row(row: &Row, chunking: Chunking) -> Vec<String> {
    let cases = spec::parse_fixture(&row.block);
    assert_eq!(cases.len(), 1, "{}: one case per block", row.id);
    let mut errors = spec::check_case(&cases[0], chunking);
    let (term, _) = row.script.run(chunking);
    let (mode, pen) = &row.pstate;
    let got = (
        term.protection_mode(),
        term.grid().pen().has(Style::PROTECTED),
    );
    if got != (mode.as_str(), *pen) {
        errors.push(format!(
            "{} ({chunking:?}) pstate: expected {mode}/{}, got {}/{}",
            row.id,
            u8::from(*pen),
            got.0,
            u8::from(got.1)
        ));
    }
    for (r, c0, c1, want) in &row.attrs {
        for col in *c0..=*c1 {
            let cell = term.grid().cell(Point::new(*r, col)).expect("attr cell");
            let got = attr_text(cell);
            if got != *want {
                errors.push(format!(
                    "{} ({chunking:?}) cell ({r},{col}): {got}, expected {want}",
                    row.id
                ));
            }
        }
    }
    term.grid().check_invariants();
    errors
}

/// Every row holds, whole and byte by byte.
#[test]
fn every_row_holds() {
    let rows = rows();
    common::check_table(
        FIXTURE_NAME,
        CASES,
        rows.iter().map(|row| (row.id.clone(), row)),
        |row| {
            let errors: Vec<String> = [Chunking::Whole, Chunking::Bytewise]
                .into_iter()
                .flat_map(|chunking| check_row(row, chunking))
                .collect();
            assert!(errors.is_empty(), "{}", errors.join("\n"));
        },
    );
}

/// The table has control rows and decision rows, and every row that
/// records an xterm difference says so in its flags.
#[test]
fn the_table_has_controls_and_marks_xterm_differences() {
    let rows = rows();
    let flagged = |flag: &str| {
        rows.iter()
            .filter(|r| r.flags.iter().any(|f| f == flag))
            .count()
    };
    assert!(flagged("control") >= 5, "control rows");
    assert!(flagged("decision") >= 10, "decision rows");
    for row in &rows {
        let differs = row.block.contains("\nxterm_screen:") || row.block.contains("\nxterm_attr:");
        if differs {
            assert!(
                row.flags
                    .iter()
                    .any(|f| f == "xterm-differs" || f == "decision" || f == "xterm-quirk"),
                "{}: an xterm_* row without an xterm-differs or decision flag",
                row.id
            );
        }
    }
}

// ------------------------------------------------------------ direct tests

fn feed(rows: u16, cols: u16, bytes: &[u8]) -> Terminal {
    let mut term = Terminal::new(rows, cols);
    term.advance(bytes);
    term
}

fn screen_text(term: &Terminal) -> Vec<String> {
    term.grid()
        .visible_lines()
        .iter()
        .map(tessera_grid::Line::text)
        .collect()
}

fn replies(term: &mut Terminal) -> String {
    let (bytes, _) = common::split_replies(&term.take_requests());
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One of each operation over the rectangle rows 2-3, columns 2-4
/// (1-based), in the forms the tests below loop over.
const OPS: [(&str, &[u8]); 4] = [
    ("DECERA", b"\x1b[2;2;3;4$z"),
    ("DECFRA", b"\x1b[88;2;2;3;4$x"),
    ("DECCRA", b"\x1b[1;1;2;3;1;2;2;1$v"),
    ("DECSERA", b"\x1b[2;2;3;4${"),
];

/// Q1 (ADR 0034 amendment): DECFRA writes like printing, so a protecting
/// pen protects the filled cells and DECSED keeps them; DECERA's blanks
/// never carry the bit, whatever the pen.
#[test]
fn q1_decfra_cells_take_the_pens_protection_decera_blanks_never() {
    let mut term = feed(3, 6, b"\x1b[1\"q\x1b[88;1;1;1;3$x");
    let line = term.grid().line(0).expect("row");
    assert!(line.cells[..3].iter().all(Cell::is_protected));
    assert!(!line.cells[3].is_protected());
    term.advance(b"\x1b[?2J");
    assert_eq!(screen_text(&term)[0], "XXX", "DECSED kept the filled cells");
    // DECERA with the protecting pen: plain blanks, which DECSED erases.
    term.advance(b"\x1b[1;1;1;2$z");
    let line = term.grid().line(0).expect("row");
    assert!(
        line.cells[..2]
            .iter()
            .all(|c| !c.is_protected() && c.ch == ' ')
    );
    assert!(line.cells[2].is_protected(), "outside the rectangle");
    term.grid().check_invariants();
}

/// Q1, the other half: DECCRA copies the bit with the cell, DECSERA keeps
/// it on the cells it erases (ISO mode) and on the ones it keeps (DEC
/// mode).
#[test]
fn q1_deccra_copies_protection_decsera_keeps_it() {
    let mut term = feed(3, 6, b"\x1b[1\"qab\x1b[0\"qcd\x1b[1;1;1;4;1;2;1$v");
    let line = term.grid().line(1).expect("row");
    assert_eq!(line.text(), "abcd");
    let protected: Vec<bool> = line.cells[..4].iter().map(Cell::is_protected).collect();
    assert_eq!(protected, [true, true, false, false]);
    // DEC mode: DECSERA keeps a and b on row 2, erases c and d.
    term.advance(b"\x1b[2;1;2;6${");
    assert_eq!(screen_text(&term)[1], "ab");
    // ISO mode: DECSERA erases the protected cells but keeps their bit.
    term.advance(b"\x1bV\x1bW\x1b[2;1;2;6${");
    let line = term.grid().line(1).expect("row");
    assert_eq!(line.text(), "");
    assert!(line.cells[0].is_protected() && line.cells[1].is_protected());
    term.grid().check_invariants();
}

/// Q3: DECCRA copies a cell's OSC 8 link (the same link, so the live-link
/// count does not grow), the erases and the fill drop it, and the link
/// dies with its last cell.
#[test]
fn q3_deccra_copies_links_erases_and_fills_drop_them() {
    let mut term = feed(
        4,
        8,
        b"\x1b]8;id=x;https://a.example/\x1b\\abc\x1b]8;;\x1b\\d",
    );
    assert_eq!(term.grid().live_hyperlinks(), 1);
    term.advance(b"\x1b[1;1;1;4;1;2;1$v");
    let line = term.grid().line(1).expect("row");
    assert_eq!(line.text(), "abcd");
    for col in 0..3 {
        let copied = line.hyperlink(col).expect("copied link");
        assert_eq!(
            (copied.uri(), copied.id()),
            ("https://a.example/", Some("x"))
        );
    }
    assert!(line.hyperlink(3).is_none(), "d had no link");
    assert_eq!(term.grid().live_hyperlinks(), 1, "the copy shares the link");
    // DECFRA over the original, DECERA and DECSERA over the copy: the
    // link has no cell left and dies.
    term.advance(b"\x1b[120;1;1;1;3$x");
    assert!((0..3).all(|c| term.grid().line(0).expect("row").hyperlink(c).is_none()));
    assert_eq!(term.grid().live_hyperlinks(), 1, "the copy still holds it");
    term.advance(b"\x1b[2;1;2;1$z\x1b[2;2;2;3${");
    let line = term.grid().line(1).expect("row");
    assert!((0..4).all(|c| line.hyperlink(c).is_none()));
    assert_eq!(term.grid().live_hyperlinks(), 0);
    term.grid().check_invariants();
}

/// Q3 with clusters and wide characters: DECCRA copies the cluster text
/// and both halves; an overlapping copy within one row works through its
/// scratch row.
#[test]
fn q3_deccra_copies_clusters_and_overlaps_within_a_row() {
    let mut term = feed(2, 8, "e\u{301}中x".as_bytes());
    // Shift columns 1-4 right by one, within row 1.
    term.advance(b"\x1b[1;1;1;4;1;1;2;1$v");
    let line = term.grid().line(0).expect("row");
    assert_eq!(line.text(), "e\u{301}e\u{301}中x");
    assert!(line.cells[2].is_wide() && line.cells[3].is_spacer());
    // And left by one: the cut wide character is blanked, never split.
    term.advance(b"\x1b[1;4;1;5;1;1;3;1$v");
    term.grid().check_invariants();
    let line = term.grid().line(0).expect("row");
    assert!(line.cells.iter().all(|c| !c.is_spacer() || c.ch == ' '));
}

/// Q4: Pch maps through GL as printing would (DEC Special Graphics), and
/// DECFRA does not consume a pending single shift.
#[test]
fn q4_pch_maps_through_gl_and_leaves_a_single_shift_pending() {
    let term = feed(2, 6, b"\x1b(0\x1b[113;1;1;1;2$x");
    assert_eq!(screen_text(&term)[0], "──");
    // G2 = Special Graphics, SS2 pending: the fill takes GL (ASCII) and
    // the next printed character still takes G2.
    let term = feed(2, 6, b"\x1b*0\x1bN\x1b[113;1;1;1;2$x\x1b[1;3Hq");
    assert_eq!(screen_text(&term)[0], "qq─");
    // A Latin-1 Pch maps through GR (ASCII by default: itself).
    let term = feed(2, 6, b"\x1b[233;1;1;1;1$x");
    assert_eq!(screen_text(&term)[0], "é");
}

/// Q4: every Pch DECFRA refuses leaves the screen as it was.
#[test]
fn q4_refused_pch_leave_the_screen() {
    for pch in [
        1u32, 27, 31, 127, 128, 155, 159, 0x300, 0x4E00, 0xD800, 0xFDD0, 0xFFFE, 0xFFFF, 0x200B,
    ] {
        let seq = format!("abc\x1b[{pch};1;1;1;3$x");
        let term = feed(2, 6, seq.as_bytes());
        assert_eq!(screen_text(&term)[0], "abc", "Pch {pch:#x}");
    }
}

/// Q5: DECSCL 61-63 make all four no-ops that keep the screen; 64 and 65
/// bring them back.
#[test]
fn q5_level_gate() {
    for level in [61u8, 62, 63, 64, 65] {
        for (name, op) in OPS {
            let mut term = feed(4, 6, format!("\x1b[{level};1\"p").as_bytes());
            term.advance(b"abcdef\r\nghijkl\r\nmnopqr\r\nstuvwx");
            let before = screen_text(&term);
            term.advance(op);
            let changed = screen_text(&term) != before;
            assert_eq!(changed, level >= 64, "{name} at level {level}");
        }
    }
}

/// D10: a colon sub-parameter voids each operation, and a private marker
/// makes it another (ignored) sequence.
#[test]
fn sub_parameters_and_private_markers_void_the_operations() {
    for op in [
        &b"\x1b[2:1;2;3;4$z"[..],
        b"\x1b[88;2;2:1;3;4$x",
        b"\x1b[1;1;2;3;1;2;2:1;1$v",
        b"\x1b[2;2;3;4:0${",
        b"\x1b[?2;2;3;4$z",
        b"\x1b[?88;2;2;3;4$x",
        b"\x1b[?1;1;2;3;1;2;2;1$v",
        b"\x1b[?2;2;3;4${",
    ] {
        let mut term = feed(4, 6, b"abcdef\r\nghijkl\r\nmnopqr\r\nstuvwx");
        let before = screen_text(&term);
        term.advance(op);
        assert_eq!(screen_text(&term), before, "{op:?}");
    }
}

/// B05: an operation damages exactly the rows of its clamped rectangle
/// (DECCRA: of its destination); a no-op damages none. The cursor sits
/// on the first row of the rectangle, which `damage_since` always
/// reports.
#[test]
fn b05_damage_is_the_rectangles_rows() {
    let cases: [(&[u8], u16, u16); 7] = [
        (b"\x1b[2;2;4;4$z", 1, 3),
        (b"\x1b[88;2;2;4;4$x", 1, 3),
        (b"\x1b[2;2;4;4${", 1, 3),
        (b"\x1b[5;1;6;3;1;2;1$v", 1, 2),
        // Clipped at the bottom.
        (b"\x1b[2;1;99;3$z", 1, 7),
        // Inverted and refused: only the cursor row.
        (b"\x1b[4;1;2;3$z", 1, 1),
        (b"\x1b[31;2;1;4;3$x", 1, 1),
    ];
    for (op, top, bottom) in cases {
        let mut term = feed(8, 6, b"abcdef\r\nghijkl\r\nmnopqr\r\nstuvwx\x1b[2;1H");
        let generation = term.grid().damage_generation();
        assert_eq!(
            term.grid().damage_since(generation),
            DamageRows::Rows(1..=1),
            "only the cursor row before"
        );
        term.advance(op);
        assert_eq!(
            term.grid().damage_since(generation),
            DamageRows::Rows(top..=bottom),
            "{op:?}"
        );
    }
}

/// D1-4: a row a rectangle operation changed is reported like a row EL
/// changed, which is what selection and search clear and rescan on.
#[test]
fn a_changed_row_is_damaged_like_an_erased_line() {
    for (name, op) in OPS {
        let mut rect = feed(6, 6, b"abcdef\r\nghijkl\r\nmnopqr\x1b[2;1H");
        let mut el = feed(6, 6, b"abcdef\r\nghijkl\r\nmnopqr\x1b[2;1H");
        let (g1, g2) = (
            rect.grid().damage_generation(),
            el.grid().damage_generation(),
        );
        rect.advance(op);
        el.advance(b"\x1b[2K\x1b[3;1H\x1b[2K\x1b[2;1H");
        // DECCRA's destination is rows 2-3 too.
        assert_eq!(
            rect.grid().damage_since(g1),
            el.grid().damage_since(g2),
            "{name}"
        );
    }
}

/// Everything an operation must leave alone (B04, D8), compared
/// structurally.
#[derive(Debug, PartialEq)]
struct Untouched {
    cursor: Point,
    pending_wrap: bool,
    pen: Style,
    protection: &'static str,
    region: tessera_grid::ScrollRegion,
    margins: tessera_grid::Margins,
    origin: bool,
    alt: bool,
    tabs: tessera_grid::TabStops,
    history: Vec<tessera_grid::Line>,
    wrapped_and_marks: Vec<(bool, Vec<tessera_grid::Mark>)>,
    modes: tessera_term::Modes,
    level: u8,
    links: (usize, usize),
    counters: (u64, u64, u64, u64),
}

fn untouched(term: &Terminal) -> Untouched {
    let grid = term.grid();
    Untouched {
        cursor: grid.cursor(),
        pending_wrap: grid.pending_wrap(),
        pen: grid.pen(),
        protection: term.protection_mode(),
        region: grid.scroll_region(),
        margins: grid.margins(),
        origin: grid.origin_mode(),
        alt: grid.is_alt_screen(),
        tabs: grid.tabs().clone(),
        history: (0..grid.scrollback_len())
            .filter_map(|i| grid.scrollback_line(i).cloned())
            .collect(),
        wrapped_and_marks: grid
            .visible_lines()
            .iter()
            .map(|l| (l.wrapped, l.marks.clone()))
            .collect(),
        modes: term.modes(),
        level: term.conformance_level(),
        links: (grid.live_hyperlinks(), grid.hyperlink_bytes()),
        counters: (
            term.line_epoch(),
            term.screen_clears(),
            grid.scrollback_pushed(),
            grid.history_clear_generation(),
        ),
    }
}

/// B04: none of the four touches the cursor, pending wrap, pen, protection
/// mode, margins, modes, tab stops, scrollback, the lines' wrap flags and
/// marks, the link table or the counters; the saved cursor restores as
/// before; nothing is answered.
#[test]
fn b04_nothing_but_the_cells_changes() {
    // History, a soft-wrapped row, an OSC 133 mark, a custom tab stop, a
    // scroll region, margins, a protecting coloured pen, a saved cursor
    // and a pending wrap.
    let setup: &[u8] = b"h1\r\nh2\r\nh3\r\nh4\r\n\x1b]133;A\x1b\\abcdefghijkl\x1b[1;4H\x1bH\
        \x1b[1\"q\x1b[1;31;44m\x1b[2;5r\x1b[?69h\x1b[2;5s\x1b[3;3H\x1b7\x1b[6;1H\x1b[1;6Hxyz";
    for origin in [false, true] {
        for (name, op) in OPS {
            let mut term = feed(6, 6, setup);
            if origin {
                term.advance(b"\x1b[?6h\x1b[1;5Hxyzw");
            }
            let _ = term.take_requests();
            let before = untouched(&term);
            term.advance(op);
            assert!(
                term.take_requests().is_empty(),
                "{name} requested something"
            );
            assert_eq!(untouched(&term), before, "{name} origin {origin}");
            // The saved cursor: DECRC lands where it did without the op.
            let mut plain = feed(6, 6, setup);
            if origin {
                plain.advance(b"\x1b[?6h\x1b[1;5Hxyzw");
            }
            term.advance(b"\x1b8");
            plain.advance(b"\x1b8");
            assert_eq!(untouched(&term).cursor, untouched(&plain).cursor, "{name}");
            assert_eq!(term.grid().pen(), plain.grid().pen(), "{name}");
            term.grid().check_invariants();
        }
    }
}

/// B06: DA1 does not claim rectangular editing (attribute 28) until
/// DECCARA, DECRARA and DECRQCRA exist.
#[test]
fn b06_da1_is_pinned() {
    let mut term = feed(2, 10, b"\x1b[c");
    let da1 = replies(&mut term);
    assert_eq!(da1, "\x1b[?64;6;9;21;22c");
    assert!(!da1.contains(";28"));
}

/// B07: the operations add no state, so RIS after them leaves the terminal
/// like a new one, and DECSTR keeps the screen they made.
#[test]
fn b07_ris_and_decstr_need_no_new_state() {
    let ops: Vec<u8> = OPS.iter().flat_map(|(_, op)| op.iter().copied()).collect();
    let mut term = feed(5, 7, b"abcdefg\r\nhijklmn\r\nopqrstu\x1b[1\"q");
    term.advance(&ops);
    let screen = screen_text(&term);
    term.advance(b"\x1b[!p");
    assert_eq!(screen_text(&term), screen, "DECSTR keeps the screen");
    term.advance(&ops);
    term.advance(b"\x1bc");
    let fresh = Terminal::new(5, 7);
    assert_eq!(format!("{:?}", term.grid()), format!("{:?}", fresh.grid()));
}

/// S6: a flood of operations answers nothing and requests nothing: no
/// per-cell or per-operation event.
#[test]
fn a_flood_requests_nothing() {
    let ops: Vec<u8> = OPS.iter().flat_map(|(_, op)| op.iter().copied()).collect();
    let mut term = feed(24, 80, b"abc");
    let _ = term.take_requests();
    term.advance(&ops.repeat(20_000));
    assert_eq!(term.take_requests().len(), 0);
}

fn best_of_three(rows: u16, cols: u16, flood: &[u8]) -> std::time::Duration {
    (0..3)
        .map(|_| {
            let mut term = feed(rows, cols, b"abc");
            let start = std::time::Instant::now();
            term.advance(flood);
            start.elapsed()
        })
        .min()
        .expect("three runs")
}

/// The full-screen DECCRA and DECFRA on the largest grid (2048 columns x
/// 1024 rows) used by the floods below.
const LARGEST_OPS: [(&str, &[u8]); 2] = [
    ("DECCRA", b"\x1b[1;1;1024;2048;1;2;1;1$v"),
    ("DECFRA", b"\x1b[88;1;1;1024;2048$x"),
];

/// B02 (release only, where timing means something): 100 000 full-screen
/// DECCRA on 24x80, and 256 full-screen DECCRA and DECFRA on the largest
/// grid, cost a constant per cell: each stays within a small factor of
/// as many full-screen ED 2, best of three. (A 1 MiB flood of either is
/// about 45 000 full-screen operations, minutes of work, as 1 MiB of ED 2
/// is: `b02_one_mib_floods_on_the_largest_grid` measures it on request.)
#[test]
#[cfg_attr(debug_assertions, ignore = "timing: release only")]
fn b02_floods_cost_a_constant_per_cell() {
    use std::time::Duration;
    let n = 100_000;
    let copy = best_of_three(24, 80, &b"\x1b[1;1;24;80;1;2;1;1$v".repeat(n));
    let erase = best_of_three(24, 80, &b"\x1b[2J".repeat(n));
    eprintln!("100k DECCRA 24x80: {copy:?}, 100k ED 2: {erase:?}");
    assert!(
        copy < erase * 4 + Duration::from_millis(50),
        "{copy:?} vs {erase:?}"
    );
    for (name, op) in LARGEST_OPS {
        let took = best_of_three(1024, 2048, &op.repeat(256));
        let base = best_of_three(1024, 2048, &b"\x1b[2J".repeat(256));
        eprintln!("256 {name} on 2048x1024: {took:?}, 256 ED 2: {base:?}");
        assert!(
            took < base * 4 + Duration::from_millis(50),
            "{name} {took:?} vs {base:?}"
        );
    }
}

/// A 1024x2048 screen with a cluster (`e` + U+0301) or an OSC 8 linked
/// `x` in every cell.
fn filled_largest(kind: &str) -> Vec<u8> {
    let cells = 1024 * 2048;
    if kind == "clusters" {
        "e\u{301}".repeat(cells).into_bytes()
    } else {
        let mut bytes = b"\x1b]8;;https://a.example/\x1b\\".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', cells));
        bytes.extend_from_slice(b"\x1b]8;;\x1b\\");
        bytes
    }
}

/// The shortest of `runs` runs of `ops` on a new 1024x2048 terminal fed
/// `setup` (not timed).
fn timed_on(runs: usize, setup: &[u8], ops: &[u8]) -> std::time::Duration {
    (0..runs)
        .map(|_| {
            let mut term = feed(1024, 2048, setup);
            let start = std::time::Instant::now();
            term.advance(ops);
            start.elapsed()
        })
        .min()
        .expect("a run")
}

/// B02 with clusters and links (#251 round 1, B1): a DECCRA that copies
/// the left half of every row onto itself, on a screen of clusters or of
/// linked cells, so each destination row keeps entries right of the copy,
/// costs a constant per cell, like ED 2 on the same screen, one at a time
/// and as a flood of 256 (the screen stays full, so each costs the same):
/// within 10 times ED 2 (measured: 3 times over clusters, 6 over links,
/// whose copies count references; inserting each entry on its own was 19
/// and 40 times).
#[test]
#[cfg_attr(debug_assertions, ignore = "timing: release only")]
fn b02_partial_copies_over_clusters_and_links_cost_a_constant_per_cell() {
    use std::time::Duration;
    let copy = b"\x1b[1;1;1024;1024;1;1;1;1$v";
    for kind in ["clusters", "links"] {
        let setup = filled_largest(kind);
        let erase = timed_on(3, &setup, b"\x1b[2J");
        let one = timed_on(3, &setup, copy);
        let flood = timed_on(1, &setup, &copy.repeat(256));
        eprintln!("{kind}: DECCRA of the left half {one:?}, x256 {flood:?}; ED 2 {erase:?}");
        assert!(
            one < erase * 10 + Duration::from_millis(5),
            "{kind}: {one:?} vs ED 2 {erase:?}"
        );
        assert!(
            flood < erase * (10 * 256) + Duration::from_millis(50),
            "{kind}: x256 {flood:?} vs ED 2 {erase:?}"
        );
    }
}

/// The 1 MiB floods of B02 on the largest grid, once each, against 1 MiB
/// worth of operations of ED 2: run with `--ignored` in release for the
/// record (minutes).
#[test]
#[ignore = "minutes of work: run in release with --ignored for the record"]
fn b02_one_mib_floods_on_the_largest_grid() {
    for (name, op) in LARGEST_OPS {
        let reps = (1 << 20) / op.len();
        let start = std::time::Instant::now();
        feed(1024, 2048, &op.repeat(reps));
        let took = start.elapsed();
        let start = std::time::Instant::now();
        feed(1024, 2048, &b"\x1b[2J".repeat(reps));
        let base = start.elapsed();
        eprintln!("1 MiB of {name} ({reps}) on 2048x1024: {took:?}, {reps} ED 2: {base:?}");
        assert!(took < base * 4, "{name} {took:?} vs {base:?}");
    }
}

/// The reference model: each cell's character and protection, written
/// independently of the terminal's code, in `u32` so no edge saturates.
mod model {
    use proptest::prelude::*;
    use tessera_term::Terminal;

    pub const ROWS: u16 = 7;
    pub const COLS: u16 = 9;

    pub type Screen = Vec<Vec<(char, bool)>>;

    /// A parameter: absent, or a value from around the screen's edges and
    /// the saturated maximum.
    pub fn param() -> impl Strategy<Value = Option<u16>> {
        prop_oneof![
            Just(None),
            Just(Some(0)),
            (1u16..=COLS + 2).prop_map(Some),
            Just(Some(65535)),
        ]
    }

    /// `Pch`: the default, printable, Latin-1 and above, and refused ones.
    pub fn pch() -> impl Strategy<Value = Option<u16>> {
        prop_oneof![
            Just(None),
            Just(Some(0)),
            (32u16..=126).prop_map(Some),
            prop::sample::select(vec![
                1u16, 31, 127, 155, 159, 160, 200, 255, 0x100, 0x300, 0x4E00, 0xFFFE, 65535
            ])
            .prop_map(Some),
        ]
    }

    pub fn encode(params: &[Option<u16>]) -> String {
        params
            .iter()
            .map(|p| p.map_or_default(|n| n.to_string()))
            .collect::<Vec<_>>()
            .join(";")
    }

    /// The terminal's screen as the model sees it.
    pub fn read(term: &Terminal) -> Screen {
        term.grid()
            .visible_lines()
            .iter()
            .map(|line| {
                line.cells
                    .iter()
                    .map(|c| (c.ch, c.is_protected()))
                    .collect()
            })
            .collect()
    }

    /// Inclusive 0-based bounds positions are clamped to.
    #[derive(Clone, Copy, Debug)]
    pub struct Bounds {
        pub top: u32,
        pub bottom: u32,
        pub left: u32,
        pub right: u32,
    }

    fn at(params: &[Option<u16>], i: usize) -> u32 {
        params.get(i).copied().flatten().map_or(0, u32::from)
    }

    fn first(params: &[Option<u16>], i: usize, min: u32, max: u32) -> u32 {
        match at(params, i) {
            0 => min,
            n => (min + n - 1).min(max),
        }
    }

    fn last(params: &[Option<u16>], i: usize, min: u32, max: u32) -> u32 {
        match at(params, i) {
            0 => max,
            n => (min + n - 1).min(max),
        }
    }

    /// (top, left, bottom, right) of `params[from..]`, if not inverted.
    fn rect(b: Bounds, params: &[Option<u16>], from: usize) -> Option<(u32, u32, u32, u32)> {
        let r = (
            first(params, from, b.top, b.bottom),
            first(params, from + 1, b.left, b.right),
            last(params, from + 2, b.top, b.bottom),
            last(params, from + 3, b.left, b.right),
        );
        (r.0 <= r.2 && r.1 <= r.3).then_some(r)
    }

    fn cells(r: (u32, u32, u32, u32)) -> impl Iterator<Item = (u32, u32)> {
        (r.0..=r.2).flat_map(move |row| (r.1..=r.3).map(move |col| (row, col)))
    }

    fn cell(s: &mut Screen, (row, col): (u32, u32)) -> &mut (char, bool) {
        &mut s[row as usize][col as usize]
    }

    /// The fill character for `pch`, if DECFRA takes it (no character set
    /// is designated in the property, so it maps to itself).
    fn fill(pch: Option<u16>) -> Option<char> {
        match pch.unwrap_or(0) {
            0 => Some(' '),
            n @ (32..=126 | 160..=255 | 0x100) => char::from_u32(u32::from(n)),
            _ => None,
        }
    }

    /// The screen after `op` (`z`, `x`, `v` or `{`, DEC protection mode)
    /// with a protecting pen when `pen` is set.
    pub fn apply(mut s: Screen, b: Bounds, op: char, params: &[Option<u16>], pen: bool) -> Screen {
        match op {
            'z' => {
                for at in rect(b, params, 0).into_iter().flat_map(cells) {
                    *cell(&mut s, at) = (' ', false);
                }
            }
            'x' => {
                if let Some(ch) = fill(params.first().copied().flatten()) {
                    for at in rect(b, params, 1).into_iter().flat_map(cells) {
                        *cell(&mut s, at) = (ch, pen);
                    }
                }
            }
            '{' => {
                for at in rect(b, params, 0).into_iter().flat_map(cells) {
                    if !cell(&mut s, at).1 {
                        *cell(&mut s, at) = (' ', false);
                    }
                }
            }
            'v' => {
                if let Some(src) = rect(b, params, 0) {
                    let dt = first(params, 5, b.top, b.bottom);
                    let dl = first(params, 6, b.left, b.right);
                    let mut from = s.clone();
                    for (r, c) in cells(src) {
                        let to = (dt + r - src.0, dl + c - src.1);
                        if to.0 <= b.bottom && to.1 <= b.right {
                            *cell(&mut s, to) = *cell(&mut from, (r, c));
                        }
                    }
                }
            }
            _ => unreachable!(),
        }
        s
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

    /// All four operations, with any parameters, any margins, origin mode
    /// on or off and a protecting pen or not, match the reference model;
    /// the cursor and the scrollback stay, and the grid stays well formed.
    #[test]
    fn the_operations_match_the_reference(
        content in proptest::collection::vec((b'!'..=b'~', proptest::bool::ANY), usize::from(model::ROWS * model::COLS)),
        region in proptest::option::of((1u16..=model::ROWS, 1u16..=model::ROWS)),
        margins in proptest::option::of((1u16..=model::COLS, 1u16..=model::COLS)),
        origin in proptest::bool::ANY,
        pen in proptest::bool::ANY,
        op in proptest::sample::select(vec!['z', 'x', 'v', '{']),
        pch in model::pch(),
        params in proptest::collection::vec(model::param(), 0..=9),
    ) {
        use model::{COLS, ROWS};
        let mut term = Terminal::new(ROWS, COLS);
        let mut setup = Vec::new();
        for (i, &(ch, protected)) in content.iter().enumerate() {
            let (row, col) = (i / usize::from(COLS), i % usize::from(COLS));
            setup.extend(format!("\x1b[{};{}H\x1b[{}\"q", row + 1, col + 1, u8::from(protected)).bytes());
            setup.push(ch);
        }
        let mut bounds = model::Bounds { top: 0, bottom: u32::from(ROWS) - 1, left: 0, right: u32::from(COLS) - 1 };
        let mut region_set = (0, u32::from(ROWS) - 1);
        let mut margins_set = (0, u32::from(COLS) - 1);
        if let Some((t, b)) = region.filter(|(t, b)| t < b) {
            setup.extend(format!("\x1b[{t};{b}r").bytes());
            region_set = (u32::from(t) - 1, u32::from(b) - 1);
        }
        if let Some((l, r)) = margins.filter(|(l, r)| l < r) {
            setup.extend(format!("\x1b[?69h\x1b[{l};{r}s").bytes());
            margins_set = (u32::from(l) - 1, u32::from(r) - 1);
        }
        if origin {
            setup.extend(b"\x1b[?6h");
            bounds = model::Bounds { top: region_set.0, bottom: region_set.1, left: margins_set.0, right: margins_set.1 };
        }
        setup.extend(format!("\x1b[{}\"q", u8::from(pen)).bytes());
        term.advance(&setup);
        let before = model::read(&term);
        let cursor = term.grid().cursor();
        let all: Vec<Option<u16>> = if op == 'x' {
            std::iter::once(pch).chain(params.iter().copied()).collect()
        } else {
            params.clone()
        };
        let seq = format!("\x1b[{}${op}", model::encode(&all));
        term.advance(seq.as_bytes());
        let want = model::apply(before, bounds, op, &all, pen);
        proptest::prop_assert_eq!(model::read(&term), want, "{:?} bounds {:?}", seq, bounds);
        proptest::prop_assert_eq!(term.grid().cursor(), cursor);
        proptest::prop_assert_eq!(term.grid().scrollback_len(), 0);
        term.grid().check_invariants();
    }
}

/// On the alternate screen the operations change that screen only, and
/// the primary screen comes back as it was.
#[test]
fn the_alternate_screen_takes_the_operations_alone() {
    for (name, op) in OPS {
        let mut term = feed(4, 6, b"abcdef\r\nghijkl\r\nmnopqr\x1b[?1049h");
        term.advance(b"ABCDEF\r\nGHIJKL\r\nMNOPQR");
        let alt = screen_text(&term);
        term.advance(op);
        assert_ne!(
            screen_text(&term),
            alt,
            "{name} did nothing on the alternate screen"
        );
        term.advance(b"\x1b[?1049l");
        assert_eq!(
            screen_text(&term)[..3],
            ["abcdef", "ghijkl", "mnopqr"],
            "{name}"
        );
        term.grid().check_invariants();
    }
}

/// B05 at level 63: the ignored operations damage nothing but the
/// cursor's row, which `damage_since` always reports.
#[test]
fn b05_below_level_4_nothing_is_damaged() {
    for (name, op) in OPS {
        let mut term = feed(6, 6, b"\x1b[63;1\"pabcdef\r\nghijkl\r\nmnopqr\x1b[5;1H");
        let generation = term.grid().damage_generation();
        term.advance(op);
        assert_eq!(
            term.grid().damage_since(generation),
            DamageRows::Rows(4..=4),
            "{name}"
        );
    }
}

/// m1: a wide character cut by DECCRA's source right edge arrives as a
/// blank, never as a lone wide half.
#[test]
fn deccra_blanks_a_wide_character_cut_by_the_source_right_edge() {
    let term = feed(2, 10, "ab中cd\x1b[1;1;1;3;1;1;7;1$v".as_bytes());
    let line = term.grid().line(0).expect("row");
    assert_eq!(line.text(), "ab中cdab");
    assert!(!line.cells[8].is_wide() && line.cells[8].ch == ' ');
    term.grid().check_invariants();
}

/// m2: DECSERA over the left half of a wide character erases it whole.
#[test]
fn decsera_erases_a_wide_character_cut_by_its_right_edge() {
    let term = feed(2, 10, "ab中x\x1b[1;1;1;3${".as_bytes());
    let line = term.grid().line(0).expect("row");
    assert_eq!(line.text(), "    x");
    assert!(
        line.cells[..4]
            .iter()
            .all(|c| c.ch == ' ' && !c.is_wide() && !c.is_spacer())
    );
    term.grid().check_invariants();
}

/// m3: DECCRA copies a soft-wrap pad cell (the last column a wrapped wide
/// character left empty) as a plain blank: the pad belongs to its row's
/// last column only.
#[test]
fn deccra_does_not_copy_the_pad_flag() {
    let term = feed(4, 6, "abcde中\x1b[1;6;1;6;1;3;1;1$v".as_bytes());
    assert!(
        term.grid().line(0).expect("row").cells[5].is_pad(),
        "the setup pads column 6"
    );
    let copied = &term.grid().line(2).expect("row").cells[0];
    assert!(!copied.is_pad(), "{copied:?}");
    term.grid().check_invariants();
}

/// B1: DECCRA merges the copied clusters and links into a destination row
/// that has entries on both sides of the copy, in order, keeping the
/// others.
#[test]
fn deccra_merges_clusters_and_links_into_a_full_row() {
    let row = "\x1b]8;;https://l.example/\x1b\\e\u{301}x\x1b]8;;\x1b\\o\u{308}y\x1b]8;;https://r.example/\x1b\\u\u{303}z";
    let mut term = feed(3, 6, format!("{row}\r\n{row}").as_bytes());
    // Row 1 columns 1-2 (the left link's cluster and x) onto row 2 columns 3-4.
    term.advance(b"\x1b[1;1;1;2;1;2;3;1$v");
    let line = term.grid().line(1).expect("row");
    assert_eq!(line.text(), "e\u{301}xe\u{301}xu\u{303}z");
    let uris: Vec<Option<&str>> = (0..6)
        .map(|c| line.hyperlink(c).map(tessera_grid::Hyperlink::uri))
        .collect();
    let (l, r) = (Some("https://l.example/"), Some("https://r.example/"));
    assert_eq!(uris, [l, l, l, l, r, r]);
    let clusters: Vec<(usize, &str)> = line.graphemes.iter().collect();
    assert_eq!(
        clusters,
        [(0, "e\u{301}"), (2, "e\u{301}"), (4, "u\u{303}")]
    );
    term.grid().check_invariants();
}
