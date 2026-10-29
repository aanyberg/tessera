//! Surfaces: drawing a session into the frontend's `CAMetalLayer`
//! (ADR 0014).
//!
//! The view model (GPU, swapchain, renderer, font size, grid following the
//! surface) lives in `tessera-app-core`; this module checks that the layer
//! pointer names a `CAMetalLayer` (through the Objective-C runtime), turns it
//! into a `wgpu::Surface` (the only `unsafe` the surface path needs), checks
//! the other arguments and maps results to status codes.

use std::ffi::{c_char, c_void};

use tessera_app_core::view::{FrameStatus as CoreFrameStatus, PhysicalSize, wgpu};
use tessera_app_core::{SessionId, ViewMetrics};

use crate::app::App;
use crate::error::{self, Error};
use crate::panic::ffi_guard;

/// What `tessera_surface_render` did with a frame.
///
/// `SKIPPED`, `PACED` and `NOT_READY` all mean nothing was drawn; they
/// differ in whether the host must render again (ADR 0016). Values are
/// only ever appended: a host built against an older header that sees a
/// value it does not list should treat it like `PACED` and render again on
/// its next refresh.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameStatus {
    /// The frame was drawn and queued for presentation.
    Presented = 0,
    /// The frame was drawn and presented, but the layer's drawables no
    /// longer match its size: call `tessera_surface_resize` with the view's
    /// current size and scale before the next frame.
    Suboptimal = 1,
    /// Nothing was drawn because nothing changed since the last presented
    /// frame. Nothing is pending: the host need not render again until
    /// something gives it a frame to draw (new output (`on_output`), input,
    /// a resize, a focus or font-size change, or a re-theme
    /// (`on_appearance_changed`)), and may pause its display link until
    /// then. One known gap: a row showing a missing glyph can become
    /// drawable when the fallback font index finishes loading in the
    /// background, with no callback; it is drawn by the next frame the host
    /// renders for another reason.
    Skipped = 2,
    /// Nothing was drawn because the call came too soon after the frames
    /// before it (more than 240 frames per second, or a third call within
    /// 1/240 s). The newest content was captured but not drawn: render
    /// again on the next refresh. A display link of up to 240 Hz is never
    /// paced; a host that renders faster or on every output notification
    /// is.
    Paced = 3,
    /// Nothing was drawn because the surface had nothing to draw into: it
    /// is zero-sized, its window is occluded, or no drawable came in time.
    /// What changed stays pending: render again once the surface is ready
    /// (the next refresh, after a resize to a non-zero size, or when the
    /// window becomes visible). Waiting for a drawable has no time limit on
    /// Metal: a render waits while every drawable is in flight.
    NotReady = 4,
}

impl From<CoreFrameStatus> for FrameStatus {
    fn from(status: CoreFrameStatus) -> Self {
        match status {
            CoreFrameStatus::Presented => Self::Presented,
            CoreFrameStatus::Suboptimal => Self::Suboptimal,
            CoreFrameStatus::Skipped => Self::Skipped,
            CoreFrameStatus::Paced => Self::Paced,
            CoreFrameStatus::NotReady => Self::NotReady,
        }
    }
}

/// Whether `object` is a `CAMetalLayer` (or an instance of a subclass), or
/// `None` when the class is not registered (`QuartzCore` is not loaded).
///
/// wgpu asserts this itself and would panic; checking first turns a host
/// that passes a plain `CALayer`, an `NSView` or any other object into
/// `TESSERA_ERROR_INVALID_ARGUMENT`.
///
/// # Safety
///
/// `object` must be a valid, non-`NULL` Objective-C object pointer.
unsafe fn is_metal_layer(object: *const c_void) -> Option<bool> {
    // Declared inside the function so cbindgen, which reads every
    // module-level item, does not copy them into TesseraCore.h.
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *const c_void;
        fn object_getClass(object: *const c_void) -> *const c_void;
        fn class_getSuperclass(class: *const c_void) -> *const c_void;
    }

    // SAFETY: a NUL-terminated class name; the runtime returns NULL for an
    // unknown class.
    let target = unsafe { objc_getClass(c"CAMetalLayer".as_ptr()) };
    if target.is_null() {
        return None;
    }
    // SAFETY: `object` is a valid object per the contract.
    let mut class = unsafe { object_getClass(object) };
    while !class.is_null() {
        if class == target {
            return Some(true);
        }
        // SAFETY: `class` is a class the runtime handed out; the root
        // class's superclass is NULL, which ends the walk.
        class = unsafe { class_getSuperclass(class) };
    }
    Some(false)
}

