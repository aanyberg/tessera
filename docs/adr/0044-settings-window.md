# ADR 0044: The Settings window

**Status:** accepted · 2026-11-07

## Context

ADR 0004 makes a Preferences window the primary way to configure Tessera;
ADR 0040 gives it a writer: `tessera_config_set` and `_remove` edit one key
path of `config.toml` (keeping comments, refusing a value the loader would
warn about, returning `Conflict` when the file changed since it was read),
`tessera_config_json` reads the configuration in effect with a generation,
and `tessera_theme_list` and `_import` list and add themes. Issue #40b is
the window over them: General, Appearance, Profiles and Advanced panes, with
Keys (#40c) and Plugins (#48) to follow. Until now ⌘, opened `config.toml`
in an editor.

The window must not freeze while the file is written (a few milliseconds,
more on a slow disk or a dotfiles symlink), must not write a dragged slider
sixty times a second, must survive the user editing the file by hand while
it is open, and must not let the app's view of the file drift from the
file.

## Decision

**Hosting.** AppKit hosts the window (`SettingsWindowController`): an
`NSTabViewController` in toolbar style over one `NSHostingController` per
pane, in a window with the `.preference` toolbar style. The app has no
SwiftUI `App`, so a SwiftUI `Settings` scene is not available; each pane is
a SwiftUI `Form`. There is one window: ⌘, (Tessera > Settings…) opens it or
brings it to the front, and its frame is saved under `TesseraSettings`.
Open config.toml stays in the Tessera menu (with no shortcut) and on the
Advanced pane, for the keys no pane shows. A pane is one entry in
`SettingsWindowController.panes`, so Keys (#40c) and Plugins (#48) add an
entry and a view, nothing else.

**The model.** `SettingsModel` (main actor, `@Observable`) holds the
configuration in effect (`TesseraConfigSnapshot`: `generation` and the
`config` JSON as a `TesseraConfigValue` tree) and the theme list, read when
the window opens and again from `on_config_applied`. A control reads a key
path through the model and writes one key path through it; the panes are
built from tables of `SettingsField`s (key, title, control), which the tests
drive through the same bindings the controls use. The model holds its
source (the core) weakly and the window owns the model, so closing the
window frees both; `windowWillClose` also takes the panes out of the window,
since AppKit keeps a closed key window as the previous key window. Replacing
the core (a test changing `TESSERA_CONFIG_DIR`) closes the window first.

**One write queue.** Every edit is one set or remove, run in order on one
serial background queue (`SettingsModel.writeQueue`, shared by every model,
so the order holds across a closed and reopened window), never on the main
thread. The FFI's contract says "the UI thread"; the exports take the
handle by shared reference, the watch thread already uses it concurrently,
and `CoreHandle` makes a shutdown wait for a call in progress (`call`), so a
single other thread is safe. Only this queue writes; the main thread reads.

**What a control shows.** An edit shows at once: the model lays the value
over the snapshot (`pending`) until a configuration applied after the edit
was queued (a later `generation`) and no edit of that key is queued; then
the file's value shows. The core does not apply its own write: the
configuration watch applies the saved file like a hand edit (ADR 0040), so
the window, the open tabs and a hand edit all go through one apply path.

**Coalescing.** A slider or stepper writes at once if it wrote nothing for
100 ms, else once the 100 ms have passed (with the latest value), and once
more with the final value on release: at most eleven writes for a one-second
drag, the last one the value the user let go at. Slider values snap to the
step's digits (`0.85`, not `0.8500000000000001`).

**Refused, invalid and failed edits** write nothing (the core checks before
it saves): the control shows the file's value again and the core's message
next to it, sanitised (S3: control and bidirectional formatting characters
removed, at most 400 characters). Only the error kind is logged (S8).

**Conflict.** When the file changed since the core read it, the edit is run
once more at once (each set re-reads the file, so the other edit is kept and
this one goes on top of it) and the model reloads; a second conflict shows
its message. The value the user set stays shown until a configuration
after the retry applies.

**Numbers keep their type.** Float keys are written as JSON numbers with a
fraction (`15.0`), since the loader refuses an integer for a float key.
`JSONDecoder` reads `15.0` as an integer, so `TesseraConfigValue.parse`
reads the core's JSON with `JSONSerialization` and keeps a number's float
type (`CFNumberIsFloatType`); a float the core reads as `f32` comes back
widened (`0.30000001192092896`), which the panes round for display.

**Profiles** are `[[profiles]]` in file order, edited by index
(`profiles[N].key`): Add appends a profile named "New Profile" (numbered if
taken), Duplicate copies the keys the pane edits (command, working
directory, appearance) to a new profile at the end, Rename keeps
`general.default_profile` pointing at the profile, and Delete removes the
entry, refused for the default profile. Add, Duplicate and Delete move the
indices the other edits name, so from the moment one is queued until the
watch applies the file (or the write fails, with its message by the list)
the list, its buttons and the profile's controls are disabled and the
profile actions do nothing. Each `ProfileAppearance` key has
an Inherit toggle: on removes the profile's key, so `[appearance]`'s
applies; off writes the value shown. Profile names, theme names and plugin
names are shown sanitised (`MainMenu.profileTitle`).

**Themes.** Import Theme… opens an `NSOpenPanel` for `.itermcolors` and
base16 YAML, calls `tessera_theme_import`, and selects the new theme; a file
the importer refuses writes nothing and shows the importer's message.

## Consequences

- A hand edit and a window edit can meet: the hand edit wins the conflict
  and the window's edit goes on top of it, once.
- Profiles are addressed by index. If the file's profiles are reordered by
  hand and an edit is made before the watch has applied that (0.1 to 4 s),
  the edit lands on the profile now at that index. Accepted for now: the
  window reloads within seconds, and naming profiles by name in the key
  path would need an FFI change (#40c or later). Once the watch applies a
  hand edit that adds, removes, reorders or changes any key of a profile
  (compared by every key, so profiles named alike or not named count too),
  the selected profile's controls start again from the profile now at that
  index, so text not yet committed is dropped rather than written to
  another profile. The keys the window itself set and has not yet seen
  applied are left out of the comparison, so its own edits (a rename, a
  field, an Inherit toggle, a slider) keep the controls and the focus. The
  cost: a hand edit of another key of the selected profile drops its
  uncommitted text too.
- A slider drag writes up to ten times a second, each an atomic rename. The
  watch debounces 100 ms but waits at most 1 s, so during a steady drag it
  may apply only about once a second; the tabs follow the drag at that rate
  and reach the released value about 100 ms after the last write.
- After Add, Duplicate or Delete the Profiles pane waits for the watch (0.1
  to about 1 s) before the next profile edit. A watch that never applies
  keeps the pane disabled until the window is closed and opened again,
  which starts from the file.
- `on_config_applied` reloads the model once per applied configuration, not
  per edit; nothing in the window runs per byte of terminal output.
- Keys the panes do not show are untouched by the window and reachable
  through Open config.toml.
