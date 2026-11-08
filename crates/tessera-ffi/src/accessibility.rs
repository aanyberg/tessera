//! The text a session's view shows, for assistive clients such as
//! `VoiceOver` (ADR 0041).
//!
//! `tessera_session_accessible_text` takes a snapshot of the text: an owned
//! handle the caller frees with `tessera_accessible_text_free`. The other
//! calls read the snapshot only, never the session, so they take no lock
//! and stay valid after the session closes. Offsets and lengths are UTF-16
//! code units (what `NSRange` counts); lines are the rows of the view,
//! line `n` being row `n`; cells are view-relative, row 0 being the top row
//! the view shows at its scroll offset.
//!
//! The text is the child's (security rule S3): the core sanitises it (no
//! NUL, no control character, no bidirectional formatting control, no line
//! or paragraph separator), caps it at `TESSERA_MAX_ACCESSIBLE_TEXT_UNITS`
//! units of cell text and never logs it (S8). Announcements are capped at
//! `TESSERA_MAX_ANNOUNCEMENT_CHARS` characters (S5).

use std::slice;

use tessera_app_core::{AccessibleText as CoreText, SessionId, accessibility};

use crate::app::App;
use crate::error::{self, Error};
use crate::panic::ffi_guard;

/// The most UTF-16 units of cell text a snapshot holds; the newlines that
/// end its lines come on top (one per row at most).
pub const MAX_ACCESSIBLE_TEXT_UNITS: usize = 4_194_304;

const _: () = assert!(MAX_ACCESSIBLE_TEXT_UNITS == accessibility::MAX_TEXT_UNITS);

/// The most characters `tessera_accessible_text_announcement` gives: the
/// newest are kept, cut between characters. A character is a cell's text
/// or a newline, at most one extended grapheme cluster each.
pub const MAX_ANNOUNCEMENT_CHARS: usize = 1024;

const _: () = assert!(MAX_ANNOUNCEMENT_CHARS == accessibility::MAX_ANNOUNCEMENT_CHARS);

/// A snapshot of the text a session's view showed when it was taken
/// (`tessera_session_accessible_text`). Opaque; free it with
/// `tessera_accessible_text_free`. It owns its data and is never changed,
/// so it may be read from any thread, also concurrently.
#[derive(Debug)]
pub struct AccessibleText {
    text: CoreText,
}

impl AccessibleText {
    /// Borrows the snapshot behind `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must be `NULL` or a live handle from
    /// `tessera_session_accessible_text` that is not freed while the
    /// reference is used.
    unsafe fn from_ptr<'a>(ptr: *const Self) -> Option<&'a CoreText> {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr.as_ref() }.map(|text| &text.text)
    }
}

/// A range of UTF-16 code units: `length` units from `location`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextRange {
    /// The first unit.
    pub location: usize,
    /// The number of units.
    pub length: usize,
}

impl From<std::ops::Range<usize>> for TextRange {
    fn from(range: std::ops::Range<usize>) -> Self {
        Self {
            location: range.start,
            length: range.end - range.start,
        }
    }
}

/// A rectangle of cells: `rows` rows from row `row` and `cols` columns from
/// column `col`, view-relative.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CellRect {
    /// The top row.
    pub row: usize,
    /// The left column.
    pub col: usize,
    /// The number of rows (at least 1).
    pub rows: usize,
    /// The number of columns (0 for an empty range).
    pub cols: usize,
}

/// What a snapshot holds, as `tessera_accessible_text_contents` reports
/// it. `text` points into the snapshot: valid until it is freed.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccessibleTextContents {
    /// The text, `length` UTF-16 units, not NUL-terminated; never `NULL`
    /// (a dangling, aligned pointer when `length` is 0).
    pub text: *const u16,
    /// The text's length in UTF-16 units.
    pub length: usize,
    /// The number of lines: 0 for an empty view scrolled back, otherwise
    /// at least 1.
    pub line_count: usize,
    /// The insertion point: the cursor, or `length` while the view is
    /// scrolled back and shows none.
    pub insertion_point: usize,
    /// `true` when the view shows the cursor (`insertion_point` is it).
    pub has_cursor: bool,
    /// The selected range: the selection clipped to the text, or an empty
    /// range at `insertion_point` without one in view.
    pub selected: TextRange,
    /// `true` when `selected` is a selection.
    pub has_selection: bool,
}

