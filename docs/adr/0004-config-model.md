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

## Amendment 2026-10-29: the startup load path (#100)

The application reads its configuration at start: `tessera_app_new`
(`App::with_config_dir`) reads `config.toml` in
`~/Library/Application Support/Tessera`, or in the directory the
`TESSERA_CONFIG_DIR` environment variable names (unset by default; for
tests and side-by-side installs), creates the `App` from it, resolving
`appearance.theme` against `themes/`, and starts the `App`'s configuration
watch (ADR 0006) when the directory exists. A missing or invalid
`config.toml` gives the defaults with a logged warning and is never an
error from `tessera_app_new`, so a broken configuration never prevents
launch.

**Starting writes nothing.** The read goes through an in-memory store
(`ConfigStore::in_memory` + `reload`), not `ConfigStore::open`, which
creates the directory and the default file. A directory that does not
exist yet is looked for every 2 s (one `stat`, so an idle application
barely wakes up) and watched, its configuration applied, once it appears
(ADR 0006). The commented default `config.toml` is created when the user
first asks to edit the configuration: `tessera_app_create_config`
(`App::open_config`, TesseraKit `createConfig()`), which the app's Open
Config menu item (#29) and Preferences (#40) are to call before opening
the file; neither exists yet, so today nothing in the shipped app creates
the file. It also starts the watch at once and applies a `config.toml`
the user made by hand. One thing does create the directory without being
asked: shell integration (#44) writes its scripts under
`~/Library/Application Support/Tessera/shell-integration` at the first
session's spawn; the directory poll then starts the watch (there is still
no `config.toml`, and none is written). The default file is published
without ever replacing one: written to a temporary file and hard-linked
into place, so a file the user saves meanwhile wins. This replaces "writes defaults on first launch" in
this ADR's original decision. Test runs (`cargo` integration tests,
`scripts/ffi-c-test.sh`, `just app-test`) point `TESSERA_CONFIG_DIR` at a
scratch directory so they never read the user's.
