//! [`NativePty`]: the `portable-pty` backed implementation of [`Pty`].

use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::BorrowedFd;
use std::os::unix::process::ExitStatusExt;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, MasterPty, native_pty_system};

use crate::{ExitStatus, Program, Pty, PtyError, PtyEvent, PtyEvents, PtySize, SpawnOptions};

/// Size of the buffer each `read(2)` on the master fills.
const READ_CHUNK: usize = 64 * 1024;
/// Output chunks the reader thread may queue before it stops reading.
/// Beyond this the kernel's PTY buffer fills and the child blocks on
/// `write(2)`, so a slow consumer throttles the child instead of us.
const READ_QUEUE_CHUNKS: usize = 64;
/// Bytes the write queue may hold before [`Pty::write`] blocks and
/// [`Pty::try_write`] reports `WouldBlock`.
pub const WRITE_QUEUE_BYTES: usize = 1024 * 1024;
/// Most bytes the writer thread hands to one `write(2)`. A cooked terminal
/// processes its input byte by byte under the kernel's terminal lock, and
/// with several sessions doing so at once a single 1 MiB write can stay in
/// the kernel for seconds, ignoring a close and stalling the child's exit.
/// Writing in slices returns to the closed check after every slice. With
/// 100 such sessions closing at once on a loaded machine, 4 KiB slices
/// still let dropping ten of them take over 100 ms (8-16 runs in 30); 1 KiB
/// slices did so once in 60 runs and write bulk input as fast as unsliced
/// writes (512 B costs 16% more).
const WRITE_SLICE: usize = 1024;

/// A child process attached to a kernel pseudo-terminal.
///
/// Two threads back each instance: a reader that forwards child output as
/// [`PtyEvent::Output`] and, once the master reaches end-of-file, reaps the
/// child and emits [`PtyEvent::Exited`]; and a writer that drains a queue
/// bounded to [`WRITE_QUEUE_BYTES`] into the master so callers never block
/// on a stalled child beyond that budget.
///
/// Dropping a `NativePty` whose child is still running sends it `SIGHUP`,
/// matching what closing a terminal window does. Nothing is ever written to
/// the child's input on close: pending input is dropped (see
/// [`Pty::hangup`]), and the writer is a plain duplicate of the master
/// descriptor, so dropping it only closes a file. The reader thread outlives
/// the handle until the child exits, so the process is always reaped.
///
/// # Closing and pending input
///
/// [`Pty::hangup`], [`Pty::kill`] and dropping the handle close the write
/// queue before they signal the child: queued input is dropped, later
/// writes fail with [`io::ErrorKind::BrokenPipe`], and the writer thread
/// abandons the chunk it is writing after the `write(2)` in progress, which
/// carries at most 1 KiB. That one slice may still reach the child after
/// the close. The writer's descriptor stays blocking, so that `write(2)`
/// can sleep.
///
/// Known limit: for a child that ignores `SIGHUP` and never reads its
/// input, the writer stays asleep in that `write(2)` until the child dies
/// (a session closing through `tessera-app-core` kills it after its grace
/// period); if such a child starts reading again first, it receives the
/// rest of that one slice, at most 1 KiB. Three facts rule out waking the
/// writer instead:
///
/// - `O_NONBLOCK` belongs to the open file description, which every
///   duplicate of the master (the reader's, the writer's) shares, and a
///   master cannot be reopened, so it cannot be set for the writer alone.
///   Set for both, it made reading output, the hot path, cost 35-40% more
///   CPU, because each idle read becomes `read`, `EAGAIN`, `poll`, `read`.
/// - On macOS, `close(2)` of a descriptor another thread is blocked writing
///   to does not wake the writer; it hangs the closing thread, which only
///   `SIGKILL` frees.
/// - The writer is not usually asleep anyway: the multi-second closes this
///   design fixes came from writers *running* in the kernel. A cooked
///   terminal processes input byte by byte under the kernel's terminal
///   lock, so one 1 MiB `write(2)` per session, with ten sessions at once,
///   stays in the kernel for seconds, and `kill(2)` on the children stalls
///   behind it. Slicing bounds that to one slice.
///
/// # Exit detection
///
/// The child counts as exited when the master reaches end-of-file. macOS
/// revokes the terminal as soon as the session leader exits, so a shell that
/// leaves a backgrounded grandchild behind still reports its exit within
/// milliseconds. Linux only signals end-of-file once the last descriptor on
/// the slave side closes, so there a lingering grandchild delays
/// [`PtyEvent::Exited`] until it too lets go of the terminal.
pub struct NativePty {
    master: Box<dyn MasterPty + Send>,
    queue: Arc<WriteQueue>,
    pid: u32,
    size: PtySize,
    phase: Arc<Mutex<Phase>>,
    threads: IoThreads,
}

/// Liveness of one [`NativePty`]'s reader and writer threads.
///
/// Obtained from [`NativePty::io_threads`] and usable after the PTY itself
/// is dropped, so a caller (or a test) can confirm that closing a session
/// really let both threads finish. Holding it keeps nothing alive: the
/// threads are detached, and this only remembers their join handles.
#[derive(Clone)]
pub struct IoThreads {
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl IoThreads {
    /// How many of the PTY's threads (at most two) have not finished yet.
    #[must_use]
    pub fn running(&self) -> usize {
        self.handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|handle| !handle.is_finished())
            .count()
    }
}

