//! Pseudo-terminal management.
//!
//! Spawns a child process attached to a PTY, pumps its output to a channel
//! from a dedicated reader thread, accepts writes through a queue bounded in
//! bytes (so a stalled child applies backpressure instead of growing memory)
//! and propagates window-size changes. This is the only crate besides
//! `tessera-ffi` that is allowed to contain `unsafe`, because it talks to the
//! OS; the implementation stays on `portable-pty`'s safe surface except for
//! one duplication of the master descriptor, documented at its `SAFETY:`
//! comment.
//!
//! The crate is Unix-only: it relies on `fork`/`exec` semantics, POSIX
//! signals and `wait(2)` statuses, and refuses to compile elsewhere.
//!
//! Everything above this crate treats a PTY as a byte stream behind the
//! [`Pty`] trait plus a receiver of [`PtyEvent`]s, so the implementation
//! ([`NativePty`]) can be swapped for a fake in tests.
//!
//! # Lifecycle
//!
//! ```no_run
//! use tessera_pty::{NativePty, Pty, PtyEvent, PtySize, SpawnOptions};
//!
//! let options = SpawnOptions::login_shell("session-1").size(PtySize::new(40, 120));
//! let (mut pty, events) = NativePty::spawn(&options)?;
//! pty.write(b"exit 3\n")?;
//! for event in events {
//!     match event {
//!         PtyEvent::Output(bytes) => println!("{}", String::from_utf8_lossy(&bytes)),
//!         PtyEvent::Exited(status) => println!("child finished: {status}"),
//!     }
//! }
//! # Ok::<(), tessera_pty::PtyError>(())
//! ```

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

#[cfg(not(unix))]
compile_error!("tessera-pty supports Unix platforms only");

mod native;

pub use native::{NativePty, WRITE_QUEUE_BYTES};

/// Value injected as `TERM` for every child.
///
/// A dedicated `tessera` terminfo entry is planned; until it ships we claim
/// `xterm-256color`, which every emulator-aware program understands.
pub const TERM: &str = "xterm-256color";
/// Value injected as `COLORTERM`, advertising 24-bit colour support.
pub const COLORTERM: &str = "truecolor";
/// Value injected as `TERM_PROGRAM`.
pub const TERM_PROGRAM: &str = "Tessera";
/// Value injected as `TERM_PROGRAM_VERSION` (the workspace version).
pub const TERM_PROGRAM_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Name of the environment variable carrying the session identifier.
pub const SESSION_ID_VAR: &str = "TESSERA_SESSION_ID";

/// Terminal dimensions in character cells, plus the optional pixel size of
/// the viewport that programs such as image protocols read via `TIOCGWINSZ`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PtySize {
    /// Number of rows.
    pub rows: u16,
    /// Number of columns.
    pub cols: u16,
    /// Width of the viewport in pixels; `0` when unknown.
    pub pixel_width: u16,
    /// Height of the viewport in pixels; `0` when unknown.
    pub pixel_height: u16,
}

impl PtySize {
    /// Creates a size with unknown pixel dimensions.
    #[must_use]
    pub const fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// Sets the pixel dimensions of the viewport.
    #[must_use]
    pub const fn with_pixels(mut self, width: u16, height: u16) -> Self {
        self.pixel_width = width;
        self.pixel_height = height;
        self
    }
}

impl Default for PtySize {
    /// The classic 24×80 terminal.
    fn default() -> Self {
        Self::new(24, 80)
    }
}

/// How a child process finished.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExitStatus {
    /// The child called `exit` (or returned from `main`) with this code.
    Exited(i32),
    /// The child was terminated by the signal with this number (for example
    /// `1` for `SIGHUP`, `9` for `SIGKILL`); [`ExitStatus::signal_name`]
    /// gives the symbolic name.
    Signaled(i32),
}

impl ExitStatus {
    /// `true` for a clean exit with code zero.
    #[must_use]
    pub fn success(&self) -> bool {
        matches!(self, Self::Exited(0))
    }

    /// The exit code, if the child exited rather than being signaled.
    #[must_use]
    pub fn code(&self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(*code),
            Self::Signaled(_) => None,
        }
    }

    /// The signal number, if the child was terminated by one.
    #[must_use]
    pub fn signal(&self) -> Option<i32> {
        match self {
            Self::Exited(_) => None,
            Self::Signaled(signal) => Some(*signal),
        }
    }

    /// The symbolic name of the terminating signal (`SIGHUP`, `SIGKILL`,
    /// …), or `None` when the child exited normally or the number is not
    /// one this platform knows.
    #[must_use]
    pub fn signal_name(&self) -> Option<&'static str> {
        let signal = self.signal()?;
        nix::sys::signal::Signal::try_from(signal)
            .ok()
            .map(nix::sys::signal::Signal::as_str)
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exited(code) => write!(f, "exited with code {code}"),
            Self::Signaled(signal) => match self.signal_name() {
                Some(name) => write!(f, "terminated by signal {signal} ({name})"),
                None => write!(f, "terminated by signal {signal}"),
            },
        }
    }
}

