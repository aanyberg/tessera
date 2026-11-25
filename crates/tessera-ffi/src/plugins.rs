//! The built-in plugins, for the Settings window's Plugins pane (ADR 0047).

use tessera_app_core::plugins::{PluginInfo, StatusItem};

use crate::app::App;
use crate::error::Error;
use crate::panic::ffi_guard;
use crate::selection::copy_out;

/// Most ids of `[plugins] enabled` the plugin host reads; it ignores the
/// rest. The configuration loader returns a warning of them on every load
/// of such a list, and the configuration store logs it once, when it first
/// appears (not again while the list stays past the cap). A frontend's
/// Plugins pane refuses to list another id at it. Equal to
/// `tessera_app_core::plugins::MAX_ENABLED_PLUGINS`, tessera-config's cap
/// re-exported (a literal here, so that cbindgen can export it; a
/// compile-time assertion in tessera-ffi ties the two).
pub const MAX_ENABLED_PLUGINS: usize = 64;

// #590 (#560 M4): a cap that drifts from the host's (tessera-config's,
// which the host re-exports) fails the build, not only a test.
const _: () = assert!(
    MAX_ENABLED_PLUGINS == tessera_app_core::plugins::MAX_ENABLED_PLUGINS,
    "TESSERA_MAX_ENABLED_PLUGINS must equal the plugin host's cap"
);

/// One status item as JSON.
fn item_json(item: &StatusItem) -> serde_json::Value {
    serde_json::json!({
        "id": item.id,
        "text": item.text,
        "tooltip": item.tooltip,
    })
}

/// One plugin as JSON; every string is escaped by the serialiser, as in
/// the configuration JSON.
fn plugin_json(plugin: &PluginInfo) -> serde_json::Value {
    serde_json::json!({
        "id": plugin.id,
        "name": plugin.name,
        "version": plugin.version,
        "capabilities": plugin.capabilities,
        "enabled": plugin.enabled,
        "state": plugin.state.as_str(),
        "message": plugin.message,
        "status_items": plugin.status_items.iter().map(item_json).collect::<Vec<_>>(),
    })
}

/// Writes every plugin linked into this build as UTF-8 JSON, an array in
/// a fixed order:
///
/// ```json
/// [{"id": "dev.tessera.hello", "name": "Hello", "version": "0.1.0",
///   "capabilities": ["sessions", "status_bar"], "enabled": true,
///   "state": "active", "message": null,
///   "status_items": [{"id": "sessions", "text": "2 sessions",
///                     "tooltip": null}]}]
/// ```
///
/// `enabled` is whether `[plugins] enabled` lists it; `state` is `active`,
/// `refused` (it declares a capability this version does not grant),
/// `failed` (it panicked, or its activation returned an error) or
/// `disabled`; `message` says why it is refused or failed, written by the
/// core, never by the plugin, and is `null` otherwise. Status items are at
/// most 8, their text sanitised and capped (id 64 bytes, text 256,
/// tooltip 1024). Plugins are activated and deactivated on their own
/// thread, so read this again on every `on_config_applied` and when the
/// pane is shown.
///
/// Buffer handling as for `tessera_config_json`: `*len` gets the length;
/// `TESSERA_ERROR_INVALID_ARGUMENT` when `cap` is too small, with nothing
/// written; `TESSERA_ERROR_NULL_POINTER` for a `NULL` `app` or `len`.
///
/// Any thread; never waits for a plugin.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `len` must be `NULL` or point to
/// a writable `size_t`. `buf` must be `NULL` or valid for `cap` bytes of
/// writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_app_plugins_json(
    app: *const App,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> Error {
    ffi_guard!("tessera_app_plugins_json", {
        if len.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { len.write(0) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        let plugins: Vec<serde_json::Value> = app.core.plugins().iter().map(plugin_json).collect();
        let json = serde_json::Value::Array(plugins).to_string();
        // SAFETY: `len` and `buf` per the contract.
        unsafe { copy_out(&json, buf, cap, len) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #560: the exported cap is the host's.
    #[test]
    fn the_exported_cap_is_the_hosts() {
        assert_eq!(
            MAX_ENABLED_PLUGINS,
            tessera_app_core::plugins::MAX_ENABLED_PLUGINS
        );
        assert_eq!(MAX_ENABLED_PLUGINS, tessera_config::MAX_ENABLED_PLUGINS);
    }
}
