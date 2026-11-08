//! Tests of the selection API (`tessera_session_select_begin`, `_extend`,
//! `_adjust`, `_all`, `_clear`, `_has_selection`, `_selection_text`,
//! `_selection_rich_text`, `_link_at`, `_set_hover`), calling the `extern "C"` functions as a
//! frontend would (ADR 0025): a `NULL` handle, an unknown or closed session
//! id, a bad enum value, `NULL` and short out-buffers, and a valid round
//! trip for each.
//!
//! Sessions are the core's test sessions (no child; bytes fed through the
//! test-only `tessera_test_with_core` hook), so the text under test is
//! exactly the bytes fed. `crates/tessera-ffi/tests/c/selection.c` covers
//! the same calls from C against a real shell.

#![cfg(unix)]

mod common;

use std::ptr;

use tessera_app_core::{PtySize, SessionId};
use tessera_ffi::{
    App, Error, Granularity, TextBuffer, tessera_app_free, tessera_app_new, tessera_session_close,
    tessera_session_has_selection, tessera_session_link_at, tessera_session_select_adjust,
    tessera_session_select_all, tessera_session_select_begin, tessera_session_select_clear,
    tessera_session_select_extend, tessera_session_selection_rich_text,
    tessera_session_selection_text, tessera_session_set_hover, tessera_test_with_core,
};

/// An id no session has.
const UNKNOWN: u64 = 0xDEAD_BEEF;

/// An application handle, freed on drop.
struct Handle(*mut App);

impl Handle {
    fn new() -> Self {
        common::isolate_home();
        common::hermetic_config_dir();
        let mut app = ptr::null_mut();
        // SAFETY: `app` is a writable pointer slot.
        assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
        Self(app)
    }

    /// Runs `f` on the core behind the handle.
    fn core<R>(&self, f: impl FnOnce(&tessera_app_core::App) -> R) -> R {
        // SAFETY: a live handle.
        unsafe { tessera_test_with_core(self.0, f) }.expect("a live handle")
    }

    /// A child-less session of `rows` x `cols` that has been fed `bytes`.
    fn session(&self, rows: u16, cols: u16, bytes: &[u8]) -> u64 {
        self.core(|core| {
            let id = core
                .spawn_test_session(PtySize::new(rows, cols))
                .expect("a test session");
            core.feed_for_test(id, bytes).expect("feed");
            id.get()
        })
    }

    fn begin(&self, id: u64, col: u16, row: u16, granularity: u32) -> Error {
        // SAFETY: a live handle.
        unsafe { tessera_session_select_begin(self.0, id, col, row, granularity, false) }
    }

    fn has(&self, id: u64) -> (Error, bool) {
        // Pre-set to the value a failing call must overwrite.
        let mut out = true;
        // SAFETY: a live handle and a writable `bool`.
        let code = unsafe { tessera_session_has_selection(self.0, id, &raw mut out) };
        (code, out)
    }

    /// The selection's text through a buffer of `cap` bytes (of 64).
    fn text(&self, id: u64, cap: usize) -> (Error, usize, Vec<u8>) {
        let mut buf = vec![b'#'; 64];
        let mut len = usize::MAX;
        // SAFETY: a live handle; `buf` holds 64 >= `cap` bytes; `len` is
        // writable.
        let code = unsafe {
            tessera_session_selection_text(self.0, id, buf.as_mut_ptr(), cap, &raw mut len)
        };
        (code, len, buf)
    }

    /// The link at (`col`, `row`) through a buffer of `cap` bytes (of 64).
    fn link(&self, id: u64, col: u16, row: u16, cap: usize) -> (Error, usize, Vec<u8>) {
        let mut buf = vec![b'#'; 64];
        let mut len = usize::MAX;
        // SAFETY: as in `text`.
        let code = unsafe {
            tessera_session_link_at(self.0, id, col, row, buf.as_mut_ptr(), cap, &raw mut len)
        };
        (code, len, buf)
    }