impl Default for AccessibleTextContents {
    /// What a failed call reports: an empty text.
    fn default() -> Self {
        Self {
            text: std::ptr::NonNull::dangling().as_ptr(),
            length: 0,
            line_count: 0,
            insertion_point: 0,
            has_cursor: false,
            selected: TextRange::default(),
            has_selection: false,
        }
    }
}

/// Takes a snapshot of the text session `session_id`'s view shows now (the
/// rows at its scroll offset, its cursor and its selection) and stores a
/// new handle to it in `*out` (`NULL` on failure when `out` is not
/// `NULL`). Free it with `tessera_accessible_text_free`.
///
/// Holds the session's terminal lock only to copy the rows, as a frame
/// does; the text is built after. Call it when an assistive client is
/// attached and asks, at most a few times a second, not per output
/// callback.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `out` is `NULL` and
/// `TESSERA_ERROR_SESSION_NOT_FOUND` for an id that is not a live session.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle; `out` must be `NULL` or point to
/// a writable pointer. Main thread only, like the other session calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_accessible_text(
    app: *const App,
    session_id: u64,
    out: *mut *mut AccessibleText,
) -> Error {
    ffi_guard!("tessera_session_accessible_text", {
        if out.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { out.write(std::ptr::null_mut()) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        match app.core.accessible_text(SessionId::from_raw(session_id)) {
            Ok(text) => {
                let handle = Box::into_raw(Box::new(AccessibleText { text }));
                // SAFETY: as above.
                unsafe { out.write(handle) };
                Error::Ok
            }
            Err(error) => error::report("tessera_session_accessible_text", &error),
        }
    })
}

/// Frees a snapshot. `NULL` is ignored.
///
/// # Safety
///
/// `text` must be `NULL` or a live handle from
/// `tessera_session_accessible_text`, not used again afterwards and not in
/// use on another thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_accessible_text_free(text: *mut AccessibleText) {
    ffi_guard!(void "tessera_accessible_text_free", {
        if !text.is_null() {
            // SAFETY: from `Box::into_raw` in `tessera_session_accessible_text`,
            // freed once per the contract.
            drop(unsafe { Box::from_raw(text) });
        }
    });
}

/// Stores what snapshot `text` holds in `*out` (an empty text on failure
/// when `out` is not `NULL`).
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `text` or `out` is `NULL`.
///
/// # Safety
///
/// `text` must be `NULL` or a live snapshot handle; `out` must be `NULL`
/// or point to a writable `TesseraAccessibleTextContents`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_accessible_text_contents(
    text: *const AccessibleText,
    out: *mut AccessibleTextContents,
) -> Error {
    ffi_guard!("tessera_accessible_text_contents", {
        if out.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { out.write(AccessibleTextContents::default()) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(text) = (unsafe { AccessibleText::from_ptr(text) }) else {
            return Error::NullPointer;
        };
        let units = text.text();
        let contents = AccessibleTextContents {
            text: if units.is_empty() {
                std::ptr::NonNull::dangling().as_ptr()
            } else {
                units.as_ptr()
            },
            length: units.len(),
            line_count: text.line_count(),
            insertion_point: text.insertion_point(),
            has_cursor: text.cursor().is_some(),
            selected: text.selected_range().into(),
            has_selection: text.selection().is_some(),
        };
        // SAFETY: as above.
        unsafe { out.write(contents) };
        Error::Ok
    })
}

/// Reads snapshot `text` with `f` and stores the result in `*out`, or the
/// default there and `TESSERA_ERROR_NULL_POINTER` when either is `NULL`.
///
/// # Safety
///
/// As the calls below: `text` `NULL` or live, `out` `NULL` or writable.
unsafe fn read<T: Default>(
    text: *const AccessibleText,
    out: *mut T,
    f: impl FnOnce(&CoreText) -> Option<T>,
) -> Error {
    if out.is_null() {
        return Error::NullPointer;
    }
    // SAFETY: non-null and writable per the contract.
    unsafe { out.write(T::default()) };
    // SAFETY: the caller passes NULL or a live handle.
    let Some(text) = (unsafe { AccessibleText::from_ptr(text) }) else {
        return Error::NullPointer;
    };
    match f(text) {
        Some(value) => {
            // SAFETY: as above.
            unsafe { out.write(value) };
            Error::Ok
        }
        None => Error::InvalidArgument,
    }
}

/// Stores in `*out` the line holding offset `index` of snapshot `text`; an
/// index at or past the end gives the last line, an empty text line 0.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `text` or `out` is `NULL`.
///
/// # Safety
///
/// `text` must be `NULL` or a live snapshot handle; `out` must be `NULL`
/// or point to a writable `size_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_accessible_text_line_for_index(
    text: *const AccessibleText,
    index: usize,
    out: *mut usize,
) -> Error {
    ffi_guard!("tessera_accessible_text_line_for_index", {
        // SAFETY: per the contract.
        unsafe { read(text, out, |text| Some(text.line_for_index(index))) }
    })
}

