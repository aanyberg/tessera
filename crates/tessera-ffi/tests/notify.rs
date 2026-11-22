//! `on_session_command_finished` and `tessera_app_command_finished_decision`
//! (#470, ADR 0048), called exactly as a frontend calls them, against real
//! `/bin/sh` children in PTYs. Every wait is bounded by [`TIMEOUT`].

#![cfg(unix)]

mod common;

use std::ffi::{CString, c_char, c_void};
use std::ptr;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use tessera_ffi::{
    App, COMMAND_FINISHED_MARK, COMMAND_FINISHED_MARK_AND_POST, COMMAND_FINISHED_NOTHING,
    Callbacks, Error, MAX_NOTIFICATIONS_PER_MINUTE, SpawnOptions,
    tessera_app_command_finished_decision, tessera_app_free, tessera_app_new,
    tessera_app_set_callbacks, tessera_session_spawn,
};

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct Seen {
    outputs: usize,
    /// `(session_id, exit_known, exit_code, duration_ms)` per finish.
    finishes: Vec<(u64, bool, i32, u64)>,
    exits: Vec<u64>,
}

#[derive(Default)]
struct Recorder {
    seen: Mutex<Seen>,
    changed: Condvar,
}

impl Recorder {
    fn record(&self, f: impl FnOnce(&mut Seen)) {
        f(&mut self.seen.lock().unwrap());
        self.changed.notify_all();
    }

    fn wait_exit(&self, id: u64) -> MutexGuard<'_, Seen> {
        let (seen, timeout) = self
            .changed
            .wait_timeout_while(self.seen.lock().unwrap(), TIMEOUT, |seen| {
                !seen.exits.contains(&id)
            })
            .unwrap();
        assert!(!timeout.timed_out(), "timed out waiting for exit: {seen:?}");
        seen
    }

    fn callbacks(&self) -> Callbacks {
        Callbacks {
            user_data: ptr::from_ref(self).cast_mut().cast(),
            on_session_output: Some(on_output),
            on_session_title: None,
            on_session_bell: None,
            on_session_exit: Some(on_exit),
            on_appearance_changed: None,
            on_session_clipboard: None,
            on_config_applied: None,
            on_session_remote: None,
            on_config_invalid: None,
            on_session_command_finished: Some(on_finished),
        }
    }
}

/// # Safety
///
/// `user_data` is a `Recorder` that outlives the application.
unsafe fn recorder<'a>(user_data: *mut c_void) -> &'a Recorder {
    // SAFETY: every test keeps its `Recorder` alive past `tessera_app_free`.
    unsafe { &*user_data.cast::<Recorder>() }
}

unsafe extern "C" fn on_output(user_data: *mut c_void, _id: u64) {
    // SAFETY: see `recorder`.
    unsafe { recorder(user_data) }.record(|seen| seen.outputs += 1);
}

unsafe extern "C" fn on_exit(user_data: *mut c_void, id: u64, _code: i32, _signal: i32) {
    // SAFETY: see `recorder`.
    unsafe { recorder(user_data) }.record(|seen| seen.exits.push(id));
}

unsafe extern "C" fn on_finished(
    user_data: *mut c_void,
    id: u64,
    exit_known: bool,
    exit_code: i32,
    duration_ms: u64,
) {
    // SAFETY: see `recorder`.
    unsafe { recorder(user_data) }
        .record(|seen| seen.finishes.push((id, exit_known, exit_code, duration_ms)));
}

/// An application freed on drop.
struct Handle(*mut App);

impl Handle {
    fn new() -> Self {
        common::isolate_home();
        common::hermetic_config_dir();
        let mut app = ptr::null_mut();
        // SAFETY: `app` is writable storage for one pointer.
        assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
        Self(app)
    }

    fn observed(recorder: &Recorder) -> Self {
        let handle = Self::new();
        let callbacks = recorder.callbacks();
        // SAFETY: live handle; `recorder` outlives it.
        assert_eq!(
            unsafe { tessera_app_set_callbacks(handle.0, &raw const callbacks) },
            Error::Ok
        );
        handle
    }

    fn sh(&self, script: &str) -> u64 {
        let program = CString::new("/bin/sh").unwrap();
        let args = [CString::new("-c").unwrap(), CString::new(script).unwrap()];
        let arg_ptrs: Vec<*const c_char> = args.iter().map(|arg| arg.as_ptr()).collect();
        let options = SpawnOptions {
            program: program.as_ptr(),
            args: arg_ptrs.as_ptr(),
            arg_count: arg_ptrs.len(),
            cwd: ptr::null(),
            rows: 24,
            cols: 80,
            width_px: 0,
            height_px: 0,
            profile: ptr::null(),
        };
        let mut id = 0;
        // SAFETY: live handle, valid options, writable id.
        assert_eq!(
            unsafe { tessera_session_spawn(self.0, &raw const options, &raw mut id) },
            Error::Ok
        );
        id
    }

