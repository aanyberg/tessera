//! [`NativePty`]: the `portable-pty` backed implementation of [`Pty`].

use std::collections::VecDeque;
use std::ffi::{CString, OsStr};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::panic;
use std::path::{Component, Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, MasterPty, PtyPair, native_pty_system};

use crate::bounded::{MAX_STUCK_HELPERS, Pool, TimedOut, run_bounded, run_bounded_in};
use crate::foreground::{self, ForegroundProcess};
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
/// Dropping a `NativePty` whose child is still running sends `SIGHUP` to
/// the child's process group, matching what closing a terminal window
/// does (see [`Pty::hangup`] for which processes that reaches). Nothing is ever written to
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
///
/// # Output written just before exit
///
/// On macOS the parent keeps one descriptor on the slave side open until
/// the reader reaches end-of-file or the session is closed. Without it,
/// the child's own exit closes the last slave descriptor, and the kernel's
/// last close waits only about 0.6 s for the master to drain the
/// terminal's output queue, then discards what is left: a child that
/// printed and exited while the reader thread was not scheduled (many
/// sessions in one busy process) lost its last output (#562). With the
/// parent's descriptor open, the child's close is not the last one, and
/// the session leader's exit drains the queue to the master without a time
/// limit before it revokes the terminal, which still ends the stream as
/// described above. The reader drains the master even once the events are
/// no longer received, so only a consumer that keeps the events but stops
/// reading them holds up an exiting child, as it already holds up a child
/// that writes.
///
/// That exit also waits, without a time limit and immune to `SIGKILL`,
/// while the terminal's output is stopped (`Ctrl-S` with `IXON`, or
/// `tcflow(TCOOFF)`) with output queued, until output restarts (any key
/// with `IXANY`, or `Ctrl-Q`). [`Pty::hangup`], [`Pty::kill`] and dropping
/// the handle therefore release the parent's descriptor before they
/// signal: that close is then the last one, the kernel discards the queued
/// output at once (the last close of a non-blocking description does not
/// wait for the drain), and the exit completes and the child is reaped.
/// The parent's descriptor is a description of its own, the slave reopened
/// by name with `O_NONBLOCK` before the fork, so the child's descriptors
/// stay blocking and the release never blocks the caller. A leader whose
/// session is not closed keeps waiting until output restarts, as a child
/// writing to the stopped terminal does.
pub struct NativePty {
    master: Box<dyn MasterPty + Send>,
    queue: Arc<WriteQueue>,
    pid: u32,
    size: PtySize,
    phase: Arc<Mutex<Phase>>,
    threads: IoThreads,
    held_slave: Arc<HeldSlave>,
}

/// The parent's own descriptor on the slave side, held on macOS until the
/// reader reaches end-of-file or the session is closed (see "Output written
/// just before exit" on [`NativePty`]); empty elsewhere.
#[derive(Debug, Default)]
struct HeldSlave(Mutex<Option<File>>);

impl HeldSlave {
    /// Opens a fresh, non-blocking description of `master`'s slave by name.
    /// Called before the child is forked: once the session leader has
    /// exited and revoked the terminal, a new open would make the slave
    /// open again and hold off the master's end-of-file.
    fn open(master: &dyn MasterPty) -> Result<Self, PtyError> {
        let name = master
            .tty_name()
            .ok_or_else(|| PtyError::Open("pty slave has no name".to_owned()))?;
        let slave = File::options()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(&name)
            .map_err(|err| PtyError::Open(format!("reopening {}: {err}", name.display())))?;
        Ok(Self(Mutex::new(Some(slave))))
    }