/// Stores in `*out` the range of line `line` of snapshot `text`, its `\n`
/// included (the last line has none).
///
/// Returns `TESSERA_ERROR_INVALID_ARGUMENT` (and an empty range) for a line
/// past the last, `TESSERA_ERROR_NULL_POINTER` if `text` or `out` is
/// `NULL`.
///
/// # Safety
///
/// `text` must be `NULL` or a live snapshot handle; `out` must be `NULL`
/// or point to a writable `TesseraTextRange`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_accessible_text_line_range(
    text: *const AccessibleText,
    line: usize,
    out: *mut TextRange,
) -> Error {
    ffi_guard!("tessera_accessible_text_line_range", {
        // SAFETY: per the contract.
        unsafe {
            read(text, out, |text| {
                text.range_for_line(line).map(TextRange::from)
            })
        }
    })
}

/// Stores in `*out` the range of the character holding offset `index` of
/// snapshot `text`: a whole grapheme cluster or surrogate pair, a `\n`
/// alone, an empty range at the end for an index at or past it.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `text` or `out` is `NULL`.
///
/// # Safety
///
/// As `tessera_accessible_text_line_range`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_accessible_text_char_range(
    text: *const AccessibleText,
    index: usize,
    out: *mut TextRange,
) -> Error {
    ffi_guard!("tessera_accessible_text_char_range", {
        // SAFETY: per the contract.
        unsafe { read(text, out, |text| Some(text.range_for_index(index).into())) }
    })
}

/// Stores in `*out` the range of the character cell (`col`, `row`) of the
/// view showed in snapshot `text`: the whole character for a cell with text
/// (either half of a wide one), the line's last character for a cell right
/// of the text, an empty range at the start of an empty line, and an empty
/// range at the end for a row below the last line.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `text` or `out` is `NULL`.
///
/// # Safety
///
/// As `tessera_accessible_text_line_range`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_accessible_text_range_at_cell(
    text: *const AccessibleText,
    col: u16,
    row: u16,
    out: *mut TextRange,
) -> Error {
    ffi_guard!("tessera_accessible_text_range_at_cell", {
        // SAFETY: per the contract.
        unsafe {
            read(text, out, |text| {
                Some(
                    text.range_at_cell(usize::from(row), usize::from(col))
                        .into(),
                )
            })
        }
    })
}

/// Stores in `*out` the cells range `range` of snapshot `text` covers
/// (clamped to the text), as the rectangle that encloses them: a range over
/// several lines encloses each line's part. An empty range is 0 columns
/// wide and 1 row high, at the left edge of the cell its location is in.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `text` or `out` is `NULL`.
///
/// # Safety
///
/// `text` must be `NULL` or a live snapshot handle; `out` must be `NULL`
/// or point to a writable `TesseraCellRect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_accessible_text_cells_for_range(
    text: *const AccessibleText,
    range: TextRange,
    out: *mut CellRect,
) -> Error {
    ffi_guard!("tessera_accessible_text_cells_for_range", {
        // SAFETY: per the contract.
        unsafe {
            read(text, out, |text| {
                let end = range.location.saturating_add(range.length);
                let rect = text.cells_for_range(range.location..end);
                Some(CellRect {
                    row: rect.row,
                    col: rect.col,
                    rows: rect.rows,
                    cols: rect.cols,
                })
            })
        }
    })
}

