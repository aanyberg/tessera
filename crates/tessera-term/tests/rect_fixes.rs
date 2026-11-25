//! Two fixes to the rectangle operations (ADR 0046) from the wave 20
//! review (#430):
//!
//! - DECFRA maps `Pch` through the character sets before it decides
//!   whether it can fill with the result, as printing the same character
//!   would (20.1 adversarial M1: `Pch` 173, U+00AD, was refused under DEC
//!   Supplemental and DEC Technical although printing it shows a glyph);
//! - an exact DECCRA self copy (source and destination at the same place)
//!   changes nothing: no cell, no damage, no work per cell (20.1
//!   adversarial M2: 127 ms on a 1024x2048 screen of clusters). That it
//!   allocates nothing is checked in `rect_alloc.rs`, the binary with the
//!   counting allocator.
//!
//! And from #489: the DECFRA charset fixture
//! (`fixtures/decfra_charset_spec.txt`, xterm's `Pch` handling through GL,
//! GR and the single shifts, every row whole and byte by byte) and the
//! differential against printing widened from G1 to every slot G0-G3,
//! every invocation (GL, GR, SS2, SS3), DECNRCM and levels 4 and 5:
//! exhaustively per slot and invocation, and over at least ten million
//! random charset states in a release run.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;
#[allow(dead_code)] // Shared with tests/csi.rs.
mod spec;

use std::time::Duration;

use common::Chunking;
use tessera_grid::{DamageRows, Point};
use tessera_term::Terminal;

/// The G1 designations DECFRA is compared with printing under: every set
/// of the designation table, the 94-character ones with `ESC )` and ISO
/// Latin-1 with `ESC - A`. Each runs with DECNRCM off and on (the national
/// sets need it).
const G1_SETS: &[&[u8]] = &[
    b"\x1b)B", b"\x1b)A", b"\x1b)0", b"\x1b)<", b"\x1b)%5", b"\x1b)>", b"\x1b-A", b"\x1b)4",
    b"\x1b)5", b"\x1b)C", b"\x1b)R", b"\x1b)f", b"\x1b)Q", b"\x1b)K", b"\x1b)Y", b"\x1b)E",
    b"\x1b)6", b"\x1b)Z", b"\x1b)7", b"\x1b)H", b"\x1b)=", b"\x1b)9", b"\x1b)`", b"\x1b)%6",
];

/// How G1 is invoked: into GR (LS1R, `ESC ~`) or into GL (SO).
const INVOCATIONS: [(&str, &[u8]); 2] = [("GR", b"\x1b~"), ("GL", b"\x0e")];

/// What a DECFRA that is ignored leaves in the cell: no set maps a byte
/// to it.
const SENTINEL: char = '\u{0416}';

/// A 3x10 terminal at level 4 (the default) with DECNRCM `nrcs`, G1 designated by
/// `designate` and invoked by `invoke`, the cursor home; `first` printed
/// at the home position before the sets change.
fn setup(nrcs: bool, designate: &[u8], invoke: &[u8], first: &str) -> Terminal {
    let mut term = Terminal::new(3, 10);
    term.advance(first.as_bytes());
    term.advance(if nrcs { b"\x1b[?42h" } else { b"\x1b[?42l" });
    term.advance(designate);
    term.advance(invoke);
    term.advance(b"\x1b[H");
    term
}

/// What printing `ch` at home leaves there: the character, when it took
/// exactly the home cell; `None` when it printed nothing there (dropped,
/// zero width, combining) or something wider.
fn printed(term: &Terminal) -> Option<char> {
    let grid = term.grid();
    let line = &grid.visible_lines()[0];
    let cell = &line.cells[0];
    let one_cell = grid.cursor() == Point::new(0, 1) && !cell.is_wide() && !cell.is_cluster();
    one_cell.then_some(cell.ch)
}

