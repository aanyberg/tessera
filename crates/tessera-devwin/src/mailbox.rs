//! Session events from the reader thread to the event loop, coalesced.
//!
//! The core calls the observer on the session's reader thread once per
//! chunk of output, so a child that floods the terminal produces events far
//! faster than a window redraws. Forwarding each one through winit's
//! unbounded `EventLoopProxy` queue would let that queue grow without bound
//! while the main thread is busy drawing. Instead the observer
//! [`post`](Mailbox::post)s into a [`Mailbox`], which folds every event into
//! one [`Pending`] record (output: a flag; title: the latest; bell: a
//! count; exit: the status) and asks for a wake-up only when none is
//! outstanding. The event loop [`take`](Mailbox::take)s the record on each
//! wake-up. However fast the child writes, at most two wake-ups are queued
//! (one outstanding, plus one racing with a `take`), and the record holds
//! one title.

use std::sync::{Arc, Mutex, PoisonError};

use tessera_app_core::{ExitStatus, SessionEvent};

use crate::clipboard::ClipboardError;

/// Everything that happened in the session since the last
/// [`Mailbox::take`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pending {
    /// The terminal changed: redraw.
    pub output: bool,
    /// The most recent title the child set, if it set one.
    pub title: Option<String>,
    /// How many times the bell rang (saturating).
    pub bells: u32,
    /// How the child exited, if it did.
    pub exit: Option<ExitStatus>,
    /// The result of a clipboard read for a paste, if one finished.
    pub paste: Option<Result<String, ClipboardError>>,
}

#[derive(Debug, Default)]
struct State {
    pending: Pending,
    /// A wake-up was requested and its [`Mailbox::take`] has not run yet.
    wake_outstanding: bool,
}

/// Coalesces [`SessionEvent`]s between the reader thread and the event
/// loop; see the [module docs](self).
#[derive(Debug, Default)]
pub struct Mailbox {
    state: Mutex<State>,
}

impl Mailbox {
    /// Records `event`. Returns `true` when the caller must wake the event
    /// loop, which then calls [`Mailbox::take`]; `false` when a wake-up is
    /// already outstanding.
    pub fn post(&self, event: SessionEvent) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let pending = &mut state.pending;
        match event {
            SessionEvent::Output => pending.output = true,
            SessionEvent::Title(title) => pending.title = Some(title),
            SessionEvent::Bell => pending.bells = pending.bells.saturating_add(1),
            SessionEvent::Exit(status) => pending.exit = Some(status),
            // The dev window does not touch the system clipboard on a
            // program's behalf: nothing to wake the event loop for.
            SessionEvent::Clipboard(_) => return false,
        }
        !std::mem::replace(&mut state.wake_outstanding, true)
    }

    /// Records the result of a clipboard read (from the paste helper
    /// thread). Returns `true` when the caller must wake the event loop, as
    /// [`Mailbox::post`] does. The window runs one read at a time, so a
    /// result is never overwritten before it is taken.
    pub fn post_paste(&self, result: Result<String, ClipboardError>) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.pending.paste = Some(result);
        !std::mem::replace(&mut state.wake_outstanding, true)
    }

    /// Everything posted since the last call; the next
    /// [`Mailbox::post`] requests a wake-up again.
    pub fn take(&self) -> Pending {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.wake_outstanding = false;
        std::mem::take(&mut state.pending)
    }
}

/// Delivers a paste's clipboard result to a [`Mailbox`] exactly once.
///
/// Owned by the thread that reads the clipboard. [`PasteDelivery::deliver`]
/// posts the result; if the thread ends without delivering (a panic
/// unwinds through it), dropping the guard posts
/// [`ClipboardError::Lost`] instead, so the window never waits for a paste
/// forever. `wake` runs whenever posting asks for a wake-up.
pub struct PasteDelivery<W: Fn()> {
    mailbox: Arc<Mailbox>,
    wake: W,
    delivered: bool,
}

