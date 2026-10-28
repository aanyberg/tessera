//! Developer tooling for the Tessera core: headless screen dumps and theme
//! import.
//!
//! The `tessera-devtool` binary runs a command in a pseudo-terminal, drives
//! the emulator ([`tessera_term::Terminal`]) with its output and prints the
//! final grid as ANSI-styled text, plain text or one line per cell:
//!
//! ```text
//! cargo run -p tessera-devtool -- run 'ls --color' --cols 80 --rows 24
//! ```
//!
//! The library holds the two halves so tests can reuse them without the
//! binary: [`run`] spawns the command and returns the [`Terminal`], and
//! [`dump`] formats any [`Grid`], including one built by hand. The crate is
//! dev-only: it is never published and nothing in the app depends on it.
//!
//! `tessera-devtool theme import <file>` converts an iTerm2 `.itermcolors`
//! file or a base16 scheme into a theme file (see [`theme`]).
//!
//! The run is bounded by a timeout (default 10 s, exit code 124; see
//! [`run::run`] for how far extreme sizes can stretch it). On timeout only
//! the direct child, the `/bin/sh` running the command, is signalled
//! (`SIGHUP`, then `SIGKILL` 200 ms later): `tessera-pty` has no
//! process-group signal, so anything the command started is left to the
//! hangup the kernel sends when the shell dies.
//!
//! [`Terminal`]: tessera_term::Terminal
//! [`Grid`]: tessera_grid::Grid

#![forbid(unsafe_code)]

pub mod dump;
pub mod run;
pub mod theme;
