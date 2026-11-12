//! Text selection, Copy and link hit-testing (ADR 0025).
//!
//! Cells are view-relative: row 0 is the top row the session's surface
//! shows at its current scroll offset, column 0 the left column; the core
//! converts them to the lines they show. Call these from the UI (main)
//! thread, like the input calls. Text comes back as UTF-8 bytes, not
//! NUL-terminated, through a caller's buffer: `*len` receives the length
//! the text needs, and nothing is written when `cap` is smaller (call
//! again with a buffer of `*len` bytes).

use std::slice;

use tessera_app_core::{FormattedText, Granularity as CoreGranularity, Point, RichCopy, SessionId};

use crate::app::App;
use crate::error::{self, Error};
use crate::panic::ffi_guard;

/// What a click selects and a drag extends by (the `granularity` argument
/// of `tessera_session_select_begin`, carried as `uint32_t`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Granularity {
    /// Single cells: a click, then a drag.
    Char = 0,
    /// Words: a double-click.
    Word = 1,
    /// Whole logical lines: a triple-click or more.
    Line = 2,
}

impl Granularity {
    /// The granularity with discriminant `raw`, or `None` when none has
    /// it.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            0 => Self::Char,
            1 => Self::Word,
            2 => Self::Line,
            _ => return None,
        })
    }

    const fn to_core(self) -> CoreGranularity {
        match self {
            Self::Char => CoreGranularity::Char,
            Self::Word => CoreGranularity::Word,
            Self::Line => CoreGranularity::Line,
        }
    }
}

/// Runs `f` on the live application `app` points to, mapping a core
/// error to its status code.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
unsafe fn with_app(
    function: &'static str,
    app: *mut App,
    f: impl FnOnce(&App) -> tessera_app_core::Result<()>,
) -> Error {
    // SAFETY: guaranteed by the caller.
    let Some(app) = (unsafe { App::from_ptr(app) }) else {
        return Error::NullPointer;
    };
    error::status(function, f(app))
}

/// Starts a selection in session `session_id` at cell (`col`, `row`) of
/// its surface (a mouse-down): `granularity` is a `TesseraGranularity`
/// value (`TESSERA_GRANULARITY_CHAR` for a click, `_WORD` for a
/// double-click, `_LINE` for a triple-click or more) and `block` asks for a
/// rectangle. A cell outside the grid is clamped to it. Any earlier
/// selection is replaced.
///
/// A click selects nothing until `tessera_session_select_extend`; a
/// double-click selects the longest smart-selection match under it (a
/// path, URL, email address, hash or IP address, by the
/// `[selection] smart_rules` patterns; ADR 0042), else the word (letters,
/// digits and `/-+\~_.`), the run of spaces or the single other character
/// under it, and nothing past the end of the line's text; dragging after a
/// double-click extends by words; a triple-click selects the whole line, its
/// soft-wrapped rows included.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_INVALID_ARGUMENT` for an unknown `granularity` (nothing
/// changes), `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live
/// session.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_select_begin(
    app: *mut App,
    session_id: u64,
    col: u16,
    row: u16,
    granularity: u32,
    block: bool,
) -> Error {
    ffi_guard!("tessera_session_select_begin", {
        let Some(granularity) = Granularity::from_raw(granularity) else {
            // A NULL app is reported before the argument.
            if app.is_null() {
                return Error::NullPointer;
            }
            return Error::InvalidArgument;
        };
        // SAFETY: the caller passes NULL or a live handle.
        unsafe {
            with_app("tessera_session_select_begin", app, |app| {
                app.core.select_begin(
                    SessionId::from_raw(session_id),
                    Point::new(row, col),
                    granularity.to_core(),
                    block,
                )
            })
        }
    })
}

/// Extends session `session_id`'s selection to cell (`col`, `row`) (a
/// drag; clamped to the grid), by the granularity it began with. Call it
/// when the pointer reaches another cell; extending onto the cell clicked
/// selects that cell. Nothing happens without a selection begun.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_select_extend(
    app: *mut App,
    session_id: u64,
    col: u16,
    row: u16,
) -> Error {
    ffi_guard!("tessera_session_select_extend", {
        // SAFETY: the caller passes NULL or a live handle.
        unsafe {
            with_app("tessera_session_select_extend", app, |app| {
                app.core
                    .select_extend(SessionId::from_raw(session_id), Point::new(row, col))
            })
        }
    })
}

