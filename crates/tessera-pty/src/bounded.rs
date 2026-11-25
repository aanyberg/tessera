//! [`run_bounded`]: a call that may block on the file system, run on a
//! helper thread so the caller waits for it only up to a time bound.
//!
//! A caller such as [`crate::NativePty::spawn`] runs on the app's main
//! thread, where a `read(2)` or `stat(2)` of a slow file (an iCloud file
//! not yet downloaded, a stalled network volume) would block the UI. No
//! system call here can be interrupted once it waits on such a file, so a
//! helper that has not answered in time is left running and counted; the
//! count is capped at [`MAX_STUCK_HELPERS`] (docs/security.md S5), so a
//! volume that never answers cannot make each new call leave another
//! thread behind.

use std::mem;
use std::panic;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Most helper threads [`run_bounded`] runs at once, and so most that can
/// be left running past their caller's timeout (docs/security.md S5). With
/// this many outstanding past their timeout, [`run_bounded`] returns
/// [`Unanswered::Busy`] at once without starting a thread; with fewer
/// stuck but every helper busy, a caller waits (within its own timeout)
/// for one to finish.
pub(crate) const MAX_STUCK_HELPERS: usize = 4;

/// Why [`run_bounded`] has no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unanswered {
    /// The helper did not finish within the timeout. The call may still be
    /// running on its helper, which then counts as stuck.
    TimedOut,
    /// The call was never made: [`MAX_STUCK_HELPERS`] earlier helpers are
    /// stuck (refused at once), or every helper stayed busy with an earlier
    /// call for the whole timeout (#602).
    Busy,
    /// The call was never made: its helper thread could not be started.
    /// The reason is the [`std::io::Error`]'s text, `(os error N)` included
    /// (`EAGAIN` when the process is out of threads), so a spawn error
    /// built from it keeps the `errno` (#602).
    NotStarted(String),
}

/// The process's helpers for [`run_bounded`]. Calls that must not share
/// a cap with these (the pty opens of [`crate::NativePty::spawn`], #568)
/// have a [`Pool`] of their own and use [`run_bounded_in`].
pub(crate) static HELPERS: Pool = Pool::new(MAX_STUCK_HELPERS);

/// Runs `f` on a helper thread named `name` that first runs the crate's
/// thread start hook (see [`crate::set_thread_start_hook`]), and waits up
/// to `timeout` for its result.
///
/// On time the helper has finished and been joined when this returns, so
/// it leaves no thread behind. Past `timeout` the helper is left to finish
/// on its own and its result is dropped there; until it finishes it counts
/// as stuck. At most [`MAX_STUCK_HELPERS`] helpers run at once, stuck or
/// not (see there for what a caller then gets). A panic in `f` is resumed
/// on the caller's thread, as if `f` had run there.
///
/// # Errors
///
/// [`Unanswered`] when there is no result within `timeout` (see there).
pub(crate) fn run_bounded<T: Send + 'static>(
    name: &str,
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Unanswered> {
    run_bounded_in(&HELPERS, name, timeout, f)
}

/// [`run_bounded`] counted against `pool`'s cap instead of the one shared
/// by every other caller, so helpers stuck in one kind of call cannot
/// refuse another kind: with [`MAX_STUCK_HELPERS`] program lookups stuck,
/// a pty open still runs, and the other way round.
///
/// # Errors
///
/// [`Unanswered`] when there is no result within `timeout` (see there),
/// `pool` standing in for the shared helpers.
pub(crate) fn run_bounded_in<T: Send + 'static>(
    pool: &'static Pool,
    name: &str,
    timeout: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Unanswered> {
    pool.run(name, timeout, f)
}

/// Helpers counted against one cap: [`HELPERS`], another kind of call's
/// own (see [`run_bounded_in`]), or a test's.
#[derive(Debug)]
pub(crate) struct Pool {
    /// Most helpers running at once.
    cap: usize,
    counts: Mutex<Counts>,
    /// Notified whenever a count or a [`State`] changes.
    changed: Condvar,
    /// Helper threads started, for the tests' thread count.
    #[cfg(test)]
    started: std::sync::atomic::AtomicUsize,
}

