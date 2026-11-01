# ADR 0031: Close confirmation and window-state restore

**Status:** accepted · 2026-11-01

## Context

Issue #34: closing a tab or window whose shell is running a command
(`sleep 100`, an editor, a build) kills that command without warning, and a
relaunch opens one empty window whatever was open before. Terminal.app
and iTerm2 ask before closing a busy session and bring the windows back.

The frontend cannot tell whether a session is busy: the PTY, the child's
pid and the terminal's process groups live in the core. The issue names the
mechanism: compare the PTY's foreground process group with the shell's.

The owner decided the behaviour (D4, 2026-11-01):

- **(a)** Every foreground job other than the shell asks before it is
  closed, `tmux`, `ssh` and editors included. There is no ignore list and
  no configuration key in 0.1.0.
- **(b)** Cmd-Q with busy sessions shows one alert that lists them, not
  one alert per session.
- **(c)** Restore covers each window's frame and its tab count. Restored
  tabs start fresh shells in the default directory; restoring each tab's
  working directory is #61 (0.2.0). Restore is skipped when macOS's "Close
  windows when quitting an application" is on (`NSQuitAlwaysKeepsWindows`
  false).

## Decision

### Core: the foreground job (34-core)

**Detection.** An interactive shell with job control puts every job it
starts in a process group of its own and hands it the terminal
(`tcsetpgrp`); at its prompt it takes the terminal back. The shell is the
session leader, so its group id is its pid. A session is busy exactly when
`tcgetpgrp` on the PTY master names another group.
`tessera_pty::Pty::foreground_process() -> Option<ForegroundProcess { pid,
name }>` does that comparison (`crates/tessera-pty/src/foreground.rs`,
through `portable-pty`'s `MasterPty::process_group_leader`). `NativePty`
holds its phase lock across the comparison, the same lock that keeps
signals from reaching a reaped pid, so the shell's pid cannot be recycled
under it; once the child is being reaped or has been, the answer is
`None`. The trait method's default implementation answers `None`, so the
test PTYs need no change.

Consequences of the mechanism, accepted:

- A program spawned directly (`Program::Command` that is not an
  interactive shell) or under `sh -c` runs in the child's own group and
  never counts as busy. The app always spawns login shells.
- Background jobs (`sleep 100 &`) do not count: the shell owns the
  terminal. A close hangs up the shell as before, whatever it does with
  its background jobs.
- Three busy cases read as idle, because the work runs in the shell's
  own group: a program the shell `exec`s (`exec sleep 100`, or an rc
  file's `exec tmux`, which D4a would have prompt), since it takes over
  the shell's pid and group; command substitution (`echo $(sleep 100)`,
  `vim $(fzf)` while `fzf` runs), whose child the shell runs in its own
  group while it keeps the terminal; and a shell with job control off
  (`set +m`). Catching them needs a second signal: counting the shell's
  own children in its group (`proc_listchildpids` and their group ids)
  and noticing an `exec` by the child's name changing from the shell's.
  That changes the mechanism and is deferred to 0.2.0.
- A job counts whatever it is (D4a); there is no allow-list of "safe"
  programs such as `tmux` or `ssh` in the core or the app.

**Name.** The name is the group leader's (the job's first process, `cat`
in `cat | less`): `proc_name` on macOS (at most 31 bytes, as the kernel stores it). The kernel refuses
`proc_name` for another user's process, which is what `sudo vim` leaves in
the foreground, so it falls back to the 16-byte command name from
`proc_pidinfo(PROC_PIDT_SHORTBSDINFO)`, which it reports for any process.
Linux reads `/proc/<pid>/comm`; other Unix systems report no name. Names are
decoded lossily as UTF-8 and cut to 64 bytes (`MAX_NAME_BYTES`) at a
character boundary. When the leader has exited while the rest of its
pipeline runs on, the name is empty and the job still counts. Both macOS
lookups are the only `unsafe` the feature adds, confined to
`foreground.rs` with `SAFETY:` comments; `libc = "0.2"` became a workspace
dependency, used by `tessera-pty` on macOS only.

**App and FFI.** `App::foreground_process(id) -> Result<Option<
ForegroundProcess>>` takes the session's PTY lock, which input writes hold
only for a non-blocking attempt, so it never waits on a child that stopped
reading. It fails with `SessionNotFound` for an unknown or closing session.
The C call is:

```c
#define TESSERA_FOREGROUND_NAME_LEN 64

typedef struct TesseraForegroundProcess {
  bool running;      /* a job other than the shell owns the terminal */
  int32_t pid;       /* its process group id (the leader's pid); 0 if idle */
  char name[65];     /* UTF-8, NUL-terminated, zero-filled; may be empty */
} TesseraForegroundProcess;

TesseraError tessera_session_foreground_process(TesseraApp *app,
                                                uint64_t session_id,
                                                TesseraForegroundProcess *out);
```

`*out` is zeroed first, so an idle shell, an exited shell and every failure
(`NULL_POINTER`, `SESSION_NOT_FOUND`) all leave `running` false. The call is
safe from any thread and costs one `tcgetpgrp` (plus one or two process
lookups when a job runs) and two uncontended mutexes; it is a snapshot, read
when the user asks to close, never per frame. `running` is a separate flag
rather than "`pid` is non-zero" so the frontend does not need to know how
the core encodes an idle session.