/// `true` for a backing scale factor the core accepts: finite and
/// positive.
fn valid_scale(scale: f32) -> bool {
    scale.is_finite() && scale > 0.0
}

/// Makes the `CAMetalLayer` at `ca_metal_layer` session `session_id`'s
/// view, `width_px` x `height_px` physical pixels at backing `scale`
/// (`NSWindow.backingScaleFactor`: 1.0 on a standard display, 2.0 on
/// Retina), and resizes the session to the grid of cells that fits.
///
/// The core configures the layer (device, pixel format, drawable size,
/// opacity) and draws into it with the configured font family, font size
/// and opacity. The first attach in a process brings up the GPU (about
/// 100 ms) and loads the font database; later attaches share both. A zero
/// width or height is allowed: nothing is drawn until a non-empty
/// `tessera_surface_resize`, and the session keeps its size meanwhile.
///
/// From now on the session's grid follows the surface: this call,
/// `tessera_surface_resize` and `tessera_surface_set_font_size` resize the
/// session (its child receives `SIGWINCH`) to the rows and columns that fit
/// inside the theme's padding. Do not also call `tessera_session_resize`
/// for a session with a surface.
///
/// **Main thread only**, like every `tessera_surface_*` function (Metal
/// surface creation and presentation).
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `ca_metal_layer` is
/// `NULL`, `TESSERA_ERROR_INVALID_ARGUMENT` if `scale` is not a finite
/// positive number, the session already has a surface (detach it first) or
/// `ca_metal_layer` is an object that is not a `CAMetalLayer` (a plain
/// `CALayer`, an `NSView`, ...),
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session, and
/// `TESSERA_ERROR_GPU` if the renderer could not be created (no GPU can
/// present to the layer, or no usable font); nothing is attached then.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `ca_metal_layer` must be `NULL`
/// or a valid Objective-C object pointer (anything other than a
/// `CAMetalLayer *` is rejected as above); a `CAMetalLayer` is one the host
/// keeps alive (and does not
/// hand to another session) until `tessera_surface_detach`,
/// `tessera_session_close` or `tessera_app_free` for this session has
/// returned, or a `tessera_surface_render` reported `TESSERA_ERROR_GPU`
/// and detached it. The core retains the layer while it uses it but does
/// not take over the host's reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_surface_attach(
    app: *mut App,
    session_id: u64,
    ca_metal_layer: *mut c_void,
    width_px: u32,
    height_px: u32,
    scale: f32,
) -> Error {
    ffi_guard!("tessera_surface_attach", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        if ca_metal_layer.is_null() {
            return Error::NullPointer;
        }
        if !valid_scale(scale) {
            return Error::InvalidArgument;
        }
        let id = SessionId::from_raw(session_id);
        // Checked before the layer is touched, so a stale id or a second
        // attach never creates a surface on a layer that is in use.
        match app.core.has_view(id) {
            Ok(false) => {}
            Ok(true) => return Error::InvalidArgument,
            Err(error) => return error::report("tessera_surface_attach", &error),
        }
        // SAFETY: non-NULL and a valid Objective-C object per the contract.
        match unsafe { is_metal_layer(ca_metal_layer) } {
            Some(true) => {}
            Some(false) => return Error::InvalidArgument,
            None => {
                tracing::warn!("tessera_surface_attach: CAMetalLayer is not registered");
                return Error::Gpu;
            }
        }
        let instance = app.core.view_instance();
        // SAFETY: `ca_metal_layer` is a valid `CAMetalLayer *` that the
        // host keeps alive until the view is detached, per the contract;
        // wgpu retains it for the surface's lifetime, so `'static` holds.
        let surface = match unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                ca_metal_layer,
            ))
        } {
            Ok(surface) => surface,
            Err(err) => {
                tracing::warn!(%err, "tessera_surface_attach: creating the surface failed");
                return Error::Gpu;
            }
        };
        error::status(
            "tessera_surface_attach",
            app.core.attach_view(
                id,
                instance,
                surface,
                PhysicalSize::new(width_px, height_px),
                scale,
            ),
        )
    })
}