    /// The underlines the next frame of session `id` would draw.
    fn underlines(&self, id: u64) -> usize {
        self.core(|core| {
            core.view_decorations_for_test(SessionId::from_raw(id))
                .expect("a live session")
                .1
                .len()
        })
    }
}

/// Three output buffers for `tessera_session_selection_rich_text`, each
/// of 4096 bytes pre-filled with `#`, with the capacities given.
struct RichBuffers {
    bytes: [Vec<u8>; 3],
    outs: [TextBuffer; 3],
}

impl RichBuffers {
    fn new(caps: [usize; 3]) -> Self {
        let mut bytes = [vec![b'#'; 4096], vec![b'#'; 4096], vec![b'#'; 4096]];
        let outs = [0, 1, 2].map(|i| TextBuffer {
            buf: bytes[i].as_mut_ptr(),
            cap: caps[i],
            len: usize::MAX,
        });
        Self { bytes, outs }
    }

    /// Calls the copy on session `id` of `app` (`html`, `rtf`, `plain`).
    fn copy(&mut self, app: *mut App, id: u64) -> Error {
        let [html, rtf, plain] = &mut self.outs;
        // SAFETY: every buffer points into a live 4096-byte vector and its
        // `cap` is at most 4096.
        unsafe { tessera_session_selection_rich_text(app, id, html, rtf, plain) }
    }

    fn lens(&self) -> [usize; 3] {
        self.outs.each_ref().map(|out| out.len)
    }

    fn text(&self, i: usize) -> &str {
        std::str::from_utf8(&self.bytes[i][..self.outs[i].len]).expect("UTF-8")
    }

    fn untouched(&self) -> bool {
        self.bytes
            .iter()
            .all(|bytes| bytes.iter().all(|&b| b == b'#'))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from `tessera_app_new` and is not used
        // after this.
        unsafe { tessera_app_free(self.0) };
    }
}

