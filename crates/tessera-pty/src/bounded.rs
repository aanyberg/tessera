//! [`run_bounded`]: a call that may block on the file system, run on a
//! helper thread so the caller waits for it only up to a time bound.
//!
//! A caller such as [`crate::NativePty::spawn`] runs on the app's main
//! thread, where a `read(2)` or `stat(2)` of a slow file (an iCloud file
//! not yet downloaded, a stalled network volume) would block the UI. No
//! system call here can be interrupted once it waits on such a file, so a
//! helper that has not answered in time is left running and counted; the
//! helpers running at once, stuck or not, are capped at
//! [`MAX_STUCK_HELPERS`] (docs/security.md S5), so a volume that never
//! answers cannot make each new call leave another thread behind. A helper
//! left running counts as stuck once it has run its call's whole timeout
//! without an answer, not before (#661 F1): one its caller gave up on
//! after waiting for a free helper first is late, not stuck, until then. One more helper, [`RESERVED_HELPERS`], is kept for the
//! built-in start a profile falls back to ([`run_bounded_reserved`], #661),
//! so at most `MAX_STUCK_HELPERS + RESERVED_HELPERS` are ever stuck here.

use std::mem;
use std::panic;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Most helper threads [`run_bounded`] runs at once, and so most that can
/// be left running past their caller's timeout (docs/security.md S5). With
/// this many stuck (each run its call's whole timeout without an answer),
/// [`run_bounded`] returns [`Unanswered::Busy`] at once without starting a
/// thread; with fewer stuck but every helper taken (busy, or late: left by
/// a caller whose time ran out while the helper had not yet run its own),
/// a caller waits (within its own timeout) for one to finish.
pub(crate) const MAX_STUCK_HELPERS: usize = 4;

/// Helpers kept apart from [`HELPERS`] for [`run_bounded_reserved`], the
/// built-in start's program lookup (#661): with every shared helper taken
/// (stuck in lookups on a stalled volume, say) that lookup still runs, so
/// the default shell a profile falls back to can start. With this many
/// reserved helpers stuck too, a reserved call is refused like any other,
/// so the helpers stuck at once stay at most
/// `MAX_STUCK_HELPERS + RESERVED_HELPERS` (docs/security.md S5).
pub(crate) const RESERVED_HELPERS: usize = 1;

/// Why [`run_bounded`] has no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unanswered {
    /// The helper did not finish within the timeout. The call may still be
    /// running on its helper, which then keeps its place and counts as
    /// stuck once it has run the whole timeout itself.
    TimedOut,
    /// The call was never made: [`MAX_STUCK_HELPERS`] earlier helpers are
    /// stuck (refused at once), or every helper stayed busy with an earlier
    /// call for the whole timeout (#602). `marks` has bit `n` set for each
    /// [`Mark`] reading `n` (at most 7) among the helpers it found taken,
    /// so the caller can tell what they were waiting on (#661 F2); 0 when
    /// none of them was marked.
    Busy {
        /// The taken helpers' marks, one bit per value.
        marks: u8,
    },
    /// The call was never made: its helper thread could not be started.
    /// The reason is the [`std::io::Error`]'s text, `(os error N)` included
    /// (`EAGAIN` when the process is out of threads), so a spawn error
    /// built from it keeps the `errno` (#602).
    NotStarted(String),
}

/// A value a helper's call updates as it goes (a program lookup: the step
/// it has reached), which a caller refused for want of a helper reads in
/// [`Unanswered::Busy`] (#661 F2).
pub(crate) type Mark = Arc<AtomicU8>;

/// The process's helpers for [`run_bounded`]. Calls that must not share
/// a cap with these (the pty opens of [`crate::NativePty::spawn`], #568)
/// have a [`Pool`] of their own and use [`run_bounded_in`].
pub(crate) static HELPERS: Pool = Pool::new(MAX_STUCK_HELPERS);

/// The helpers [`run_bounded_reserved`] falls back to when no shared one
/// ([`HELPERS`]) is free (#661).
pub(crate) static RESERVE: Pool = Pool::reserve_for(RESERVED_HELPERS, &HELPERS);

/// Runs `f` on a helper thread named `name` that first runs the crate's
/// thread start hook (see [`crate::set_thread_start_hook`]), and waits up
/// to `timeout` for its result.
///
/// On time the helper has finished and been joined when this returns, so
/// it leaves no thread behind. Past `timeout` the helper is left to finish
/// on its own and its result is dropped there; until it finishes it takes
/// its place, and once it has run `timeout` itself (the caller's timeout
/// less any wait for a free helper, plus that wait) it counts as stuck. At
/// most [`MAX_STUCK_HELPERS`] helpers run at once, stuck or
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

