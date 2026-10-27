//! Tests of the input API (`tessera_session_send_key`, `_send_mouse`,
//! `_send_scroll`, `_paste`, `_set_focus`), calling the `extern "C"`
//! functions as a frontend would.
//!
//! Enum round trips are pure; the end-to-end tests run `/bin/cat` or
//! `/bin/sh` in real PTYs and observe the core through the test-only
//! `tessera_test_with_core` hook. Every wait is bounded by [`TIMEOUT`].

#![cfg(unix)]

use std::collections::HashSet;
use std::ffi::{CString, c_char, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tessera_app_core::SessionId;
use tessera_app_core::input::{
    Key as CoreKey, KeyEventKind as CoreKeyEventKind, MouseButton as CoreMouseButton,
    MouseEventKind as CoreMouseEventKind,
};
use tessera_ffi::{
    App, Callbacks, Error, InputOutcome, Key, KeyEvent, KeyEventKind, Modifiers, MouseButton,
    MouseEvent, MouseEventKind, ScrollEvent, SpawnOptions, tessera_app_free, tessera_app_new,
    tessera_app_set_callbacks, tessera_session_close, tessera_session_paste,
    tessera_session_send_key, tessera_session_send_mouse, tessera_session_send_scroll,
    tessera_session_set_focus, tessera_session_spawn, tessera_test_with_core,
};

const TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Enum round trips
// ---------------------------------------------------------------------------

#[test]
fn every_key_round_trips_c_to_rust_to_c() {
    let mut core_keys = HashSet::new();
    for raw in 0..Key::COUNT {
        let key = Key::from_raw(raw).unwrap_or_else(|| panic!("no key with value {raw}"));
        assert_eq!(key as u32, raw, "{key:?} has the wrong discriminant");
        let core = key.to_core('a');
        assert_eq!(
            Key::from(core),
            key,
            "{key:?} -> {core:?} does not map back"
        );
        assert!(core_keys.insert(core), "{key:?} maps to {core:?} twice");
    }
    assert_eq!(core_keys.len(), 113);
    for raw in [Key::COUNT, Key::COUNT + 1, u32::MAX] {
        assert_eq!(Key::from_raw(raw), None, "{raw} should be no key");
    }
}

#[test]
fn char_keys_carry_their_codepoint() {
    assert_eq!(Key::Char.to_core('é'), CoreKey::Char('é'));
    assert_eq!(Key::from(CoreKey::Char('z')), Key::Char);
    assert_eq!(Key::F1.to_core('x'), CoreKey::F(1));
    assert_eq!(Key::F35.to_core('x'), CoreKey::F(35));
}

#[test]
fn keys_c_cannot_name_become_unidentified() {
    for core in [CoreKey::F(0), CoreKey::F(36), CoreKey::F(u8::MAX)] {
        assert_eq!(Key::from(core), Key::Unidentified, "{core:?}");
    }
}

#[test]
fn every_key_event_kind_round_trips() {
    let all = [
        (KeyEventKind::Press, CoreKeyEventKind::Press),
        (KeyEventKind::Repeat, CoreKeyEventKind::Repeat),
        (KeyEventKind::Release, CoreKeyEventKind::Release),
    ];
    for (raw, (kind, core)) in (0_u32..).zip(all) {
        assert_eq!(KeyEventKind::from_raw(raw), Some(kind));
        assert_eq!(CoreKeyEventKind::from(kind), core);
        assert_eq!(KeyEventKind::from(core), kind);
    }
    assert_eq!(KeyEventKind::from_raw(3), None);
    assert_eq!(KeyEventKind::from_raw(u32::MAX), None);
}

#[test]
fn every_mouse_button_round_trips() {
    let all = [
        (MouseButton::None, CoreMouseButton::None),
        (MouseButton::Left, CoreMouseButton::Left),
        (MouseButton::Middle, CoreMouseButton::Middle),
        (MouseButton::Right, CoreMouseButton::Right),
        (MouseButton::Back, CoreMouseButton::Back),
        (MouseButton::Forward, CoreMouseButton::Forward),
    ];
    for (raw, (button, core)) in (0_u32..).zip(all) {
        assert_eq!(MouseButton::from_raw(raw), Some(button));
        assert_eq!(CoreMouseButton::from(button), core);
        assert_eq!(MouseButton::from(core), button);
    }
    assert_eq!(MouseButton::from_raw(6), None);
    assert_eq!(MouseButton::from_raw(u32::MAX), None);
}

#[test]
fn every_mouse_event_kind_round_trips() {
    let all = [
        (MouseEventKind::Press, CoreMouseEventKind::Press),
        (MouseEventKind::Release, CoreMouseEventKind::Release),
        (MouseEventKind::Motion, CoreMouseEventKind::Motion),
    ];
    for (raw, (kind, core)) in (0_u32..).zip(all) {
        assert_eq!(MouseEventKind::from_raw(raw), Some(kind));
        assert_eq!(CoreMouseEventKind::from(kind), core);
        assert_eq!(MouseEventKind::from(core), kind);
    }
    assert_eq!(MouseEventKind::from_raw(3), None);
    assert_eq!(MouseEventKind::from_raw(u32::MAX), None);
}

#[test]
fn outcome_and_modifier_values_are_stable() {
    assert_eq!(InputOutcome::Ignored as u32, 0);
    assert_eq!(InputOutcome::Consumed as u32, 1);
    let bits = [
        Modifiers::None,
        Modifiers::Shift,
        Modifiers::Alt,
        Modifiers::Ctrl,
        Modifiers::Super,
        Modifiers::Hyper,
        Modifiers::Meta,
        Modifiers::CapsLock,
        Modifiers::NumLock,
    ]
    .map(|bit| bit as u32);
    assert_eq!(bits, [0, 1, 2, 4, 8, 16, 32, 64, 128]);
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Counts `on_session_output` calls.
#[derive(Default)]
struct Outputs(AtomicUsize);

unsafe extern "C" fn on_output(user_data: *mut c_void, _id: u64) {
    // SAFETY: every test installs an `Outputs` that outlives its handle.
    let outputs = unsafe { &*user_data.cast::<Outputs>() };
    outputs.0.fetch_add(1, Ordering::SeqCst);
}

/// An application handle freed on drop.
struct Handle(*mut App);

impl Handle {
    /// A handle whose `on_session_output` counts into `outputs`.
    fn new(outputs: &Outputs) -> Self {
        let mut app = ptr::null_mut();
        // SAFETY: `app` is writable storage for one pointer.
        assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
        let callbacks = Callbacks {
            user_data: ptr::from_ref(outputs).cast_mut().cast(),
            on_session_output: Some(on_output),
            on_session_title: None,
            on_session_bell: None,
            on_session_exit: None,
        };
        // SAFETY: live handle; valid table; `outputs` outlives the handle.
        assert_eq!(
            unsafe { tessera_app_set_callbacks(app, &raw const callbacks) },
            Error::Ok
        );
        Self(app)
    }

    fn spawn(&self, program: &str, args: &[&str]) -> u64 {
        let program = CString::new(program).unwrap();
        let args: Vec<CString> = args.iter().map(|arg| CString::new(*arg).unwrap()).collect();
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
        };
        let mut id = 0;
        // SAFETY: live handle, valid options, writable id.
        let code = unsafe { tessera_session_spawn(self.0, &raw const options, &raw mut id) };
        assert_eq!(code, Error::Ok);
        id
    }

    fn key(&self, id: u64, event: &KeyEvent) -> (Error, InputOutcome) {
        // Pre-set to the value a failing call must overwrite.
        let mut out = InputOutcome::Consumed;
        // SAFETY: live handle; `event` and `out` are valid for the call.
        let code = unsafe { tessera_session_send_key(self.0, id, event, &raw mut out) };
        (code, out)
    }

    fn mouse(&self, id: u64, event: &MouseEvent) -> (Error, InputOutcome) {
        let mut out = InputOutcome::Consumed;
        // SAFETY: as in `key`.
        let code = unsafe { tessera_session_send_mouse(self.0, id, event, &raw mut out) };
        (code, out)
    }

    fn scroll(&self, id: u64, delta_lines: f32) -> (Error, InputOutcome) {
        let event = ScrollEvent {
            delta_lines,
            ..scroll_event()
        };
        let mut out = InputOutcome::Consumed;
        // SAFETY: as in `key`.
        let code =
            unsafe { tessera_session_send_scroll(self.0, id, &raw const event, &raw mut out) };
        (code, out)
    }

    fn paste(&self, id: u64, text: &[u8]) -> Error {
        // SAFETY: live handle; `text` is readable for its length.
        unsafe { tessera_session_paste(self.0, id, text.as_ptr().cast(), text.len()) }
    }

    fn set_focus(&self, id: u64, focused: bool) -> Error {
        // SAFETY: live handle.
        unsafe { tessera_session_set_focus(self.0, id, focused) }
    }

    /// Runs `f` with the core application.
    fn core<R>(&self, f: impl FnOnce(&tessera_app_core::App) -> R) -> R {
        // SAFETY: live handle, not freed while `f` runs.
        unsafe { tessera_test_with_core(self.0, f) }.expect("non-null handle")
    }

    /// The text of visible row `row` of session `id`.
    fn row(&self, id: u64, row: u16) -> String {
        self.core(|core| {
            core.with_terminal(SessionId::from_raw(id), |term| {
                match term.grid().line(row) {
                    Some(line) => line.text(),
                    None => String::new(),
                }
            })
        })
        .expect("live session")
    }

    fn scroll_offset(&self, id: u64) -> usize {
        self.core(|core| core.scroll_offset(SessionId::from_raw(id)))
            .expect("live session")
    }

    fn scrollback_len(&self, id: u64) -> usize {
        self.core(|core| {
            core.with_terminal(SessionId::from_raw(id), |term| term.grid().scrollback_len())
        })
        .expect("live session")
    }

    fn is_focused(&self, id: u64) -> bool {
        self.core(|core| core.is_focused(SessionId::from_raw(id)))
            .expect("live session")
    }

    /// Polls `done` until it holds, failing the test after [`TIMEOUT`].
    fn wait_until(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !done(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a live handle not freed before.
        unsafe { tessera_app_free(self.0) };
    }
}

fn key_event(key: Key) -> KeyEvent {
    KeyEvent {
        key: key as u32,
        codepoint: 0,
        shifted: 0,
        base_layout: 0,
        modifiers: Modifiers::None as u8,
        kind: KeyEventKind::Press as u32,
        text: ptr::null(),
        text_len: 0,
    }
}

fn char_event(ch: char) -> KeyEvent {
    KeyEvent {
        codepoint: u32::from(ch),
        ..key_event(Key::Char)
    }
}

fn mouse_event() -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Press as u32,
        button: MouseButton::Left as u32,
        col: 3,
        row: 4,
        x_px: 30.5,
        y_px: 70.0,
        modifiers: Modifiers::None as u8,
    }
}

