# ADR 0002: One plugin API, built-in loader now, WASM loader later

**Status:** accepted · 2026-10-27

## Context

Plugins must support long-running integrations (an agent-usage tracker for
Claude Code, Codex, Copilot), UI contributions (status items, panels,
commands) and eventually third-party authors. Options: Rust `cdylib`,
compiled-in Rust crates, WASM components, out-of-process only, Lua.

## Decision

A single `Plugin` trait and capability-gated `Host` in `tessera-plugin-api`.
Everything crossing it is `serde`-serializable; plugins never draw.

- v1: `BuiltinLoader` links first-party plugins as workspace crates behind
  feature flags, toggled from Preferences.
- v2: `WasmLoader` (wasmtime, WIT generated from the same types) for
  third-party and user-installed plugins.
- `cdylib` is rejected permanently.

## Consequences

- First-party plugins are type-safe, zero-overhead and ship in the binary.
- The serialization constraint is paid up front and makes WASM and the
  control socket thin adapters.
- `cdylib` rejected because Rust has no stable ABI, a plugin panic kills the
  app, and a notarized app with hardened runtime cannot load third-party
  dylibs without weakening library validation.
- Lua rejected as a plugin runtime; it may appear later as a WASM plugin
  that provides scripting.