    fn decide(&self, duration_ms: u64, focused: bool) -> u8 {
        // SAFETY: live handle, main thread (the test's).
        unsafe { tessera_app_command_finished_decision(self.0, duration_ms, focused) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a live handle, freed once.
        unsafe { tessera_app_free(self.0) };
    }
}

/// A command between OSC 133 C and D reaches the callback once, with its
/// exit status and a duration; nothing else about it crosses.
#[test]
fn a_finished_command_reaches_the_callback_once() {
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let id = app.sh(r"printf '\033]133;C\007'; sleep 0.2; printf '\033]133;D;7\007'; sleep 0.2");
    let seen = recorder.wait_exit(id);
    let [(finished, true, 7, duration_ms)] = seen.finishes[..] else {
        panic!("{seen:?}");
    };
    assert_eq!(finished, id);
    assert!(duration_ms < 60_000, "{duration_ms}");
}

/// E04: a command still running when the child exits makes no finish.
#[test]
fn a_command_running_at_exit_makes_no_finish() {
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let id = app.sh(r"printf '\033]133;C\007'; sleep 0.2; exit 0");
    let seen = recorder.wait_exit(id);
    assert!(seen.finishes.is_empty(), "{seen:?}");
}

/// E03 (S6): a child flooding C/D pairs in one write makes a bounded
/// number of finish calls: at most four command events per chunk of
/// output, so at most four finishes per output call, and far fewer than
/// one per pair, since each chunk's marks are coalesced to the last of
/// each kind (without that, every pair would make a finish).
#[test]
fn a_flood_of_finishes_is_bounded_by_the_output() {
    /// 2^17 pairs, about 2.6 MB, written by one `printf`.
    const DOUBLINGS: u32 = 17;
    let pairs = 1usize << DOUBLINGS;
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let id = app.sh(&format!(
        r#"p=$(printf '\033]133;C\007\033]133;D;0\007'); i=0; while [ $i -lt {DOUBLINGS} ]; do p="$p$p"; i=$((i+1)); done; printf '%s' "$p"; sleep 0.2"#
    ));
    let seen = recorder.wait_exit(id);
    let finishes = seen.finishes.len();
    eprintln!(
        "{pairs} pairs: {finishes} finishes, {} outputs",
        seen.outputs
    );
    assert!(finishes > 0, "{seen:?}");
    assert!(
        finishes <= 4 * seen.outputs,
        "{finishes} finishes for {} outputs",
        seen.outputs
    );
    assert!(
        finishes * 10 < pairs,
        "{finishes} finishes for {pairs} pairs"
    );
}

/// The decision reads `[notifications]` of the configuration in effect
/// (C18), says nothing for a focused or short finish (P05, P04) and
/// counts posts against the app-wide limit, marking past it (D7, L09).
#[test]
fn the_decision_follows_the_configuration_and_the_limit() {
    let config_dir = common::hermetic_config_dir();
    std::fs::write(
        config_dir.join("config.toml"),
        "[notifications]\ncommand_finished = \"unfocused\"\ncommand_finished_after = 2\n",
    )
    .unwrap();
    let app = Handle::new();
    assert_eq!(app.decide(1_999, false), COMMAND_FINISHED_NOTHING, "P03");
    assert_eq!(app.decide(60_000, true), COMMAND_FINISHED_NOTHING, "P05");
    for i in 0..MAX_NOTIFICATIONS_PER_MINUTE {
        assert_eq!(
            app.decide(2_000, false),
            COMMAND_FINISHED_MARK_AND_POST,
            "post {i}"
        );
    }
    assert_eq!(
        app.decide(u64::MAX, false),
        COMMAND_FINISHED_MARK,
        "the 11th marks"
    );
}

/// The default configuration is off: nothing, however long.
#[test]
fn the_default_decision_is_nothing() {
    let app = Handle::new();
    assert_eq!(app.decide(u64::MAX, false), COMMAND_FINISHED_NOTHING);
}

/// A NULL application decides nothing.
#[test]
fn a_null_app_decides_nothing() {
    // SAFETY: NULL is allowed.
    let decision = unsafe { tessera_app_command_finished_decision(ptr::null(), 60_000, false) };
    assert_eq!(decision, COMMAND_FINISHED_NOTHING);
}
