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

## Amendment 2026-10-29: `themes/` is watched; one watcher per App in app-core (#100)

**What is watched.** Besides `config.toml`, the watcher follows the themes
directory. The native backend registers `themes/` (a theme file created,
deleted or renamed, an editor's rename-save) and the theme file
`appearance.theme` currently resolves to (an in-place write, which raises
no directory event). The theme file is looked up again after every settled
change and re-registered only by itself: re-registering a directory drops
its pending events, so the directories are never re-armed for it, and at
thread start the file is re-registered only when the lookup names a
different file, so a write already pending on it is not dropped. A theme
file that is not in use is not registered, so an in-place write to it is
not seen; it cannot change the theme in use. A missing `themes/` does not
make the native backend poll, since the root directory's watch sees it
come back. A theme file in use that stops loading (a half-written save, a
typo) is kept registered while it exists, found again by its stem
(`files_named`) or kept from the previous lookup, so the save that fixes it
is seen. The polling backend samples, on each tick, `config.toml`, the
`themes/` directory itself (an entry created, deleted or renamed changes
its metadata) and the theme file in use, and every twentieth tick the
metadata of every `*.toml` entry the theme lookup would consider (not
hidden, UTF-8 name, at most `MAX_THEME_DIR_ENTRIES`): a directory of
thousands of themes costs one listing a second, not twenty. A settle
resolves the theme twice (the watcher to find the file to follow, the
consumer to apply it); both read only the themes directory and at most
`MAX_THEME_FILES` files, once per settled change, so this stays. Both debounce a `themes/` change
like `config.toml` and report it as `ConfigEvent::ThemesChanged`, an event
of its own. The watcher does not resolve the theme; the consumer does, and
applies it only when the resolved theme differs. The store records the
directory's state at each sample, so several watchers of one store report
one change once, and the first sample is a baseline that reports nothing.
A native watcher costs about seven descriptors, up from five.

**Who watches.** `tessera-app-core` runs one watcher per `App`
(`App::start_config_watch`), not the FFI layer or the Swift app, so every
frontend (the app, devwin, a future Linux shell) gets live reload from one
place (devwin starts it through `App::with_config_dir` as the app does). A
second thread, `tessera-config-apply`, consumes the events. It
blocks on the watcher's channel (`Watcher::into_parts` hands it the
receiver and keeps the thread's `WatchGuard` in the `App`), so it never
wakes while idle. On `Changed` it applies the new configuration with the
re-resolved theme (`App::apply_config`). On `ThemesChanged` it re-resolves
and applies only a theme that differs. On `Invalid` it logs and keeps the
last good configuration. Dropping the `App` drops the guard first. That
stops the watcher and disconnects the channel, and the apply thread is
joined within `CONFIG_WATCH_STOP_CAP` (1 s; past it the thread is left to
end on its own, logged, which only an observer callback that blocks can
cause). Dropping the `App` still waits for an `on_appearance_changed` in
progress, for as long as it runs: removing the observer
(`set_observer(None)`, `tessera_app_free`) waits for every observer call,
as it does for session events. An observer that replaces the observer
(`tessera_app_set_callbacks` from the callback, which the C contract
forbids) therefore deadlocks.
`tessera_app_new` starts the watch when the configuration directory
exists. When it does not, the apply thread looks for it once every
`CONFIG_DIR_POLL_INTERVAL` (2 s, one `stat`; the native backend's own
fallback would poll every 50 ms) and starts the watcher, applying what the
directory holds, once it appears: made by hand, by
`tessera_app_create_config` (which starts the watch at once) or by the
first session's shell integration (#44). A directory whose parent is
missing too (only a `TESSERA_CONFIG_DIR` naming nothing can be: the user's
Application Support always exists) is not waited for; Open Config creates
it and starts the watch. The app's Xcode test runs point there on purpose,
so they run without these threads: under the Thread Sanitizer, a `fork`
for a session could deadlock with the Thread Performance Checker
generating a backtrace for one of them (TSan's slot locks against libc's
locale lock), and fewer waking threads make that rarer. Starting a watch on a store whose
configuration differs from the `App`'s applies it (`reconcile`). See ADR
0004 for the startup load.
