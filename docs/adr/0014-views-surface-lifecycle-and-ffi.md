# ADR 0014: Views, the surface lifecycle and its C API

**Status:** accepted · 2026-10-28

## Context

`tessera-render` can draw a `FrameInput` into a `RenderSurface` (ADR 0010),
but nothing connects a session to a surface yet. Issue #25 asks for a C API
that lets the macOS frontend hand a `CAMetalLayer` to the core, render into
it on every display refresh, follow window resizes and moves between
displays of different backing scale, and detach it again. The Swift view
that calls it is #27; the scrollback viewport (#31), font zoom (#29) and
the control server all need the same per-session view state, so it cannot
live in Swift or in the FFI crate.

Four constraints shaped the design:

- `unsafe` may only live in `tessera-pty` and `tessera-ffi`
  (`docs/architecture.md`, invariant 6).
- The issue text proposed wrapping the layer in a `raw-window-handle`
  `AppKitWindowHandle`, but that handle names an `NSView`, not a layer.
- The session's reader thread applies child output under the terminal
  lock; anything the frontend does under that lock delays output.
- `libtessera_ffi.a` now contains `wgpu` and `cosmic-text`, so every host
  that links it must link what they need.

## Decision

### Views live in `tessera-app-core`

`tessera-app-core` depends on `tessera-render` (allowed by the crate table)
and gains a `view` module. A `View` is one session's on-screen presence:
the `RenderSurface` configured on the frontend's layer, the `Renderer`
(shaper and glyph atlas) that draws into it, its font size and a reusable
snapshot buffer. `App` owns at most one view per session, keyed by
`SessionId`, behind its own mutex:

| `App` method | C function |
|---|---|
| `attach_view(id, instance, surface, size, scale)` | `tessera_surface_attach(app, id, ca_metal_layer, width_px, height_px, scale)` |
| `resize_view(id, size, scale)` | `tessera_surface_resize(app, id, width_px, height_px, scale)` |
| `render_view(id) -> FrameStatus` | `tessera_surface_render(app, id, TesseraFrameStatus *out_status)` |
| `set_view_font_size(id, size_pt)` | `tessera_surface_set_font_size(app, id, size_pt)` |
| `detach_view(id)` | `tessera_surface_detach(app, id)` |

`App::view_instance`, `has_view`, `view_ids`, `view_size`,
`view_font_size` and `view_cell_metrics` support the FFI, tests and #29;
`View` itself is crate-private and reachable only through `App`. The process has one `Gpu`, brought up by the first attach
with `Gpu::with_compatible_surface` on the instance the first surface was
created on, and one font database (`Fonts`), loaded by the first attach
and shared by every view's `Shaper::with_fonts`; later views reuse both.
`App::view_instance` hands the frontend the GPU's instance (or a fresh one
before the first view), so every surface belongs to the instance its
adapter came from. `tessera-ffi/src/surface.rs` only checks arguments,
converts the layer pointer into a `wgpu::Surface<'static>` and maps
results to status codes.

### The layer becomes a surface through `CoreAnimationLayer`

The FFI calls `instance.create_surface_unsafe(
SurfaceTargetUnsafe::CoreAnimationLayer(ptr))` (wgpu 30.0.1). This
supersedes the issue's `raw-window-handle` wording. wgpu-hal asserts that
the object is a `CAMetalLayer` and panics otherwise, which the FFI would
report as `TESSERA_ERROR_PANIC`; so the FFI first walks the object's class
chain through the Objective-C runtime (`object_getClass`,
`class_getSuperclass`, compared with `objc_getClass("CAMetalLayer")`) and
returns `TESSERA_ERROR_INVALID_ARGUMENT` for a plain `CALayer`, an
`NSView` or any other object. The pointer must still be a valid
Objective-C object; that cannot be checked. wgpu retains the layer for as long as the
surface lives, so the `'static` lifetime holds; the header nevertheless
asks the host to keep the layer alive until the surface is detached (by
`tessera_surface_detach`, `tessera_session_close`, `tessera_app_free` or a
lost swapchain), which is what a view owning its layer does anyway.

Every `tessera_surface_*` function is **main thread only**, like every
other call except `tessera_session_write`: Metal surface creation,
configuration and presentation belong there.

