# ADR 0047: Plugin host and built-in loader (amends 0002)

**Status:** accepted · 2026-11-14

## Context

ADR 0002 defines the plugin contract (`tessera-plugin-api`: `Plugin`,
`Host`, `Capability`, `Event`) and leaves the host to later work. Nothing
loads a plugin today: `tessera-plugin-hello` exists but is never called,
`[plugins] enabled` is parsed and ignored, and `docs/security.md` lists
plugins as "not built yet" (S9). Issue #48 builds the host for the plugins
that ship inside the binary, so the contract is exercised end to end before
third-party loading (WASM, the control socket) is designed.

## Decision

**Trust.** A built-in plugin is first-party code, reviewed like the core and
linked in by a Cargo feature of `tessera-app-core` (`plugin-hello`, on by
default). It runs only when `[plugins] enabled` in `config.toml` lists its
id; listing it is the consent. Everything it hands the host is still treated
as untrusted (status items, stored keys and values, errors, panics, time
spent): a bug in a plugin must not hang a session, crash the app, put
plugin text in the log or grow memory without bound. ADR 0002's capability
model and ADR 0003's consent stay the model for third-party plugins.

**Capabilities.** The host grants exactly what the manifest declares. Only
`sessions` (session, title and command events), `status_bar` and `storage`
are implemented; a manifest that declares any other (`output`, `panels`,
`commands`, `fs`, `http`) is refused: never activated, state `refused`, one
warning naming the capability. `ConfigChanged` goes to every active plugin.
Every `Host` method returns `Error::NotGranted` without its capability, and
`Error::Plugin("plugin is not active")` once the activation it was handed
for has ended.

**Enablement.** `[plugins] enabled` is read on start and on every apply
(`App::apply_config` and the config watch, through the observer's
`on_config_applied`): a newly listed plugin is activated with a fresh
instance, which first gets `SessionCreated` for every open session, also
when it was removed and listed again before the plugin thread saw the
removal (a per-plugin enable epoch); a removed one gets `deactivate`, can
still use its storage and status items there, and is then `disabled`, its
status items cleared.
At most 64 ids (`MAX_ENABLED_PLUGINS`) of at most 128 bytes
(`MAX_PLUGIN_ID_LEN`) are read; an extra, too-long or unknown id gets one
escaped warning, only when the list changes.

**Thread and queue.** Plugins run on one thread, `tessera-plugins`, started
at the first activation; it runs the thread start hook (ADR 0018). The
host stands in the session observer slot as a tee in front of the
frontend's observer: `SessionEvent::Output` is forwarded and returns before
any plugin work; titles (at most one per chunk, the reader's coalescing)
and command marks become `TitleChanged`, `CommandStarted` (command and
working directory `None`: the core keeps no command text, ADR 0039) and
`CommandFinished`; `App` adds `SessionCreated` and `SessionClosed` from
attach and close. Events are queued for the thread, 1024 at most
(`MAX_PLUGIN_EVENTS`) for all plugins; when full the newest event is
dropped and counted, with one counts-only debug log per burst, except that
`SessionCreated` and `SessionClosed` evict the oldest other event instead.
When the queue holds only lifecycle events, a `SessionClosed` whose
`SessionCreated` is still queued removes that creation (neither is
delivered), and any other lifecycle event is dropped and counted; the
plugin thread's next turn then resyncs each active plugin with
`sessions`: it delivers `SessionClosed` for every session the plugin was
told about that is gone and `SessionCreated` for every live one it was
not, so a plugin that counts sessions stays right after a flood (#489).
The resync comes ahead of the events still queued; the net set of open
sessions is right, not the interleaving. Nothing is
queued while no enabled plugin has `sessions` (and no `ConfigChanged` while
none is enabled). The reader only locks the queue; it never waits for a
plugin.

**Failure.** Every call into a plugin (factory, manifest, `activate`,
`on_event`, `deactivate`, its drop) runs under `catch_unwind`. A panic, or
an error from `activate`, leaves the plugin `failed` with one warning (its
id and the error kind, never the plugin's text) and no further calls until
it is removed from `enabled` and listed again. An error returned from
`on_event` is logged at debug, by kind, and does not fail the plugin.

**Status items and storage.** At most 8 status items per plugin; id 64
bytes, text 256, tooltip 1024, cut at a character boundary after control,
bidi-control and line-separator characters are removed (S3). Storage is
in memory only and lasts for the `App`: per plugin 64 keys, key 128 bytes,
value 64 KiB and 1 MiB of keys and values in all; past a cap the write
fails with `Error::Plugin("storage full")` and changes nothing.

**Frontend.** `App::plugins` and `tessera_app_plugins_json` report every
built-in plugin in a fixed order as
`[{id, name, version, capabilities, enabled, state, message, status_items}]`
(`state` one of `active`, `refused`, `failed`, `disabled`; `message`
written by the host, never the plugin). There is no new callback: the app
reads the JSON again on `on_config_applied` and when it shows the list.

**Out of scope for 0.2.0.** Third-party loading (WASM, socket clients),
the `output`, `panels`, `commands`, `fs` and `http` capabilities, persisted
storage and a status bar in the app.

## Consequences

- A plugin that sleeps on every event costs a session nothing measurable:
  100 MB with a title every KiB reads within 1.05x of no plugin, the queue
  stays at 1024 and the overflow is counted
  (`a_slow_plugin_keeps_session_throughput`, release timing step).
- Events past the queue are lost; a plugin cannot rely on seeing every
  title or command, only on its set of open sessions ending right: a
  session created and closed while the queue was full of lifecycle
  events may never be reported.
- A failed plugin needs a config edit to be tried again.
- `tessera-control` no longer depends on `tessera-protocol` and is no
  longer a workspace dependency (#417); it stays an empty crate for #53.