/// [`run_bounded`] with the helper marked by `mark` while it runs, for
/// [`Unanswered::Busy`] of callers refused for want of it.
///
/// # Errors
///
/// As for [`run_bounded`].
pub(crate) fn run_bounded_marked<T: Send + 'static>(
    name: &str,
    timeout: Duration,
    mark: Mark,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Unanswered> {
    HELPERS.run_marked(name, timeout, Some(mark), f)
}

/// [`run_bounded`] for the built-in start's program lookup (#661): run on a
/// shared helper ([`HELPERS`]) if one is free, else on a reserved one
/// ([`RESERVE`]) if that is free, else on whichever of the two frees first
/// within `timeout`. So a lookup that the shared cap would refuse still
/// runs while a reserved helper is free, one queued behind a stuck reserved
/// helper still gets a shared one as soon as that frees, and each call
/// starts at most one helper. The helper is marked by `mark` as for
/// [`run_bounded_marked`].
///
/// # Errors
///
/// As for [`run_bounded`], both pools counted: [`Unanswered::Busy`] at once
/// only when [`MAX_STUCK_HELPERS`] shared and [`RESERVED_HELPERS`] reserved
/// helpers are all stuck, and after `timeout` when neither pool freed a
/// helper in time.
pub(crate) fn run_bounded_reserved<T: Send + 'static>(
    name: &str,
    timeout: Duration,
    mark: Mark,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Unanswered> {
    run_bounded_reserved_in(&HELPERS, &RESERVE, name, timeout, Some(mark), f)
}

/// [`run_bounded_reserved`] with `shared` and `reserve` standing in for
/// [`HELPERS`] and [`RESERVE`]; `reserve` must be
/// [`Pool::reserve_for`] `shared`, so that its changes wake a caller
/// waiting here.
///
/// # Errors
///
/// As for [`run_bounded_reserved`].
pub(crate) fn run_bounded_reserved_in<T: Send + 'static>(
    shared: &'static Pool,
    reserve: &'static Pool,
    name: &str,
    timeout: Duration,
    mark: Option<Mark>,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Unanswered> {
    debug_assert!(
        reserve
            .wakes
            .is_some_and(|woken| std::ptr::eq(woken, shared)),
        "a reserve pool that does not wake its shared pool's waiters"
    );
    let deadline = Instant::now().checked_add(timeout);
    let (pool, claim) = claim_either(shared, reserve, deadline, mark)?;
    pool.run_claimed(name, deadline, claim, f)
}

/// Takes a place for one helper on `shared` if one is free there, else on
/// `reserve` if one is free there, else waits until `deadline` for
/// whichever frees first (#661 B2). Refused at once only when both pools
/// have their cap of helpers stuck, so the two caps together bound the
/// helpers stuck; a late helper (see [`MAX_STUCK_HELPERS`]) is waited for
/// like a busy one, the wait re-checked when it would turn stuck.
///
/// Waits on `shared`'s condition variable, holding its lock while it
/// checks both pools: `reserve`'s changes notify that variable under that
/// lock ([`Pool::wake_linked`]), so none is missed between the check and
/// the wait. Takes `reserve`'s lock only inside `shared`'s, never the
/// other way round.
fn claim_either(
    shared: &'static Pool,
    reserve: &'static Pool,
    deadline: Option<Instant>,
    mark: Option<Mark>,
) -> Result<(&'static Pool, Claim), Unanswered> {
    let mut slots = shared.lock();
    let mut queued = Queued::default();
    loop {
        if slots.taken() < shared.cap {
            return Ok((shared, slots.take(mark, queued)));
        }
        let now = Instant::now();
        let next = {
            let mut spare = reserve.lock();
            if spare.taken() < reserve.cap {
                return Ok((reserve, spare.take(mark, queued)));
            }
            let marks = slots.marks() | spare.marks();
            if slots.stuck(now) >= shared.cap && spare.stuck(now) >= reserve.cap {
                return Err(Unanswered::Busy { marks });
            }
            if past(deadline) {
                return Err(Unanswered::Busy { marks });
            }
            earliest(slots.next_stuck(now), spare.next_stuck(now))
        };
        queued.start(now);
        slots = shared.wait(slots, earliest(deadline, next));
    }
}

/// The earlier of two instants, either of which may be absent (never).
fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
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

