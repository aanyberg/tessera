# ADR 0012: TesseraKit, the Swift wrapper over TesseraCore.h

**Status:** accepted · 2026-10-27

## Context

Until now the app called the C functions of `TesseraCore.h` directly
(`tessera_version()` in the app delegate). Issue #32 puts a Swift layer
between the AppKit code and the C ABI so that raw pointers, C strings,
status codes and the header's thread contract are handled in one place:

- Every `tessera_app_*` and `tessera_session_*` call is made on the UI
  thread, except `tessera_session_write`, which may be called from any
  thread concurrently with everything but `tessera_app_free`, and which
  can block with no time limit while a session's input queue is full.
- Callbacks run on the core's reader threads. They must return quickly,
  must not call back into the core, and must not wait for the main thread:
  closing a session, replacing the callbacks and freeing the application
  wait for a running callback, so a synchronous hop deadlocks.
- The callback `user_data` must stay valid until `tessera_app_free` (or a
  replacing `tessera_app_set_callbacks`) returns. Strings handed to a
  callback are valid only during it; pointers in `TesseraSpawnOptions` are
  borrowed for the spawn call only.

The app is built in Swift 6 language mode with strict concurrency checking
set to complete, so the wrapper must express this contract in the type
system rather than in comments.

## Decision

### Module split

A new Xcode target, `TesseraKit` (`apps/macos/Sources/TesseraKit`), is the
only code that imports the C module `TesseraCore`, and it does so with
`internal import`, so no C type appears in its interface. The app target
depends on `TesseraKit` and imports nothing else from the core;
`grep -rn "TesseraCore\|tessera_" apps/macos/Sources/Tessera/` stays empty.
Business logic stays in Rust: the wrapper converts types, maps errors and
marshals threads, nothing more.

`TesseraKit` is a **static** framework. With the hardened runtime, dyld's
library validation refuses to load a dynamic framework whose signature has
a different Team ID from the app's, and an ad-hoc signature has none: an
embedded dynamic `TesseraKit.framework` built that way fails at launch
("mapping process and mapped file (non-platform) have different Team IDs").
Linking statically leaves one signed executable, needs no
`disable-library-validation` entitlement, and keeps `just archive` an
application archive (`SKIP_INSTALL`). libtool merges `libtessera_ffi.a`
into `TesseraKit.a`; `-no_warning_for_no_symbols` silences the warnings for
Rust's empty object files.

Names: the wrapper's types are `TesseraApp` and `TesseraSession`, as the
issue asks. The C `struct TesseraApp` is opaque and imports into Swift as
`OpaquePointer`, so the names do not clash; the C enum `TesseraError` does
import, so the Swift error is `TesseraCoreError`. The app's delegate class,
previously also called `TesseraApp`, is renamed `AppDelegate`.

### Actor model

- `TesseraApp` is `@MainActor`, like the C handle. `coreVersion` is
  `nonisolated` because `tessera_version` is safe from any thread.
- `TesseraSession` is a `Sendable` class of immutable references.
  `write(_:)` is `nonisolated` and callable from any thread;
  `resize`, `clearScrollback` and `close` are `@MainActor`.
- `TesseraAppDelegate` is a `@MainActor` protocol; every event is delivered
  on the main actor.

### Callback bridge

`CallbackBridge` is a `@MainActor` class (and therefore checked `Sendable`,
which an `@unchecked Sendable` box would not be). It is the `user_data` of
the callback table, passed with `Unmanaged.passUnretained`. Each C
callback, running on a reader thread, recovers the bridge from `user_data`,
copies its arguments (the title string before returning), records the event
and, when needed, hops with `Task { @MainActor in ... }`.

Events are coalesced per session. The core calls back once per PTY read and
once per BEL byte, and a program can emit those far faster than any
delegate runs: with one task per event, `yes $'\a'` queued over a million
main-actor tasks within seconds (gigabytes of memory, a minute to quit). So
the bridge keeps, under a `Mutex`, a pending record per session: an output
flag, the latest title, a bell flag and the exit code and signal. A callback
updates the record and schedules a delivery only when it creates the record,
that is when none is queued for that session; the delivery takes the record
and reports it as `sessionDidOutput`, then `didSetTitle` with the latest
title, then one `sessionDidRingBell`, then the exit. What is guaranteed:

- at most one delivery per session is queued, so memory and main-actor work
  are bounded by the number of sessions, not by the event rate;
- deliveries of one session run in order, because at most one is queued at
  a time and the next is scheduled only once the previous has taken its
  record; each reports everything received since the previous one;
- within one delivery the fixed order above replaces the order of arrival;
  a title is reported once with its latest value and a bell once however
  many rang;
- the exit, the core's last callback for a session, is always the last
  event delivered for it.

Tests drive the C function pointers from a background thread to check this,
and a regression test floods a real session with `yes $'\a'` and bounds the
footprint, main-actor lag and shutdown time.

