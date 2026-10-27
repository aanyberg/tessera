//! Control protocol types.
//!
//! Newline-delimited JSON-RPC 2.0 over a Unix socket. Methods mirror the
//! plugin [`tessera_plugin_api::Host`] surface so that anything a plugin can
//! do, a script can do. The CLI, the future MCP adapter and third-party
//! clients all depend on this crate alone. JSON Schema for these types is
//! generated into `docs/protocol/` by CI.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
pub use tessera_plugin_api::{Event, SessionId};

/// Protocol version negotiated on connect.
pub const VERSION: u32 = 1;

/// A request from client to server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Client-chosen id echoed in the response.
    pub id: u64,
    /// Method name and parameters.
    #[serde(flatten)]
    pub call: Method,
}

/// Methods the server understands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Method {
    /// Protocol handshake.
    Hello {
        /// Client's protocol version.
        version: u32,
        /// Client name for consent prompts.
        client: String,
    },
    /// List sessions.
    SessionsList,
    /// Send text to a session as if typed.
    SessionsSendText {
        /// Target session.
        session: SessionId,
        /// Text to send.
        text: String,
    },
    /// Subscribe to events.
    EventsSubscribe,
}

/// A response from server to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Echoed request id.
    pub id: u64,
    /// Outcome.
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// Success or failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Method-specific result.
    Result(serde_json::Value),
    /// Error.
    Error {
        /// JSON-RPC error code.
        code: i32,
        /// Message.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_format() {
        let req = Request {
            jsonrpc: "2.0".into(),
            id: 1,
            call: Method::Hello {
                version: VERSION,
                client: "test".into(),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(
            json,
            r#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":1,"client":"test"}}"#
        );
    }
}
