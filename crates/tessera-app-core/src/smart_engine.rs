//! The engines smart-selection rules match with (ADR 0042, #518): each
//! rule attempt's cache bounded by construction, and its time with it as
//! measured.
//!
//! [`tessera_config`] compiles every rule with the regex crate, which stays
//! the arbiter of what loads. A `regex::Regex` cannot be bounded per
//! attempt: when its lazy DFA thrashes it falls back to slower engines
//! that never give up (about 1 s for one find over the scan window, 3 s
//! and more with capture groups, #433). So each rule also gets a lazy DFA
//! of its own, built here from the same pattern with the same limits
//! (`regex_automata::hybrid::regex::Regex`, no captures), whose cache of
//! [`SMART_RULE_CACHE_BYTES`] per direction gives up the first time it
//! would be cleared: one attempt fills that cache at most once cold, after
//! at most one fresh start, a few milliseconds, within
//! [`SMART_RULE_ATTEMPT_BOUND`].
//!
//! A rule's scan of a click's window, in order:
//!
//! 1. A rule with a Unicode word boundary (`\b`, `\B`, `\<`, ...) on a
//!    window that is not all ASCII skips its DFA, which would quit at the
//!    first non-ASCII byte; so does a Unicode-`\b` rule whose DFA quits.
//! 2. Otherwise the DFA scans the window, leftmost-first and without
//!    overlaps as the regex crate's `find_iter` (through
//!    `regex_automata::util::iter::Searcher`). When a search gives up on a
//!    cache that earlier searches had used, the cache is replaced with an
//!    empty one and that search runs once more on it cold; a second give-up is the rule's for
//!    that click: no match from it, and one warning per rule index
//!    carrying the index only (S8).
//! 3. A rule that skipped its DFA, or a Unicode-`\b` rule whose DFA quit,
//!    runs the fallback instead: the matches of the regex crate's
//!    `find_iter`, each search on the rule's DFA until it quits or gives
//!    up and then on a `PikeVM` of the same program (#649: not the regex
//!    crate's own engine, whose backtracker's stack has no bound). It runs
//!    only when its compiled program's work per byte (its states, a class
//!    of many ranges and a look-around counting more, #687) times the
//!    window's bytes is at most [`SMART_FALLBACK_UNITS`] (the `PikeVM`
//!    costs at most about 4.2 nanoseconds a unit) and its states times its
//!    capture slots fit
//!    [`SMART_FALLBACK_SLOT_BYTES`]; past either the rule has no match for
//!    that click, with one warning per rule index. A rule whose DFA cache
//!    regex-automata raises past [`SMART_RULE_CACHE_BYTES`] has no
//!    fallback (#649), with one warning when its engines are built. A rule without a Unicode
//!    word boundary whose DFA quit (it cannot) has no fallback: no match,
//!    and one warning saying so.
//!
//! Caches persist across clicks, one per rule behind its own lock (a leaf:
//! held for that rule's scan only), so a warm click costs what the regex
//! crate's own cache pool did. Whether a search gives up does not depend
//! on what earlier clicks left there: a warm cache that fills is reset and
//! the search retried cold, and a search a cold cache cannot finish a warm
//! one cannot finish either. So the same click on the same text gives the
//! same selection whatever came before.

use std::ops::{ControlFlow, Range};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use regex_automata::hybrid::{dfa, regex as hybrid};
use regex_automata::nfa::thompson::{self, pikevm};
use regex_automata::util::iter::Searcher;
use regex_automata::util::prefilter::Prefilter;
use regex_automata::util::syntax;
use regex_automata::{Input, MatchErrorKind, MatchKind};
use tessera_config::{
    CompiledRule, MAX_SMART_RULES, SMART_RULE_NEST_LIMIT, SMART_RULE_SIZE_LIMIT, SmartRules,
};

/// The most CPU time one match attempt of a smart-selection rule takes
/// (S5, S6, #518): one search of the scan window, which fills the rule's
/// DFA cache at most twice (a warm cache that fills is reset and the search
/// retried cold) or runs the fallback within [`SMART_FALLBACK_UNITS`].
/// About 25 ms was the worst seen on a loaded machine, over thousands of
/// random patterns, for a DFA search, and, for a fallback attempt (the lazy DFA
/// run up to its quit, then the `PikeVM`), 32.7 ms measured worst case on a
/// cold cache and an estimated 40 ms at most, with the `PikeVM` search at
/// the gate with every state live about 21 ms of it
/// ([`SMART_FALLBACK_UNITS`], #687; before it a class of 64 one-byte
/// members took about 190 ms there, 115 ms with the regex crate's engine
/// before #649). With the
/// click budget
/// ([`crate::selection::SMART_RULE_BUDGET`]) checked after each costly
/// search (the first on a cold cache, a reset, or the fallback) and every
/// [`crate::selection::SMART_CLOCK_STRIDE`] matches, a rule costs a click
/// at most the budget plus this, plus fewer than that stride of searches
/// on cached states.
pub const SMART_RULE_ATTEMPT_BOUND: Duration = Duration::from_millis(50);

/// The most work a smart-selection rule may give its fallback in one
/// search (#518, #687): its compiled program's work per byte times the
/// scan window's bytes. A state of the program (capture groups included)
/// is one unit, a `Sparse` state a sixteenth more for each of its
/// transitions (the `PikeVM` scans them one by one) and a look-around state
/// one more (`state_work`). Past it the rule has no match for that
/// click. Measured on the fallback's `PikeVM` (#687, thread CPU time,
/// release, load about 10), one search at the gate with every state live
/// at every byte takes about 21 ms for a run of one-byte states
/// (`\b[ab]*[ab]{990}c` on random `a` and `b`, 4.2 ns a unit, the most
/// per unit), 15.6 ms for classes of 64 one-byte members (`\b C*C{990}c`),
/// 15.7 ms with a Unicode `\B` before each byte and 18.6 ms for an
/// alternation, the `PikeVM` alone. A fallback attempt adds the DFA run up to
/// its quit: measured 32.7 ms worst case on a cold cache, estimated at most
/// 40 ms, under [`SMART_RULE_ATTEMPT_BOUND`]. Counted as states alone, the classes of 64 members cost 3.9
/// times a unit (`\b C*bC{990}` of #687 took 120 ms a search, 150 ms a
/// click, at 7.99 M units under the old gate of 8 000 000). Every
/// built-in rule stays under the gate on a full 32 KiB window (the
/// largest, the compressed IPv6 rule of 131 states, counts 146 units
/// a byte, 4.78 M).
pub const SMART_FALLBACK_UNITS: usize = 5_000_000;

/// The most memory a smart-selection rule's fallback `PikeVM` may take in
/// its slot tables (S5, #590): its compiled program's states (capture
/// groups included) times its capture slots times 16 bytes (two tables of
/// 8-byte slots, as regex-automata sizes them when the fallback first runs,
/// kept in the rule's fallback cache), leaving out only the table's few
/// extra slots for one set of captures. The fallback's own program keeps
/// only the implicit group, so it takes at most this. Past it the
/// rule has no match for a click that would fall back. The table is
/// quadratic in the pattern: 250 groups repeated 23 times in a 1011-byte
/// pattern would take 177 MiB. No built-in rule has a capture group: each
/// takes at most 21 KiB, about 25 times under the cap.
pub const SMART_FALLBACK_SLOT_BYTES: usize = 512 * 1024;

/// The bytes one smart-selection rule's DFA may cache in each direction
/// (S5, #518): 512 KiB (1 MiB before #649), kept across clicks, forward
/// and reverse. regex-automata raises a direction's capacity to its
/// minimum for the program when that is larger: about 27 bytes an NFA
/// state (2 sparse sets of 8 bytes, a stack of 4 and room for two states
/// of 5 bytes per NFA state), past 512 KiB for a program of more than
/// about 19 000 states, 0.84 MiB at the most (32 768 states, all the size
/// limit allows). A rule whose capacity is raised in either direction has
/// no fallback (#649): it matches wherever its DFA completes, every line
/// that is all ASCII, and selects nothing on a line that is not when it
/// would fall back (`\b\w{20}`, whose reverse program has about 28 700
/// states, though its forward one is within the slot gate), one warning
/// per rule index when its engines are built.
///
/// What a rule keeps allocated at most (ADR 0042, #649), in bytes the
/// allocator hands out, not the crates' counts:
///
/// - its DFA's two caches, by construction at most 2.59 times what each
///   counts against its capacity: a state counts its transition row
///   (4 bytes a class, at least 2 classes), 16 bytes in the state list,
///   20 in the state map and its own bytes (at least 9), 4s + 36 + l, and
///   allocates at most twice its row and list entry (both grow by
///   doubling), 2.29 map buckets of 25 bytes and its bytes behind a
///   16-byte header rounded to 8, 8s + 112.15 + l, so at most 137.15 / 53
///   of its count; the sparse sets, stack and scratch allocate what they
///   count. A reset cache would keep its tables' capacity, and a fill of
///   many small states followed by one of large states would reach three
///   times the capacity (2.90 measured), so a give-up replaces the cache
///   with an empty one: 2 x 2.59 x 512 KiB = 2.59 MiB. A raised direction
///   allocates at most its sparse sets plus 2.59 times the rest,
///   8 + 2.59 x 19 bytes an NFA state, 1.79 MiB at 32 768 states; such a
///   rule has no `PikeVM` cache, so its terms come to at most 2 x 1.79 +
///   2 + 0.156 = 5.74 MiB;
/// - its fallback's `PikeVM` cache, by the slot gate at most 16 384 states
///   of 80 bytes (1.25 MiB): two slot tables of two 8-byte slots, two
///   sparse sets of two 4-byte ids and a stack frame of 16 bytes grown by
///   doubling;
/// - its programs, its DFA's forward and reverse NFAs (the `PikeVM` runs
///   the forward one), each compiled within
///   [`tessera_config::SMART_RULE_SIZE_LIMIT`]: 2 x 1 MiB = 2 MiB. The
///   limit bounds the compiler's count of its program, 32 bytes a state
///   plus their transitions, and the finished NFA is smaller than that
///   count (boxed slices for the compiler's vectors); the most measured,
///   over families of patterns at their largest size that loads, is
///   1.86 MiB for both, 0.93 MiB an NFA (a class of 64 one-byte members
///   repeated 1820 times; `\b(?-u:a){32760}` 1.50 MiB, `\b\w{20}`
///   1.07 MiB). tessera-config drops its own compile of the rule after
///   the load-time checks ([`tessera_config::CompiledRule::pattern`]),
///   which allocated as much again;
/// - its DFA's literal prefilter (the `PikeVM` shares it), kept only when
///   its own count (`Prefilter::memory_usage`) is at most 128 KiB: an
///   exact set of up to 250 literals becomes an Aho-Corasick DFA of
///   19 MiB or more (`\b[0-9]{2}[ab]` and 97 more bytes), and a rule
///   whose prefilter counts more has none (#649). The count leaves out
///   the automaton's own structs, at most 6.3 KiB measured over 1005
///   prefilters, so the term allows 128 + 32 KiB = 0.156 MiB.
///
/// So 2.59 + 1.25 + 2 + 0.156 = 5.996 MiB per rule, and 64 rules keep at
/// most 64 x 5.996 = 383.76 MiB, under the 384 MiB bar. The bar holds
/// through a reload too: when the rules change, the app drops the old
/// engines, filled caches and all, before it builds the new ones
/// (`SmartEngines::update`, #688; 277.8 MiB over the baseline for 64
/// rules when it held both). Every cache is one per rule whatever thread
/// clicks: the app holds one `SmartEngines` behind a lock that each click
/// holds while it searches (read, so clicks run side by side) and a
/// rebuild holds written, so every session and thread shares it, it is
/// rebuilt only when the rules change, and a rebuild waits for the clicks
/// on the old engines to return; each rule's DFA cache and `PikeVM` cache
/// sit behind that rule's own `Mutex`; and no engine keeps a per-thread
/// pool. So a further clicking thread waits for the rule's lock and adds
/// 0 MiB, and a click racing a reload searches either the old rules with
/// the old engines or the new rules with the new ones.
///
/// Measured (release, kept heap after the clicks, programs included, #649):
/// the critic's mix of a large program and a cache filler
/// (`\b(?:(?-u:[cegikmoqsuwyCEGIKMOQSUWY02468]){2000}|(?-u:q){12000}|[ab]*a[ab]{14})`)
/// 2.67 MiB alone; 64 rules mixing it with the families above, cache
/// fillers and stack fillers 96.3 MiB, which tessera-ffi's
/// `smart_selection_heap` holds under the ceiling.
pub const SMART_RULE_CACHE_BYTES: usize = 512 * 1024;

