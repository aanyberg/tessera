# ADR 0018: Non-blocking session close

**Status:** accepted · 2026-10-29

## Context

`App::close_session` used to finish the whole close on the caller's thread:
mute the session, hang up the child, wait up to `HANGUP_GRACE` (1 s) for
it to exit, send `SIGKILL`, wait up to `KILL_GRACE` (1 s) more and join
the session's reader and writer threads. The caller is the main thread
(`TesseraSession.close()` → `tessera_session_close`), so closing a tab
froze the UI for as long as the child took to die: about 3 ms for a shell,
about a second for a child that ignores `SIGHUP`, and that second once per
tab when a window closes its tabs one after another (issue #79 measured
20.5 s for 20 tabs). Xcode's Thread Performance Checker also reported a
priority inversion: the main thread waits on locks held by the core's
threads, which ran at the default quality-of-service class.

Two contracts had to survive:

- **No callback after close.** Once `tessera_session_close` returns, no
  callback for the session runs, and `on_session_exit` is never called for
  it. TesseraKit unregisters the session as soon as close returns, and the
  C header promises silence. The issue's "deliver the exit through the
  existing callback" was rejected by the lead for this reason.
- **`tessera_app_free` waits.** The macOS app calls it from
  `applicationWillTerminate`, and the host may free `user_data` once it
  returns, so it still has to finish every close, capped at about two
  seconds however many children ignore `SIGHUP`.

## Decision

### The close state machine

A session is **live** while it is in `App`'s session map, **closing** from
the moment `close_session` takes it out of the map until the reaper has
finished with it, and **reaped** once its reader and writer threads have
been joined (or, for a terminal still held open after `SIGKILL`, detached).

`close_session(id)` does only what cannot wait, in this order, and returns:

1. Remove the id from the map. From here every call with it, a second
   close included, fails with `SessionNotFound`; `session_ids()` no longer
   lists it. Ids are never reused, so a closing session's id can leave the
   map at once.
2. Detach the view (its surface is released on the calling thread, the
   main thread, as before).