impl fmt::Debug for IoThreads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoThreads")
            .field("running", &self.running())
            .finish()
    }
}

/// Where the child is in its life, guarded by one mutex so that signalling
/// and reaping cannot interleave: a signal is only sent while the lock shows
/// `Running`, and the reader thread flips to `Reaping` under the same lock
/// before it calls `wait(2)`. A pid is therefore never signalled after the
/// kernel may have recycled it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Running,
    Reaping,
    Exited(ExitStatus),
}

impl NativePty {
    /// Opens a PTY, starts the child described by `options` on its slave
    /// side and returns the control handle together with the event stream.
    ///
    /// # Errors
    ///
    /// [`PtyError::InvalidSize`] if the requested size has zero rows or
    /// columns, [`PtyError::ArgumentsTooLong`] if the arguments and
    /// environment exceed what `execve(2)` accepts (checked before
    /// anything is opened or forked), [`PtyError::Open`] if no PTY could be
    /// allocated, [`PtyError::Spawn`] if the program is missing or not
    /// executable or an argument or variable contains a NUL byte,
    /// [`PtyError::Io`] if the reader or writer side of the master or one
    /// of their threads could not be set up.
    ///
    /// Every error leaves nothing behind: a child started before the
    /// failure is killed and reaped, and both ends of the PTY are closed.
    pub fn spawn(options: &SpawnOptions) -> Result<(Self, PtyEvents), PtyError> {
        check_size(options.size)?;
        let command = build_command(options);
        check_exec_size(&command, options)?;
        let pair = native_pty_system()
            .openpty(options.size.into())
            .map_err(|err| PtyError::Open(err.to_string()))?;

        let program = describe_program(&options.program);
        let spawn_error = |reason: String| PtyError::Spawn {
            program: program.clone(),
            reason,
        };
        let child: Box<dyn portable_pty::Child> = pair
            .slave
            .spawn_command(command)
            .map_err(|err| spawn_error(err.to_string()))?;
        // portable-pty hands back a `std::process::Child` on Unix. Holding
        // the concrete type lets the reader thread see the raw wait status
        // (signal numbers) instead of portable-pty's `strsignal` text.
        let child = match child.downcast::<Child>() {
            Ok(child) => {
                fault::forked(child.id());
                ChildGuard(Some(*child))
            }
            Err(mut other) => {
                let _ = other.kill();
                let _ = other.wait();
                return Err(spawn_error("unexpected child handle type".to_owned()));
            }
        };
        // The parent must not keep the slave open: the master would never
        // see end-of-file after the child exits.
        drop(pair.slave);

        // From here on, an early return drops `child`, which kills and
        // reaps it.
        let master = pair.master;
        fault::point(fault::Step::Setup)?;
        let reader = master.try_clone_reader().map_err(io_error)?;
        let writer = duplicate_master(master.as_ref())?;
        let pid = child.id();

        let phase = Arc::new(Mutex::new(Phase::Running));
        let queue = Arc::new(WriteQueue::default());
        let (event_tx, event_rx) = mpsc::sync_channel(READ_QUEUE_CHUNKS);

        let reader_phase = Arc::clone(&phase);
        let reader_queue = Arc::clone(&queue);
        // A thread that cannot be created drops its closure, and with it
        // the guard, so the child is killed and reaped then too.
        let reader_thread = spawn_thread(
            format!("pty-reader-{pid}"),
            fault::Step::ReaderThread,
            move || {
                pump_output(
                    reader,
                    child.into_inner(),
                    &event_tx,
                    &reader_phase,
                    &reader_queue,
                );
            },
        )?;
        let writer_queue = Arc::clone(&queue);
        let writer_thread = spawn_thread(
            format!("pty-writer-{pid}"),
            fault::Step::WriterThread,
            move || pump_input(writer, &writer_queue),
        )
        .inspect_err(|_| {
            // The reader owns the child now. Stop it as `kill` would; the
            // reader sees end-of-file, reaps the child and finishes.
            // Closing the queue first is not observable here (no writer
            // runs and nothing was queued); it keeps the order every other
            // closing path uses (close, then signal).
            queue.close();
            if let Err(err) = signal_child(&phase, pid, Signal::SIGKILL) {
                tracing::warn!(pid, %err, "killing a half-spawned child failed");
            }
        })?;
        let threads = IoThreads {
            handles: Arc::new(Mutex::new(vec![reader_thread, writer_thread])),
        };

        Ok((
            Self {
                master,
                queue,
                pid,
                size: options.size,
                phase,
                threads,
            },
            event_rx,
        ))
    }

    /// A handle on this PTY's reader and writer threads that outlives it.
    #[must_use]
    pub fn io_threads(&self) -> IoThreads {
        self.threads.clone()
    }

    fn phase(&self) -> MutexGuard<'_, Phase> {
        self.phase.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn check_writable(&self) -> io::Result<()> {
        match *self.phase() {
            Phase::Running => Ok(()),
            Phase::Reaping | Phase::Exited(_) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child has exited",
            )),
        }
    }

    /// Delivers `signal` to the child unless it has been (or is being)
    /// reaped. Holding the phase lock across `kill(2)` is what keeps the pid
    /// from being recycled underneath us.
    fn signal_if_running(&self, signal: Signal) -> io::Result<()> {
        signal_child(&self.phase, self.pid, signal)
    }
}