/// The most a rule's literal prefilter may count
/// (`Prefilter::memory_usage`) to be kept (#649,
/// [`SMART_RULE_CACHE_BYTES`]); past it the rule's DFA has none.
const SMART_PREFILTER_BYTES: usize = 128 * 1024;

/// What a kept prefilter allocates at most: its count and 32 KiB for
/// the structs the count leaves out (6.3 KiB the most measured).
const RULE_PREFILTER_BYTES: usize = SMART_PREFILTER_BYTES + 32 * 1024;

/// What a lazy DFA cache allocates at most, in hundredths of its capacity
/// (#649 B2), by construction over one fill from empty (a give-up
/// replaces the cache, [`SMART_RULE_CACHE_BYTES`]): 137.15 / 53, at most
/// 2.59 times.
const DFA_ALLOC_HUNDREDTHS: usize = 259;

/// The most states a fallback program has within
/// [`SMART_FALLBACK_SLOT_BYTES`]: every program has at least the two slots
/// of the whole match.
const FALLBACK_MAX_STATES: usize = SMART_FALLBACK_SLOT_BYTES / (2 * 16);

/// The `PikeVM`'s cache per state: two slot tables of two 8-byte slots, two
/// sparse sets of two 4-byte ids, and a 16-byte stack frame grown by
/// doubling.
const FALLBACK_STATE_BYTES: usize = 2 * 2 * 8 + 2 * 2 * 4 + 2 * 16;

/// The bytes a rule's programs (its DFA's forward and reverse NFAs)
/// allocate at most: each within [`SMART_RULE_SIZE_LIMIT`] (#649 B1,
/// [`SMART_RULE_CACHE_BYTES`]).
const RULE_PROGRAM_BYTES: usize = 2 * SMART_RULE_SIZE_LIMIT;

/// What a rule's two DFA caches allocate at most.
const RULE_DFA_BYTES: usize = (2 * SMART_RULE_CACHE_BYTES * DFA_ALLOC_HUNDREDTHS).div_ceil(100);

/// What one rule keeps allocated at most ([`SMART_RULE_CACHE_BYTES`] names
/// the terms): its DFA's caches, its fallback's cache, its programs and
/// its prefilter.
const RULE_CEILING: usize = RULE_DFA_BYTES
    + FALLBACK_MAX_STATES * FALLBACK_STATE_BYTES
    + RULE_PROGRAM_BYTES
    + RULE_PREFILTER_BYTES;

/// Every rule's allocations at most, whatever threads click.
const SMART_CACHE_CEILING: usize = MAX_SMART_RULES * RULE_CEILING;

/// The memory bar the ceiling stays under (#649).
const SMART_CACHE_BAR: usize = 384 << 20;

const _: () = assert!(SMART_CACHE_CEILING <= SMART_CACHE_BAR);

#[cfg(test)]
thread_local! {
    /// Test hook: how many times a rule's fallback ran on this thread.
    pub(crate) static FALLBACK_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Test hook: how many give-up or over-the-gate warnings, or warnings
    /// that a DFA quit with no fallback, were logged on this thread.
    pub(crate) static WARNINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The engines of one set of smart-selection rules, one per rule in order,
/// built from the patterns the regex crate compiled.
#[derive(Debug, Default)]
pub(crate) struct SmartEngines {
    rules: Arc<SmartRules>,
    /// One per rule of `rules`; [`SmartEngines::update`] keeps them, caches
    /// and all, for a reload of equal rules, and drops them before it builds
    /// others. Shared only by test handles ([`SmartEngines::shared`]).
    engines: Arc<[RuleEngine]>,
}

/// What the search that found a match may have cost (#557): whether the
/// caller must read the clock after it to keep a click within
/// [`crate::selection::SMART_RULE_BUDGET`] plus one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchCost {
    /// A DFA search on a cache it neither found cold nor reset: the states
    /// it added since the cache was last reset fit in one fill, so a run of
    /// such searches costs at most one fill plus a pass over the window
    /// each (microseconds on cached states).
    Cached,
    /// A search that may have cost up to [`SMART_RULE_ATTEMPT_BOUND`]: the
    /// first on a cold cache, one that reset its cache and ran again cold,
    /// or one of the regex crate's fallback.
    Costly,
}

/// How a rule's scan of a window ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scanned {
    /// Every match it reported stands: it saw them all, or the caller
    /// stopped it.
    Done,
    /// It has no match for this click (it gave up, its fallback was over
    /// the gate, or its DFA quit with no fallback): the caller drops what
    /// it reported.
    Nothing,
}

impl SmartEngines {
    /// The engines of `rules`. A rule whose DFA does not build (none the
    /// regex crate accepts is known to) always takes the fallback.
    pub(crate) fn new(rules: Arc<SmartRules>) -> Self {
        let engines = rules
            .rules()
            .iter()
            .enumerate()
            .map(|(index, rule)| RuleEngine::new(index, rule.pattern()))
            .collect();
        Self { rules, engines }
    }

    /// The rules these engines are for.
    pub(crate) fn rules(&self) -> &SmartRules {
        &self.rules
    }

    /// Whether these are the engines of `rules`, that compiled set.
    pub(crate) fn are_for(&self, rules: &Arc<SmartRules>) -> bool {
        Arc::ptr_eq(&self.rules, rules)
    }

    /// Makes these the engines of `rules`: unchanged when they are for that
    /// compiled set; these rules' engines, caches and warnings kept, when
    /// `rules` holds the same patterns with the same precisions and checks
    /// in the same order (a reload that changed no rule compiles them
    /// again, #518); new ones otherwise. The old engines, filled caches and
    /// all, are dropped before the new ones are built, so a rules change
    /// holds one set at a time (#688) unless another handle to them lives.
    pub(crate) fn update(&mut self, rules: Arc<SmartRules>) {
        if self.are_for(&rules) {
            return;
        }
        let same = |a: &CompiledRule, b: &CompiledRule| {
            a.pattern() == b.pattern() && a.precision() == b.precision() && a.check() == b.check()
        };
        let unchanged = self.rules.len() == rules.len()
            && self
                .rules
                .rules()
                .iter()
                .zip(rules.rules())
                .all(|(a, b)| same(a, b));
        if unchanged {
            self.rules = rules;
        } else {
            // The new engines are built before an assignment drops the old
            // ones, so the old ones go first.
            *self = Self::default();
            *self = Self::new(rules);
        }
    }

    /// Another handle to these engines, which keeps them while it lives.
    #[cfg(test)]
    pub(crate) fn shared(&self) -> Self {
        Self {
            rules: Arc::clone(&self.rules),
            engines: Arc::clone(&self.engines),
        }
    }

    /// Whether these and `other` share their engines.
    #[cfg(test)]
    pub(crate) fn shares_engines_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.engines, &other.engines)
    }

    /// Scans `text` with rule `index` from its start, calling `found` with
    /// each match's bytes, and what its search may have cost, in order
    /// until it breaks. `ascii` is whether `text` is all ASCII. See the
    /// module docs for the engine it takes.
    pub(crate) fn scan(
        &self,
        index: usize,
        text: &str,
        ascii: bool,
        mut found: impl FnMut(Range<usize>, SearchCost) -> ControlFlow<()>,
    ) -> Scanned {
        let (Some(engine), Some(rule)) = (self.engines.get(index), self.rules.rules().get(index))
        else {
            return Scanned::Nothing;
        };
        let mut seen = 0;
        if let Some(dfa) = &engine.dfa
            && (ascii || !engine.unicode_word)
        {
            match dfa.scan(text, &mut found) {
                DfaScan::Done => return Scanned::Done,
                DfaScan::GaveUp => {
                    RuleEngine::warn_once(&engine.gave_up, index, "its DFA cache filled");
                    return Scanned::Nothing;
                }
                // Not reached (#590): without a Unicode word boundary the
                // DFA has no byte to quit on, and a rule with one runs it
                // on all-ASCII windows only. Such a rule has no fallback, so
                // the quit is the rule's answer for that click.
                DfaScan::Quit(_) if !engine.unicode_word => {
                    RuleEngine::warn_once(
                        &engine.over_gate,
                        index,
                        "its DFA quit and it has no fallback",
                    );
                    return Scanned::Nothing;
                }
                DfaScan::Quit(matches) => seen = matches,
            }
        }
        engine.fallback(index, rule.pattern(), text, seen, found)
    }

    /// Whether rule `index` has given up, been over the gate, or had its
    /// DFA quit with no fallback in a click since these engines were built
    /// (its warning is logged).
    #[cfg(test)]
    pub(crate) fn warned(&self, index: usize) -> bool {
        self.engines.get(index).is_some_and(|engine| {
            engine.gave_up.load(Ordering::Relaxed) || engine.over_gate.load(Ordering::Relaxed)
        })
    }

    /// Whether rule `index` has no fallback because its DFA's cache is
    /// raised past [`SMART_RULE_CACHE_BYTES`].
    #[cfg(test)]
    pub(crate) fn barred_from_fallback(&self, index: usize) -> bool {
        self.engines
            .get(index)
            .is_some_and(|engine| engine.unicode_word && !engine.may_fall_back())
    }

    /// What rule `index`'s kept prefilter counts, if it has one.
    #[cfg(test)]
    pub(crate) fn prefilter_bytes(&self, index: usize) -> Option<usize> {
        let dfa = self.engines.get(index)?.dfa.as_ref()?;
        let config = dfa.regex.forward().get_config();
        config.get_prefilter().map(Prefilter::memory_usage)
    }

    /// Whether rule `index` has a DFA (it built).
    #[cfg(test)]
    pub(crate) fn has_dfa(&self, index: usize) -> bool {
        self.engines
            .get(index)
            .is_some_and(|engine| engine.dfa.is_some())
    }

    /// Rule `index`'s fallback work for a window of `bytes` bytes, as the
    /// gate counts it ([`SMART_FALLBACK_UNITS`]); `None` when it may never
    /// fall back.
    #[cfg(test)]
    pub(crate) fn fallback_units(&self, index: usize, bytes: usize) -> Option<usize> {
        let engine = self.engines.get(index)?;
        let rule = self.rules.rules().get(index)?;
        engine
            .fallback_states(rule.pattern())
            .map(|cost| cost.work.saturating_mul(bytes))
    }

    /// The bytes rule `index`'s fallback slot tables take, as the gate
    /// counts them ([`SMART_FALLBACK_SLOT_BYTES`]); `None` when it may
    /// never fall back.
    #[cfg(test)]
    pub(crate) fn fallback_slot_bytes(&self, index: usize) -> Option<usize> {
        let engine = self.engines.get(index)?;
        let rule = self.rules.rules().get(index)?;
        engine
            .fallback_states(rule.pattern())
            .map(FallbackCost::slot_bytes)
    }
}

