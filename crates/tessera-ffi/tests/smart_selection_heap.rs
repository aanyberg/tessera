//! The heap smart-selection clicks keep allocated (S5, #649): bytes the
//! allocator hands out, not the regex crates' counts of their caches.
//!
//! No API reports what a rule's engines allocate, so
//! this binary's global allocator counts it: tessera-ffi is one of the two
//! crates that may hold `unsafe` (docs/security.md S7). The heap is
//! process-wide; under nextest each test is its own process, and the
//! session is a test session (no child), so the count is the click's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tessera_app_core::{App, Config, ConfigPaths, Granularity, Point, PtySize, SessionId};
use tessera_config::{Precision, SmartRule};

/// [`System`], counting the bytes currently allocated by every thread.
struct Counting;

/// Bytes allocated and not yet freed, process-wide.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// The most [`LIVE`] has been since [`reset_peak`] (#688).
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// Counts `bytes` more allocated, raising [`PEAK`] with them.
fn grew(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

/// Starts [`PEAK`] again at the live heap.
fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::SeqCst), Ordering::SeqCst);
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

// SAFETY: every call forwards to `System` with the caller's arguments; the
// counter is a side effect that never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let block = unsafe { System.alloc(layout) };
        if !block.is_null() {
            grew(layout.size());
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let block = unsafe { System.alloc_zeroed(layout) };
        if !block.is_null() {
            grew(layout.size());
        }
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(block, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let moved = unsafe { System.realloc(block, layout, new_size) };
        if !moved.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        moved
    }
}

/// The live heap once it has not moved by more than 4 KiB for five polls
/// 20 ms apart (or after 5 s): the session's threads may still be busy.
fn settled_heap() -> usize {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = LIVE.load(Ordering::SeqCst);
    let mut still = 0;
    while still < 5 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
        let now = LIVE.load(Ordering::SeqCst);
        still = if now.abs_diff(last) < 4096 {
            still + 1
        } else {
            0
        };
        last = now;
    }
    last
}

/// `count` letters `a` and `b` from `seed` (xorshift).
fn random_ab(count: usize, mut seed: u64) -> String {
    (0..count)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            if seed & 1 == 0 { 'a' } else { 'b' }
        })
        .collect()
}

/// The columns of the test session, the critic's layout (#649 B1).
const COLS: usize = 200;
/// The rows each fed line gets: the longest line, 7981 cells, wraps to 40.
const LINE_ROWS: usize = 41;

/// What `app` keeps allocated after clicking each of `lines` `clicks` times,
/// past what the fed session holds. Line `i` starts at row `41 i` of a 330 x
/// 200 session and is clicked in its middle row, at column `column` or the
/// next one, alternately, so every click starts a new search.
///
/// Every rule must load: a rule left out would keep nothing.
fn kept_by_clicks(rules: &[String], lines: &[String], column: u16, clicks: usize) -> usize {
    let fed = Fed::new(rules, lines);
    let before = settled_heap();
    fed.click(column, clicks);
    let kept = settled_heap().saturating_sub(before);
    fed.close();
    kept
}

/// The configuration of `rules`, each of which must load (a rule left out
/// would keep nothing).
fn config_of(rules: &[String]) -> Config {
    let mut config = Config::default();
    config.selection.smart_rules = Some(
        rules
            .iter()
            .map(|rule| SmartRule::new(rule, Precision::Normal))
            .collect(),
    );
    // A copy without the shared compile, so the app compiles them itself.
    let mut probe = Config::default();
    probe
        .selection
        .smart_rules
        .clone_from(&config.selection.smart_rules);
    assert_eq!(
        probe.selection.compiled_rules().rules().len(),
        rules.len(),
        "a rule did not load"
    );
    config
}

/// An app with smart rules and a 330 x 200 test session fed with lines,
/// line `i` from row `41 i`.
struct Fed {
    app: App,
    id: SessionId,
    /// The middle row of each line.
    rows: Vec<u16>,
}

