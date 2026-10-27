# ADR 0006: Config file watching via kqueue, polling as fallback

**Status:** accepted · 2026-10-27

## Context

ADR 0004 requires a file watcher that reloads external edits to
`config.toml`. Issue #33 suggested the `notify` crate. A terminal emulator
sits open all day, so an idle watcher must not wake up periodically, and an
editor's rename-save (write a temporary file, rename it over the target)
must be seen as an edit.

`notify` is licensed CC0-1.0, which `deny.toml` does not allow and which the
project policy keeps out of the dependency graph (permitted: MIT, Apache,
BSD, ISC, Zlib, Unicode, MPL). Binding FSEvents or kqueue directly from
`tessera-config` would need `unsafe`, which is reserved for `tessera-pty`
and `tessera-ffi`.

## Decision

`tessera-config` watches with the MIT-licensed `kqueue` crate
(`WatchBackend::Native`): the configuration directory and the file are
registered with `kqueue(2)` and the watcher thread blocks in the kernel
until a vnode event or the stop pipe fires. The directory watch catches
rename-saves, deletes and recreates; the file watch is re-registered after
every event so it follows the current inode.

The earlier metadata-polling loop stays as `WatchBackend::Poll`: the
fallback on platforms without `kqueue`, selectable at runtime so both
backends are exercised by the same tests on macOS. Its interval is clamped
to `MIN_POLL_INTERVAL` so it can never spin. It compares mtime, size,
inode and ctime, so a same-size in-place rewrite inside the volume's
timestamp granularity (coarse on SMB or ExFAT) is the one edit it misses.

Both backends feed one debounce-and-apply path: the file must be quiet for
`WatchOptions::debounce` before it is re-read, rapid writes coalesce into
one `ConfigChanged`, a failed load keeps the last good configuration and
reports `ConfigEvent::Invalid`, and the first successful load after a
failure always reports `Changed` so the application can clear its error.
The debounce is bounded by `WatchOptions::max_wait` (1 s by default): a
writer that never goes quiet cannot postpone a reload indefinitely. Every
duration in `WatchOptions` is also capped at `MAX_WATCH_DURATION` (an
hour), so no setting can overflow a deadline or a kernel timeout and kill
the thread. Should the thread die anyway, a guard on it closes the
watcher's channel (the consumer sees `Disconnected`, not silence) and logs
at error level.

Events are applied to the `ConfigStore` once and then broadcast to every
`Watcher` of that store, so the application may hold one watcher and fan
out to its windows (the intended shape) or one per window; either way each
watcher sees each change exactly once.

If the configuration directory itself is removed, `kqueue` keeps reporting
on the dead inode and nothing can wake the thread when the directory comes
back, so the native backend polls at `WatchOptions::poll_interval` until
the directory exists again, re-registers it, and returns to sleeping on
kernel events. Registrations are made from handles the backend owns, and a
handle whose path no longer names its inode right after arming (removed or
replaced between the open and the `kevent` call, which can never fire
`NOTE_DELETE`) is treated as missing. The check compares device and inode
numbers rather than the link count, since APFS still reports two links for
an open directory after `rmdir`.

A `config.toml` that is a symlink (into a dotfiles checkout, say) is
followed to its target: the directory holding the final target is
registered as well, so a target deleted and recreated by `git checkout` or
`rm` plus rewrite is seen even though nothing in the configuration
directory changes. While that directory is missing the backend polls, as
for a missing root. The file is registered last, after the root and the
target's directory: re-registering a directory drops its pending events,
so a rename-save of the target landing between opening the file and
re-arming that directory would otherwise leave the file watch on the old
inode and miss the next in-place write. The file's type is checked with `stat` before it is
opened, so a FIFO in place of `config.toml` cannot block the watcher.

A path that exists but cannot be registered (`config.toml` with mode 000,
a symlink cycle, a target in an unreadable directory) is treated the same
way: the watcher starts anyway, as `ConfigStore::open` tolerates the same
file, and polls until every registration succeeds, then re-reads the file
and goes back to kernel events. Giving up on the file instead would miss
every later in-place rewrite, which produces no directory event.

Each `Watcher` costs a thread, a kernel queue and about five descriptors;
the application holds one per process and fans events out to its windows.

## Consequences

- `notify` is not used; if its licensing changes or a compatible wrapper
  appears, the `Native` backend can be swapped without touching the
  `ConfigStore`, `Watcher` or `ConfigEvent` API.
- Linux and Windows builds, which the project does not target, would poll
  at 50 ms. They would need an inotify / `ReadDirectoryChangesW` backend to
  match the idle behaviour.
- `kqueue` and `kqueue-sys` (both MIT) join the dependency graph on Apple
  and BSD targets only.