/// Delivers `signal` to `pid` unless `phase` shows it has been (or is
/// being) reaped; see [`NativePty::signal_if_running`].
fn signal_child(phase: &Mutex<Phase>, pid: u32, signal: Signal) -> io::Result<()> {
    let phase = phase.lock().unwrap_or_else(PoisonError::into_inner);
    if *phase != Phase::Running {
        return Ok(());
    }
    let pid = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))?;
    signal::kill(Pid::from_raw(pid), signal).map_err(io::Error::from)
}

/// A child that has not been handed to its reader thread yet. Dropping
/// the guard kills and reaps the child, so no error path of
/// [`NativePty::spawn`] leaves a running process or a zombie behind.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn id(&self) -> u32 {
        self.0.as_ref().map_or(0, Child::id)
    }

    /// Disarms the guard and hands the child over.
    fn into_inner(mut self) -> Child {
        self.0
            .take()
            .expect("the guard holds its child until disarmed")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // `kill` fails only if the child already exited; `wait` reaps
            // it either way.
            let _ = child.kill();
            if let Err(err) = child.wait() {
                tracing::warn!(pid = child.id(), %err, "reaping a half-spawned child failed");
            }
        }
    }
}

/// Starts a named thread running `body`. On failure `body` is dropped
/// without running, as [`thread::Builder::spawn`] does.
fn spawn_thread(
    name: String,
    step: fault::Step,
    body: impl FnOnce() + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    if let Err(err) = fault::point(step) {
        drop(body);
        return Err(err);
    }
    thread::Builder::new().name(name).spawn(body)
}

/// Fraction of `ARG_MAX` withheld for what the kernel adds to the
/// arguments and environment itself (one tenth).
const ARG_MAX_HEADROOM_DIVISOR: usize = 10;

/// Linux caps argument space at three quarters of `_STK_LIM` (6 MiB)
/// whatever the stack limit, while glibc's `sysconf(_SC_ARG_MAX)` reports a
/// quarter of `RLIMIT_STACK`, which is far more with a large or unlimited
/// stack.
#[cfg(target_os = "linux")]
const KERNEL_ARG_CAP: Option<usize> = Some(6 * 1024 * 1024);
#[cfg(not(target_os = "linux"))]
const KERNEL_ARG_CAP: Option<usize> = None;

/// What [`exec_bytes`] measured for one spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecFootprint {
    /// Every byte `execve(2)` copies.
    bytes: usize,
    /// The longest single argument or `KEY=value` string, NUL included.
    longest: usize,
}

/// The limits [`check_exec_size`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecLimits {
    /// `ARG_MAX` (clamped to the kernel's own cap), or `None` when the
    /// system reports none.
    arg_max: Option<usize>,
    /// Most bytes one string may take, NUL included, where the kernel caps
    /// each string: `MAX_ARG_STRLEN` (32 pages) on Linux, none on macOS.
    max_string: Option<usize>,
}

impl ExecLimits {
    /// This system's limits.
    fn system() -> Self {
        let arg_max = arg_max().map(|max| KERNEL_ARG_CAP.map_or(max, |cap| max.min(cap)));
        #[cfg(target_os = "linux")]
        let max_string = page_size().map(|page| 32 * page);
        #[cfg(not(target_os = "linux"))]
        let max_string = None;
        Self {
            arg_max,
            max_string,
        }
    }

    /// Refuses `footprint` with [`PtyError::ArgumentsTooLong`] when it is
    /// over `ARG_MAX` less the headroom (`bytes` and `max` are then the
    /// totals) or has a string over `max_string` (`bytes` and `max` are
    /// then that string's length and the per-string limit).
    fn check(self, footprint: ExecFootprint) -> Result<(), PtyError> {
        if let Some(arg_max) = self.arg_max {
            let max = arg_max - arg_max / ARG_MAX_HEADROOM_DIVISOR;
            if footprint.bytes > max {
                return Err(PtyError::ArgumentsTooLong {
                    bytes: footprint.bytes,
                    max,
                });
            }
        }
        if let Some(max) = self.max_string
            && footprint.longest > max
        {
            return Err(PtyError::ArgumentsTooLong {
                bytes: footprint.longest,
                max,
            });
        }
        Ok(())
    }
}

/// Refuses `command` with [`PtyError::ArgumentsTooLong`] when its argument
/// and environment strings exceed what `execve(2)` accepts here (see
/// [`ExecLimits`]).
///
/// On an unknown `ARG_MAX` (`sysconf` reports none) the total is not
/// checked here and the kernel's own check stands.
fn check_exec_size(command: &CommandBuilder, options: &SpawnOptions) -> Result<(), PtyError> {
    ExecLimits::system().check(exec_bytes(command, options))
}

/// `sysconf(_SC_ARG_MAX)`: the most bytes of arguments and environment
/// `execve(2)` accepts. macOS reports 1 MiB (its `<limits.h>` constant can
/// say otherwise); glibc reports a quarter of the stack limit, which
/// [`KERNEL_ARG_CAP`] clamps.
fn arg_max() -> Option<usize> {
    let value = nix::unistd::sysconf(nix::unistd::SysconfVar::ARG_MAX).ok()??;
    usize::try_from(value).ok().filter(|&max| max > 0)
}

#[cfg(target_os = "linux")]
fn page_size() -> Option<usize> {
    let value = nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE).ok()??;
    usize::try_from(value).ok().filter(|&size| size > 0)
}

