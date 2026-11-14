//! The rectangle operations damage a session's rows as an erase of the
//! same lines does (#430 20.1 D1-4, ADR 0046), checked through the
//! application rather than the grid alone (`tessera-term`'s `tests/rect.rs`
//! checks the grid): the bytes go through a session's own `advance` path
//! (`App::feed_for_test`, the reader's chunk advance), the damage is what
//! the session's terminal reports to the view and the selection guard
//! (`Grid::damage_since` since the last look), and the selection, which
//! clears itself when a row it covers changed, sees a DECERA exactly as it
//! sees an EL of the same rows.

use tessera_app_core::{App, Config, ConfigPaths, Granularity, Point, PtySize, SessionId};
use tessera_grid::DamageRows;

/// Three rows of text with the cursor on row 2, column 1 (1-based), so
/// the operations below and the EL they are compared with start from the
/// same cursor.
const SCREEN: &[u8] = b"abcdef\r\nghijkl\r\nmnopqr\x1b[2;1H";

/// An EL of rows 2 and 3 (1-based) that leaves the cursor where it was.
const EL_ROWS_2_3: &[u8] = b"\x1b[2K\x1b[3;1H\x1b[2K\x1b[2;1H";

/// The four operations on rows 2-3, columns 2-4 (DECCRA's destination is
/// rows 2-3 too).
const OPS: [(&str, &[u8]); 4] = [
    ("DECERA", b"\x1b[2;2;3;4$z"),
    ("DECFRA", b"\x1b[88;2;2;3;4$x"),
    ("DECCRA", b"\x1b[1;1;2;3;1;2;2;1$v"),
    ("DECSERA", b"\x1b[2;2;3;4${"),
];

/// An application with one session without a child, 6 x 6, showing
/// [`SCREEN`].
fn session() -> (App, SessionId) {
    let app = App::new(Config::default(), ConfigPaths::new("/nonexistent/tessera"));
    let id = app
        .spawn_test_session(PtySize::new(6, 6))
        .expect("test session");
    app.feed_for_test(id, SCREEN).expect("feed");
    (app, id)
}

/// The rows session `id`'s terminal reports damaged by `bytes`, fed the
/// way its reader feeds output.
fn damage_of(app: &App, id: SessionId, bytes: &[u8]) -> DamageRows {
    let generation = app
        .with_terminal(id, |term| term.grid().damage_generation())
        .expect("session");
    app.feed_for_test(id, bytes).expect("feed");
    app.with_terminal(id, |term| term.grid().damage_since(generation))
        .expect("session")
}

/// D1-4 through a session: each operation damages exactly the rows an EL
/// of its rows damages.
#[test]
fn a_rectangle_operation_damages_a_session_like_an_erase_of_its_lines() {
    for (name, op) in OPS {
        let (rect_app, rect) = session();
        let (el_app, el) = session();
        let by_rect = damage_of(&rect_app, rect, op);
        let by_el = damage_of(&el_app, el, EL_ROWS_2_3);
        assert_eq!(by_rect, by_el, "{name}");
        assert_eq!(by_rect, DamageRows::Rows(1..=2), "{name}: rows 2 and 3");
    }
}

/// An EL of rows 2 and 3 (1-based) from the cursor parked on row 6, and
/// back there.
const EL_ROWS_2_3_FROM_ROW_6: &[u8] = b"\x1b[2;1H\x1b[2K\x1b[3;1H\x1b[2K\x1b[6;1H";

/// What the damage is for: a selection over row 2 that an operation
/// changed is cleared, as an EL of the row clears it; one over row 1,
/// which neither touches, stays. The cursor is parked on row 6 first:
/// the cursor's row is always damaged, so a selection on it would be
/// compared whatever the operation reported.
#[test]
fn a_selection_over_a_row_a_rectangle_operation_changed_is_cleared_like_after_an_el() {
    for (name, op) in OPS {
        for (row, cleared) in [(1, true), (0, false)] {
            let mut after = Vec::new();
            for bytes in [op, EL_ROWS_2_3_FROM_ROW_6] {
                let (app, id) = session();
                app.feed_for_test(id, b"\x1b[6;1H").expect("feed");
                app.select_begin(id, Point::new(row, 0), Granularity::Char, false)
                    .expect("select");
                app.select_extend(id, Point::new(row, 5)).expect("extend");
                assert!(app.has_selection(id).expect("session"), "{name}: selected");
                app.feed_for_test(id, bytes).expect("feed");
                after.push(app.has_selection(id).expect("session"));
            }
            assert_eq!(after[0], after[1], "{name}, row {row}: unlike EL");
            assert_eq!(after[0], !cleared, "{name}, row {row}");
        }
    }
}