impl Fed {
    fn new(rules: &[String], lines: &[String]) -> Self {
        let app = App::new(
            config_of(rules),
            ConfigPaths::new("/nonexistent/tessera-649"),
        );
        let id = app
            .spawn_test_session(PtySize::new(330, 200))
            .expect("a session");
        let mut rows = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            let start = LINE_ROWS * index;
            // CUP is 1-based.
            app.feed_for_test(id, format!("\x1b[{};1H{line}", start + 1).as_bytes())
                .expect("fed");
            let middle = start + line.chars().count() / COLS / 2;
            rows.push(u16::try_from(middle).expect("within 330 rows"));
        }
        Self { app, id, rows }
    }

    /// Double-clicks each line's middle row `clicks` times, at `column` or
    /// the next one, alternately, so every click starts a new search.
    fn click(&self, column: u16, clicks: usize) {
        for click in 0..clicks {
            for row in &self.rows {
                let column = column + u16::try_from(click % 2).expect("0 or 1");
                self.app
                    .select_begin(self.id, Point::new(*row, column), Granularity::Word, false)
                    .expect("clicked");
            }
        }
    }

    fn close(self) {
        self.app.close_session(self.id).expect("closed");
    }
}

/// `|[cC]` for each of `chars`, `c` and its upper case.
fn alternatives(chars: impl Iterator<Item = char>) -> String {
    chars.fold(String::new(), |mut out, c| {
        // Writing to a String cannot fail.
        let _ = write!(out, "|[{c}{}]", c.to_uppercase());
        out
    })
}

/// The critic's stack filler (#649 B1): 159 one-letter classes and `ab`
/// repeated, on `ab` x 3990 and an `é`. The regex crate's bounded
/// backtracker kept 10.5 MiB of stack for it; the fallback (the rule's DFA,
/// then a `PikeVM`) keeps under its cache terms, 2.59 MiB of DFA caches and
/// 1.25 MiB of `PikeVM` cache (`SMART_RULE_CACHE_BYTES`).
#[test]
fn the_stack_filler_keeps_no_backtracker_stack() {
    // The critic's harness rule, exactly (`stack1`).
    let classes = alternatives(
        ('c'..='z')
            .chain('C'..='Z')
            .chain('0'..='9')
            .chain("!#%,/:;<=>@_".chars())
            .chain('\u{c0}'..'\u{17f}')
            .take(159),
    );
    let rule = format!(r"\b(?:ab{classes})+(?:Q0)?");
    let line = format!("{}\u{e9}", "ab".repeat(3990));
    let kept = kept_by_clicks(&[rule], &[line], 96, 8);
    eprintln!("kept {kept} bytes");
    assert!(
        kept <= (3 << 20) + (1 << 18),
        "the fallback kept {kept} bytes"
    );
}

/// The critic's mixed rule (#649 round 2): a large program (1.65 MiB as
/// measured) with a DFA cache filler, on a cache-filling line and a line
/// that makes it fall back. It keeps under one rule's ceiling, 5.996 MiB
/// (`SMART_RULE_CACHE_BYTES`).
#[test]
fn the_mixed_rule_stays_under_a_rule_ceiling() {
    let rule = MIXED_RULE.to_owned();
    let lines = [random_ab(8000, 7), format!("{}\u{e9}", "ab".repeat(3800))];
    let kept = kept_by_clicks(&[rule], &lines, 96, 4);
    eprintln!("kept {kept} bytes");
    assert!(kept > 1 << 20, "the program and caches: {kept} bytes kept");
    assert!(kept <= 5 << 20, "the mixed rule kept {kept} bytes");
    // 2.67 MiB measured; tessera-config's compile, were it kept, would add
    // about 1.65 MiB.
    assert!(
        kept <= 7 << 19,
        "{kept} bytes: tessera-config keeps no compile of the rule"
    );
}

/// What a rule's programs, its DFA's forward and reverse NFAs, allocate at
/// most (app-core's `RULE_PROGRAM_BYTES`): 2 x `SMART_RULE_SIZE_LIMIT`.
const PROGRAM_TERM: usize = 2 << 20;

