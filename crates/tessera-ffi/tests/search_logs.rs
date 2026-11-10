//! S6/S8 for the search calls (#331): the query is user data and the core
//! never logs it. Every search call (a valid pattern stepped, moved,
//! revealed and copied; refused patterns of each kind; calls on an
//! unknown session) runs under a subscriber that records every event at
//! every level, and no recorded field holds the pattern.
//!
//! One test, in a binary of its own: it installs a global subscriber,
//! which no other test may see.

#![cfg(unix)]

mod common;

use std::fmt::Write as _;
use std::ptr;
use std::sync::{Arc, Mutex};

use tessera_app_core::PtySize;
use tessera_ffi::{
    App, Error, SEARCH_REGEX, SearchStatus, tessera_app_free, tessera_app_new,
    tessera_session_search_clear, tessera_session_search_matches_text,
    tessera_session_search_newer, tessera_session_search_older, tessera_session_search_reveal,
    tessera_session_search_start, tessera_session_search_status, tessera_session_search_step,
    tessera_test_with_core,
};

/// The secret every pattern carries.
const SECRET: &str = "hunter2";

/// Every event, at every level, on any thread: its fields, `name=value`.
struct Capture(Mutex<Vec<String>>);

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        let _ = write!(self.0, "{}={value} ", field.name());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut line = String::from("span ");
        span.record(&mut Fields(&mut line));
        self.0.lock().unwrap().push(line);
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        let mut line = String::from("record ");
        values.record(&mut Fields(&mut line));
        self.0.lock().unwrap().push(line);
    }

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        self.0.lock().unwrap().push(line);
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

fn start(app: *mut App, id: u64, pattern: &[u8], options: u32) -> Error {
    let mut refusal = 0;
    let mut message = [0u8; 256];
    let mut len = 0;
    // SAFETY: a live handle; every buffer is valid for its length.
    unsafe {
        tessera_session_search_start(
            app,
            id,
            pattern.as_ptr(),
            pattern.len(),
            options,
            &raw mut refusal,
            message.as_mut_ptr(),
            message.len(),
            &raw mut len,
        )
    }
}

/// Every status call on `id`.
fn exercise(app: *mut App, id: u64) {
    let mut status = SearchStatus::default();
    let mut text = vec![0u8; 4096];
    let mut len = 0;
    let mut truncated = false;
    // SAFETY: a live handle and writable out-parameters.
    unsafe {
        for _ in 0..10 {
            let _ = tessera_session_search_step(app, id, 1000, &raw mut status);
        }
        let _ = tessera_session_search_older(app, id, &raw mut status);
        let _ = tessera_session_search_newer(app, id, &raw mut status);
        let _ = tessera_session_search_reveal(app, id, &raw mut status);
        let _ = tessera_session_search_status(app, id, &raw mut status);
        let _ = tessera_session_search_matches_text(
            app,
            id,
            text.as_mut_ptr(),
            text.len(),
            &raw mut len,
            &raw mut truncated,
        );
    }
}

#[test]
fn no_search_call_logs_the_query() {
    let capture = Arc::new(Capture(Mutex::new(Vec::new())));
    tracing::subscriber::set_global_default(Arc::clone(&capture)).unwrap();
    common::isolate_home();
    common::hermetic_config_dir();
    let mut app = ptr::null_mut();
    // SAFETY: `app` is a writable pointer slot.
    assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
    // SAFETY: a live handle.
    let id = unsafe {
        tessera_test_with_core(app, |core| {
            let id = core.spawn_test_session(PtySize::new(5, 40)).unwrap();
            core.feed_for_test(id, format!("{SECRET} one\r\n{SECRET} two\r\n").as_bytes())
                .unwrap();
            id.get()
        })
    }
    .unwrap();

    // A valid literal and a valid regex, searched through every call.
    assert_eq!(start(app, id, SECRET.as_bytes(), 0), Error::Ok);
    exercise(app, id);
    let regex = format!("{SECRET}|x");
    assert_eq!(start(app, id, regex.as_bytes(), SEARCH_REGEX), Error::Ok);
    exercise(app, id);
    // Refused: syntax (the message quotes the pattern), too complex, too
    // long, not UTF-8.
    let refused = [
        format!("({SECRET}").into_bytes(),
        format!(r"{SECRET}\w{{5000}}").into_bytes(),
        SECRET.repeat(200).into_bytes(),
        [SECRET.as_bytes(), b"\xff"].concat(),
    ];
    for pattern in &refused {
        assert_eq!(
            start(app, id, pattern, SEARCH_REGEX),
            Error::InvalidArgument
        );
        exercise(app, id);
    }
    // An unknown session: the failures are reported (and logged).
    let unknown = id + 1000;
    assert_eq!(
        start(app, unknown, SECRET.as_bytes(), 0),
        Error::SessionNotFound
    );
    exercise(app, unknown);
    // SAFETY: a live handle.
    unsafe {
        let _ = tessera_session_search_clear(app, id);
        tessera_app_free(app);
    }

    let lines = capture.0.lock().unwrap();
    assert!(
        lines.iter().any(|line| line.contains("refused")),
        "the capture saw no refusal: {lines:?}"
    );
    let leaked: Vec<_> = lines.iter().filter(|line| line.contains(SECRET)).collect();
    assert!(leaked.is_empty(), "S8: logged the query: {leaked:?}");
}
