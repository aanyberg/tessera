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
    /// columns, [`PtyError::Open`] if no PTY could be allocated,
    /// [`PtyError::Spawn`] if the program is missing or not executable,
    /// [`PtyError::Io`] if the reader or writer side of the master could not
    /// be set up.
    pub fn spawn(options: &SpawnOptions) -> Result<(Self, PtyEvents), PtyError> {
        check_size(options.size)?;
        let pair = native_pty_system()
            .openpty(options.size.into())
            .map_err(|err| PtyError::Open(err.to_string()))?;

        let command = build_command(options);
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
        let child = child
            .downcast::<Child>()
            .map_err(|_| spawn_error("unexpected child handle type".to_owned()))?;
        // The parent must not keep the slave open: the master would never
        // see end-of-file after the child exits.
        drop(pair.slave);

        let master = pair.master;
        let reader = master.try_clone_reader().map_err(io_error)?;
        let writer = duplicate_master(master.as_ref())?;
        let pid = child.id();

        let phase = Arc::new(Mutex::new(Phase::Running));
        let queue = Arc::new(WriteQueue::default());
        let (event_tx, event_rx) = mpsc::sync_channel(READ_QUEUE_CHUNKS);

        let reader_phase = Arc::clone(&phase);
        let reader_queue = Arc::clone(&queue);
        let reader_thread = thread::Builder::new()
            .name(format!("pty-reader-{pid}"))
            .spawn(move || pump_output(reader, *child, &event_tx, &reader_phase, &reader_queue))?;
        let writer_queue = Arc::clone(&queue);
        let writer_thread = thread::Builder::new()
            .name(format!("pty-writer-{pid}"))
            .spawn(move || pump_input(writer, &writer_queue))?;
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
        let phase = self.phase();
        if *phase != Phase::Running {
            return Ok(());
        }
        let pid = i32::try_from(self.pid)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pid out of range"))?;
        signal::kill(Pid::from_raw(pid), signal).map_err(io::Error::from)
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
