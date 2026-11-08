# ADR 0036: Configurable key bindings

**Status:** accepted · 2026-11-06

## Context

Issue #43 asks for user key bindings: a `[keys]` table in `config.toml`
that rebinds the app's commands and binds chords to text, reloaded live
like the rest of the configuration (ADR 0006). Until now every shortcut
was a key equivalent written into `MainMenu.swift`, the core bound no key,
and Command keys never reached the core (`processKeyDown` sent them up the
responder chain). Later wave-13 features (panes, find, prompt navigation,
copy mode, the Keys pane of the Settings window) need commands a key can
reach before their menus exist.

The behaviour is pinned by the spec-researcher's fixtures: a 56-row
default-chord table (`crates/tessera-app-core/tests/fixtures/default-chords.csv`:
every shipped shortcut, its AppKit key equivalent and mask, iTerm2's and
Terminal.app's), and 121 chord-grammar cases, rules R1 to R13
(`crates/tessera-config/tests/fixtures/keys-grammar-cases.toml`). The lead
adopted the researcher's recommendations N1 to N12 and the plan's D3, D4
and D17.

## Decision

**Notation** (`tessera_config::keys`). Each entry is a quoted TOML key, a
chord, and a string value:

```toml
[keys]
"cmd+u" = "new_tab"
"ctrl+shift+t" = "select_tab:3"
"cmd+k" = "send_text:clear\n"
"cmd+q" = "none"
"cmd+shift+d" = "unbound"
```

A chord is modifiers and one key joined with `+`, written in any order and
stored canonically (`cmd+ctrl+alt+shift`, lowercase, the order
`Chord`'s `Display` prints). Modifier aliases are
`cmd`/`super`/`command`, `alt`/`opt`/`option`, `ctrl`/`control`, `shift`.
Key names follow kitty (`page_up`, `left_bracket`, `plus`, `f1` to `f35`,
`kp_0`), N7. A shifted symbol is the unshifted key plus `shift`
(`cmd+shift+[`); the US-shifted literal (`cmd+{`, `cmd+?`) is sugar for it
and matches on any layout that types the symbol (N4). Key sequences and
leader keys are rejected with a warning ("not supported yet").

A value is a command (`new_tab`), a command with its parameter
(`select_tab:3`), `send_text:` with escaped bytes (`\e`, `\n`, `\t`, `\r`,
`\\`, `\xHH`, `\u{H…}`, rule R13), `unbound` (the chord loses what it did
by default and the key goes on as unbound keys do) or `none` (the key is
swallowed) (N9).

**One key map, owned by the core** (D3). `tessera-app-core::command` holds
the registry, `COMMANDS`: every command a key can be bound to, its
parameter, its default chords (the menu's shortcut first, hidden
alternates after, N2) and its owner (`system` for AppKit's own items such
as Quit and Hide, `app`, `planned` for commands a later task runs, which
may be bound now). `Keymap::compile` lays `[keys]` over the defaults: an
entry whose chord is another command's default takes it from that
command, a command keeps the defaults no entry took, and within the table
the first entry for a chord wins. Entries are additive: a command's own
entries come first, in document order, and its old default stays bound
as a hidden alternate unless the table also says `"<chord>" = "unbound"`
(`"cmd+u" = "new_tab"` alone leaves ⌘T opening a tab too). The compiled map is cached against the
configuration it came from and swapped as a whole when one applies.

**Warnings, never errors.** An entry that breaks the grammar or the caps,
names no command, or repeats a chord is left out with one warning naming
its key; the rest of the table applies. An entry is honoured with a
warning when it takes a chord macOS uses (⌃⌘F, ⌃⌘Space, ⌃Tab, ⌃⇧Tab,
⇧⌘\\), the default chord of a `system` command (N10), replaces a menu
command's chord with text or `none`, or shadows plain typing (a chord
without Command, Control or a function key, N8). Warnings are escaped and
bounded (docs/security.md S3, S5) and listed with the key map.

**Dispatch** (N11, N12). Keys reach the core first:
`tessera_session_handle_key` resolves the chord and returns `COMMAND` with
the command's id (nothing written), `CONSUMED` for `send_text` and `none`
(and for an unbound key, encoded as before), or `IGNORED`. With
`TESSERA_HANDLE_KEY_BINDINGS_ONLY` an unbound key is `IGNORED` and nothing
is sent. In the app:

* `performKeyEquivalent(with:)` of the first-responder terminal view sees
  every Command key before the menus. It asks with bindings only: an
  unbound key goes on to the menus (Cut, Undo and AppKit's own items keep
  working); `send_text` and `none` are consumed there (so ⌘ chords can
  send text, the question D17 left open); a command with a menu item
  returns `false` so `AppKit` runs the item, which `validateMenuItem`
  still gates; a command without an item (a `planned` one) is swallowed
  rather than reaching the shell as `CSI u`. Until their menus land, the
  planned chords (⌘D, ⌘⇧D, ⌘F, ⌘G and the like) therefore do nothing where
  they used to beep.
* `keyDown` runs every other chord: a command runs its menu item
  (`MainMenu.run(command:)`, validated first, a beep when disabled), so a
  chord without Command (`ctrl+shift+t`) can open a tab without being a
  menu key equivalent. A bound Option chord runs before the input system
  composes the key (`alt+t` would otherwise type †).

**Menus show the map** (D3, N12). Every menu item carries its command's
name in `NSMenuItem.identifier`. At start and on every `on_config_applied`
(a new last field of `TesseraCallbacks`, coalesced like
`on_appearance_changed`) the app reads `tessera_app_keymap_json` and sets
each item's key equivalent to its command's first Command chord, with
hidden alternates (`allowsKeyEquivalentWhenHidden`) for the others; an
item whose command has none shows no shortcut. Chords without Command
never go on a menu: menu key equivalents run before `keyDown` and would
take ⌃C, F5 or ⌥B from the shell. The defaults reproduce the shipped
shortcuts exactly (the fixture's first test).

**Fast path.** `Keymap::resolve` allocates nothing: an atomic mask of the
modifier sets any chord uses rejects most keys (plain typing, Shift
alone) with one load, and a bitmap of the ASCII keys the chords use with
each modifier set (32 atomic words) rejects unbound Command letters (⌘J),
both before the map's lock is taken; otherwise a binary search over a
sorted table, at most three probes (the key, the shifted character
without Shift, and for a key beyond ASCII the base-layout key, as
tessera-term's legacy encoding does: ⌘Т on a Cyrillic layout is ⌘T, but a
Latin layout's own key is never replaced, so French ⌘Z is not ⌘W).
Unbound keys encode exactly as
before, and the key encode path stays within 5% of main (`cargo xtask
ab`, ASCII and modifier chords; without the bitmap unbound Command
chords measured 1.052).

**Bounds** (docs/security.md S5). At most 512 entries
(`MAX_KEY_BINDINGS`), a chord of 64 bytes, a command name or parameter
of 64 bytes, a `send_text` value of 4096 bytes after unescaping; one
warning per entry, plus one for the entries past the cap. `send_text` is
the user's own bytes and is written as is: it is not a paste and is not
sanitised (S3 covers what the core displays, not what the user types).

## Consequences

* Users rebind or unbind any shortcut, Quit included, and the menus show
  the result without a restart; a broken entry costs only itself.
* Later tasks register their commands and default chords in `COMMANDS`
  and add a menu item with the command's identifier; until then the chord
  is bound and does nothing.
* The Keys pane (#40c) reads the same JSON (commands, chords, owners,
  warnings) and writes `[keys]` back in document order, which the table
  keeps for that reason.
* AppKit's own alternates (Close All, ⌥⌘W, on Close Tab) are AppKit's and
  keep their chords when Close Tab is rebound.
* Known limitation: on a layout whose own key is not ASCII, the key runs
  the command of the US key in its place (the physical-key fallback that
  makes Russian ⌘Т New Tab), so Czech ⌘ě is Tab 2 and Lithuanian ⌘ą is
  Quit; a user who does not want that binds the chord to `unbound`.

## Not done

* **Leader keys and key sequences** (`ctrl+a>c`, tmux style) need a
  pending-prefix state per session and a timeout, and a way for the menus
  to show them. Out of scope (D3); the grammar rejects `>` with a warning
  so a later version can give it meaning without breaking a config.
* Mode-dependent bindings (only on the alternate screen, only in copy
  mode) wait for copy mode (#47).

## Amendment: #317 (2026-11-08)

The Rust half of the follow-ups #317 lists; the Keys pane's own items stay
with #40c.

* **Invisible keys are refused.** A chord whose key is a bidi control, a
  zero-width character or any other `Default_Ignorable_Code_Point` (U+00AD,
  U+200B to U+200F, U+202A to U+202E, U+2060 to U+206F, U+FEFF, the
  variation selectors and tags, and the rest of the Unicode property) is
  left out with an `invisible key` warning naming the code point: a
  binding the user cannot see in the file or in the Keys pane is a binding
  nobody can audit. Combining marks and visible non-ASCII keys (`cmd+é`,
  `cmd+т`) still bind.
* **Warnings escape what they echo** (S3). The unknown-key segment, the
  unknown command or parameter and the entry key a warning names are
  escaped as the loader escapes paths (C0 and C1 controls, U+2028/9, bidi
  controls) and, stricter than the loader, every default-ignorable
  character as `\u{…}`, then cut to `MAX_WARNING_PATH_LEN` bytes. Text a
  warning shows can no longer reorder or hide the rest of the line. The
  key path `tessera_config_set` echoes in front of a refusal is the
  caller's own text and is not the key map's to escape.
* **The source table is dropped after load** (S5). `Keys` kept every raw
  `[keys]` entry it read (up to the 1 MiB file cap) to put the bindings in
  document order; it now builds from the entries once and keeps only the
  at most 512 bindings and their warnings.
* **The map and its generation are read together.** `App::keymap_and_generation`
  takes both under the map lock (applies bump the generation under it), and
  `tessera_app_keymap_json` builds its JSON from that pair: the chords in
  one JSON are always those of its `generation`. Reading them one after the
  other tore about one read in 180 000 under a storm of applies.
* **`Keymap::default()` is the defaults** (the map of an empty `[keys]`
  table), with chords for every command; it used to be empty and
  `chords` panicked on it. `chords` no longer indexes.
* **No re-parsing per key.** The registry's default chords are parsed once
  (a `LazyLock`); `is_default` is a binary search over the defaults no
  entry names, compiled with the map, and the ⌘↑ / ⌘↓ prompt rule reads the
  parsed default. Neither allocates.
* **A binding runs after the map lock is released.** Dispatch takes the
  map's `Arc` under the lock and resolves and runs the binding on it
  unlocked, so a `send_text` write (or anything else a binding does) that
  reaches the key map again cannot deadlock, and an apply never waits on
  a write to the PTY.