/// Tells session `session_id`'s surface that its view is now `width_px` x
/// `height_px` physical pixels at backing `scale`, and resizes the session
/// to the grid that fits.
///
/// Call it from the view's layout and backing-properties-changed hooks
/// (window resizes, moves between displays of different scale) and after
/// a frame reported `TESSERA_FRAME_STATUS_SUBOPTIMAL`. A zero width or
/// height suspends drawing (frames with anything to draw report
/// `TESSERA_FRAME_STATUS_NOT_READY` until a resize to a non-zero size) and
/// leaves the session's size alone. Main thread only.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_INVALID_ARGUMENT` if `scale` is not a finite positive
/// number, `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live
/// session, `TESSERA_ERROR_NO_SURFACE` if it has no surface attached.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_surface_resize(
    app: *mut App,
    session_id: u64,
    width_px: u32,
    height_px: u32,
    scale: f32,
) -> Error {
    ffi_guard!("tessera_surface_resize", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        if !valid_scale(scale) {
            return Error::InvalidArgument;
        }
        error::status(
            "tessera_surface_resize",
            app.core.resize_view(
                SessionId::from_raw(session_id),
                PhysicalSize::new(width_px, height_px),
                scale,
            ),
        )
    })
}

/// Draws session `session_id`'s visible screen into its surface, presents
/// it and stores what happened in `*out_status`.
///
/// Call it from the view's display link. The session's terminal is locked
/// only while the changed rows are copied out (microseconds); drawing and
/// waiting for a drawable do not hold up the session's output. The cursor
/// is a block, drawn hollow while the session is not focused
/// (`tessera_session_set_focus`). Main thread only.
///
/// When the call returns `TESSERA_ERROR_OK`, `*out_status` says whether to
/// call again (see `TesseraFrameStatus`):
///
/// - `TESSERA_FRAME_STATUS_PRESENTED`: drawn; render again when there is
///   something new.
/// - `TESSERA_FRAME_STATUS_SUBOPTIMAL`: drawn; call
///   `tessera_surface_resize` before the next frame.
/// - `TESSERA_FRAME_STATUS_SKIPPED`: nothing changed, nothing is pending.
///   Do not retry; the display link may pause until new output, input, a
///   resize, a focus or font-size change, or a re-theme
///   (`on_appearance_changed`). See `TESSERA_FRAME_STATUS_SKIPPED` for the
///   one known gap.
/// - `TESSERA_FRAME_STATUS_PACED`: too soon after the previous frames
///   (over 240 frames per second); the newest content is not drawn yet.
///   Render again on the next refresh.
/// - `TESSERA_FRAME_STATUS_NOT_READY`: the surface had nothing to draw
///   into (zero-sized, occluded, no drawable); what changed is still
///   pending. Render again once the surface is ready: on the next refresh,
///   after a resize to a non-zero size, or when the window becomes visible.
///
/// `*out_status` is set to `TESSERA_FRAME_STATUS_SKIPPED` before anything
/// else, so it reads "skipped" after any failure; the returned error, not
/// the status, then says what to do.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `out_status` is
/// `NULL`, `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live
/// session, `TESSERA_ERROR_NO_SURFACE` if it has no surface attached, and
/// `TESSERA_ERROR_GPU` if drawing failed. When the layer's swapchain was
/// lost beyond recovery, or wgpu reported the GPU device lost, the surface
/// is also **detached** (the next call returns `TESSERA_ERROR_NO_SURFACE`)
/// and the host attaches it again, on a new device after a device loss;
/// after any other GPU failure the surface stays attached and the next
/// frame may succeed. wgpu's Metal backend reports a loss only for its
/// own limits and errors, not a real device removal such as an unplugged
/// external GPU (ADR 0014, #138).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `out_status` must be `NULL` or
/// point to a writable `TesseraFrameStatus`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_surface_render(
    app: *mut App,
    session_id: u64,
    out_status: *mut FrameStatus,
) -> Error {
    ffi_guard!("tessera_surface_render", {
        if out_status.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { out_status.write(FrameStatus::Skipped) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        match app.core.render_view(SessionId::from_raw(session_id)) {
            Ok(status) => {
                // SAFETY: as above.
                unsafe { out_status.write(status.into()) };
                Error::Ok
            }
            Err(error) => error::report("tessera_surface_render", &error),
        }
    })
}

/// Changes session `session_id`'s surface to draw text at `size_pt`
/// points, and resizes the session to the grid that fits.
///
/// The size is clamped to 4..=512 points; a non-finite size becomes the
/// default (13 points). Glyphs are re-rasterised at the next frame. Main
/// thread only.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session,
/// `TESSERA_ERROR_NO_SURFACE` if it has no surface attached.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_surface_set_font_size(
    app: *mut App,
    session_id: u64,
    size_pt: f32,
) -> Error {
    ffi_guard!("tessera_surface_set_font_size", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        error::status(
            "tessera_surface_set_font_size",
            app.core
                .set_view_font_size(SessionId::from_raw(session_id), size_pt),
        )
    })
}

