//! `tessera` command-line client.
//!
//! Talks to a running Tessera.app over the control socket to open tabs, split
//! panes, send text and query sessions from scripts. Functional in v1.x; for
//! now it only reports the protocol version so the binary target exists.

#![forbid(unsafe_code)]

fn main() {
    println!("tessera cli (protocol v{})", tessera_protocol::VERSION);
}