3. `Session::begin_close`: set `muted` (no observer call starts after
   this), close the outbox (pending input is dropped) and hang up the PTY.
   `Pty::hangup` closes the PTY's write queue and sends `SIGHUP`; neither
   waits for the child (#75, below).
4. `Session::wait_for_delivery`: wait for an observer call of the session
   that started before the mute. This is the only wait in a close, and it
   lasts as long as the host's callback, which must return quickly.
5. Hand the session to the reaper with a kill deadline of now +
   `HANGUP_GRACE`.

The barrier in step 4 is per session. The reader thread now holds a
per-session *delivery lock* around the `muted` check and the observer
call (`Pump::deliver`, after taking the observer slot's read lock), and the
closer takes that lock once after muting. Before, the closer took the
observer slot's write lock, which waited for every session's observer call
and deadlocked when the close came from inside any observer call (the
calling thread holds a read lock). A close made from inside an observer
call *of the session being closed* now skips the barrier (it compares the
current thread with the session's reader thread): the call it would wait
for is the one it is made from, and once that call returns the reader sees
`muted` and delivers nothing more. `tessera_*` functions still must not be
called from a callback; the exemption is a Rust-level property that keeps
`App::close_session` from deadlocking where it no longer has to. Such a
close releases the session's view on the reader thread, and views belong
to the main thread (ADR 0014), so the `close_session` documentation asks
the caller to detach the view on the main thread first. The exemption was
not restricted to sessions without a view: refusing the close (or skipping
the view) would turn a documentation rule into a surprising error.

### The reaper

One reaper per `App`, not one per session: a thread named
`tessera-reaper`, started by the first close and kept until the `App` is
dropped, with a queue of closing sessions. For each one it runs the grace
logic `close_all_of` used to run on the caller's thread:

- when the session's reader and writer threads have both ended, join them
  (which then takes no time) and drop the session;
- at the kill deadline, if they have not, send `SIGKILL` and set a detach
  deadline of now + `KILL_GRACE`;
- at the detach deadline, detach whatever is still running (logged).

The reaper sleeps on an event count (`Signal`) until the earliest deadline
in its queue, or indefinitely when the queue is empty. It is woken by a new
hand-over, by the shutdown, and by every session thread that ends: each
reader and writer thread is started through `Worker::spawn`, whose
`Finished` guard, dropped as the thread's last act, disconnects the
thread's `done` channel and then bumps the count. So a shell that exits on
`SIGHUP` is joined a few milliseconds after its close, not at a polling
interval, and an idle reaper costs nothing. Each session keeps its own
deadlines from its own close, so sessions closed together share their grace
periods exactly as `close_all_of` shared them before.

If the reaper thread cannot be started, the sessions stay queued (muted,
hung up, out of the map) and the next close tries again; dropping the `App`
reaps them on the dropping thread.

A panic must not strand closing children, which `NativePty`'s drop would
only send `SIGHUP`. The start hook runs under `catch_unwind` (a panicking
hook is logged and the thread carries on), and so does each step of a
close: a step that panics is finished the hard way, with `SIGKILL` and a
join-or-detach, rather than retried at a deadline already past. If the
reaper thread still dies, `Reaper::shut_down` sees the failed join and runs
the loop over whatever is still queued on the dropping thread. One gap
remains: a panic in the loop itself, outside a step (after it has taken the
incoming queue), drops the sessions it had taken without `SIGKILL`, and
the thread is not restarted, so later closes wait for the drop. Nothing in
the loop is known to panic; making it restart is a follow-up.

### `App` drop and `tessera_app_free`

Dropping the `App` detaches every view, runs steps 3 and 4 for every
session still live, queues them with the reaper without starting its
thread, sets the shutdown flag and joins the thread if a close started
one. The reaper finishes its queue, then exits. A session closed earlier has
an earlier kill deadline than those closed by the drop, so the drop waits
at most about `HANGUP_GRACE + KILL_GRACE` (two seconds) after it starts,
however many sessions are closing: the existing cap is unchanged, and so are
the bounds in `closing_many_stubborn_sessions_shares_the_grace_periods`
and `closing_sessions_that_survive_kill_detaches_within_both_graces`. When
no close ever started the reaper thread (or it could not be started, or
it died), the drop runs the same loop on its own thread instead. The observer slot is emptied last, as
before, so a detached reader does not keep the host's callback table.

`tessera_app_free` is still blocking, and TesseraKit's `shutdown()` still
calls it; only the closes before it got faster. The Swift-side close loop
for sessions with a write in flight (`CoreHandle.free`) now costs a few
microseconds per session instead of up to two seconds each.

### What #75 contributes

The reaper only works because a hangup no longer stalls. Since #75,
`Pty::hangup`, `Pty::kill` and dropping a `NativePty` close the PTY's write
queue before they signal, and the PTY writer writes in 1 KiB slices. Before
that, a cooked-mode child with a large paste pending kept `kill(2)` waiting
behind seconds of `write(2)` in the kernel, so `begin_close` itself could
block. ADR 0011 records one residue that this ADR inherits: a child that
ignores `SIGHUP` and never reads keeps the PTY's writer thread asleep in
`write(2)` until it dies. That wait is now the reaper's (the child is
killed at its kill deadline, the writer wakes, and the reader's event
stream ends), never the caller's.

### Quality of service

The core's own threads (each session's reader `tessera-session-<id>` and
input writer `tessera-input-<id>`, and `tessera-reaper`) run at
`QOS_CLASS_USER_INITIATED` on macOS. The main thread runs at
`USER_INTERACTIVE` and waits on locks these threads hold: the terminal lock
while the reader applies a slice of output (ADR 0014), a session's delivery
lock while a callback runs, the PTY lock during a write attempt. At the
default class the scheduler may leave the holder descheduled behind other
work while the main thread waits, which is the inversion Xcode's Thread
Performance Checker reports. `USER_INTERACTIVE` was rejected: output
parsing must not compete with the main thread for the CPU.

`pthread_set_qos_class_self_np` is an `unsafe` C call, and
`tessera-app-core` forbids `unsafe` (`docs/architecture.md`, invariant 6),
so the brief's "helper in `session.rs`" became a safe hook there:
`session::set_thread_start_hook(fn())` installs a process-wide function
(first one wins) that every thread the crate starts runs first.
`tessera_app_new` installs one that calls
`pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0)` on macOS and
does nothing elsewhere; the `unsafe` stays in `tessera-ffi`. The hook is a
`OnceLock<fn()>`: setting it is atomic, a second call (every later
`tessera_app_new` in the same process) returns `false` and changes nothing,
and a thread started before it was set simply keeps the default class.
Rust-only users of `App` (the crate's tests, `devwin`) never set it and run
these threads at the default class.

`tessera-pty`'s own reader and writer threads keep the default class: the
crate was out of scope, and the main thread never waits on them for long.
Checked against `native.rs` after #75: the main thread reaches two
`tessera-pty` locks, the write queue's (`Pty::hangup`/`kill` →
`WriteQueue::close`) and the child's phase lock (`signal_if_running`). The
PTY writer holds the queue lock only inside `WriteQueue::pop`, to take one
chunk off the front, and releases it while it waits for input (condition
variable) and while it writes (`write_chunk` reads the closed flag as an
atomic, without the lock). The PTY reader holds the phase lock only to
store `Reaping` before `waitpid` and `Exited` after it, never across the
wait. Both are constant-time sections, so an inversion there lasts a few
instructions.

Verified headlessly: `every_core_thread_runs_the_start_hook` checks that
all three kinds of thread run the hook, and
`session_threads_run_at_user_initiated_qos` (`tessera-ffi`) reads the class
of the thread an output callback runs on with `pthread_get_qos_class_np`.
To verify the inversion is gone (manual, lead): in Xcode, edit the Tessera
scheme → Run → Diagnostics → enable **Thread Performance Checker**, run the
app, open a tab running `yes`, resize the window and close the tab while
output floods, and check that no "Hang Risk: Thread running at
User-interactive quality-of-service class waiting on a lower QoS thread"
issue appears in the Issue navigator.

## Measurements

Release build, Apple silicon, load average 7 to 10, three rounds each, with a
throwaway probe against the public `App` API on the merge base (5e6c6eb)
and on this branch. "Stubborn" is `sh -c "trap '' HUP; echo ready; exec
sleep 100"`, closed once its `ready` is on screen.

| | before (5e6c6eb) | after |
|---|---|---|
| `close_session`, idle shell | 0.40–0.55 ms | 0.06–0.08 ms (first close, starts the reaper) |
| `close_session`, one stubborn child | 1.001–1.010 s | 0.020–0.034 ms |
| 20 stubborn children closed in a loop | 20.14–20.18 s (worst single 1.01 s) | 0.18–0.45 ms (worst single 0.03–0.06 ms) |
| `App` drop right after that loop | < 0.01 ms (nothing left) | 1.004–1.021 s (waits for the background kills) |
| `App` drop with 20 live stubborn children | 1.002–1.011 s | 1.009–1.026 s |

Closing a window of 20 `SIGHUP`-ignoring tabs went from 20 s of frozen UI
to well under a millisecond; quitting still takes the one shared second
it took before. The tests bound one close (fake and real child) and a
close during a `yes` flood at 5 ms. The loops of 20 (fake and real) hold
the median close to 0.25 ms (the issue's 5 ms for 20) and the whole loop
to `HANGUP_GRACE` / 4: a loaded scheduler preempts a few closes, and a
5 ms bound on the whole loop failed at 15 ms (load 10) and 51 ms
(background priority, three test processes), while a close that waited
for a grace period would still fail both. The reap
at `HANGUP_GRACE` + 500 ms for fake PTYs and `HANGUP_GRACE` +
`KILL_GRACE` + 500 ms for real children. The reap is timed by the
reaper's own count of unfinished closes (a session's reader ends only
after the PTY reports the exit, which follows `waitpid`), and one `ps`
afterwards confirms that no pid is left, zombies included. The 500 ms
slack covers a reaper scheduled late: under `taskpolicy -b` fake closes
finished 1.21–1.25 s after the 1 s grace. The tests run in nextest's
`timing` group.

## Consequences

- Closing a tab or a window never waits for a child; `just app-test`'s
  `shutdown()` paths only wait once, in `tessera_app_free`.
- A closed session stays in memory, terminal and scrollback included,
  until it is reaped, and a `SIGHUP`-ignoring child keeps running for up
  to `HANGUP_GRACE` after the close, invisible to the host (no callback,
  no id). The reaper drops the session as soon as its threads are joined.
- `App` holds one more thread, from the first close on, which sleeps on a
  condition variable when nothing is closing.
- ADR 0011's "the writer stops within one write backoff period and is
  joined, so a close during a pending paste returns promptly" now reads:
  the reaper joins it; the close returns at once either way.
- #34 (close confirmation, window state restore) can close many sessions
  from the main thread without a progress UI.