/// Helpers counted against one cap: [`HELPERS`], [`RESERVE`], another
/// kind of call's own (see [`run_bounded_in`]), or a test's.
#[derive(Debug)]
pub(crate) struct Pool {
    /// Most helpers running at once.
    cap: usize,
    slots: Mutex<Slots>,
    /// Notified whenever a count or a [`State`] changes.
    changed: Condvar,
    /// The shared pool this one is the reserve of ([`Pool::reserve_for`]),
    /// whose waiters in [`claim_either`] are notified of this one's
    /// changes too.
    wakes: Option<&'static Pool>,
    /// Helper threads started, for the tests' thread count.
    #[cfg(test)]
    started: std::sync::atomic::AtomicUsize,
}

/// The helpers of a [`Pool`] that have not finished: one [`Slot`] each,
/// never more than the pool's cap (S5).
#[derive(Debug)]
struct Slots {
    running: Vec<Slot>,
    /// The id the next slot gets.
    next: u64,
}

/// One running helper's place in its [`Pool`].
#[derive(Debug)]
struct Slot {
    id: u64,
    /// `None` while its caller waits for it. Once the caller gave up, when
    /// it counts as stuck: the caller's deadline plus the time the caller
    /// waited for a free helper first, so it has run its call's whole
    /// timeout itself. Until then it is late.
    stuck_from: Option<Instant>,
    /// What its call has reached ([`run_bounded_marked`]).
    mark: Option<Mark>,
    /// How long its caller waited for a free helper.
    queued: Duration,
}

/// A place [`Slots::take`] gave a caller.
#[derive(Debug, Clone, Copy)]
struct Claim {
    id: u64,
    queued: Duration,
}

/// When a caller first had to wait for a free helper.
#[derive(Debug, Default, Clone, Copy)]
struct Queued(Option<Instant>);

impl Queued {
    /// Notes that the caller waits, from `now` if it did not already.
    fn start(&mut self, now: Instant) {
        self.0.get_or_insert(now);
    }

    /// How long the caller has waited.
    fn waited(self) -> Duration {
        self.0.map_or(Duration::ZERO, |from| from.elapsed())
    }
}

impl Slots {
    const fn new() -> Self {
        Self {
            running: Vec::new(),
            next: 0,
        }
    }

    /// Helpers running, stuck or not: at most the pool's cap.
    fn taken(&self) -> usize {
        self.running.len()
    }

    /// Helpers that have run their call's whole timeout without an answer
    /// by `now`.
    fn stuck(&self, now: Instant) -> usize {
        self.running
            .iter()
            .filter(|slot| slot.stuck_from.is_some_and(|from| from <= now))
            .count()
    }

    /// The earliest moment after `now` a late helper turns stuck.
    fn next_stuck(&self, now: Instant) -> Option<Instant> {
        self.running
            .iter()
            .filter_map(|slot| slot.stuck_from)
            .filter(|&from| from > now)
            .min()
    }

    /// The running helpers' marks, one bit per value (see
    /// [`Unanswered::Busy`]).
    fn marks(&self) -> u8 {
        self.running
            .iter()
            .filter_map(|slot| slot.mark.as_ref())
            .fold(0, |marks, mark| {
                marks | 1 << mark.load(Ordering::SeqCst).min(7)
            })
    }

    /// Takes a place, waited for, for a caller that waited as `queued`
    /// says. The caller has checked that the cap leaves one.
    fn take(&mut self, mark: Option<Mark>, queued: Queued) -> Claim {
        let claim = Claim {
            id: self.next,
            queued: queued.waited(),
        };
        self.next = self.next.wrapping_add(1);
        self.running.push(Slot {
            id: claim.id,
            stuck_from: None,
            mark,
            queued: claim.queued,
        });
        claim
    }

    /// Marks `id`'s caller as gone, its deadline `deadline`.
    fn abandon(&mut self, id: u64, deadline: Instant) {
        if let Some(slot) = self.running.iter_mut().find(|slot| slot.id == id) {
            slot.stuck_from = Some(deadline.checked_add(slot.queued).unwrap_or(deadline));
        }
    }

    /// Frees `id`'s place.
    fn release(&mut self, id: u64) {
        self.running.retain(|slot| slot.id != id);
    }
}

/// The helpers of a [`Pool`], counted for tests.
#[cfg(test)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Counts {
    /// Helpers whose caller still waits for them.
    waited: usize,
    /// Helpers whose caller gave up on them before they ran their whole
    /// timeout, and that have not yet.
    late: usize,
    /// Helpers that ran their whole timeout without an answer.
    stuck: usize,
}