fn scroll_event() -> ScrollEvent {
    ScrollEvent {
        delta_lines: 0.0,
        delta_x_px: 0.0,
        delta_y_px: 0.0,
        precise: 0,
        col: 0,
        row: 0,
        modifiers: Modifiers::None as u8,
    }
}

/// A path for a scratch file no other test uses.
fn scratch_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "tessera-ffi-input-{name}-{}-{nanos}",
        std::process::id()
    ))
}

// ---------------------------------------------------------------------------
// End to end
// ---------------------------------------------------------------------------

#[test]
fn keys_reach_cat_and_come_back_as_output() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let id = app.spawn("/bin/cat", &[]);
    assert_eq!(
        app.key(id, &char_event('a')),
        (Error::Ok, InputOutcome::Consumed)
    );
    assert_eq!(
        app.key(id, &key_event(Key::Enter)),
        (Error::Ok, InputOutcome::Consumed)
    );
    // The terminal echoes "a", then cat writes its line back.
    app.wait_until("cat to echo the line", |app| {
        app.row(id, 0) == "a" && app.row(id, 1) == "a"
    });
    assert!(
        outputs.0.load(Ordering::SeqCst) >= 1,
        "no on_session_output"
    );
}

#[test]
fn an_ime_commit_sends_its_text() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let id = app.spawn("/bin/cat", &[]);
    let text = "ñü";
    let commit = KeyEvent {
        text: text.as_ptr().cast(),
        text_len: text.len(),
        ..key_event(Key::Unidentified)
    };
    assert_eq!(app.key(id, &commit), (Error::Ok, InputOutcome::Consumed));
    app.wait_until("the echo", |app| app.row(id, 0) == "ñü");
}

