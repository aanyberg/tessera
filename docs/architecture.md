# Architecture

Tessera is a fat Rust core with a thin native shell (ADR 0001). This document
is the map; the ADRs record why.

## Layers

```
┌──────────────────────────── Tessera.app (Swift) ─────────────────────────────┐
│ NSWindow · tab bar · split handles · menus · Preferences (SwiftUI) · dialogs │
│ TerminalSurfaceView (NSView + CAMetalLayer) · keyboard / IME / mouse events  │
└─────────────────────────────────────┬────────────────────────────────────────┘
                                      │ C ABI: crates/tessera-ffi (cbindgen)
┌─────────────────────────────────────┴────────────────────────────────────────┐
│ tessera-app-core   windows → tabs → pane tree → sessions · commands ·        │
│                    keybindings · config store · plugin host · control server │
├──────────────┬──────────────┬──────────────┬───────────────┬─────────────────┤
│ tessera-pty  │ tessera-vt   │ tessera-grid │ tessera-render│ tessera-search  │
│ tessera-term (vt + grid + modes)  ·  tessera-shell-integration (OSC 133/7)   │
├──────────────┴──────────────┴──────────────┴───────────────┴─────────────────┤
│ tessera-config · tessera-plugin-api · tessera-protocol · tessera-control     │
└──────────────────────────────────────────────────────────────────────────────┘
```

The diagram is the target design; today the core holds sessions only
(ADR 0015).

## Crates

| Crate | Owns | May depend on |
|---|---|---|
| `tessera-pty` | spawning, PTY I/O, resize (`unsafe` allowed) | – |
| `tessera-vt` | bytes → typed `Action`s via `vte` | – |
| `tessera-grid` | cells, lines, scrollback, cursor, selection, marks, wide cells (ADR 0008), resize with soft-wrap reflow (ADR 0009), `Style` layout (ADR 0013) | – |
| `tessera-term` | applies actions to a grid, terminal modes, side-effect requests, `TERM` / terminfo (ADR 0005) | vt, grid |
| `tessera-shell-integration` | OSC 133 / 7 / 1337 → marks and metadata | grid |
| `tessera-search` | regex over grid + scrollback | grid |
| `tessera-render` | wgpu pipeline, glyph atlas, shaping (ADR 0007), four passes over a `FrameInput` (ADR 0010), `Theme`; `RenderSurface` wraps a `wgpu::Surface` it is handed | grid |
| `tessera-config` | TOML schema, defaults, load/save, watch (ADR 0004, 0006) | – |
| `tessera-plugin-api` | `Plugin`, `Host`, `Capability`, `Event` (ADR 0002) | – |
| `tessera-plugins/*` | first-party plugins | plugin-api |
| `tessera-protocol` | JSON-RPC request/response types (ADR 0003) | plugin-api |
| `tessera-control` | Unix socket server, consent, dispatch | protocol, app-core |
| `tessera-app-core` | sessions, input, and one view per session (`view.rs`: a `View` holds `tessera-render`'s `RenderSurface` and `Renderer`; `App` shares one `Gpu`, ADR 0014); no OS UI | pty, term, config, plugin-api, render, grid (and the `hello` plugin) |
| `tessera-ffi` | `extern "C"` surface, including turning a `CAMetalLayer*` into a wgpu surface (`unsafe` allowed); `apps/macos` wraps it as TesseraKit (ADR 0012) | app-core |
| `tessera-cli` | `tessera` binary speaking the protocol | protocol |
| `tessera-devtool` | dev-only `tessera-devtool` binary and library: headless screen dumps for debugging and snapshot tests; never published, nothing in the app depends on it | pty, term, grid |

## Invariants

1. **Downward dependencies only.** The table above is the allowed graph.
2. **No OS UI below the shell.** Nothing in `crates/` imports AppKit, winit or
   any windowing system. The frontend passes a `CAMetalLayer*` to
   `tessera-ffi`, which is the only crate that turns it into a wgpu surface;
   `tessera-render` and `tessera-app-core` see a `wgpu::Surface`.
3. **One host API.** Built-in plugins, WASM plugins (v2) and socket clients
   all go through `tessera-plugin-api`. If a feature cannot be expressed
   there, the API grows; a side channel is never added.
4. **Serializable boundaries.** Every type in `plugin-api` and `protocol`
   derives `serde`. This is what makes WASM, the socket and the MCP adapter
   adapters rather than redesigns.
5. **Panics stop at the FFI.** Every exported function in `tessera-ffi` is
   wrapped in `catch_unwind` and returns an error code.
6. **`unsafe` lives in two crates**, `tessera-pty` and `tessera-ffi`, and every
   block has a `SAFETY:` comment.

## Views and the frontend surface

A view is one session's on-screen presence, owned by `tessera-app-core`
(ADR 0014): the `RenderSurface` on the frontend's layer, the `Renderer` that
draws into it, the font size and a snapshot buffer. `App` holds at most one
per session; the process shares one `Gpu` and one font database, created by
the first attach.

1. The Swift view owns a `CAMetalLayer` and passes it to
   `tessera_surface_attach` with the size in pixels and the backing scale.
2. `tessera-ffi` (`surface.rs`) checks that the object is a `CAMetalLayer`
   and creates the surface with
   `SurfaceTargetUnsafe::CoreAnimationLayer`. `raw-window-handle` is not
   used: its AppKit handle names an `NSView`, not a layer.
3. `App::attach_view` configures the surface and the renderer. The view's
   size, divided into whole cells, decides the session's grid and PTY size,
   so `tessera_surface_resize` and font-size changes resize the session
   (and reflow its grid, ADR 0009).
4. `tessera_surface_render` draws a frame; `tessera_surface_detach`,
   `tessera_session_close` and `tessera_app_free` release the view.

Every `tessera_surface_*` call is main thread only. The Swift view that
makes these calls is not in the tree yet (issue #27).

Windows and tabs are not in the core yet. In 0.1.0 each tab is an `NSWindow`
with one session, grouped by AppKit's tab bar, and the registry lives in the
Swift shell; the core keeps only sessions (ADR 0015).

## Threading

Each session has a reader thread, which applies child output to the terminal
under its lock, and a writer thread for queued input. Rendering must not
delay the reader: `render_view` holds the terminal lock only to copy the
visible rows into the view's snapshot buffer and draws after releasing it.
Presenting can block on a drawable, so drawing under the lock would stall
output without a bound; ADR 0014 has the measurements. `Grid::resize` and
the PTY resize take their locks one after the other, never together
(ADR 0009).

## Data flow for one keystroke

1. `TerminalSurfaceView` receives `keyDown`, resolves IME, fills a
   `TesseraKeyEvent` and calls `tessera_session_send_key` (ADR 0011).
2. `tessera-app-core` (`App::send_input`) resolves keybindings; unbound
   keys are encoded by `tessera-term` and queued for the session's PTY
   without blocking. The returned `TesseraInputOutcome` tells the view
   whether to pass the event on to AppKit (menu key equivalents).
3. The PTY reader thread receives child output, `tessera-term` parses and
   applies it, emits `Request`s (bell, title).
4. The core notifies the frontend via the `on_needs_redraw` callback.
5. The frontend's display link calls `tessera_surface_render`, which runs
   `App::render_view`: it copies the visible rows under the terminal lock,
   releases it, and draws the copy with `tessera-render` (ADR 0014).
6. Events worth observing (`CommandFinished`, `TitleChanged`) are fanned out
   to plugins and socket subscribers.

## Future frontends

A Linux or Windows frontend is a new directory under `apps/` that links
`tessera-ffi` and implements the same callback table. No crate under
`crates/` changes.