/// Something the reader thread observed on the PTY.
///
/// The stream always ends with exactly one [`PtyEvent::Exited`]; after it
/// the sender is dropped and the receiver reports disconnection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    /// A chunk of bytes the child wrote to its terminal.
    Output(Vec<u8>),
    /// The child exited and has been reaped.
    ///
    /// Exit is detected through end-of-file on the master. On macOS that
    /// happens as soon as the session leader exits, even if it left a
    /// backgrounded grandchild behind. On Linux end-of-file only arrives
    /// once every descriptor on the slave side is closed, so such a
    /// grandchild delays this event until it releases the terminal.
    Exited(ExitStatus),
}

/// Receiving end of the event stream produced by a spawned PTY.
pub type PtyEvents = Receiver<PtyEvent>;

/// Errors from spawning or driving a PTY.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    /// The PTY pair could not be allocated.
    #[error("failed to open pty: {0}")]
    Open(String),
    /// The child could not be started (program missing, not executable, …).
    #[error("failed to spawn {program}: {reason}")]
    Spawn {
        /// Program that was requested.
        program: String,
        /// Platform-provided explanation.
        reason: String,
    },
    /// A window-size update was rejected by the kernel.
    #[error("failed to resize pty: {0}")]
    Resize(String),
    /// A size with zero rows or columns was requested; programs divide by
    /// the window size, so such a size is never passed to the kernel.
    #[error("pty size must have non-zero rows and cols, got {}x{}", .0.rows, .0.cols)]
    InvalidSize(PtySize),
    /// An I/O operation on the master side failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// What to run inside the PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Program {
    /// The user's login shell (`$SHELL`, falling back to the password
    /// database), invoked with a leading `-` in `argv[0]` so it sources its
    /// login profile.
    LoginShell,
    /// An explicit program, resolved via `PATH` when not absolute.
    Command {
        /// Executable path or name.
        program: PathBuf,
        /// Arguments after `argv[0]`.
        args: Vec<OsString>,
    },
}

/// Everything needed to start a child in a fresh PTY.
///
/// The child inherits this process's environment, then receives the Tessera
/// identification variables ([`TERM`], [`COLORTERM`], [`TERM_PROGRAM`],
/// [`TERM_PROGRAM_VERSION`], [`SESSION_ID_VAR`]), and finally any entries
/// from [`SpawnOptions::env`], which win over both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnOptions {
    /// Program to run.
    pub program: Program,
    /// Working directory; the home directory when `None`.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables, applied last.
    pub env: Vec<(OsString, OsString)>,
    /// Initial window size.
    pub size: PtySize,
    /// Identifier exported as `TESSERA_SESSION_ID`.
    pub session_id: String,
}

impl SpawnOptions {
    /// Options for the user's login shell.
    #[must_use]
    pub fn login_shell(session_id: impl Into<String>) -> Self {
        Self::new(Program::LoginShell, session_id)
    }

    /// Options for an explicit program with no arguments.
    #[must_use]
    pub fn command(program: impl AsRef<Path>, session_id: impl Into<String>) -> Self {
        Self::new(
            Program::Command {
                program: program.as_ref().to_path_buf(),
                args: Vec::new(),
            },
            session_id,
        )
    }

    fn new(program: Program, session_id: impl Into<String>) -> Self {
        Self {
            program,
            cwd: None,
            env: Vec::new(),
            size: PtySize::default(),
            session_id: session_id.into(),
        }
    }

