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
and applies only a theme that differs. On `Invalid` it keeps the last good
configuration (the store logged the error; amendment 2026-11-05). Dropping the `App` drops the guard first. That
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

## Amendment 2026-11-05: settle order, unloadable themes, fail-closed rules (#253, #262)

**Settle order.** Every settle (a debounced change, and the catch-up a
watcher does when it starts) runs in one order on both backends: load
`config.toml`; follow the theme file the loaded configuration names (the
native backend registers it, the polling backend records it as the
baseline it samples); only then apply the result to the store and send it
to the watchers. A consumer that saves the theme file in reaction to
`Changed` (or a script that switches the theme and then writes its file)
is therefore always seen, at start-up too. The amendment of 2026-10-29
looked the theme file up after the store had published the change (#243,
#253 items 1 to 3).

This supersedes two sentences of that amendment. The native backend now
re-registers the theme file in use, and every unloadable one it follows
(below), at every settle, the catch-up included, not only when the lookup
names a different file. Re-registering drops an event already pending on
the file. That is safe because the settle samples the themes directory
(`report_themes`: the metadata of every `*.toml` entry the lookup
considers) after the registration: a write that landed before it, whose
event was dropped, has changed that sample and is reported as
`ThemesChanged` by the same settle, and a write after it raises an event
on the new registration. The directories are still never re-armed for
the theme file. A theme file that is neither in use nor unloadable is
still not registered.

**Unloadable theme files.** The native backend follows a theme file only
when it can tell it is the one in use: by its stem, by its `name` key, or
as the previous lookup's file. A file that does not load when a settle runs
(created empty by an editor's New File, recreated after `rm`, saved in
chunks around a switch) has no readable `name`, so nothing was registered
and its completing in-place save was lost for good (#253 item 8). While
the configured theme is not found as a user file, the backend now also
registers the `*.toml` files in `themes/` that fail to load, newest
modification first, at most `MAX_UNLOADABLE_FOLLOWED` (8; S5: a directory
of thousands of broken files costs at most eight descriptors), so the save
that completes the configured theme raises an event. These are kept to the
same best-effort rule as the theme file: one that cannot be registered is
logged at debug level and skipped. Every registration opens its path
non-blocking and checks the type on the open handle as well as with the
`stat` before it, so a FIFO renamed over a broken theme file between the
two opens at once and is skipped instead of blocking the watcher thread
and the drop that joins it (#287). The polling backend needs none of this;
its full scan every twentieth tick sees the save. A native watcher costs
about seven descriptors, plus at most eight while the configured theme
does not load.

**Symlinked configuration directory (#384).** `kqueue` watches inodes:
opening a configuration directory that is a symlink (GNU stow, a
home-manager generation) or lies below one (a dotfiles checkout switched
with `ln -sfn`) registers the current target, and retargeting the link
changes only the directory that holds it. The native backend therefore
resolves the root component by component, as the kernel does, and also
watches each directory holding a link on the way, at most
`MAX_DIR_LINKS_FOLLOWED` (8; S5), re-resolving on every re-registration
so it follows the new target. More links than that, or a cycle, register
none of them; the backend then counts as not armed and polls, settling
whenever `config.toml`'s metadata (followed through the links) changes,
so a retarget is still applied, a poll interval later. The polling
backend already follows names and needs none of this.

**Opacity on `ThemesChanged`.** The apply thread applies a re-resolved theme
with the store's configuration, the one whose `appearance.theme` it
resolved, not with the configuration it applied last. A `ThemesChanged`
handled after the store reloaded a `config.toml` that names another theme
and opacity (its `Changed` still queued) therefore applies the new theme
at the new opacity, not at the old one first (#253 item 4); the `Changed`
then changes nothing.

**Fail-closed rules (D8, docs/security.md).** A `config.toml` that does
not load before any load has succeeded keeps clipboard writes (OSC 52)
off. Two cases are added: a `config.toml` symlink whose target is missing
is a failed load, like a symlink loop, not a missing file (#262 item 1),
so at launch it refuses writes and while running it keeps the last good
configuration; so is a `config.toml` missing because the configuration
directory, or one above it, is a symlink whose target is missing (GNU
stow's layout, a dotfiles volume that is not mounted; #287): until the
directory appears, whether the directory itself is the link or one above
it (the application waits for the directory in both cases, #382 item 1,
and the store it then watches is the one that refused, so the refusal
carries over, #382 m2, except when ⌘, (`App::open_config`) runs in the
up to 2 s before the wait's next poll sees the directory: that started a
watch on a fresh store, and an empty file there turned writes on, until
#393 F1 closed it, see the amendment of 2026-11-12); and
while writes are refused this way, an empty (or
whitespace-only, after an optional byte order mark, which TOML skips;
#287) file applies the defaults with writes still off, since a save that
truncates first leaves the file empty for a moment (#262 item 9). Only a
non-empty file that loads turns them on. Under the settled
rule that a missing file is a successful load, a broken file made or
edited while running (after a successful load) keeps writes as they were
until it is fixed or Tessera restarts (#262 N1). A failed load is logged
once, by the store, when it differs from the previous failure (#262 item
6); the text it quotes from the file is escaped (controls, U+2028 and
U+2029, and the bidirectional formatting controls) before it is logged or
reported, as are theme lookup warnings (#262 items 4, 5 and 10; S8).

## Amendment: link chains, flood rearm, waiting store, profile themes (#393, #397, 2026-11-12)

**Every directory on the way.** The native backend no longer registers only
the directories that hold a link. It walks `config.toml`'s path the way
the kernel does (`chain_dirs`), and registers every directory it passes
through on the way to the file, except the one that holds the file's last
component (the root, or a linked file's target directory, registered
already). That includes the real directories above the root, so a
directory above it swapped by rename (`dirswap`), and a `config.toml`
symlink through a retargeted directory link (`filelink`), are applied
from an event, as fast as the poll backend applies them (#393 F3). The
walk is capped at 8 links (`MAX_DIR_LINKS_FOLLOWED`) and 32 directories
(`MAX_CHAIN_DIRS`); past either, or in a cycle, nothing on the way is
registered (S5). A directory on the way that cannot be read (`EACCES`)
and that the walk only passes through is left unwatched, with a debug
log, and does not unarm the backend: a change there is seen only from the
directories below it. One that holds a link the walk follows fails the
registration instead, since a retarget there changes nothing below it, so
the backend polls, as before this amendment (#393 B1).

**Resolve, register, resolve again.** A link retargeted between the walk
and the registrations could move the chain through a directory nobody
registered (#393 m1). After registering, the backend walks the path again
and registers anew until two walks agree, at most 9 times; a chain still
changing after that is a failed registration, and the backend polls.

**Past the cap, slowly.** A chain past a cap stays so until someone
changes the links, so the backend polls it every 2 s
(`CAPPED_CHAIN_POLL_INTERVAL`, the interval at which the application looks
for a missing configuration directory) instead of every `poll_interval`
(#393 m3): an idle watcher wakes at most once per 2 s. An edit in the
configuration directory still applies at once, from the directory's own
watch; a retarget on the way, within about 2 s. Each poll reloads only
when `config.toml`'s metadata (followed through the links) changed since
the last load.

**An unrelated change on the way changes nothing.** A directory on the
way is often busy (`~`, a dotfiles checkout). An event there re-registers
and reloads only if the walk now lists other directories or a registered
handle (the chain's, the root's, a linked file's target directory's, the
file's) no longer names what its path names (#393 m2). Such events are
debounced like the others, but they never extend the wait, and the walk
is made once at its end: a stream of them costs one walk per `debounce`,
and neither an edit of `config.toml` nor a change on the way during the
stream waits longer than the usual debounce. Under 70 changes a second the
watcher spends a few percent of the CPU time it did.

**`open_config` keeps the waiting store.** While the application waits for
a configuration directory behind a dangling link with a store that refused
`config.toml`, ⌘, (`App::open_config`) starts the watch on that store, not
a fresh one, so the refusal carries over and an empty or byte-order-mark
only `config.toml` keeps clipboard writes off (#393 F1). The fail-closed
rules of 2026-11-05 now hold without the exception noted there.

**Profile themes.** The theme files named by profiles (`[profiles.*]
appearance.theme`) are followed like the `[appearance]` theme: looked up
before an outcome is applied, and registered on the native backend so a
save in place (truncate and write, no rename) of one raises an event, the
sessions using that profile restyle and `on_appearance_changed` is called
(#397). The first 8 distinct ones (`MAX_PROFILE_THEMES_FOLLOWED`, in
profile order) are registered; past that, a profile theme is still read at
each reload but saved in place it is applied only with the next change in
`themes/` or `config.toml`. The poll backend needs no change: it already
fingerprints every theme file every few ticks (`POLL_FULL_THEMES_EVERY`).
Starting a native watch no longer looks the theme up itself: the settle
that `Watcher::spawn` runs first does, so start-up looks it up once on the
caller's thread, not twice (#283 item 2).

**Descriptors.** A native watcher holds at most one descriptor for the
stop pipe, the root, a linked file's target directory, `themes/`, the
theme file and `config.toml`, plus 8 unloadable theme files, 8 profile
theme files and 32 directories on the way.