/// Bytes `execve(2)` copies into the new process for `command`, counted as
/// the kernel counts them: every argument and every `KEY=value` string with
/// its terminating NUL, one pointer per string, one terminating `NULL`
/// pointer per array, and the executable's path.
///
/// The environment is the one `portable-pty` builds: this process's
/// variables plus `SHELL`, overridden by the injected and caller
/// variables. The path counted is the program as given, or `$SHELL` for a
/// login shell (`portable-pty` falls back to the password database's shell
/// when `$SHELL` is not executable); a `PATH` lookup or that fallback may
/// lengthen it, which the headroom in [`ExecLimits::check`] covers.
fn exec_bytes(command: &CommandBuilder, options: &SpawnOptions) -> ExecFootprint {
    use std::collections::BTreeSet;
    use std::ffi::{OsStr, OsString};
    use std::path::Path;

    const POINTER: usize = size_of::<*const u8>();
    let mut longest = 0;
    let mut string = |len: usize| {
        longest = longest.max(len.saturating_add(1));
        len.saturating_add(1 + POINTER)
    };

    let (arg_bytes, path) = match &options.program {
        // portable-pty runs the shell as `-<basename>`.
        Program::LoginShell => {
            let shell = command.get_env("SHELL").map_or(0, OsStr::len);
            let base = command
                .get_env("SHELL")
                .and_then(|shell| Path::new(shell).file_name())
                .map_or(0, OsStr::len);
            (string(1 + base), shell)
        }
        Program::Command { program, .. } => (
            command
                .get_argv()
                .iter()
                .fold(0usize, |sum, arg| sum.saturating_add(string(arg.len()))),
            program.as_os_str().len(),
        ),
    };
    let mut keys: BTreeSet<OsString> = std::env::vars_os().map(|(key, _)| key).collect();
    keys.insert("SHELL".into());
    keys.extend(options.injected_env().map(|(key, _)| key.into()));
    keys.extend(options.env.iter().map(|(key, _)| key.clone()));
    let env_bytes = keys
        .iter()
        .filter_map(|key| Some((key, command.get_env(key)?)))
        .fold(0usize, |sum, (key, value)| {
            sum.saturating_add(string(key.len().saturating_add(1 + value.len())))
        });
    ExecFootprint {
        bytes: arg_bytes
            .saturating_add(env_bytes)
            .saturating_add(2 * POINTER)
            .saturating_add(path.saturating_add(1)),
        longest,
    }
}

/// Test-only failure injection at the points of [`NativePty::spawn`] that
/// run after the child exists, so the tests can prove each one cleans up.
/// Outside tests every point is a no-op.
mod fault {
    use std::io;

    /// A point after the fork where spawning can fail.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Step {
        /// Setting up the reader and writer descriptors.
        Setup,
        /// Creating the reader thread.
        ReaderThread,
        /// Creating the writer thread.
        WriterThread,
    }

    #[cfg(test)]
    thread_local! {
        static FAIL_AT: std::cell::Cell<Option<Step>> = const { std::cell::Cell::new(None) };
    }

    #[cfg(test)]
    thread_local! {
        static FORKED: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    }

    /// Makes the next spawn on this thread fail at `step`.
    #[cfg(test)]
    pub(super) fn fail_at(step: Step) {
        FAIL_AT.with(|cell| cell.set(Some(step)));
    }

    /// The pid of the last child a spawn on this thread forked, so a test
    /// can follow that one process instead of every child of the process.
    #[cfg(test)]
    pub(super) fn take_forked() -> Option<u32> {
        FORKED.with(std::cell::Cell::take)
    }

    /// Records that a spawn forked `pid` (tests only).
    #[cfg_attr(not(test), inline)]
    pub(super) fn forked(pid: u32) {
        #[cfg(test)]
        FORKED.with(|cell| cell.set(Some(pid)));
        let _ = pid;
    }

    /// Fails if a test asked for a failure at `step`.
    #[cfg_attr(
        not(test),
        expect(clippy::unnecessary_wraps, reason = "fails only in tests")
    )]
    #[cfg_attr(not(test), inline)]
    pub(super) fn point(step: Step) -> io::Result<()> {
        #[cfg(test)]
        if FAIL_AT.with(std::cell::Cell::get) == Some(step) {
            FAIL_AT.with(|cell| cell.set(None));
            return Err(io::Error::other(format!("injected failure at {step:?}")));
        }
        let _ = step;
        Ok(())
    }
}