### Lifecycle and error codes

- One view per session. A second attach for the same id returns
  `TESSERA_ERROR_INVALID_ARGUMENT` (`Error::ViewAttached`) before the layer
  is touched, so a stale call cannot create a second surface on a layer in
  use.
- `resize`, `render`, `set_font_size` and `detach` on a live session
  without a view return the new `TESSERA_ERROR_NO_SURFACE`
  (`Error::NoView`); on an unknown id they return
  `TESSERA_ERROR_SESSION_NOT_FOUND`. A second detach is therefore
  `NO_SURFACE`.
- A GPU or font failure is the new `TESSERA_ERROR_GPU`
  (`Error::Gpu(RenderError)`): on attach (no adapter, unsupported surface,
  device error, no usable font) nothing is attached; on render, a
  swapchain that cannot be recovered (`RenderError::SurfaceLost`) detaches
  the view and the host attaches again, and any other render error leaves
  the view attached so the next frame can succeed.
- A scale that is not finite and positive is
  `TESSERA_ERROR_INVALID_ARGUMENT`; a font size is clamped through
  `sanitize_font_size` (4 to 512 points, non-finite becomes 13), so a zoom
  gesture can never fail.
- `tessera_session_close` and `tessera_app_free` detach the view first.
- `tessera_session_resize` stays available, but its header asks hosts not
  to call it for a session with a surface: the child keeps the requested
  size only until the next surface resize or font-size change.
- `tessera_surface_render` writes `TESSERA_FRAME_STATUS_SKIPPED` to
  `*out_status` before anything else. `TesseraFrameStatus` mirrors
  `FrameStatus`: `PRESENTED` (0), `SUBOPTIMAL` (1, the host should call
  `tessera_surface_resize` with its current size) and `SKIPPED` (2, a
  zero-sized surface, an occluded window, or a layer that vended no
  drawable). There is no drawable timeout on Metal (see below).

### The grid follows the surface

On attach, resize and font-size change the view computes
`(rows, cols) = view::grid_size_for(size, scale, theme.padding,
cell_metrics)`: the surface minus `round(padding · scale)` pixels on every
side, divided into whole cells, at least 1×1 and at most
`MAX_VIEW_COLS` = 2048 columns by `MAX_VIEW_ROWS` = 1024 rows. The cap
keeps a pathological surface from allocating a huge grid: the largest
Metal surface, 16384² pixels, at backing scale 0.25 with a 4 pt font has
1×1-pixel cells and would otherwise ask for 16382×16382 cells, about
4.3 GB once #3 makes `App::resize` reflow the grid. 2048 columns of the
smallest readable font still span several 8K displays. The session is
then resized with the existing
`App::resize(id, PtySize::new(rows, cols).with_pixels(w, h))`
(`view::pty_size_for`), which sends `SIGWINCH` and, once #3 lands,
reflows the grid. The pixel size is the **text area**, `cols × cell
width` by `rows × cell height` (each clamped to `u16`), not the whole
surface with its padding, which is what xterm reports in `ws_xpixel` and
`ws_ypixel`: programs that divide it by the cell count to size images get
the cell size. An 800×600 view of Menlo 13 pt at 1x is 99×39 cells and
reports 792×585 pixels. A capped grid reports the capped text area, not
the surface. Two refinements:

- A **zero-sized** surface does not resize the session. A collapsed or
  hidden view would otherwise shrink the session to one cell, reflowing
  its screen and sending full-screen programs a 1×1 window, only to grow
  it back a moment later. Frames are skipped until a non-empty resize.
- A PTY that rejects the size is **logged, not reported**: the view
  operation itself succeeded, and `tessera_session_resize` remains the
  call that reports PTY errors. The header asks hosts not to call it for
  a session with a surface.

The surface is resized with `RenderSurface::resize(&gpu, size, scale)` and
the shaper follows the surface's scale with `Renderer::set_font_size`,
which keeps the atlas when the pixel size did not change.

### Theme and font from the configuration

