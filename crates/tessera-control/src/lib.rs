//! Control socket server.
//!
//! Listens on a per-user Unix socket, authenticates clients (descendants of
//! the app are trusted; others trigger a frontend consent prompt), decodes
//! `tessera-protocol` messages and dispatches them to the app core. Lands in
//! v1.x; this crate currently fixes the socket location contract.

#![forbid(unsafe_code)]

use std::path::PathBuf;

/// Default socket path for the current user.
pub fn default_socket_path(app_support_dir: &std::path::Path) -> PathBuf {
    app_support_dir.join("control.sock")
}