impl Pty for NativePty {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.check_writable()?;
        self.queue.push_blocking(bytes)
    }

    fn try_write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.check_writable()?;
        self.queue.try_push(bytes)
    }

    fn resize(&mut self, size: PtySize) -> Result<(), PtyError> {
        check_size(size)?;
        self.master
            .resize(size.into())
            .map_err(|err| PtyError::Resize(err.to_string()))?;
        self.size = size;
        Ok(())
    }

    fn size(&self) -> PtySize {
        self.size
    }

    fn child_pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn exit_status(&self) -> Option<ExitStatus> {
        match &*self.phase() {
            Phase::Exited(status) => Some(status.clone()),
            Phase::Running | Phase::Reaping => None,
        }
    }

    /// Closes the write queue, then sends `SIGHUP`.
    ///
    /// Closing drops every queued chunk and stops the writer thread before
    /// the child is signalled: a chunk being written is abandoned after the
    /// `write(2)` in progress. At most one 1 KiB slice may still reach the
    /// child after this returns: a `write(2)` already running when the
    /// queue closes, or about to start (its closed check passed just before
    /// the close), completes, at once if the child is reading or whenever
    /// it next reads (see "Closing and pending input" on [`NativePty`]).
    /// The writer checks again before the next slice, so there is never
    /// more than one.
    ///
    /// A writer blocked in `write(2)` is not woken by closing its duplicate
    /// of the master: on macOS that `close(2)` hangs the closing thread
    /// instead of failing the write, so no design can rely on it.
    fn hangup(&mut self) -> io::Result<()> {
        self.queue.close();
        self.signal_if_running(Signal::SIGHUP)
    }

    /// Closes the write queue as [`Pty::hangup`] does, then sends
    /// `SIGKILL`.
    fn kill(&mut self) -> io::Result<()> {
        self.queue.close();
        self.signal_if_running(Signal::SIGKILL)
    }
}

impl Drop for NativePty {
    fn drop(&mut self) {
        // Same order as `hangup`: drop pending input and stop the writer,
        // then signal a child that is still running. With 1 KiB slices the
        // reverse order is not observable from outside (the writer would
        // stop microseconds later either way), but closing first means no
        // slice is started after the child was told to go, and it keeps
        // the three closing paths identical. The writer's descriptor is a
        // bare duplicate of the master, so closing it injects nothing into
        // the child's input. The reader thread keeps its own handle on the
        // master and reaps the child once it goes away.
        self.queue.close();
        if let Err(err) = self.signal_if_running(Signal::SIGHUP) {
            tracing::debug!(pid = self.pid, %err, "hangup on drop failed");
        }
    }
}

impl fmt::Debug for NativePty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativePty")
            .field("pid", &self.pid)
            .field("size", &self.size)
            .field("phase", &*self.phase())
            .field("queued_bytes", &self.queue.queued_bytes())
            .finish_non_exhaustive()
    }
}

/// Duplicates the master descriptor into a plain [`File`] for the writer
/// thread.
///
/// `portable-pty`'s own writer writes a newline and `VEOF` into the child
/// when dropped, which a terminal must never do; a duplicate descriptor
/// closes silently.
fn duplicate_master(master: &dyn MasterPty) -> Result<File, PtyError> {
    let raw = master
        .as_raw_fd()
        .ok_or_else(|| PtyError::Io(io::Error::other("pty master exposes no file descriptor")))?;
    // SAFETY: `raw` is the descriptor owned by `master`, which is borrowed
    // for the whole expression and so stays open; the `BorrowedFd` does not
    // outlive this statement and ownership of the duplicate is taken by the
    // returned `OwnedFd`.
    let duplicate = unsafe { BorrowedFd::borrow_raw(raw) }
        .try_clone_to_owned()
        .map_err(PtyError::Io)?;
    Ok(File::from(duplicate))
}

fn check_size(size: PtySize) -> Result<(), PtyError> {
    if size.rows == 0 || size.cols == 0 {
        return Err(PtyError::InvalidSize(size));
    }
    Ok(())
}

/// Reader thread body: forwards output until end-of-file, then reaps.
fn pump_output(
    mut reader: Box<dyn Read + Send>,
    mut child: Child,
    events: &SyncSender<PtyEvent>,
    phase: &Mutex<Phase>,
    queue: &WriteQueue,
) {
    let mut buf = vec![0u8; READ_CHUNK];
    let mut consumer_alive = true;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if consumer_alive && events.send(PtyEvent::Output(buf[..n].to_vec())).is_err() {
                    // Nobody is listening any more; keep draining so the
                    // child is not blocked on a full PTY and gets reaped.
                    consumer_alive = false;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => {
                // Linux and macOS report EIO on the master once the slave
                // side is fully closed; that is the ordinary end of a
                // session, so only unexpected errors are worth a trace.
                if err.raw_os_error() != Some(nix::errno::Errno::EIO as i32) {
                    tracing::debug!(%err, "pty read failed; treating as end of stream");
                }
                break;
            }
        }
    }

    // Announce the reap under the lock so no signal can target the pid
    // between `wait` returning and the status being recorded.
    *phase.lock().unwrap_or_else(PoisonError::into_inner) = Phase::Reaping;
    let status = match child.wait() {
        Ok(status) => convert_status(status),
        Err(err) => {
            tracing::warn!(%err, "waiting for pty child failed");
            ExitStatus::Exited(-1)
        }
    };
    *phase.lock().unwrap_or_else(PoisonError::into_inner) = Phase::Exited(status.clone());
    queue.close();
    if consumer_alive {
        let _ = events.send(PtyEvent::Exited(status));
    }
}

/// Writer thread body: drains queued chunks into the master until the
/// queue closes.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the thread owns its descriptor and closes it when it exits"
)]
fn pump_input(writer: File, queue: &WriteQueue) {
    while let Some(chunk) = queue.pop() {
        if let Err(err) = write_chunk(&writer, &chunk, queue) {
            tracing::debug!(%err, "pty write failed; closing writer");
            queue.close();
            break;
        }
    }
}

