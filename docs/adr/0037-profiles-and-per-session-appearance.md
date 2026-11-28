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
warning that carries the directory's length only, never the path or the
command (S8). The directory reaches the spawn unchecked: before #673 the
profile code stated it on the caller, the app's main thread, with no time
bound, so one on a stalled volume held the app for the whole stall
(53.9 s measured). Now the spawn's bounded program lookup (#554) states it
first, on its helper, and the warning is logged there; a directory that
gives no answer within the lookup's 5 s is a `PtyError::ProgramTimedOut`,
which falls back as below.

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

## Amendment: profile fallback and default_profile (#342, 2026-11-13)

- A profile `command` too long to run (`PtyError::ArgumentsTooLong`, the
  system's limit on a program's arguments) falls back like a spawn error:
  the built-in start runs once more and the notice reads `could not start
  (Argument list too long)`, with `E2BIG` as the logged errno.
- A `command` with a space runs through `/bin/sh -c`, which always starts:
  a program it names that does not exist is the shell's error, shown in the
  session (`sh: …: command not found`, exit 127), not a spawn failure, so it
  gets no fallback. `docs/config.md` says so.
- `[general] default_profile` gets the same NUL replacement (U+FFFD) as
  profile names, with an out-of-range warning, so a name with a NUL still
  selects the profile it named.

## Amendment: profile files (#312, 2026-11-14)

- **One profile per file.** `profiles/<stem>.toml`, next to `config.toml`,
  holds one profile: the keys of one `[[profiles]]` table at the top
  level of the file. `name` defaults to the stem. Only UTF-8 names ending
  in `.toml` with a stem that is neither empty nor starts with `.` are
  read; anything that is not a regular file once symbolic links are
  followed (a directory, a FIFO, a dangling link) is skipped without a
  warning. The loader never writes `profiles/`.
- **Order and names.** The profiles are `config.toml`'s `[[profiles]]`
  entries, then the files sorted by stem in byte order. Names keep the
  existing rule: of two profiles with one name the first wins and the
  second gets the duplicate warning, across both sources.
  `default_profile` may name a file's profile. Each `Profile` carries its
  `ProfileSource` (`Config` or `File(stem)`); it is not a key and is not
  serialised.
- **Bounds (S5).** `MAX_PROFILES` (256) counts both sources, with one
  `TooManyProfiles` warning. At most `MAX_PROFILE_FILES` (256) files are
  read: the listing reads at most `MAX_PROFILE_DIR_ENTRIES` (4096)
  entries, sorts the candidates, takes the first 256 regular files and
  reports the rest in one `TooManyProfileFiles` warning. Each file is read
  through `read_capped` at most `MAX_PROFILE_FILE_LEN` (64 KiB). A file
  lists at most `MAX_PROFILE_FILE_UNKNOWN_KEY_WARNINGS` (8) unknown keys.
- **Failures.** A file that is too large, not UTF-8, not TOML or has a
  value of the wrong type is skipped with one `ProfileFileIgnored`
  warning: the file name escaped and cut as a warning path, and a reason
  that carries a position (line and column, or a byte offset) but no text
  from the file. A file that loads gets the warnings an entry would,
  wrapped as `ProfileFile`, with key paths relative to the file. A
  `config.toml` that does not parse keeps the last good configuration,
  profile files included, as before.
- **Watching.** The native backend registers `profiles/` with `themes/`
  (one descriptor) and the first `MAX_PROFILE_FILES_FOLLOWED` (32) files
  by stem, for in-place saves, re-registered when the set changes at a
  settle. A profile file that is a symbolic link puts the directories on
  the way to its target, and the one holding the target, on the chain
  shared with `config.toml`, under the same `MAX_CHAIN_DIRS` (32) and
  link caps; past them the backend polls `config.toml` and `profiles/`
  every two seconds. The polling backend samples `profiles/` and its
  first 32 files every interval, lists the directory again when it
  changed, and lists all of it every twentieth interval. A profile file
  saved, added or removed is one reload through the quiet loader, like a
  `config.toml` save. Limit: natively, an in-place save of a file past
  the first 32 by stem is seen only with the next change in `profiles/`
  (a rename-save, an add or a remove), while the polling backend sees it
  within a full listing.