/// #430 20.1 M1: DECFRA with `Pch` 173 after `ESC ) % 5` (DEC
/// Supplemental into G1) and `ESC ~` (G1 into GR) fills with what printing
/// U+00AD shows under the same sets, not nothing.
#[test]
fn decfra_maps_pch_173_before_checking_it() {
    let mut print = setup(false, b"\x1b)%5", b"\x1b~", "");
    print.advance("\u{ad}".as_bytes());
    let shown = printed(&print).expect("printing U+00AD under DEC Supplemental shows a glyph");
    let mut fill = setup(false, b"\x1b)%5", b"\x1b~", &SENTINEL.to_string());
    fill.advance(b"\x1b[173;1;1;1;1$x");
    let filled = fill.grid().visible_lines()[0].cells[0].ch;
    assert_eq!(filled, shown, "DECFRA 173 fills like printing U+00AD");
}

/// #430 20.1 M1, exhaustively: for every `Pch` 32-126 and 160-255, under
/// every G1 set invoked into GR and into GL, with DECNRCM off and on,
/// DECFRA fills the cell with what printing the character shows there,
/// and does nothing exactly when printing shows nothing one cell wide.
#[test]
fn decfra_fills_with_what_printing_shows_for_every_pch_and_set() {
    let mut mismatches = Vec::new();
    let mut compared = 0;
    for designate in G1_SETS {
        for (how, invoke) in INVOCATIONS {
            for nrcs in [false, true] {
                for pch in (32..=126).chain(160..=255) {
                    let ch = char::from_u32(pch).expect("Latin-1");
                    let mut print = setup(nrcs, designate, invoke, "");
                    print.advance(ch.to_string().as_bytes());
                    let want = printed(&print);
                    let mut fill = setup(nrcs, designate, invoke, &SENTINEL.to_string());
                    fill.advance(format!("\x1b[{pch};1;1;1;1$x").as_bytes());
                    let cell = fill.grid().visible_lines()[0].cells[0].ch;
                    let got = (cell != SENTINEL).then_some(cell);
                    compared += 1;
                    if got != want {
                        mismatches.push(format!(
                            "{:?} {how} nrcs={nrcs} Pch {pch}: DECFRA {got:?}, print {want:?}",
                            String::from_utf8_lossy(designate)
                        ));
                    }
                }
            }
        }
    }
    assert_eq!(compared, G1_SETS.len() * 2 * 2 * (95 + 96));
    assert!(
        mismatches.is_empty(),
        "{} of {compared} differ:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// A screen with a bit of everything a copy carries or could break: wide
/// characters cut by the rectangles below, clusters, an OSC 8 link,
/// protected and coloured cells and a soft-wrapped row whose last column
/// is a wide character's pad.
fn busy_screen() -> Terminal {
    let mut term = Terminal::new(8, 12);
    term.advance(
        "ab語cd\u{1f600}ef\r\n\
         e\u{301}x\u{301}\x1b]8;;https://a.example/\x1b\\link\x1b]8;;\x1b\\\x1b[1\"qPR\x1b[0\"q\r\n\
         \x1b[31;44mred\x1b[m 語語語\r\n\
         abcdefghijk語z"
            .as_bytes(),
    );
    term.advance(b"\x1b[8;12H");
    term
}

/// #430 20.1 M2: a DECCRA whose destination is its source's top-left
/// corner, whatever the source, the origin mode and the page parameters,
/// changes no cell and damages no row but the cursor's own, which
/// `damage_since` always reports.
#[test]
fn an_exact_self_copy_changes_nothing() {
    // (setup, copy): the setup's own damage is not the copy's.
    let copies: [(&[u8], &[u8]); 7] = [
        // Every parameter defaulted: the whole screen onto itself.
        (b"", b"\x1b[$v"),
        (b"", b"\x1b[1;1;7;12;1;1;1;1$v"),
        // Cutting the wide characters of rows 1, 3 and 4 at both edges.
        (b"", b"\x1b[1;4;4;9;1;1;4;1$v"),
        (b"", b"\x1b[3;6;4;7;1;3;6;1$v"),
        // Page parameters are ignored.
        (b"", b"\x1b[2;2;3;5;3;2;2;9$v"),
        // Clipped: the source's bottom-right is past the screen.
        (b"", b"\x1b[5;5;99;99;1;5;5;1$v"),
        // In origin mode, relative to the margins (rows 2-7, columns 2-10).
        (
            b"\x1b[2;7r\x1b[?69h\x1b[2;10s\x1b[?6h",
            b"\x1b[1;1;3;4;1;1;1;1$v",
        ),
    ];
    for (setup, op) in copies {
        let mut term = busy_screen();
        term.advance(setup);
        let before = format!("{:?}", term.grid().visible_lines());
        let generation = term.grid().damage_generation();
        term.advance(op);
        let name = String::from_utf8_lossy(op);
        assert_eq!(
            format!("{:?}", term.grid().visible_lines()),
            before,
            "{name}: a cell changed"
        );
        let row = term.grid().cursor().row;
        assert_eq!(
            term.grid().damage_since(generation),
            DamageRows::Rows(row..=row),
            "{name}: damaged more than the cursor's row"
        );
    }
}

/// A copy one column off its source is still a copy: it moves the cells
/// and damages its destination rows (so the self-copy exit above is exact).
#[test]
fn a_copy_one_column_off_still_copies() {
    let mut term = busy_screen();
    let generation = term.grid().damage_generation();
    term.advance(b"\x1b[1;1;2;4;1;1;2;1$v");
    let first: String = term.grid().visible_lines()[0]
        .cells
        .iter()
        .take(5)
        .map(|c| c.ch)
        .collect();
    assert_eq!(
        first, "aab語 ",
        "columns 1-4 of row 1 land one column right"
    );
    assert_eq!(
        term.grid().damage_since(generation),
        DamageRows::Rows(0..=7),
        "rows 0 and 1 and the cursor's"
    );
}

/// A 1024x2048 terminal with a cluster (`e` + U+0301) in every cell.
fn all_clusters() -> Terminal {
    let mut term = Terminal::new(1024, 2048);
    term.advance("e\u{301}".repeat(1024 * 2048).as_bytes());
    term
}

/// The least CPU time of the calling thread over `runs` runs of `op`, each
/// on a new [`all_clusters`] screen (not measured).
///
/// CPU time, not wall time (#678): time the thread spends preempted does
/// not count, so a loaded machine cannot fail the bars below the way it
/// failed the shifted copy's wall-clock bar at a 1-minute load of 60-70.
fn best_of(runs: usize, op: &[u8]) -> Duration {
    (0..runs)
        .map(|_| {
            let mut term = all_clusters();
            let start = common::thread_cpu_time();
            term.advance(op);
            common::thread_cpu_time().saturating_sub(start)
        })
        .min()
        .expect("a run")
}

/// #430 20.1 M2's bar (release only, where cost means something): the
/// full-screen self copy on a 1024x2048 screen of clusters takes at most
/// twice a DECALN on the same screen, best of five (it took 127 ms, every
/// cluster's text copied). DECALN drops every cluster, so that bar alone
/// passes the full copy too (50 ms against 540 ms); the second bar, at
/// most ten one-cell DECERAs plus a millisecond, is the one the copy
/// fails (the no-op takes microseconds). Both are on the thread's CPU time
/// ([`best_of`]).
#[test]
#[cfg_attr(debug_assertions, ignore = "cost bar: release only")]
fn a_self_copy_of_a_screen_of_clusters_costs_no_more_than_decaln() {
    let copy = best_of(5, b"\x1b[1;1;1024;2048;1;1;1;1$v");
    let decaln = best_of(5, b"\x1b#8");
    let one_cell = best_of(5, b"\x1b[1;1;1;1$z");
    eprintln!(
        "self copy of 1024x2048 clusters: {copy:?}, DECALN: {decaln:?}, one-cell DECERA: {one_cell:?} \
         (thread CPU time)"
    );
    assert!(copy <= decaln * 2, "{copy:?} vs DECALN {decaln:?}");
    assert!(
        copy <= one_cell * 10 + Duration::from_millis(1),
        "{copy:?} vs one-cell DECERA {one_cell:?}: the self copy did work per cell"
    );
}

/// Interleaved rounds of the shifted copy's bar, each three runs of ED 2
/// and three of the copy.
const SHIFTED_COPY_ROUNDS: usize = 3;

/// The partial copy `tests/rect.rs` B02 times over clusters and links
/// (also one column to the right since the self copy became a no-op), here
/// over the all-cluster screen of the self-copy bar: each
/// destination row keeps entries left and right of the copy and the cost
/// stays a constant per cell, under 10 times ED 2 on the same screen.
///
/// The ratio is of CPU times ([`best_of`]) taken in
/// [`SHIFTED_COPY_ROUNDS`] interleaved rounds: the lowest copy over all
/// nine runs divided by the lowest ED 2 over all nine. Load still moves
/// the ratio (lower clocks with every core busy, an efficiency core): the
/// copy reads 2.0 to 3.6 times ED 2, but not past the bar for a constant
/// cost. 2026-11-25, Mac16,10: the copy is about 3.3 times ED 2 (43 ms
/// against 13 ms); one more pass over the row per copied cell is about 23
/// times quiet and 12 times under load, and setting the copied clusters
/// one by one in front of the row's own entries 21 to 31 times.
#[test]
#[cfg_attr(debug_assertions, ignore = "cost bar: release only")]
fn a_shifted_partial_copy_over_clusters_costs_a_constant_per_cell() {
    let rounds: Vec<(Duration, Duration)> = (0..SHIFTED_COPY_ROUNDS)
        .map(|_| {
            let erase = best_of(3, b"\x1b[2J");
            let copy = best_of(3, b"\x1b[1;1;1024;1024;1;1;2;1$v");
            (copy, erase)
        })
        .collect();
    let copy = rounds.iter().map(|&(copy, _)| copy).min().expect("a round");
    let erase = rounds
        .iter()
        .map(|&(_, erase)| erase)
        .min()
        .expect("a round");
    let ratio = copy.as_secs_f64() / erase.as_secs_f64().max(f64::MIN_POSITIVE);
    eprintln!(
        "left half one column right over clusters vs ED 2 (thread CPU time): {rounds:?}, \
         lowest copy {copy:?} over lowest ED 2 {erase:?}: {ratio:.2}"
    );
    assert!(
        ratio < 10.0,
        "the shifted copy cost {ratio:.2} times ED 2 at best (copy, ED 2: {rounds:?})"
    );
}

const DECFRA_FIXTURE: &str = include_str!("fixtures/decfra_charset_spec.txt");
const DECFRA_FIXTURE_NAME: &str = "fixtures/decfra_charset_spec.txt";

/// `###` rows in the DECFRA fixture, so a row lost from it cannot pass
/// silently.
const DECFRA_ROWS: usize = 48;

/// The `cells:` of fixture row `id`, the code points its first row's
/// first cells must hold (for rows whose fill is invisible in a render:
/// a no-break space trims like a blank).
fn fixture_cells(id: &str) -> Option<Vec<char>> {
    let heading = format!("### {id}");
    let mut lines = DECFRA_FIXTURE.lines().skip_while(|line| *line != heading);
    lines.next()?;
    let cells = lines
        .take_while(|line| !line.starts_with("### ") && !line.starts_with("# --"))
        .find_map(|line| line.strip_prefix("cells: "))?;
    Some(
        cells
            .split_whitespace()
            .map(|code| {
                let hex = code.strip_prefix("U+").expect("cells are U+XXXX");
                char::from_u32(u32::from_str_radix(hex, 16).expect("hex code point"))
                    .expect("a scalar")
            })
            .collect(),
    )
}

/// #489, the first test: every row of the spec researcher's DECFRA fixture
/// (xterm's `Pch` acceptance, GL, GR, the single shift and DECCRA's
/// overlap and self copy, with the lead's rulings on the four places
/// Tessera and xterm could differ), whole and byte by byte.
#[test]
fn decfra_charset_spec_rows() {
    let cases = spec::parse_fixture(DECFRA_FIXTURE);
    let headings = DECFRA_FIXTURE
        .lines()
        .filter(|line| line.starts_with("### "))
        .count();
    assert_eq!(headings, DECFRA_ROWS, "rows in the fixture");
    common::check_table(
        DECFRA_FIXTURE_NAME,
        headings,
        cases.iter().map(|case| (case.id.clone(), case)),
        |case| {
            let mut errors: Vec<String> = [Chunking::Whole, Chunking::Bytewise]
                .into_iter()
                .flat_map(|chunking| spec::check_case(case, chunking))
                .collect();
            if let Some(cells) = fixture_cells(&case.id) {
                let (term, _) = case.script.run(Chunking::Whole);
                let got: Vec<char> = term.grid().visible_lines()[0].cells[..cells.len()]
                    .iter()
                    .map(|cell| cell.ch)
                    .collect();
                if got != cells {
                    errors.push(format!(
                        "{} cells: expected {cells:?}, got {got:?}",
                        case.id
                    ));
                }
            }
            assert!(errors.is_empty(), "{}", errors.join("\n"));
        },
    );
}

/// The 94-character designations after `ESC (`, `ESC )`, `ESC *` or
/// `ESC +`: every final and prefix the designation table knows, the ones
/// that fall back to ASCII (`1`, `2`, the `"` and `&` prefixes, the `%`
/// finals Tessera does not implement) included.
const SETS_94: &[&str] = &[
    "B", "A", "0", "<", "%5", ">", "4", "5", "C", "R", "f", "Q", "K", "Y", "E", "6", "Z", "7", "H",
    "=", "9", "`", "%6", "1", "2", "\"4", "\">", "\"?", "&4", "&5", "%0", "%2", "%=", "%3",
];

/// The 96-character designations after `ESC -`, `ESC .` or `ESC /` (G1 to
/// G3 only): ISO Latin-1 and the ones that fall back to ASCII.
const SETS_96: &[&str] = &["A", "B", "F", "H", "L", "M"];

/// A charset designation: G`slot` := `set`, a 96-character one when
/// `is_96` (never G0).
#[derive(Clone, Copy, Debug)]
struct Designation {
    slot: usize,
    set: &'static str,
    is_96: bool,
}

impl Designation {
    fn bytes(self) -> Vec<u8> {
        let intermediate = if self.is_96 {
            b"?-./"[self.slot]
        } else {
            b"()*+"[self.slot]
        };
        let mut bytes = vec![0x1b, intermediate];
        bytes.extend_from_slice(self.set.as_bytes());
        bytes
    }
}

/// How a `Pch` below 128 reaches its set: G`n` invoked into GL (SI, SO,
/// LS2, LS3), or a pending single shift of G2 or G3 (SS2, SS3) on top.
const GL: [&[u8]; 4] = [b"\x0f", b"\x0e", b"\x1bn", b"\x1bo"];
/// G1 to G3 invoked into GR (LS1R, LS2R, LS3R), index 1 to 3.
const GR: [&[u8]; 4] = [b"", b"\x1b~", b"\x1b}", b"\x1b|"];
/// SS2 and SS3, index 2 and 3.
const SS: [&[u8]; 4] = [b"", b"", b"\x1bN", b"\x1bO"];

/// One charset state: DECNRCM, the four designations, GL, GR and an
/// optional single shift.
#[derive(Clone, Debug)]
struct CharsetState {
    nrcs: bool,
    sets: [Designation; 4],
    gl: usize,
    gr: usize,
    single_shift: Option<usize>,
}

impl CharsetState {
    /// The bytes that put a terminal in this state, whatever state it was
    /// in: every slot is designated and both invocations are set.
    fn bytes(&self) -> Vec<u8> {
        let mut bytes = if self.nrcs {
            b"\x1b[?42h".to_vec()
        } else {
            b"\x1b[?42l".to_vec()
        };
        for designation in self.sets {
            bytes.extend(designation.bytes());
        }
        bytes.extend_from_slice(GL[self.gl]);
        bytes.extend_from_slice(GR[self.gr]);
        if let Some(g) = self.single_shift {
            bytes.extend_from_slice(SS[g]);
        }
        bytes
    }
}

/// A terminal that compares DECFRA with printing, case after case: each
/// case sets the whole charset state again, fills the home cell with
/// [`SENTINEL`] (a `Pch` above 255, which no set maps), then DECFRA's
/// `Pch` there, and then prints the same character at home, which
/// consumes a single shift DECFRA left pending.
struct Probe {
    term: Terminal,
    level: u8,
}

impl Probe {
    /// A 3x10 probe at conformance level `level` (4 or 5).
    fn new(level: u8) -> Self {
        let mut term = Terminal::new(3, 10);
        term.advance(format!("\x1b[6{level};1\"p").as_bytes());
        assert_eq!(term.conformance_level(), level);
        Self { term, level }
    }

    /// DECFRA's fill and what printing showed for `pch` under `state`, as
    /// [`printed`] reads them; `Err` describes a mismatch.
    fn compare(&mut self, state: &CharsetState, pch: u32) -> Result<(), String> {
        let mut bytes = state.bytes();
        bytes.extend_from_slice(
            format!(
                "\x1b[H\x1b[{};1;1;1;1$x\x1b[{pch};1;1;1;1$x",
                u32::from(SENTINEL)
            )
            .as_bytes(),
        );
        self.term.advance(&bytes);
        let cell = self.term.grid().visible_lines()[0].cells[0].ch;
        let filled = (cell != SENTINEL).then_some(cell);
        let ch = char::from_u32(pch).expect("Latin-1");
        self.term.advance(format!("\x1b[H{ch}").as_bytes());
        let shown = printed(&self.term);
        if filled.is_some_and(char::is_control) {
            return Err(format!(
                "level {} {state:?} Pch {pch}: DECFRA wrote a control {filled:?}",
                self.level
            ));
        }
        if filled == shown {
            Ok(())
        } else {
            Err(format!(
                "level {} {state:?} Pch {pch}: DECFRA {filled:?}, print {shown:?}",
                self.level
            ))
        }
    }
}

/// Every `Pch` DECFRA maps: 32-126 and 160-255.
fn latin1_pch() -> impl Iterator<Item = u32> + Clone {
    (32..=126).chain(160..=255)
}

/// Every designation into G`slot`.
fn designations(slot: usize) -> impl Iterator<Item = Designation> {
    let sets_96 = if slot == 0 { &[][..] } else { SETS_96 };
    SETS_94
        .iter()
        .map(move |&set| Designation {
            slot,
            set,
            is_96: false,
        })
        .chain(sets_96.iter().map(move |&set| Designation {
            slot,
            set,
            is_96: true,
        }))
}

/// A 94-character set other than `set`, for the slots under test not to
/// hold: DEC Special Graphics, or DEC Technical in place of it.
fn decoy(slot: usize, set: &str) -> Designation {
    Designation {
        slot,
        set: if set == "0" { ">" } else { "0" },
        is_96: false,
    }
}

/// #489, the widened differential, exhaustive per slot and invocation:
/// every designation into each of G0-G3, the other three slots holding a
/// different set, reached through GL, through GR (G1-G3) and through a
/// pending SS2 or SS3 (G2, G3), with DECNRCM off and on, at levels 4 and
/// 5, for every `Pch` 32-126 and 160-255: DECFRA fills with what printing
/// the same character shows (a pending single shift included, which
/// DECFRA leaves for the print), nothing exactly where printing shows
/// nothing one cell wide, and never a control character (S3).
#[test]
fn decfra_fills_like_printing_through_every_slot_and_invocation() {
    let mut mismatches = Vec::new();
    let mut compared = 0usize;
    for level in [4, 5] {
        let mut probe = Probe::new(level);
        for slot in 0..4 {
            for designation in designations(slot) {
                let mut sets: [Designation; 4] =
                    std::array::from_fn(|other| decoy(other, designation.set));
                sets[slot] = designation;
                // (GL, GR, single shift): the slot in GL, in GR, or shifted
                // to over a GL of G0 (another set).
                let mut ways = vec![(slot, if slot == 1 { 2 } else { 1 }, None)];
                if slot >= 1 {
                    ways.push((0, slot, None));
                }
                if slot >= 2 {
                    ways.push((0, 1, Some(slot)));
                }
                for (gl, gr, single_shift) in ways {
                    for nrcs in [false, true] {
                        let state = CharsetState {
                            nrcs,
                            sets,
                            gl,
                            gr,
                            single_shift,
                        };
                        for pch in latin1_pch() {
                            compared += 1;
                            if let Err(mismatch) = probe.compare(&state, pch) {
                                mismatches.push(mismatch);
                            }
                        }
                    }
                }
            }
        }
    }
    // Per level: G0 (34 sets) one way, G1 (40) two, G2 and G3 (40) three.
    assert_eq!(compared, 2 * (34 + 40 * 2 + 40 * 3 * 2) * 2 * 191);
    let shown: Vec<&str> = mismatches.iter().take(20).map(String::as_str).collect();
    assert!(
        mismatches.is_empty(),
        "{} of {compared} differ, the first:\n{}",
        mismatches.len(),
        shown.join("\n")
    );
}

/// `SplitMix64`: a seeded generator, so a failing case can be rerun.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).expect("below n")
    }
}

