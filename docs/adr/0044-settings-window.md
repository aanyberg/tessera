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
applies; off writes the value shown. Profile names and theme names are
shown sanitised (`MainMenu.profileTitle`); plugin text as the Plugins pane
amendment says.

**Themes.** Import Theme… opens an `NSOpenPanel` for `.itermcolors` and
base16 YAML, calls `tessera_theme_import`, and selects the new theme; a file
the importer refuses writes nothing and shows the importer's message.

## Consequences

- A hand edit and a window edit can meet: the hand edit wins the conflict
  and the window's edit goes on top of it, once.
- Profiles are addressed by index. If the file's profiles are reordered by
  hand and an edit is made before the watch has applied that (0.1 to about
  1 s: it debounces 100 ms and waits at most 1 s),
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
  which starts from the configuration the core has applied.
- `on_config_applied` reloads the model once per applied configuration, not
  per edit; nothing in the window runs per byte of terminal output.
- Keys the panes do not show are untouched by the window and reachable
  through Open config.toml.

## Amendment: the Keys pane, #40c (2026-11-08)

The Keys pane (`KeysPane.swift`, `KeysModel`) lists every command of the
core's registry (ADR 0036) with its chords, grouped by the menu its item is
in, and records, clears and restores shortcuts by writing `[keys]` one key
at a time.

* **What it reads.** The key map comes from `tessera_app_keymap_json`
  (commands, owners, chords with the user's entries first, warnings, and the
  generation it was compiled at); `[keys]` as written comes from the
  configuration snapshot. The two are read one after the other and can tear
  (#348), so the model reads both again (up to three times) until their
  generations agree, and the pane edits nothing from a torn pair ("config.toml
  is being applied; try again."). Both are read again on every
  `on_config_applied`, as the rest of the window is.
* **Recording.** Clicking a shortcut records the next key pressed in the
  Settings window through a local event monitor (`NSEvent`'s, behind
  `KeyEventMonitoring` for the tests), installed only while recording.
  Modifiers alone are shown and wait for a key; Escape alone cancels; Delete
  or Forward Delete alone clears the command's shortcut; any other key ends
  the recording. Only key events of the Settings window are taken; every
  other passes untouched. The monitor and its window observers go when a key
  is recorded, when the window resigns key or closes, when the pane goes
  (another tab), and at the latest when the model is freed.
* **Naming a key.** A pressed key is named by the chord the core's key map
  resolves for the event the terminal view sends it, in canonical form
  (`cmd+ctrl+alt+shift+key`). The terminal view's named keys (F1 to F35,
  keypad, arrows) and Space are named as such. A character key is named by
  the first of the core's lookups, in the core's order, that a chord can
  fill: the key's own character (lowercased) with the held modifiers; with
  Shift, the character Shift types without Shift; for a key whose own
  character is beyond ASCII, the US-layout key at its place (Russian ⌘Т is
  `cmd+t`, Swedish ⌘å is `cmd+[`). Only a visible ASCII key fills a lookup,
  and a symbol the US layout types with Shift is written as its key plus
  Shift (`?` is `shift+/`), so it fills only a lookup without Shift.
  Swedish ⇧⌘+ (typing `?`) is `cmd+shift+/`, French ⇧⌘& (typing `1`) is
  `cmd+1`, German ⇧⌘+ (typing `*`) is `cmd+shift+8`. Since the pane writes
  no chord on a character beyond ASCII, a lookup of one holds none of its
  chords and a later lookup is the one that runs. A key no lookup can fill
  is refused ("This key cannot be part of a shortcut."), nothing written:
  a dead key, the ISO § and JIS keys when neither their own nor their Shift
  character is ASCII (⌘§), and Shift with a key whose own character is a
  US shifted symbol and whose Shift character is beyond ASCII (French ⇧⌘)
  typing `°`). A hand-written chord on a character beyond ASCII can still
  take a key before the name the pane gave it (the next limit). The core
  still parses and checks every chord the pane writes.
* **Conflicts.** The core refuses an edit that adds a key-map warning (a
  system chord, a reserved chord, a chord that shadows typing, a duplicate),
  but an entry on another app command's default chord silently takes it. So
  before writing, the pane looks the chord up in the key map: a chord another
  command has, or an entry that sends text or is `none`, is shown inline by
  the row ("⌘W is the shortcut of “Close Tab”.") and nothing is written until
  the user chooses Replace; Cancel keeps it. A chord the core refuses shows
  the core's message, sanitised.