/// Detaches session `session_id`'s surface and releases everything the
/// core held for it, including its reference to the layer. Once this
/// returns, the host may release the layer. The session keeps running at
/// its current size. Main thread only.
///
/// Closing the session (`tessera_session_close`) or the application
/// (`tessera_app_free`) detaches its surface first, so a separate detach
/// is not needed then.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session,
/// `TESSERA_ERROR_NO_SURFACE` if it has no surface attached (for example a
/// second detach).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_surface_detach(app: *mut App, session_id: u64) -> Error {
    ffi_guard!("tessera_surface_detach", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        error::status(
            "tessera_surface_detach",
            app.core.detach_view(SessionId::from_raw(session_id)),
        )
    })
}

/// A surface's geometry, font size and cursor cell, as
/// `tessera_surface_metrics` reads them in one call.
///
/// Every length is in physical pixels; divide by `scale` for points. Cell
/// `(row, col)` covers `cell_width_px` x `cell_height_px` pixels from
/// `(padding_px + col * cell_width_px, padding_px + row * cell_height_px)`,
/// measured from the surface's top-left corner.
///
/// Read it once per frame and after every resize or font-size change, and
/// keep it for the mouse and IME events in between: every call locks the
/// session's terminal briefly.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SurfaceMetrics {
    /// The surface's width (after clamping to the GPU's largest texture).
    pub width_px: u32,
    /// The surface's height (after clamping to the GPU's largest texture).
    pub height_px: u32,
    /// The surface's backing scale factor, as last passed to
    /// `tessera_surface_attach` or `tessera_surface_resize`, after raising
    /// it to at least 0.25.
    pub scale: f32,
    /// Width of one cell at the current font size and scale.
    pub cell_width_px: u32,
    /// Height of one cell (one line) at the current font size and scale.
    pub cell_height_px: u32,
    /// Distance from a cell's top to the text baseline.
    pub baseline_px: f32,
    /// The empty band between each edge of the surface and the grid, the
    /// same on all four sides: the theme's padding at the renderer's scale
    /// (`scale`, capped at 8).
    pub padding_px: u32,
    /// Rows of the session's grid. It follows the surface, except after a
    /// `tessera_session_resize` until the next surface resize or font-size
    /// change.
    pub rows: u16,
    /// Columns of the session's grid; see `rows`.
    pub cols: u16,
    /// The surface's font size in points (after clamping to 4..=512), the
    /// value font zoom steps from.
    pub font_size_pt: f32,
    /// The cursor's row on the live screen (scroll offset 0), below `rows`.
    pub cursor_row: u16,
    /// The cursor's column, below `cols`.
    pub cursor_col: u16,
    /// Whether the cursor is on the screen the surface shows: `false` while
    /// the view is scrolled back into the scrollback, when `cursor_row` and
    /// `cursor_col` are where it would be at scroll offset 0.
    pub cursor_on_screen: bool,
    /// Whether the cursor is on the left half of a two-column character, so
    /// the caret spans `cursor_col` and the column after it.
    pub cursor_wide: bool,
}