/// One call's progress. Changed only under its pool's `slots` lock (taken
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
            slots: Mutex::new(Slots::new()),
            changed: Condvar::new(),
            wakes: None,
            #[cfg(test)]
            started: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// A pool of `cap` helpers kept for [`run_bounded_reserved_in`] calls
    /// on `shared`: [`Pool::new`] whose changes also wake the callers that
    /// wait for a helper of either pool.
    pub(crate) const fn reserve_for(cap: usize, shared: &'static Self) -> Self {
        let mut pool = Self::new(cap);
        pool.wakes = Some(shared);
        pool
    }

    /// Wakes the callers waiting in [`claim_either`] on the pool this one
    /// is the reserve of, after a change to this one. Takes that pool's
    /// lock to notify, so it must not be called with this one's held.
    fn wake_linked(&self) {
        if let Some(shared) = self.wakes {
            let _slots = shared.lock();
            shared.changed.notify_all();
        }
    }

    /// Helpers of this pool that ran their whole timeout without an
    /// answer and still run.
    #[cfg(test)]
    pub(crate) fn stuck(&self) -> usize {
        self.lock().stuck(Instant::now())
    }

    /// Helpers of this pool still running, stuck, late or waited for.
    #[cfg(test)]
    pub(crate) fn running(&self) -> usize {
        self.lock().taken()
    }

    /// This pool's helpers by state.
    #[cfg(test)]
    fn counts(&self) -> Counts {
        let slots = self.lock();
        let waited = slots
            .running
            .iter()
            .filter(|slot| slot.stuck_from.is_none())
            .count();
        let stuck = slots.stuck(Instant::now());
        Counts {
            waited,
            late: slots.taken() - waited - stuck,
            stuck,
        }
    }

    /// Helper threads this pool has started.
    #[cfg(test)]
    pub(crate) fn started(&self) -> usize {
        self.started.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn lock(&self) -> MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits for a change, at most until `deadline` (forever without one).
    fn wait<'a>(
        &self,
        slots: MutexGuard<'a, Slots>,
        deadline: Option<Instant>,
    ) -> MutexGuard<'a, Slots> {
        match deadline {
            None => self
                .changed
                .wait(slots)
                .unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                self.changed
                    .wait_timeout(slots, left)
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
        self.run_marked(name, timeout, None, f)
    }

    /// [`Pool::run`] with the helper marked by `mark`.
    fn run_marked<T: Send + 'static>(
        &'static self,
        name: &str,
        timeout: Duration,
        mark: Option<Mark>,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Unanswered> {
        let deadline = Instant::now().checked_add(timeout);
        let claim = self.claim(deadline, mark)?;
        self.run_claimed(name, deadline, claim, f)
    }

    /// Takes a place for one helper, counted as waited for: refused at once
    /// with the pool's cap of helpers stuck, else once a helper is free,
    /// waiting for one until `deadline` (and re-checking when a late one
    /// would turn stuck).
    fn claim(&self, deadline: Option<Instant>, mark: Option<Mark>) -> Result<Claim, Unanswered> {
        let mut slots = self.lock();
        let mut queued = Queued::default();
        loop {
            let now = Instant::now();
            if slots.stuck(now) >= self.cap || (slots.taken() >= self.cap && past(deadline)) {
                return Err(Unanswered::Busy {
                    marks: slots.marks(),
                });
            }
            if slots.taken() < self.cap {
                return Ok(slots.take(mark, queued));
            }
            queued.start(now);
            let next = slots.next_stuck(now);
            slots = self.wait(slots, earliest(deadline, next));
        }
    }

    /// Runs `f` on a helper in the place `claim` ([`Pool::claim`]) took,
    /// waiting for it until `deadline` (see [`run_bounded`]).
    fn run_claimed<T: Send + 'static>(
        &'static self,
        name: &str,
        deadline: Option<Instant>,
        claim: Claim,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Unanswered> {
        let state = Arc::new(Mutex::new(State::Waiting));
        let finish = Finish {
            pool: self,
            id: claim.id,
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

        let mut slots = self.lock();
        loop {
            let mut call = lock(&state);
            match mem::replace(&mut *call, State::Waiting) {
                State::Done(value) => {
                    drop(call);
                    drop(slots);
                    join(helper);
                    return Ok(value);
                }
                State::Panicked => {
                    drop(call);
                    drop(slots);
                    match helper.join() {
                        Err(payload) => panic::resume_unwind(payload),
                        Ok(()) => unreachable!("a helper that panicked returned"),
                    }
                }
                State::Waiting => {}
                State::Abandoned => unreachable!("only this caller abandons its helper"),
            }
            if let Some(deadline) = deadline.filter(|_| past(deadline)) {
                *call = State::Abandoned;
                slots.abandon(claim.id, deadline);
                // A caller waiting for a free helper returns at once if
                // this filled the cap with stuck ones, and otherwise waits
                // until a late one would turn stuck.
                self.changed.notify_all();
                drop(call);
                drop(slots);
                self.wake_linked();
                return Err(Unanswered::TimedOut);
            }
            drop(call);
            slots = self.wait(slots, deadline);
        }
    }
}

