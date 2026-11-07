# ADR 0040: Settings write-back and the config FFI (amends 0004 by reference)

**Status:** accepted · 2026-11-07

## Context

ADR 0004 makes the Preferences window the primary way to configure Tessera
and has it "write back through the same types, preserving comments via
`toml_edit`". Until now nothing wrote `config.toml` but the commented
default (`write_default_if_missing`): every setting was a hand edit. Issue
#40 builds the Preferences window; #40a, this decision, is its Rust half: an
editor for `config.toml` and the C exports the Swift window (#40b) and its
Restore Defaults (#40c) call. No Swift is part of it.

The file is the user's. It may be hand-edited with comments, kept in a
dotfiles repository behind a symlink, open in an editor while Preferences
saves, or broken. A write must never corrupt it, lose a comment, or
overwrite an edit the user made since it was read; a value the app would
then warn about (and ignore or clamp) must never be written.

## Decision

**`tessera_config::edit::ConfigEditor`** over `toml_edit` 0.25 (a new
workspace dependency). `open` reads the file (following symlinks; at most
16 MiB, `MAX_CONFIG_FILE_LEN`), `set`/`remove`/`add_profile` change one key
path at a time in memory, `save` writes it back.

- **Key paths** are keys joined by `.`, bare or double-quoted, each with
  optional `[N]` indices: `appearance.opacity`,
  `profiles[1].appearance.theme`, `keys."cmd+d"`. Capped at 1024 bytes, 16
  steps, index 65 535 (`MAX_KEY_PATH_*`). An index one past the end
  appends, so `profiles[N].name` (N profiles) adds a profile; removing
  `profiles[i]` moves the later ones down. A set never writes a table (a
  value, or an array nested at most 32 deep); tables appear as needed.
- **Formatting.** What an edit does not touch comes out byte for byte: the
  editor refuses to open a file `toml_edit` would not write back unchanged.
  `toml_edit` always writes LF and a final newline, so the editor keeps the
  file's own endings: a file without a final newline is saved without one,
  and a file whose every line ends in CRLF is edited as LF and saved with
  CRLF. A file with mixed line endings is refused, and so is a CRLF file
  whose multi-line strings would change (the CRLF in one is part of its
  value), or an edit that would write such a string into a CRLF file.
  A replaced value keeps its line, spacing and trailing comment; a new key
  goes at the end of its table (inline and dotted tables are edited where
  they are), a new table after the existing ones, an array element on its
  own line in a multi-line array. A byte order mark stays.
- **No comment is lost with a value.** Removing a key keeps the comment
  lines above it where they were and turns a comment on its line into a
  line of its own. Removing a table or a profile takes the comments inside
  it (its header line and body) with it, as deleting that section by hand
  would; the comments above its header stay. Restore Defaults (#40c)
  therefore removes the `[keys]` entries it wrote and every unrelated
  comment survives.
- **Every edit is checked twice before it is kept.** (1) The edited text,
  read as plain TOML, must equal the old document with exactly this change
  applied, and its comments must be the old ones minus only those the
  removed lines held: a `toml_edit` bug or an editor bug becomes a refused
  edit, never a damaged file. (2) The lenient loader reads it on a copy, its
  log silenced: an edit that makes the load fail (a value of the wrong
  type) is `Invalid`, one that adds any warning the file did not have
  before (unknown key, out of range, a key binding dropped, a theme that
  does not resolve) is `Refused` with those warnings. A warning the file
  had already does not block other edits. The FFI also compiles the key
  map and refuses an edit that adds a key-map warning (an unknown command,
  or a binding honoured with a caveat).
- **Saving is atomic and never overwrites someone else's edit.** The text
  goes to a new temporary file (`create_new`, `.<name>.<pid>.<n>.edit`) in
  the target's directory, with the target's permission bits, is flushed
  (`fsync`), and renamed over the target; the directory is then flushed. A
  symlinked `config.toml` stays a symlink and its target is replaced. A
  crash leaves the old file or the new one (a temporary file may stay
  behind). Right before the rename the file is read again: if it is not
  byte for byte what was opened (an editor saved it, it was removed or
  replaced, the symlink was re-pointed), nothing is replaced and the result
  is `Conflict`. A byte comparison, not a hash: the file is small and a
  hash adds nothing. The window between that read and the rename stays (no
  portable compare-and-rename exists); it is microseconds against an
  editor's save.
- **Files it does not write.** Not a regular file (FIFO, directory), a
  second hard link (the rename would split them silently), read-only
  (the user's choice), not UTF-8, not TOML: `open` refuses, and the file is
  left for the user to fix by hand.
- **Themes.** `theme_entries` lists what `appearance.theme` can name:
  bundled themes (minus those a user file shadows, as `Theme::resolve`
  does), then up to 256 `themes/*.toml` files in name order. `import_theme`
  runs the existing importers (iTerm2 plist, base16 YAML; 1 MiB and their
  value caps) and publishes `themes/<slug>.toml` without ever replacing a
  file (temporary file and hard link). Every name handed out passes
  `display_name` (S3): C0, C1, DEL and bidirectional formatting characters
  removed, at most 128 bytes; a file whose stem would not survive it is not
  listed.

**C ABI** (`tessera-ffi/src/config.rs`; `tessera-app-core` re-exports
`tessera_config::edit` so the FFI keeps depending on app-core alone):

- `tessera_config_json`: `{"generation", "config"}`, the configuration in
  effect (two-call buffer protocol, as `tessera_app_keymap_json`).
- `tessera_config_set(key, value_json)` and `tessera_config_remove(key)`:
  one edit, saved at once. They call `App::open_config` first (create the
  directory and default file, start the watch). The value is JSON (string,
  number, boolean, array; `null`, objects and integers past `i64` refused;
  at most 64 KiB). Two new status codes: `TESSERA_ERROR_REFUSED` (9) and
  `TESSERA_ERROR_CONFLICT` (10). A message for the user goes to an
  optional buffer, cut on a character boundary.
- `tessera_theme_list` and `tessera_theme_import(path)`: the latter writes
  the new theme's id and needs `id_cap` ≥ `TESSERA_THEME_ID_LEN` (64)
  before it imports anything, so the id is never lost.

The core does not apply the write itself: the configuration watch (ADR
0006) sees the rename like a hand edit and applies it once
(`on_config_applied`). One writer path, one apply path.

**Logs** carry the function, the key path (escaped) and the kind of
failure, never a value or the loader's message, which quotes values (S8).

**Security.** Not a new attack surface: the editor writes the user's own
file with input from the user's own Preferences window; it extends
"Parsing files" in `docs/security.md` with the write rule above, and the C
ABI rules (S7) cover the exports.

## Consequences

- Hand edits and Preferences edits coexist: comments, layout and unknown
  keys survive, and a concurrent hand edit wins (the window reloads on
  `Conflict`).
- A setting the loader would warn about cannot be saved from the window;
  the user can still write it by hand.
- A file `toml_edit` would reformat, or a hard-linked or read-only one,
  cannot be edited from Preferences; the window says why and offers the
  file.
- Removing a commented key leaves its comments, which may then describe
  nothing; deleting them is the user's call.
- `profiles/*.toml` (separate profile files) are not edited yet; the
  editor works on `config.toml`'s `[[profiles]]`.