- **Editing.** `tessera_config_json` gives each profile `"source"`:
  `"config"` or `"profiles/<stem>.toml"`. `ConfigEditor` refuses a set or
  remove under `profiles[i]` for a file's profile (ADR 0040 amendment
  below) rather than writing it into `config.toml`.

## Amendment: profile files past the followed ones (#489 21.4 M1, 2026-11-18)

The limit in the watching rule above is lifted: while `profiles/` holds
more than 32 profile files, the native backend samples all of them every
2 s, so an in-place save of one past the first 32 by stem is applied
within about 2 s, as the polling backend's full listing does. The profile
themes' resolution and warning rules are in ADR 0006's amendment of the
same date.

## Amendment: the notice for a refused `#!` script, and polling (#489, #526, 2026-11-18)

- **The errno of a refused script.** `tessera-pty` refuses a `#!` script
  whose interpreter cannot run, and the fallback notice takes its errno
  text from that refusal. A script with CRLF line endings is `ENOEXEC`
  (`Exec format error`), not the `ENOENT` of its `/bin/sh\r`, which said
  "No such file or directory" about a file that is there. Only a CRLF
  script whose `#!` line ends at the interpreter is refused: with an
  argument or through `env` (`#!/bin/sh -e\r`, `#!/usr/bin/env bash\r`) the
  `\r` is in the argument, the interpreter exists, and the script starts;
  the interpreter or `env` then fails on that argument, with no notice
  (#526). An interpreter that is not a regular file or not executable is
  `EACCES`, a missing one or a line naming none `ENOENT`, and one that
  cannot be reached (`ENOTDIR`, `ELOOP`) keeps its own errno. Only the
  text after the quoted interpreter is read: the path comes from the
  script and cannot choose the errno. The notice still shows only the
  sanitised profile name and the errno text (S3, S8).
- **Registration errors stay on the fast poll.** The native backend polls
  every `CAPPED_CHAIN_POLL_INTERVAL` only when the chain cannot be walked
  (past a cap, a cycle, a directory it may not search): nothing there can
  be seen until the links or the mode change. A directory holding a link
  that can be walked but not registered (#393 B1, mode 0311) fails the
  registration and stays on `poll_interval`, as intended: the walk still
  works, so the poll is what applies a retarget of that link, as promptly
  as the polling backend does.

## Amendment: fallback on program lookups without an answer (#602, 2026-11-25)

- **Fallback list, appended.** Since #554 a profile `command` is looked up
  (its path, each `PATH` candidate) on a helper thread that the spawn waits
  for at most 5 s. Two more refusals of that lookup fall back like a spawn
  error, the built-in start running once more:
  - `PtyError::ProgramTimedOut`: the file system gave no answer within
    the 5 s, as for a command on a stalled network volume. The notice reads
    `could not start (Operation timed out)`, `ETIMEDOUT` the logged errno,
    as for the pty open that gave no answer in time (#568). Before this
    the tab failed after 5 s (and before #554 the caller hung until the
    volume answered, then fell back).
  - `PtyError::ProgramLookupBusy`: the lookup was not made because four
    earlier lookups or `#!` checks are still waiting for the file system
    (the helper cap, docs/security.md S5), or every helper stayed busy with
    earlier ones for the whole 5 s. Its message says so (`too many earlier
    program checks are still waiting`) rather than that the file system
    gave no answer, since nothing was asked. The notice reads `could not
    start (Resource temporarily unavailable)`, `EAGAIN` the logged errno.

  A lookup or `#!` check whose helper thread cannot be started (the
  process out of threads) refuses the spawn as a `PtyError::Spawn` whose
  reason keeps the thread start's `(os error N)` (`EAGAIN`), so the
  fallback's notice names that errno, as for any other spawn error; a pty
  open whose helper cannot be started is a `PtyError::Open` with that
  reason, which does not fall back. The notice and the warning still carry
  only the sanitised profile name, its index and the errno: never the
  command's path (S3, S8).
- **Known limit: the fallback at the helper cap.** The built-in start
  looks its program up (the login shell) through the same capped helpers.
  While four earlier lookups are stuck, its lookup is refused too, so no
  default shell starts: the tab fails with the profile's
  `ProgramLookupBusy` error ("too many earlier program checks are still
  waiting"). The fallback on `ProgramLookupBusy` stays, since it works as
  soon as one stuck lookup ends. Keeping a helper free for the built-in
  start (or exempting its lookup) is #661. *Lifted by the #661 amendment
  below.*
- **Known limit: `$HOME` on a stalled volume.** A session that starts in
  `$HOME` (no working directory given, or one that is not a directory)
  whose home is on a volume that gives no answer still holds the caller,
  the app's main thread, until the volume answers: the forked child's
  `chdir(2)` into `$HOME` blocks before its `exec`, and the spawn waits for
  the `exec`. This predates #554 and is not bounded; neither the lookup's
  5 s nor the fallback applies to it.

## Amendment: a helper for the built-in start, and no second wait on the same stall (#661, 2026-11-25)

- **A reserved lookup helper.** The built-in profile's start (a tab with
  no profile, a profile that changes neither the program nor the
  directory, and the fallback start) looks its program up through
  `NativePty::spawn_builtin`: on a shared helper when one is free at once,
  otherwise on one helper kept for these starts (`RESERVED_HELPERS` = 1),
  and with neither free waiting within the 5 s for whichever frees first
  (a shared one or the reserved one). So
  four tabs of a profile whose command is on a stalled volume, which fill
  the shared helpers with stuck lookups, each still start the default
  shell, and so do later tabs (their profile lookup refused at once with
  `ProgramLookupBusy`, the notice naming `EAGAIN`). The bound stays finite:
  the reserved helper can itself get stuck (a built-in start whose own
  lookup stalls), and with it and every shared helper stuck, built-in
  starts are refused at once too, so at most 4 + 1 lookup and `#!` check
  helpers are ever stuck
  (docs/security.md S5). The built-in start's `#!` check stays on the
  shared helpers and is skipped while none is free, as for any spawn. A
  reservation was chosen over exempting the login-shell lookup from the
  cap because an exempt lookup would either run on the caller (the app's
  main thread, held by a stalled `$SHELL`) or leave a thread per tab
  behind on a volume that never answers.
- **No second wait on the same stall.** A timed-out lookup names the step
  that got no answer (`PtyError::ProgramTimedOut { stalled }`: the working
  directory, `$SHELL`, or the program; `LookupStep`), with that path's
  length. The fallback is not tried when the built-in start would ask
  about the same path: `$SHELL` (every start checks it, so a profile with
  a local `command = "/bin/sh"`, or one that changes only the working
  directory, under a `$SHELL` on a stalled volume), the same working
  directory, or the same program. The tab then fails after the one 5 s
  wait, with one stuck helper instead of two, and its error names the
  login shell (`the login shell ($SHELL) (<n>-byte path) could not be
  looked up`) rather than the profile's command. A profile whose own
  command or working directory stalled still falls back.
- **One wait after a refusal for want of a helper.** A profile's start
  refused with `ProgramLookupBusy` names the step the helpers it found
  taken were on when the refusal came (`ProgramLookupBusy { stalled }`,
  from a progress mark each lookup helper keeps; `$SHELL` before the
  working directory before the program when they differ), with that
  path's length. When the fallback would ask about that same step (a
  stalled `$SHELL` under a profile with a local `command = "/bin/sh"`, the
  same working directory, the same program), a tab that waited the whole
  5 s for a helper would wait 5 s more on the same stall, so the
  fallback's lookup ends when the first start's would have
  (`NativePty::spawn_builtin(options, since)`, `since` the start of the
  tab's spawn), but never less than 1 s (`SPAWN_PROGRAM_FLOOR`): refused
  at once, the fallback gets nearly the whole 5 s; refused after waiting,
  it gets what is left or the 1 s floor, whichever is longer, so the tab
  takes at most about 6 s in all, and when that lookup times out the tab
  fails with its error, which names `$SHELL` (say) rather than the
  profile's command. The floor is for a tab refused just as the stuck
  lookups time out, whose fallback would otherwise have no time at all.
  When the taken helpers were on a step the fallback does not ask about
  (the profile's own command or working directory on a stalled volume),
  the fallback gets its own 5 s, as after that step's own timeout: its
  lookup may queue behind other tabs' fallbacks on the one reserved
  helper, and with a slow but healthy `$SHELL` (300 or 600 ms a lookup)
  five such fallbacks take up to 3 s in turn, which a 1 s floor would cut
  short (adversarial cycle 2 of #661). Such a tab waits at most 5 s for a
  helper and 5 s on its fallback, about 10 s, the two-stall limit below.
  The spawn runs on its caller, which since #722 is the app's spawn
  queue, not its main thread (`TesseraApp.spawnInBackground`, one spawn
  at a time): this is how long the tab shows its placeholder before its
  session or its failure, the worst case of these bounds, accepted
  because the tab now opens where it failed; the app stays responsive
  meanwhile, and its main thread waits for a spawn only when the core is
  shut down during one (a quit while a terminal opens). Skipping the
  fallback after every refusal was rejected: it would fail the tabs the
  reserved helper exists to start. Not bounded by this: a profile whose
  working directory and the `$SHELL` both stall waits 5 s on each (the
  fallback is tried after the directory's timeout and stalls on `$SHELL`),
  10 s in all, as before #661 (#630).
- **Late helpers are not stuck.** A helper counts toward the cap of 4
  stuck ones only once it has run its own 5 s without an answer. A
  caller that waited for a free helper and then gave up at its deadline
  leaves a helper that has run less than that: it is late, keeps its
  place (so at most 4 + 1 helper threads exist all the same), and is
  counted stuck from the caller's deadline plus the time the caller
  queued. A caller waiting for a helper is refused at once only while
  every place holds a stuck helper, and a waiting caller wakes when a
  late helper turns stuck, so it is refused then rather than at its own
  deadline. Before this, the fifth of five fallbacks queued on the
  reserved helper behind a slow `$SHELL` was counted stuck when its 1 s
  floor ran out, and the next tab was refused although no lookup had
  stalled.

## Amendment (#722): spawns run off the main thread

`tessera_session_spawn` may be called from any thread (concurrently with
every call but `tessera_app_free`), and the app calls it on one serial
background queue (`TesseraApp.spawnInBackground`). A new window or tab
opens at once in its place, showing a placeholder (a spinner on the
window background), and becomes the session's terminal when the spawn
returns, or closes and shows the error; a window closed meanwhile gets
nothing, and its session is closed when it arrives. A split, and a
restored tab's further panes, appear once their sessions run. The serial
queue starts the sessions asked for together (a restore) in the order
asked, so their ids follow the tabs' order; the placeholders keep the
tabs' order whatever the spawns do. The cost: with several tabs behind
one stalled lookup they open one after the other, each within the bounds
above (and refused at once once the helpers are all stuck). A core free
(quit, a core replaced) waits for a spawn in flight, at most those
bounds, and the free closes the session it started; a window that was
still waiting is not saved at that quit. Events a session emits before
its spawn returns are held for it in the callback bridge while a spawn
is in flight and delivered once it registers. Keys typed at the
placeholder are kept (256 at the most, docs/security.md) and given to the
session's view in order once it runs, before what is typed after; they
are dropped if the tab closes or its spawn fails. The spawn's completion
runs from a main run loop block rather than a main-queue job, so an error
alert it shows leaves the main queue (session deliveries, the termination
signal sources of #441) running.