`view::theme_for(config)` is `Theme::default()` with
`appearance.font_family`, `appearance.font_size` (sanitised) and
`background.a = appearance.opacity`. A translucent background asks the
surface for per-pixel alpha (`SurfaceOptions::transparent`). The font
database is loaded with that family and no extra fallback; `Shaper` and
`Fonts` failures surface as `Error::Gpu(RenderError::NoFont)`, so the
header documents `TESSERA_ERROR_GPU` as "the renderer could not be created
(GPU or font)".

### Frame input, and how long the terminal lock is held

`render_view` locks the session's terminal (`with_terminal`) only to copy
the grid's `visible_lines()` cells and cursor into the view's snapshot
buffer (reused across frames), then releases it and builds the frame from
the copy with `Snapshot::frame` (a pure function, tested without a GPU):
`FrameInput::new(&rows)` with
`CursorState::new(row, col, CursorShape::Block)` (visible), `focused` from
`App::is_focused`, and no selection or overlays. The scrollback viewport is
#31; blink and cursor shape are #27 and #29.

The wave plan had `render_view` draw while holding the lock. Measured on
the development machine (Apple M4, macOS 26.6.2, release build), a full
200×60 screen of the `ascii` corpus line takes **6.5 ms** to render
(shaping, instance building, submission; best of 30), over the 2 ms budget
for the reader thread. Worse, presenting acquires a drawable first, and
`nextDrawable` waits while every drawable is in flight, and wgpu-hal
30.0.1 sets `allowsNextDrawableTimeout = NO` on the layer
(`metal/surface.rs`), so that wait has no time limit; wgpu only skips
acquiring for a window it sees as occluded. Under the lock, a GPU that
falls behind would stall the session's output for as long. Copying the same
screen takes **8 µs** in release and 13 µs in debug (best of 50,
`view::tests::snapshot_of_a_large_screen_is_fast`, which asserts < 2 ms),
so the copy is what holds the lock. That test times the copy only; that
`render_view` draws after releasing the lock is a property of its code,
not of a test. A 200×60 snapshot costs about 190 KiB
per view.

### Linking

`libtessera_ffi.a` now needs the Objective-C runtime and five system
frameworks, per `cargo rustc -p tessera-ffi --lib --crate-type staticlib
-- --print native-static-libs`: Metal, QuartzCore, CoreGraphics,
CoreFoundation and Foundation (CoreText is not among them: fonts are read
from files by `fontdb`). They are `sdk:` dependencies of the `TesseraKit`
target in `apps/macos/project.yml`; today the app and the test bundle also
get them through Clang module autolinking of what they import, but the
explicit dependencies keep the static library's needs from depending on
that. `scripts/ffi-c-test.sh` links every C test with them (the existing
lifecycle and input tests no longer link with `-liconv` alone) and runs
the new `tests/c/surface.c`.

## Consequences

- #27 writes a `TesseraSurface` view that owns a `CAMetalLayer`, attaches
  it once it has a size, forwards `layout`/`viewDidChangeBackingProperties`
  to `tessera_surface_resize`, calls `tessera_surface_render` from a
  display link, and re-attaches after a render returns `TESSERA_ERROR_GPU`
  with the surface gone. The manual checks deferred from #20 (a
  translucent `#808080` at a = 0.5 must not look like `#bababa`) and the
  Retina/non-Retina move move to #27; this ADR's Done-when is headless.
- #29 (font zoom) is Swift-only: `tessera_surface_set_font_size` exists.
- #31 (viewport) changes what `Snapshot::capture` copies, not the API.
- #21 extends `Snapshot::capture` with each line's grapheme-cluster text
  (`Line::graphemes`), so clusters can be drawn whole from the copy; today
  only base characters are copied, as only they are shaped.
- The first attach in a process costs about 100 ms (adapter and device)
  plus the font database load, on the main thread, once.
- Every view keeps its own atlas and snapshot; the GPU, the device and the
  font database are shared.
- `App` stays `Send + Sync` (`View` is, through `Renderer` and
  `RenderSurface`), checked by the existing static assertion.
- The release build of `tessera-ffi` (a gate step) now compiles `wgpu` and
  `cosmic-text`, and so takes longer.
- The surface tests attach real `CAMetalLayer`s; a layer outside any
  window may not vend drawables, so frames may be `SKIPPED`. Both Rust and
  C suites skip their GPU parts when `TESSERA_SKIP_GPU_TESTS` is set.