impl<W: Fn()> PasteDelivery<W> {
    /// A guard posting into `mailbox` and waking with `wake`.
    pub fn new(mailbox: Arc<Mailbox>, wake: W) -> Self {
        Self {
            mailbox,
            wake,
            delivered: false,
        }
    }

    /// Posts `result`.
    pub fn deliver(mut self, result: Result<String, ClipboardError>) {
        self.post(result);
    }

    fn post(&mut self, result: Result<String, ClipboardError>) {
        self.delivered = true;
        if self.mailbox.post_paste(result) {
            (self.wake)();
        }
    }
}

impl<W: Fn()> Drop for PasteDelivery<W> {
    fn drop(&mut self) {
        if !self.delivered {
            self.post(Err(ClipboardError::Lost));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use tessera_app_core::{App, Config, ConfigPaths, Program, SpawnOptions};

    use super::*;

    #[test]
    fn events_fold_into_one_record() {
        let mailbox = Mailbox::default();
        assert!(mailbox.post(SessionEvent::Output));
        assert!(!mailbox.post(SessionEvent::Title("one".into())));
        assert!(!mailbox.post(SessionEvent::Title("two".into())));
        assert!(!mailbox.post(SessionEvent::Bell));
        assert!(!mailbox.post(SessionEvent::Bell));
        assert!(!mailbox.post(SessionEvent::Exit(ExitStatus::Exited(3))));
        assert_eq!(
            mailbox.take(),
            Pending {
                output: true,
                title: Some("two".into()),
                bells: 2,
                exit: Some(ExitStatus::Exited(3)),
                paste: None,
            }
        );
        assert_eq!(mailbox.take(), Pending::default(), "take empties it");
    }

    #[test]
    fn paste_results_share_the_wake_up() {
        let mailbox = Mailbox::default();
        assert!(mailbox.post_paste(Ok("text".into())));
        assert!(!mailbox.post(SessionEvent::Output));
        let pending = mailbox.take();
        assert_eq!(pending.paste, Some(Ok("text".into())));
        assert!(pending.output);
        assert!(mailbox.post(SessionEvent::Bell));
        assert!(!mailbox.post_paste(Err(ClipboardError::Unavailable)));
        assert_eq!(mailbox.take().paste, Some(Err(ClipboardError::Unavailable)));
    }

    #[test]
    fn a_delivered_paste_posts_its_result_once() {
        let mailbox = Arc::new(Mailbox::default());
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let guard = PasteDelivery::new(Arc::clone(&mailbox), move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        guard.deliver(Ok("text".into()));
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(mailbox.take().paste, Some(Ok("text".into())));
        assert_eq!(
            mailbox.take().paste,
            None,
            "dropping after delivery posts nothing"
        );
    }

    #[test]
    fn a_panicking_paste_thread_still_reports() {
        fn read_that_panics() -> Result<String, ClipboardError> {
            panic!("clipboard read panicked")
        }
        let mailbox = Arc::new(Mailbox::default());
        let wakes = Arc::new(AtomicUsize::new(0));
        let posting = Arc::clone(&mailbox);
        let counter = Arc::clone(&wakes);
        let thread = std::thread::spawn(move || {
            let guard = PasteDelivery::new(posting, move || {
                counter.fetch_add(1, Ordering::Relaxed);
            });
            guard.deliver(read_that_panics());
        });
        assert!(thread.join().is_err());
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(mailbox.take().paste, Some(Err(ClipboardError::Lost)));
    }

    #[test]
    fn a_take_rearms_the_wake_up() {
        let mailbox = Mailbox::default();
        assert!(mailbox.post(SessionEvent::Bell));
        assert!(!mailbox.post(SessionEvent::Bell));
        let _ = mailbox.take();
        assert!(mailbox.post(SessionEvent::Output));
        // A spurious take (the wake-up raced with an earlier take) is
        // harmless and also rearms.
        let _ = mailbox.take();
        let _ = mailbox.take();
        assert!(mailbox.post(SessionEvent::Output));
    }

    #[test]
    fn bells_saturate() {
        let mailbox = Mailbox::default();
        mailbox.state.lock().unwrap().pending.bells = u32::MAX - 1;
        let _ = mailbox.post(SessionEvent::Bell);
        let _ = mailbox.post(SessionEvent::Bell);
        assert_eq!(mailbox.take().bells, u32::MAX);
    }

    /// A million events from four threads with nobody draining request one
    /// wake-up and leave a record of constant size.
    #[test]
    fn a_flood_without_a_drain_requests_one_wake_up() {
        let mailbox = Arc::new(Mailbox::default());
        let wakes = Arc::new(AtomicUsize::new(0));
        let started = Instant::now();
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let mailbox = Arc::clone(&mailbox);
                let wakes = Arc::clone(&wakes);
                std::thread::spawn(move || {
                    for i in 0..250_000_u32 {
                        let event = match i % 4 {
                            0 | 1 => SessionEvent::Output,
                            2 => SessionEvent::Bell,
                            _ => SessionEvent::Title(format!("{t}:{i}")),
                        };
                        if mailbox.post(event) {
                            wakes.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let elapsed = started.elapsed();
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        let pending = mailbox.take();
        assert!(pending.output);
        assert_eq!(pending.bells, 250_000);
        assert!(pending.title.is_some_and(|t| t.len() <= "3:249999".len()));
        // Posting is a lock and a few stores: well under a microsecond each
        // even on a loaded machine (generous bound: 10 µs per event).
        assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    }

    /// The real observer path: a child flooding a session while the "event
    /// loop" (this thread) drains slowly. Wake-ups outstanding never exceed
    /// two, however many output events the reader thread produces.
    #[test]
    fn a_flooding_session_keeps_wake_ups_bounded() {
        let dir = std::env::temp_dir().join(format!("tessera-devwin-{}", std::process::id()));
        let app = App::new(Config::default(), ConfigPaths::new(&dir));
        let mailbox = Arc::new(Mailbox::default());
        let posted = Arc::new(AtomicUsize::new(0));
        let outstanding = Arc::new(AtomicUsize::new(0));
        let max_outstanding = Arc::new(AtomicUsize::new(0));
        {
            let mailbox = Arc::clone(&mailbox);
            let posted = Arc::clone(&posted);
            let outstanding = Arc::clone(&outstanding);
            let max_outstanding = Arc::clone(&max_outstanding);
            app.set_observer(Some(Box::new(move |_id, event| {
                posted.fetch_add(1, Ordering::Relaxed);
                if mailbox.post(event) {
                    let now = outstanding.fetch_add(1, Ordering::SeqCst) + 1;
                    max_outstanding.fetch_max(now, Ordering::SeqCst);
                }
            })));
        }
        let mut options = SpawnOptions::login_shell("");
        options.program = Program::Command {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "yes 0123456789 | head -c 20000000".into()],
        };
        let id = app.spawn_session(options).unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut drains = 0_usize;
        let exit = loop {
            assert!(Instant::now() < deadline, "the flood did not finish");
            // A slow frame: the reader keeps posting meanwhile.
            std::thread::sleep(Duration::from_millis(16));
            if outstanding.load(Ordering::SeqCst) == 0 {
                continue;
            }
            outstanding.fetch_sub(1, Ordering::SeqCst);
            drains += 1;
            if let Some(exit) = mailbox.take().exit {
                break exit;
            }
        };
        app.close_session(id).unwrap();
        assert_eq!(exit, ExitStatus::Exited(0));
        let posted = posted.load(Ordering::Relaxed);
        let max = max_outstanding.load(Ordering::SeqCst);
        eprintln!("{posted} events posted, {drains} drains, at most {max} wake-ups outstanding");
        assert!(
            posted > drains,
            "the flood produced more events than frames"
        );
        assert!(max <= 2, "{max} wake-ups outstanding");
        let _ = std::fs::remove_dir_all(dir);
    }
}
