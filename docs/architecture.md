# Architecture

Tessera is a fat Rust core with a thin native shell. This document is the
map; the ADRs record why.

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

## Crates

| Crate | Owns | May depend on |
|---|---|---|
| `tessera-pty` | spawning, PTY I/O, resize (`unsafe` allowed) | – |
| `tessera-vt` | bytes → typed `Action`s via `vte` | – |
| `tessera-grid` | cells, lines, scrollback, cursor, selection, marks | – |
| `tessera-term` | applies actions to a grid, terminal modes, side-effect requests | vt, grid |
| `tessera-shell-integration` | OSC 133 / 7 / 1337 → marks and metadata | grid |
| `tessera-search` | regex over grid + scrollback | grid |
| `tessera-render` | wgpu pipeline, glyph atlas, shaping, damage tracking, `Theme` | grid |
| `tessera-config` | TOML schema, defaults, load/save, watch | – |
| `tessera-plugin-api` | `Plugin`, `Host`, `Capability`, `Event` | – |
| `tessera-plugins/*` | first-party plugins | plugin-api |
| `tessera-protocol` | JSON-RPC request/response types | plugin-api |
| `tessera-control` | Unix socket server, consent, dispatch | protocol, app-core |
| `tessera-app-core` | everything above wired together, no OS UI | all of the above |
| `tessera-ffi` | `extern "C"` surface (`unsafe` allowed) | app-core |
| `tessera-cli` | `tessera` binary speaking the protocol | protocol |

## Invariants

1. **Downward dependencies only.** The table above is the allowed graph.
2. **No OS UI below the shell.** Nothing in `crates/` imports AppKit, winit or
   any windowing system. `tessera-render` receives a raw window handle.
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

## Data flow for one keystroke

1. `TerminalSurfaceView` receives `keyDown`, resolves IME, fills a
   `TesseraKeyEvent` and calls `tessera_session_send_key` (ADR 0011).
2. `tessera-app-core` (`App::send_input`) resolves keybindings; unbound
   keys are encoded by `tessera-term` and queued for the session's PTY
   without blocking. The returned `TesseraInputOutcome` tells the view
   whether to pass the event on to AppKit (menu key equivalents).
3. The PTY reader thread receives child output, `tessera-term` parses and
   applies it, emits `Request`s (bell, title) and marks dirty rows.
4. The core notifies the frontend via the `on_needs_redraw` callback.
5. The frontend's display link calls `tessera_surface_render`;
   `tessera-render` uploads dirty rows and draws.
6. Events worth observing (`CommandFinished`, `TitleChanged`) are fanned out
   to plugins and socket subscribers.

## Future frontends

A Linux or Windows frontend is a new directory under `apps/` that links
`tessera-ffi` and implements the same callback table. No crate under
`crates/` changes.
