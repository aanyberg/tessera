# ADR 0014: Views, the surface lifecycle and its C API

**Status:** accepted · 2026-10-28 · amended 2026-10-29 (#90: reading a
surface's metrics back; see "Amendment: #90" at the end)

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
| `view_metrics(id) -> ViewMetrics` (#90) | `tessera_surface_metrics(app, id, TesseraSurfaceMetrics *out)` |

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
  swapchain that cannot be recovered (`RenderError::SurfaceLost`) or a
  lost GPU device (`RenderError::DeviceLost`, #84) detaches the view and
  the host attaches again, and any other render error leaves the view
  attached so the next frame can succeed.
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
  drawable). There is no drawable timeout on Metal (see below). Since #101,
  `SKIPPED` means only "nothing changed" and a surface that had nothing to
  draw into reports `NOT_READY` (4); a paced frame reports `PACED` (3). See
  ADR 0016's amendment of 2026-10-29.

### The grid follows the surface

On attach, resize and font-size change the view computes
`(rows, cols) = view::grid_size_for(size, scale, theme.padding,
cell_metrics)`: the surface minus `round(padding · scale)` pixels on every
side, divided into whole cells, at least 1×1 and at most
`MAX_VIEW_COLS` = 2048 columns by `MAX_VIEW_ROWS` = 1024 rows. The cap
keeps a pathological surface from allocating a huge grid: the largest
Metal surface, 16384² pixels, at backing scale 0.25 with a 4 pt font has
1×1-pixel cells and would otherwise ask for 16382×16382 cells, about
5.4 GB (20-byte cells, ADR 0013) once #3 makes `App::resize` reflow the grid. 2048 columns of the
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

The other side of the lock is the session's reader thread (#83). It used to
hold the lock while `Terminal::advance` parsed a whole PTY chunk, and under a
`yes` flood `render_view` waited 4.5–5.9 ms at the 99th percentile and
14–25 ms at worst (the issue's measurements in the app). Long holds were only
part of the cause. `std::sync::Mutex` is not fair: the reader releases the
lock, the waiting frame is woken, and the reader, still running, takes the
lock again before the waiter is scheduled. A macOS PTY hands out 1 KiB per
read, so a flood is a stream of short holds that a waiter can still lose to
for tens of milliseconds.

`Pump::run` now feeds each chunk to `advance` in slices of `ADVANCE_SLICE` =
**4096** bytes, one lock hold per slice (a slice that would end inside a
UTF-8 character runs to the character's end). The terminal mutex lives in a
`SharedTerminal` that counts the threads waiting for it: every lock taker
other than the reader (`Session::with_terminal`, which `render_view` uses,
and `Session::resize`) tries the lock first and counts itself only when it
is taken. After each slice the reader calls `thread::yield_now` while the
count is non-zero, at most 16 times, which lets the woken waiter take the
lock. With nobody waiting it does not yield at all.

A slice bounds bytes, not time. For ordinary output (every wave 5 corpus
line) a slice takes tens of microseconds, but a few sequences are expensive
per byte, and one 4 KiB slice containing them holds the lock for long:
DECALN on a 96×430 screen 238–504 ms, `REP` 65535 183 ms, `RIS` 37 ms, `SU`
or `IL` 65535 10–20 ms (adversarial measurements). That is still about 16
times shorter than holding the lock for the whole chunk, as before. Making
`RIS` cheap is #107; bounding the cost of the others (DECALN, `REP` 65535,
`SU`/`IL`) is #117.

The handoff has a cost for lock takers that do not pace themselves. The
reader gives way to every counted waiter and is not counted itself, so a
thread that takes the lock again the moment it releases it wins every
handoff and starves the session's output. Under a `yes`-style flood, one
thread calling `with_terminal` in a tight loop cut throughput from 135 to
70–98 MB/s; eight threads each holding it for 500 µs with no gap never let
16 MiB finish; `App::resize` in a tight loop took 160 s instead of 0.14 s
for 16 MiB. Paced callers, with gaps of 1 ms or more such as a display link
at up to ~120 Hz, share the lock fairly, and no current caller loops:
TesseraKit renders and resizes from the display link. `App::with_terminal`
and `App::resize` document the rule.

The chunk still produces one `SessionEvent::Output`, after its last slice,
followed by its bell and title requests in order, so the observer sees no
more callbacks than before.

The rejected alternative was `try_lock` in `render_view`, drawing the
previous snapshot when the lock is taken: under load every frame would show
stale content, which is the stutter #83 is about, and what to draw belongs
to the frame pacing of #21. A simpler reader-only variant, yielding after
every 4096 bytes whether or not anyone waits, met the lock bar too. It was
dropped because on a loaded machine each yield can hand the core to another
process: its median wall time was up to 1.5× (load 20) and 3.7× (load 27)
that of main, with the instruction count unchanged.

Measured with `session::tests::reader_lock_hold_is_bounded` (release, Apple
M4, a machine already loaded by other builds; load averages in brackets):
50 MiB of the `sgr_heavy` and `cjk` corpus lines in 64 KiB chunks (the
largest the native PTY reader delivers) and in 1 KiB chunks (what a macOS
PTY delivers), while a probe takes the lock through `with_terminal`, the way
`render_view` does, every 200 µs.

| | 64 KiB chunks: p99 / max | 1 KiB chunks: p99 / max |
|---|---|---|
| before: whole chunk, never gives way [11–22] | 170–330 ms / 170–330 ms (2–5 samples) | 15–47 ms / 20–47 ms |
| slices, never gives way [11–22] | 24–90 ms / 27–205 ms | 1.1–5.7 ms / 2–29 ms |
| yield every 4096 bytes, whole chunk [20] | 1.7–1.9 ms / 2.7–3.1 ms | 0.18 ms / 0.27–0.34 ms |
| slices, yield every 4096 bytes (rejected) [11–22] | 0.1–1.0 ms / 0.3–1.7 ms | 0.05–0.6 ms / 0.1–1.4 ms |
| **slices of 4096, give way to a counted waiter** [8–17] | **0.03–0.37 ms / 0.03–1.7 ms** | **0.01–0.07 ms / 0.01–0.83 ms** |

2048-byte slices measured about the same, and 8192 and 16384 were somewhat
worse at the 99th percentile; 4096 leaves margin under the bar (p99 2 ms,
max 5 ms) on a slower machine. On an oversubscribed machine (ten extra
CPU-bound processes, load 13–39) the 99th percentile stayed under 0.9 ms,
but single waits of 5–53 ms appeared: the scheduler preempts the reader
inside a slice, or the probe itself, and no reader-side change prevents
that. That is why the test gives each case three attempts.

Throughput, as instructions retired head/main (`cargo xtask ab`, best of 15
interleaved pairs): `cat` of 16 MiB of each of the 11 wave 5 corpus lines
through a real PTY and `App` is 0.998–1.005, and the same bytes through
`Terminal::advance` alone are 1.000 (`tessera-term` is unchanged). Through a
fake PTY in 64 KiB chunks (a temporary test binary at main and at this
change, `time -l`) every line is 0.999–1.000: with nobody waiting the reader
adds one relaxed load per slice. With a thread taking the lock every 8 ms
like a frame, in 64 KiB and in 1 KiB chunks, instructions are 0.991–1.018
of main and median wall time 0.95–1.02. The rejected yield every 4096 bytes
also kept instructions within 1.02, but its median wall time was 1.5–3.7
times main's at load 20–27.

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
- Device loss (#84). `Gpu` registers a `wgpu` device-lost callback that
  sets a flag shared by every clone; `Gpu::is_lost` is one atomic load,
  and `Gpu::capture_errors` reports `RenderError::DeviceLost` once it is
  set (work on a lost device raises no validation error, it does
  nothing). `wgpu` runs the callback when a call fails inside `wgpu-hal`
  with a device error, during that call; nothing probes the device per
  frame. `App::render_view` detaches a view whose GPU is lost, checking
  before the frame (so an idle view on a dead device is detached by its
  next render instead of showing its last frame forever) and after a
  failed one, and returns `Error::Gpu(DeviceLost)`, `TESSERA_ERROR_GPU`
  through the FFI: the host re-attaches, as after `SurfaceLost`. The view
  is dropped whole, so its renderer, atlas, glyph caches, instance-buffer
  mirror, snapshot, damage and pacing state (ADR 0016) are all rebuilt
  by `View::new` on the next attach, whose first frame draws every row.
  An attach whose cached GPU is lost brings up a new one (fonts are kept:
  they are CPU-side), and the last view to detach drops the cached GPU,
  so an external GPU that went away is never reused. Views still on the
  dead GPU keep their clone until their own next render detaches them;
  after that the host gets `TESSERA_ERROR_NO_SURFACE`, never a loop of
  GPU errors. Every removal (`detach_view`, a detaching render,
  `close_session`, an attach that lost a race with a close) goes through
  `App::remove_view`, so the last one always drops the cache; `App`'s
  drop releases it after every view. The last clone of a `Gpu` (dropping
  its queue waits for the GPU to go idle) is dropped after the view and
  GPU locks are released, never under them.

  Feature `test-hooks` of `tessera-render` adds `Gpu::lose_device_for_test`
  (`wgpu::Device::destroy`, then polls until `wgpu` runs the callback; a
  destroyed device is lost only at the next poll, so without the polling
  the flag would not be set; after a second the hook sets it itself and
  logs that). App-core's `test-hooks` enables it and adds
  `App::lose_gpu_for_test`; the release `tessera-ffi` build does not. The
  hook must run on the thread that draws: `wgpu-core` 30.0.1 deadlocks
  when a poll releases a lost device's resources (tracker lock, then
  snatch lock) while another thread writes to the queue (snatch lock,
  then tracker lock), which a first version of the tests hit by losing
  the device from a second thread. Production draws and polls only on the
  main thread, so it cannot. The tests also lose a device through
  `wgpu-hal`'s own error path, by filling Metal's command queue (4096
  command buffers) so the next frame's own command buffer fails inside
  the frame.

  **Real hardware loss is not covered.** `wgpu-hal`'s Metal backend
  (30.0.1) reports `DeviceError::Lost` in only three places: its own
  counter of command buffers created but not submitted reaching 4096,
  a nil internal wait or signal command buffer at submit, and a fence
  wait with no pending command buffer. Elsewhere it unwraps a nil
  `commandBuffer` (`begin_encoding`, `wait_for_idle`), and it never
  checks a command buffer's completion error (it treats
  `MTLCommandBufferStatus::Error` like `Completed`). So an unplugged
  external GPU (device removed) probably never sets the flag, or panics
  inside `wgpu`, which the FFI reports as `TESSERA_ERROR_PANIC`. Handling
  real removal is a follow-up: the frontend observes
  `MTLCopyAllDevicesWithObserver` / `MTLDeviceWasRemovedNotification` and
  detaches the affected views, after which the last detach drops the
  `Gpu` and the next attach brings up a new one through this path.
- `App` stays `Send + Sync` (`View` is, through `Renderer` and
  `RenderSurface`), checked by the existing static assertion.
- The release build of `tessera-ffi` (a gate step) now compiles `wgpu` and
  `cosmic-text`, and so takes longer.
- The surface tests attach real `CAMetalLayer`s; a layer outside any
  window may not vend drawables, so frames may be `SKIPPED`. Both Rust and
  C suites skip their GPU parts when `TESSERA_SKIP_GPU_TESTS` is set.

## Amendment: #90, reading a surface's metrics back

The frontend has to turn pointer positions into cells (mouse reporting
#12, selection #30), put the input method's candidate window under the
cursor, and step font zoom (#29) from the current size. None of that was
readable through the C API, so `TerminalSurfaceView` used cell 0,0 for the
pointer and the grid's top-left corner for the candidate window.

**One call.** `tessera_surface_metrics(app, id, TesseraSurfaceMetrics *out)`
fills a `#[repr(C)]` struct, in this field order: `width_px`, `height_px`
(`u32`, the surface after clamping to the largest texture), `scale`
(`f32`), `cell_width_px`, `cell_height_px` (`u32`), `baseline_px` (`f32`),
`padding_px` (`u32`), `rows`, `cols` (`u16`), `font_size_pt` (`f32`),
`cursor_row`, `cursor_col` (`u16`), `cursor_on_screen`, `cursor_wide`
(`bool`). That is 44 bytes, aligned to 4. A Rust test pins the size and
every offset; the `_Static_assert`s in `tests/c/surface.c` pin the
generated header (its size and two offsets), and a `MemoryLayout` test in
`TesseraKitTests` pins the struct as Swift imports it, which
`TesseraSurface.Metrics` mirrors field by field. Five separate getters were rejected because
the Swift wrapper would take the locks five times per read. The font size
is in the struct because every consumer that needs cells also zooms.

- `padding_px` is `round(theme.padding × renderer scale)`, where the
  renderer's scale is `scale` capped at `MAX_SCALE` (8): the same value
  and rounding as the renderer's `Layout::new` and `grid_size_for`
  (`View::padding_px`). At a backing scale of 10 it is 32, not 40.
  `scale` itself is reported as attached or resized, raised to at least
  0.25 and not capped. Cell `(row, col)` starts at
  `(padding_px + col × cell_width_px, padding_px + row × cell_height_px)`
  from the surface's top-left corner.
- `rows` and `cols` are the **session's grid**, not the surface's
  `grid_size_for`. They are the same unless `tessera_session_resize` was
  called on a session with a surface, which this ADR discourages; then
  they report what the cursor and the mouse are clamped to.
- `cursor_row` and `cursor_col` are the cursor at scroll offset 0, inside
  the grid. `cursor_on_screen` is `false` while the view is scrolled back.
  The core reports it, so the frontend does no scroll arithmetic. Once #8
  tracks DECTCEM, a hidden cursor will also report `false`.
- `cursor_wide` is `true` on the left half of a two-column character: the
  caret spans two columns. On the right half (the spacer) it is `false`,
  although the renderer draws the caret over both halves there.
- Errors are `TESSERA_ERROR_NULL_POINTER` for a `NULL` `app` or `out`,
  `TESSERA_ERROR_SESSION_NOT_FOUND` and `TESSERA_ERROR_NO_SURFACE`. `*out`
  is written once, on success only.

**Locks.** `App::view_metrics` takes the session table, then the view table,
releases them, and then takes the session's terminal: once through
`scroll_offset` (with the input state after it) and once to read the grid
size and the cursor cell. It never holds the view table and the terminal
together, so it adds no edge to the lock order. `render_view` takes the
view table and then the terminal; `Grid::resize` and the PTY resize take
theirs one after the other (ADR 0009). The call draws nothing and waits
for no GPU work of its own. It does wait for the view table, though, and
`render_view` holds that table while it draws and waits for a drawable.
Like every `tessera_surface_*` call it is main thread only, and the main
thread serialises it with rendering. A render in flight on another thread
breaks that rule, and this call would wait for it. Bounding that case too
would need a metrics copy outside the view table that every attach,
resize, font change, detach and close path keeps up to date; it was left
out because the main-thread rule already excludes the case. The tests
check that the view table is free while the call waits for
the terminal (`view_metrics_never_hold_the_view_table_while_waiting_for_the_terminal`)
and that under a flood of child output the 99th percentile of 200 calls,
paced 1 ms apart, stays under 2 ms (`view_metrics_are_fast_while_output_floods_the_terminal`).

**The frontend caches the metrics.** Each read locks the session's
terminal, which is subject to the pacing rule above. `TerminalSurfaceView`
therefore reads the metrics on attach, after every resize the core accepts
and at the start of every `render()`, and keeps them in `metrics`. Pointer
events (`cell(at:)`) and the input method (`firstRect(forCharacterRange:)`)
use that copy, so a mouse event never calls into the core. `cell(at:)`
divides by `metrics.scale` and clamps the padding band and anything past the
grid to the nearest edge cell. `firstRect` returns the cursor cell, two
columns wide when `cursor_wide`, and falls back to the view's top-left
corner only while no metrics are known (no surface). The copy is `nil` before the first attach and
after the view tears its surface down (window closed, view moved out of
it). When the core drops the surface (a lost
swapchain, reported by that frame as `TESSERA_ERROR_GPU`, or a closed
session), the wrapper still counts it as attached, so the next display
tick renders again. That render's read reports no surface and clears the
copy, and the copy stays `nil` until a re-attach (the display link does
not render while detached). In between, for at most one tick, the copy
holds the last metrics. A
change seen at a read while text is marked moves the composition
overlay and invalidates the input context's character coordinates, so
the candidate window follows the cursor. The copy can be up to
one frame old: output that arrived since the last render has not moved it
yet. That is the frame the user is looking at. `TesseraSurface.cellMetrics`
and `cursorCell` read the core on each access and are meant for callers
outside the view.
