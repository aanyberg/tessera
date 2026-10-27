//! [`NativePty`]: the `portable-pty` backed implementation of [`Pty`].

use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::BorrowedFd;
use std::os::unix::process::ExitStatusExt;
use std::process::Child;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

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
/// the child's input on close: the writer is a plain duplicate of the master
/// descriptor, so dropping it only closes a file. The reader thread outlives
/// the handle until the child exits, so the process is always reaped.
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
        thread::Builder::new()
            .name(format!("pty-reader-{pid}"))
            .spawn(move || pump_output(reader, *child, &event_tx, &reader_phase, &reader_queue))?;
        let writer_queue = Arc::clone(&queue);
        thread::Builder::new()
            .name(format!("pty-writer-{pid}"))
            .spawn(move || pump_input(writer, &writer_queue))?;

        Ok((
            Self {
                master,
                queue,
                pid,
                size: options.size,
                phase,
            },
            event_rx,
        ))
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

    fn hangup(&mut self) -> io::Result<()> {
        self.signal_if_running(Signal::SIGHUP)
    }

    fn kill(&mut self) -> io::Result<()> {
        self.signal_if_running(Signal::SIGKILL)
    }
}

impl Drop for NativePty {
    fn drop(&mut self) {
        // Hang up a child that is still running, then close the queue so
        // the writer thread exits. The writer's descriptor is a bare
        // duplicate of the master, so closing it injects nothing into the
        // child's input. The reader thread keeps its own handle on the
        // master and reaps the child once it goes away.
        if let Err(err) = self.signal_if_running(Signal::SIGHUP) {
            tracing::debug!(pid = self.pid, %err, "hangup on drop failed");
        }
        self.queue.close();
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

/// Writer thread body: drains queued chunks into the master.
fn pump_input(mut writer: File, queue: &WriteQueue) {
    while let Some(chunk) = queue.pop() {
        if let Err(err) = writer.write_all(&chunk).and_then(|()| writer.flush()) {
            tracing::debug!(%err, "pty write failed; closing writer");
            queue.close();
            break;
        }
    }
}

/// Bounded FIFO of pending input, measured in bytes rather than chunks so a
/// large paste cannot inflate the bound.
#[derive(Default)]
struct WriteQueue {
    state: Mutex<QueueState>,
    /// Signalled when bytes leave the queue or it closes.
    space: Condvar,
    /// Signalled when bytes enter the queue or it closes.
    data: Condvar,
}

#[derive(Default)]
struct QueueState {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
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

    /// Queues all of `bytes`, waiting for room as the writer drains. Input
    /// larger than the whole budget is fed through in budget-sized pieces.
    fn push_blocking(&self, mut bytes: &[u8]) -> io::Result<()> {
        let mut state = self.lock();
        while !bytes.is_empty() {
            while !state.closed && state.bytes >= WRITE_QUEUE_BYTES {
                state = self
                    .space
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if state.closed {
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
        if state.closed {
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

    /// Waits for the next chunk; `None` once the queue is closed and empty.
    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = self.lock();
        loop {
            if let Some(chunk) = state.chunks.pop_front() {
                state.bytes -= chunk.len();
                self.space.notify_all();
                return Some(chunk);
            }
            if state.closed {
                return None;
            }
            state = self
                .data
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn close(&self) {
        self.lock().closed = true;
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

fn convert_status(status: std::process::ExitStatus) -> ExitStatus {
    match (status.signal(), status.code()) {
        (Some(signal), _) => ExitStatus::Signaled(signal),
        (None, Some(code)) => ExitStatus::Exited(code),
        // Stopped or continued statuses never come out of a blocking
        // `wait` without `WUNTRACED`; treat anything else as failure.
        (None, None) => ExitStatus::Exited(-1),
    }
}

fn io_error(err: impl fmt::Display) -> PtyError {
    PtyError::Io(io::Error::other(err.to_string()))
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