#[test]
fn a_zeroed_key_event_is_ignored() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let id = app.spawn("/bin/cat", &[]);
    // SAFETY: every field of `TesseraKeyEvent` is an integer or a raw
    // pointer, for which all-zero bytes are valid; this is C's `= {0}`.
    let zeroed: KeyEvent = unsafe { std::mem::zeroed() };
    assert_eq!(zeroed.key, Key::Unidentified as u32);
    assert_eq!(app.key(id, &zeroed), (Error::Ok, InputOutcome::Ignored));
    assert_untouched(&app, id);
}

#[test]
fn a_paste_into_a_stopped_child_never_blocks_and_close_drops_it() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    // `sleep` never reads its terminal, so the paste can only be queued.
    let id = app.spawn("/bin/sleep", &["1000"]);
    let big = vec![b'p'; 3 * 1024 * 1024];
    let started = Instant::now();
    assert_eq!(app.paste(id, &big), Error::Ok);
    assert_eq!(
        app.key(id, &char_event('k')),
        (Error::Ok, InputOutcome::Consumed)
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "queueing took {:?}",
        started.elapsed()
    );
    let closing = Instant::now();
    // SAFETY: live handle.
    assert_eq!(unsafe { tessera_session_close(app.0, id) }, Error::Ok);
    assert!(
        closing.elapsed() < Duration::from_millis(1500),
        "close took {:?}",
        closing.elapsed()
    );
}