/// The size of a rule's program with capture groups, as the regex crate's
/// fallback engines run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FallbackCost {
    /// Its states.
    states: usize,
    /// Its capture slots (two per group, the whole match included).
    slots: usize,
    /// What a `PikeVM` search costs per byte of the window at most, in
    /// units of one state ([`state_work`], #687).
    work: usize,
}

/// What one state of a fallback program costs a `PikeVM` search per byte
/// at most, in sixteenths of a unit ([`SMART_FALLBACK_UNITS`], #687): a
/// unit for every state, and a sixteenth more for each transition of a
/// `Sparse` state, which the `PikeVM` scans one by one (a class of 64
/// one-byte members counts 5 units, about 3.9 times a byte's cost as
/// measured), and a unit more for a look-around state, which a Unicode word
/// boundary evaluates by decoding the characters on both sides (1.4 times a
/// byte's cost measured with `\B` before each byte).
fn state_work(state: &thompson::State) -> usize {
    match state {
        thompson::State::Sparse(sparse) => 16 + sparse.transitions.len(),
        thompson::State::Look { .. } => 32,
        _ => 16,
    }
}

impl FallbackCost {
    /// The bytes of the `PikeVM`'s two slot tables: 8 bytes per slot of
    /// every state in each (regex-automata's `SlotTable::reset`).
    fn slot_bytes(self) -> usize {
        self.states.saturating_mul(self.slots).saturating_mul(16)
    }

    /// Whether the fallback may run on a window of `bytes` bytes: within
    /// [`SMART_FALLBACK_UNITS`] and [`SMART_FALLBACK_SLOT_BYTES`].
    fn within(self, bytes: usize) -> bool {
        self.slot_bytes() <= SMART_FALLBACK_SLOT_BYTES
            && self.work.saturating_mul(bytes) <= SMART_FALLBACK_UNITS
    }
}

/// One rule's engine.
#[derive(Debug)]
struct RuleEngine {
    /// The lazy DFA, unless it did not build.
    dfa: Option<Dfa>,
    /// Whether the rule has a Unicode word boundary, on which the DFA quits
    /// at any non-ASCII byte.
    unicode_word: bool,
    /// The states of the rule's program with capture groups, as the regex
    /// crate's fallback engines run it, compiled the first time a click
    /// needs the fallback (most clicks never do); `None` for a rule that
    /// cannot reach the fallback (no Unicode word boundary and a DFA, which
    /// then never quits), so a quit there selects nothing
    /// ([`SmartEngines::scan`]).
    fallback_states: OnceLock<Option<FallbackCost>>,
    /// The fallback's `PikeVM`, built the first time a click runs it within
    /// the gate; `None` if it did not build (the regex crate built the
    /// same pattern with the same limits).
    fallback: OnceLock<Option<Fallback>>,
    /// Whether its give-up warning is logged.
    gave_up: AtomicBool,
    /// Whether its over-the-gate warning, or its warning that its DFA quit
    /// with no fallback, is logged.
    over_gate: AtomicBool,
}

impl RuleEngine {
    fn new(index: usize, pattern: &str) -> Self {
        let dfa = Dfa::build(pattern);
        if dfa.is_none() {
            tracing::warn!(
                rule = index,
                "smart selection rule has no bounded engine; it falls back"
            );
        }
        let unicode_word = dfa.as_ref().is_none_or(|dfa| {
            dfa.regex
                .forward()
                .get_nfa()
                .look_set_any()
                .contains_word_unicode()
        });
        // A rule whose DFA cache is raised has no fallback: its warning is
        // logged once, now, and not again by a click.
        let barred = unicode_word && dfa.as_ref().is_some_and(|dfa| dfa.raised);
        if barred {
            #[cfg(test)]
            WARNINGS.with(|warnings| warnings.set(warnings.get() + 1));
            tracing::warn!(
                rule = index,
                "smart selection rule's program needs a DFA cache over the standard size; \
                 it selects nothing on a line that is not ASCII"
            );
        }
        Self {
            dfa,
            unicode_word,
            fallback_states: OnceLock::new(),
            fallback: OnceLock::new(),
            gave_up: AtomicBool::new(false),
            over_gate: AtomicBool::new(barred),
        }
    }

    /// Whether the rule may run its fallback: not when its DFA's cache is
    /// raised past [`SMART_RULE_CACHE_BYTES`] (#649).
    fn may_fall_back(&self) -> bool {
        self.dfa.as_ref().is_none_or(|dfa| !dfa.raised)
    }

    /// The fallback program's size ([`RuleEngine::fallback_states`]).
    fn fallback_states(&self, pattern: &str) -> Option<FallbackCost> {
        if !self.unicode_word {
            return None;
        }
        *self.fallback_states.get_or_init(|| {
            thompson::Compiler::new()
                .syntax(syntax::Config::new().nest_limit(SMART_RULE_NEST_LIMIT))
                .configure(
                    thompson::Config::new()
                        .nfa_size_limit(Some(SMART_RULE_SIZE_LIMIT))
                        .which_captures(thompson::WhichCaptures::All),
                )
                .build(pattern)
                .ok()
                .map(|nfa| FallbackCost {
                    states: nfa.states().len(),
                    slots: nfa.group_info().slot_len(),
                    work: nfa
                        .states()
                        .iter()
                        .map(state_work)
                        .sum::<usize>()
                        .div_ceil(16),
                })
        })
    }

    /// The fallback's matches in `text`, as the regex crate's `find_iter`
    /// finds them, the first `skip` (already reported) left out, when
    /// within the gate. Each search runs the rule's DFA first, as the regex
    /// crate runs its lazy DFA, and the `PikeVM` when the DFA quits (on a
    /// byte that is not ASCII, at a Unicode word boundary) or gives up;
    /// after a give-up the DFA's cache is replaced with an empty one and the rest of the scan is
    /// the `PikeVM`'s, so one scan fills that cache at most once.
    fn fallback(
        &self,
        index: usize,
        pattern: &str,
        text: &str,
        skip: usize,
        mut found: impl FnMut(Range<usize>, SearchCost) -> ControlFlow<()>,
    ) -> Scanned {
        let within = self.may_fall_back()
            && self
                .fallback_states(pattern)
                .is_some_and(|cost| cost.within(text.len()));
        if !within {
            Self::warn_once(&self.over_gate, index, "its fallback is over the gate");
            return Scanned::Nothing;
        }
        let Some(fallback) = self
            .fallback
            .get_or_init(|| Fallback::build(self.dfa.as_ref(), pattern))
        else {
            Self::warn_once(&self.over_gate, index, "its fallback did not build");
            return Scanned::Nothing;
        };
        #[cfg(test)]
        FALLBACK_RUNS.with(|runs| runs.set(runs.get() + 1));
        // Lock order: the fallback's cache, then the DFA's (both leaves).
        let mut vm = fallback.cache.lock().unwrap_or_else(|poisoned| {
            let mut held = poisoned.into_inner();
            held.reset(&fallback.vm);
            held
        });
        let mut dfa = self.dfa.as_ref().map(|dfa| (dfa, dfa.lock()));
        let mut searcher = Searcher::new(Input::new(text));
        let mut seen = 0;
        loop {
            let next = searcher.advance(|input| {
                if let Some((engine, held)) = &mut dfa {
                    held.cold = false;
                    match engine.regex.try_search(&mut held.cache, input) {
                        Ok(one) => return Ok(one),
                        Err(error) if matches!(error.kind(), MatchErrorKind::GaveUp { .. }) => {
                            held.cache = engine.empty_cache();
                            held.cold = true;
                            dfa = None;
                        }
                        Err(_) => {}
                    }
                }
                Ok(fallback.vm.find(&mut vm, input.clone()))
            });
            let Some(one) = next else {
                return Scanned::Done;
            };
            seen += 1;
            if seen > skip && found(one.range(), SearchCost::Costly).is_break() {
                return Scanned::Done;
            }
        }
    }

    /// Logs that rule `index` selects nothing (`why`), the first time only.
    fn warn_once(flag: &AtomicBool, index: usize, why: &str) {
        if flag.swap(true, Ordering::Relaxed) {
            return;
        }
        #[cfg(test)]
        WARNINGS.with(|warnings| warnings.set(warnings.get() + 1));
        tracing::warn!(
            rule = index,
            why,
            "smart selection rule selects nothing for a click"
        );
    }
}

/// A rule's fallback: a `PikeVM` on its DFA's forward program (one
/// program for both), or on its own when the DFA did not build, with one
/// cache kept across clicks behind a leaf lock. The regex crate's own
/// engine (`regex::Regex`, `regex_automata::meta`) is not used: its bounded
/// backtracker's stack has no bound (#649 B1: one rule kept 10.5 MiB), and
/// it would compile the program again and keep three lazy DFA caches more.
#[derive(Debug)]
struct Fallback {
    vm: pikevm::PikeVM,
    cache: Mutex<pikevm::Cache>,
}

impl Fallback {
    /// The fallback of `pattern`, whose DFA is `dfa`; `None` if it does not
    /// build.
    fn build(dfa: Option<&Dfa>, pattern: &str) -> Option<Self> {
        let config = pikevm::Config::new().match_kind(MatchKind::LeftmostFirst);
        let vm = match dfa {
            Some(dfa) => {
                let forward = dfa.regex.forward();
                pikevm::Builder::new()
                    .configure(config.prefilter(forward.get_config().get_prefilter().cloned()))
                    .build_from_nfa(forward.get_nfa().clone())
            }
            None => pikevm::Builder::new()
                .configure(config)
                .syntax(syntax::Config::new().nest_limit(SMART_RULE_NEST_LIMIT))
                .thompson(
                    thompson::Config::new()
                        .nfa_size_limit(Some(SMART_RULE_SIZE_LIMIT))
                        .which_captures(thompson::WhichCaptures::Implicit),
                )
                .build(pattern),
        }
        .ok()?;
        let cache = Mutex::new(vm.create_cache());
        Some(Self { vm, cache })
    }
}