* **What recording writes.** The recorded chord becomes the command's only
  shortcut: `keys."<chord>" = "<command>"` (unless already so, or the chord
  is one of its own defaults), the command's other entries removed, and each
  of its remaining defaults set to `"unbound"` (`"cmd+shift+e" =
  "clear_scrollback"` and `"cmd+k" = "unbound"`). Clearing removes the
  command's entries and unbinds its defaults. Replacing writes over another
  command's entry, or takes its default (the entry wins, ADR 0036).
* **Restore Defaults** (after a confirmation) removes, one key at a time,
  each `[keys]` entry that names a command or is `unbound`; entries that
  send text or are `none`, comments and other tables stay. The pane cannot
  tell its own entries from hand-written ones, so a hand-written command
  binding goes too; the dialog says so.
* **One edit at a time.** An edit is a run of sets and removes on
  `SettingsModel.writeQueue`, in order, stopping at the first that fails
  (its message by the row); a conflict is run once more and the model
  reloads, as the other panes do. From the moment one is queued until a key
  map of a later generation applies, the row shows the chord it set and the
  pane records, clears and restores nothing, as the Profiles pane does while
  its indices move.
* **Sanitised** (S3). Chords, menu titles, command names, conflict text,
  warnings (at most twenty listed, then a count) and the core's messages are
  shown through `SettingsModel.sanitisedMessage`.

Known limits:

* **Hand-written chords in another form.** `[keys]` as written keeps each
  entry's key as the user typed it, while the key map lists canonical chords,
  and no export canonicalises a chord (copying the grammar into Swift would
  put core logic in the app). An entry written by hand in another form
  (modifiers in another order, an alias such as `opt`, a shifted symbol such
  as `cmd+?`) is therefore not recognised by its chord: one that sends text
  or is `none` is not shown as a conflict, and replacing or recording again a
  chord such an entry holds writes a second entry for it, which the core
  refuses ("same chord as …"); the pane shows that refusal, sanitised, and
  nothing is written. docs/config.md says so. #355 exports the `[keys]`
  entries in file order with their canonical chord and effect, which lifts
  this and the next limit.
* **Defaults by order.** The defaults a command still has are inferred from
  the key map's order (one chord per entry of the command first, then the
  defaults no entry took). That is off by one for every entry of the command
  the key map dropped (a malformed chord, one past the 512-entry cap, a
  duplicate, including a non-canonical one): a real default is skipped, so
  after recording it stays bound as a hidden shortcut.
* **The lock** can release one apply early when an edit is queued just
  before a hand edit applies, as the Profiles pane's can.

## Amendment: Settings menu item, Open config.toml in Advanced only (#410, 2026-11-12)

The Tessera menu's item reads **Settings**, with no ellipsis, and keeps ⌘,.
AppKit does not add the "…" back: the menu bar installed at launch has the
title as authored (`MainMenuTests` pins it). Open config.toml leaves the
Tessera menu and is on the Advanced pane only; it still creates the file
with the core's commented defaults if it is missing, and Finder shows the
file when no application opens it. Every remaining menu item runs a
registry command, so the key map exempts none. This replaces "Tessera >
Settings…" and "Open config.toml stays in the Tessera menu" under Hosting;
the keys no pane shows are reachable through the Advanced pane's Open
config.toml.

## Amendment: Settings minors (#344, 2026-11-14)

- **The built-in profile.** General's Default profile picker lists
  "Default (built-in)" first: the profile new tabs start with while
  `default_profile` is not set (the core's `BUILT_IN_PROFILE_NAME`,
  `"Default"`, which the configuration reads for an unset key). Choosing it
  removes the key. A `[[profiles]]` entry of that name is the one the core
  uses, so it shows instead of the built-in item.
- **Renaming the default profile** writes the name, then `default_profile`,
  as one run on the write queue that stops at the first refusal: a refused
  rename writes neither. The core has no edit of several keys at once, and
  no order of two single-key edits avoids a file in between whose
  `default_profile` names no profile (the name first: the old name is gone;
  the default first: the new name is not there yet). That file is not a
  loader warning (only a new tab would warn), and the watch's 100 ms
  debounce normally applies both edits together. The name is written first
  so the in-between file never names a profile that a refusal then leaves
  missing.
