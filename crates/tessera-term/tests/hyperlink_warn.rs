//! The OSC 8 "hyperlink table full" warning is logged once per run of
//! refusals (#224, ADR 0026), not once per refused link.
//!
//! The warning is counted by a process-wide `tracing` subscriber, installed
//! with `set_global_default` before the test touches a terminal, so it
//! lives in this binary of its own rather than in `hyperlink.rs` (#282). A
//! thread-local `with_default` subscriber there raced the callsite interest
//! cache: another test logging on another thread could cache the warning's
//! callsite as uninteresting, and the warning never reached the counter.
//! With a global subscriber the cache is only ever computed against it.
//! The subscriber is installed once for the whole binary and counts per
//! thread, and only the one message, so tests added to this file later
//! neither panic on a second install nor disturb each other's counts.

use std::cell::Cell;
use std::fmt;
use std::sync::Once;

use tessera_term::Terminal;

/// The message `osc.rs` logs at the first refusal of a run.
const TABLE_FULL: &str = "hyperlink table full: new links are ignored";

thread_local! {
    /// Warn-level `TABLE_FULL` events this thread has logged.
    static WARNS: Cell<usize> = const { Cell::new(0) };
}

/// Counts warn-level `TABLE_FULL` events into the logging thread's
/// [`WARNS`].
struct WarnCounter;

/// Records whether an event's `message` field is [`TABLE_FULL`].
#[derive(Default)]
struct IsTableFull(bool);

impl tracing::field::Visit for IsTableFull {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}") == TABLE_FULL;
        }
    }
}

impl tracing::Subscriber for WarnCounter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() != tracing::Level::WARN {
            return;
        }
        let mut visitor = IsTableFull::default();
        event.record(&mut visitor);
        if visitor.0 {
            // `try_with`: the thread-local may be gone while its thread exits.
            let _ = WARNS.try_with(|warns| warns.set(warns.get() + 1));
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Installs the [`WarnCounter`] as the global subscriber, once per
/// binary, before the caller touches a terminal.
fn count_warns() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        tracing::subscriber::set_global_default(WarnCounter).expect("first global subscriber");
    });
}

/// The `TABLE_FULL` warnings the calling thread has logged.
fn warns() -> usize {
    WARNS.with(Cell::get)
}

fn open(uri: &str) -> Vec<u8> {
    format!("\x1b]8;;{uri}\x1b\\").into_bytes()
}

/// Opens `MAX_LIVE_LINKS` links of a cell each, from the cursor on.
fn fill_to_the_cap(term: &mut Terminal, tag: &str) {
    let first = term.grid().live_hyperlinks();
    for i in first..tessera_grid::MAX_LIVE_LINKS {
        term.advance(&open(&format!("https://a.bc/{tag}/{i}")));
        term.advance(b"x");
    }
    term.advance(b"\x1b]8;;\x1b\\");
    assert_eq!(term.grid().live_hyperlinks(), tessera_grid::MAX_LIVE_LINKS);
}

#[test]
fn refilling_at_the_cap_warns_once_per_run_until_the_table_drains() {
    count_warns();
    let mut term = Terminal::new(10, 200);
    term.set_max_scrollback(1000);
    fill_to_the_cap(&mut term, "a");
    assert_eq!(warns(), 0, "no warn below the cap");
    term.advance(&open("https://a.bc/one-too-many"));
    assert_eq!(warns(), 1);
    // A program that frees one link and opens another, 1 000 times:
    // every refill hits the cap again, but the table never drained
    // below three quarters, so this is the same run of refusals.
    for round in 0..1000 {
        term.advance(b"\x1b[1;1H\x1b[X");
        assert_eq!(
            term.grid().live_hyperlinks(),
            tessera_grid::MAX_LIVE_LINKS - 1
        );
        term.advance(&open(&format!("https://a.bc/refill/{round}")));
        term.advance(b"x");
        term.advance(&open(&format!("https://a.bc/refused/{round}")));
        assert!(term.grid().hyperlink().is_none(), "round {round}");
    }
    assert_eq!(warns(), 1, "one warn for the run");
    // Drained (ED 2 and ED 3) and filled again: a new run, warned anew.
    term.advance(b"\x1b[2J\x1b[3J\x1b[H");
    assert_eq!(term.grid().live_hyperlinks(), 0);
    fill_to_the_cap(&mut term, "b");
    term.advance(&open("https://a.bc/one-too-many"));
    term.advance(&open("https://a.bc/two-too-many"));
    assert_eq!(warns(), 2);
}
