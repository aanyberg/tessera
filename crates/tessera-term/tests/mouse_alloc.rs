//! Every mouse report [`encode_mouse`] writes is one allocation (#194.10):
//! the bytes are written straight into the returned buffer, which is sized
//! for the longest report, so no encoding formats into a temporary string
//! first or grows the buffer.
//!
//! The counting allocator is process-wide, so it lives in this binary of
//! its own rather than in `mouse.rs`. Allocations are counted per thread,
//! so the test harness's own threads do not disturb the count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell as StdCell;

use tessera_term::{
    Modifiers, MouseEncoding, MouseReport, MouseReportButton, MouseReportKind, MouseTracking,
    encode_mouse,
};

thread_local! {
    /// Allocations and reallocations this thread made.
    static ALLOCATIONS: StdCell<usize> = const { StdCell::new(0) };
}

/// Counts each thread's allocations, so a test sees only its own.
struct Counting;

fn count() {
    // `try_with`: the thread-local may be gone while its thread exits.
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: every call goes to `System` with the caller's arguments.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System` with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations `f` made on this thread.
fn allocations_by<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let before = ALLOCATIONS.with(StdCell::get);
    let result = f();
    (result, ALLOCATIONS.with(StdCell::get) - before)
}

#[test]
fn every_report_is_one_allocation() {
    let encodings = [
        MouseEncoding::Default,
        MouseEncoding::Utf8,
        MouseEncoding::Sgr,
        MouseEncoding::Urxvt,
        MouseEncoding::SgrPixels,
    ];
    // The longest reports: the largest code (motion of forward with every
    // modifier) at the largest coordinates, and the smallest.
    let mods = Modifiers::SHIFT | Modifiers::ALT | Modifiers::CTRL;
    let reports = [
        MouseReport::new(MouseReportKind::Press, MouseReportButton::Left, 0, 0),
        MouseReport::new(MouseReportKind::Release, MouseReportButton::Left, 200, 100),
        MouseReport::new(
            MouseReportKind::Motion,
            MouseReportButton::Forward,
            u32::MAX,
            u32::MAX,
        )
        .with_pixels(f32::MAX, f32::MAX, 0, 0)
        .with_modifiers(mods),
        MouseReport::new(
            MouseReportKind::Release,
            MouseReportButton::Forward,
            u32::MAX - 1,
            u32::MAX - 1,
        )
        .with_pixels(1e10, 1e10, 0, 0)
        .with_modifiers(mods),
    ];
    let mut checked = 0;
    for encoding in encodings {
        for report in &reports {
            let (bytes, allocations) =
                allocations_by(|| encode_mouse(report, MouseTracking::AnyMotion, encoding));
            let Some(bytes) = bytes else {
                continue;
            };
            assert_eq!(
                allocations,
                1,
                "{encoding:?} {report:?}: {} bytes in {allocations} allocations",
                bytes.len()
            );
            checked += 1;
        }
    }
    // The large legacy presses and motion are not expressible; the rest are.
    assert!(checked >= 14, "only {checked} reports were encoded");
}