#[test]
fn keys_that_send_nothing_are_ignored() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let id = app.spawn("/bin/cat", &[]);
    let release = KeyEvent {
        kind: KeyEventKind::Release as u32,
        ..char_event('a')
    };
    let shift = KeyEvent {
        modifiers: Modifiers::Shift as u8,
        ..key_event(Key::LeftShift)
    };
    for event in [release, shift] {
        assert_eq!(app.key(id, &event), (Error::Ok, InputOutcome::Ignored));
    }
}

#[test]
fn composed_text_and_layout_hints_are_used() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let id = app.spawn("/bin/cat", &[]);
    let text = "é";
    let composed = KeyEvent {
        text: text.as_ptr().cast(),
        text_len: text.len(),
        ..char_event('e')
    };
    assert_eq!(app.key(id, &composed), (Error::Ok, InputOutcome::Consumed));
    let shifted = KeyEvent {
        modifiers: Modifiers::Shift as u8,
        shifted: u32::from('Q'),
        ..char_event('q')
    };
    assert_eq!(app.key(id, &shifted), (Error::Ok, InputOutcome::Consumed));
    app.wait_until("the echo", |app| app.row(id, 0) == "éQ");
}

#[test]
fn paste_is_sanitised_before_the_child_reads_it() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let path = scratch_path("paste");
    let script = format!(
        "stty raw -echo; echo ready; head -c 8 > '{}'",
        path.display()
    );
    let id = app.spawn("/bin/sh", &["-c", &script]);
    // Raw mode must be on before the paste arrives, or the line discipline
    // would translate the carriage return.
    app.wait_until("the child to enter raw mode", |app| {
        app.row(id, 0) == "ready"
    });
    assert_eq!(app.paste(id, b"a\nb\x1b[31mc"), Error::Ok);
    let deadline = Instant::now() + TIMEOUT;
    let written = loop {
        match std::fs::read(&path) {
            Ok(bytes) if bytes.len() >= 8 => break bytes,
            _ => {
                assert!(Instant::now() < deadline, "the child never saw the paste");
                thread::sleep(Duration::from_millis(10));
            }
        }
    };
    let _ = std::fs::remove_file(&path);
    assert_eq!(written, b"a\rb[31mc");
}

#[test]
fn focus_round_trips() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let id = app.spawn("/bin/cat", &[]);
    assert!(!app.is_focused(id), "a new session is not focused");
    assert_eq!(app.set_focus(id, true), Error::Ok);
    assert!(app.is_focused(id));
    assert_eq!(app.set_focus(id, false), Error::Ok);
    assert!(!app.is_focused(id));
}