    /// Closes the descriptor if it is still held. Never blocks: the
    /// description is non-blocking, so a last close discards undrained
    /// output instead of waiting for the master to take it.
    fn release(&self) {
        drop(self.0.lock().unwrap_or_else(PoisonError::into_inner).take());
    }
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
    /// allocated (an open that lost the kernel's pty allocation race to a
    /// concurrent one, in this or another process, is retried a few times
    /// first, #563), [`PtyError::Spawn`] if the program is missing, a
    /// directory or not executable, if it is a `#!` script whose
    /// interpreter is missing, not a regular file or not executable
    /// (checked before anything is opened or forked; a script with CRLF
    /// line endings names `/bin/sh\r` and is refused with a hint), or if
    /// an argument or variable contains a NUL byte,
    /// [`PtyError::ProgramTimedOut`] if the file system gave no answer
    /// within 5 s while the program was looked up (see below),
    /// [`PtyError::OpenTimedOut`] if opening the PTY gave no answer within
    /// 5 s or four earlier opens are still without one (the open runs on
    /// a helper thread, so a `grantpt(3)` that never returns fails this
    /// spawn only), [`PtyError::Io`] if the
    /// reader or writer side of the master or one of their threads could
    /// not be set up.
    ///
    /// Every error leaves nothing behind: a child started before the
    /// failure is killed and reaped, and both ends of the PTY are closed.
    ///
    /// Finding the program and checking its `#!` line touch the file
    /// system, which can block (a stalled network volume, an iCloud file
    /// not yet downloaded), so both run on helper threads and the caller
    /// waits for each a bounded time. The lookup (the `stat` and
    /// `access(2)` calls `portable-pty` makes on the program, each `PATH`
    /// candidate and `$SHELL`) gets 5 s; without an answer by then the
    /// spawn fails with [`PtyError::ProgramTimedOut`] before
    /// `portable-pty`, which would make the same calls on the caller, is
    /// called. A program found through `PATH` is handed to `portable-pty`
    /// by its path, so it checks that one file again (then cached) instead
    /// of searching `PATH` again, and the child sees that path in
    /// `argv[0]`. The `#!` check (the program's first 512 bytes and a
    /// `stat` of the interpreter) gets 200 ms; without an answer by then
    /// the check is skipped and the program left to `exec`, so a script
    /// with a broken interpreter then spawns and its child ends at once as
    /// described below. While four earlier lookups or checks are still
    /// stuck, both give up at once: the spawn fails. A stuck helper's
    /// thread ends when its call does. `portable-pty` still stats the
    /// working directory and `$SHELL`, and resolves the program it is
    /// handed, on the caller's thread before it forks; after a lookup in
    /// time those answers are cached, but a volume that stops answering
    /// after the lookup can still hold the caller there. Both bounds are
    /// timed waits, which macOS may stretch for an app at background
    /// quality of service (App Nap): there the 200 ms wait took about
    /// 400 ms (#554).
    ///
    /// A file that is not a `#!` script and not a binary (`ENOEXEC`, such
    /// as a text file without a `#!` line or an empty file) is not refused:
    /// `execvp(3)` on macOS (and glibc) runs it with `/bin/sh`, so it
    /// spawns and runs.
    ///
    /// `exec` failures this cannot see before the fork make a spawn that
    /// returns `Ok` and a child that ends at once, because `portable-pty`
    /// closes the pipe through which the standard library would report the
    /// error: the child aborts and [`PtyEvent::Exited`] carries
    /// `Signaled(SIGABRT)`. On macOS these are a FIFO or device with an
    /// execute bit named as the program (`EACCES`; it is never opened, so
    /// the spawn cannot block on it), a fat binary without a slice for this
    /// machine or a damaged Mach-O file (`EBADARCH`, `EBADMACHO`), `ENOMEM`,
    /// and a file replaced between the check and the `exec`. `ETXTBSY`
    /// never occurs on macOS (a binary open for
    /// writing execs and is then killed, `Signaled(SIGKILL)`, as is a
    /// binary that code signing rejects); on Linux it would abort like the
    /// others.
    pub fn spawn(options: &SpawnOptions) -> Result<(Self, PtyEvents), PtyError> {
        check_size(options.size)?;
        let command = checked_command(options)?;
        let pair = open_pair(options.size)?;
        // On Linux the parent must not keep the slave open: the master
        // would never see end-of-file after the child exits. On macOS it
        // holds a description of its own until end-of-file or the close, so
        // the child's exit never makes the last close, which would drop
        // undrained output after 0.6 s (see "Output written just before
        // exit" above).
        let held_slave = Arc::new(if cfg!(target_os = "macos") {
            HeldSlave::open(pair.master.as_ref())?
        } else {
            HeldSlave::default()
        });

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
        // The child's own copy: `held_slave` is the parent's.
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
        let reader_slave = Arc::clone(&held_slave);
        // The child goes to the reader only once its thread exists, so a
        // failed start leaves the guard here (#566; see `hand_over_child`).
        let (child_tx, child_rx) = mpsc::sync_channel::<ChildGuard>(1);
        let reader_thread = spawn_thread(
            format!("pty-reader-{pid}"),
            fault::Step::ReaderThread,
            move || {
                if let Ok(child) = child_rx.recv() {
                    pump_output(
                        reader,
                        &reader_slave,
                        child.into_inner(),
                        &event_tx,
                        &reader_phase,
                        &reader_queue,
                    );
                }
            },
        );
        let reader_thread = match hand_over_child(reader_thread, &child_tx, child) {
            Ok(handle) => handle,
            Err((err, child)) => {
                // Close every reference to the terminal, the parent's slave
                // included, before the guard reaps.
                drop((writer, master));
                held_slave.release();
                drop(child);
                return Err(err.into());
            }
        };
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
            held_slave.release();
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
                held_slave,
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

    /// Delivers `signal` to the child's process group unless the child has
    /// been (or is being) reaped. Holding the phase lock across
    /// `killpg(2)` is what keeps the pid, and with it the group id, from
    /// being recycled underneath us.
    fn signal_if_running(&self, signal: Signal) -> io::Result<()> {
        signal_child(&self.phase, self.pid, signal)
    }
}

/// Sends `child` to a reader thread that started; a thread that did not
/// start, or that ended before it got the child, gives the guard back with
/// the error so the caller can close the master before the guard reaps: a
/// child that already printed cannot finish exiting (its terminal waits for
/// the output to be read) while the master is open and nobody reads it.
fn hand_over_child(
    reader_thread: io::Result<JoinHandle<()>>,
    child_tx: &mpsc::SyncSender<ChildGuard>,
    child: ChildGuard,
) -> Result<JoinHandle<()>, (io::Error, ChildGuard)> {
    match reader_thread {
        Ok(handle) => child_tx.send(child).map(|()| handle).map_err(|err| {
            let reason = "the pty reader thread ended before it got the child";
            (io::Error::other(reason), err.0)
        }),
        Err(err) => Err((err, child)),
    }
}

/// Delivers `signal` to the process group `pid` leads unless `phase` shows
/// the child has been (or is being) reaped; see
/// [`NativePty::signal_if_running`].
///
/// The child leads its own session and so its own process group
/// (`portable-pty` calls `setsid(2)` before `exec`, and a session leader
/// cannot move to another group), so the group holds the child and every
/// process it started without job control. A job a job-control shell moved
/// to a group of its own is not signalled (ADR 0018, amendment for #126).
///
/// While the phase lock shows `Running` the child is alive or a zombie, so
/// its pid, which is the group id, cannot have been reused. `ESRCH` and
/// `EPERM` count as success: macOS answers `EPERM` when no member could be
/// signalled, which is what a group whose only member is the child's
/// zombie gives (the reader thread may not have reaped it yet), and either
/// way no process we may signal is left in the group. A pid of 0 or 1 is
/// refused before any signal: `killpg(0)` would signal this process's own
/// group, and 1 is `launchd` or `init`.
fn signal_child(phase: &Mutex<Phase>, pid: u32, signal: Signal) -> io::Result<()> {
    let phase = phase.lock().unwrap_or_else(PoisonError::into_inner);
    if *phase != Phase::Running {
        return Ok(());
    }
    let pid = i32::try_from(pid)
        .ok()
        .filter(|&pid| pid > 1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))?;
    match signal::killpg(Pid::from_raw(pid), signal) {
        Ok(()) | Err(Errno::ESRCH | Errno::EPERM) => Ok(()),
        Err(err) => Err(err.into()),
    }
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

/// The hook [`set_thread_start_hook`] installed, if any.
static THREAD_START_HOOK: OnceLock<fn()> = OnceLock::new();

/// Installs `hook`, which every thread this crate starts (each PTY's
/// `pty-reader-<pid>` and `pty-writer-<pid>`, and the `pty-spawn-program`
/// and `pty-spawn-check` helpers that look up and check a program before
/// [`NativePty::spawn`] forks) runs first.
///
/// For thread attributes the caller sets per thread, such as the macOS
/// quality-of-service class `tessera-ffi` gives session threads (ADR
/// 0018). Process-wide; only the first hook is kept, and threads already
/// running are not affected. `tessera_app_core::session::set_thread_start_hook`
/// installs its hook here too. The hook runs once per thread start, never
/// per read or write. A hook that panics is logged and the thread carries
/// on. Returns whether `hook` was installed.
pub fn set_thread_start_hook(hook: fn()) -> bool {
    THREAD_START_HOOK.set(hook).is_ok()
}

/// Runs the hook [`set_thread_start_hook`] installed, if any. A panicking
/// hook is logged (without its payload) and the thread carries on: a
/// reader that died here would strand the child.
pub(crate) fn run_thread_start_hook() {
    if let Some(hook) = THREAD_START_HOOK.get()
        && panic::catch_unwind(*hook).is_err()
    {
        tracing::error!("pty thread start hook panicked");
    }
}

/// Starts a named thread that runs the start hook, then `body`. On failure
/// `body` is dropped without running, as [`thread::Builder::spawn`] does.
fn spawn_thread(
    name: String,
    step: fault::Step,
    body: impl FnOnce() + Send + 'static,
) -> io::Result<JoinHandle<()>> {
    if let Err(err) = fault::point(step) {
        drop(body);
        return Err(err);
    }
    thread::Builder::new().name(name).spawn(move || {
        run_thread_start_hook();
        body();
    })
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

/// The command for `options`, checked before anything is opened or
/// forked: its size ([`check_exec_size`]), its program looked up
/// ([`resolve_program_bounded`]) and, for a `#!` script, its interpreter
/// ([`check_interpreter_bounded`]).
fn checked_command(options: &SpawnOptions) -> Result<CommandBuilder, PtyError> {
    let mut command = build_command(options);
    check_exec_size(&command, options)?;
    let program = resolve_program_bounded(&mut command)?;
    check_interpreter_bounded(program, &command).map_err(|reason| PtyError::Spawn {
        program: describe_program(&options.program),
        reason,
    })?;
    Ok(command)
}

/// How long [`NativePty::spawn`] waits for the program to be looked up
/// ([`resolve_program`]: its path, each `PATH` candidate, or the login
/// shell stated and checked for execute permission). A file system that
/// gives no answer (a stalled network volume) would otherwise hold the
/// caller, the app's main thread, for as long as it takes, which on a
/// stalled `WebDAV` volume is more than a minute. Past it the spawn fails
/// with [`PtyError::ProgramTimedOut`]: `portable-pty` would stat the same
/// paths again on the caller. Longer than [`SPAWN_CHECK_TIMEOUT`] because
/// a lookup that ends late still lets the spawn go on, where a skipped
/// header check only loses a better error.
const SPAWN_PROGRAM_TIMEOUT: Duration = Duration::from_secs(5);

/// How long [`NativePty::spawn`] waits for [`check_interpreter`], which
/// reads the program's header and stats its `#!` interpreter: a file not
/// yet downloaded (an iCloud file) would otherwise hold the caller for as
/// long as the download takes. Past it the check is skipped and the
/// program left to `exec`. Under App Nap (background quality of service)
/// the wait can stretch to about twice this.
const SPAWN_CHECK_TIMEOUT: Duration = Duration::from_millis(200);

/// [`resolve_program`] on a `pty-spawn-program` helper thread, waited for
/// at most [`SPAWN_PROGRAM_TIMEOUT`] (see [`run_bounded`]). Without an
/// answer in time, or with [`crate::bounded::MAX_STUCK_HELPERS`] earlier
/// lookups or checks still stuck, the spawn fails with
/// [`PtyError::ProgramTimedOut`] before `portable-pty` is called; the
/// lookup's late answer is dropped on its helper.
///
/// A program found through a `PATH` search replaces `argv[0]` with its
/// path, so `portable-pty` checks that one file instead of searching
/// `PATH` again on the caller (the child sees the path in `argv[0]`). The
/// program as `exec` will run it, if it was found.
fn resolve_program_bounded(command: &mut CommandBuilder) -> Result<Option<PathBuf>, PtyError> {
    let lookup = command.clone();
    let resolved = run_bounded("pty-spawn-program", SPAWN_PROGRAM_TIMEOUT, move || {
        resolve_program(&lookup)
    })
    .map_err(|TimedOut| PtyError::ProgramTimedOut {
        path_bytes: program_len(command),
    })?;
    Ok(resolved.map(|Resolved { path, searched }| {
        if searched && let Some(program) = command.get_argv_mut().first_mut() {
            *program = path.clone().into_os_string();
        }
        path
    }))
}

/// [`check_interpreter`] of `program` on a `pty-spawn-check` helper thread,
/// waited for at most [`SPAWN_CHECK_TIMEOUT`] (see [`run_bounded`]).
/// Without an answer in time, or with
/// [`crate::bounded::MAX_STUCK_HELPERS`] earlier checks still stuck, the
/// check is skipped and the program left to `exec`, which runs it or fails
/// after the fork as described on [`NativePty::spawn`]. The skip is logged
/// at debug with the length of the program's path only (docs/security.md
/// S8). A program that was not found (`None`) is left to `portable-pty`,
/// which refuses it with its own reason.
fn check_interpreter_bounded(
    program: Option<PathBuf>,
    command: &CommandBuilder,
) -> Result<(), String> {
    let Some(program) = program else {
        return Ok(());
    };
    let program_len = program.as_os_str().len();
    let check = command.clone();
    run_bounded("pty-spawn-check", SPAWN_CHECK_TIMEOUT, move || {
        check_interpreter(&program, &check)
    })
    .unwrap_or_else(|TimedOut| {
        tracing::debug!(
            program_len,
            "the spawn check had no answer in time; leaving the program to exec"
        );
        Ok(())
    })
}

/// Bytes in the path of the program `command` runs as given (`$SHELL` for
/// the login shell), for errors that must not carry it. Reads only the
/// command, never the file system.
fn program_len(command: &CommandBuilder) -> usize {
    if command.is_default_prog() {
        command.get_env("SHELL").map_or(0, OsStr::len)
    } else {
        command
            .get_argv()
            .first()
            .map_or(0, |program| program.len())
    }
}

/// Bytes of a file `execve(2)` reads to find its `#!` line: XNU's
/// `IMG_SHSIZE`. A `#!` line that does not end within them is no `#!` line
/// to the kernel.
const SHEBANG_HEADER_BYTES: usize = 512;

/// Refuses, with the reason for [`PtyError::Spawn`], a program that is a
/// `#!` script whose interpreter `execve(2)` cannot run: missing, not a
/// regular file or not executable, including the interpreter `/bin/sh\r`
/// of a script saved with CRLF line endings.
///
/// Without this such a spawn would look successful: `portable-pty` closes
/// the pipe the standard library reports `exec` errors through, so the
/// child aborts after the fork (`SIGABRT`) instead of the spawn failing.
/// Every other kind of file is left to `exec`, a FIFO or device named as
/// the program included: it fails there with `EACCES`, after the fork. A
/// file with no `#!` line (`ENOEXEC`) is not refused, because `execvp(3)`
/// runs it with `/bin/sh`.
///
/// `program` is the file `portable-pty` will run (see
/// [`resolve_program`]). Best effort: the file can change between this
/// check and the `exec`.
///
/// The `#!` rules are XNU's ([`shebang_interpreter`], `IMG_SHSIZE`) and run
/// unchanged on Linux, which is a development target only: its kernel's
/// own rules differ, and this check does not follow them.
fn check_interpreter(program: &Path, command: &CommandBuilder) -> Result<(), String> {
    let header = read_header(program);
    let Some(interpreter) = shebang_interpreter(&header) else {
        return Ok(());
    };
    if interpreter.is_empty() {
        // `#!` followed by a NUL: the kernel looks up an empty path.
        return Err("its #! line names no interpreter (ENOENT)".to_owned());
    }
    let interpreter = Path::new(OsStr::from_bytes(interpreter));
    let crlf = if interpreter.as_os_str().as_bytes().ends_with(b"\r") {
        " (the script has CRLF line endings; save it with LF endings)"
    } else {
        ""
    };
    let path = if interpreter.is_absolute() {
        interpreter.to_path_buf()
    } else {
        // The kernel resolves a relative interpreter against the child's
        // working directory.
        let Some(dir) = working_dir(command) else {
            return Ok(());
        };
        dir.join(interpreter)
    };
    let problem = match std::fs::metadata(&path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => "does not exist (ENOENT)".to_owned(),
        Err(err) => format!("cannot be reached ({err})"),
        Ok(meta) if !meta.is_file() => "is not a regular file (EACCES)".to_owned(),
        Ok(_) if !executable(&path) => "is not executable (EACCES)".to_owned(),
        Ok(_) => return Ok(()),
    };
    // Quoted and escaped (Debug), so a `\r` or a control byte shows.
    let shown = String::from_utf8_lossy(interpreter.as_os_str().as_bytes());
    Err(format!("its #! interpreter {shown:?} {problem}{crlf}"))
}

/// A program as [`resolve_program`] found it.
#[derive(Debug, PartialEq, Eq)]
struct Resolved {
    /// The file `exec` will run.
    path: PathBuf,
    /// Found through a `PATH` search (a name without a `/` at its start or
    /// a `./` or `../` prefix).
    searched: bool,
}

/// The file `portable-pty` will hand to `exec` for `command`, looked up
/// with the same file system calls its `CommandBuilder::as_command` makes,
/// in the same order, so a slow file system holds this lookup (on a
/// helper, see [`resolve_program_bounded`]) and not the caller, and the
/// calls `portable-pty` repeats on the caller find their answers cached.
///
/// It first stats the requested working directory and checks `$SHELL`
/// for execute permission, which `portable-pty` does whatever it runs.
/// The login shell is `$SHELL` if executable, else the password database's
/// shell, else `/bin/sh`. Any other program is probed ([`probe`]) as given
/// when absolute, joined to the working directory when it starts with
/// `./` or `../`, and otherwise looked up in the command's `PATH` (the
/// caller's environment over this process's), each entry joined to the
/// working directory, taking the first candidate that is executable and
/// not a directory. `None` where the `PATH` search finds nothing or the
/// working directory or `PATH` is unknown here; `portable-pty` then
/// refuses the program with its own reason.
fn resolve_program(command: &CommandBuilder) -> Option<Resolved> {
    let dir = working_dir(command);
    let shell = command.get_shell();
    if command.is_default_prog() {
        return Some(Resolved {
            path: PathBuf::from(shell),
            searched: false,
        });
    }
    let exe = Path::new(command.get_argv().first()?);
    let given = |path: PathBuf| {
        probe(&path);
        Some(Resolved {
            path,
            searched: false,
        })
    };
    if exe.is_absolute() {
        return given(exe.to_path_buf());
    }
    let dir = dir?;
    if matches!(
        exe.components().next(),
        Some(Component::CurDir | Component::ParentDir)
    ) {
        return given(dir.join(exe));
    }
    std::env::split_paths(command.get_env("PATH")?)
        .map(|entry| dir.join(entry).join(exe))
        .find(|candidate| probe(candidate))
        .map(|path| Resolved {
            path,
            searched: true,
        })
}

/// Whether `path` is executable and not a directory, asked as
/// `portable-pty`'s `search_path` asks: a stat (`is_dir`), then
/// `access(X_OK)`, then `access(F_OK)` when that fails.
fn probe(path: &Path) -> bool {
    #[cfg(test)]
    slow_read::pause(path, slow_read::Stage::Lookup);
    if path.is_dir() {
        return false;
    }
    if executable(path) {
        return true;
    }
    access(path, nix::libc::F_OK);
    false
}

/// The child's working directory as `portable-pty` picks it: the requested
/// directory if it is one, else `$HOME` (from the command's environment).
/// `None` when neither is known here (`portable-pty` then asks the password
/// database).
fn working_dir(command: &CommandBuilder) -> Option<PathBuf> {
    command
        .get_cwd()
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir())
        .or_else(|| {
            command
                .get_env("HOME")
                .and_then(OsStr::to_str)
                .map(PathBuf::from)
        })
}

/// `access(path, X_OK)`: whether this process's real user may execute
/// `path`, as `portable-pty` asks before it spawns.
fn executable(path: &Path) -> bool {
    access(path, nix::libc::X_OK)
}

/// `access(path, mode) == 0`.
fn access(path: &Path, mode: nix::libc::c_int) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call, and
    // `access(2)` only reads it.
    unsafe { nix::libc::access(path.as_ptr(), mode) == 0 }
}