/// The helpers of a [`Pool`] that have not finished.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Counts {
    /// Helpers whose caller still waits for them.
    waited: usize,
    /// Helpers whose caller gave up on them.
    stuck: usize,
}

/// One call's progress. Changed only under its pool's `counts` lock (taken
/// before this one's), so a helper that finishes and a caller that gives
/// up never both win.
#[derive(Debug)]
enum State<T> {
    /// The helper is running and its caller waits.
    Waiting,
    /// The helper finished with this result.
    Done(T),
    /// `f` panicked; the payload is the helper thread's join result.
    Panicked,
    /// The caller gave up; the helper drops its result.
    Abandoned,
}

impl Pool {
    /// A pool that runs at most `cap` helpers at once, stuck or not.
    pub(crate) const fn new(cap: usize) -> Self {
        Self {
            cap,
            counts: Mutex::new(Counts {
                waited: 0,
                stuck: 0,
            }),
            changed: Condvar::new(),
            #[cfg(test)]
            started: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Helpers of this pool still running past their caller's timeout.
    #[cfg(test)]
    pub(crate) fn stuck(&self) -> usize {
        self.lock().stuck
    }

    /// Helpers of this pool still running, stuck or waited for.
    #[cfg(test)]
    pub(crate) fn running(&self) -> usize {
        let counts = self.lock();
        counts.waited + counts.stuck
    }

    /// Helper threads this pool has started.
    #[cfg(test)]
    pub(crate) fn started(&self) -> usize {
        self.started.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn lock(&self) -> MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for a change, at most until `deadline` (forever without one).
    fn wait<'a>(
        &self,
        counts: MutexGuard<'a, Counts>,
        deadline: Option<Instant>,
    ) -> MutexGuard<'a, Counts> {
        match deadline {
            None => self
                .changed
                .wait(counts)
                .unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                self.changed
                    .wait_timeout(counts, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
        }
    }

    /// [`run_bounded`] against this pool's cap.
    fn run<T: Send + 'static>(
        &'static self,
        name: &str,
        timeout: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Unanswered> {
        let deadline = Instant::now().checked_add(timeout);
        let mut counts = self.lock();
        loop {
            if counts.stuck >= self.cap {
                return Err(Unanswered::Busy);
            }
            if counts.waited + counts.stuck < self.cap {
                break;
            }
            if past(deadline) {
                return Err(Unanswered::Busy);
            }
            counts = self.wait(counts, deadline);
        }
        counts.waited += 1;
        drop(counts);

        let state = Arc::new(Mutex::new(State::Waiting));
        let finish = Finish {
            pool: self,
            state: Arc::clone(&state),
            value: None,
        };
        let body = move || {
            let mut finish = finish;
            crate::native::run_thread_start_hook();
            finish.value = Some(f());
        };
        let spawned = match start_fault::take() {
            Some(err) => {
                drop(body);
                Err(err)
            }
            None => thread::Builder::new().name(name.to_owned()).spawn(body),
        };
        let helper = match spawned {
            Ok(helper) => helper,
            Err(err) => {
                // The closure, and the `Finish` in it, was dropped without
                // running, which released the helper's place.
                tracing::debug!(%err, "cannot start a helper thread");
                return Err(Unanswered::NotStarted(err.to_string()));
            }
        };
        #[cfg(test)]
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let mut counts = self.lock();
        loop {
            let mut call = lock(&state);
            match mem::replace(&mut *call, State::Waiting) {
                State::Done(value) => {
                    drop(call);
                    drop(counts);
                    join(helper);
                    return Ok(value);
                }
                State::Panicked => {
                    drop(call);
                    drop(counts);
                    match helper.join() {
                        Err(payload) => panic::resume_unwind(payload),
                        Ok(()) => unreachable!("a helper that panicked returned"),
                    }
                }
                State::Waiting => {}
                State::Abandoned => unreachable!("only this caller abandons its helper"),
            }
            if past(deadline) {
                *call = State::Abandoned;
                counts.waited -= 1;
                counts.stuck += 1;
                // A caller waiting for a free helper returns at once if
                // this filled the cap.
                self.changed.notify_all();
                return Err(Unanswered::TimedOut);
            }
            drop(call);
            counts = self.wait(counts, deadline);
        }
    }
}

/// Hands a helper's result to its caller, or drops it for a caller that
/// gave up, and releases the helper's place in its pool. Runs as the
/// helper's body ends, also when `f` panicked (`value` is then `None`), and
/// when the helper thread never started.
struct Finish<T> {
    pool: &'static Pool,
    state: Arc<Mutex<State<T>>>,
    value: Option<T>,
}

impl<T> Drop for Finish<T> {
    fn drop(&mut self) {
        let mut counts = self.pool.lock();
        let mut call = lock(&self.state);
        match *call {
            State::Waiting => {
                counts.waited -= 1;
                *call = self.value.take().map_or(State::Panicked, State::Done);
            }
            // The late value (`self.value`) drops only after this body,
            // once the stuck count is released, so for that moment the
            // pool may run one helper past its cap and a late pty open may
            // hold its two descriptors uncounted.
            State::Abandoned => counts.stuck -= 1,
            State::Done(_) | State::Panicked => {
                unreachable!("a helper finishes once")
            }
        }
        drop(call);
        drop(counts);
        self.pool.changed.notify_all();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether `deadline` has passed (never, without one).
fn past(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

/// Test-only failure of a helper's thread start (#602): the next
/// [`run_bounded`] on a thread that armed it gets `errno` where
/// [`thread::Builder::spawn`] would have started the helper, as when the
/// process is out of threads. Only the arming thread's next start fails, so
/// other tests running beside it (`cargo test`) are unaffected. Outside
/// tests (and the `test-hooks` feature) nothing is armed.
mod start_fault {
    use std::io;

    #[cfg(any(test, feature = "test-hooks"))]
    thread_local! {
        static NEXT: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
    }

    /// Makes the next helper start on this thread fail with `errno`.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn fail_next(errno: i32) {
        NEXT.with(|next| next.set(Some(errno)));
    }

    /// The armed failure, disarmed.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn take() -> Option<io::Error> {
        NEXT.with(std::cell::Cell::take)
            .map(io::Error::from_raw_os_error)
    }

    /// Nothing is ever armed.
    #[cfg(not(any(test, feature = "test-hooks")))]
    #[inline]
    pub(super) const fn take() -> Option<io::Error> {
        None
    }
}

#[cfg(any(test, feature = "test-hooks"))]
pub(crate) use start_fault::fail_next as fail_next_start;

/// Joins a helper that has delivered its result, so it is gone when the
/// caller returns. It cannot have panicked: the start hook's panics are
/// caught, `f` returned, and `Finish::drop` does not panic.
fn join(helper: JoinHandle<()>) {
    if helper.join().is_err() {
        tracing::error!("a helper thread panicked after delivering its result");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;

    use super::*;

    /// Generous bound for what must happen "at once" or "in time", so a
    /// loaded machine does not fail these tests; the waits they compare
    /// with are far longer.
    const SLACK: Duration = Duration::from_secs(5);

    fn counts(pool: &Pool) -> Counts {
        *pool.lock()
    }

    fn started(pool: &Pool) -> usize {
        pool.started.load(Ordering::SeqCst)
    }

    /// Waits until `pool` has no helper left, failing after [`SLACK`].
    fn wait_idle(pool: &Pool) {
        let deadline = Instant::now() + SLACK;
        while counts(pool) != Counts::default() {
            assert!(
                Instant::now() < deadline,
                "helpers left: {:?}",
                counts(pool)
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Number of threads in this process (`ps -M`: one header line plus
    /// one line per thread); only meaningful when the test runs alone in
    /// its process, as nextest runs every test.
    fn thread_count() -> usize {
        let output = std::process::Command::new("ps")
            .args(["-M", "-p", &std::process::id().to_string()])
            .output()
            .expect("run ps");
        String::from_utf8_lossy(&output.stdout).lines().count() - 1
    }

    #[test]
    fn a_result_in_time_comes_from_a_named_helper_that_is_gone() {
        static POOL: Pool = Pool::new(MAX_STUCK_HELPERS);
        static EXITED: AtomicBool = AtomicBool::new(false);
        /// Marks the helper's exit late in it: a thread-local destructor
        /// runs after the closure, and only a join waits for it.
        struct OnExit;
        impl Drop for OnExit {
            fn drop(&mut self) {
                thread::sleep(Duration::from_millis(50));
                EXITED.store(true, Ordering::SeqCst);
            }
        }
        thread_local! {
            static EXIT: RefCell<Option<OnExit>> = const { RefCell::new(None) };
        }
        let caller = thread::current().id();
        let result = POOL.run("bounded-test", SLACK, move || {
            EXIT.with(|exit| *exit.borrow_mut() = Some(OnExit));
            assert_ne!(thread::current().id(), caller, "ran on the caller");
            thread::current().name().map(str::to_owned)
        });
        assert_eq!(result, Ok(Some("bounded-test".to_owned())));
        assert!(
            EXITED.load(Ordering::SeqCst),
            "returned before the helper thread was gone"
        );
        assert_eq!(counts(&POOL), Counts::default());
        assert_eq!(started(&POOL), 1);
    }

    #[test]
    fn a_slow_call_times_out_and_counts_as_stuck_until_it_finishes() {
        static POOL: Pool = Pool::new(MAX_STUCK_HELPERS);
        let (release, released) = mpsc::channel::<()>();
        let timeout = Duration::from_millis(100);
        let start = Instant::now();
        let result = POOL.run("bounded-test", timeout, move || {
            released.recv_timeout(SLACK * 2).ok();
            7
        });
        let waited = start.elapsed();
        assert_eq!(result, Err(Unanswered::TimedOut));
        assert!(waited >= timeout, "gave up after {waited:?}");
        assert!(waited < SLACK, "held the caller for {waited:?}");
        assert_eq!(
            counts(&POOL),
            Counts {
                waited: 0,
                stuck: 1
            }
        );
        release.send(()).unwrap();
        wait_idle(&POOL);
    }

    /// The cap (S5): with [`MAX_STUCK_HELPERS`] helpers stuck, a further
    /// call returns at once, starts no thread and does not run; once the
    /// stuck ones finish, calls run again.
    #[test]
    fn a_fifth_stuck_helper_starts_no_thread() {
        static POOL: Pool = Pool::new(MAX_STUCK_HELPERS);
        static RAN: AtomicUsize = AtomicUsize::new(0);
        let alone = std::env::var_os("NEXTEST").is_some();
        let (release, released) = mpsc::channel::<()>();
        let released = Arc::new(Mutex::new(released));
        for _ in 0..MAX_STUCK_HELPERS {
            let released = Arc::clone(&released);
            let result = POOL.run("bounded-test", Duration::ZERO, move || {
                RAN.fetch_add(1, Ordering::SeqCst);
                lock(&released).recv_timeout(SLACK * 2).ok();
            });
            assert_eq!(result, Err(Unanswered::TimedOut));
        }
        assert_eq!(
            counts(&POOL),
            Counts {
                waited: 0,
                stuck: MAX_STUCK_HELPERS
            }
        );
        assert_eq!(started(&POOL), MAX_STUCK_HELPERS);
        let threads = thread_count();

        let start = Instant::now();
        let fifth = POOL.run("bounded-test", SLACK, || {
            RAN.fetch_add(100, Ordering::SeqCst);
        });
        let waited = start.elapsed();
        assert_eq!(fifth, Err(Unanswered::Busy));
        assert!(waited < SLACK, "the fifth call waited {waited:?}");
        assert_eq!(started(&POOL), MAX_STUCK_HELPERS, "a fifth helper started");
        if alone {
            assert_eq!(thread_count(), threads, "a fifth helper thread exists");
        }

        for _ in 0..MAX_STUCK_HELPERS {
            release.send(()).unwrap();
        }
        wait_idle(&POOL);
        assert_eq!(POOL.run("bounded-test", SLACK, || 3), Ok(3));
        assert!(
            RAN.load(Ordering::SeqCst) < 100,
            "the refused call ran: {}",
            RAN.load(Ordering::SeqCst)
        );
    }

    /// A flood of callers (S5): never more than the cap of helpers at once,
    /// and every caller gets its result, waiting for a free helper.
    #[test]
    fn a_flood_of_callers_never_runs_more_helpers_than_the_cap() {
        static POOL: Pool = Pool::new(MAX_STUCK_HELPERS);
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        static MOST: AtomicUsize = AtomicUsize::new(0);
        const CALLERS: usize = 32;
        const CALLS: usize = 8;
        let callers: Vec<_> = (0..CALLERS)
            .map(|caller| {
                thread::spawn(move || {
                    (0..CALLS)
                        .map(|call| {
                            POOL.run("bounded-test", SLACK * 2, move || {
                                let now = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
                                MOST.fetch_max(now, Ordering::SeqCst);
                                thread::sleep(Duration::from_millis(2));
                                RUNNING.fetch_sub(1, Ordering::SeqCst);
                                caller * CALLS + call
                            })
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut results: Vec<usize> = callers
            .into_iter()
            .flat_map(|caller| caller.join().expect("caller panicked"))
            .map(|result| result.expect("a call timed out"))
            .collect();
        results.sort_unstable();
        assert_eq!(results, (0..CALLERS * CALLS).collect::<Vec<_>>());
        let most = MOST.load(Ordering::SeqCst);
        assert!(
            (1..=MAX_STUCK_HELPERS).contains(&most),
            "{most} helpers ran at once"
        );
        assert_eq!(started(&POOL), CALLERS * CALLS);
        assert_eq!(counts(&POOL), Counts::default());
    }

    /// With every helper busy but none stuck, a caller waits only up to its
    /// own timeout for a free one.
    #[test]
    fn a_caller_waits_for_a_free_helper_only_within_its_timeout() {
        static POOL: Pool = Pool::new(1);
        let (release, released) = mpsc::channel::<()>();
        let (entered, has_entered) = mpsc::channel::<()>();
        let busy = thread::spawn(move || {
            POOL.run("bounded-test", SLACK * 2, move || {
                entered.send(()).unwrap();
                released.recv_timeout(SLACK * 2).ok();
                1
            })
        });
        has_entered
            .recv_timeout(SLACK)
            .expect("busy helper started");
        let timeout = Duration::from_millis(100);
        let start = Instant::now();
        assert_eq!(
            POOL.run("bounded-test", timeout, || 2),
            Err(Unanswered::Busy)
        );
        let waited = start.elapsed();
        assert!(waited >= timeout, "gave up after {waited:?}");
        assert!(waited < SLACK, "waited {waited:?}");
        assert_eq!(started(&POOL), 1, "a second helper started");
        release.send(()).unwrap();
        assert_eq!(busy.join().unwrap(), Ok(1));
        assert_eq!(POOL.run("bounded-test", SLACK, || 3), Ok(3));
        assert_eq!(counts(&POOL), Counts::default());
    }

    /// A caller waiting for a free helper returns at once when the busy
    /// one times out and so fills the cap with stuck helpers. The busy
    /// call's 1 s leaves a loaded machine time to have the caller waiting
    /// before then, so the wake-up on the cap filling is what returns it.
    #[test]
    fn a_waiting_caller_returns_once_the_cap_fills_with_stuck_helpers() {
        static POOL: Pool = Pool::new(1);
        let (release, released) = mpsc::channel::<()>();
        let (entered, has_entered) = mpsc::channel::<()>();
        let busy = thread::spawn(move || {
            POOL.run("bounded-test", Duration::from_secs(1), move || {
                entered.send(()).unwrap();
                released.recv_timeout(SLACK * 4).ok();
            })
        });
        has_entered
            .recv_timeout(SLACK)
            .expect("busy helper started");
        let start = Instant::now();
        assert_eq!(
            POOL.run("bounded-test", SLACK * 2, || 2),
            Err(Unanswered::Busy)
        );
        let waited = start.elapsed();
        assert!(waited < SLACK, "waited {waited:?} for a full cap");
        assert_eq!(busy.join().unwrap(), Err(Unanswered::TimedOut));
        assert_eq!(started(&POOL), 1, "a second helper started");
        release.send(()).unwrap();
        wait_idle(&POOL);
    }

    /// #554 M3: every helper first runs the crate's thread start hook
    /// (ADR 0018), on the process's own helpers too. The hook is
    /// process-wide: no other test in this crate's unit tests installs
    /// one, and it only records the threads it ran on, so tests beside it
    /// (`cargo test`) are unaffected.
    #[test]
    fn a_helper_runs_the_thread_start_hook_first() {
        static HOOKED: Mutex<Vec<thread::ThreadId>> = Mutex::new(Vec::new());
        fn record() {
            lock(&HOOKED).push(thread::current().id());
        }
        assert!(
            crate::set_thread_start_hook(record),
            "another hook was installed"
        );
        for name in ["pty-spawn-check", "pty-spawn-program"] {
            let helper = run_bounded(name, SLACK, || {
                let me = thread::current().id();
                (me, lock(&HOOKED).contains(&me))
            });
            let (helper, hooked_first) = helper.expect("the helper answered");
            assert_ne!(helper, thread::current().id(), "{name} ran on the caller");
            assert!(hooked_first, "{name} ran before the hook");
        }
    }

    /// #602: a helper thread that cannot start (the process out of
    /// threads) gives the start's error with its `errno`, not a timeout;
    /// `f` never runs, no thread is counted, the helper's place is free
    /// again, and only the arming thread's next start fails.
    #[test]
    fn a_helper_that_cannot_start_reports_its_errno_and_frees_its_place() {
        static POOL: Pool = Pool::new(1);
        static RAN: AtomicBool = AtomicBool::new(false);
        let eagain = nix::errno::Errno::EAGAIN as i32;
        fail_next_start(eagain);
        let result = POOL.run("bounded-test", SLACK, || RAN.store(true, Ordering::SeqCst));
        assert_eq!(
            result,
            Err(Unanswered::NotStarted(
                std::io::Error::from_raw_os_error(eagain).to_string()
            ))
        );
        assert!(!RAN.load(Ordering::SeqCst), "the call ran");
        assert_eq!(started(&POOL), 0);
        assert_eq!(counts(&POOL), Counts::default());
        assert_eq!(POOL.run("bounded-test", SLACK, || 8), Ok(8));
        fail_next_start(eagain);
        let other = thread::spawn(|| POOL.run("bounded-test", SLACK, || 9));
        assert_eq!(
            other.join().unwrap(),
            Ok(9),
            "another thread's start failed"
        );
        assert!(
            matches!(
                POOL.run("bounded-test", SLACK, || 10),
                Err(Unanswered::NotStarted(_))
            ),
            "this thread's start did not fail"
        );
    }

    #[test]
    fn a_panic_in_the_call_is_resumed_on_the_caller() {
        static POOL: Pool = Pool::new(MAX_STUCK_HELPERS);
        let caught = panic::catch_unwind(|| {
            POOL.run("bounded-test", SLACK, || -> u8 {
                panic::panic_any("deliberate helper panic")
            })
        });
        let payload = caught.expect_err("the panic reached the caller");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&"deliberate helper panic")
        );
        assert_eq!(counts(&POOL), Counts::default());
        assert_eq!(POOL.run("bounded-test", SLACK, || 4), Ok(4));
    }

    #[test]
    fn a_stuck_helper_that_panics_still_releases_its_place() {
        static POOL: Pool = Pool::new(1);
        let (release, released) = mpsc::channel::<()>();
        let result = POOL.run("bounded-test", Duration::ZERO, move || -> u8 {
            released.recv_timeout(SLACK * 2).ok();
            panic::panic_any("deliberate stuck helper panic")
        });
        assert_eq!(result, Err(Unanswered::TimedOut));
        assert_eq!(POOL.run("bounded-test", SLACK, || 5), Err(Unanswered::Busy));
        release.send(()).unwrap();
        wait_idle(&POOL);
        assert_eq!(POOL.run("bounded-test", SLACK, || 6), Ok(6));
    }
}
