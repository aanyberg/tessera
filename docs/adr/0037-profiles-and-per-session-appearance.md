# ADR 0037: Profiles and per-session appearance

**Status:** accepted · 2026-11-07 · amended 2026-11-07 (#333)

## Context

`[[profiles]]` has been in `config.toml` since 0.1.0, but nothing used it
beyond `[general] default_profile` choosing an appearance: every session
was drawn with one app-wide look, a profile's `command` and
`working_directory` were never read, and a profile's `appearance` table
replaced `[appearance]` whole, so a profile that set only `font_size` also
reset the theme, opacity and cursor to their built-in defaults. Issue #39
makes profiles real: a session starts with a profile's command, directory
and look, the app offers "New Tab with Profile" and "New Window with
Profile", and a configuration change restyles each session by its own
profile. Profile files of their own (`profiles/*.toml`) are #312, not this.

## Decision

**A profile's appearance inherits key by key.**
`tessera_config::ProfileAppearance` has every `[appearance]` key but
`dim_inactive_panes` as an `Option`; `ProfileAppearance::over(base)` puts
the keys that are set over `[appearance]`. A key left out follows
`[appearance]`, also after it changes. `dim_inactive_panes` is drawn by the
window, not the session, so in a profile it is ignored with a warning. The
inheritance changes 0.1.0 behaviour (a left-out key was reset to its
default); the change is in docs/config.md and the release notes. A
`font_size` or `opacity` that is not finite (`nan`, `inf`) is left out with
a warning, so `[appearance]`'s applies (#333).

**Resolution order.** `Config::resolve_profile(requested)` gives the
`[[profiles]]` entry named `requested`, else the one `[general]
default_profile` names, else the built-in profile (`[appearance]` and the
login shell). A name that matches nothing gives a `Warning::ProfileNotFound`,
except the default `"Default"` when no profile has it. Of two profiles with
one name the first wins, and the second gets a warning. The
default profile is used for every new tab, window and pane and for
restored panes; a pane split off another starts with that pane's profile.

**Spawning.** `App::spawn_session_with_profile(options, profile)` (and the
FFI's `SpawnOptions.profile`, `TesseraSession.SpawnOptions.profile`) runs
the profile's `command` when the caller asked for the login shell: one word
is run directly, anything else through `/bin/sh -c`. An explicit program
wins over the profile's command. `working_directory` is used when the
caller gave no directory; `~` and `~/...` are the home directory, and a
directory that does not exist falls back to the home directory with a
warning that names the directory (escaped) and never the command (S8).

**A profile that cannot start (#333).** When what the profile changed
fails to start (a `working_directory` that exists but cannot be entered, a
`command` that cannot be executed: `PtyError::Spawn`), the session is
started once more as the built-in profile would start it, the caller's own
options (the login shell in the home directory), and drawn with
`[appearance]`. Its terminal reads one line before any of the child's
output: `[Tessera] profile "<name>" could not start (<errno text>); started
the default shell.` The name passes `display_name` (S3: control and
bidirectional formatting characters removed, at most 128 bytes); the errno
text is the C library's, so no path or `PATH` from the spawn error reaches
the screen. The warning logged names the profile by its index and the
errno only (S8). A failure of options the caller chose itself is not
retried, nor is the retry: if the built-in start fails too, the profile's
error is returned. So a broken `default_profile` never leaves New Tab
without a shell.

**Per-session looks.** `App` keeps a `Look` (theme, font size, cursor) per
session, stored at attach from the session's resolved profile and
re-derived from it on every configuration apply. An `[appearance]` edit
restyles only the sessions whose profile leaves that key out; a profile
edit restyles only that profile's sessions. A font size change resets a
session's zoom only when its own resolved size changed (the earlier lead
decision D6, as Terminal.app does on a profile change, now per session). A session attached after a configuration it was not styled with
catches up, guarded by the look's generation so a late attach never puts
an older look back. The app-wide theme (`App::view_theme`,
`on_appearance_changed`) is the default profile's.

**One opacity switch.** AppKit windows share one opacity setting in the
host, so `on_appearance_changed` reports opaque only while every live view
is opaque (with no view, the default profile's). An opaque session in a
translucent window still draws its own opaque background. ADR 0043
replaces this with opacity per tab window and adds blur per pane.

**Menus without registry commands (the E5 choice).** ADR 0036's command
registry has integer parameters only, so a profile, which is chosen by
name, cannot be a parameterised registry command. The Shell menu's two
submenus therefore carry no registry command and no key binding: their
items are identified `profile_menu:new_tab:N` / `profile_menu:new_window:N`
(the submenus themselves `profile_menu:new_tab` and
`profile_menu:new_window`), the profile name rides in `representedObject`,
and `MenuKeymapTests` exempts exactly those two identifiers. The submenus
list the profiles in file order, hidden when there are none, and are
rebuilt on every `on_config_applied`. Titles are sanitised (S3): whitespace
runs become one space; control, format (but the zero-width joiner),
separator, surrogate, private-use and unassigned characters are removed;
at most 64 characters; an empty title reads "Unnamed Profile"; titles that
come out equal are numbered "(2)", "(3)". A name that repeats exactly is
listed once, as the core starts the first.

**Bounds (S5, S7).** At most 256 profiles (`MAX_PROFILES`), later ones
dropped with one warning; a name longer than 128 bytes
(`MAX_PROFILE_NAME_LEN`) is cut on a character boundary with a warning.
A NUL character in a name (which could never be asked for over the C
ABI) is replaced with U+FFFD, before the cut, with a warning naming the
entry. The entry keeps its place, so the loaded `profiles[i]` is always
the file's `i`-th `[[profiles]]` and the key paths of ADR 0040 edit the
profile the frontend shows (#333).
The FFI reads `SpawnOptions.profile` at most 129 bytes and refuses a longer
or non-UTF-8 name with `INVALID_ARGUMENT`.

**Not persisted.** A pane's profile lives in the app's memory only;
restored windows start with the default profile.

## Consequences

- A profile's `font_family` is stored but not honoured per session: the
  font is process-wide (glyph atlas and shaper), so only `[appearance]
  font_family` changes it.
- Profile commands with a path containing spaces must be quoted for
  `/bin/sh -c`.
- A parameterised registry command with a string parameter would let the
  profile items take key bindings; that is a later registry change.
- #42 (transparency and blur) builds on `theme_for` and
  `ProfileAppearance`, with a session appearance getter in the FFI
  (ADR 0043).