/// The calls that take only the handle, the id and a cell, each with the
/// given handle and id.
fn simple_calls(app: *mut App, id: u64) -> [(&'static str, Error); 6] {
    // SAFETY: every call accepts a NULL or live handle and any id.
    unsafe {
        [
            (
                "select_begin",
                tessera_session_select_begin(app, id, 0, 0, Granularity::Word as u32, false),
            ),
            (
                "select_extend",
                tessera_session_select_extend(app, id, 1, 0),
            ),
            (
                "select_adjust",
                tessera_session_select_adjust(app, id, 1, 0),
            ),
            ("select_all", tessera_session_select_all(app, id)),
            ("select_clear", tessera_session_select_clear(app, id)),
            ("set_hover", tessera_session_set_hover(app, id, 0, 0, true)),
        ]
    }
}

#[test]
fn granularity_values_round_trip() {
    for granularity in [Granularity::Char, Granularity::Word, Granularity::Line] {
        assert_eq!(Granularity::from_raw(granularity as u32), Some(granularity));
    }
    for raw in [3, 4, u32::MAX] {
        assert_eq!(Granularity::from_raw(raw), None, "{raw}");
    }
}

#[test]
fn a_null_handle_is_reported_by_every_call() {
    for (name, code) in simple_calls(ptr::null_mut(), 1) {
        assert_eq!(code, Error::NullPointer, "{name}");
    }
    // An unknown granularity with a NULL handle reports the handle.
    // SAFETY: NULL is an accepted handle.
    let code = unsafe { tessera_session_select_begin(ptr::null_mut(), 1, 0, 0, 9, false) };
    assert_eq!(code, Error::NullPointer);
    let mut out = true;
    // SAFETY: a NULL handle and a writable `bool`.
    let code = unsafe { tessera_session_has_selection(ptr::null_mut(), 1, &raw mut out) };
    assert_eq!((code, out), (Error::NullPointer, false));
    let mut buf = [0u8; 8];
    let mut len = usize::MAX;
    // SAFETY: a NULL handle, a buffer of 8 bytes and a writable length.
    let code = unsafe {
        tessera_session_selection_text(ptr::null_mut(), 1, buf.as_mut_ptr(), 8, &raw mut len)
    };
    assert_eq!((code, len), (Error::NullPointer, 0));
    len = usize::MAX;
    // SAFETY: as above.
    let code = unsafe {
        tessera_session_link_at(ptr::null_mut(), 1, 0, 0, buf.as_mut_ptr(), 8, &raw mut len)
    };
    assert_eq!((code, len), (Error::NullPointer, 0));
}

#[test]
fn an_unknown_or_closed_session_is_reported_and_out_parameters_reset() {
    let app = Handle::new();
    let id = app.session(3, 20, b"hello");
    // SAFETY: a live handle.
    assert_eq!(unsafe { tessera_session_close(app.0, id) }, Error::Ok);
    for gone in [UNKNOWN, id] {
        for (name, code) in simple_calls(app.0, gone) {
            assert_eq!(code, Error::SessionNotFound, "{name} on {gone}");
        }
        assert_eq!(app.has(gone), (Error::SessionNotFound, false));
        let (code, len, buf) = app.text(gone, 64);
        assert_eq!((code, len), (Error::SessionNotFound, 0));
        assert!(buf.iter().all(|&b| b == b'#'), "nothing written");
        let (code, len, _) = app.link(gone, 0, 0, 64);
        assert_eq!((code, len), (Error::SessionNotFound, 0));
    }
}

#[test]
fn an_unknown_granularity_is_refused_and_keeps_the_selection() {
    let app = Handle::new();
    let id = app.session(3, 20, b"hello world");
    assert_eq!(app.begin(id, 0, 0, Granularity::Word as u32), Error::Ok);
    for raw in [3, u32::MAX] {
        assert_eq!(app.begin(id, 6, 0, raw), Error::InvalidArgument, "{raw}");
    }
    let (code, len, buf) = app.text(id, 64);
    assert_eq!((code, &buf[..len]), (Error::Ok, &b"hello"[..]));
}

#[test]
fn has_selection_needs_its_out_pointer() {
    let app = Handle::new();
    let id = app.session(3, 20, b"hello");
    // SAFETY: a live handle; `out` is NULL, which the call refuses.
    let code = unsafe { tessera_session_has_selection(app.0, id, ptr::null_mut()) };
    assert_eq!(code, Error::NullPointer);
    assert_eq!(app.has(id), (Error::Ok, false));
    assert_eq!(app.begin(id, 0, 0, Granularity::Word as u32), Error::Ok);
    assert_eq!(app.has(id), (Error::Ok, true));
}

#[test]
fn gestures_round_trip_through_the_c_calls() {
    let app = Handle::new();
    let id = app.session(3, 20, b"hello world\r\nsecond line");
    // A click selects nothing until a drag.
    assert_eq!(app.begin(id, 0, 0, Granularity::Char as u32), Error::Ok);
    assert_eq!(app.has(id), (Error::Ok, false));
    // SAFETY: a live handle.
    assert_eq!(
        unsafe { tessera_session_select_extend(app.0, id, 4, 0) },
        Error::Ok
    );
    let (code, len, buf) = app.text(id, 64);
    assert_eq!((code, &buf[..len]), (Error::Ok, &b"hello"[..]));
    // A shift-click past the end moves the end.
    // SAFETY: a live handle.
    assert_eq!(
        unsafe { tessera_session_select_adjust(app.0, id, 5, 1) },
        Error::Ok
    );
    let (_, len, buf) = app.text(id, 64);
    assert_eq!(&buf[..len], b"hello world\nsecond");
    // A line click, Select All and clearing.
    assert_eq!(app.begin(id, 3, 1, Granularity::Line as u32), Error::Ok);
    let (_, len, buf) = app.text(id, 64);
    assert_eq!(&buf[..len], b"second line");
    // SAFETY: a live handle.
    assert_eq!(unsafe { tessera_session_select_all(app.0, id) }, Error::Ok);
    let (_, len, buf) = app.text(id, 64);
    assert_eq!(&buf[..len], b"hello world\nsecond line");
    // SAFETY: a live handle.
    assert_eq!(
        unsafe { tessera_session_select_clear(app.0, id) },
        Error::Ok
    );
    assert_eq!(app.has(id), (Error::Ok, false));
    // No selection: Ok with an empty text.
    let (code, len, _) = app.text(id, 64);
    assert_eq!((code, len), (Error::Ok, 0));
}

#[test]
fn selection_text_honours_its_buffer_rules() {
    let app = Handle::new();
    let id = app.session(3, 20, b"hello world");
    assert_eq!(app.begin(id, 0, 0, Granularity::Line as u32), Error::Ok);
    // A short buffer: the size needed, nothing written.
    let (code, len, buf) = app.text(id, 5);
    assert_eq!((code, len), (Error::InvalidArgument, 11));
    assert!(buf.iter().all(|&b| b == b'#'), "nothing written: {buf:?}");
    // Asking for the size with a NULL buffer and no capacity.
    let mut len = 0;
    // SAFETY: a live handle; `buf` NULL with `cap` 0; `len` writable.
    let code =
        unsafe { tessera_session_selection_text(app.0, id, ptr::null_mut(), 0, &raw mut len) };
    assert_eq!((code, len), (Error::InvalidArgument, 11));
    // A NULL buffer that claims room for the text.
    let mut len = usize::MAX;
    // SAFETY: as above; the call refuses the NULL buffer before writing.
    let code =
        unsafe { tessera_session_selection_text(app.0, id, ptr::null_mut(), 64, &raw mut len) };
    assert_eq!((code, len), (Error::NullPointer, 0));
    // A NULL length.
    let mut buf = [0u8; 64];
    // SAFETY: a live handle and a 64-byte buffer; `len` NULL is refused.
    let code =
        unsafe { tessera_session_selection_text(app.0, id, buf.as_mut_ptr(), 64, ptr::null_mut()) };
    assert_eq!(code, Error::NullPointer);
    // An exact fit, and the bytes after it untouched.
    let (code, len, buf) = app.text(id, 11);
    assert_eq!((code, &buf[..len]), (Error::Ok, &b"hello world"[..]));
    assert_eq!(buf[11], b'#');
    // A selection of blanks only: Ok, empty, even with a NULL buffer.
    assert_eq!(app.begin(id, 0, 2, Granularity::Line as u32), Error::Ok);
    assert_eq!(app.has(id), (Error::Ok, true));
    let mut len = usize::MAX;
    // SAFETY: a live handle; nothing is written for an empty text.
    let code =
        unsafe { tessera_session_selection_text(app.0, id, ptr::null_mut(), 0, &raw mut len) };
    assert_eq!((code, len), (Error::Ok, 0));
}

#[test]
fn link_at_honours_its_buffer_rules() {
    let app = Handle::new();
    // `https://a.bc/x` at columns 4-17; the `.` after it is not part of it.
    let id = app.session(3, 30, b"see https://a.bc/x. ok");
    let (code, len, buf) = app.link(id, 8, 0, 64);
    assert_eq!((code, &buf[..len]), (Error::Ok, &b"https://a.bc/x"[..]));
    let (code, len, buf) = app.link(id, 8, 0, 13);
    assert_eq!((code, len), (Error::InvalidArgument, 14));
    assert!(buf.iter().all(|&b| b == b'#'), "nothing written: {buf:?}");
    let mut len = usize::MAX;
    // SAFETY: a live handle; the NULL buffer is refused before writing.
    let code =
        unsafe { tessera_session_link_at(app.0, id, 8, 0, ptr::null_mut(), 64, &raw mut len) };
    assert_eq!((code, len), (Error::NullPointer, 0));
    let mut buf = [0u8; 64];
    // SAFETY: a live handle and a 64-byte buffer; `len` NULL is refused.
    let code =
        unsafe { tessera_session_link_at(app.0, id, 8, 0, buf.as_mut_ptr(), 64, ptr::null_mut()) };
    assert_eq!(code, Error::NullPointer);
    // No link on the dot, the text after it, or outside the grid.
    for (col, row) in [(18, 0), (20, 0), (200, 0), (0, 200)] {
        let (code, len, _) = app.link(id, col, row, 64);
        assert_eq!((code, len), (Error::Ok, 0), "({col}, {row})");
    }
}

#[test]
fn hover_sets_and_clears_the_frame_underline() {
    let app = Handle::new();
    let id = app.session(3, 30, b"see https://a.bc/x. ok");
    assert_eq!(app.underlines(id), 0);
    // SAFETY: a live handle.
    assert_eq!(
        unsafe { tessera_session_set_hover(app.0, id, 8, 0, true) },
        Error::Ok
    );
    assert_eq!(app.underlines(id), 1);
    // Inactive: the cell is ignored and the hover cleared.
    // SAFETY: a live handle.
    assert_eq!(
        unsafe { tessera_session_set_hover(app.0, id, 8, 0, false) },
        Error::Ok
    );
    assert_eq!(app.underlines(id), 0);
}

#[test]
fn rich_text_round_trips_the_colours_and_the_plain_text() {
    let app = Handle::new();
    let id = app.session(3, 30, b"\x1b[1;31mred\x1b[0m <b>&{x}\\");
    // Nothing selected: Ok and empty.
    let mut out = RichBuffers::new([4096; 3]);
    assert_eq!(out.copy(app.0, id), Error::Ok);
    assert_eq!(out.lens(), [0; 3]);
    assert!(out.untouched());
    assert_eq!(app.begin(id, 0, 0, Granularity::Line as u32), Error::Ok);
    assert_eq!(out.copy(app.0, id), Error::Ok);
    assert_eq!(out.text(2), "red <b>&{x}\\");
    let html = out.text(0);
    assert!(html.contains("font-weight:bold\">red</span>"), "{html}");
    assert!(html.contains("&lt;b&gt;&amp;&#123;x&#125;&#92;"), "{html}");
    assert!(!html.contains("<b>"), "{html}");
    let rtf = out.text(1);
    assert!(rtf.starts_with("{\\rtf1"), "{rtf}");
    assert!(rtf.contains("\\{x\\}\\\\"), "{rtf}");
    // The plain text equals what Copy copies.
    let (_, len, buf) = app.text(id, 64);
    assert_eq!(&buf[..len], out.text(2).as_bytes());
}

#[test]
fn rich_text_honours_its_buffer_rules() {
    let app = Handle::new();
    let id = app.session(3, 30, b"\x1b[31mred\x1b[0m text");
    assert_eq!(app.begin(id, 0, 0, Granularity::Line as u32), Error::Ok);
    let mut full = RichBuffers::new([4096; 3]);
    assert_eq!(full.copy(app.0, id), Error::Ok);
    let need = full.lens();
    assert_eq!(need[2], 8);
    // Any one buffer short: every size reported, nothing written anywhere.
    for short in 0..3 {
        let mut caps = need;
        caps[short] -= 1;
        let mut out = RichBuffers::new(caps);
        assert_eq!(out.copy(app.0, id), Error::InvalidArgument, "{short}");
        assert_eq!(out.lens(), need, "{short}");
        assert!(out.untouched(), "{short}");
    }
    // Exact fits, the byte after each untouched.
    let mut out = RichBuffers::new(need);
    assert_eq!(out.copy(app.0, id), Error::Ok);
    for (i, n) in need.into_iter().enumerate() {
        assert_eq!(out.text(i), full.text(i));
        assert_eq!(out.bytes[i][n], b'#');
    }
    // Asking for the sizes with NULL buffers and no capacity.
    let mut out = RichBuffers::new([0; 3]);
    for o in &mut out.outs {
        o.buf = ptr::null_mut();
    }
    assert_eq!(out.copy(app.0, id), Error::InvalidArgument);
    assert_eq!(out.lens(), need);
    // A NULL buffer that claims room: refused before anything is written.
    let mut out = RichBuffers::new([4096; 3]);
    out.outs[1].buf = ptr::null_mut();
    assert_eq!(out.copy(app.0, id), Error::NullPointer);
    assert_eq!(out.lens(), [0; 3]);
    assert!(out.untouched());
    // ... also when a later buffer is short: the NULL one decides.
    let mut out = RichBuffers::new([4096, 4096, 1]);
    out.outs[0].buf = ptr::null_mut();
    assert_eq!(out.copy(app.0, id), Error::NullPointer);
    assert_eq!(out.lens(), [0; 3]);
    assert!(out.untouched());
    // A NULL buffer struct, a NULL handle, an unknown session.
    for missing in 0..3 {
        let mut out = RichBuffers::new([4096; 3]);
        let [html, rtf, plain] = &mut out.outs;
        let mut ptrs: [*mut TextBuffer; 3] = [html, rtf, plain];
        ptrs[missing] = ptr::null_mut();
        // SAFETY: the other buffers are live; NULL is refused.
        let code =
            unsafe { tessera_session_selection_rich_text(app.0, id, ptrs[0], ptrs[1], ptrs[2]) };
        assert_eq!(code, Error::NullPointer, "{missing}");
        for (i, o) in out.outs.iter().enumerate() {
            if i != missing {
                assert_eq!(o.len, 0, "{missing}/{i}");
            }
        }
        assert!(out.untouched());
    }
    let mut out = RichBuffers::new([4096; 3]);
    assert_eq!(out.copy(ptr::null_mut(), id), Error::NullPointer);
    assert_eq!(out.lens(), [0; 3]);
    let mut out = RichBuffers::new([4096; 3]);
    assert_eq!(out.copy(app.0, UNKNOWN), Error::SessionNotFound);
    assert_eq!(out.lens(), [0; 3]);
    assert!(out.untouched());
}

#[test]
fn rich_text_past_the_cap_is_refused_and_the_plain_text_written() {
    let app = Handle::new();
    let id = app.core(|core| {
        let id = core
            .spawn_test_session(PtySize::new(50, 200))
            .expect("a test session");
        core.set_max_scrollback_for_test(id, 2_000)
            .expect("scrollback");
        // Every attribute on, the colour alternating so no run merges.
        let mut feed = String::from("\x1b[1;3;4;9;48;5;4m");
        for cell in 0..700 * 200 {
            feed.push_str(if cell % 2 == 0 {
                "\x1b[31mé"
            } else {
                "\x1b[32mé"
            });
        }
        core.feed_for_test(id, feed.as_bytes()).expect("feed");
        id.get()
    });
    // SAFETY: a live handle.
    assert_eq!(unsafe { tessera_session_select_all(app.0, id) }, Error::Ok);
    // One soft-wrapped line: no newlines.
    let plain_len = 700 * 200 * "é".len();
    let mut bytes = vec![b'#'; plain_len];
    let mut html = TextBuffer {
        buf: ptr::null_mut(),
        cap: 0,
        len: usize::MAX,
    };
    let mut rtf = TextBuffer {
        buf: ptr::null_mut(),
        cap: 0,
        len: usize::MAX,
    };
    let mut plain = TextBuffer {
        buf: bytes.as_mut_ptr(),
        cap: plain_len,
        len: usize::MAX,
    };
    // SAFETY: a live handle; `plain` holds `plain_len` bytes.
    let code = unsafe {
        tessera_session_selection_rich_text(app.0, id, &raw mut html, &raw mut rtf, &raw mut plain)
    };
    assert_eq!(
        (code, html.len, rtf.len, plain.len),
        (Error::Refused, 0, 0, plain_len)
    );
    let (_, len, _) = app.text(id, 0);
    assert_eq!(len, plain_len);
    assert!(bytes.starts_with("éé".as_bytes()) && bytes.ends_with("é".as_bytes()));
    // A short plain buffer: the size, as for the other calls.
    plain.cap = 10;
    // SAFETY: as above.
    let code = unsafe {
        tessera_session_selection_rich_text(app.0, id, &raw mut html, &raw mut rtf, &raw mut plain)
    };
    assert_eq!((code, plain.len), (Error::InvalidArgument, plain_len));
}
