//! Many sessions spawned at the same time all open their pty (#563).
//!
//! Two concurrent opens of `/dev/ptmx` can be handed the same minor number
//! by the macOS kernel; the loser's open is redriven a few times inside the
//! kernel and then fails with the internal errno `EREDRIVEOPEN` (-6),
//! "Unknown error: -6". The test lives in its own binary so its children
//! never meet the process-wide child and thread counts of `spawn.rs` under
//! `cargo test`.

#![cfg(unix)]

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use tessera_pty::{ExitStatus, NativePty, PtyEvent, SpawnOptions};

/// Threads that spawn at once.
const THREADS: usize = 16;
/// Sessions each thread spawns, one after another.
const SPAWNS_PER_THREAD: usize = 16;
/// Bound on each session's run, so a regression fails instead of hanging.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Spawns `/bin/echo` with a token of its own and waits for it to exit;
/// `Err` carries the reason. The session must show exactly its own token,
/// so two sessions handed one terminal (or a child given another's
/// arguments) fail too, not only a spawn that fails (#526, #563 M3).
fn echo_once(n: usize) -> Result<(), String> {
    let token = format!("token-{n}");
    let options =
        SpawnOptions::command("/bin/echo", format!("concurrent-{n}")).args([token.as_str()]);
    let (_pty, events) = NativePty::spawn(&options).map_err(|err| format!("spawn: {err}"))?;
    let deadline = Instant::now() + TIMEOUT;
    let mut output = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(PtyEvent::Exited(ExitStatus::Exited(0))) => break,
            Ok(PtyEvent::Exited(status)) => return Err(format!("exited with {status:?}")),
            Ok(PtyEvent::Output(bytes)) => output.extend_from_slice(&bytes),
            Err(err) => return Err(format!("waiting for exit: {err}")),
        }
    }
    let shown = String::from_utf8_lossy(&output);
    if shown == format!("{token}\r\n") {
        Ok(())
    } else {
        Err(format!("{token}: the session showed {shown:?}"))
    }
}

#[test]
fn concurrent_spawns_all_open_their_pty() {
    let start = Arc::new(Barrier::new(THREADS));
    let workers: Vec<_> = (0..THREADS)
        .map(|thread| {
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                (0..SPAWNS_PER_THREAD)
                    .filter_map(|i| echo_once(thread * SPAWNS_PER_THREAD + i).err())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let failures: Vec<String> = workers
        .into_iter()
        .flat_map(|worker| worker.join().expect("spawn thread panicked"))
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} concurrent spawns failed: {failures:?}",
        failures.len(),
        THREADS * SPAWNS_PER_THREAD
    );
}