/// Moves the end of session `session_id`'s selection nearer to cell
/// (`col`, `row`) there (a shift-click; on a tie, the start), keeping its
/// granularity and whether it is a block. Nothing happens without a
/// selection.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_select_adjust(
    app: *mut App,
    session_id: u64,
    col: u16,
    row: u16,
) -> Error {
    ffi_guard!("tessera_session_select_adjust", {
        // SAFETY: the caller passes NULL or a live handle.
        unsafe {
            with_app("tessera_session_select_adjust", app, |app| {
                app.core
                    .select_adjust(SessionId::from_raw(session_id), Point::new(row, col))
            })
        }
    })
}

/// Selects everything in session `session_id`: the scrollback and the
/// screen, or only the screen while a full-screen application shows the
/// alternate screen.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_select_all(app: *mut App, session_id: u64) -> Error {
    ffi_guard!("tessera_session_select_all", {
        // SAFETY: the caller passes NULL or a live handle.
        unsafe {
            with_app("tessera_session_select_all", app, |app| {
                app.core.select_all(SessionId::from_raw(session_id))
            })
        }
    })
}

/// Clears session `session_id`'s selection.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_select_clear(app: *mut App, session_id: u64) -> Error {
    ffi_guard!("tessera_session_select_clear", {
        // SAFETY: the caller passes NULL or a live handle.
        unsafe {
            with_app("tessera_session_select_clear", app, |app| {
                app.core.select_clear(SessionId::from_raw(session_id))
            })
        }
    })
}

