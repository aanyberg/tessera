//! Pseudo-terminal management.
//!
//! Spawns a child process attached to a PTY, exposes async-friendly read/write
//! handles, and propagates window-size changes. This is the only crate besides
//! `tessera-ffi` that is allowed to contain `unsafe`, because it talks to libc.
//! Everything above it treats a PTY as a byte stream behind the [`Pty`] trait,
//! so the implementation (currently planned: `portable-pty`) can be swapped.

use std::io;

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    /// Number of rows.
    pub rows: u16,
    /// Number of columns.
    pub cols: u16,
}

/// Abstraction over a spawned pseudo-terminal.
pub trait Pty: Send {
    /// Writes bytes to the child's stdin.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize>;
    /// Reads bytes produced by the child.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Informs the child of a new window size.
    fn resize(&mut self, size: PtySize) -> io::Result<()>;
}