/// Hands a helper's result to its caller, or drops it for a caller that
/// gave up, and releases the helper's place in its pool. Runs as the
/// helper's body ends, also when `f` panicked (`value` is then `None`), and
/// when the helper thread never started.
struct Finish<T> {
    pool: &'static Pool,
    /// The helper's [`Slot`].
    id: u64,
    state: Arc<Mutex<State<T>>>,
    value: Option<T>,
}

impl<T> Drop for Finish<T> {
    fn drop(&mut self) {
        let mut slots = self.pool.lock();
        let mut call = lock(&self.state);
        slots.release(self.id);
        match *call {
            State::Waiting => {
                *call = self.value.take().map_or(State::Panicked, State::Done);
            }
            // The late value (`self.value`) drops only after this body,
            // once its place is released, so for that moment the pool may
            // run one helper past its cap and a late pty open may hold its
            // two descriptors uncounted.
            State::Abandoned => {}
            State::Done(_) | State::Panicked => {
                unreachable!("a helper finishes once")
            }
        }
        drop(call);
        drop(slots);
        self.pool.changed.notify_all();
        self.pool.wake_linked();
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
        pool.counts()
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
                late: 0,
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
                late: 0,
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
        assert_eq!(fifth, Err(Unanswered::Busy { marks: 0 }));
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
            Err(Unanswered::Busy { marks: 0 })
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
            Err(Unanswered::Busy { marks: 0 })
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
        assert_eq!(
            POOL.run("bounded-test", SLACK, || 5),
            Err(Unanswered::Busy { marks: 0 })
        );
        release.send(()).unwrap();
        wait_idle(&POOL);
        assert_eq!(POOL.run("bounded-test", SLACK, || 6), Ok(6));
    }

    /// Starts a call on `pool` (cap 1) that holds its only helper until the
    /// returned sender is dropped, and waits until it runs.
    fn occupy(
        pool: &'static Pool,
    ) -> (mpsc::Sender<()>, thread::JoinHandle<Result<(), Unanswered>>) {
        let (release, released) = mpsc::channel::<()>();
        let busy = thread::spawn(move || {
            pool.run("bounded-test", SLACK * 4, move || {
                released.recv_timeout(SLACK * 2).ok();
            })
        });
        let deadline = Instant::now() + SLACK;
        while counts(pool).waited == 0 {
            assert!(Instant::now() < deadline, "the busy call did not start");
            thread::sleep(Duration::from_millis(1));
        }
        (release, busy)
    }

    /// Starts a call on `pool` with `timeout` that first waits `queued` for
    /// the helper [`occupy`] holds, then runs until the returned sender is
    /// dropped: its caller gives up with its helper having run only
    /// `timeout - queued`. Returns once the call's helper runs.
    fn queue_then_run(
        pool: &'static Pool,
        timeout: Duration,
        queued: Duration,
    ) -> (mpsc::Sender<()>, thread::JoinHandle<Result<u8, Unanswered>>) {
        let (busy_release, busy) = occupy(pool);
        let (release, released) = mpsc::channel::<()>();
        let (running, runs) = mpsc::channel::<()>();
        let late = thread::spawn(move || {
            pool.run("bounded-test", timeout, move || {
                running.send(()).ok();
                released.recv_timeout(SLACK * 2).ok();
                9
            })
        });
        thread::sleep(queued);
        drop(busy_release);
        assert_eq!(busy.join().unwrap(), Ok(()));
        runs.recv_timeout(SLACK)
            .expect("the queued call did not run");
        (release, late)
    }