/// The adversary's largest programs (#649 B1, `prog1820`): a class of 64
/// one-byte members repeated 1820 times, the most that loads. Its NFAs
/// allocate 1.86 MiB, over the 1.75 MiB term this branch documented
/// before; one click on a short ASCII line keeps them and two cold DFA
/// caches, within the program term.
#[test]
fn the_largest_programs_stay_within_the_program_term() {
    let kept = kept_by_clicks(&[prog1820()], &["ab cd ef".to_owned()], 3, 1);
    eprintln!("kept {kept} bytes");
    assert!(kept > (7 << 20) / 4, "its programs: {kept} bytes kept");
    assert!(kept <= PROGRAM_TERM, "{kept} bytes kept");
}

/// `prog1820`: the even bytes under 0x80, repeated 1820 times.
fn prog1820() -> String {
    let mut class = String::new();
    for byte in (0..0x80_u8).step_by(2) {
        write!(class, "\\x{byte:02x}").expect("a String takes every write");
    }
    format!(r"\b(?-u:[{class}]){{1820}}")
}

/// The adversary's refill (#649 B2): a line that fills the rule's DFA cache
/// with many small states, then one that makes it start again with large
/// ones. A reset cache would keep the first fill's tables and allocate up
/// to three times its capacity (2.90 measured); the fresh cache that
/// replaces it allocates for one fill, at most 2.59 times its capacity.
/// With its programs the rule keeps 1.01 MiB as measured, and 2.08 MiB
/// with a reset cache.
#[test]
fn a_refilled_dfa_cache_keeps_one_fill() {
    let rule = r"x(?-u:a){4200}|(?-u:b){1000}".to_owned();
    let lines = [format!("x{}", "a".repeat(4200)), "b".repeat(8000)];
    let kept = kept_by_clicks(&[rule], &lines, 96, 3);
    eprintln!("kept {kept} bytes");
    assert!(kept > 1 << 19, "the caches filled: {kept} bytes kept");
    assert!(kept <= 3 << 19, "{kept} bytes kept");
}

/// One rule's ceiling in bytes allocated (app-core's `RULE_CEILING`):
/// 2.59 + 1.25 + 2 + 0.156 = 5.996 MiB.
const RULE_CEILING: usize = 2_715_812 + (5 << 18) + (2 << 20) + (160 << 10);

/// The adversary's literal set (#649 cycle 2, `pre19.pat`): 200 exact
/// literals of 100 bytes, whose Aho-Corasick prefilter kept 19.1 MiB.
/// The prefilter is dropped, and the rule clicked on a line it matches
/// keeps within one rule's ceiling.
#[test]
fn a_large_literal_set_keeps_no_prefilter() {
    let tail = "\u{80}\"\u{b6}\\$\u{e4}\\&\u{112}\\(\u{140}\\*\u{176},\u{1a4}\\.\u{1d2}0\u{200}2\u{236}4\u{264}6\u{292}8\u{2c0}:\u{2f6}<\u{324}>\u{352}@\u{380}B\u{3b6}D\u{3e4}F\u{412}H\u{440}J\u{476}L\u{4a4}N\u{4d2}P\u{500}R\u{536}T\u{564}V\u{592}X\u{5c0}Z\u{5f6}\\\\\u{624}\\^\u{652}`";
    let rule = format!(r"\b[0-9]{{2}}[ab]{tail}");
    assert_eq!(rule.len(), 117);
    let mut line = "x 12a".to_owned();
    let mut escaped = false;
    for c in tail.chars() {
        escaped = !escaped && c == '\\';
        if !escaped {
            line.push(c);
        }
    }
    line.push_str(" end");
    let kept = kept_by_clicks(&[rule], &[line], 3, 2);
    eprintln!("kept {kept} bytes");
    assert!(kept <= RULE_CEILING, "{kept} bytes kept");
}

/// The critic's mix of a large program and a DFA cache filler (#649).
const MIXED_RULE: &str =
    r"\b(?:(?-u:[cegikmoqsuwyCEGIKMOQSUWY02468]){2000}|(?-u:q){12000}|[ab]*a[ab]{14})";

