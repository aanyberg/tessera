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
the file's value shows. A `config.toml` that stops loading after the edit
(a broken hand edit read before or instead of the save) applies nothing:
the core calls `on_config_invalid` once per new failure, the window reads
the snapshot's `load_error` again, shows it (sanitised, S3) above every
pane's controls, the Profiles pane's too (#570), and drops the
edit, an add or a delete of a profile from `pending`, so the control shows
the configuration in effect (#547). An add or delete dropped so leaves no
notice by the list: the load error says why it went. An edit queued while the file already
failed with that error (a repair) waits for its configuration as before. The core does not apply its own write: the
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
after the retry applies. (A profile's key waits for the external edit to
apply before it is run again, at the index of its profile: the #549
amendment.)

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
  to about 1 s) before the next profile edit, at most 2 s after the write
  (the #344 Profiles amendment): a watch that never applies it, or another
  app that saved config.toml without it, unlocks the pane then, with a
  notice by the list (or, while config.toml does not load, the load error
  above the pane, #570).
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
  missing. If the second write fails (refused, or the file changed twice),
  the rename stays and `default_profile` names the old name: new tabs start
  with the built-in profile and the loader warns once one opens; the
  core's message shows by General's Default profile picker.
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
  (" Copy" pushes a name of 124 bytes or more past its 128) writes none of the
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
  Starting… or Stopping… until the pane is shown again (also by ⌘, onto
  the open window, #489) or the next apply.

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
  on disk) is written again once on the write queue, as an edit is, and
  only then does the model read the configuration again; the add waits for
  a configuration after the external edit's.
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

## Amendment: the Profiles lock's release, file profiles and the cap (#344, #489, 2026-11-18)

This replaces "A hand edit that puts back the very profiles…" in the #344
amendment's "The profile list's lock", and "The cap is the core's" in the
#312 amendment.

- **A bounded lock.** An add or delete, once written, locks the list at
  most 2 s (the Keys pane's settle rule). Then the model reads the applied
  configuration again; an add or delete it does not show no longer locks
  the list, and a notice by the list says so ("… was written but is not in
  the configuration applied: another app may have saved config.toml
  without it", or that the profile deleted is still there). Another app
  that saves config.toml from a read before the add drops it; until #344
  that kept the pane locked until Settings was closed. The selected
  profile's controls start again, as after a hand edit. If the watch
  applies the other app's file before the window's own write lands, no
  configuration showed the add, and the notice is the only sign (the
  file on disk is the other app's). A watch slower than the 2 s is the
  other way to get here: when a later configuration shows the add or
  delete after all, the notice goes and the rows after a late add move on
  again with their messages (#517 round). Until then, after a delete is
  released, a control acts on the index the list shows: if the delete did
  reach the file, that index is the next profile there. The risk is
  accepted in the window as it is for an edit from another app.
- **Profiles of `profiles/` are read-only.** Selected, one shows "Defined
  in profiles/<file>.toml. Edit that file to change this profile." above
  its controls (the core's `source`, sanitised for control, bidi and line
  separator characters, S3); its row has the same text only as its help
  tag. Every control is disabled, and Delete is disabled for them (its
  help tag says why); the model refuses a delete of one with the same
  reason. The core edits only config.toml and refused each edit anyway.
- **The cap, mirrored again.** The configuration lists at most the core's
  256 profiles, entries and profile files together, so the count the
  window lists is the core's. At 256 Add and Duplicate are disabled and a
  line under the list says why; while an add or delete waits, the same
  line says the pane waits for config.toml to be applied. The line is
  selectable text, so VoiceOver and tests read it. `MAX_PROFILES` is still
  not exported: the app's constant mirrors it, and the core's refusal is
  still shown if they ever differ.
- **The new row is selected through the model.** An Add or Duplicate
  leaves the index the core wrote it at in the model's selection request,
  which whichever Profiles pane shows the model takes: the model holds no
  closure of a pane (a closed window's pane is not kept alive by a waiting
  model, and Settings opened again on that model selects the copy).
- **The keys pending at queue time.** An add keeps the keys this window
  had set and not seen applied when it was queued, and leaves them out of
  the lists it compares, so an earlier edit applied before the add's
  result does not end it.
- **Messages move with their profile.** A message keyed by a profile's key
  path moves one index on when an add puts a profile before it, and back
  when the add is released unseen.
- **Names compared as the core compares them.** A name taken by another
  profile is the same bytes (UTF-8), not the same canonical string: NFC
  and NFD spellings are two names, as for the core.
- **Nothing dropped silently.** A slider's or stepper's value of a
  profile held back while the list was locked is not written, with a
  notice by the list; Add or Duplicate after a refused rename says the
  name typed was not saved and why. Only a field holding typed text is
  committed by a click elsewhere, so a typed working directory no longer
  overwrites the folder Choose… just wrote. A retried delete that the
  reload shows applied unlocks at once, and a configuration applied
  while a slider or stepper is pressed keeps the value it shows (a step
  by VoiceOver or the keyboard has no press: it shows the configuration
  applied after it, as before). The #517 amendment says what the release
  does.

## Amendment: the Plugins pane's cap, ids and close, and the Keys pane's keys (#489, 2026-11-18)

This replaces "on adds the id at the end unless it is listed" and "every
unknown id goes through `SettingsModel.sanitisedMessage`" in the #48
amendment.

- **The host's 64 ids.** The plugin host reads the first 64 ids of
  `[plugins] enabled` and ignores the rest with a warning. The FFI does not
  export the cap: `SettingsModel.maxEnabledPlugins` mirrors
  `MAX_ENABLED_PLUGINS` (tessera-app-core), and the two change together.
  The toggles and the status follow the first 64 ids, as the host does, so
  a plugin listed only past them shows Off. At 64 ids a toggle turned on
  writes nothing and says why next to the toggles; turning one off still
  writes, unless the JSON of the list left is over the 64 KiB a config
  value may be (about 6 000 ids of 8 bytes, or one id of 100 KB): the core
  then refuses the edit with
  "value too long" and the file stays as it was. A hand edit that makes
  room clears the line. Past 64 the pane
  shows the host's warning in its words, with how many ids are ignored.
  The config loader does not refuse a longer list, so the pane does not
  either.
- **Unknown ids, escaped and capped (S3, S5).** The ids of the first 64
  that this version does not have are listed once each, compared byte for
  byte, in quotes: printable ASCII as is (a quote and a backslash escaped),
  every other code point as `\u{…}`, at most 80 characters each. Ids that
  differ by a zero-width or trailing space, or by NFC and NFD, show apart,
  and nothing of one reaches the window unescaped. At most 16 are listed,
  then "and N more".
- **Ids and keys are bytes, as for the core.** The toggles compare ids byte
  for byte (turning `café` off keeps its NFD spelling, another id for the
  host), and the Keys pane keys what an edit leaves in `[keys]` by the
  table key's bytes, so an edit of `"cmd+ö"` written in NFC beside an NFD
  entry ends when the key map shows it, not after the 2 s timeout. It
  compares the key map's chords by bytes too: `cmd+;` and `cmd+` with
  U+037E are one Swift `String` and two keys for `Chord::parse`, so an edit
  of one neither removes, overwrites nor conflicts with the other.
- **A closed window's plugin edit.** A model whose `[plugins] enabled` edit
  waits to apply is kept when Settings closes, as one whose profile add or
  delete waits (#455), with the same 2 s limit; Settings opened again
  before then shows it, still locked.
- **Reads.** ⌘, onto the open Settings window reads the plugins again. With
  no core the pane lists no plugins and says they could not be read, also
  from a read while one settles. A read that returns the plugins shown sets
  nothing, so the reads while a plugin settles redraw the pane only when it
  changes.
- **Entry values.** A `[keys]` entry value of a shape this version does not
  know (a number, list, other object, or none) reads as not a string; one
  entry never fails the key map.

## Amendment: a configuration applied during a press (#517, 2026-11-18)

This completes "a configuration applied while a slider or stepper is
pressed keeps the value it shows" in the #344 amendment: a value set stops
waiting once a later configuration applies, and for a held control that
rule now runs at the release.

- **The settle rule and held presses.** A pressed control keeps its value
  through every configuration applied. The model tells its own values from
  a hand edit by what it wrote: the values written and not yet seen applied
  (at most `SettingsModel.maxUnappliedWrites`, 32, per key), compared as the
  core stores them (`f32`). A new applied value that is one of them is the
  press's own, and an earlier one does not replace the last at the release.
  Any other new value of the key is a hand edit: at the release the model
  reloads, the control shows the edit, and the next step starts from it.
- **What looks like no edit.** A configuration applied during the press
  can leave the key as it was, or on an earlier value of the press: another
  app saved over the step before the watch read it. (A profile row that
  moved ends the press instead: the #534 and #536 amendment.) That cannot be told from the press's own values on their way, so if one
  applied since the last write, the model reloads
  `profileSettleTimeout` (2 s) after the release, unless the key is pressed
  again, and the control shows what is applied. A watch slower than that
  shows the earlier value until the last one applies.
- **Which value wins.** A step held back by the coalescing when the edit
  applies was taken before it and is dropped. A step taken after it, in the
  same press, is written over it: the last act wins, as for two edits in
  the file. A change of another key says nothing about this one.
- **The release (#534).** A release writes only if its press made a step,
  or found one held back by the coalescing when it began. A press that only
  held the control writes nothing, whatever applied meanwhile. The reload
  after a hand edit runs on the next turn of the main queue, after the
  release event: in an app linked against the macOS 15 SDK, which is what CI
  and releases build, `Slider` is an `NSSlider`, whose mouse-up sends one
  more action with the knob's value. SwiftUI takes that for a new edit if
  the binding has already moved, and it would write the old value over the
  hand edit.
  *Note (2026-11-22, #583):* since #584 CI and releases link against SDK
  27, where `Slider` is not an `NSSlider`; the handling stays for
  older-SDK builds.
- **Limits.** After more than 32 unseen writes (the watch 3 s behind), an
  older value applied counts as a hand edit: the control shows it from the
  release until the newer values apply. In a press that made a step, a hand
  edit equal to one of its unseen writes still reads as the press's own: a
  last step not yet written goes over it, and if the last step was written
  the control shows the edit after `profileSettleTimeout`. A profile row
  that moved under the press ends it (the #534 and #536 amendment).

## Amendment: a press that does not move, and a press whose row goes (#534, #536, 2026-11-19)

This completes the #517 amendment's "The release".

- **The value shown is no step.** A slider sends the knob's value when it
  is pressed, and an `NSSlider` (the macOS 15 SDK) again on mouse-up. The
  knob sits on a step, so for a value off the step (`0.123` by hand, or
  `0.85` as the core's `f32` reads it back, 0.85000002) that value differs
  from the one shown. A value that the label shows as it shows the value
  set (a slider's to a hundredth, `12 %` or `0.12`; a stepper's to a tenth)
  is no step: a press that does not move writes nothing, and the file keeps
  `0.123`. A value the label shows differently is always written. On the
  opacity slider the step is the label's hundredth, so that is any step.
  On the dim slider the label counts in 1 % and the step is 5 %, so a
  value off the step (`0.33` by hand, `33 %`) and the step nearest it
  (`35 %`, where the knob sits) look different: a press that ends there
  writes `0.35`. A click on the knob does too, as it cannot be told from a
  drag that ends where it began. Steppers follow the same rule.
- **A press whose row goes.** When `profileListGeneration` moves (a hand
  edit added, removed, reordered or changed a profile, or an add or delete
  was released), the Profiles pane makes the rows again, so a pressed
  slider or stepper there goes with no release. The model ends the press of
  every profile key then: a step held back is dropped, nothing more is
  written, and with no write running the row shows the configuration
  applied at once. The old control's value binding carries the generation
  it was made for and writes nothing after it moved, so its late actions
  go to no profile, and its release finds no press. A press of the new row
  writes as any. Only the reload of a hand edit settles the row at once: a
  value pending when an add or delete is released settles at the next
  apply.
- **Not covered: a save undone before the watch reads it (#535).** When
  another app restores the file inside the watch's debounce, the watch
  reads a configuration equal to the one applied, so no
  `on_config_applied` arrives and the value set stays shown as pending
  until the next apply or the window opens again. The model cannot tell
  that read from a slow watch (the #517 B1 rule waits for the watch however
  long it takes), so the fix is the core's: report a read that changed
  nothing.

## Amendment: the mouse-up after a hand edit during a hold (#550, 2026-11-19)

The #534 rule compares a value as the label shows it, so on the dim slider
(1 % label, 5 % step) the knob's step nearest a hand-typed `0.33` (`35 %`)
gets past it. When a hand edit applied during a press that made no step
after it (the #517 "overridden" release), the `NSSlider` of the macOS 15
SDK still sends the knob's value on mouse-up, after the release and in a
press of its own; that wrote `0.35` over the hand edit. Now such a release
holds every change of its key until its events are handled (a timer of no
delay, which a new press does not cancel): the mouse-up writes nothing on
any slider, whether it carries the step nearest the hand edit or the
knob's value from before it, and the next press writes as any. A press with
no hand edit keeps the #534 rule: a no-move click on a hand-typed `0.33`
still writes `0.35`.

## Amendment: a profile's edit that meets a hand edit (#549, 2026-11-19)

A profile's key names the profile by its index (`profiles[1].command`).
When a hand edit of the file meets the edit (`Conflict`) and removes or
moves an earlier profile, running the edit again at once would write the
profile now at that index. So a profile's edit (a value, a rename, a
delete) is not run again on the write queue. It stays shown and in flight,
the model reads the configuration again, and once a configuration applied
after the conflict came back lists the profile under the name it had when
the edit was queued, the edit runs again at that profile's index, with the
edits of its run after it (a rename's `general.default_profile`). With no
apply within the settle time (`profileSettleTimeout`: the watch applied the
hand edit before the conflict came back, or the hand edit changed nothing
it applies), it is placed by the configuration applied then. If no profile
or more than one has that name, the edit is dropped: a delete has nothing
left to do, and for a value the list says it was not saved. A second
conflict is shown, as for any edit. An edit of the same key already
queued behind the one that met the conflict is the latest word, so the
earlier one is dropped. While one waits, a new profile's edit waits with it
and is placed the same way; one of the same key replaces it, a key of a
profile meaning the same field of the same profile, by its name in the
list shown (an apply that moved the list since gives it another index).
Waiting edits are written again oldest first, so a later one's value is
the one left. The model waits even when its window is closed, and while
a profile's edit is unwritten (queued, running or waiting) the closed
window keeps it (2 s after the close at most, as for an add or delete),
so Settings opened again edits on with it rather than beside it.

Not covered: the core reports `Conflict` only when the file changes during
the edit call itself. Any profile's edit made after a hand save and before
the watch applies it (the watch's debounce and its event latency) meets no
conflict, so it still goes by index. Closing that needs the core to take
the profile's identity with the edit.

## Amendment: a held slider that sends the value from before a hand edit (#564, 2026-11-19)

A press that has made no step shows a hand edit applied during it, and
SwiftUI moves the knob to it. When AppKit cancels the press while it is
held (it cancels the active mouse gesture recognizers when the window
server moves the window, for one), the `NSSlider` of the macOS 15 SDK ends
its tracking at the point it was pressed and sends the value under it:
the value from before the edit, inside the press, where the #517 rule took
it for a step after the edit and wrote it over the hand edit. The press
need not have begun for the model by then: SwiftUI begins a no-move
press only with the cancel, and does not pass on the press's own action
(the knob's value is unchanged), so neither marks it. The mouse button
does: a configuration the model reads while the left button is down
(`NSEvent.pressedMouseButtons`), or while a slider or stepper is begun,
keeps what the window showed: the configuration it replaces and the
values set over it and not yet applied, with the window server's count
of left mouse downs at that moment. A press that steps before any reload
keeps it at its first step. The shown value, not only the applied one,
matters twice when a press follows a release quickly: the knob of a
press that begins before the previous press's write applies sits on that
unapplied value, which a cancel re-sends; and that write applying during
the new press is the press's own value arriving, not a hand edit, so a
step back from it is written. While the same press holds, a key that
has made no step since and holds none back reports the value it showed
before the press's first reload or step if that differs from what is
applied now (`valueBeforeHandEdit`), and a slider's
binding drops a value the label shows as that one: it is the press
point sent again, not a step, so the hand edit stays and the release
shows it, also where the press made a step before the hand edit (the
edit marks the press overridden). Any other value is a step and is
written, and a key with a step after the hand edit gets no kept value,
so a move back to the press point is written too. The
kept configuration goes at a release (`endChange`), at a reload with no
press held, and with the next press (another count), so a click on the
old value in a later press is written. Synthetic events in hosted tests
never reach the window server, so the tests set the press
(`leftMousePress`). Steppers do not get the rule: a stepper's step from
a hand edit can land on the value from before it, and is a real step.

## Amendment: the plugin cap comes from the core (#560, 2026-11-22)

This replaces "The FFI does not export the cap" and the last two sentences
of "The host's 64 ids" in the #489 amendment.

- **One cap.** `MAX_ENABLED_PLUGINS` (64) lives in tessera-config, next to
  `[plugins]`; tessera-app-core's plugin host re-exports it and the C ABI
  exports it as `TESSERA_MAX_ENABLED_PLUGINS`, tied to the host's by a
  test. The pane reads it as `TesseraPlugin.maxEnabled` (TesseraKit) and
  keeps no copy.
- **The warning.** The config loader, not the host, warns of a list past
  the cap: one warning per load that names the cap, never an id or how
  many are listed (S8). The pane still shows its own line past 64, with
  how many ids are ignored. Because the loader's warning reads the same
  for any list past the cap, an edit that takes a list at 64 past it is
  refused by the core (the pane refuses it first, as before), while
  turning one off in a list still past the cap is written.