/// Copies the text to announce for the output that turned snapshot
/// `previous` into snapshot `current` (two snapshots of the same view, in
/// that order): the lines from `previous`'s cursor line to `current`'s, by
/// line number so scrolled output is followed, without the old cursor line
/// when unchanged and without blank lines, joined with `\n`; nothing when
/// the cursor stayed on its line, either view is scrolled back, or the
/// terminal was resized, cleared to the cursor or switched between the
/// main and alternate screens in between. At most
/// `TESSERA_MAX_ANNOUNCEMENT_CHARS` characters, the newest kept.
///
/// Stores its length in UTF-16 units in `*len` and writes the units (no
/// NUL) to `buf` when `cap` (in units) holds them all; nothing to announce
/// gives `*len` 0. When `cap` is smaller nothing is written and
/// `TESSERA_ERROR_INVALID_ARGUMENT` is returned: call again with `*len`
/// units. `buf` may be `NULL` when `cap` is 0.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `previous`, `current` or `len`
/// is `NULL`, or `buf` is `NULL` with room claimed for the text (`*len`
/// is then 0).
///
/// # Safety
///
/// `previous` and `current` must be `NULL` or live snapshot handles; `len`
/// must be `NULL` or point to a writable `size_t`; `buf` must be valid for
/// `cap` writable `uint16_t`s unless `cap` is 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_accessible_text_announcement(
    previous: *const AccessibleText,
    current: *const AccessibleText,
    buf: *mut u16,
    cap: usize,
    len: *mut usize,
) -> Error {
    ffi_guard!("tessera_accessible_text_announcement", {
        if len.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { len.write(0) };
        // SAFETY: the caller passes NULL or live handles.
        let (Some(previous), Some(current)) =
            (unsafe { AccessibleText::from_ptr(previous) }, unsafe {
                AccessibleText::from_ptr(current)
            })
        else {
            return Error::NullPointer;
        };
        let units = accessibility::announcement(previous, current);
        if units.is_empty() {
            return Error::Ok;
        }
        if cap < units.len() {
            // SAFETY: as above.
            unsafe { len.write(units.len()) };
            return Error::InvalidArgument;
        }
        if buf.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: `buf` holds `cap >= units.len()` writable units.
        unsafe { slice::from_raw_parts_mut(buf, units.len()) }.copy_from_slice(&units);
        // SAFETY: as above.
        unsafe { len.write(units.len()) };
        Error::Ok
    })
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;

    #[test]
    fn null_pointers_are_refused_and_outputs_cleared() {
        // SAFETY: NULL handles and writable outputs are allowed.
        unsafe {
            let mut out = ptr::dangling_mut();
            assert_eq!(
                tessera_session_accessible_text(ptr::null(), 1, &raw mut out),
                Error::NullPointer
            );
            assert!(out.is_null());
            assert_eq!(
                tessera_session_accessible_text(ptr::null(), 1, ptr::null_mut()),
                Error::NullPointer
            );
            assert_eq!(
                tessera_accessible_text_contents(ptr::null(), ptr::null_mut()),
                Error::NullPointer
            );
            let mut contents = AccessibleTextContents {
                length: 5,
                ..AccessibleTextContents::default()
            };
            assert_eq!(
                tessera_accessible_text_contents(ptr::null(), &raw mut contents),
                Error::NullPointer
            );
            assert_eq!(contents, AccessibleTextContents::default());
            let mut line = 7;
            assert_eq!(
                tessera_accessible_text_line_for_index(ptr::null(), 0, &raw mut line),
                Error::NullPointer
            );
            assert_eq!(line, 0);
            let mut range = TextRange {
                location: 3,
                length: 4,
            };
            assert_eq!(
                tessera_accessible_text_line_range(ptr::null(), 0, &raw mut range),
                Error::NullPointer
            );
            assert_eq!(range, TextRange::default());
            assert_eq!(
                tessera_accessible_text_char_range(ptr::null(), 0, &raw mut range),
                Error::NullPointer
            );
            assert_eq!(
                tessera_accessible_text_range_at_cell(ptr::null(), 0, 0, &raw mut range),
                Error::NullPointer
            );
            let mut rect = CellRect {
                row: 1,
                col: 1,
                rows: 1,
                cols: 1,
            };
            assert_eq!(
                tessera_accessible_text_cells_for_range(
                    ptr::null(),
                    TextRange::default(),
                    &raw mut rect
                ),
                Error::NullPointer
            );
            assert_eq!(rect, CellRect::default());
            let mut len = 9;
            assert_eq!(
                tessera_accessible_text_announcement(
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                    0,
                    &raw mut len
                ),
                Error::NullPointer
            );
            assert_eq!(len, 0);
            tessera_accessible_text_free(ptr::null_mut());
        }
    }
}
