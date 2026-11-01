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

The Swift half (confirmation alerts per tab, window and Cmd-Q, and the
window-state store) is recorded here by 34-app.

## Consequences

- The frontend can ask before closing a busy session with one call per
  session; Cmd-Q calls it once per session to build its single list (D4b).
- A job that changes between the check and the close (it finishes, or a
  new one starts) is decided by the check; the close itself never waits
  (ADR 0018).
- Very rarely the leader's pid could be recycled between `tcgetpgrp` and
  the name lookup, giving the wrong name for a job that has just ended;
  `running` is still right at the time of the call.