    /// #661 F1: a helper whose caller gave up after waiting for a free
    /// helper first has not run its own timeout yet, so it is late, not
    /// stuck: a caller then waiting for a helper is not refused at once as
    /// if the cap were full of stuck ones, and gets the helper once that
    /// late call returns.
    #[test]
    fn a_helper_left_after_its_caller_queued_is_waited_for_not_counted_stuck() {
        static POOL: Pool = Pool::new(1);
        let timeout = Duration::from_secs(2);
        let (release, late) = queue_then_run(&POOL, timeout, Duration::from_secs(1));
        let waiter = thread::spawn(|| POOL.run("bounded-test", SLACK * 4, || 10_u8));
        assert_eq!(late.join().unwrap(), Err(Unanswered::TimedOut));
        assert_eq!(POOL.stuck(), 0, "a late helper counts as stuck");
        assert_eq!(POOL.running(), 1);
        thread::sleep(Duration::from_millis(100));
        assert!(
            !waiter.is_finished(),
            "refused while the only helper was late, not stuck: {:?}",
            waiter.join()
        );
        drop(release);
        assert_eq!(waiter.join().unwrap(), Ok(10));
        wait_idle(&POOL);
    }

    /// #661 F1, S5: a late helper turns stuck once it has run its call's
    /// whole timeout without an answer, and a caller waiting for a helper
    /// is then refused at once (not at its own deadline), so the cap of
    /// stuck helpers still holds.
    #[test]
    fn a_late_helper_turns_stuck_once_it_has_run_its_own_timeout() {
        static POOL: Pool = Pool::new(1);
        let timeout = Duration::from_millis(600);
        let queued = Duration::from_millis(300);
        let (release, late) = queue_then_run(&POOL, timeout, queued);
        assert_eq!(late.join().unwrap(), Err(Unanswered::TimedOut));
        let start = Instant::now();
        let result = POOL.run("bounded-test", SLACK * 4, || 11_u8);
        let waited = start.elapsed();
        assert_eq!(result, Err(Unanswered::Busy { marks: 0 }));
        assert!(waited < SLACK, "refused only after {waited:?}");
        assert_eq!(
            counts(&POOL),
            Counts {
                waited: 0,
                late: 0,
                stuck: 1
            }
        );
        assert_eq!(started(&POOL), 2, "a second helper started");
        drop(release);
        wait_idle(&POOL);
    }

    /// #661 F1: a reserved call waiting while both pools are full is
    /// refused as soon as the late shared helper turns stuck, not at its
    /// own deadline.
    #[test]
    fn a_reserved_call_is_refused_once_a_late_shared_helper_turns_stuck() {
        static SHARED: Pool = Pool::new(1);
        static RESERVED: Pool = Pool::reserve_for(RESERVED_HELPERS, &SHARED);
        let stuck = stick(&RESERVED, RESERVED_HELPERS);
        let timeout = Duration::from_millis(600);
        let (release, late) = queue_then_run(&SHARED, timeout, Duration::from_millis(300));
        assert_eq!(late.join().unwrap(), Err(Unanswered::TimedOut));
        let start = Instant::now();
        let result =
            run_bounded_reserved_in(&SHARED, &RESERVED, "bounded-test", SLACK * 4, None, || 3);
        let waited = start.elapsed();
        assert_eq!(result, Err(Unanswered::Busy { marks: 0 }));
        assert!(waited < SLACK, "refused only after {waited:?}");
        drop((release, stuck));
        wait_idle(&SHARED);
        wait_idle(&RESERVED);
    }

    /// #661 F2: a call refused for want of a helper reads the marks of the
    /// helpers it found taken, in both pools, one bit per value.
    #[test]
    fn a_refused_call_reads_the_marks_of_the_helpers_it_found_taken() {
        static SHARED: Pool = Pool::new(1);
        static RESERVED: Pool = Pool::reserve_for(RESERVED_HELPERS, &SHARED);
        let (release, released) = mpsc::channel::<()>();
        let released = Arc::new(Mutex::new(released));
        for (pool, mark) in [(&SHARED, 2_u8), (&RESERVED, 5)] {
            let released = Arc::clone(&released);
            let mark = Arc::new(AtomicU8::new(mark));
            let result = pool.run_marked("bounded-test", Duration::ZERO, Some(mark), move || {
                lock(&released).recv_timeout(SLACK * 2).ok();
            });
            assert_eq!(result, Err(Unanswered::TimedOut));
        }
        assert_eq!(
            SHARED.run("bounded-test", SLACK, || 1),
            Err(Unanswered::Busy { marks: 1 << 2 })
        );
        let result = run_bounded_reserved_in(&SHARED, &RESERVED, "bounded-test", SLACK, None, || 2);
        assert_eq!(
            result,
            Err(Unanswered::Busy {
                marks: 1 << 2 | 1 << 5
            })
        );
        drop(release);
        wait_idle(&SHARED);
        wait_idle(&RESERVED);
    }