/// A rule's lazy DFA and its cache.
#[derive(Debug)]
struct Dfa {
    regex: hybrid::Regex,
    /// A leaf lock: held for one scan.
    cache: Mutex<RuleCache>,
    /// Whether regex-automata raised a direction's cache capacity past
    /// [`SMART_RULE_CACHE_BYTES`] to its minimum for the program: such a
    /// rule has no fallback (#649), so its raised cache and a `PikeVM`
    /// cache are never kept together.
    raised: bool,
}

/// A rule's DFA cache, and whether it is cold.
#[derive(Debug)]
struct RuleCache {
    cache: hybrid::Cache,
    /// Whether no search has run on it since it was made or reset.
    cold: bool,
}

/// How a DFA scan ended.
#[derive(Debug)]
enum DfaScan {
    /// It reported every match, or the caller stopped it.
    Done,
    /// A search gave up on a cold cache.
    GaveUp,
    /// A search quit, after this many matches were reported.
    Quit(usize),
}

impl Dfa {
    /// The DFA of `pattern` within the regex crate's limits
    /// ([`SMART_RULE_SIZE_LIMIT`] for the program and each cache,
    /// [`SMART_RULE_NEST_LIMIT`]), giving up instead of clearing its cache,
    /// with a literal prefilter as the regex crate has when it counts at
    /// most [`SMART_PREFILTER_BYTES`]; `None` if it does not build.
    fn build(pattern: &str) -> Option<Self> {
        // One parse for both directions and the prefilter.
        let syntax = syntax::Config::new().nest_limit(SMART_RULE_NEST_LIMIT);
        let hir = syntax::parse_with(pattern, &syntax).ok()?;
        // A large literal set's prefilter is an Aho-Corasick DFA that no
        // cache bounds: kept only within its term (#649).
        let prefilter = Prefilter::from_hirs_prefix(MatchKind::LeftmostFirst, &[&hir])
            .filter(|prefilter| prefilter.memory_usage() <= SMART_PREFILTER_BYTES);
        let nfa = thompson::Config::new()
            .nfa_size_limit(Some(SMART_RULE_SIZE_LIMIT))
            .which_captures(thompson::WhichCaptures::None);
        // The whole match's capture slots, which the fallback's `PikeVM`
        // reports from; the DFA skips them.
        let forward = thompson::Compiler::new()
            .configure(
                nfa.clone()
                    .which_captures(thompson::WhichCaptures::Implicit),
            )
            .build_from_hir(&hir)
            .ok()?;
        let reverse = thompson::Compiler::new()
            .configure(nfa.reverse(true))
            .build_from_hir(&hir)
            .ok()?;
        let config = dfa::Config::new()
            .cache_capacity(SMART_RULE_CACHE_BYTES)
            // A program near the size limit needs most of the cache; the
            // check would refuse some at 1 MiB minus overhead, and the
            // minimum is used instead.
            .skip_cache_capacity_check(true)
            // Without it a pattern with `\b` does not build; with it the
            // DFA quits on non-ASCII bytes (handled above).
            .unicode_word_boundary(true)
            // Give up the first time the cache would be cleared: one fill
            // bounds one search.
            .minimum_cache_clear_count(Some(0));
        let forward = dfa::Builder::new()
            .configure(config.clone().prefilter(prefilter))
            .build_from_nfa(forward)
            .ok()?;
        // As `hybrid::regex::Builder::build` configures its reverse DFA.
        let reverse = dfa::Builder::new()
            .configure(
                config
                    .prefilter(None)
                    .specialize_start_states(false)
                    .match_kind(MatchKind::All),
            )
            .build_from_nfa(reverse)
            .ok()?;
        let raised = [&forward, &reverse].into_iter().any(|dfa| {
            dfa.get_config()
                .get_minimum_cache_capacity(dfa.get_nfa())
                .ok()
                .is_none_or(|minimum| minimum > SMART_RULE_CACHE_BYTES)
        });
        let regex = hybrid::Regex::builder().build_from_dfas(forward, reverse);
        let cache = regex.create_cache();
        Some(Self {
            regex,
            cache: Mutex::new(RuleCache { cache, cold: true }),
            raised,
        })
    }

    /// An empty cache, in place of a reset one: `hybrid::Cache::reset`
    /// keeps every table's capacity, so a fill of many small states and
    /// then a refill of large ones would allocate up to three times the
    /// cache's capacity (2.90 times measured, #649 B2); a new cache
    /// allocates for one fill only ([`SMART_RULE_CACHE_BYTES`]).
    fn empty_cache(&self) -> hybrid::Cache {
        self.regex.create_cache()
    }

    /// Its cache, held.
    fn lock(&self) -> MutexGuard<'_, RuleCache> {
        self.cache.lock().unwrap_or_else(|poisoned| {
            // A scan that panicked may have left the cache half updated.
            let mut held = poisoned.into_inner();
            held.cache = self.empty_cache();
            held.cold = true;
            held
        })
    }

    /// Scans `text` from its start, reporting each match, and whether its
    /// search found the cache cold or reset it, to `found`.
    fn scan(
        &self,
        text: &str,
        found: &mut impl FnMut(Range<usize>, SearchCost) -> ControlFlow<()>,
    ) -> DfaScan {
        let mut held = self.lock();
        let RuleCache { cache, cold } = &mut *held;
        let mut searcher = Searcher::new(Input::new(text));
        let mut seen = 0;
        loop {
            let mut cost = SearchCost::Cached;
            let next = searcher.try_advance(|input| {
                let was_cold = std::mem::replace(cold, false);
                if was_cold {
                    cost = SearchCost::Costly;
                }
                match self.regex.try_search(cache, input) {
                    Err(error)
                        if !was_cold && matches!(error.kind(), MatchErrorKind::GaveUp { .. }) =>
                    {
                        cost = SearchCost::Costly;
                        *cache = self.empty_cache();
                        self.regex.try_search(cache, input)
                    }
                    result => result,
                }
            });
            match next {
                Ok(Some(one)) => {
                    seen += 1;
                    if found(one.range(), cost).is_break() {
                        return DfaScan::Done;
                    }
                }
                Ok(None) => return DfaScan::Done,
                Err(error) => {
                    return match error.kind() {
                        MatchErrorKind::GaveUp { .. } => DfaScan::GaveUp,
                        _ => DfaScan::Quit(seen),
                    };
                }
            }
        }
    }
}

#[cfg(test)]
impl SmartEngines {
    /// The bytes rule `index`'s DFA cache holds, forward and reverse.
    pub(crate) fn cache_bytes(&self, index: usize) -> usize {
        self.engines
            .get(index)
            .and_then(|engine| engine.dfa.as_ref())
            .map_or(0, |dfa| dfa.cache.lock().unwrap().cache.memory_usage())
    }
}