/// Writes `chunk` to the master in [`WRITE_SLICE`] pieces, abandoning the
/// rest as soon as the queue closes. The closed flag is an atomic load per
/// `write(2)`, so the hot path takes no lock beyond the queue's own.
fn write_chunk(mut writer: &File, mut chunk: &[u8], queue: &WriteQueue) -> io::Result<()> {
    while !chunk.is_empty() && !queue.is_closed() {
        match writer.write(&chunk[..chunk.len().min(WRITE_SLICE)]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => chunk = &chunk[n..],
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Bounded FIFO of pending input, measured in bytes rather than chunks so a
/// large paste cannot inflate the bound.
#[derive(Default)]
struct WriteQueue {
    state: Mutex<QueueState>,
    /// Set once, under the state lock (so condvar waiters cannot miss it),
    /// and read without it by the writer between partial writes.
    closed: AtomicBool,
    /// Signalled when bytes leave the queue or it closes.
    space: Condvar,
    /// Signalled when bytes enter the queue or it closes.
    data: Condvar,
}

#[derive(Default)]
struct QueueState {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
}

impl QueueState {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes += chunk.len();
        self.chunks.push_back(chunk.to_vec());
    }
}

impl WriteQueue {
    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn queued_bytes(&self) -> usize {
        self.lock().bytes
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Queues all of `bytes`, waiting for room as the writer drains. Input
    /// larger than the whole budget is fed through in budget-sized pieces.
    fn push_blocking(&self, mut bytes: &[u8]) -> io::Result<()> {
        let mut state = self.lock();
        while !bytes.is_empty() {
            while !self.is_closed() && state.bytes >= WRITE_QUEUE_BYTES {
                state = self
                    .space
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if self.is_closed() {
                return Err(closed_error());
            }
            let take = bytes.len().min(WRITE_QUEUE_BYTES - state.bytes);
            state.push(&bytes[..take]);
            self.data.notify_one();
            bytes = &bytes[take..];
        }
        Ok(())
    }

    /// Queues `bytes` only if they fit in the remaining budget.
    fn try_push(&self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.lock();
        if self.is_closed() {
            return Err(closed_error());
        }
        if state.bytes + bytes.len() > WRITE_QUEUE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "pty write queue is full",
            ));
        }
        state.push(bytes);
        self.data.notify_one();
        Ok(())
    }

    fn take(&self, state: &mut QueueState) -> Option<Vec<u8>> {
        if self.is_closed() {
            return None;
        }
        let chunk = state.chunks.pop_front()?;
        state.bytes -= chunk.len();
        self.space.notify_all();
        Some(chunk)
    }

    /// Waits for the next chunk; `None` once the queue is closed.
    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.lock();
        loop {
            if let Some(chunk) = self.take(&mut state) {
                return Some(chunk);
            }
            if self.is_closed() {
                return None;
            }
            state = self
                .data
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Closes the queue for good: drops every pending chunk, refuses new
    /// input, wakes blocked producers and stops the writer.
    fn close(&self) {
        let mut state = self.lock();
        self.closed.store(true, Ordering::Release);
        state.chunks.clear();
        state.bytes = 0;
        drop(state);
        self.space.notify_all();
        self.data.notify_all();
    }
}

fn closed_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "pty writer is closed")
}

fn build_command(options: &SpawnOptions) -> CommandBuilder {
    let mut command = match &options.program {
        Program::LoginShell => CommandBuilder::new_default_prog(),
        Program::Command { program, args } => {
            let mut argv = Vec::with_capacity(args.len() + 1);
            argv.push(program.clone().into_os_string());
            argv.extend(args.iter().cloned());
            CommandBuilder::from_argv(argv)
        }
    };
    for (key, value) in options.injected_env() {
        command.env(key, value);
    }
    for (key, value) in &options.env {
        command.env(key, value);
    }
    if let Some(dir) = &options.cwd {
        command.cwd(dir);
    }
    command
}

fn describe_program(program: &Program) -> String {
    match program {
        Program::LoginShell => "login shell".to_owned(),
        Program::Command { program, .. } => program.display().to_string(),
    }
}

fn io_error(err: impl fmt::Display) -> PtyError {
    PtyError::Io(io::Error::other(err.to_string()))
}

fn convert_status(status: std::process::ExitStatus) -> ExitStatus {
    match (status.signal(), status.code()) {
        (Some(signal), _) => ExitStatus::Signaled(signal),
        (None, Some(code)) => ExitStatus::Exited(code),
        // Stopped or continued statuses never come out of a blocking
        // `wait` without `WUNTRACED`; treat anything else as failure.
        (None, None) => ExitStatus::Exited(-1),
    }
}