impl From<ViewMetrics> for SurfaceMetrics {
    fn from(m: ViewMetrics) -> Self {
        Self {
            width_px: m.pixels.width,
            height_px: m.pixels.height,
            scale: m.scale,
            cell_width_px: m.cell.width_px,
            cell_height_px: m.cell.height_px,
            baseline_px: m.cell.baseline_px,
            padding_px: m.padding_px,
            rows: m.rows,
            cols: m.cols,
            font_size_pt: m.font_size_pt,
            cursor_row: m.cursor_row,
            cursor_col: m.cursor_col,
            cursor_on_screen: m.cursor_on_screen,
            cursor_wide: m.cursor_wide,
        }
    }
}

/// Reads session `session_id`'s surface geometry, font size and cursor cell
/// into `*out` (see `TesseraSurfaceMetrics`).
///
/// Draws nothing and waits for no GPU work. It locks the session's terminal
/// just long enough to read the grid size and cursor, so call it once per
/// frame or resize and keep the result rather than calling it per mouse
/// event. Main thread only.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `out` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session,
/// `TESSERA_ERROR_NO_SURFACE` if it has no surface attached. `*out` is
/// written only on success, all at once; after an error it holds what it
/// held before.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `out` must be `NULL` or point to
/// a writable `TesseraSurfaceMetrics`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_surface_metrics(
    app: *mut App,
    session_id: u64,
    out: *mut SurfaceMetrics,
) -> Error {
    ffi_guard!("tessera_surface_metrics", {
        if out.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        match app.core.view_metrics(SessionId::from_raw(session_id)) {
            Ok(metrics) => {
                // SAFETY: non-null and writable per the contract.
                unsafe { out.write(metrics.into()) };
                Error::Ok
            }
            Err(error) => error::report("tessera_surface_metrics", &error),
        }
    })
}

/// Panics inside the core while it holds the view table lock, through the
/// same guard every surface export uses. Returns [`Error::Panic`]; `app`
/// and its surfaces stay usable.
///
/// Test-only (feature `test-hooks`), not exported to C and absent from the
/// header.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle from `tessera_app_new`.
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub unsafe extern "C" fn tessera_test_surface_panic(app: *mut App) -> Error {
    ffi_guard!("tessera_test_surface_panic", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        app.core.panic_holding_view_lock();
        Error::Ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_status_values_are_stable() {
        let pairs = [
            (CoreFrameStatus::Presented, FrameStatus::Presented, 0),
            (CoreFrameStatus::Suboptimal, FrameStatus::Suboptimal, 1),
            (CoreFrameStatus::Skipped, FrameStatus::Skipped, 2),
            (CoreFrameStatus::Paced, FrameStatus::Paced, 3),
            (CoreFrameStatus::NotReady, FrameStatus::NotReady, 4),
        ];
        for (core, c, value) in pairs {
            assert_eq!(FrameStatus::from(core), c);
            assert_eq!(c as i32, value);
        }
    }

    /// The C header's layout, pinned: a field added, removed or reordered
    /// must change the Swift and C mirrors with it.
    #[test]
    fn surface_metrics_layout_is_stable() {
        use std::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<SurfaceMetrics>(), 44);
        assert_eq!(align_of::<SurfaceMetrics>(), 4);
        let offsets = [
            offset_of!(SurfaceMetrics, width_px),
            offset_of!(SurfaceMetrics, height_px),
            offset_of!(SurfaceMetrics, scale),
            offset_of!(SurfaceMetrics, cell_width_px),
            offset_of!(SurfaceMetrics, cell_height_px),
            offset_of!(SurfaceMetrics, baseline_px),
            offset_of!(SurfaceMetrics, padding_px),
            offset_of!(SurfaceMetrics, rows),
            offset_of!(SurfaceMetrics, cols),
            offset_of!(SurfaceMetrics, font_size_pt),
            offset_of!(SurfaceMetrics, cursor_row),
            offset_of!(SurfaceMetrics, cursor_col),
            offset_of!(SurfaceMetrics, cursor_on_screen),
            offset_of!(SurfaceMetrics, cursor_wide),
        ];
        assert_eq!(
            offsets,
            [0, 4, 8, 12, 16, 20, 24, 28, 30, 32, 36, 38, 40, 41]
        );
    }

    #[test]
    fn scales_must_be_finite_and_positive() {
        for scale in [1.0, 2.0, 0.5, 3.0] {
            assert!(valid_scale(scale), "{scale}");
        }
        for scale in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(!valid_scale(scale), "{scale}");
        }
    }
}