/// `count` characters over many scripts from `seed`: ASCII, accented
/// Latin, Greek, Cyrillic, Hebrew, CJK, Hangul, emoji and digits, now and
/// then an ASCII letter with a combining accent.
#[cfg(test)]
pub(crate) fn multi_script(count: usize, mut seed: u64) -> String {
    const RANGES: [(u32, u32); 9] = [
        (0x21, 0x7E),
        (0xC0, 0x17F),
        (0x391, 0x3C9),
        (0x410, 0x44F),
        (0x4E00, 0x9FFF),
        (0xAC00, 0xD7A3),
        (0x1F600, 0x1F64F),
        (0x5D0, 0x5EA),
        (0x30, 0x39),
    ];
    let mut text = String::new();
    for _ in 0..count {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let (low, high) = RANGES[usize::try_from(seed % 9).unwrap()];
        let span = u64::from(high - low + 1);
        let c = char::from_u32(low + u32::try_from((seed >> 8) % span).unwrap()).unwrap();
        text.push(c);
        if (seed >> 40).is_multiple_of(7) && c.is_ascii_alphabetic() {
            text.push('\u{301}');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::time::Instant;

    use proptest::prelude::*;
    use tessera_config::{DEFAULT_SMART_RULES, Precision, Selection, SmartRule};

    use super::*;

    /// The fixture's tricky patterns (#518 section 6): empty matches, `\B`,
    /// the half and start/end word boundaries, case folding with U+212A
    /// and `ſ`, lazy repeats, anchors and Unicode classes.
    const TRICKY: [&str; 36] = [
        r"\b",
        r"x*",
        r"(?:)",
        r"a*?",
        r"$",
        r"(?m)^",
        r"\B",
        "\u{e9}*",
        r"\p{L}*",
        r"(?-u)\b\w",
        r"(?i)k+",
        r"(?i)ſ+",
        r"a+?",
        r"[a-z]{2,5}?",
        r"\w+?b",
        r"(?U)a+",
        r"\b{start}\w+",
        r"\w+\b{end}",
        r"\b{start-half}\w",
        r"\w\b{end-half}",
        r"\<\w+",
        r"\w+\>",
        r"(?R)^a$",
        r"(?m)^\w+$",
        r"\A\w+",
        r"\w+\z",
        r"(?x) a b # comment",
        r"\p{Greek}+",
        r"[[:alpha:]]+",
        r"(?i)stra(?:ß|ss)e",
        r".",
        r"\w{2,6}",
        r"\S{1,200}",
        r"\B\w+\B",
        r"(?s).{3}",
        r"\d+(?:\.\d+)*",
    ];

    /// Windows of every class: ASCII, Latin, CJK and emoji, combining and
    /// decomposed, real and random multi-script.
    fn windows() -> Vec<String> {
        vec![
            "std::vector<int> a.b.c ~/src/x.rs \"q\\\"s\" mailto:a@b.co 10.0.0.1 ::1 ab ba k K".to_owned(),
            "caf\u{e9} na\u{ef}ve stra\u{df}e Stra\u{df}e STRASSE \u{17f}\u{17f} \u{212a}elvin K\u{212a}k 10.1.2.3".to_owned(),
            "\u{65e5}\u{672c}\u{8a9e} \u{30c6}\u{30b9}\u{30c8} \u{1f600}\u{1f44d}\u{1f3fd} x/y \u{65e5}/\u{65e5} 192.168.0.1".to_owned(),
            "e\u{301}e\u{301}a\u{300} n\u{303}u.x\u{300}y cafe\u{301}.module.name /tmp/e\u{301}t\u{e9}".to_owned(),
            "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442} \u{3b1}\u{3b2}\u{3b3} \u{5e9}\u{5dc}\u{5d5}\u{5dd} \u{c548}\u{b155} git 0123abc fe80::1".to_owned(),
            multi_script(2_000, 7),
            multi_script(2_000, 0x5EED),
            "a\nb\n\nc d\n".to_owned(),
            String::new(),
        ]
    }

    /// The regex crate's spans of `pattern` in `text`.
    fn reference(pattern: &str, text: &str) -> Vec<Range<usize>> {
        regex::Regex::new(pattern)
            .unwrap()
            .find_iter(text)
            .map(|one| one.range())
            .collect()
    }

    /// The DFA's spans of `pattern` in `text`, or how it stopped.
    fn dfa_spans(dfa: &Dfa, text: &str) -> Result<Vec<Range<usize>>, DfaScan> {
        let mut spans = Vec::new();
        match dfa.scan(text, &mut |one, _| {
            spans.push(one);
            ControlFlow::Continue(())
        }) {
            DfaScan::Done => Ok(spans),
            other => Err(other),
        }
    }

    /// #518: wherever the DFA finishes, its spans are the regex crate's
    /// `find_iter` spans, for the built-in rules and the tricky patterns
    /// on every window class; it quits only for a Unicode word boundary
    /// on a window that is not ASCII, and never gives up here.
    #[test]
    fn the_dfa_finds_what_the_regex_crate_finds() {
        let patterns = DEFAULT_SMART_RULES.iter().map(|&(p, _)| p).chain(TRICKY);
        let mut compared = 0;
        for pattern in patterns {
            let dfa = Dfa::build(pattern).unwrap_or_else(|| panic!("{pattern} builds"));
            let unicode_word = dfa
                .regex
                .forward()
                .get_nfa()
                .look_set_any()
                .contains_word_unicode();
            for text in windows() {
                match dfa_spans(&dfa, &text) {
                    Ok(spans) => {
                        assert_eq!(spans, reference(pattern, &text), "{pattern} on {text:?}");
                        compared += 1;
                    }
                    Err(DfaScan::Quit(_)) => {
                        assert!(
                            unicode_word && !text.is_ascii(),
                            "{pattern} quit on {text:?}"
                        );
                    }
                    Err(other) => panic!("{pattern} on {text:?}: {other:?}"),
                }
            }
        }
        assert!(compared > 300, "{compared} comparisons");
    }

    /// The engines' spans, through whichever path a rule takes, are the
    /// regex crate's for the built-in rules on every window class: a
    /// Unicode word boundary on a non-ASCII window goes to the fallback.
    #[test]
    fn the_engines_select_what_the_regex_crate_selects() {
        let engines = SmartEngines::new(Selection::default().compiled_rules());
        assert_eq!(engines.rules().len(), DEFAULT_SMART_RULES.len());
        FALLBACK_RUNS.with(|runs| runs.set(0));
        for (index, rule) in engines.rules().rules().iter().enumerate() {
            for text in windows() {
                let mut spans = Vec::new();
                let scanned = engines.scan(index, &text, text.is_ascii(), |one, _| {
                    spans.push(one);
                    ControlFlow::Continue(())
                });
                assert_eq!(scanned, Scanned::Done, "rule {index} on {text:?}");
                assert_eq!(
                    spans,
                    reference(rule.pattern(), &text),
                    "rule {index} on {text:?}"
                );
            }
        }
        // 8 of the 15 rules have a Unicode `\b`; 6 of the 9 windows are not
        // ASCII (the empty one is).
        assert_eq!(FALLBACK_RUNS.with(std::cell::Cell::get), 8 * 6);
        assert!(!(0..engines.rules().len()).any(|index| engines.warned(index)));
    }

    /// #649 (round 2): the tricky patterns that load, and every assertion
    /// on its own beside a word, through [`SmartEngines::scan`] (the DFA,
    /// or on a window that is not ASCII the fallback's DFA then `PikeVM`)
    /// select what the regex crate selects on every window class.
    #[test]
    fn the_tricky_assertions_select_what_the_regex_crate_selects() {
        let assertions = [
            r"\b\w+\b",
            r"\B\w",
            r"\w\B",
            r"\b{start}\w",
            r"\w\b{end}",
            r"\b{start-half}\w+\b{end-half}",
            r"\<\w+\>",
            r"^\w+",
            r"\w+$",
            r"(?m)^\w",
            r"(?m)\w$",
            r"\A\w",
            r"\w\z",
            r"(?-u:\b)\w+(?-u:\b)",
            r"(?-u:\B)\w",
            r"(?i)\bk\w*",
            r"\b\p{Greek}+\b",
        ];
        let loads = |pattern: &str| {
            let mut selection = Selection::default();
            selection.smart_rules = Some(vec![SmartRule::new(pattern, Precision::Normal)]);
            selection.compiled_rules().len() == 1
        };
        let tricky: Vec<&str> = TRICKY.into_iter().filter(|p| loads(p)).collect();
        assert!(tricky.len() >= 20, "{tricky:?}");
        let patterns: Vec<&str> = tricky.into_iter().chain(assertions).collect();
        let engines = engines(&patterns);
        FALLBACK_RUNS.with(|runs| runs.set(0));
        let mut non_ascii = 0;
        for (index, pattern) in patterns.iter().enumerate() {
            for text in windows() {
                let (spans, scanned) = scan(&engines, index, &text);
                assert_eq!(scanned, Scanned::Done, "{pattern} on {text:?}");
                assert_eq!(spans, reference(pattern, &text), "{pattern} on {text:?}");
                non_ascii += usize::from(!text.is_ascii());
            }
        }
        assert!(non_ascii > 100, "{non_ascii} non-ASCII scans");
        assert!(
            FALLBACK_RUNS.with(std::cell::Cell::get) > 50,
            "the fallback ran on the Unicode word boundaries"
        );
        assert!(!(0..patterns.len()).any(|index| engines.warned(index)));
    }

    proptest! {
        /// The same over random text from an alphabet that holds word and
        /// non-word characters, ASCII and not, letters that fold
        /// (K, U+212A, s, ſ), combining marks, digits, dots, colons and
        /// slashes.
        #[test]
        fn the_dfa_agrees_on_random_text(
            text in proptest::collection::vec(
                proptest::sample::select(vec![
                    'a', 'b', 'k', 'K', '\u{212a}', 's', '\u{17f}', 'S', '\u{df}', 'e', '\u{301}',
                    '\u{e9}', '\u{65e5}', '\u{1f600}', '0', '1', '9', 'f', '.', ':', '/', ' ', '@',
                    '"', '\\', '_', '-', '\n', '\u{3b1}',
                ]),
                0..60,
            ).prop_map(|chars| chars.into_iter().collect::<String>()),
            pick in 0..(DEFAULT_SMART_RULES.len() + TRICKY.len()),
        ) {
            let pattern = DEFAULT_SMART_RULES
                .get(pick)
                .map_or_else(|| TRICKY[pick - DEFAULT_SMART_RULES.len()], |&(p, _)| p);
            let dfa = Dfa::build(pattern).unwrap();
            match dfa_spans(&dfa, &text) {
                Ok(spans) => prop_assert_eq!(spans, reference(pattern, &text)),
                Err(DfaScan::Quit(_)) => prop_assert!(!text.is_ascii()),
                Err(other) => prop_assert!(false, "{:?}", other),
            }
        }
    }

    /// Engines for `patterns`, loaded as the configuration loads them.
    /// `pre19.pat`'s text after its `\b[0-9]{2}[ab]`: 32 ASCII bytes and
    /// 32 two-byte characters, some escaped.
    const PRE19_TAIL: &str = "\u{80}\"\u{b6}\\$\u{e4}\\&\u{112}\\(\u{140}\\*\u{176},\u{1a4}\\.\u{1d2}0\u{200}2\u{236}4\u{264}6\u{292}8\u{2c0}:\u{2f6}<\u{324}>\u{352}@\u{380}B\u{3b6}D\u{3e4}F\u{412}H\u{440}J\u{476}L\u{4a4}N\u{4d2}P\u{500}R\u{536}T\u{564}V\u{592}X\u{5c0}Z\u{5f6}\\\\\u{624}\\^\u{652}`";

    /// The adversary's `pre19.pat`, 117 bytes.
    fn pre19() -> String {
        format!(r"\b[0-9]{{2}}[ab]{PRE19_TAIL}")
    }

    fn engines(patterns: &[&str]) -> SmartEngines {
        let mut selection = Selection::default();
        selection.smart_rules = Some(
            patterns
                .iter()
                .map(|&p| SmartRule::new(p, Precision::Normal))
                .collect(),
        );
        let rules = selection.compiled_rules();
        assert_eq!(rules.len(), patterns.len(), "every pattern loads");
        SmartEngines::new(rules)
    }

    /// The matches rule `index` reports on `text`, and how its scan ended.
    fn scan(engines: &SmartEngines, index: usize, text: &str) -> (Vec<Range<usize>>, Scanned) {
        let mut spans = Vec::new();
        let scanned = engines.scan(index, text, text.is_ascii(), |one, _| {
            spans.push(one);
            ControlFlow::Continue(())
        });
        (spans, scanned)
    }

    /// #518 L3: a rule whose search fills its cache gives up for that
    /// click (no match), warns once for its index and never again; and a
    /// search that a used cache cannot finish is retried on it cold, so the
    /// same text gives the same matches after the give-up as on fresh
    /// engines.
    #[test]
    fn a_rule_that_gives_up_selects_nothing_and_warns_once() {
        let thrash = r"(?:[^X]{0,50}){0,20}X|a";
        let long = "a".repeat(8192);
        let short = "ab X ok";
        let fresh = engines(&[thrash]);
        let (spans, scanned) = scan(&fresh, 0, short);
        assert_eq!(
            (spans.as_slice(), scanned),
            (&[Range { start: 0, end: 4 }][..], Scanned::Done)
        );
        assert!(!fresh.warned(0));

        let used = engines(&[thrash]);
        WARNINGS.with(|warnings| warnings.set(0));
        assert_eq!(
            scan(&used, 0, &long).1,
            Scanned::Nothing,
            "the long line fills the cache"
        );
        assert!(used.warned(0));
        assert_eq!(WARNINGS.with(std::cell::Cell::get), 1);
        assert_eq!(scan(&used, 0, &long).1, Scanned::Nothing);
        assert_eq!(
            WARNINGS.with(std::cell::Cell::get),
            1,
            "one warning per rule"
        );
        // The cache is full; the short line's search gives up on it, and
        // its cold retry finishes as on fresh engines.
        assert_eq!(
            scan(&used, 0, short),
            (vec![Range { start: 0, end: 4 }], Scanned::Done)
        );
        assert_eq!(WARNINGS.with(std::cell::Cell::get), 1);
    }

    /// The cost rule `index` reports for each match it finds on `text`.
    fn costs(engines: &SmartEngines, index: usize, text: &str) -> Vec<SearchCost> {
        let mut costs = Vec::new();
        let _ = engines.scan(index, text, text.is_ascii(), |_, cost| {
            costs.push(cost);
            ControlFlow::Continue(())
        });
        costs
    }

    /// #557: a scan reports its first search on a cold cache as costly and
    /// the later ones on that cache as cached, a search that resets a used
    /// cache as costly, and every match of the fallback as costly, so the
    /// click budget reads the clock after each of those.
    #[test]
    fn a_scan_reports_which_searches_may_have_been_costly() {
        use SearchCost::{Cached, Costly};
        let rule = engines(&["a"]);
        assert_eq!(costs(&rule, 0, "a a a"), [Costly, Cached, Cached], "cold");
        assert_eq!(costs(&rule, 0, "a a a"), [Cached, Cached, Cached], "warm");
        let thrash = engines(&[r"(?:[^X]{0,50}){0,20}X|a"]);
        // No ASCII in it, so the short line's search needs new states.
        let long: String = multi_script(8192, 1)
            .chars()
            .filter(|c| !c.is_ascii())
            .collect();
        assert_eq!(
            scan(&thrash, 0, &long).1,
            Scanned::Nothing,
            "the long line fills the cache"
        );
        // The short line's search gives up on the full cache, resets it and
        // runs again cold; on the cache it left, it runs cached.
        assert_eq!(costs(&thrash, 0, "ab X ok"), [Costly], "reset");
        assert_eq!(costs(&thrash, 0, "ab X ok"), [Cached], "warm again");
        let word = engines(&[r"\x{e9}\B"]);
        assert_eq!(
            costs(&word, 0, "\u{e9}\u{e9}\u{e9}"),
            [Costly, Costly],
            "the fallback"
        );
    }

    /// #518: the fallback runs on a window whose work is exactly
    /// [`SMART_FALLBACK_UNITS`] and not on one byte more. `\ba{243}` counts
    /// 250 units (249 states with captures, one of them a look, #687), so
    /// a 20 000-byte window is the gate.
    #[test]
    fn the_fallback_runs_up_to_the_gate_and_not_one_byte_past() {
        let rule = engines(&[r"\ba{243}"]);
        let at_gate = SMART_FALLBACK_UNITS / 250;
        assert_eq!(at_gate, 20_000);
        assert_eq!(rule.fallback_units(0, at_gate), Some(SMART_FALLBACK_UNITS));
        let window = |bytes: usize| {
            let mut text = format!("{} \u{e9}", "a".repeat(243));
            text.push_str(&" ".repeat(bytes - text.len()));
            text
        };
        WARNINGS.with(|warnings| warnings.set(0));
        assert_eq!(
            scan(&rule, 0, &window(at_gate)),
            (vec![Range { start: 0, end: 243 }], Scanned::Done),
            "at the gate"
        );
        assert!(!rule.warned(0));
        assert_eq!(
            scan(&rule, 0, &window(at_gate + 1)),
            (Vec::new(), Scanned::Nothing),
            "one byte past it"
        );
        assert_eq!(WARNINGS.with(std::cell::Cell::get), 1);
    }

    /// A class of the 64 even ASCII bytes, `\x00` to `\x7e`: one `Sparse`
    /// state of 64 one-byte transitions, which the `PikeVM` scans one by
    /// one (#687).
    fn sparse_class() -> String {
        let members = (0..0x80_u8)
            .step_by(2)
            .fold(String::new(), |mut members, byte| {
                let _ = write!(members, "\\x{byte:02x}"); // writing to a String cannot fail
                members
            });
        format!("(?-u:[{members}])")
    }

    /// A line of at most `bytes` bytes that is not ASCII: characters drawn
    /// from `chars` by `seed`, then `é`.
    fn line_from(chars: &str, bytes: usize, mut seed: u64) -> String {
        let chars: Vec<char> = chars.chars().collect();
        let mut text = String::new();
        loop {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let c = chars[usize::try_from(seed % chars.len() as u64).unwrap()];
            if text.len() + c.len_utf8() + '\u{e9}'.len_utf8() > bytes {
                break;
            }
            text.push(c);
        }
        text.push('\u{e9}');
        text
    }

    /// #687: the gate counts what a `PikeVM` search costs per byte, not
    /// states alone: a state is one unit, a `Sparse` state (a class of
    /// many ranges, scanned one by one) a sixteenth more for each
    /// transition, and a look-around state (a Unicode word boundary
    /// decodes characters on both sides) two. Sixteen more repeats add 16
    /// units of `a`, 16 x 5 of a class of 64 one-byte members, 16 x 18 / 16
    /// of a class of two, and 16 x 3 of `\Ba`.
    #[test]
    fn the_gate_counts_a_class_by_its_transitions_and_a_look_twice() {
        let work = |pattern: &str| {
            engines(&[pattern])
                .fallback_units(0, 1)
                .expect("it may fall back")
        };
        let grown = |unit: &str| {
            work(&format!(r"\b(?:{unit}){{32}}")) - work(&format!(r"\b(?:{unit}){{16}}"))
        };
        assert_eq!(grown("a"), 16, "one byte");
        assert_eq!(grown(&sparse_class()), 16 * 5, "64 one-byte members");
        assert_eq!(grown("(?-u:[ac])"), 18, "two members");
        assert_eq!(grown(r"\Ba"), 16 * 3, "a look and a byte");
        // The whole program: `\ba{243}` has 249 states with captures and
        // one look, 250 units.
        assert_eq!(work(r"\ba{243}"), 250);
    }

    /// #687: the rule of the issue, `\b C*bC{990}` with C a class of 64
    /// one-byte members, took about 120 ms a search (150 ms a click) on
    /// 8000 characters of random `b` and `z` and an `é`, though its
    /// states times bytes (7.99 M) were under the old gate. Its work now
    /// counts each class five times: over the gate, it selects nothing at
    /// once, without running the fallback, and warns once; on a line of
    /// 1000 bytes it is within the gate and finds what the regex crate
    /// finds.
    #[test]
    fn a_wide_sparse_class_is_over_the_gate_where_its_search_would_outlast_an_attempt() {
        let class = sparse_class();
        let pattern = format!(r"\b{class}*b{class}{{990}}");
        let rule = engines(&[&pattern]);
        let long = line_from("bz", 8002, 7);
        assert!(long.len() >= 8000, "{}", long.len());
        assert!(
            rule.fallback_units(0, long.len())
                .is_some_and(|units| units > SMART_FALLBACK_UNITS)
        );
        FALLBACK_RUNS.with(|runs| runs.set(0));
        WARNINGS.with(|warnings| warnings.set(0));
        let started = crate::thread_cpu_time();
        for _ in 0..3 {
            assert_eq!(scan(&rule, 0, &long), (Vec::new(), Scanned::Nothing));
        }
        let spent = crate::thread_cpu_time().saturating_sub(started);
        assert_eq!(FALLBACK_RUNS.with(std::cell::Cell::get), 0, "no fallback");
        assert_eq!(WARNINGS.with(std::cell::Cell::get), 1, "one warning");
        assert!(rule.warned(0));
        eprintln!("three clicks over the gate: {spent:?}");
        #[cfg(not(debug_assertions))]
        assert!(spent < Duration::from_millis(3), "{spent:?}");
        let short = format!("{}\u{e9}", "b".repeat(995));
        assert!(
            rule.fallback_units(0, short.len())
                .is_some_and(|units| units <= SMART_FALLBACK_UNITS)
        );
        let spans = reference(&pattern, &short);
        assert_eq!(spans, vec![Range { start: 0, end: 995 }]);
        assert_eq!(scan(&rule, 0, &short), (spans, Scanned::Done));
        assert_eq!(FALLBACK_RUNS.with(std::cell::Cell::get), 1, "it fell back");
    }

    /// #687: one fallback search on the widest window the gate admits
    /// takes at most [`SMART_RULE_ATTEMPT_BOUND`], for programs whose every
    /// state is live at every byte: a run of one-byte states, classes of
    /// 64 one-byte members, a Unicode `\B` before each byte (ASCII and
    /// Latin), an alternation and optional bytes, and a wide alternation of
    /// long branches; plus the rule of the issue. Each scan is one search
    /// that finds nothing (the trailing `c` never comes) or, for the issue's
    /// rule, a few. On the thread's CPU clock, the fastest of three.
    /// Release builds only: the bound is for the shipped code. Measured on
    /// a 4+6-core M-series laptop at a load of about 10 (#687): the run of
    /// one-byte states, the most per unit, about 35 ms; each other family
    /// under 30 ms. Before #687 (8 000 000 states times bytes) the classes
    /// of 64 members took about 210 ms and the run 55 ms.
    #[cfg(not(debug_assertions))]
    #[test]
    fn hostile_smart_rules_at_the_fallback_gate_take_one_attempt_at_most() {
        let class = sparse_class();
        // Branches of different lengths: regex-syntax lifts a common
        // prefix out of an alternation.
        let wide: Vec<String> = (300..320).map(|i| format!("[ab]{{{i}}}c")).collect();
        let families = [
            (r"\b[ab]*[ab]{990}c".to_owned(), "ab", "one-byte states"),
            (
                format!(r"\b{class}*{class}{{990}}c"),
                "bz",
                "64-member classes",
            ),
            (
                format!(r"\b{class}*b{class}{{990}}"),
                "bz",
                "the rule of #687",
            ),
            (
                r"\b[ab]*(?:\B[ab]){990}c".to_owned(),
                "ab",
                r"\B before each byte",
            ),
            (
                r"\b[\u{e9}\u{f1}]*(?:\B[\u{e9}\u{f1}]){600}c".to_owned(),
                "\u{e9}\u{f1}",
                r"\B before é and ñ",
            ),
            (
                r"\b[ab]*(?:[ab]|[ab]a|[ab]b|[ab]aa){500}c".to_owned(),
                "ab",
                "an alternation",
            ),
            (r"\b[ab]*(?:[ab]?){990}c".to_owned(), "ab", "optional bytes"),
            (
                format!(r"\b[ab]*(?:{})", wide.join("|")),
                "ab",
                "a wide alternation",
            ),
        ];
        for (pattern, chars, family) in &families {
            let rule = engines(&[pattern]);
            let work = rule.fallback_units(0, 1).expect("it may fall back");
            let text = line_from(chars, SMART_FALLBACK_UNITS / work, 0x687);
            let units = rule.fallback_units(0, text.len()).unwrap();
            assert!(units <= SMART_FALLBACK_UNITS, "{family}: {units} units");
            assert!(
                units > SMART_FALLBACK_UNITS - 4 * work,
                "{family}: at the gate"
            );
            let spans = reference(pattern, &text);
            let mut fastest = Duration::MAX;
            for _ in 0..3 {
                FALLBACK_RUNS.with(|runs| runs.set(0));
                let started = crate::thread_cpu_time();
                let scanned = scan(&rule, 0, &text);
                fastest = fastest.min(crate::thread_cpu_time().saturating_sub(started));
                assert_eq!(scanned, (spans.clone(), Scanned::Done), "{family}");
                assert_eq!(FALLBACK_RUNS.with(std::cell::Cell::get), 1, "{family}");
            }
            eprintln!(
                "{family}: {work} units a byte, {} bytes, fastest {fastest:?}, {} matches",
                text.len(),
                spans.len()
            );
            assert!(
                fastest <= SMART_RULE_ATTEMPT_BOUND,
                "{family}: fastest scan {fastest:?}, over {SMART_RULE_ATTEMPT_BOUND:?}"
            );
        }
    }

    /// #649 B2: a rule whose program raises its DFA's cache past
    /// [`SMART_RULE_CACHE_BYTES`] (`\b\w{20}`: its reverse program has
    /// about 28 700 states) loads, warns once when its engines are built,
    /// matches wherever its DFA completes (every ASCII line), and on a line
    /// that is not ASCII selects nothing at once: no fallback, no warning
    /// per click. A smaller rule of the same kind keeps its fallback.
    #[test]
    fn a_rule_with_a_raised_cache_has_no_fallback() {
        WARNINGS.with(|warnings| warnings.set(0));
        let rule = engines(&[r"\b\w{20}"]);
        assert!(rule.has_dfa(0), "its DFA builds");
        assert!(rule.barred_from_fallback(0));
        assert_eq!(WARNINGS.with(std::cell::Cell::get), 1, "one warning");
        let word = "abcdefghijklmnopqrst";
        let ascii = format!("x {word} {word}y {word}");
        assert_eq!(
            scan(&rule, 0, &ascii),
            (vec![2..22, 23..43, 45..65], Scanned::Done),
            "an ASCII line"
        );
        assert_eq!(reference(r"\b\w{20}", &ascii), [2..22, 23..43, 45..65]);
        FALLBACK_RUNS.with(|runs| runs.set(0));
        // The short line is within the fallback's units and slot gates
        // (6331 states x 23 bytes): only the raised cache bars it.
        let short = format!("\u{e9} {word}");
        let cost = rule.engines[0].fallback_states(r"\b\w{20}").unwrap();
        assert!(cost.within(short.len()), "{} states", cost.states);
        let accented = format!("\u{e9} {word} {}", "\u{e9}a".repeat(4000));
        let started = Instant::now();
        for line in [&short, &accented].repeat(4) {
            assert_eq!(
                scan(&rule, 0, line),
                (Vec::new(), Scanned::Nothing),
                "a line that is not ASCII"
            );
        }
        assert!(started.elapsed() < Duration::from_secs(1), "no stall");
        assert_eq!(FALLBACK_RUNS.with(std::cell::Cell::get), 0, "no fallback");
        assert_eq!(WARNINGS.with(std::cell::Cell::get), 1, "no more warnings");
        let smaller = engines(&[r"\b\w{5}"]);
        assert!(!smaller.barred_from_fallback(0));
        assert_eq!(
            scan(&smaller, 0, "\u{e9} abcde"),
            (vec![Range { start: 3, end: 8 }], Scanned::Done),
            "the smaller rule falls back"
        );
        assert_eq!(FALLBACK_RUNS.with(std::cell::Cell::get), 1);
    }

    /// The adversary's literal set (#649 cycle 2, `pre19.pat`): 200 exact
    /// literals of 100 bytes, whose prefilter is an Aho-Corasick DFA of
    /// 19 MiB. The rule loads, its DFA keeps no prefilter and it matches
    /// as the regex crate does; the built-in rules and the bench's rules
    /// keep theirs, within [`SMART_PREFILTER_BYTES`].
    #[test]
    fn a_large_literal_prefilter_is_dropped() {
        let uncapped = |pattern: &str| {
            let syntax = syntax::Config::new().nest_limit(SMART_RULE_NEST_LIMIT);
            let hir = syntax::parse_with(pattern, &syntax).unwrap();
            Prefilter::from_hirs_prefix(MatchKind::LeftmostFirst, &[&hir])
                .map(|prefilter| prefilter.memory_usage())
        };
        let pattern = pre19();
        assert_eq!(pattern.len(), 117);
        let rule = engines(&[&pattern]);
        assert!(rule.has_dfa(0), "its DFA builds");
        let built = uncapped(&pattern).unwrap();
        assert!(built > 19 << 20, "{built} bytes");
        assert_eq!(rule.prefilter_bytes(0), None, "its prefilter is dropped");
        // The tail as text: each escaped character stands for itself.
        let mut line = "x 12a".to_owned();
        let mut escaped = false;
        for c in PRE19_TAIL.chars() {
            escaped = !escaped && c == '\\';
            if !escaped {
                line.push(c);
            }
        }
        line.push_str(" end");
        let (found, scanned) = scan(&rule, 0, &line);
        assert_eq!(found, reference(&pattern, &line));
        assert_eq!((found.len(), scanned), (1, Scanned::Done));
        let bench = [
            r"\b[A-Z]{2,}-\d+\b",
            r"\b[0-9a-f]{7,40}\b",
            r"\b[a-z0-9-]+-[a-z0-9]{5}\b",
            r"\bv?\d+\.\d+\.\d+(?:-[\w.]+)?\b",
        ];
        let patterns: Vec<&str> = DEFAULT_SMART_RULES
            .iter()
            .map(|&(pattern, _)| pattern)
            .chain(bench)
            .collect();
        let kept = engines(&patterns);
        let mut with = 0;
        for (index, pattern) in patterns.iter().enumerate() {
            let built = uncapped(pattern);
            assert_eq!(kept.prefilter_bytes(index), built, "{pattern}");
            assert!(built.is_none_or(|bytes| bytes <= SMART_PREFILTER_BYTES));
            with += usize::from(built.is_some());
        }
        assert!(with > 0, "some rules have a prefilter");
    }

    /// #590 m3: a DFA quit on a rule without a Unicode word boundary cannot
    /// happen (such a DFA has no byte to quit on), so it is forced here: an
    /// engine told its `\b` rule has none runs the DFA on text that is not
    /// ASCII. The rule then selects nothing for that click, without the
    /// fallback, and its one warning says its DFA quit, not that a fallback
    /// was over the gate.
    #[test]
    fn a_dfa_quit_without_a_fallback_says_so_and_selects_nothing() {
        let real = engines(&[r"\bx"]);
        let forced = SmartEngines {
            rules: Arc::clone(&real.rules),
            engines: Arc::from([RuleEngine {
                unicode_word: false,
                ..RuleEngine::new(0, r"\bx")
            }]),
        };
        let text = "x \u{e9} x";
        FALLBACK_RUNS.with(|runs| runs.set(0));
        captures_see_every_event();
        let logged = crate::session::tests::warnings(|| {
            assert_eq!(scan(&forced, 0, text).1, Scanned::Nothing);
            assert_eq!(scan(&forced, 0, text).1, Scanned::Nothing, "again");
        });
        assert_eq!(FALLBACK_RUNS.with(std::cell::Cell::get), 0, "no fallback");
        assert!(forced.warned(0));
        assert_eq!(logged.len(), 1, "one warning per rule: {logged:?}");
        assert!(
            logged[0].contains("rule=0")
                && logged[0].contains(r#"why="its DFA quit and it has no fallback""#),
            "{logged:?}"
        );
        // The real engine takes the fallback there and finds both.
        assert_eq!(scan(&real, 0, text), (vec![0..1, 5..6], Scanned::Done));
    }

    /// Makes the scoped subscriber of [`crate::session::tests::warnings`]
    /// see every event when tests share a process (`cargo test`), as
    /// `config_watch`'s tests do: a process-wide subscriber interested in
    /// every callsite and enabled for none. Without it a callsite first hit
    /// by a thread with no subscriber is cached as of no interest (#430).
    fn captures_see_every_event() {
        struct Interested;

        impl tracing::Subscriber for Interested {
            fn register_callsite(
                &self,
                _: &'static tracing::Metadata<'static>,
            ) -> tracing::subscriber::Interest {
                tracing::subscriber::Interest::sometimes()
            }
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                false
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, _: &tracing::Event<'_>) {}
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // Fails only if a global subscriber is set already, which then
            // decides; nothing in this crate sets one.
            let _ = tracing::subscriber::set_global_default(Interested);
        });
        // A callsite whose interest was cached before the line above.
        tracing::callsite::rebuild_interest_cache();
    }

    /// #590: the fallback's slot tables may take exactly
    /// [`SMART_FALLBACK_SLOT_BYTES`] and not one state more, whatever the
    /// window; the window gate still applies within it.
    #[test]
    fn the_slot_gate_holds_at_its_cap_and_not_one_state_past() {
        let at_cap = FallbackCost {
            states: 256,
            slots: 128,
            work: 256,
        };
        assert_eq!(at_cap.slot_bytes(), SMART_FALLBACK_SLOT_BYTES);
        assert!(at_cap.within(1), "at the cap");
        assert!(
            at_cap.within(SMART_FALLBACK_UNITS / 256),
            "and the window gate"
        );
        assert!(
            !at_cap.within(SMART_FALLBACK_UNITS / 256 + 1),
            "past the window gate"
        );
        let past = FallbackCost {
            states: 257,
            ..at_cap
        };
        assert!(!past.within(1), "one state past the cap");
        let wide = FallbackCost {
            slots: 129,
            ..at_cap
        };
        assert!(!wide.within(1), "one slot past the cap");
    }

    /// #590: the gate counts a real rule's program as the regex crate runs
    /// it. Two groups repeated 681 times (5455 states, 6 slots) stay under
    /// [`SMART_FALLBACK_SLOT_BYTES`] and fall back on a non-ASCII line; one
    /// repeat more (5463 states) is over it and selects nothing, with one
    /// warning.
    #[test]
    fn a_real_rule_falls_back_just_under_the_slot_cap_and_not_just_over() {
        let pattern = |repeats: usize| format!(r"\bx(?:(a?)(a?)){{{repeats}}}");
        let text = "\u{e9}x \u{e9} xa x\u{e9}";
        let under = engines(&[&pattern(681)]);
        let over = engines(&[&pattern(682)]);
        assert_eq!(under.fallback_slot_bytes(0), Some(5455 * 6 * 16));
        assert_eq!(over.fallback_slot_bytes(0), Some(5463 * 6 * 16));
        FALLBACK_RUNS.with(|runs| runs.set(0));
        WARNINGS.with(|warnings| warnings.set(0));
        assert_eq!(
            scan(&under, 0, text),
            (reference(&pattern(681), text), Scanned::Done)
        );
        assert_eq!(reference(&pattern(681), text).len(), 2, "it matches");
        assert_eq!(FALLBACK_RUNS.with(std::cell::Cell::get), 1, "it fell back");
        assert!(!under.warned(0));
        assert_eq!(scan(&over, 0, text), (Vec::new(), Scanned::Nothing));
        assert_eq!(FALLBACK_RUNS.with(std::cell::Cell::get), 1, "it did not");
        assert_eq!(WARNINGS.with(std::cell::Cell::get), 1, "one warning");
        assert!(over.warned(0));
    }

    /// #590 (B1 of the 590e review): a 1011-byte rule within the window
    /// gate whose slot tables would take about 177 MiB (23 007 states, 502
    /// slots) is over the slot gate: on a 120-column line of `éx` (within
    /// the window gate since #687 only up to 217 bytes) its click
    /// selects nothing from it, with one over-the-gate warning, and the
    /// regex crate never runs, so nothing is kept for it in that crate's
    /// cache pool; what the rule keeps is its own DFA cache, within its cap.
    #[test]
    fn a_rule_over_the_slot_gate_selects_nothing_and_keeps_no_fallback_cache() {
        let pattern = format!(r"\bx(?:{}){{23}}", "(a?)".repeat(250));
        assert_eq!(pattern.len(), 1011);
        let rule = engines(&[&pattern]);
        let text = "\u{e9}x".repeat(60);
        assert!(
            rule.fallback_units(0, text.len())
                .is_some_and(|units| units <= SMART_FALLBACK_UNITS),
            "within the window gate"
        );
        let slot_bytes = rule.fallback_slot_bytes(0).unwrap();
        assert!(slot_bytes > 64 * SMART_FALLBACK_SLOT_BYTES, "{slot_bytes}");
        FALLBACK_RUNS.with(|runs| runs.set(0));
        WARNINGS.with(|warnings| warnings.set(0));
        for _ in 0..3 {
            assert_eq!(scan(&rule, 0, &text), (Vec::new(), Scanned::Nothing));
        }
        assert_eq!(
            FALLBACK_RUNS.with(std::cell::Cell::get),
            0,
            "no fallback ran"
        );
        assert_eq!(WARNINGS.with(std::cell::Cell::get), 1, "one warning");
        assert!(rule.warned(0));
        assert!(rule.cache_bytes(0) <= 2 * SMART_RULE_CACHE_BYTES + 64 * 1024);
    }

    /// #590: no built-in rule is refused by the slot gate (the largest
    /// takes 21 KiB, over 16 times under it), and each falls back and finds what the regex crate
    /// finds on the widest window a click scans
    /// ([`crate::selection::SMART_SCAN_BYTES`]) of CJK and emoji text with
    /// paths, addresses and URLs in it, and on one of CJK alone.
    #[test]
    fn every_built_in_rule_falls_back_on_the_widest_cjk_window() {
        let engines = SmartEngines::new(Selection::default().compiled_rules());
        let widest = |unit: &str| {
            let mut text = String::new();
            for c in unit.chars().cycle() {
                if text.len() + c.len_utf8() > crate::selection::SMART_SCAN_BYTES {
                    break;
                }
                text.push(c);
            }
            text
        };
        let windows = [
            widest(
                "\u{65e5}\u{672c}\u{8a9e} user@example.com \u{1f600} /usr/lib/x.so 10.0.0.1 \
                 https://a.b/c?d=1 std::vector<int> 0123abcd fe80::1 mailto:a@b.co \
                 \"q\" ssh://h/x [a b:c] <x.h> \u{1f44d}\u{1f3fd}",
            ),
            widest("\u{65e5}\u{672c}\u{8a9e}\u{1f600}"),
        ];
        FALLBACK_RUNS.with(|runs| runs.set(0));
        let mut matched = 0;
        for (index, rule) in engines.rules().rules().iter().enumerate() {
            assert!(!engines.barred_from_fallback(index), "rule {index}");
            let slot_bytes = engines.fallback_slot_bytes(index);
            assert!(
                slot_bytes.is_none_or(|bytes| bytes <= SMART_FALLBACK_SLOT_BYTES / 16),
                "rule {index}: {slot_bytes:?}"
            );
            for text in &windows {
                assert!(text.len() > crate::selection::SMART_SCAN_BYTES - 4);
                let spans = reference(rule.pattern(), text);
                matched += spans.len();
                assert_eq!(
                    scan(&engines, index, text),
                    (spans, Scanned::Done),
                    "rule {index}"
                );
            }
        }
        assert!(matched > 1000, "{matched} matches");
        // #687: the largest work, the compressed IPv6 rule's, on the widest
        // window.
        let most = (0..engines.rules().len())
            .filter_map(|index| engines.fallback_units(index, crate::selection::SMART_SCAN_BYTES))
            .max();
        eprintln!("the most work of a built-in rule: {most:?}");
        assert!(
            most.is_some_and(|units| units <= SMART_FALLBACK_UNITS),
            "{most:?}"
        );
        // 8 of the 15 rules have a Unicode `\b`: each fell back on both.
        assert_eq!(FALLBACK_RUNS.with(std::cell::Cell::get), 8 * 2);
        assert!(!(0..engines.rules().len()).any(|index| engines.warned(index)));
    }

    /// S5: a rule's DFA cache holds at most [`SMART_RULE_CACHE_BYTES`] in
    /// each direction (plus the small fixed part of a cache), however many
    /// thrashing clicks fill it.
    #[test]
    fn a_rule_cache_stays_within_its_cap() {
        let engines = engines(&[r"(?:[^X]{0,50}){0,20}X|a", r"\p{L}{0,20}X|a"]);
        let texts = [
            "a".repeat(8192),
            multi_script(8192, 1),
            multi_script(8192, 2),
            "\u{e9}".repeat(8192),
        ];
        for index in 0..2 {
            for text in texts.iter().cycle().take(12) {
                let _ = scan(&engines, index, text);
                let bytes = engines.cache_bytes(index);
                assert!(
                    bytes <= 2 * SMART_RULE_CACHE_BYTES + 64 * 1024,
                    "rule {index}: {bytes} bytes"
                );
            }
        }
    }

    /// The ceiling the docs give (#649) is the one these constants make:
    /// every term and total, in this file's doc, ADR 0042 and S5.
    #[test]
    fn the_documented_ceiling_is_the_constants() {
        const KIB: usize = 1024;
        const MIB: usize = 1024 * 1024;
        // MiB to the nearest hundredth, trailing zeros dropped.
        let mib = |bytes: usize| {
            let hundredths = (bytes * 100 + MIB / 2) / MIB;
            let text = format!("{}.{:02}", hundredths / 100, hundredths % 100);
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        };
        // The same to the nearest thousandth.
        let mib3 = |bytes: usize| {
            let thousandths = (bytes * 1000 + MIB / 2) / MIB;
            let text = format!("{}.{:03}", thousandths / 1000, thousandths % 1000);
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        };
        let own = RULE_DFA_BYTES;
        let vm = FALLBACK_MAX_STATES * FALLBACK_STATE_BYTES;
        let programs = RULE_PROGRAM_BYTES;
        let prefilter = RULE_PREFILTER_BYTES;
        assert_eq!(own + vm + programs + prefilter, RULE_CEILING);
        let states = format!(
            "{} {:03}",
            FALLBACK_MAX_STATES / 1000,
            FALLBACK_MAX_STATES % 1000
        );
        let phrases = [
            format!(
                "2 x {}.{:02} x {} KiB = {} MiB",
                DFA_ALLOC_HUNDREDTHS / 100,
                DFA_ALLOC_HUNDREDTHS % 100,
                SMART_RULE_CACHE_BYTES / KIB,
                mib(own)
            ),
            format!(
                "{states} states of {FALLBACK_STATE_BYTES} bytes ({} MiB)",
                mib(vm)
            ),
            format!(
                "2 x {} MiB = {} MiB",
                mib(SMART_RULE_SIZE_LIMIT),
                mib(RULE_PROGRAM_BYTES)
            ),
            format!(
                "{} + {} KiB = {} MiB",
                SMART_PREFILTER_BYTES / KIB,
                (RULE_PREFILTER_BYTES - SMART_PREFILTER_BYTES) / KIB,
                mib3(prefilter)
            ),
            format!(
                "{} + {} + {} + {} = {} MiB per rule",
                mib(own),
                mib(vm),
                mib(programs),
                mib3(prefilter),
                mib3(RULE_CEILING)
            ),
            format!(
                "{MAX_SMART_RULES} x {} = {} MiB",
                mib3(RULE_CEILING),
                mib(SMART_CACHE_CEILING)
            ),
            "adds 0 MiB".to_owned(),
        ];
        assert_eq!(phrases[5], "64 x 5.996 = 383.76 MiB");
        // The docs are read at run time: xtask ab's base archive copies only
        // what crates include at build time.
        let doc = |path: &str| {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(path);
            std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        };
        // Words only: a phrase may wrap, in a doc comment too.
        let words = |text: &str| {
            text.split_whitespace()
                .filter(|word| *word != "///")
                .collect::<Vec<_>>()
                .join(" ")
        };
        let docs = [
            ("smart_engine.rs", words(include_str!("smart_engine.rs"))),
            (
                "ADR 0042",
                words(&doc("docs/adr/0042-smart-selection-and-rich-copy.md")),
            ),
            ("security.md", words(&doc("docs/security.md"))),
        ];
        for (name, text) in &docs {
            for phrase in &phrases {
                assert!(text.contains(phrase.as_str()), "{name} lacks {phrase:?}");
            }
        }
    }

    /// No built-in rule gives up in its DFA at [`SMART_RULE_CACHE_BYTES`]
    /// (#649, 1 MiB before): searched from every start a scan searches
    /// from, on the widest windows of ASCII and of other scripts, each
    /// search finds what the regex crate finds, or quits on a byte that is
    /// not ASCII (a Unicode-`\b` rule, which then runs its `PikeVM`). A
    /// rule whose DFA has 2^15 states of 64 byte classes gives up there, so
    /// the search would show one.
    #[test]
    fn no_built_in_rule_gives_up_in_its_dfa() {
        let others: String = ('c'..='z')
            .chain('C'..='Z')
            .chain('0'..='9')
            .flat_map(|c| ['|', c, c])
            .collect();
        let mut seed = 7_u64;
        let line: String = (0..8_000)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                if seed & 1 == 0 { 'a' } else { 'b' }
            })
            .collect();
        let filler = Dfa::build(&format!(r"(?:[ab]*a[ab]{{14}}{others})")).unwrap();
        let mut held = filler.lock();
        let gave_up = filler.regex.try_search(&mut held.cache, &Input::new(&line));
        assert!(
            gave_up
                .as_ref()
                .is_err_and(|error| matches!(error.kind(), MatchErrorKind::GaveUp { .. })),
            "the filler gives up: {gave_up:?}"
        );
        drop(held);
        let widest = |unit: &str, tail: &str| {
            let mut text = String::new();
            for c in unit.chars().cycle() {
                if text.len() + c.len_utf8() + tail.len() > crate::selection::SMART_SCAN_BYTES {
                    break;
                }
                text.push(c);
            }
            text + tail
        };
        let ascii = "user@example.com /usr/lib/x.so 10.0.0.1 https://a.b/c?d=1 \
                     std::vector<int> 0123abcd fe80::1 mailto:a@b.co \"q\" \
                     ssh://h/x [a b:c] <x.h> a.b.c::d \"x\\\"y\" @selector(a:) ";
        let windows = [
            widest(ascii, ""),
            widest(&format!("\u{65e5}\u{672c}\u{8a9e} {ascii}\u{1f600} "), ""),
            widest("\u{65e5}\u{672c}\u{8a9e}\u{1f600}", ""),
            widest(ascii, "\u{e9}"),
            multi_script(8_000, 7),
            multi_script(8_000, 0x5EED),
        ];
        let mut searched = 0;
        let mut quits = 0;
        for pattern in DEFAULT_SMART_RULES.iter().map(|rule| rule.0) {
            let dfa = Dfa::build(pattern).unwrap_or_else(|| panic!("{pattern}: no DFA"));
            let mut held = dfa.lock();
            for text in &windows {
                let spans = reference(pattern, text);
                let starts = std::iter::once(0).chain(spans.iter().map(|span| span.end));
                for start in starts.filter(|&start| start <= text.len()) {
                    searched += 1;
                    let input = Input::new(text.as_str()).range(start..);
                    match dfa.regex.try_search(&mut held.cache, &input) {
                        Ok(one) => assert_eq!(
                            one.map(|one| one.range()),
                            spans.iter().find(|span| span.start >= start).cloned(),
                            "{pattern} from {start}"
                        ),
                        Err(error) => {
                            assert!(
                                !matches!(error.kind(), MatchErrorKind::GaveUp { .. }),
                                "{pattern} gave up from {start}: {error}"
                            );
                            quits += 1;
                        }
                    }
                }
            }
        }
        assert!(
            searched > 1000 && quits > 0,
            "{searched} searches, {quits} quits"
        );
    }
}
