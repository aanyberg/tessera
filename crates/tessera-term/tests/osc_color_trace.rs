//! S8 (#249): a colour spec or index that is not understood (an unknown
//! name, a long one past the 22-byte cap, a bad `rgb:`, a malformed `OSC 4`
//! or `OSC 104` index) is traced with its length only, never its bytes, in
//! `OSC 4`, `OSC 104` and the `OSC 10` chain: every event carries only a
//! `message` and a `len` field, and no field holds the input in any form. A
//! test-local subscriber records every event's fields (D7: no new
//! dependency).
//!
//! The events are recorded by a process-wide `tracing` subscriber,
//! installed with `set_global_default` before the test touches a terminal,
//! so this test lives in a binary of its own rather than in `osc_color.rs`
//! (#313, the race #282 fixed in `hyperlink_warn.rs`). A thread-local
//! `with_default` subscriber there could race the callsite interest cache:
//! another test logging on another thread could cache a callsite as
//! uninteresting, and its event never reached the recorder. With a global
//! subscriber the cache is only ever computed against it. The subscriber is
//! installed once for the whole binary and records per thread, so tests
//! added to this file later neither panic on a second install nor see each
//! other's events.

use std::cell::RefCell;
use std::fmt;
use std::sync::Once;

use tessera_term::Terminal;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// One event: each field's name and `Debug` text.
type Fields = Vec<(&'static str, String)>;

thread_local! {
    /// Every event this thread has logged since [`events`] last cleared it,
    /// in order.
    static EVENTS: RefCell<Vec<Fields>> = const { RefCell::new(Vec::new()) };
}

/// Records every event's fields into the logging thread's [`EVENTS`].
struct Recorder;

/// Collects an event's fields.
struct Collect(Fields);

impl Visit for Collect {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.push((field.name(), format!("{value:?}")));
    }
}

impl Subscriber for Recorder {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = Collect(Vec::new());
        event.record(&mut fields);
        // `try_with`: the thread-local may be gone while its thread exits.
        let _ = EVENTS.try_with(|events| events.borrow_mut().push(fields.0));
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// Installs the [`Recorder`] as the global subscriber, once per binary,
/// before the caller touches a terminal.
fn record_events() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        tracing::subscriber::set_global_default(Recorder).expect("first global subscriber");
    });
}

/// The events tracing `bytes` fed to a new terminal on this thread.
fn events(bytes: &[u8]) -> Vec<Fields> {
    record_events();
    EVENTS.with(|events| events.borrow_mut().clear());
    let mut term = Terminal::new(2, 4);
    term.advance(bytes);
    assert_eq!(term.take_requests(), [], "no reply");
    assert!(term.color_overrides().is_empty(), "nothing set");
    EVENTS.with(RefCell::take)
}

#[test]
fn unknown_colour_specs_are_traced_by_length_only() {
    let long = format!("secretname{}", "x".repeat(54));
    // `secret` as `Debug` prints bytes: `115, 101, 99, 114, 101, 116`.
    let secret_bytes = format!("{:?}", b"secret")
        .trim_matches(['[', ']'])
        .to_owned();
    for (head, piece, tail) in [
        ("4;1;", "secretname", ""),
        ("4;1;", long.as_str(), ""),
        ("10;", "secretname", ""),
        ("11;", long.as_str(), ""),
        ("10;;;", "secretname", ""),
        ("12;", "rgb:secretname", ""),
        ("4;", "secret", ";#000000"),
        ("104;", "secret", ""),
    ] {
        let osc = format!("{head}{piece}{tail}");
        let events = events(format!("\x1b]{osc}\x07").as_bytes());
        let len = piece.len().to_string();
        assert!(
            events.iter().any(|e| e
                .iter()
                .any(|(name, value)| *name == "len" && *value == len)),
            "OSC {osc}: no event with len={len}: {events:?}"
        );
        for (name, value) in events.iter().flatten() {
            assert!(
                matches!(*name, "message" | "len"),
                "OSC {osc}: the trace has a field {name}={value}"
            );
            assert!(
                !value.contains("secret") && !value.contains(&secret_bytes),
                "OSC {osc}: the trace holds the input: {name}={value}"
            );
        }
    }
}