    /// Appends an argument. Ignored for [`Program::LoginShell`].
    #[must_use]
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        if let Program::Command { args, .. } = &mut self.program {
            args.push(arg.as_ref().to_os_string());
        }
        self
    }

    /// Appends several arguments. Ignored for [`Program::LoginShell`].
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self = self.arg(arg);
        }
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn cwd(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Adds an environment variable that overrides inherited and injected
    /// values.
    #[must_use]
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    /// Sets the initial window size.
    #[must_use]
    pub fn size(mut self, size: PtySize) -> Self {
        self.size = size;
        self
    }

    /// The variables this crate injects for the given session, in order.
    ///
    /// Exposed so callers (and tests) can see exactly what a child receives.
    #[must_use]
    pub fn injected_env(&self) -> [(&'static str, String); 5] {
        [
            ("TERM", TERM.to_owned()),
            ("COLORTERM", COLORTERM.to_owned()),
            ("TERM_PROGRAM", TERM_PROGRAM.to_owned()),
            ("TERM_PROGRAM_VERSION", TERM_PROGRAM_VERSION.to_owned()),
            (SESSION_ID_VAR, self.session_id.clone()),
        ]
    }
}

/// Abstraction over a spawned pseudo-terminal.
///
/// Output and the exit status arrive on the [`PtyEvents`] receiver handed
/// out at spawn time; this trait covers the control side. Implementations
/// are `Send` so a session can live on whichever thread owns it, but not
/// necessarily `Sync`; wrap in a mutex to share.
pub trait Pty: Send {
    /// Queues bytes for the child's stdin, blocking while the write queue
    /// (bounded in bytes, see [`WRITE_QUEUE_BYTES`]) is full. Fails once the
    /// child has exited or the writer has shut down.
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Like [`Pty::write`] but returns [`io::ErrorKind::WouldBlock`] instead
    /// of waiting when `bytes` does not fit in the queue's remaining budget.
    /// Nothing is queued on failure.
    fn try_write(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Informs the kernel, and through `SIGWINCH` the child, of a new size.
    /// Rejects zero rows or columns with [`PtyError::InvalidSize`].
    fn resize(&mut self, size: PtySize) -> Result<(), PtyError>;

    /// The most recent size passed to [`Pty::resize`] (or the spawn size).
    fn size(&self) -> PtySize;

    /// The child's process id, if it was spawned successfully.
    fn child_pid(&self) -> Option<u32>;

    /// The exit status, once the reader thread has reaped the child.
    /// Never blocks.
    fn exit_status(&self) -> Option<ExitStatus>;

    /// Asks the child to hang up (`SIGHUP`), as closing a terminal window
    /// does. Shells save history and exit; the status arrives as an event.
    fn hangup(&mut self) -> io::Result<()>;

    /// Forcibly terminates the child (`SIGKILL`).
    fn kill(&mut self) -> io::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_defaults_to_vt100_dimensions() {
        let size = PtySize::default();
        assert_eq!((size.rows, size.cols), (24, 80));
        assert_eq!((size.pixel_width, size.pixel_height), (0, 0));
        let sized = size.with_pixels(800, 600);
        assert_eq!((sized.pixel_width, sized.pixel_height), (800, 600));
    }

    #[test]
    fn exit_status_accessors() {
        assert!(ExitStatus::Exited(0).success());
        assert!(!ExitStatus::Exited(2).success());
        assert!(!ExitStatus::Signaled(9).success());
        assert_eq!(ExitStatus::Exited(7).code(), Some(7));
        assert_eq!(ExitStatus::Exited(7).signal(), None);
        assert_eq!(ExitStatus::Exited(7).signal_name(), None);
        assert_eq!(ExitStatus::Signaled(1).code(), None);
        assert_eq!(ExitStatus::Signaled(1).signal(), Some(1));
        assert_eq!(ExitStatus::Signaled(1).signal_name(), Some("SIGHUP"));
        assert_eq!(ExitStatus::Signaled(9).signal_name(), Some("SIGKILL"));
        assert_eq!(ExitStatus::Exited(7).to_string(), "exited with code 7");
        assert_eq!(
            ExitStatus::Signaled(9).to_string(),
            "terminated by signal 9 (SIGKILL)"
        );
        assert_eq!(
            ExitStatus::Signaled(10_000).to_string(),
            "terminated by signal 10000"
        );
    }

    #[test]
    fn invalid_size_error_names_the_dimensions() {
        assert_eq!(
            PtyError::InvalidSize(PtySize::new(0, 80)).to_string(),
            "pty size must have non-zero rows and cols, got 0x80"
        );
    }

    #[test]
    fn options_builder_collects_args_env_and_size() {
        let opts = SpawnOptions::command("sh", "s1")
            .arg("-c")
            .args(["echo", "hi"])
            .env("FOO", "bar")
            .cwd("/")
            .size(PtySize::new(10, 20));
        assert_eq!(
            opts.program,
            Program::Command {
                program: PathBuf::from("sh"),
                args: vec!["-c".into(), "echo".into(), "hi".into()],
            }
        );
        assert_eq!(opts.env, vec![("FOO".into(), "bar".into())]);
        assert_eq!(opts.cwd.as_deref(), Some(Path::new("/")));
        assert_eq!(opts.size, PtySize::new(10, 20));
        assert_eq!(opts.session_id, "s1");
    }

    #[test]
    fn login_shell_ignores_args() {
        let opts = SpawnOptions::login_shell("s2").arg("--ignored");
        assert_eq!(opts.program, Program::LoginShell);
    }

    #[test]
    fn injected_env_lists_every_identification_variable() {
        let opts = SpawnOptions::login_shell("abc");
        let env = opts.injected_env();
        let keys: Vec<&str> = env.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            [
                "TERM",
                "COLORTERM",
                "TERM_PROGRAM",
                "TERM_PROGRAM_VERSION",
                "TESSERA_SESSION_ID"
            ]
        );
        assert_eq!(env[0].1, "xterm-256color");
        assert_eq!(env[1].1, "truecolor");
        assert_eq!(env[2].1, "Tessera");
        assert_eq!(env[3].1, env!("CARGO_PKG_VERSION"));
        assert_eq!(env[4].1, "abc");
    }
}
