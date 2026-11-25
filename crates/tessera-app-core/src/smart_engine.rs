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
//! at most one reset, a few milliseconds, within
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
//!    cache that earlier searches had used, the cache is reset and that
//!    search runs once more on it cold; a second give-up is the rule's for
//!    that click: no match from it, and one warning per rule index
//!    carrying the index only (S8).
//! 3. A rule that skipped its DFA, or a Unicode-`\b` rule whose DFA quit,
//!    runs the regex crate's `find_iter` instead, only when its compiled
//!    program's states times the window's bytes are at most
//!    [`SMART_FALLBACK_UNITS`] (that engine costs at most a few nanoseconds
//!    per state and byte) and its states times its capture slots fit
//!    [`SMART_FALLBACK_SLOT_BYTES`]; past either the rule has no match for
//!    that click, with one warning per rule index. A rule without a Unicode
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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use regex::Regex;
use regex_automata::hybrid::{dfa, regex as hybrid};
use regex_automata::nfa::thompson;
use regex_automata::util::iter::Searcher;
use regex_automata::util::prefilter::Prefilter;
use regex_automata::util::syntax;
use regex_automata::{Input, MatchErrorKind, MatchKind};
use tessera_config::{CompiledRule, SMART_RULE_NEST_LIMIT, SMART_RULE_SIZE_LIMIT, SmartRules};

/// The most CPU time one match attempt of a smart-selection rule takes
/// (S5, S6, #518): one search of the scan window, which fills the rule's
/// DFA cache at most twice (a warm cache that fills is reset and the search
/// retried cold) or runs the fallback within [`SMART_FALLBACK_UNITS`].
/// About 25 ms was the worst seen on a loaded machine, over thousands of
/// random patterns. With the click budget
/// ([`crate::selection::SMART_RULE_BUDGET`]) checked after each costly
/// search (the first on a cold cache, a reset, or the fallback) and every
/// [`crate::selection::SMART_CLOCK_STRIDE`] matches, a rule costs a click
/// at most the budget plus this, plus fewer than that stride of searches
/// on cached states.
pub const SMART_RULE_ATTEMPT_BOUND: Duration = Duration::from_millis(50);

/// The most work a smart-selection rule may give the regex crate's own
/// engine in one click (#518): its compiled program's states (capture
/// groups included) times the scan window's bytes. Past it the rule has no
/// match for that click. 8 000 000 cost at most 22 ms measured, and every
/// built-in rule stays under it on a full 32 KiB window (the largest, 131
/// states, at 4.3 M).
pub const SMART_FALLBACK_UNITS: usize = 8_000_000;

/// The most memory the regex crate's `PikeVM` may take for a smart-selection
/// rule's fallback in its slot tables (S5, #590): its compiled program's
/// states (capture groups included) times its capture slots times 16 bytes
/// (two tables of 8-byte slots, as regex-automata sizes them when the
/// fallback first runs, kept in the regex crate's cache pool), leaving out
/// only the table's few extra slots for one set of captures. Past it the
/// rule has no match for a click that would fall back. The table is
/// quadratic in the pattern: 250 groups repeated 23 times in a 1011-byte
/// pattern would take 177 MiB. No built-in rule has a capture group: each
/// takes at most 21 KiB, about 25 times under the cap.
pub const SMART_FALLBACK_SLOT_BYTES: usize = 512 * 1024;