/// The 64 rules of [`sixty_four_filling_rules_stay_under_the_ceiling`] and
/// the lines that fill them.
fn sixty_four_rules() -> (Vec<String>, [String; 3]) {
    let classes = alternatives(('q'..='z').chain('Q'..='Z').chain('0'..='9'));
    let kinds = [
        prog1820(),
        r"\b(?-u:[cegikmoqsuwyCEGIKMOQSUWY02468]){3541}".to_owned(),
        r"\b\w{20}".to_owned(),
        r"\bx[a-z]{14559}".to_owned(),
        r"[ab]*a[ab]{14}".to_owned(),
        format!(r"\b(?:ab{classes})+"),
        MIXED_RULE.to_owned(),
    ];
    let rules = kinds.iter().cycle().take(64).cloned().collect();
    let lines = [
        format!("x{}\u{e9}", "a".repeat(300)),
        random_ab(8000, 7),
        format!("{}\u{e9}", "ab".repeat(3800)),
    ];
    (rules, lines)
}

/// The ceiling in bytes allocated (S5, #649): 64 rules, about 9 of each
/// kind that fills a term (the largest programs, `\b\w{20}` with its raised
/// cache, rules at the slot gate, rules filling their DFA caches, stack
/// fillers and the critic's mix of a large program with a cache filler),
/// each clicked on its line, keep at most 64 x 5.996 = 383.76 MiB
/// (`SMART_RULE_CACHE_BYTES`).
#[test]
fn sixty_four_filling_rules_stay_under_the_ceiling() {
    let (rules, lines) = sixty_four_rules();
    let kept = kept_by_clicks(&rules, &lines, 96, 2);
    assert!(kept > 64 << 20, "the rules filled: {kept} bytes kept");
    eprintln!("kept {kept} bytes");
    assert!(kept <= 64 * RULE_CEILING, "{kept} bytes kept");
}

/// A smart-rules reload peaks at one set of engines, not two (#688): the
/// app drops the old engines, filled caches and all, before it builds the
/// new ones. The 64 filling rules are clicked on their lines; the
/// configuration then changes every rule (the list moves up by one, so
/// each place holds another pattern, compiled before it is applied, as a
/// load does), and a double-click on a blank row, which searches nothing,
/// rebuilds the engines. That rebuild peaks at the larger set, the old one
/// kept or the new one built, plus what building one rule holds for a
/// moment (1.1 MB at most as measured), within one rule's ceiling; holding
/// both sets peaked at the old set plus the whole new one (180.5 MB over
/// 97.5 MB kept, as measured). Clicks on the lines then fill the new set,
/// and the reload's peak through them stays under the ceiling. A rebuild
/// reads no clock, so load changes only how much of each set the clicks
/// fill (the clicks' budget), not the bound.
#[test]
fn a_rules_reload_peaks_at_one_set_of_engines() {
    let (rules, lines) = sixty_four_rules();
    let fed = Fed::new(&rules, &lines);
    let before = settled_heap();
    fed.click(96, 2);
    let old = settled_heap().saturating_sub(before);
    let changed: Vec<String> = rules.iter().cycle().skip(1).take(64).cloned().collect();
    let changed = config_of(&changed);
    // Compiled now, as a load does, not at the click.
    changed.selection.compiled_rules();
    fed.app
        .apply_config(&changed, &tessera_config::Theme::default());
    let applied = settled_heap();
    reset_peak();
    // The bottom row holds no text.
    fed.app
        .select_begin(fed.id, Point::new(329, 0), Granularity::Word, false)
        .expect("clicked");
    let built = PEAK.load(Ordering::SeqCst);
    let rebuilt = settled_heap();
    fed.click(96, 2);
    let peak = PEAK.load(Ordering::SeqCst).saturating_sub(before);
    let new = settled_heap().saturating_sub(before);
    eprintln!(
        "old {old} applied {} rebuild peak {} rebuilt {} reload peak {peak} new {new} bytes",
        applied.saturating_sub(before),
        built.saturating_sub(before),
        rebuilt.saturating_sub(before),
    );
    assert!(old > 64 << 20, "the rules filled: {old} bytes kept");
    assert!(
        built <= applied.max(rebuilt) + RULE_CEILING,
        "the rebuild peaked at {} bytes: {} kept by the old engines, {} by the new",
        built.saturating_sub(before),
        applied.saturating_sub(before),
        rebuilt.saturating_sub(before),
    );
    assert!(new > 64 << 20, "the changed rules filled: {new} bytes kept");
    assert!(peak <= 64 * RULE_CEILING, "{peak} bytes at the peak");
    fed.close();
}