#[test]
fn scroll_clamps_at_both_ends_of_the_scrollback() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let id = app.spawn(
        "/bin/sh",
        &[
            "-c",
            "i=0; while [ $i -lt 40 ]; do echo line$i; i=$((i+1)); done; echo done; exec cat",
        ],
    );
    app.wait_until("the output", |app| app.row(id, 22) == "done");
    let max = app.scrollback_len(id);
    // 41 lines and the cursor's empty row on a 24-row screen.
    assert_eq!(max, 18);
    assert_eq!(app.scroll(id, -3.0), (Error::Ok, InputOutcome::Ignored));
    assert_eq!(app.scroll_offset(id), 0);
    assert_eq!(app.scroll(id, 2.5), (Error::Ok, InputOutcome::Consumed));
    assert_eq!(app.scroll_offset(id), 2);
    assert_eq!(app.scroll(id, 1e9), (Error::Ok, InputOutcome::Consumed));
    assert_eq!(app.scroll_offset(id), max);
    assert_eq!(app.scroll(id, 1.0), (Error::Ok, InputOutcome::Ignored));
    assert_eq!(app.scroll_offset(id), max);
    assert_eq!(app.scroll(id, -1e9), (Error::Ok, InputOutcome::Consumed));
    assert_eq!(app.scroll_offset(id), 0);
    // Typing returns the view to the live screen.
    assert_eq!(app.scroll(id, 5.0), (Error::Ok, InputOutcome::Consumed));
    assert_eq!(
        app.key(id, &char_event('x')),
        (Error::Ok, InputOutcome::Consumed)
    );
    assert_eq!(app.scroll_offset(id), 0);
}

#[test]
fn mouse_is_accepted_clamped_and_ignored() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let id = app.spawn("/bin/cat", &[]);
    let event = MouseEvent {
        col: u16::MAX,
        row: u16::MAX,
        x_px: -4.0,
        ..mouse_event()
    };
    assert_eq!(app.mouse(id, &event), (Error::Ok, InputOutcome::Ignored));
    let last = app
        .core(|core| core.last_mouse(SessionId::from_raw(id)))
        .unwrap()
        .expect("a recorded mouse event");
    assert_eq!((last.col, last.row, last.x_px), (79, 23, -4.0));
    assert_eq!(last.button, CoreMouseButton::Left);
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[test]
fn null_pointers_are_rejected() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let id = app.spawn("/bin/cat", &[]);
    let key = char_event('a');
    let mouse = mouse_event();
    let scroll = scroll_event();
    let mut out = InputOutcome::Consumed;
    // SAFETY: every non-NULL pointer is valid; NULLs are documented errors.
    unsafe {
        assert_eq!(
            tessera_session_send_key(app.0, id, &raw const key, ptr::null_mut()),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_send_mouse(app.0, id, &raw const mouse, ptr::null_mut()),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_send_scroll(app.0, id, &raw const scroll, ptr::null_mut()),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_send_key(app.0, id, ptr::null(), &raw mut out),
            Error::NullPointer
        );
        assert_eq!(out, InputOutcome::Ignored);
        out = InputOutcome::Consumed;
        assert_eq!(
            tessera_session_send_mouse(app.0, id, ptr::null(), &raw mut out),
            Error::NullPointer
        );
        assert_eq!(out, InputOutcome::Ignored);
        out = InputOutcome::Consumed;
        assert_eq!(
            tessera_session_send_scroll(app.0, id, ptr::null(), &raw mut out),
            Error::NullPointer
        );
        assert_eq!(out, InputOutcome::Ignored);
        out = InputOutcome::Consumed;
        assert_eq!(
            tessera_session_send_key(ptr::null_mut(), id, &raw const key, &raw mut out),
            Error::NullPointer
        );
        assert_eq!(out, InputOutcome::Ignored);
        assert_eq!(
            tessera_session_send_mouse(ptr::null_mut(), id, &raw const mouse, &raw mut out),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_send_scroll(ptr::null_mut(), id, &raw const scroll, &raw mut out),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_paste(ptr::null_mut(), id, c"x".as_ptr(), 1),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_paste(app.0, id, ptr::null(), 1),
            Error::NullPointer
        );
        assert_eq!(tessera_session_paste(app.0, id, ptr::null(), 0), Error::Ok);
        assert_eq!(
            tessera_session_set_focus(ptr::null_mut(), id, true),
            Error::NullPointer
        );
        assert!(tessera_test_with_core(ptr::null(), |_| ()).is_none());
    }
    assert!(!app.is_focused(id));
    assert_untouched(&app, id);
}