/// The first [`SHEBANG_HEADER_BYTES`] of `program`, zero-filled past its
/// end as the kernel's header buffer is. All zeros (no `#!`) for anything
/// that is not a regular file or cannot be opened and read: `exec` refuses
/// a non-regular file itself, and checking the type first means a FIFO or
/// device named as the program is never opened. The file is opened
/// read-only, non-blocking and without becoming a controlling terminal,
/// and symbolic links are followed, as `exec` follows them.
fn read_header(program: &Path) -> [u8; SHEBANG_HEADER_BYTES] {
    #[cfg(test)]
    slow_read::pause(program, slow_read::Stage::Header);
    let mut header = [0u8; SHEBANG_HEADER_BYTES];
    if !std::fs::metadata(program).is_ok_and(|meta| meta.is_file()) {
        return header;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOCTTY)
        .open(program)
    else {
        return header;
    };
    let mut filled = 0;
    while filled < header.len() {
        match file.read(&mut header[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return [0u8; SHEBANG_HEADER_BYTES],
        }
    }
    header
}

/// The interpreter a `#!` line names, parsed as XNU's `exec_shell_imgact`
/// parses it, or `None` where `execve(2)` would not treat the header as a
/// `#!` script (it then fails with `ENOEXEC`, and `execvp(3)` runs the file
/// with `/bin/sh`).
///
/// After `#!`, spaces and tabs are skipped; the line ends at the first `\n`
/// or `#`, which must come within the header (an end right after `#!`, or
/// none at all, is `ENOEXEC`); the interpreter runs from the first
/// non-blank byte to the next space or tab or the line's end, and stops at
/// a NUL as the kernel's C string does. A `\r` is part of the name, which
/// is how a CRLF script fails.
fn shebang_interpreter(header: &[u8]) -> Option<&[u8]> {
    let is_end = |byte: u8| byte == b'\n' || byte == b'#';
    let is_blank = |byte: u8| byte == b' ' || byte == b'\t';
    let line = header.strip_prefix(b"#!")?;
    let start = line.iter().position(|&byte| !is_blank(byte))?;
    let line = &line[start..];
    let end = line.iter().position(|&byte| is_end(byte))?;
    if end == 0 {
        return None;
    }
    let line = &line[..end];
    let token = line
        .iter()
        .position(|&byte| is_blank(byte))
        .map_or(line, |blank| &line[..blank]);
    Some(
        token
            .iter()
            .position(|&byte| byte == 0)
            .map_or(token, |nul| &token[..nul]),
    )
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

/// Test-only stand-in for a slow file: a [`probe`] (the program lookup's
/// stat) or [`read_header`] of a path a test names blocks until the test
/// releases it, as on a stalled network volume or a file not yet
/// downloaded. Only the named paths are held, each at one stage, so other
/// tests running beside it (`cargo test`) are unaffected. A held call
/// gives up after [`slow_read::MOST`], so a regression fails its test
/// instead of hanging.
#[cfg(test)]
mod slow_read {
    use std::path::{Path, PathBuf};
    use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
    use std::time::Duration;

    /// Longest a held call blocks without a release.
    pub(super) const MOST: Duration = Duration::from_secs(10);

    /// Where a path is held.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Stage {
        /// The program lookup's [`super::probe`].
        Lookup,
        /// [`super::read_header`].
        Header,
    }

    /// A held path and what its calls did.
    struct Hold {
        path: PathBuf,
        stage: Stage,
        held: bool,
        /// Names of the threads that started a call on it.
        callers: Vec<String>,
        /// Calls on it that have returned.
        finished: usize,
    }

    static HOLDS: Mutex<Vec<Hold>> = Mutex::new(Vec::new());
    static RELEASED: Condvar = Condvar::new();

    fn lock() -> MutexGuard<'static, Vec<Hold>> {
        HOLDS.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn find<'a>(holds: &'a mut [Hold], path: &Path, stage: Stage) -> Option<&'a mut Hold> {
        holds
            .iter_mut()
            .find(|hold| hold.path == path && hold.stage == stage)
    }

    /// Makes every call at `stage` on `path` block until [`release`].
    pub(super) fn hold(path: &Path, stage: Stage) {
        let mut holds = lock();
        holds.retain(|hold| hold.path != path || hold.stage != stage);
        holds.push(Hold {
            path: path.to_path_buf(),
            stage,
            held: true,
            callers: Vec::new(),
            finished: 0,
        });
    }

    /// Lets the held calls at `stage` on `path`, and every later one,
    /// return.
    pub(super) fn release(path: &Path, stage: Stage) {
        if let Some(hold) = find(&mut lock(), path, stage) {
            hold.held = false;
        }
        RELEASED.notify_all();
    }

    /// The names of the threads that started a call at `stage` on `path`,
    /// and how many of those calls have returned.
    pub(super) fn calls(path: &Path, stage: Stage) -> (Vec<String>, usize) {
        find(&mut lock(), path, stage).map_or_default(|hold| (hold.callers.clone(), hold.finished))
    }

    /// Blocks while `path` is held at `stage`, at most [`MOST`].
    pub(super) fn pause(path: &Path, stage: Stage) {
        let mut holds = lock();
        let Some(active) = find(&mut holds, path, stage) else {
            return;
        };
        let name = std::thread::current().name().unwrap_or("?").to_owned();
        active.callers.push(name);
        let (mut holds, _) = RELEASED
            .wait_timeout_while(holds, MOST, |holds| {
                find(holds, path, stage).is_some_and(|hold| hold.held)
            })
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(active) = find(&mut holds, path, stage) {
            active.finished += 1;
        }
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

    #[cfg(test)]
    thread_local! {
        static BEFORE_FAILURE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Makes the next spawn on this thread fail at `step`.
    #[cfg(test)]
    pub(super) fn fail_at(step: Step) {
        FAIL_AT.with(|cell| cell.set(Some(step)));
    }

    /// Like [`fail_at`], but runs `before` just before the failure fires,
    /// so a test can let the child get somewhere first.
    #[cfg(test)]
    pub(super) fn fail_at_after(step: Step, before: impl FnOnce() + 'static) {
        BEFORE_FAILURE.with(|cell| *cell.borrow_mut() = Some(Box::new(before)));
        fail_at(step);
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
            if let Some(before) = BEFORE_FAILURE.with(|cell| cell.borrow_mut().take()) {
                before();
            }
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
    ///
    /// On macOS it also releases the parent's slave descriptor before the
    /// signal, so a session leader stuck exiting on stopped output ends
    /// (see "Output written just before exit" on [`NativePty`]).
    fn hangup(&mut self) -> io::Result<()> {
        self.queue.close();
        self.held_slave.release();
        self.signal_if_running(Signal::SIGHUP)
    }

    /// Closes the write queue and releases the parent's slave descriptor as
    /// [`Pty::hangup`] does, then sends `SIGKILL`.
    fn kill(&mut self) -> io::Result<()> {
        self.queue.close();
        self.held_slave.release();
        self.signal_if_running(Signal::SIGKILL)
    }

    /// Compares `tcgetpgrp(3)` on the master with the child's pid under the
    /// phase lock, as signalling does: the reader cannot start reaping the
    /// child meanwhile, so its pid is not recycled under the comparison.
    /// `None` once the child is being reaped or has been.
    fn foreground_process(&self) -> Option<ForegroundProcess> {
        let phase = self.phase();
        if *phase != Phase::Running {
            return None;
        }
        foreground::foreground_process(self.master.as_ref(), self.pid)
    }

    /// Looks the child's pid up under the phase lock, as
    /// [`Pty::foreground_process`] does, so a recycled pid is never named.
    fn session_leader_name(&self) -> Option<String> {
        let phase = self.phase();
        if *phase != Phase::Running {
            return None;
        }
        foreground::process_name(self.pid)
    }
}

impl Drop for NativePty {
    fn drop(&mut self) {
        // Same order as `hangup`: drop pending input and stop the writer,
        // release the parent's slave descriptor, then signal a child that
        // is still running. With 1 KiB slices the
        // reverse order is not observable from outside (the writer would
        // stop microseconds later either way), but closing first means no
        // slice is started after the child was told to go, and it keeps
        // the three closing paths identical. The writer's descriptor is a
        // bare duplicate of the master, so closing it injects nothing into
        // the child's input. The reader thread keeps its own handle on the
        // master and reaps the child once it goes away.
        self.queue.close();
        self.held_slave.release();
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

/// How long [`NativePty::spawn`] waits for the pty pair to open (#568).
/// On macOS `grantpt(3)`, which `openpty(3)` calls, can spin in the kernel
/// and never return while other ptys are opened and closed at the same
/// time (once in 170 000 to 600 000 opens in a 16-thread stress run,
/// `tests/c/grantpt_stress.c`); only a signal ends that thread. The open
/// runs on a helper thread so such a spin fails one spawn with
/// [`PtyError::OpenTimedOut`] instead of holding its caller, the app's
/// main thread, for ever. An open that is merely slow (a loaded machine,
/// the retries of [`redrive_open`]) takes milliseconds, far inside this.
const PTY_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// The helpers that open ptys: a cap of [`MAX_STUCK_HELPERS`] of their
/// own (docs/security.md S5), apart from the program lookups and `#!`
/// checks of [`run_bounded`], so stuck lookups on a stalled volume cannot
/// make opens fail at once, nor stuck opens lookups.
static OPEN_HELPERS: Pool = Pool::new(MAX_STUCK_HELPERS);

/// Opens a pty pair of `size` ([`redrive_open`] over `openpty(3)`) on a
/// `pty-open` helper thread of [`OPEN_HELPERS`], waited for at most
/// [`PTY_OPEN_TIMEOUT`] (see [`run_bounded_in`]). Without an answer in
/// time, or with [`MAX_STUCK_HELPERS`] earlier opens still stuck (the
/// spawn then fails at once and starts no thread), the spawn fails with
/// [`PtyError::OpenTimedOut`] before anything is forked. An open that
/// returns after its caller gave up drops its result on the helper, which
/// closes both ends of the pty it opened.
fn open_pair(size: PtySize) -> Result<PtyPair, PtyError> {
    let hold = open_hold::take();
    run_bounded_in(
        open_hold::pool(&OPEN_HELPERS),
        "pty-open",
        PTY_OPEN_TIMEOUT,
        move || {
            open_hold::pause(&hold);
            let opened = redrive_open(|| native_pty_system().openpty(size.into()));
            open_hold::returned(&hold, opened.is_ok());
            opened
        },
    )
    .map_err(|TimedOut| {
        tracing::warn!("opening a pty gave no answer in time; failing the spawn");
        PtyError::OpenTimedOut
    })?
    .map_err(|err| PtyError::Open(err.to_string()))
}

/// Test-only stand-in for an open that does not return (#568): the next
/// [`open_pair`] on a thread a test arms blocks on its helper, before the
/// real open, until the test releases its [`open_hold::Gate`], and records
/// what it did. A test can also make the opens of one thread count against
/// a pool of its own, so filling a cap does not touch other tests running
/// beside it (`cargo test`). Outside tests every call is a no-op and
/// [`open_hold::pool`] returns its argument.
mod open_hold {
    use crate::bounded::Pool;

    /// What [`take`] hands the helper: the armed gate, if any (nothing
    /// outside tests).
    #[derive(Debug)]
    pub(super) struct Hold(#[cfg(test)] Option<std::sync::Arc<Gate>>);

    /// Longest a held open blocks without a release, so a regression
    /// fails its test instead of hanging it.
    #[cfg(test)]
    pub(super) const MOST: std::time::Duration = std::time::Duration::from_secs(60);

    /// A held open and what it did.
    #[cfg(test)]
    #[derive(Debug, Default)]
    pub(super) struct Gate {
        state: std::sync::Mutex<GateState>,
        changed: std::sync::Condvar,
    }

    #[cfg(test)]
    #[derive(Debug, Default, Clone, PartialEq, Eq)]
    pub(super) struct GateState {
        /// Released by the test.
        pub(super) released: bool,
        /// Names of the threads that entered the held open.
        pub(super) entered: Vec<String>,
        /// Held opens whose real open then succeeded.
        pub(super) opened: usize,
    }

    #[cfg(test)]
    impl Gate {
        fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        /// What the held opens did so far.
        pub(super) fn state(&self) -> GateState {
            self.lock().clone()
        }

        /// Lets the held open go on to the real open.
        pub(super) fn release(&self) {
            self.lock().released = true;
            self.changed.notify_all();
        }
    }

    #[cfg(test)]
    thread_local! {
        static NEXT: std::cell::RefCell<Option<std::sync::Arc<Gate>>> =
            const { std::cell::RefCell::new(None) };
        static POOL: std::cell::Cell<Option<&'static Pool>> = const { std::cell::Cell::new(None) };
    }

    /// Holds the next open on this thread at `gate`.
    #[cfg(test)]
    pub(super) fn hold_next(gate: &std::sync::Arc<Gate>) {
        NEXT.with(|next| *next.borrow_mut() = Some(std::sync::Arc::clone(gate)));
    }

    /// Makes this thread's opens count against `pool`.
    #[cfg(test)]
    pub(super) fn use_pool(pool: &'static Pool) {
        POOL.with(|cell| cell.set(Some(pool)));
    }

    /// The gate armed for this thread's open, disarming it.
    #[cfg_attr(not(test), inline)]
    pub(super) fn take() -> Hold {
        Hold(
            #[cfg(test)]
            NEXT.with(|next| next.borrow_mut().take()),
        )
    }

    /// The pool this thread's opens count against: `pool` unless a test
    /// chose another.
    #[cfg_attr(not(test), inline)]
    pub(super) fn pool(pool: &'static Pool) -> &'static Pool {
        #[cfg(test)]
        if let Some(chosen) = POOL.with(std::cell::Cell::get) {
            return chosen;
        }
        pool
    }

    /// On the helper, before the real open: blocks while `hold` is held,
    /// at most [`MOST`].
    #[cfg_attr(not(test), inline)]
    pub(super) fn pause(hold: &Hold) {
        #[cfg(test)]
        if let Hold(Some(gate)) = hold {
            let mut state = gate.lock();
            let name = std::thread::current().name().unwrap_or("?").to_owned();
            state.entered.push(name);
            let _ = gate
                .changed
                .wait_timeout_while(state, MOST, |state| !state.released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let _ = hold;
    }

    /// On the helper, after the real open: records a held open that
    /// succeeded.
    #[cfg_attr(not(test), inline)]
    pub(super) fn returned(hold: &Hold, ok: bool) {
        #[cfg(test)]
        if let (Hold(Some(gate)), true) = (hold, ok) {
            gate.lock().opened += 1;
            gate.changed.notify_all();
        }
        let _ = (hold, ok);
    }
}

/// The errno XNU uses inside the kernel to redrive an open that lost the
/// pty allocation race (`EREDRIVEOPEN` in `sys/errno.h`, kernel only).
///
/// Two opens of `/dev/ptmx` at the same time, in any processes, can be
/// handed the same minor number: the clone picks the first free slot and
/// claims it only later, with the devfs lock dropped in between. The loser
/// gets `EREDRIVEOPEN`, which `vn_open_auth` redrives ten times and then
/// returns to user space, where `openpty(3)` reports it as
/// `Unknown error: -6` (#563).
const EREDRIVEOPEN: i32 = -6;
/// Most `openpty` calls one spawn makes while the kernel keeps reporting
/// [`EREDRIVEOPEN`]. A lost race leaves no state behind, so the next
/// attempt is a fresh open; at 16 threads opening ptys as fast as they
/// can, no open needed more than one retry in 160 000.
const OPEN_ATTEMPTS: u32 = 8;

/// Runs `open` until it succeeds, fails with anything but a lost pty
/// allocation race ([`EREDRIVEOPEN`]), or has run [`OPEN_ATTEMPTS`] times,
/// sleeping one more millisecond after each lost race so a competing open
/// can claim its minor first.
///
/// `portable-pty` reports a failed `openpty(3)` as text only (`failed to
/// openpty: {errno:?}`), so the race is recognised by the `Debug` form of
/// the errno inside that text.
///
/// A process-wide lock around `openpty` is no fix: the race is lost to
/// opens in other processes too, and a rare kernel spin in `grantpt(3)`
/// (#563) would then hold up every later spawn instead of one.
fn redrive_open<T, E: fmt::Display>(mut open: impl FnMut() -> Result<T, E>) -> Result<T, E> {
    let mut attempt: u32 = 1;
    loop {
        match open() {
            Err(err) if attempt < OPEN_ATTEMPTS && lost_allocation_race(&err) => {
                tracing::debug!(attempt, "pty allocation race lost; opening again");
                thread::sleep(Duration::from_millis(attempt.into()));
                attempt += 1;
            }
            result => return result,
        }
    }
}

/// Whether `err` is `openpty(3)` failing with [`EREDRIVEOPEN`].
fn lost_allocation_race(err: &impl fmt::Display) -> bool {
    let raced = format!("{:?}", io::Error::from_raw_os_error(EREDRIVEOPEN));
    err.to_string().contains(&raced)
}

fn check_size(size: PtySize) -> Result<(), PtyError> {
    if size.rows == 0 || size.cols == 0 {
        return Err(PtyError::InvalidSize(size));
    }
    Ok(())
}

/// Reader thread body: forwards output until end-of-file, releases the
/// parent's slave descriptor (held only on macOS), then reaps.
fn pump_output(
    mut reader: Box<dyn Read + Send>,
    held_slave: &HeldSlave,
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
    held_slave.release();

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

    /// `openpty` failing with `errno`, worded as `portable-pty` 0.9 words it
    /// (#563 saw `failed to openpty: Os { code: -6, kind: Uncategorized,
    /// message: "Unknown error: -6" }`).
    fn openpty_error(errno: i32) -> String {
        format!(
            "failed to openpty: {:?}",
            io::Error::from_raw_os_error(errno)
        )
    }

    /// Runs [`redrive_open`] over `results`, one per attempt, and returns
    /// its result with the number of attempts it made.
    fn redrive(results: &[Result<u8, String>]) -> (Result<u8, String>, usize) {
        let mut calls = 0;
        let result = redrive_open(|| {
            calls += 1;
            results[calls - 1].clone()
        });
        (result, calls)
    }

    #[test]
    fn a_lost_allocation_race_is_opened_again() {
        let raced = Err(openpty_error(EREDRIVEOPEN));
        assert_eq!(redrive(&[raced.clone(), Ok(7)]), (Ok(7), 2));
        assert_eq!(
            redrive(&[raced.clone(), raced.clone(), raced, Ok(7)]),
            (Ok(7), 4)
        );
    }

    #[test]
    fn redriving_stops_after_open_attempts() {
        let raced = Err(openpty_error(EREDRIVEOPEN));
        let results = vec![raced.clone(); OPEN_ATTEMPTS as usize + 1];
        assert_eq!(redrive(&results), (raced, OPEN_ATTEMPTS as usize));
    }

    #[test]
    fn other_open_failures_are_not_retried() {
        // Descriptors or ptys exhausted, no such device, and an errno whose
        // text starts like the race's.
        let errnos = [Errno::EMFILE, Errno::EAGAIN, Errno::ENXIO].map(|errno| errno as i32);
        for errno in errnos.into_iter().chain([-60, 6]) {
            let failed = Err(openpty_error(errno));
            assert_eq!(
                redrive(&[failed.clone(), Ok(7)]),
                (failed, 1),
                "errno {errno}"
            );
        }
        assert_eq!(redrive(&[Ok(7)]), (Ok(7), 1));
    }

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

    /// #566: a child that printed before its reader thread failed to start
    /// cannot finish exiting while the master is open and nobody reads it
    /// (on macOS the session leader's exit waits for its output to drain),
    /// so the spawn must close the master before it reaps the child.
    #[test]
    fn a_reader_thread_that_cannot_start_after_output_fails_the_spawn() {
        let marker = std::env::temp_dir().join(format!(
            "tessera-pty-566-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let _ = std::fs::remove_file(&marker);
        let alone = std::env::var_os("NEXTEST").is_some();
        let fds = open_fds();
        let script = format!("printf printed; : > '{}'; sleep 30", marker.display());
        let options = SpawnOptions::command("/bin/sh", "fault").args(["-c", &script]);
        let (done_tx, done_rx) = mpsc::channel();
        let wait_for = marker.clone();
        // The spawn runs on its own thread so a hang fails this test in
        // bounded time instead of hanging it.
        thread::spawn(move || {
            let _ = fault::take_forked();
            fault::fail_at_after(fault::Step::ReaderThread, move || {
                // The child writes the marker after its output, so the
                // output is queued on the terminal once the marker exists.
                let started = std::time::Instant::now();
                while !wait_for.exists() && started.elapsed() < std::time::Duration::from_secs(10) {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
            });
            let result = NativePty::spawn(&options).map(|_| ());
            let _ = done_tx.send((result, fault::take_forked()));
        });
        let outcome = done_rx.recv_timeout(std::time::Duration::from_secs(20));
        let printed = marker.exists();
        let _ = std::fs::remove_file(&marker);
        let (result, pid) = outcome.expect("the spawn did not return after its reader failed");
        assert!(printed, "the child never got to print before the failure");
        match result {
            Err(PtyError::Io(err)) => assert!(err.to_string().contains("injected"), "{err}"),
            Err(other) => panic!("unexpected error: {other}"),
            Ok(()) => panic!("the injected failure did not fire"),
        }
        let pid = pid.expect("the failure came after the fork");
        assert!(reaped(pid), "the spawn returned before reaping child {pid}");
        // The parent's held slave (#562) is released on this path too.
        if alone {
            assert_eq!(open_fds(), fds, "the failed spawn leaked a descriptor");
        }
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

    /// #554 M3: the length [`PtyError::ProgramTimedOut`] reports is the
    /// login shell's `$SHELL`, and a command's own path otherwise.
    #[test]
    fn program_len_is_the_login_shells_variable_or_the_commands_path() {
        let shell = "/x/tessera-shell";
        let login = build_command(&SpawnOptions::login_shell("s").env("SHELL", shell));
        assert_eq!(program_len(&login), shell.len());
        let command = build_command(&SpawnOptions::command("/bin/sh", "s").env("SHELL", shell));
        assert_eq!(program_len(&command), "/bin/sh".len());
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
    fn shebang_lines_parse_as_the_kernel_parses_them() {
        let parse = |header: &[u8]| {
            let mut full = [0u8; SHEBANG_HEADER_BYTES];
            let len = header.len().min(SHEBANG_HEADER_BYTES);
            full[..len].copy_from_slice(&header[..len]);
            shebang_interpreter(&full).map(<[u8]>::to_vec)
        };
        let some = |name: &str| Some(name.as_bytes().to_vec());
        assert_eq!(parse(b"#!/bin/sh\n"), some("/bin/sh"));
        assert_eq!(parse(b"#! \t/bin/sh -e x\n"), some("/bin/sh"));
        assert_eq!(parse(b"#!/bin/sh\r\n"), some("/bin/sh\r"));
        assert_eq!(parse(b"#!/bin/sh#x\n"), some("/bin/sh"));
        assert_eq!(parse(b"#!/bin/sh   \n"), some("/bin/sh"));
        assert_eq!(parse(b"#!/bin/sh\0junk\n"), some("/bin/sh"));
        assert_eq!(parse(b"#!\0/x\n"), some(""));
        assert_eq!(parse(b"#!sh\n"), some("sh"));
        // Not a `#!` line: `ENOEXEC`, which `execvp` runs with `/bin/sh`.
        for header in [
            &b""[..],
            b"#!",
            b"#! \n",
            b"#!#/bin/sh\n",
            b"#!/bin/sh",
            b" #!/bin/sh\n",
            b"\xef\xbb\xbf#!/bin/sh\n",
            b"echo hi\n",
        ] {
            assert_eq!(parse(header), None, "{:?}", String::from_utf8_lossy(header));
        }
        // The line must end within the 512-byte header.
        let mut long = b"#!/bin/sh".to_vec();
        long.resize(SHEBANG_HEADER_BYTES - 1, b' ');
        long.push(b'\n');
        assert_eq!(parse(&long), some("/bin/sh"));
        long.insert(9, b' ');
        assert_eq!(parse(&long), None, "the line end moved past the header");
    }

    #[test]
    fn the_header_read_stops_at_512_bytes() {
        let dir = std::env::temp_dir().join(format!("tessera-pty-header-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big");
        let mut bytes = b"#!/bin/sh".to_vec();
        bytes.resize(4096, b'x');
        std::fs::write(&path, &bytes).unwrap();
        let header = read_header(&path);
        assert_eq!(&header[..], &bytes[..SHEBANG_HEADER_BYTES]);
        // A short file is zero-filled; a directory reads as no header.
        std::fs::write(&path, b"#!/x\n").unwrap();
        let header = read_header(&path);
        assert_eq!(&header[..5], b"#!/x\n");
        assert!(header[5..].iter().all(|&byte| byte == 0));
        assert!(read_header(&dir).iter().all(|&byte| byte == 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory of its own for one test, removed by the caller.
    fn scratch_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tessera-pty-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The boundary in plain numbers, through the file read and the parse
    /// together: a line ending on the 512th byte is a `#!` line and one
    /// ending on the 513th is not, so a header of 511 or 513 bytes fails
    /// here (the other tests size theirs from the constant).
    #[test]
    fn a_shebang_line_must_end_within_the_first_512_bytes() {
        let dir = scratch_dir("boundary");
        let path = dir.join("script");
        let parsed_with_line_end_at = |at: usize| {
            let mut bytes = b"#!/bin/sh".to_vec();
            bytes.resize(at, b' ');
            bytes.push(b'\n');
            bytes.resize(4096, b'x');
            std::fs::write(&path, &bytes).unwrap();
            shebang_interpreter(&read_header(&path)).map(<[u8]>::to_vec)
        };
        assert_eq!(
            parsed_with_line_end_at(510),
            Some(b"/bin/sh".to_vec()),
            "the 511th byte is in the header"
        );
        assert_eq!(
            parsed_with_line_end_at(511),
            Some(b"/bin/sh".to_vec()),
            "the 512th byte is in the header"
        );
        assert_eq!(
            parsed_with_line_end_at(512),
            None,
            "the 513th byte is not in the header"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An interpreter `stat` fails on for a reason other than a missing
    /// file (a path through a regular file, a symbolic link loop) is
    /// refused with that error as the system words it.
    #[test]
    fn an_interpreter_that_cannot_be_reached_is_refused_with_its_error() {
        let dir = scratch_dir("unreachable");
        let plain = dir.join("plain");
        std::fs::write(&plain, "").unwrap();
        let looped = dir.join("loop");
        std::os::unix::fs::symlink(&looped, &looped).unwrap();
        let script = dir.join("script");
        for (interpreter, errno) in [
            (plain.join("below"), nix::libc::ENOTDIR),
            (looped, nix::libc::ELOOP),
        ] {
            let shown = interpreter.to_str().unwrap();
            std::fs::write(&script, format!("#!{shown}\n")).unwrap();
            let command = build_command(&SpawnOptions::command(&script, "1"));
            assert_eq!(
                check_interpreter(&script, &command),
                Err(format!(
                    "its #! interpreter {shown:?} cannot be reached ({})",
                    io::Error::from_raw_os_error(errno)
                ))
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fields of every event logged on the thread it is the default
    /// subscriber of, each as `name=value` (values in `Debug` form).
    #[derive(Clone, Default)]
    struct LogRecorder(Arc<Mutex<Vec<Vec<String>>>>);

    struct FieldList(Vec<String>);

    impl tracing::field::Visit for FieldList {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
            self.0.push(format!("{}={value:?}", field.name()));
        }
    }

    impl tracing::Subscriber for LogRecorder {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut fields = FieldList(vec![format!("level={}", event.metadata().level())]);
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// How far past [`SPAWN_CHECK_TIMEOUT`] the bounded header check may
    /// return in [`a_slow_header_read_does_not_hold_the_caller`].
    const CHECK_BAR_MARGIN: Duration = Duration::from_secs(2);

    // A check that waited for its held read would return only after
    // `slow_read::MOST`, which must stay past the bar for the test to see it.
    const _: () = assert!(
        slow_read::MOST.as_nanos() > SPAWN_CHECK_TIMEOUT.as_nanos() + CHECK_BAR_MARGIN.as_nanos()
    );

    /// Holds `path` at `stage` (see [`slow_read::hold`]) until dropped,
    /// so a test that fails before its release does not leave helpers
    /// blocked for [`slow_read::MOST`] (under `cargo test` they hold slots
    /// of the process's helpers that other tests need).
    struct Held {
        path: PathBuf,
        stage: slow_read::Stage,
    }

    impl Drop for Held {
        fn drop(&mut self) {
            slow_read::release(&self.path, self.stage);
        }
    }

    /// [`slow_read::hold`], released when the returned guard drops.
    fn hold(path: &Path, stage: slow_read::Stage) -> Held {
        slow_read::hold(path, stage);
        Held {
            path: path.to_path_buf(),
            stage,
        }
    }

    /// #554: a program whose header read blocks (a file not yet
    /// downloaded) is checked on a helper, and the spawn goes on without
    /// the check once [`SPAWN_CHECK_TIMEOUT`] has passed, logging only the
    /// path's length. That the caller is not held is shown by order (the
    /// spawn returns while the read is still blocked); the wall-time bar is
    /// on the bounded wait alone, since the rest of a spawn (`openpty`,
    /// `fork`) takes what a loaded machine gives it. Its upper end is
    /// [`SPAWN_CHECK_TIMEOUT`] plus [`CHECK_BAR_MARGIN`] (#607): a held
    /// read blocks for [`slow_read::MOST`] (10 s, held above the bar by a
    /// const assert), so a check that waited for it still fails the bar,
    /// while a loaded CI runner, which took 310 ms and 322 ms, over the old
    /// 300 ms bar, wakes the timed-out caller well inside it.
    #[test]
    fn a_slow_header_read_does_not_hold_the_caller() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch_dir("slow-header");
        let script = dir.join("script");
        std::fs::write(&script, "#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let held = hold(&script, slow_read::Stage::Header);
        let header = || slow_read::calls(&script, slow_read::Stage::Header);
        let options = SpawnOptions::command(&script, "slow");

        // The bounded wait: the check of the held file gives up in time.
        let start = std::time::Instant::now();
        let checked = check_interpreter_bounded(Some(script.clone()), &build_command(&options));
        let took = start.elapsed();
        assert_eq!(checked, Ok(()), "a check without an answer passes");
        assert!(
            took >= SPAWN_CHECK_TIMEOUT && took < SPAWN_CHECK_TIMEOUT + CHECK_BAR_MARGIN,
            "the bounded check took {took:?}"
        );

        let log = LogRecorder::default();
        let spawned = tracing::subscriber::with_default(log.clone(), || NativePty::spawn(&options));
        let (pty, events) = spawned.expect("spawned without the check");
        // No held read has returned, so the spawn did not wait for one.
        let (readers, finished) = header();
        assert_eq!(
            finished, 0,
            "a held read returned before the spawn: {readers:?}"
        );
        assert!(pty.child_pid().is_some());

        // The header was read on the helpers, never on this thread.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while header().0.len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the header was not read twice: {:?}",
                header()
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(header().0, ["pty-spawn-check", "pty-spawn-check"]);

        // The child started: the script runs and exits with its code.
        let exit = loop {
            match events.recv_timeout(Duration::from_secs(10)) {
                Ok(PtyEvent::Exited(status)) => break status,
                Ok(PtyEvent::Output(_)) => {}
                Err(err) => panic!("no exit: {err}"),
            }
        };
        assert_eq!(exit, ExitStatus::Exited(7));

        let shown = script.to_str().unwrap();
        let logged = log.0.lock().unwrap().clone();
        let skipped: Vec<_> = logged
            .iter()
            .filter(|fields| {
                fields
                    .iter()
                    .any(|field| field.contains("no answer in time"))
            })
            .collect();
        assert_eq!(skipped.len(), 1, "{logged:?}");
        assert!(
            skipped[0].contains(&"level=DEBUG".to_owned()),
            "{skipped:?}"
        );
        assert!(
            skipped[0].contains(&format!("program_len={}", shown.len())),
            "{skipped:?}"
        );
        for fields in &logged {
            assert!(
                fields.iter().all(|field| !field.contains(shown)),
                "the path was logged: {fields:?}"
            );
        }
        drop(pty);
        drop(held);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while header().1 < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the held reads did not return on release"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Waits up to 10 s for `done` to hold.
    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// #554: a program whose lookup gets no answer from the file system (a
    /// stalled network volume) is looked up on a helper, and the spawn
    /// fails with [`PtyError::ProgramTimedOut`] once
    /// [`SPAWN_PROGRAM_TIMEOUT`] has passed, without calling
    /// `portable-pty` (which would stat the program again on the caller)
    /// and without naming the path. That the caller is not held is shown
    /// by order: the spawn returns while the lookup's stat is still
    /// blocked.
    #[test]
    fn a_program_lookup_without_an_answer_refuses_the_spawn_by_name() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch_dir("slow-lookup");
        let script = dir.join("script");
        std::fs::write(&script, "#!/bin/sh\nexit 7\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let held = hold(&script, slow_read::Stage::Lookup);
        let lookup = || slow_read::calls(&script, slow_read::Stage::Lookup);
        let options = SpawnOptions::command(&script, "slow");

        let start = std::time::Instant::now();
        let spawned = NativePty::spawn(&options);
        let took = start.elapsed();
        // The held stat has not returned, so the spawn did not wait for it.
        let (callers, finished) = lookup();
        assert_eq!(finished, 0, "the held stat returned before the spawn");
        assert_eq!(callers, ["pty-spawn-program"]);
        assert_eq!(fault::take_forked(), None, "a child was forked");
        let err = match spawned {
            Err(err @ PtyError::ProgramTimedOut { .. }) => err,
            other => panic!("not refused as timed out: {other:?}"),
        };
        assert!(
            matches!(err, PtyError::ProgramTimedOut { path_bytes }
                if path_bytes == script.as_os_str().len()),
            "{err:?}"
        );
        let shown = err.to_string();
        assert!(!shown.contains(dir.to_str().unwrap()), "{shown}");
        assert!(took >= SPAWN_PROGRAM_TIMEOUT, "refused after {took:?}");

        drop(held);
        wait_until("the held stat did not return on release", || {
            lookup().1 == 1
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A short-lived child that exits with `code`.
    fn exiting(code: u8) -> SpawnOptions {
        SpawnOptions::command("/bin/sh", "open").args(["-c", &format!("exit {code}")])
    }

    /// Waits for `events` to report the child's exit.
    fn exit_of(events: &PtyEvents) -> ExitStatus {
        loop {
            match events.recv_timeout(Duration::from_secs(10)) {
                Ok(PtyEvent::Exited(status)) => break status,
                Ok(PtyEvent::Output(_)) => {}
                Err(err) => panic!("no exit: {err}"),
            }
        }
    }

    /// Fills `pool` with [`MAX_STUCK_HELPERS`] opens stuck until the
    /// returned sender is dropped (or 30 s).
    fn fill_with_stuck(pool: &'static Pool) -> mpsc::Sender<()> {
        let (release, released) = mpsc::channel::<()>();
        let released = Arc::new(Mutex::new(released));
        for _ in 0..MAX_STUCK_HELPERS {
            let released = Arc::clone(&released);
            let result = run_bounded_in(pool, "pty-open", Duration::ZERO, move || {
                let _ = lock_ignoring_poison(&released).recv_timeout(Duration::from_secs(30));
            });
            assert_eq!(result.err(), Some(TimedOut));
        }
        release
    }

    fn lock_ignoring_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// #568: an open that never returns (as `grantpt(3)` spinning in the
    /// kernel) fails the spawn on the caller's thread with
    /// [`PtyError::OpenTimedOut`] once [`PTY_OPEN_TIMEOUT`] has passed,
    /// within 6 s and before anything is forked, while a spawn on another
    /// thread opens its pty and runs in the meantime: opens are not
    /// serialised behind a stuck one.
    #[test]
    fn an_open_that_never_returns_fails_that_spawn_alone() {
        static POOL: Pool = Pool::new(MAX_STUCK_HELPERS);
        static FIRST_RETURNED: AtomicBool = AtomicBool::new(false);
        open_hold::use_pool(&POOL);
        let gate = Arc::new(open_hold::Gate::default());
        open_hold::hold_next(&gate);

        let other = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                open_hold::use_pool(&POOL);
                wait_until("the held open was not entered", || {
                    !gate.state().entered.is_empty()
                });
                let (pty, events) = NativePty::spawn(&exiting(3)).expect("a concurrent spawn");
                let during = !FIRST_RETURNED.load(Ordering::SeqCst);
                let status = exit_of(&events);
                drop(pty);
                (during, status)
            })
        };

        let _ = fault::take_forked();
        let start = std::time::Instant::now();
        let spawned = NativePty::spawn(&exiting(0));
        let took = start.elapsed();
        FIRST_RETURNED.store(true, Ordering::SeqCst);
        assert!(
            matches!(spawned, Err(PtyError::OpenTimedOut)),
            "not refused as timed out: {spawned:?}"
        );
        assert!(
            took >= Duration::from_secs(5) && took < Duration::from_secs(6),
            "the spawn returned after {took:?}"
        );
        assert_eq!(fault::take_forked(), None, "a child was forked");
        let state = gate.state();
        assert_eq!(state.entered, ["pty-open"], "the open ran on the caller");
        assert_eq!(state.opened, 0, "the held open returned");
        assert_eq!(POOL.stuck(), 1);

        let (during, status) = other.join().expect("the concurrent spawn");
        assert!(during, "the concurrent spawn waited for the stuck open");
        assert_eq!(status, ExitStatus::Exited(3));

        gate.release();
        wait_until("the released open did not finish", || POOL.running() == 0);
        assert_eq!(gate.state().opened, 1);
    }

    /// #568: an open that returns after its spawn gave up closes both ends
    /// of the pty it opened on its helper: the process has as many
    /// descriptors afterwards as before. Counted only when the test runs
    /// alone in its process, as under nextest.
    #[test]
    fn a_late_open_closes_its_descriptors() {
        static POOL: Pool = Pool::new(MAX_STUCK_HELPERS);
        let alone = std::env::var_os("NEXTEST").is_some();
        open_hold::use_pool(&POOL);
        let fds = open_fds();
        let gate = Arc::new(open_hold::Gate::default());
        open_hold::hold_next(&gate);
        let spawned = NativePty::spawn(&exiting(0));
        assert!(
            matches!(spawned, Err(PtyError::OpenTimedOut)),
            "{spawned:?}"
        );
        gate.release();
        wait_until("the late open did not return", || gate.state().opened == 1);
        wait_until("the late open's helper did not finish", || {
            POOL.running() == 0
        });
        if alone {
            wait_until("the late open's descriptors are still open", || {
                open_fds() == fds
            });
        }
        // A pty opens again, and its descriptors close with it.
        let (pty, events) = NativePty::spawn(&exiting(4)).expect("spawn after a late open");
        assert_eq!(exit_of(&events), ExitStatus::Exited(4));
        drop((pty, events));
        if alone {
            wait_until("the later spawn's descriptors are still open", || {
                open_fds() == fds
            });
        }
    }

    /// #568, S5: with [`MAX_STUCK_HELPERS`] opens stuck, a spawn fails with
    /// [`PtyError::OpenTimedOut`] at once (well inside
    /// [`PTY_OPEN_TIMEOUT`]), starts no helper and never reaches its open;
    /// once the stuck opens return, spawns open ptys again.
    #[test]
    fn with_four_stuck_opens_a_spawn_fails_at_once_without_a_thread() {
        static POOL: Pool = Pool::new(MAX_STUCK_HELPERS);
        open_hold::use_pool(&POOL);
        let release = fill_with_stuck(&POOL);
        assert_eq!(POOL.stuck(), MAX_STUCK_HELPERS);
        let gate = Arc::new(open_hold::Gate::default());
        open_hold::hold_next(&gate);
        let _ = fault::take_forked();
        let start = std::time::Instant::now();
        let spawned = NativePty::spawn(&exiting(0));
        let took = start.elapsed();
        assert!(
            matches!(spawned, Err(PtyError::OpenTimedOut)),
            "{spawned:?}"
        );
        assert!(took < PTY_OPEN_TIMEOUT / 2, "the spawn waited {took:?}");
        assert_eq!(POOL.started(), MAX_STUCK_HELPERS, "a fifth helper started");
        assert_eq!(
            gate.state(),
            open_hold::GateState::default(),
            "the open ran"
        );
        assert_eq!(fault::take_forked(), None, "a child was forked");
        gate.release();

        drop(release);
        wait_until("the stuck opens did not finish", || POOL.running() == 0);
        let (pty, events) = NativePty::spawn(&exiting(5)).expect("spawn once the opens returned");
        assert_eq!(exit_of(&events), ExitStatus::Exited(5));
        drop(pty);
    }

    /// #568: opens and program lookups have caps of their own, so four
    /// stuck lookups (a stalled volume) leave opens alone and four stuck
    /// opens leave lookups alone; and the real open pool is capped. Fills
    /// the process's own pools, so it runs only alone in its process (as
    /// under nextest), where no other test spawns meanwhile.
    #[test]
    fn stuck_opens_and_stuck_lookups_do_not_share_a_cap() {
        if std::env::var_os("NEXTEST").is_none() {
            eprintln!("skipped: fills the process-wide helper pools; run under nextest");
            return;
        }
        let size = PtySize::new(24, 80);
        let opens = fill_with_stuck(&OPEN_HELPERS);
        assert!(
            matches!(open_pair(size), Err(PtyError::OpenTimedOut)),
            "the open pool is not capped"
        );
        assert_eq!(
            run_bounded("pty-spawn-program", Duration::from_secs(5), || 7),
            Ok(7),
            "stuck opens refused a lookup"
        );
        drop(opens);
        wait_until("the stuck opens did not finish", || {
            OPEN_HELPERS.running() == 0
        });

        let (release, released) = mpsc::channel::<()>();
        let released = Arc::new(Mutex::new(released));
        for stuck in 0..MAX_STUCK_HELPERS {
            // One short of the cap, a call still runs (#554 M3).
            assert_eq!(
                run_bounded("pty-spawn-check", Duration::from_secs(5), || 1),
                Ok(1),
                "{stuck} stuck lookups refused a call"
            );
            let released = Arc::clone(&released);
            let result = run_bounded("pty-spawn-check", Duration::ZERO, move || {
                let _ = lock_ignoring_poison(&released).recv_timeout(Duration::from_secs(30));
            });
            assert_eq!(result, Err(TimedOut));
        }
        assert_eq!(
            run_bounded("pty-spawn-check", Duration::from_secs(5), || 1),
            Err(TimedOut),
            "the lookup pool is not full"
        );
        let pair = open_pair(size).expect("stuck lookups refused an open");
        drop(pair);
        drop(release);
    }

    /// The lookup probes `PATH` candidates in order, through the same
    /// stat a held volume blocks, and a program it finds there reaches
    /// `portable-pty` by its path, so `portable-pty` does not search
    /// `PATH` again on the caller. A program given as a path keeps it.
    #[test]
    fn a_program_found_on_path_is_handed_over_by_its_path() {
        let dir = scratch_dir("path-lookup");
        let held = dir.join("held");
        std::fs::create_dir_all(&held).unwrap();
        let mut options = SpawnOptions::command("sh", "path");
        let path = std::env::join_paths([held.as_path(), Path::new("/bin")]).unwrap();
        options.env.push(("PATH".into(), path));

        // The first candidate is probed first, through the seam.
        let candidate = held.join("sh");
        let held = hold(&candidate, slow_read::Stage::Lookup);
        let command = build_command(&options);
        let lookup = thread::spawn(move || resolve_program(&command));
        wait_until("the first candidate was not probed", || {
            slow_read::calls(&candidate, slow_read::Stage::Lookup)
                .0
                .len()
                == 1
        });
        drop(held);
        assert_eq!(
            lookup.join().unwrap(),
            Some(Resolved {
                path: PathBuf::from("/bin/sh"),
                searched: true,
            })
        );

        let mut command = build_command(&options);
        let found = resolve_program_bounded(&mut command).unwrap();
        assert_eq!(found.as_deref(), Some(Path::new("/bin/sh")));
        assert_eq!(command.get_argv()[0], "/bin/sh");

        for given in ["/bin/sh", "./sh", "../bin/sh"] {
            let mut options = SpawnOptions::command(given, "path");
            options.cwd = Some(PathBuf::from("/bin"));
            let mut command = build_command(&options);
            let found = resolve_program_bounded(&mut command).unwrap();
            let expected = if given.starts_with('/') {
                PathBuf::from(given)
            } else {
                Path::new("/bin").join(given)
            };
            assert_eq!(found, Some(expected), "{given}");
            assert_eq!(command.get_argv()[0], given);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signalling_refuses_pids_that_name_no_child_of_ours() {
        let phase = Mutex::new(Phase::Running);
        // `killpg(0)` would signal this process's own group and `killpg(1)`
        // launchd's: refused before any signal. (1 is the one probed: as a
        // non-root user `killpg(1, 0)` fails with `EPERM`, which counts as
        // success, so a missing guard shows here without harm.)
        for pid in [1, u32::MAX] {
            let err = signal_child(&phase, pid, Signal::SIGHUP).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "pid {pid}");
        }
        assert!(signal_child(&Mutex::new(Phase::Reaping), 1, Signal::SIGHUP).is_ok());
    }

    /// The `ps` state letters of `pid` (`Z` for a zombie).
    fn process_state(pid: u32) -> String {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("run ps");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn signalling_a_child_that_exited_but_is_not_reaped_succeeds() {
        // The group's only member is the child's zombie: macOS `killpg`
        // fails with `EPERM` there, where `kill` on the pid succeeded. The
        // reader thread may not have reaped the child yet when a close
        // signals it, so this must not be an error.
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/usr/bin/true")
            .process_group(0)
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        while !process_state(child.id()).starts_with('Z') {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the child never became a zombie"
            );
            thread::sleep(std::time::Duration::from_millis(5));
        }
        #[cfg(target_os = "macos")]
        assert_eq!(
            signal::killpg(Pid::from_raw(i32::try_from(child.id()).unwrap()), None),
            Err(Errno::EPERM),
            "macOS no longer answers EPERM for a zombie-only group"
        );
        let phase = Mutex::new(Phase::Running);
        for signal in [Signal::SIGHUP, Signal::SIGKILL] {
            signal_child(&phase, child.id(), signal).unwrap();
        }
        child.wait().unwrap();
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