/// The bytes one smart-selection rule's DFA may cache in each direction
/// (S5, #518): [`tessera_config::SMART_RULE_SIZE_LIMIT`], 1 MiB, as the
/// regex crate's own lazy DFA had. Kept across clicks, forward and reverse,
/// so 64 rules' DFAs hold at most 128 MiB once every one has filled both.
///
/// That is not the whole ceiling (ADR 0042, #590): a rule that takes the
/// fallback also keeps the regex crate's own caches in that crate's pool,
/// one set per thread that clicks: its lazy DFA's 1 MiB per direction, the
/// bounded backtracker's 256 KiB, the `PikeVM`'s slot tables within
/// `SMART_FALLBACK_SLOT_BYTES` (512 KiB) and its sparse sets and stack
/// (about 32 bytes per state, at most 16 384 states within that cap),
/// about 3.3 MiB by those limits. A rule then keeps at most about 5.3 MiB,
/// and 64 rules with one clicking thread about 128 + 64 x 3.3 = 336 MiB.
/// That figure assumes one clicking thread and leaves out the regex crate's
/// reverse-inner lazy DFA cache, which is not yet bounded (#649).
/// Measured (release, kept heap after the clicks): at most 0.93 MiB of a
/// rule's own DFA caches, 1.10 MiB of the regex crate's for a rule at the
/// slot cap (5455 states) and 0.14 MiB for a built-in rule; 64 rules at the
/// cap kept 70.5 MiB and 64 thrashing rules 95.6 MiB.
pub const SMART_RULE_CACHE_BYTES: usize = SMART_RULE_SIZE_LIMIT;

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
    /// Shared with the engines of an equal set loaded later
    /// ([`SmartEngines::for_rules`]).
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
            .map(|(index, rule)| RuleEngine::new(index, rule.regex().as_str()))
            .collect();
        Self { rules, engines }
    }

    /// The rules these engines are for.
    pub(crate) fn rules(&self) -> &SmartRules {
        &self.rules
    }

    /// The engines of `rules`: these when they are for that compiled set;
    /// these rules' engines, caches and warnings kept, when `rules` holds
    /// the same patterns with the same precisions and checks in the same
    /// order (a reload that changed no rule compiles them again, #518);
    /// new ones otherwise.
    pub(crate) fn for_rules(self: &Arc<Self>, rules: Arc<SmartRules>) -> Arc<Self> {
        if Arc::ptr_eq(&self.rules, &rules) {
            return Arc::clone(self);
        }
        let same = |a: &CompiledRule, b: &CompiledRule| {
            a.regex().as_str() == b.regex().as_str()
                && a.precision() == b.precision()
                && a.check() == b.check()
        };
        let unchanged = self.rules.len() == rules.len()
            && self
                .rules
                .rules()
                .iter()
                .zip(rules.rules())
                .all(|(a, b)| same(a, b));
        if unchanged {
            Arc::new(Self {
                rules,
                engines: Arc::clone(&self.engines),
            })
        } else {
            Arc::new(Self::new(rules))
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
        engine.fallback(index, rule.regex(), text, seen, found)
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
            .fallback_states(rule.regex().as_str())
            .map(|cost| cost.states.saturating_mul(bytes))
    }

    /// The bytes rule `index`'s fallback slot tables take, as the gate
    /// counts them ([`SMART_FALLBACK_SLOT_BYTES`]); `None` when it may
    /// never fall back.
    #[cfg(test)]
    pub(crate) fn fallback_slot_bytes(&self, index: usize) -> Option<usize> {
        let engine = self.engines.get(index)?;
        let rule = self.rules.rules().get(index)?;
        engine
            .fallback_states(rule.regex().as_str())
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
            && self.states.saturating_mul(bytes) <= SMART_FALLBACK_UNITS
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
        Self {
            dfa,
            unicode_word,
            fallback_states: OnceLock::new(),
            gave_up: AtomicBool::new(false),
            over_gate: AtomicBool::new(false),
        }
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
                })
        })
    }

    /// The regex crate's `find_iter` of `text`, the first `skip` matches
    /// (already reported) left out, when within the gate.
    fn fallback(
        &self,
        index: usize,
        regex: &Regex,
        text: &str,
        skip: usize,
        mut found: impl FnMut(Range<usize>, SearchCost) -> ControlFlow<()>,
    ) -> Scanned {
        let within = self
            .fallback_states(regex.as_str())
            .is_some_and(|cost| cost.within(text.len()));
        if !within {
            Self::warn_once(&self.over_gate, index, "its fallback is over the gate");
            return Scanned::Nothing;
        }
        #[cfg(test)]
        FALLBACK_RUNS.with(|runs| runs.set(runs.get() + 1));
        for one in regex.find_iter(text).skip(skip) {
            if found(one.range(), SearchCost::Costly).is_break() {
                break;
            }
        }
        Scanned::Done
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

/// A rule's lazy DFA and its cache.
#[derive(Debug)]
struct Dfa {
    regex: hybrid::Regex,
    /// A leaf lock: held for one scan.
    cache: Mutex<RuleCache>,
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
    /// with a literal prefilter as the regex crate has; `None` if it does
    /// not build.
    fn build(pattern: &str) -> Option<Self> {
        // One parse for both directions and the prefilter.
        let syntax = syntax::Config::new().nest_limit(SMART_RULE_NEST_LIMIT);
        let hir = syntax::parse_with(pattern, &syntax).ok()?;
        let prefilter = Prefilter::from_hirs_prefix(MatchKind::LeftmostFirst, &[&hir]);
        let nfa = thompson::Config::new()
            .nfa_size_limit(Some(SMART_RULE_SIZE_LIMIT))
            .which_captures(thompson::WhichCaptures::None);
        let forward = thompson::Compiler::new()
            .configure(nfa.clone())
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
        let regex = hybrid::Regex::builder().build_from_dfas(forward, reverse);
        let cache = regex.create_cache();
        Some(Self {
            regex,
            cache: Mutex::new(RuleCache { cache, cold: true }),
        })
    }

    /// Scans `text` from its start, reporting each match, and whether its
    /// search found the cache cold or reset it, to `found`.
    fn scan(
        &self,
        text: &str,
        found: &mut impl FnMut(Range<usize>, SearchCost) -> ControlFlow<()>,
    ) -> DfaScan {
        let mut held = self.cache.lock().unwrap_or_else(|poisoned| {
            // A scan that panicked may have left the cache half updated.
            let mut held = poisoned.into_inner();
            held.cache.reset(&self.regex);
            held.cold = true;
            held
        });
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
                        cache.reset(&self.regex);
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
        Regex::new(pattern)
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
                    reference(rule.regex().as_str(), &text),
                    "rule {index} on {text:?}"
                );
            }
        }
        // 8 of the 15 rules have a Unicode `\b`; 6 of the 9 windows are not
        // ASCII (the empty one is).
        assert_eq!(FALLBACK_RUNS.with(std::cell::Cell::get), 8 * 6);
        assert!(!(0..engines.rules().len()).any(|index| engines.warned(index)));
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
        assert_eq!(
            scan(&thrash, 0, &multi_script(8192, 1)).1,
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
    /// [`SMART_FALLBACK_UNITS`] and not on one byte more. `\ba{244}` has
    /// 250 states with captures, so a 32 000-byte window is the gate.
    #[test]
    fn the_fallback_runs_up_to_the_gate_and_not_one_byte_past() {
        let rule = engines(&[r"\ba{244}"]);
        let at_gate = SMART_FALLBACK_UNITS / 250;
        assert_eq!(rule.fallback_units(0, at_gate), Some(SMART_FALLBACK_UNITS));
        let window = |bytes: usize| {
            let mut text = format!("{} \u{e9}", "a".repeat(244));
            text.push_str(&" ".repeat(bytes - text.len()));
            text
        };
        WARNINGS.with(|warnings| warnings.set(0));
        assert_eq!(
            scan(&rule, 0, &window(at_gate)),
            (vec![Range { start: 0, end: 244 }], Scanned::Done),
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
    /// slots) is over the slot gate: on a 160-column line of `éx` its click
    /// selects nothing from it, with one over-the-gate warning, and the
    /// regex crate never runs, so nothing is kept for it in that crate's
    /// cache pool; what the rule keeps is its own DFA cache, within its cap.
    #[test]
    fn a_rule_over_the_slot_gate_selects_nothing_and_keeps_no_fallback_cache() {
        let pattern = format!(r"\bx(?:{}){{23}}", "(a?)".repeat(250));
        assert_eq!(pattern.len(), 1011);
        let rule = engines(&[&pattern]);
        let text = "\u{e9}x".repeat(75);
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
            let slot_bytes = engines.fallback_slot_bytes(index);
            assert!(
                slot_bytes.is_none_or(|bytes| bytes <= SMART_FALLBACK_SLOT_BYTES / 16),
                "rule {index}: {slot_bytes:?}"
            );
            for text in &windows {
                assert!(text.len() > crate::selection::SMART_SCAN_BYTES - 4);
                let spans = reference(rule.regex().as_str(), text);
                matched += spans.len();
                assert_eq!(
                    scan(&engines, index, text),
                    (spans, Scanned::Done),
                    "rule {index}"
                );
            }
        }
        assert!(matched > 1000, "{matched} matches");
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
}