- **Profile names** are checked before anything is written: a control
  character or line break, nothing visible (empty, spaces, a lone BOM) and
  another profile's name (as shown, renames not yet applied included) are
  refused with why by the Name field.
- **The profile list's lock.** Add and Duplicate are refused at the core's
  256 profiles (`MAX_PROFILES`, which the FFI does not export: the app's
  constant mirrors it), with a message by the list, and never lock it. An
  add or delete unlocks when the listed profiles differ from the ones it was
  queued against (this window's pending keys left out of both), not with
  the first configuration applied after it, which may be an earlier edit's.
  While locked, the model refuses every edit of a profile's key (but for
  the add or delete itself and a Duplicate's keys of the copy), and a line
  under the buttons says why the pane is disabled. A hand edit that puts
  back the very profiles the add or delete was queued against before the
  watch applies it keeps the pane locked until the next change of the list
  or until the window is opened again, since #455 at least 2 s after it
  closed (accepted).
- **Uncommitted text.** A field with text typed and not yet committed holds
  it with the model; a click on another profile, Add and Duplicate commit
  it first (the field goes before it would see the focus go), so the copy
  of a profile has the command just typed; closing the window commits it
  too. A hand edit that moves the profiles drops it, as before.
- **Messages as lines.** The core writes a line break of the text it quotes
  (a TOML syntax error's excerpt of the file) as the two characters `\n`
  (S8); the window shows those as lines, each sanitised (S3), at most 12
  lines and 400 characters. A literal `\n` in a quoted value shows as a
  line break too.
- **Open panels** (Choose… of a working directory, Import Theme…) are
  sheets on the Settings window, not app-modal.

## Amendment: Keys pane on exported entries (#355, 2026-11-14)

The key map JSON now lists every `[keys]` entry in file order (`entries`:
its key as written, escaped and capped; its canonical chord, or none when the
notation was refused; its value; whether it was bound, unbound, shadowed or
refused; its first warning). The Keys pane works on those entries instead of
matching the raw table keys against canonical chords, which lifts the
three known limits of the #40c amendment (the lock's through the new lock
rule below: a hand edit that applies first does not show the run's keys):

* **Matching.** An entry belongs to a command when it is bound and its
  chord is one of the command's chords; the entry that holds a chord is the
  first bound or `unbound` one with that chord, so when the file spells a
  chord twice (`cmd+d` refused for an unknown command, `super+d` bound), the
  pane shows and changes the one that took effect. A hand-written
  `shift+cmd+u` or `opt+cmd+j` is found by its chord: a conflict with a
  `send_text` or `none` entry in another spelling shows inline, and Replace
  or recording again changes that entry under its own key, so the file keeps
  one entry per chord. Recording also removes the command's shadowed
  entries (they would bind it again once the first goes), the last in the
  file first. A command's defaults are its chords no entry of its own binds;
  nothing is inferred from order. The pane parses no chord: it compares the
  core's canonical strings, and takes an edit's value from the action, never
  from `entries[].value` (which is escaped for display).
* **Only keys as written are edited.** An edit that would set or remove an
  entry whose reported key is not one of `[keys]` as written (a key the core
  escaped or cut) is refused with a message by the row; Restore Defaults
  leaves such an entry.
* **Replace checks again.** When the file changed since a conflict was
  shown and the chord now does something else, Replace shows and announces
  what it does instead and writes nothing; a second Replace gives it.
* **The lock** (replacing "until a key map of a later generation applies")
  holds from the moment an edit is queued until the key map's entries show
  every key it wrote (each set with its value, each removed gone), so a
  long run (Restore Defaults over 200 entries, applied as the watch sees the
  writes) stays locked to its last apply. A failed edit releases at once and
  reads the configuration again; a retried one (the file changed under it)
  reads it again, since the other edit's apply may have come first. When the
  key map has not shown the run 2 seconds after its last write (an edit the
  watch saw as no change, or a hand edit that undid it), the pane reads the
  configuration again and edits on. Reading again (`SettingsModel.reload()`)
  reads the core's applied configuration and key map
  (`tessera_config_json`), not the file: released before a slow apply lands,
  the pane edits from that stale snapshot until the apply comes (the wave 22
  generation token fixes it).
* **Problems and caveats.** The warnings are listed in two parts: problems
  (each entry left out, with its own warning, in file order, including a
  notation the loader refused, whose warning is in `entries` only; then the
  key map's warnings no entry carries, the 512-entry overflow last) and
  caveats (the bound entries' warnings, honoured: a reserved chord, typing
  shadowed, a menu item that loses its chord). Each list shows at most
  twenty, then a count; the problems keep the overflow line.
* **Shown as the menus show them.** A chord with Shift on a US shifted
  symbol shows as its menu item's key equivalent from the core's `menu`
  (`⌘?` for `cmd+shift+/`, `⌘+` for `cmd+shift+=`), paired with the
  command's chords that go on a menu (with Command, off the keypad, the
  core's rule) in order, and only where the pair's modifiers are the chord's
  without Shift; every other chord is written as macOS writes a shortcut.
* **Focus and VoiceOver.** A text field taking the focus (a click into the
  search field) ends a recording; starting one takes the focus from a field
  being typed in. Conflicts and refusals are announced
  (`NSAccessibility.Notification.announcementRequested`, high priority),
  once per message shown, so a flood of refused keys is spoken once.

Known limits:

* An entry of a command on one of its own default chords (`"cmd+f" =
  "find"`) is removed, not set to `unbound`, when another chord is recorded,
  so the default binds again; the JSON does not say which of a command's
  chords are defaults.
* Removing an entry can let a later entry of another command for the same
  chord, shadowed until then, take effect.
* A second warning of one bound entry (the JSON reports the first) lists
  under the problems.
* A chord the key map does not list (a conflict on an unknown chord) shows
  in the written form, not as a menu would.

## Amendment: profile edits that wrote a nameless profile or lost text (#455, 2026-11-14)

- **Duplicate is one run.** The copy's name and the keys copied to it are
  one run on the write queue, the name first, stopping at the first
  refusal (as the rename of the default profile). A name the core refuses
  (" Copy" pushes a 125-byte name past its 128 bytes) writes none of the
  copy's keys: written alone, they made a `[[profiles]]` table without a
  name, which the core loads as "Default". The message shows by the list.
- **Settings opened again before an add or delete applies.** A closed
  window's model whose add or delete still waits for the watch is kept by
  the app delegate, and Settings opened again edits on with it, still
  locked. A new model would read the configuration from before the add or
  delete, unlocked, and an edit of its last profile landed past the end of
  the file's list as a nameless profile. The model goes with the next
  configuration applied once nothing waits (applied, or failed), with the
  core, and at the latest 2 s after the last close: the watch applies
  within about 1 s, so a model that waits longer waits for a watch that
  will not apply it, and Settings opened after that starts from the
  configuration the core has applied (the escape from a stuck list in
  "Consequences", and from the hand edit's locked pane in the #344
  amendment's "The profile list's lock", now needs those 2 s after the
  close). One timer runs at a time: Settings
  opened again takes the model with its timer, so a model closed again
  waits the whole 2 s of that close. The wave 22 generation token, which tells a
  snapshot that has the window's edits from one that does not, can
  replace this.
- **A pane switch commits typed text.** The panes' tab view controller
  commits the text typed into a field and not yet committed before another
  pane shows (`tabView(_:willSelect:)`): the switch takes the field away
  before it sees the focus go, and a typed Command, Name or Working
  directory was lost.

## Amendment: Plugins pane (#48, 2026-11-14)

The Plugins pane (between Keys and Advanced) lists every plugin built into
this version, as `tessera_app_plugins_json` reports it (ADR 0047), and
replaces the read-only list of `[plugins] enabled` in Advanced.

* **What it shows.** Per plugin: a toggle named after the plugin, its
  version and id, its status, its capabilities (in the pane's words:
  Sessions (titles and commands), Status bar items, Storage (in memory), and
  for the capabilities 0.2.0 does not grant Terminal output, Side panels,
  Commands, Files, Network; one the pane does not know as the core names
  it), and while it is listed and refused or failed, the host's message (a
  failed one adds that turning it off and on tries again). The status is
  Running, Not started (refused), Failed or Off, and Starting… or Stopping…
  while the toggle shows another value than the host last applied or the
  host has yet to start or stop it (`TesseraPlugin.isSettling`). Ids
  `[plugins] enabled` lists that this version does not have are listed once
  each under the plugins. The status bar the items go to is #49.
* **When it reads.** On `SettingsModel.reload()` (so on every
  `on_config_applied`, after the configuration) and whenever the pane shows.
  The host starts and stops plugins on its own thread after an apply, so
  while a plugin settles the model reads the JSON again every 50 ms
  (`pluginSettleInterval`), at most 40 times per reload or show
  (`pluginSettleReads`); a reload cancels the pending read and starts the
  count again. There is no new callback, and nothing is read per session
  event or output byte.
* **The write.** A toggle sets `plugins.enabled` as a whole array, as shown
  (an edit not yet applied included): on adds the id at the end unless it is
  listed, off takes every copy of it out; ids this version does not have and
  the file's order are kept. It goes through the write queue as every other
  edit (a conflict retried once on top of the other edit, a refusal shown by
  the toggles with the core's message, sanitised, and the toggles showing
  the file again). From the moment it is queued until a configuration
  applied after it (or the write fails), every toggle is disabled and a line
  says why, as the Profiles list is while its indices move.
* **Text (S3).** Every text of the JSON (name, version, id, capabilities,
  message) and every unknown id goes through `SettingsModel.sanitisedMessage`
  before it is shown; an empty name shows the id. The core already caps and
  sanitises the manifest's name and version, and writes the messages itself.
* **VoiceOver.** The toggle's label is the plugin's name, with the hint that
  it turns the plugin on or off in `config.toml`; status, capabilities and
  the problem are read as their rows.

Known limits:

* A hand edit of `[plugins] enabled` that lands between the snapshot the
  pane shows and the toggle's write is overwritten: the last writer of the
  key wins, as for every scalar.
* The lock releases on the first apply after the write, which can be an
  apply of an earlier hand edit; the toggles then edit from that snapshot
  until the next apply (the wave 22 generation token fixes it).
* A plugin that takes longer than 2 seconds to start or stop shows
  Starting… or Stopping… until the pane is shown again or the next apply.

## Amendment: Add and Duplicate through the core's add of a profile (#312, 2026-11-15)

The app half of the ADR 0040 amendment for #312.

- **One edit.** Add and Duplicate are each one `tessera_config_add_profile`
  (`TesseraConfigWriter.addProfile(named:keys:)`): Add sends
  `{"name": "New Profile"}`; Duplicate sends the source's name with " Copy"
  and every key of `profileKeys` the source sets, from an entry or from a
  profile of `profiles/`. The core writes all of it or nothing, so the
  #455 run of the name and then each key is gone, and so is the app's
  `unusedProfileName`. A value the loader warns about or a name past 128
  bytes writes nothing. A `[[profiles]]` entry without a name (the #455
  bug) can no longer come from Add or Duplicate.
- **The core names and places it.** The core numbers a taken name
  ("New Profile 2") against the entries and the profile files, and puts the
  new entry after the entries, before the profiles of `profiles/`. Index and
  name come back from the call. Until a configuration lists the profile, the
  model shows it at that index with the name and keys sent, and the profiles
  of `profiles/` one further on, as they will be. The add ends when the
  applied list has that name at that index (the core's name is unique), or
  when the listed profiles differ from the ones it was queued against (a
  hand edit of the file), as in the #344 amendment. The pane selects the
  new row once the call returns, through a callback with the index, since
  the index is not known before the write. A `Conflict` (the file changed
  on disk) is written again once, as an edit is, after a reload, and the
  add waits for a configuration after the external edit's.
- **The cap is the core's.** The #344 constant that mirrored `MAX_PROFILES`
  (256) and the disabled Add and Duplicate at that count are gone: they
  counted only what the window listed, while the core counts entries and
  profile files together. The buttons are disabled only while the list is
  locked, and the core's refusal ("the first 256 profiles, with those in
  profiles/, are read; one more would push one out") shows by the list.
- **The core's messages.** A refused add shows the core's message by the
  list, sanitised and cut as every message is (S3), not "This value cannot
  be written.". Only the error kind is logged (S8).
- **No key past the list.** A key-path edit of a profile index at or past
  the applied list's count is not written: the core would add a
  `[[profiles]]` table without a name. A new profile goes through the add.
