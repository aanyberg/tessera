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

/// A 3x10 terminal at level 5 with DECNRCM `nrcs`, G1 designated by
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

/// The shortest of `runs` runs of `op` on [`all_clusters`] screens.
fn best_of(runs: usize, op: &[u8]) -> std::time::Duration {
    (0..runs)
        .map(|_| {
            let mut term = all_clusters();
            let start = std::time::Instant::now();
            term.advance(op);
            start.elapsed()
        })
        .min()
        .expect("a run")
}

/// #430 20.1 M2's bar (release only, where timing means something): the
/// full-screen self copy on a 1024x2048 screen of clusters takes at most
/// twice a DECALN on the same screen, best of five (it took 127 ms, every
/// cluster's text copied). DECALN drops every cluster, so that bar alone
/// passes the full copy too (50 ms against 540 ms); the second bar, at
/// most ten one-cell DECERAs plus a millisecond, is the one the copy
/// fails (the no-op takes microseconds).
#[test]
#[cfg_attr(debug_assertions, ignore = "timing: release only")]
fn a_self_copy_of_a_screen_of_clusters_costs_no_more_than_decaln() {
    let copy = best_of(5, b"\x1b[1;1;1024;2048;1;1;1;1$v");
    let decaln = best_of(5, b"\x1b#8");
    let one_cell = best_of(5, b"\x1b[1;1;1;1$z");
    eprintln!(
        "self copy of 1024x2048 clusters: {copy:?}, DECALN: {decaln:?}, one-cell DECERA: {one_cell:?}"
    );
    assert!(copy <= decaln * 2, "{copy:?} vs DECALN {decaln:?}");
    assert!(
        copy <= one_cell * 10 + std::time::Duration::from_millis(1),
        "{copy:?} vs one-cell DECERA {one_cell:?}: the self copy did work per cell"
    );
}

/// The partial copy `tests/rect.rs` B02 times over clusters and links
/// (also one column to the right since the self copy became a no-op), here
/// over the all-cluster screen of the self-copy bar: each
/// destination row keeps entries left and right of the copy and the cost
/// stays a constant per cell, within 10 times ED 2 on the same screen.
#[test]
#[cfg_attr(debug_assertions, ignore = "timing: release only")]
fn a_shifted_partial_copy_over_clusters_costs_a_constant_per_cell() {
    let copy = best_of(3, b"\x1b[1;1;1024;1024;1;1;2;1$v");
    let erase = best_of(3, b"\x1b[2J");
    eprintln!("left half one column right over clusters: {copy:?}, ED 2: {erase:?}");
    assert!(
        copy < erase * 10 + std::time::Duration::from_millis(5),
        "{copy:?} vs ED 2 {erase:?}"
    );
}