On the main actor the bridge maps the session id to the `TesseraSession`
and calls the delegate, looking both up again before each call, since the
delegate may close the session or shut the app down from inside one. Events
for a session that has been closed (or not yet registered, or arriving after
shutdown or while no delegate is set) are dropped. Events emitted before
`tessera_session_spawn` returns are not lost: `spawn` registers the session
before the main actor can run the queued delivery.

The app's delegate additionally beeps at most once per 100 ms, since a
program ringing continuously still gets one delivery after another.

### Lifetime

Nothing relies on `deinit` to free the core. `TesseraApp`'s `deinit` is
nonisolated (Swift 6.2's opt-in `isolated deinit` needs a newer toolchain
than CI's 6.1), so it may run off the main thread, and the C handle must be
freed on the main thread.
`TesseraApp.shutdown()` is the contract: it frees the core and then drops
the bridge's sessions. `deinit` only acts as a last resort, with an
assertion in debug builds, freeing on the main thread (directly, or in a
main-actor task that keeps the bridge alive).

The app calls `shutdown()` from `applicationWillTerminate`. `SIGTERM`,
`SIGINT` and `SIGHUP` (`kill`, Ctrl-C, closing the launching terminal) would
end the process without it and orphan a child that ignores `SIGHUP`, so the
app arms a dispatch source on the main queue for each that
calls `NSApp.terminate`, then ignores the signal, whose default action would
otherwise kill the process first (the source still sees an ignored signal,
through kqueue). An ignored disposition survives `exec`, so this relies on the core's PTY spawn: portable-pty's `pre_exec`
resets `SIGTERM`, `SIGINT`, `SIGHUP` and others to `SIG_DFL` in the child
and clears its signal mask. A test spawns sessions while the test process
ignores all three and checks that each child still dies of them.

The bridge outlives the callback table: `TesseraApp` holds it, and
`tessera_app_free` returns only once no callback runs or can run again.

`tessera_app_free` must not race with `tessera_session_write`, and a write
may be blocked on a child that stopped reading. `CoreHandle`, which owns the
`TesseraApp *`, counts writes in flight per session under a lock. Freeing
withdraws the pointer (later writes pass `NULL` and the core answers
`TESSERA_ERROR_NULL_POINTER`), closes the sessions that have a write in
flight, which wakes a blocked write, waits for those writes to return, and
only then calls `tessera_app_free`. Sessions without a writer are left to
`tessera_app_free`, which shares one grace period across all children.

The cost is shutdown time when writes are in flight: the sessions with a
writer are closed one after another, and each can take up to about two
seconds (the hangup, then the kill grace): a child that ignores `SIGHUP`
takes about one (three such sessions measured 3.0 s), and so can a
cooked-mode child such as `exec cat > /dev/null` with a write blocked into
it. The free's own two-second bound comes after. With no write in flight
shutdown is just the free. While
it waits, the main thread blocks on writer threads that may run at a lower
QoS, a priority inversion `NSCondition` does not resolve (no QoS donation).
The wait is short in practice, because closing the session makes the blocked
write return at once, so it is accepted rather than worked around.

### Errors and options

Every non-OK `TesseraError` throws a `TesseraCoreError`, one case per
enumerator (`.panic` for `TESSERA_ERROR_PANIC`) plus `.unknown(rawValue:)`
so that a code added to the header later cannot crash the app. After
shutdown every call that needs the handle throws `.nullPointer`, the core's
own answer for a `NULL` handle.

`TesseraSession.SpawnOptions` is a Swift struct converted to
`TesseraSpawnOptions` inside nested `withCString` scopes (one buffer for all
arguments) so no pointer outlives the spawn call. A string containing a NUL
byte throws `.invalidArgument` before the call, since a C string would
silently truncate it.

### Tests

`TesseraKitTests` (XCTest, `just app-test`, also in the CI `app` job) runs
headless with the Thread Sanitizer: the lifecycle of
`crates/tessera-ffi/tests/c/lifecycle.c` through the wrapper, exit codes and
signals, error mapping, arguments and working directory reaching the child,
writes from plain threads and detached tasks, a write blocked on a child
that does not read being woken by `shutdown()`, and the bridge's copying,
ordering, coalescing and dropping, driven through the C function pointers,
and the bell-flood regression test.
The core does not apply OSC titles yet (#9), so the end-to-end test expects
the bell and the exit with no title, and fails on purpose, to be updated,
once titles arrive.

## Consequences

- AppKit code (#27 and later) uses `TesseraKit` only. The surface API of
  #25 and the input functions of #26 are wrapped there when they land.
- A second front end on another platform would write its own wrapper over
  the same header; nothing here is shared with Rust.
- Every new `TesseraError` enumerator needs a case in `TesseraCoreError`;
  until then it surfaces as `.unknown(rawValue:)`.
