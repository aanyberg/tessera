//! The plugin contract.
//!
//! Every plugin, whether linked into the binary (v1), loaded as a WASM
//! component (v2) or connected over the control socket, sees exactly this API.
//! The rules that keep that true:
//!
//! * everything crossing the boundary is `serde`-serializable;
//! * plugins declare required [`Capability`] values in their [`Manifest`] and
//!   receive only those through the [`Host`];
//! * plugins never draw; UI contributions are declarative (status items,
//!   panels, commands) and the frontend renders them.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Plugin errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A capability was used without being declared in the manifest.
    #[error("capability not granted: {0:?}")]
    NotGranted(Capability),
    /// Plugin-defined failure.
    #[error("{0}")]
    Plugin(String),
}

/// Result alias for plugin operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Something a plugin may be allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Capability {
    /// Enumerate and inspect sessions.
    Sessions,
    /// Receive raw terminal output.
    Output,
    /// Contribute status bar items.
    StatusBar,
    /// Contribute side panels.
    Panels,
    /// Register command palette entries.
    Commands,
    /// Per-plugin persistent key/value storage.
    Storage,
    /// Read files under declared paths.
    Fs,
    /// Make HTTP requests to declared hosts.
    Http,
}

/// Static description of a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Reverse-DNS style id, e.g. `dev.tessera.hello`.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Semantic version.
    pub version: String,
    /// Capabilities the plugin needs to function.
    pub capabilities: Vec<Capability>,
}

/// Opaque session identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub u64);

/// Things that happen in the terminal that plugins may react to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    /// A session was created.
    SessionCreated {
        /// The new session.
        session: SessionId,
    },
    /// A session ended.
    SessionClosed {
        /// The closed session.
        session: SessionId,
    },
    /// Shell integration reported a command started.
    CommandStarted {
        /// Session it ran in.
        session: SessionId,
        /// Command line if known.
        command: Option<String>,
        /// Working directory if known.
        cwd: Option<String>,
    },
    /// Shell integration reported a command finished.
    CommandFinished {
        /// Session it ran in.
        session: SessionId,
        /// Exit status if reported.
        exit_code: Option<i32>,
        /// Wall-clock duration in milliseconds.
        duration_ms: u64,
    },
    /// A session's title changed.
    TitleChanged {
        /// The session.
        session: SessionId,
        /// New title.
        title: String,
    },
    /// Configuration was reloaded.
    ConfigChanged,
}

/// An item a plugin contributes to the status bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusItem {
    /// Stable id within the plugin.
    pub id: String,
    /// Text to show.
    pub text: String,
    /// Optional tooltip.
    pub tooltip: Option<String>,
}

/// The host-side services a plugin may call.
pub trait Host: Send + Sync {
    /// Capabilities granted to the calling plugin.
    fn granted(&self) -> &[Capability];
    /// Replaces the plugin's status bar items. Requires [`Capability::StatusBar`].
    fn set_status_items(&self, items: Vec<StatusItem>) -> Result<()>;
    /// Writes a value to plugin storage. Requires [`Capability::Storage`].
    fn storage_set(&self, key: &str, value: &[u8]) -> Result<()>;
    /// Reads a value from plugin storage. Requires [`Capability::Storage`].
    fn storage_get(&self, key: &str) -> Result<Option<Vec<u8>>>;
}

/// A plugin.
pub trait Plugin: Send + Sync {
    /// Static metadata.
    fn manifest(&self) -> Manifest;
    /// Called once after the manifest is accepted.
    fn activate(&mut self, host: std::sync::Arc<dyn Host>) -> Result<()>;
    /// Called for every event the plugin's capabilities allow it to see.
    fn on_event(&mut self, event: &Event) -> Result<()>;
    /// Called before unload.
    fn deactivate(&mut self) -> Result<()> {
        Ok(())
    }
}
