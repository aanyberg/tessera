# ADR 0004: GUI preferences first, TOML on disk

**Status:** accepted · 2026-10-27

## Context

iTerm2's Preferences window is a large part of why it is approachable;
WezTerm's Lua config is a large part of why it is not. We want both GUI
editing and a diffable, shareable text format.

## Decision

Configuration is plain Rust types in `tessera-config` with `serde`
defaults for every field. On disk: `config.toml`, `profiles/*.toml`,
`themes/*.toml` under the app support directory. The Preferences window is
the primary surface and writes back through the same types, preserving
comments via `toml_edit`. A file watcher reloads external edits (backend
choice in ADR 0006). Unknown keys warn and are preserved; they never fail a
load, and a file that does not parse falls back to the defaults with the
error reported rather than preventing launch.

## Consequences

- Themes are standalone files users can share; an `.itermcolors` importer
  is planned for v1.x.
- Scripting-style dynamic config is out of scope for the core; a plugin can
  provide it later.
- Schema documentation can be generated from the same types.