/// Stores in `*out` whether session `session_id` has a selection (Copy is
/// possible), also while it is scrolled out of view or holds only blanks.
/// A selection whose text changed under it is cleared first.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `out` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// failure `*out` is `false` (when `out` is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `out` must be `NULL` or point to
/// a writable `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_has_selection(
    app: *mut App,
    session_id: u64,
    out: *mut bool,
) -> Error {
    ffi_guard!("tessera_session_has_selection", {
        if out.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { out.write(false) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        match app.core.has_selection(SessionId::from_raw(session_id)) {
            Ok(has) => {
                // SAFETY: as above.
                unsafe { out.write(has) };
                Error::Ok
            }
            Err(error) => error::report("tessera_session_has_selection", &error),
        }
    })
}

/// Copies `text` into the caller's buffer: stores its length in `*len`,
/// and writes it to `buf` only when `cap` holds all of it. A `NULL` `buf`
/// with text to write leaves `*len` at 0.
///
/// # Safety
///
/// `len` must be writable; `buf` must be valid for `cap` bytes of writes
/// unless `cap` is 0.
pub(crate) unsafe fn copy_out(text: &str, buf: *mut u8, cap: usize, len: *mut usize) -> Error {
    // SAFETY: writable per the contract.
    unsafe { len.write(text.len()) };
    if text.len() > cap {
        return Error::InvalidArgument;
    }
    if !text.is_empty() {
        if buf.is_null() {
            // SAFETY: as above.
            unsafe { len.write(0) };
            return Error::NullPointer;
        }
        // SAFETY: `buf` holds `cap >= text.len()` bytes per the contract.
        unsafe { slice::from_raw_parts_mut(buf, text.len()) }.copy_from_slice(text.as_bytes());
    }
    Error::Ok
}

/// Copies the text of session `session_id`'s selection, for Copy: soft
/// wraps joined, a newline at each hard line end (and between the rows of
/// a block), the blanks after each line's last text and the blank lines at
/// the end dropped, tabs as the spaces they moved over.
///
/// Stores the text's length in bytes in `*len` and writes the UTF-8 bytes
/// (no NUL terminator) to `buf` when `cap` holds them all. Without a
/// selection, and for a selection of blanks only, `*len` is 0 and nothing
/// is written; `tessera_session_has_selection` tells the two apart.
///
/// Returns `TESSERA_ERROR_INVALID_ARGUMENT` when the text is longer than
/// `cap`: nothing is written, and `*len` holds the size to call again with
/// (`buf` may be `NULL` with `cap` 0 to ask for it). Returns
/// `TESSERA_ERROR_NULL_POINTER` if `app` or `len` is `NULL`, or `buf` is
/// `NULL` with text to write; `TESSERA_ERROR_SESSION_NOT_FOUND` if the id
/// is not a live session. On those failures `*len` is 0 (when `len` is not
/// `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `len` must be `NULL` or point to
/// a writable `size_t`. `buf` must be `NULL` or valid for `cap` bytes of
/// writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_selection_text(
    app: *mut App,
    session_id: u64,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> Error {
    ffi_guard!("tessera_session_selection_text", {
        if len.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { len.write(0) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        match app.core.selection_text(SessionId::from_raw(session_id)) {
            // SAFETY: `len` and `buf` per the contract.
            Ok(text) => unsafe { copy_out(text.as_deref().unwrap_or(""), buf, cap, len) },
            Err(error) => error::report("tessera_session_selection_text", &error),
        }
    })
}

/// One caller-owned output buffer of `tessera_session_selection_rich_text`:
/// `buf` holds `cap` bytes of writes (it may be `NULL` with `cap` 0 to ask
/// for the size), and the call stores the size the text needs in `len`.
#[repr(C)]
#[derive(Debug)]
pub struct TextBuffer {
    /// Where the UTF-8 bytes go (no NUL terminator), or `NULL`.
    pub buf: *mut u8,
    /// How many bytes `buf` holds.
    pub cap: usize,
    /// Set by the call: the text's length in bytes.
    pub len: usize,
}

/// Copies `texts` into the caller's buffers all or none: stores every
/// length, and writes them only when every buffer holds its text. A NULL
/// `buf` whose `cap` claims room for its text is `NullPointer` (every
/// length then 0), whatever the other buffers hold; else a buffer too
/// short is `InvalidArgument`.
///
/// # Safety
///
/// Every buffer pointer must be writable, and each `buf` valid for its
/// `cap` bytes of writes unless `cap` is 0.
unsafe fn copy_all(texts: &[(&str, *mut TextBuffer)]) -> Error {
    let (mut short, mut null) = (false, false);
    for &(text, out) in texts {
        // SAFETY: writable per the contract.
        let out = unsafe { &mut *out };
        out.len = text.len();
        if text.len() > out.cap {
            short = true;
        } else if !text.is_empty() && out.buf.is_null() {
            null = true;
        }
    }
    if null {
        for &(_, out) in texts {
            // SAFETY: as above.
            unsafe { (*out).len = 0 };
        }
        return Error::NullPointer;
    }
    if short {
        return Error::InvalidArgument;
    }
    for &(text, out) in texts {
        // SAFETY: as above.
        let out = unsafe { &*out };
        if !text.is_empty() {
            // SAFETY: `buf` is non-null and holds `cap >= text.len()` bytes
            // per the contract.
            unsafe { slice::from_raw_parts_mut(out.buf, text.len()) }
                .copy_from_slice(text.as_bytes());
        }
    }
    Error::Ok
}

/// Copies session `session_id`'s selection for Copy with Formatting (ADR
/// 0042): `plain` receives the text `tessera_session_selection_text` would
/// copy (honouring `[selection] trim_trailing_whitespace`), `html` a
/// self-contained HTML fragment and `rtf` an RTF document of the same text,
/// with the cells' colours (the theme's, default colours left out), bold,
/// italic, underline and strikethrough. Every character of the cell text is
/// escaped; there are no links and no external resources. Put the three on
/// the pasteboard as they are.
///
/// Each buffer gets its text's length in bytes in `len`. Without a
/// selection, and for a selection of blanks only, every `len` is 0.
///
/// Returns `TESSERA_ERROR_INVALID_ARGUMENT` when a buffer is shorter than
/// its text: nothing is written to any buffer and every `len` holds the
/// size to call again with. Returns `TESSERA_ERROR_REFUSED` when the
/// formatted text would pass 16 MiB: `html` and `rtf` get `len` 0 and
/// `plain` is written as usual (copy it as plain text). Returns
/// `TESSERA_ERROR_NULL_POINTER` if `app`, `html`, `rtf` or `plain` is
/// `NULL`, or a `buf` is `NULL` with text to write and a `cap` that holds
/// it (this comes before a short buffer);
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// those failures every `len` is 0 (where its buffer is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `html`, `rtf` and `plain` must
/// each be `NULL` or point to a writable `TesseraTextBuffer` whose `buf` is
/// `NULL` or valid for `cap` bytes of writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_selection_rich_text(
    app: *mut App,
    session_id: u64,
    html: *mut TextBuffer,
    rtf: *mut TextBuffer,
    plain: *mut TextBuffer,
) -> Error {
    ffi_guard!("tessera_session_selection_rich_text", {
        let mut any_null = false;
        for out in [html, rtf, plain] {
            if out.is_null() {
                any_null = true;
            } else {
                // SAFETY: non-null and writable per the contract.
                unsafe { (*out).len = 0 };
            }
        }
        if any_null {
            return Error::NullPointer;
        }
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        match app
            .core
            .selection_copy_with_formatting(SessionId::from_raw(session_id))
        {
            Ok(None) => Error::Ok,
            Ok(Some(FormattedText {
                plain: text,
                rich: RichCopy::Text { html: h, rtf: r },
            })) => {
                // SAFETY: the buffers per the contract.
                unsafe { copy_all(&[(&h, html), (&r, rtf), (&text, plain)]) }
            }
            Ok(Some(FormattedText {
                plain: text,
                rich: RichCopy::TooLarge,
            })) => {
                // SAFETY: as above.
                match unsafe { copy_all(&[(&text, plain)]) } {
                    Error::Ok => Error::Refused,
                    status => status,
                }
            }
            Err(error) => error::report("tessera_session_selection_rich_text", &error),
        }
    })
}

/// Copies the link in session `session_id`'s text at cell (`col`, `row`)
/// of its surface (the scroll offset honoured), for command-click: the
/// OSC 8 hyperlink the cell belongs to (its URI as the program sent it,
/// any scheme), else the URL detected in the text (`http`, `https`, `ftp`,
/// `file` and `mailto` URLs, across soft wraps, without trailing
/// punctuation, as written on the screen). Opening it (and which schemes may be opened) is the caller's decision.
///
/// Stores the URL's length in bytes in `*len` and writes its UTF-8 bytes
/// (no NUL terminator) to `buf` when `cap` holds them all; with no URL
/// there (or a cell outside the grid) `*len` is 0. Buffer rules and
/// errors are those of `tessera_session_selection_text`.
///
/// # Safety
///
/// As `tessera_session_selection_text`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_link_at(
    app: *mut App,
    session_id: u64,
    col: u16,
    row: u16,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> Error {
    ffi_guard!("tessera_session_link_at", {
        if len.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { len.write(0) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        match app
            .core
            .link_at(SessionId::from_raw(session_id), Point::new(row, col))
        {
            Ok(link) => {
                let url = link.map_or_default(|link| link.url);
                // SAFETY: `len` and `buf` per the contract.
                unsafe { copy_out(&url, buf, cap, len) }
            }
            Err(error) => error::report("tessera_session_link_at", &error),
        }
    })
}

/// Sets the cell (`col`, `row`) the pointer hovers over session
/// `session_id`'s surface while the link modifier (Command) is held, with
/// `active` true; `active` false clears it (`col` and `row` are then
/// ignored). From the next `tessera_surface_render` on, the link under the
/// hovered cell is underlined: the whole OSC 8 link, or the detected URL,
/// one underline per row run it covers.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_set_hover(
    app: *mut App,
    session_id: u64,
    col: u16,
    row: u16,
    active: bool,
) -> Error {
    ffi_guard!("tessera_session_set_hover", {
        // SAFETY: the caller passes NULL or a live handle.
        unsafe {
            with_app("tessera_session_set_hover", app, |app| {
                app.core.set_hover(
                    SessionId::from_raw(session_id),
                    active.then_some(Point::new(row, col)),
                )
            })
        }
    })
}