impl From<PtySize> for portable_pty::PtySize {
    fn from(size: PtySize) -> Self {
        Self {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_dimensions_are_rejected() {
        assert!(matches!(
            check_size(PtySize::new(0, 80)),
            Err(PtyError::InvalidSize(_))
        ));
        assert!(matches!(
            check_size(PtySize::new(24, 0)),
            Err(PtyError::InvalidSize(_))
        ));
        assert!(check_size(PtySize::new(1, 1)).is_ok());
    }

    /// Threads of this process, from `ps -M` (one header line plus one line
    /// per thread). nextest runs each test alone in its process.
    fn thread_count() -> usize {
        let output = std::process::Command::new("ps")
            .args(["-M", "-p", &std::process::id().to_string()])
            .output()
            .expect("run ps");
        String::from_utf8_lossy(&output.stdout).lines().count() - 1
    }

    fn open_fds() -> usize {
        std::fs::read_dir("/dev/fd").expect("list /dev/fd").count()
    }

    /// Whether process `pid` is gone: reaped, not merely exited (a zombie
    /// still answers `kill(pid, 0)`).
    fn reaped(pid: u32) -> bool {
        let pid = Pid::from_raw(i32::try_from(pid).unwrap());
        signal::kill(pid, None) == Err(nix::errno::Errno::ESRCH)
    }

    /// Spawning fails at `step`, after the child was forked; the child must
    /// be killed and reaped, and every descriptor and thread released.
    ///
    /// The child is followed by its pid, so this holds under `cargo test`
    /// too, where sibling tests have children of their own; thread and
    /// descriptor counts are process-wide and only checked when the test
    /// runs alone in its process, as under nextest.
    fn assert_failure_at_cleans_up(step: fault::Step) {
        let alone = std::env::var_os("NEXTEST").is_some();
        let (threads, fds) = (thread_count(), open_fds());
        let options = SpawnOptions::command("/bin/sh", "fault").args(["-c", "sleep 30"]);
        let _ = fault::take_forked();
        fault::fail_at(step);
        let started = std::time::Instant::now();
        match NativePty::spawn(&options) {
            Err(PtyError::Io(err)) => assert!(err.to_string().contains("injected"), "{err}"),
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("the injected failure at {step:?} did not fire"),
        }
        let pid = fault::take_forked().expect("the failure came after the fork");
        // A reader thread that already owns the child reaps it once it is
        // killed, so give that a moment; `sleep 30` would take far longer.
        while !reaped(pid) {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "the child {pid} of a failed spawn at {step:?} was not reaped"
            );
            thread::sleep(std::time::Duration::from_millis(10));
        }
        while alone && thread_count() > threads {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "a thread outlived the failed spawn at {step:?}"
            );
            thread::sleep(std::time::Duration::from_millis(10));
        }
        if alone {
            assert_eq!(
                open_fds(),
                fds,
                "a failed spawn at {step:?} leaked a descriptor"
            );
        }
    }

    #[test]
    fn a_failure_while_setting_up_descriptors_kills_and_reaps_the_child() {
        assert_failure_at_cleans_up(fault::Step::Setup);
    }

    #[test]
    fn a_reader_thread_that_cannot_start_kills_and_reaps_the_child() {
        assert_failure_at_cleans_up(fault::Step::ReaderThread);
    }

    #[test]
    fn a_writer_thread_that_cannot_start_kills_and_reaps_the_child() {
        assert_failure_at_cleans_up(fault::Step::WriterThread);
    }

    fn bytes(options: &SpawnOptions) -> usize {
        exec_bytes(&build_command(options), options).bytes
    }

    #[test]
    fn exec_bytes_count_strings_nuls_and_pointers() {
        const POINTER: usize = size_of::<*const u8>();
        let base = SpawnOptions::command("/bin/sh", "s");
        let empty = bytes(&base);
        // One more argument costs its bytes, a NUL and a pointer.
        assert_eq!(bytes(&base.clone().arg("abcd")), empty + 4 + 1 + POINTER);
        // A new variable costs `KEY=value`, a NUL and a pointer.
        let with_var = base.clone().env("TESSERA_EXEC_BYTES_TEST", "xyz");
        assert_eq!(
            bytes(&with_var),
            empty + "TESSERA_EXEC_BYTES_TEST=xyz".len() + 1 + POINTER
        );
        // Overriding an injected variable replaces its value, not its entry.
        let session = SpawnOptions::command("/bin/sh", "a");
        let longer = SpawnOptions::command("/bin/sh", "abc");
        assert_eq!(bytes(&longer), bytes(&session) + 2);
        let overridden = session.clone().env(crate::SESSION_ID_VAR, "abc");
        assert_eq!(bytes(&overridden), bytes(&longer));
        // The fixed part: argv[0] and the path, two NULL terminators and
        // every inherited and injected variable.
        let inherited: usize = std::env::vars_os()
            .filter(|(key, _)| {
                key != "SHELL" && !base.injected_env().iter().any(|(k, _)| key == *k)
            })
            .map(|(key, value)| key.len() + value.len() + 2 + POINTER)
            .sum();
        assert!(empty > inherited + 2 * ("/bin/sh".len() + 1) + 2 * POINTER);
        // The longest string is tracked, NUL included.
        let long = base.arg("x".repeat(70_000));
        let footprint = exec_bytes(&build_command(&long), &long);
        assert!(footprint.longest >= 70_001, "{footprint:?}");
    }

    #[test]
    fn a_login_shell_counts_its_dash_name_shell_variable_and_path() {
        // `SHELL=/x/ab` runs `/x/ab` as `-ab`; one more character in the
        // name lengthens the variable, the path and `argv[0]` by one each.
        let shell = |path: &str| SpawnOptions::login_shell("s").env("SHELL", path);
        assert_eq!(bytes(&shell("/x/abc")), bytes(&shell("/x/ab")) + 3);
        // A directory in the path adds to the variable and the path only.
        assert_eq!(bytes(&shell("/xy/ab")), bytes(&shell("/x/ab")) + 2);
        // Absolutely: the same program run as a command has `argv[0]`
        // `/x/ab` (5 bytes) where the login shell has `-ab` (3), and the
        // same path and environment.
        let command = SpawnOptions::command("/x/ab", "s").env("SHELL", "/x/ab");
        assert_eq!(bytes(&command), bytes(&shell("/x/ab")) + 2);
    }