    /// Leaves `n` calls on `pool` stuck past their (zero) timeout until
    /// the returned sender is dropped.
    fn stick(pool: &'static Pool, n: usize) -> mpsc::Sender<()> {
        let (release, released) = mpsc::channel::<()>();
        let released = Arc::new(Mutex::new(released));
        for _ in 0..n {
            let released = Arc::clone(&released);
            let result = pool.run("bounded-test", Duration::ZERO, move || {
                lock(&released).recv_timeout(SLACK * 2).ok();
            });
            assert_eq!(result, Err(Unanswered::TimedOut));
        }
        release
    }

    /// #661: a reserved call takes a shared helper while one is free, and
    /// leaves the reserved one alone.
    #[test]
    fn a_reserved_call_runs_on_a_free_shared_helper() {
        static SHARED: Pool = Pool::new(MAX_STUCK_HELPERS);
        static RESERVED: Pool = Pool::reserve_for(RESERVED_HELPERS, &SHARED);
        let result = run_bounded_reserved_in(&SHARED, &RESERVED, "bounded-test", SLACK, None, || 1);
        assert_eq!(result, Ok(1));
        assert_eq!((started(&SHARED), started(&RESERVED)), (1, 0));
    }

    /// #661 E1: with every shared helper stuck, a shared call is refused
    /// at once but a reserved one runs, on the reserved helper.
    #[test]
    fn a_reserved_call_runs_while_every_shared_helper_is_stuck() {
        static SHARED: Pool = Pool::new(MAX_STUCK_HELPERS);
        static RESERVED: Pool = Pool::reserve_for(RESERVED_HELPERS, &SHARED);
        let release = stick(&SHARED, MAX_STUCK_HELPERS);
        assert_eq!(
            SHARED.run("bounded-test", SLACK, || 1),
            Err(Unanswered::Busy { marks: 0 })
        );
        let start = Instant::now();
        let result = run_bounded_reserved_in(&SHARED, &RESERVED, "bounded-test", SLACK, None, || 2);
        assert_eq!(result, Ok(2));
        assert!(start.elapsed() < SLACK, "waited {:?}", start.elapsed());
        assert_eq!(started(&SHARED), MAX_STUCK_HELPERS, "a fifth shared helper");
        assert_eq!(started(&RESERVED), 1);
        drop(release);
        wait_idle(&SHARED);
    }

    /// #661: a reserved call does not wait for a shared helper that is
    /// busy (not stuck): it takes the reserved one at once.
    #[test]
    fn a_reserved_call_does_not_wait_for_a_busy_shared_helper() {
        static SHARED: Pool = Pool::new(1);
        static RESERVED: Pool = Pool::reserve_for(RESERVED_HELPERS, &SHARED);
        let (release, released) = mpsc::channel::<()>();
        let busy = thread::spawn(move || {
            SHARED.run("bounded-test", SLACK * 4, move || {
                released.recv_timeout(SLACK * 2).ok();
            })
        });
        let deadline = Instant::now() + SLACK;
        while counts(&SHARED).waited == 0 {
            assert!(Instant::now() < deadline, "the busy call did not start");
            thread::sleep(Duration::from_millis(1));
        }
        let start = Instant::now();
        let result = run_bounded_reserved_in(&SHARED, &RESERVED, "bounded-test", SLACK, None, || 3);
        assert_eq!(result, Ok(3));
        assert!(start.elapsed() < SLACK, "waited {:?}", start.elapsed());
        assert_eq!(started(&RESERVED), 1);
        drop(release);
        assert_eq!(busy.join().unwrap(), Ok(()));
    }