#[test]
fn unknown_sessions_are_rejected() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let id = app.spawn("/bin/cat", &[]);
    // SAFETY: live handle.
    assert_eq!(unsafe { tessera_session_close(app.0, id) }, Error::Ok);
    for stale in [0, id, id + 1, u64::MAX] {
        assert_eq!(
            app.key(stale, &char_event('a')),
            (Error::SessionNotFound, InputOutcome::Ignored)
        );
        assert_eq!(
            app.mouse(stale, &mouse_event()),
            (Error::SessionNotFound, InputOutcome::Ignored)
        );
        assert_eq!(
            app.scroll(stale, 1.0),
            (Error::SessionNotFound, InputOutcome::Ignored)
        );
        assert_eq!(app.paste(stale, b"x"), Error::SessionNotFound);
        assert_eq!(app.set_focus(stale, true), Error::SessionNotFound);
    }
}

#[test]
fn malformed_events_are_rejected_without_touching_the_session() {
    let outputs = Outputs::default();
    let app = Handle::new(&outputs);
    let id = app.spawn("/bin/cat", &[]);
    reject_malformed_keys(&app, id);
    reject_malformed_mice_and_scrolls(&app, id);
    assert_eq!(app.paste(id, &BAD_UTF8), Error::InvalidArgument);
    // Nothing reached the child or the session state.
    assert!(
        app.core(|core| core.last_mouse(SessionId::from_raw(id)))
            .unwrap()
            .is_none()
    );
    assert_eq!(app.scroll_offset(id), 0);
    assert_untouched(&app, id);
}

/// Proves nothing reached `/bin/cat` session `id` so far: a line typed now
/// is the first thing on the screen.
fn assert_untouched(app: &Handle, id: u64) {
    assert_eq!(
        app.key(id, &char_event('z')),
        (Error::Ok, InputOutcome::Consumed)
    );
    assert_eq!(
        app.key(id, &key_event(Key::Enter)),
        (Error::Ok, InputOutcome::Consumed)
    );
    app.wait_until("cat to echo the line", |app| {
        app.row(id, 0) == "z" && app.row(id, 1) == "z"
    });
}

const INVALID: (Error, InputOutcome) = (Error::InvalidArgument, InputOutcome::Ignored);
const BAD_UTF8: [u8; 3] = [b'o', 0xff, b'k'];

fn reject_malformed_keys(app: &Handle, id: u64) {
    let bad_utf8 = BAD_UTF8;
    let bad_keys = [
        KeyEvent {
            key: Key::COUNT,
            ..char_event('a')
        },
        KeyEvent {
            key: u32::MAX,
            ..char_event('a')
        },
        KeyEvent {
            kind: 3,
            ..char_event('a')
        },
        KeyEvent {
            codepoint: 0xD800,
            ..char_event('a')
        },
        KeyEvent {
            codepoint: 0x11_0000,
            ..char_event('a')
        },
        KeyEvent {
            shifted: 0xDFFF,
            ..char_event('a')
        },
        KeyEvent {
            base_layout: u32::MAX,
            ..char_event('a')
        },
        KeyEvent {
            text: ptr::null(),
            text_len: 4,
            ..char_event('a')
        },
        KeyEvent {
            text: bad_utf8.as_ptr().cast(),
            text_len: bad_utf8.len(),
            ..char_event('a')
        },
    ];
    for event in &bad_keys {
        assert_eq!(app.key(id, event), INVALID, "{event:?}");
    }
}

fn reject_malformed_mice_and_scrolls(app: &Handle, id: u64) {
    let bad_mice = [
        MouseEvent {
            kind: 3,
            ..mouse_event()
        },
        MouseEvent {
            button: 6,
            ..mouse_event()
        },
        MouseEvent {
            x_px: f32::NAN,
            ..mouse_event()
        },
        MouseEvent {
            y_px: f32::INFINITY,
            ..mouse_event()
        },
    ];
    for event in &bad_mice {
        assert_eq!(app.mouse(id, event), INVALID, "{event:?}");
    }
    for delta in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(app.scroll(id, delta), INVALID, "{delta}");
        for event in [
            ScrollEvent {
                delta_x_px: delta,
                ..scroll_event()
            },
            ScrollEvent {
                delta_y_px: delta,
                ..scroll_event()
            },
        ] {
            let mut out = InputOutcome::Consumed;
            // SAFETY: live handle; valid event and out.
            let code =
                unsafe { tessera_session_send_scroll(app.0, id, &raw const event, &raw mut out) };
            assert_eq!((code, out), INVALID);
        }
    }
}