/// A random charset state: each slot a random designation (94 or 96
/// characters), GL any slot, GR G1-G3, and a single shift a third of the
/// time each way.
fn random_state(rng: &mut SplitMix) -> CharsetState {
    let sets = std::array::from_fn(|slot| {
        let choices: Vec<Designation> = designations(slot).collect();
        choices[rng.below(choices.len())]
    });
    CharsetState {
        nrcs: rng.below(2) == 1,
        sets,
        gl: rng.below(4),
        gr: 1 + rng.below(3),
        single_shift: [None, Some(2), Some(3)][rng.below(3)],
    }
}

/// Random cases of the differential: ten million in a release build (the
/// #489 bar; a release run takes seconds), fewer in a debug one, where
/// each case costs about twenty times as much and the exhaustive test
/// above covers every slot and invocation anyway.
const RANDOM_CASES: usize = if cfg!(debug_assertions) {
    200_000
} else {
    10_000_000
};

/// #489, the randomised differential: [`RANDOM_CASES`] random (charset
/// state, `Pch`) cases, all four slots designated at once and any mix of
/// GL, GR and a single shift, at level 4 or 5, seeded so a failure
/// reruns: DECFRA fills exactly what printing shows, as in the exhaustive
/// test.
#[test]
fn decfra_fills_like_printing_for_random_charset_states() {
    const SEED: u64 = 0x0004_89DE_CF2A;
    let mut rng = SplitMix(SEED);
    let mut probes = [Probe::new(4), Probe::new(5)];
    let pch: Vec<u32> = latin1_pch().collect();
    let mut mismatches = Vec::new();
    for _ in 0..RANDOM_CASES {
        let state = random_state(&mut rng);
        let probe = &mut probes[rng.below(2)];
        let pch = pch[rng.below(pch.len())];
        if let Err(mismatch) = probe.compare(&state, pch) {
            mismatches.push(mismatch);
        }
    }
    let shown: Vec<&str> = mismatches.iter().take(20).map(String::as_str).collect();
    assert!(
        mismatches.is_empty(),
        "seed {SEED:#x}: {} of {RANDOM_CASES} differ, the first:\n{}",
        mismatches.len(),
        shown.join("\n")
    );
}