    #[test]
    fn exec_limits_refuse_the_total_or_one_long_string() {
        let limits = ExecLimits {
            arg_max: Some(1000),
            max_string: Some(100),
        };
        let at = |bytes, longest| limits.check(ExecFootprint { bytes, longest });
        assert!(at(900, 100).is_ok(), "limits are inclusive");
        assert!(matches!(
            at(901, 10),
            Err(PtyError::ArgumentsTooLong {
                bytes: 901,
                max: 900
            })
        ));
        assert!(matches!(
            at(500, 101),
            Err(PtyError::ArgumentsTooLong {
                bytes: 101,
                max: 100
            })
        ));
        let unknown = ExecLimits {
            arg_max: None,
            max_string: None,
        };
        assert!(
            unknown
                .check(ExecFootprint {
                    bytes: usize::MAX,
                    longest: usize::MAX
                })
                .is_ok()
        );
    }

    #[test]
    fn system_limits_follow_the_kernel() {
        let limits = ExecLimits::system();
        let reported = arg_max().expect("ARG_MAX is known on every supported system");
        let expected = KERNEL_ARG_CAP.map_or(reported, |cap| reported.min(cap));
        assert_eq!(limits.arg_max, Some(expected));
        #[cfg(target_os = "macos")]
        assert_eq!(limits.max_string, None);
        #[cfg(target_os = "linux")]
        assert!(limits.max_string.is_some_and(|max| max >= 128 * 1024));
    }

    /// Options whose footprint is exactly `target` bytes: `sh -c 'exit 7'`
    /// plus arguments of at most 64 KiB (under any per-string limit).
    fn sized_to(target: usize) -> SpawnOptions {
        const POINTER: usize = size_of::<*const u8>();
        const PIECE: usize = 64 * 1024;
        let mut options = SpawnOptions::command("/bin/sh", "s").args(["-c", "exit 7", "argv0"]);
        let mut remaining = target - bytes(&options);
        // Stop while a whole string's overhead still fits after the last
        // piece, so the final argument (at most PIECE + 1 + POINTER bytes)
        // never needs a negative length.
        while remaining >= PIECE + 2 * (1 + POINTER) {
            options = options.arg("a".repeat(PIECE));
            remaining -= PIECE + 1 + POINTER;
        }
        options = options.arg("a".repeat(remaining - 1 - POINTER));
        assert_eq!(bytes(&options), target);
        options
    }

    #[test]
    fn a_spawn_at_the_limit_runs_and_one_byte_more_is_refused() {
        let arg_max = ExecLimits::system().arg_max.expect("known ARG_MAX");
        let max = arg_max - arg_max / ARG_MAX_HEADROOM_DIVISOR;
        let (_pty, events) = NativePty::spawn(&sized_to(max)).expect("the limit itself spawns");
        let status = loop {
            match events.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(PtyEvent::Exited(status)) => break status,
                Ok(PtyEvent::Output(_)) => {}
                Err(err) => panic!("no exit: {err}"),
            }
        };
        assert_eq!(status, ExitStatus::Exited(7), "the child really ran");
        let _ = fault::take_forked();
        assert!(matches!(
            NativePty::spawn(&sized_to(max + 1)),
            Err(PtyError::ArgumentsTooLong { bytes, max: limit }) if bytes == max + 1 && limit == max
        ));
        assert_eq!(fault::take_forked(), None, "nothing was forked");
    }

    #[test]
    fn queue_bounds_by_bytes_and_try_push_never_partially_queues() {
        let queue = WriteQueue::default();
        let chunk = vec![b'x'; WRITE_QUEUE_BYTES / 2];
        queue.try_push(&chunk).unwrap();
        queue.try_push(&chunk).unwrap();
        let err = queue.try_push(b"1").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(queue.queued_bytes(), WRITE_QUEUE_BYTES);
        assert_eq!(queue.pop().unwrap().len(), WRITE_QUEUE_BYTES / 2);
        queue.try_push(b"1").unwrap();
        assert_eq!(queue.queued_bytes(), WRITE_QUEUE_BYTES / 2 + 1);
    }

    #[test]
    fn close_drops_pending_chunks_and_wakes_a_blocked_producer() {
        let queue = Arc::new(WriteQueue::default());
        queue.try_push(&vec![b'a'; WRITE_QUEUE_BYTES]).unwrap();
        let producer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || queue.push_blocking(b"b"))
        };
        // Give the producer time to block on the full queue.
        thread::sleep(std::time::Duration::from_millis(50));
        queue.close();
        assert_eq!(
            producer.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(queue.queued_bytes(), 0, "closing kept the pending input");
        assert!(queue.lock().chunks.is_empty());
        assert!(queue.pop().is_none(), "writer got input after close");
    }

    #[test]
    fn blocking_push_splits_oversized_input_and_resumes_when_drained() {
        let queue = Arc::new(WriteQueue::default());
        let producer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || queue.push_blocking(&vec![b'y'; WRITE_QUEUE_BYTES + 10]))
        };
        let mut received = 0;
        while received < WRITE_QUEUE_BYTES + 10 {
            received += queue.pop().unwrap().len();
        }
        producer.join().unwrap().unwrap();
        queue.close();
        assert!(queue.pop().is_none());
        assert_eq!(
            queue.push_blocking(b"z").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            queue.try_push(b"z").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