Alternatives rejected:

- **Walking the shell's children** (`proc_listchildpids`) would count
  background jobs and short-lived helpers the shell's prompt runs; the
  foreground group is what the user is looking at.
- **An event when the foreground job changes.** Nothing in the PTY
  signals a `tcsetpgrp`, so it would mean polling every session; the
  frontend only needs the answer at close time.
- **Shell-integration marks (OSC 133 C/D) instead of process groups.**
  They only exist when the integration is installed and the shell is not
  replaced (`ssh`, `tmux`, `exec`), and a command that the user suspended
  would look idle.

### App: confirmation and restore (34-app)

**Asking.** `TesseraSession.foregroundProcess()` wraps the C call on the
main actor and returns `nil` for an idle shell. The app calls it only when
the user asks to close, once per affected session, through
`CloseConfirmation` (`apps/macos/Sources/Tessera/CloseConfirmation.swift`),
which is `nil` when no session is busy, so an idle tab, window or app
closes exactly as before:

| request | asks when | alert |
|---|---|---|
| ⌘W, the tab's or window's close button (AppKit closes only the selected tab) | the tab's session runs a job | sheet on the tab, which is selected first: "Close this tab?" |
| ⇧⌘W (`closeTabGroup`) | any tab of the window runs a job | one sheet on the current tab listing every busy tab's job (D4b) |
| AppKit's own multi-tab closes (Close Other Tabs, Close Tabs to the Right, Close All), which close each tab through `windowShouldClose` | that tab's session runs a job | idle tabs close at once; each busy tab asks on its own sheet, as for ⌘W |
| ⌘Q, the Dock's Quit, logging out (`applicationShouldTerminate`) | any session runs a job | one application-modal alert listing them all (D4b) |

Close (Quit) is the default button and marked destructive, as Terminal.app
makes Terminate the default; Cancel (Escape) keeps every tab and its job.
After Close the tabs close through `NSWindow.close()`, the same
non-blocking session close as before (ADR 0018), without asking again. A
close request for a tab that is already asking is ignored, so ⌘W pressed
twice asks once, and while ⇧⌘W asks, a close request for any tab of that
window is ignored; a tab's own question holds back none of its
neighbours, so an idle tab next to an asking one still closes. `SIGTERM`, `SIGINT` and `SIGHUP`
still quit without asking: there may be nobody to answer. A session the
core no longer knows counts as idle. The presenter is a protocol
(`CloseConfirmationPresenter`), so the hosted tests record the requests
and answer them.

**Untrusted names.** A process's name is the name of the file it runs,
which anyone can choose, so it can hold escape sequences, BEL, line
breaks or bidirectional overrides. The alert shows
`CloseConfirmation.displayName`: every control, format, line or paragraph
separator and unassigned scalar becomes U+FFFD, spaces are trimmed, and an
empty name reads "unnamed process". At most ten processes are listed,
then "and N more". `NSAlert` text is plain text, so nothing else in a
name is interpreted.

**Window state.** `WindowStateStore` (`WindowStateStore.swift`) keeps one
JSON value under the user-defaults key `TesseraWindowState.v1`: each
window's frame (the selected tab's) and tab count, in the order the windows
opened. It is saved on quit (`applicationShouldTerminate`, once the quit
is decided, before the core shuts down) and, coalesced to one save per turn of the main run loop, when
a window or tab opens or closes, a window moves or is resized, or a tab
becomes main (how a tab moving between windows shows). At launch, unless
"Close windows when quitting an application" is on
(`NSQuitAlwaysKeepsWindows` false or unset, which is macOS's default, as
for AppKit's own restoration; the setting can be overridden per
application with `defaults write com.aanyberg.tessera
NSQuitAlwaysKeepsWindows -bool true`), the windows are recreated: fresh
login shells in the home directory (D4c), each window at its frame moved
and shrunk to lie wholly on the screen it overlaps most, or on the main
screen if it overlaps none, and at least 160 x 80 points. Windows whose
frame is not finite or empty are dropped, tab counts are at least one and
at most 64 sessions in all are started, so a damaged value cannot start
thousands of shells; an unreadable value is ignored. No saved window means
one new window, as before. The hosted tests never save to the user's
defaults: the store is unset in the test host and the tests use a suite of
their own.

Not covered, accepted for 0.1.0: the order of windows in depth, which
tab was selected, and whether a window was full screen or minimized are
not restored (a full-screen window comes back as a normal window of the
screen's size); ⌥⌘Q (Quit and Keep Windows) is not offered; and restore
follows only `NSQuitAlwaysKeepsWindows`, not the logout panel's "Reopen
windows when logging back in".

## Consequences

- The frontend can ask before closing a busy session with one call per
  session; Cmd-Q calls it once per session to build its single list (D4b).
- With macOS's default "Close windows when quitting an application" on,
  a relaunch opens one window, as before; the windows come back only when
  the user has turned that setting off.
- A job that changes between the check and the close (it finishes, or a
  new one starts) is decided by the check; the close itself never waits
  (ADR 0018).
- Very rarely the leader's pid could be recycled between `tcgetpgrp` and
  the name lookup, giving the wrong name for a job that has just ended;
  `running` is still right at the time of the call.
