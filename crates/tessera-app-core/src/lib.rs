//! The application core.
//!
//! Owns the workspace model (windows contain tabs, tabs contain a pane tree,
//! leaves hold sessions), the command registry and keybinding resolution, the
//! configuration store, the plugin host and the control server. Frontends
//! drive it exclusively through `tessera-ffi`. This crate has no knowledge of
//! `AppKit`, `wgpu` or any OS window system.

#![forbid(unsafe_code)]

use tessera_config::Config;

/// Application version reported to frontends and the control API.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Root application object.
pub struct App {
    config: Config,
}

impl App {
    /// Creates an application with the given configuration.
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Current configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }
}