    /// #661: reserved calls share the reserved helper: one that finds it
    /// busy (not stuck) waits for it within its timeout, then runs.
    #[test]
    fn a_reserved_call_waits_for_a_busy_reserved_helper() {
        static SHARED: Pool = Pool::new(1);
        static RESERVED: Pool = Pool::reserve_for(RESERVED_HELPERS, &SHARED);
        let release = stick(&SHARED, 1);
        let (go, gone) = mpsc::channel::<()>();
        let first = thread::spawn(move || {
            run_bounded_reserved_in(
                &SHARED,
                &RESERVED,
                "bounded-test",
                SLACK * 4,
                None,
                move || {
                    gone.recv_timeout(SLACK * 2).ok();
                    4
                },
            )
        });
        let deadline = Instant::now() + SLACK;
        while counts(&RESERVED).waited == 0 {
            assert!(Instant::now() < deadline, "the first call did not start");
            thread::sleep(Duration::from_millis(1));
        }
        let second = thread::spawn(|| {
            run_bounded_reserved_in(&SHARED, &RESERVED, "bounded-test", SLACK * 4, None, || 5)
        });
        thread::sleep(Duration::from_millis(50));
        assert_eq!(started(&RESERVED), 1, "a second reserved helper started");
        drop(go);
        assert_eq!(first.join().unwrap(), Ok(4));
        assert_eq!(second.join().unwrap(), Ok(5));
        assert_eq!(started(&RESERVED), 2);
        drop(release);
        wait_idle(&SHARED);
    }

    /// #661 B2: a reserved call that finds every helper of both pools
    /// taken, the reserved one stuck and a shared one only busy, is not
    /// refused at once: it waits for whichever frees first and runs on the
    /// shared helper once that one's call returns.
    #[test]
    fn a_reserved_call_waits_for_whichever_helper_frees_first() {
        static SHARED: Pool = Pool::new(1);
        static RESERVED: Pool = Pool::reserve_for(RESERVED_HELPERS, &SHARED);
        let reserve = stick(&RESERVED, RESERVED_HELPERS);
        let (release, released) = mpsc::channel::<()>();
        let busy = thread::spawn(move || {
            SHARED.run("bounded-test", SLACK * 4, move || {
                released.recv_timeout(SLACK * 2).ok();
            })
        });
        let deadline = Instant::now() + SLACK;
        while counts(&SHARED).waited == 0 {
            assert!(Instant::now() < deadline, "the busy call did not start");
            thread::sleep(Duration::from_millis(1));
        }
        let waiter = thread::spawn(|| {
            run_bounded_reserved_in(&SHARED, &RESERVED, "bounded-test", SLACK * 4, None, || 8)
        });
        thread::sleep(Duration::from_millis(50));
        assert!(!waiter.is_finished(), "refused while a helper was busy");
        drop(release);
        assert_eq!(busy.join().unwrap(), Ok(()));
        assert_eq!(waiter.join().unwrap(), Ok(8));
        assert_eq!((started(&SHARED), started(&RESERVED)), (2, 1));
        drop(reserve);
        wait_idle(&RESERVED);
    }

    /// #661, S5: the bound with the reserve. With every shared helper and
    /// the reserved one stuck, a reserved call is refused at once and
    /// starts no thread, so no more than `MAX_STUCK_HELPERS +
    /// RESERVED_HELPERS` helpers are ever stuck; once they finish, calls
    /// run again.
    #[test]
    fn with_the_reserved_helper_stuck_too_a_reserved_call_is_refused_at_once() {
        static SHARED: Pool = Pool::new(MAX_STUCK_HELPERS);
        static RESERVED: Pool = Pool::reserve_for(RESERVED_HELPERS, &SHARED);
        let shared = stick(&SHARED, MAX_STUCK_HELPERS);
        let (release, released) = mpsc::channel::<()>();
        let result = run_bounded_reserved_in(
            &SHARED,
            &RESERVED,
            "bounded-test",
            Duration::ZERO,
            None,
            move || {
                released.recv_timeout(SLACK * 2).ok();
            },
        );
        assert_eq!(result, Err(Unanswered::TimedOut));
        let stuck = SHARED.stuck() + RESERVED.stuck();
        assert_eq!(stuck, MAX_STUCK_HELPERS + RESERVED_HELPERS);
        let start = Instant::now();
        let result = run_bounded_reserved_in(&SHARED, &RESERVED, "bounded-test", SLACK, None, || 6);
        assert_eq!(result, Err(Unanswered::Busy { marks: 0 }));
        assert!(
            start.elapsed() < SLACK,
            "refused after {:?}",
            start.elapsed()
        );
        assert_eq!(
            started(&SHARED) + started(&RESERVED),
            MAX_STUCK_HELPERS + RESERVED_HELPERS,
            "a sixth helper started"
        );
        drop((shared, release));
        wait_idle(&SHARED);
        wait_idle(&RESERVED);
        let result = run_bounded_reserved_in(&SHARED, &RESERVED, "bounded-test", SLACK, None, || 7);
        assert_eq!(result, Ok(7));
    }
}
