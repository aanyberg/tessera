//! The `[selection]` table: smart selection rules for a double-click and
//! how Copy treats trailing blanks (ADR 0042).
//!
//! A double-click selects the best match of the smart selection rules that
//! covers the clicked character, and falls back to the word under it when
//! none does. Each rule is a regular expression (the `regex` crate's
//! syntax: no look-around, no back-references) with a precision; a match
//! scores the precision's weight times its length in characters, so a
//! more precise rule wins unless the less precise match is a thousand
//! times longer. This module parses, bounds and compiles the rules; the
//! matching itself is `tessera-app-core`'s.
//!
//! Bounds (security rule S5): at most [`MAX_SMART_RULES`] rules, each
//! pattern at most [`MAX_SMART_RULE_PATTERN_LEN`] bytes, compiled with a
//! [`SMART_RULE_SIZE_LIMIT`] program and lazy-DFA cache and a nesting depth
//! of [`SMART_RULE_NEST_LIMIT`]. A rule that breaks a bound, does not
//! compile or matches the empty string is left out with a warning; the
//! others still apply.

use std::fmt;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::Warning;

/// The most smart selection rules read from `[selection] smart_rules`; the
/// others are left out with one warning.
pub const MAX_SMART_RULES: usize = 64;

/// The longest smart selection pattern, in bytes; a longer one is left out
/// with a warning.
pub const MAX_SMART_RULE_PATTERN_LEN: usize = 1024;

/// The size limit of a compiled smart selection pattern and of its lazy
/// DFA cache, in bytes (`RegexBuilder::size_limit` and `dfa_size_limit`).
pub const SMART_RULE_SIZE_LIMIT: usize = 1 << 20;

/// The deepest nesting of groups and repetitions a smart selection pattern
/// may have (`RegexBuilder::nest_limit`).
pub const SMART_RULE_NEST_LIMIT: u32 = 50;

/// How much a rule's match counts: a match scores [`Precision::weight`]
/// times its length in characters, so one level is worth a thousand times
/// the length (a hundred from `very_low` to `low`; iTerm2's precisions and
/// weights).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Precision {
    /// The lowest weight.
    VeryLow,
    /// Below normal.
    Low,
    /// The default.
    #[default]
    Normal,
    /// Above normal.
    High,
    /// The highest weight.
    VeryHigh,
}

impl Precision {
    /// Every precision, lowest first.
    pub const ALL: [Self; 5] = [
        Self::VeryLow,
        Self::Low,
        Self::Normal,
        Self::High,
        Self::VeryHigh,
    ];

    /// The name written in `config.toml` (`"very_high"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VeryLow => "very_low",
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::VeryHigh => "very_high",
        }
    }

    /// A plain string `enum` schema: the derived one, a `oneOf` of
    /// documented `const`s, is a shape `cargo xtask config-docs` does not
    /// render.
    #[cfg(feature = "schema")]
    fn schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let values: Vec<&str> = Self::ALL.iter().map(|value| value.as_str()).collect();
        schemars::json_schema!({ "type": "string", "enum": values })
    }

    /// The weight a match's length is multiplied by: iTerm2's 0.00001,
    /// 0.001, 1, 1000 and 1000000, times a million so they are integers
    /// (10 for `very_low` up to 10^12 for `very_high`).
    pub const fn weight(self) -> u64 {
        match self {
            Self::VeryLow => 10,
            Self::Low => 1_000,
            Self::Normal => 1_000_000,
            Self::High => 1_000_000_000,
            Self::VeryHigh => 1_000_000_000_000,
        }
    }
}

/// One smart selection rule as written in `[[selection.smart_rules]]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct SmartRule {
    /// The regular expression, in the `regex` crate's syntax (no
    /// look-around, no back-references): at most 1024 bytes, and it must
    /// not match the empty string.
    pub regex: String,
    /// How much a match counts: `"very_low"`, `"low"`, `"normal"`,
    /// `"high"` or `"very_high"`; a match scores the precision's weight
    /// times its length, one level being worth a thousand times the length.
    #[cfg_attr(feature = "schema", schemars(schema_with = "Precision::schema"))]
    pub precision: Precision,
}

impl SmartRule {
    /// A rule of `regex` with `precision`.
    pub fn new(regex: impl Into<String>, precision: Precision) -> Self {
        Self {
            regex: regex.into(),
            precision,
        }
    }
}

/// What the matcher does with a rule's matches besides scoring them: the
/// checks the built-in rules need where the `regex` crate has no
/// look-around. A rule gets one when its pattern is byte for byte that of
/// a built-in rule, so a copy of the defaults keeps their behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RuleCheck {
    /// No check: every match counts.
    None,
    /// The built-in `http` / `https` URL rule: a trailing `.` `,` `;` `:`
    /// `!` `?` `'` `"` is cut off the match, and so is a trailing `)` or
    /// `]` while the match holds more of them than of `(` or `[`,
    /// repeatedly (the URL detection's U4 and U5); whether the match covers
    /// the click is decided before the cut.
    UrlTrail,
    /// The built-in compressed IPv6 rule (`fe80::1`): a match followed by a
    /// hexadecimal digit or `:` does not count.
    NotBeforeHexOrColon,
    /// The built-in IPv6 rule that starts with `::` (`::1`): a match after
    /// a letter, a digit or `:` does not count.
    NotAfterAlnumOrColon,
}

/// One compiled smart selection rule.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    regex: Regex,
    precision: Precision,
    check: RuleCheck,
}

impl CompiledRule {
    /// The compiled pattern.
    pub fn regex(&self) -> &Regex {
        &self.regex
    }

    /// The rule's precision.
    pub fn precision(&self) -> Precision {
        self.precision
    }

    /// The check its matches go through ([`RuleCheck`]).
    pub fn check(&self) -> RuleCheck {
        self.check
    }
}

/// The smart selection rules in effect, compiled, in the order written
/// (the order breaks the last tie between two matches).
#[derive(Debug, Clone, Default)]
pub struct SmartRules {
    rules: Vec<CompiledRule>,
}

impl SmartRules {
    /// The rules, in order.
    pub fn rules(&self) -> &[CompiledRule] {
        &self.rules
    }

    /// How many rules there are.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// `H`, one group of an IPv6 address, in the built-in patterns.
macro_rules! ipv6_group {
    () => {
        "[0-9A-Fa-f]{1,4}"
    };
}

/// The built-in rules, in order: iTerm2's defaults (its `\S+` word rule
/// left out, so a double-click elsewhere still selects the word) and
/// Tessera's git hash, IPv4 and IPv6 rules, as ADR 0042 lists them.
pub const DEFAULT_SMART_RULES: [(&str, Precision); 15] = [
    // C++ names: std::vector.
    (r"(?:[A-Za-z0-9_]+::)+[A-Za-z0-9_]+", Precision::Normal),
    // Paths: /usr/local/bin, ~/src, ./a/b; `\p{M}` keeps a decomposed
    // (NFD) accent, as macOS file names often hold, inside the path (#351).
    (
        r"~?/?(?:[\p{L}\p{M}\p{N}._-]+/+)+[\p{L}\p{M}\p{N}._-]+/?",
        Precision::Normal,
    ),
    // Double-quoted strings, with an Objective-C `@`.
    (r#"@?"(?:[^"\\]|\\.)*""#, Precision::Normal),
    // Dotted names: java.util.List, file.txt.
    (r"(?:[\p{L}\p{N}._]+\.)+[\p{L}\p{N}._]+", Precision::Normal),
    (
        r"\bmailto:(?:[a-z0-9A-Z_]+@)?(?:[a-zA-Z0-9\-]+\.)*[a-zA-Z0-9\-]+\b",
        Precision::Normal,
    ),
    (r"@selector\([^)]+\)", Precision::High),
    (
        r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,4}\b",
        Precision::High,
    ),
    (DEFAULT_URL_PATTERN, Precision::VeryHigh),
    (
        r"\bssh:(?:[a-z0-9A-Z_]+@)?(?:[a-zA-Z0-9\-]+\.)*[a-zA-Z0-9\-]+\b",
        Precision::VeryHigh,
    ),
    (
        r"\btelnet:(?:[a-z0-9A-Z_]+@)?(?:[a-zA-Z0-9\-]+\.)*[a-zA-Z0-9\-]+\b",
        Precision::VeryHigh,
    ),
    // Git object names and their abbreviations.
    (
        r"\b(?:[0-9a-f]{40}|[0-9a-f]{64}|[0-9a-f]{7,12})\b",
        Precision::Normal,
    ),
    (
        r"\b(?:(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\b",
        Precision::High,
    ),
    // IPv6: eight groups, compressed (`fe80::1`), and from `::` (`::1`).
    (DEFAULT_IPV6_FULL_PATTERN, Precision::High),
    (DEFAULT_IPV6_COMPRESSED_PATTERN, Precision::High),
    (DEFAULT_IPV6_LEADING_PATTERN, Precision::High),
];

/// The built-in `http` / `https` URL pattern ([`RuleCheck::UrlTrail`]).
const DEFAULT_URL_PATTERN: &str = r"https?://(?:[a-z0-9A-Z]+(?::[a-zA-Z0-9]+)?@)?(?:[a-z0-9A-Z][-a-z0-9A-Z]*\.)*[A-Za-z][-A-Za-z]*(?::[0-9]+)?(?:/[a-zA-Z0-9;/.\-_+%?&@=#~()]*)?";

/// Eight IPv6 groups.
const DEFAULT_IPV6_FULL_PATTERN: &str =
    concat!(r"\b(?:", ipv6_group!(), ":){7}", ipv6_group!(), r"\b");

/// A compressed IPv6 address that starts with a group
/// ([`RuleCheck::NotBeforeHexOrColon`]).
const DEFAULT_IPV6_COMPRESSED_PATTERN: &str = concat!(
    r"\b",
    ipv6_group!(),
    "(?::",
    ipv6_group!(),
    "){0,6}::(?:",
    ipv6_group!(),
    "(?::",
    ipv6_group!(),
    "){0,6})?"
);

/// A compressed IPv6 address that starts with `::`
/// ([`RuleCheck::NotAfterAlnumOrColon`]).
const DEFAULT_IPV6_LEADING_PATTERN: &str =
    concat!("::(?:", ipv6_group!(), "(?::", ipv6_group!(), "){0,6})?");

/// The check a pattern's matches go through: a built-in pattern's own, none
/// for any other.
fn check_for(pattern: &str) -> RuleCheck {
    match pattern {
        DEFAULT_URL_PATTERN => RuleCheck::UrlTrail,
        DEFAULT_IPV6_COMPRESSED_PATTERN => RuleCheck::NotBeforeHexOrColon,
        DEFAULT_IPV6_LEADING_PATTERN => RuleCheck::NotAfterAlnumOrColon,
        _ => RuleCheck::None,
    }
}

/// Why a rule was left out.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RuleError {
    TooLong(usize),
    Invalid,
    MatchesEmpty,
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong(len) => write!(
                f,
                "a pattern of {len} bytes ignored (at most {MAX_SMART_RULE_PATTERN_LEN})"
            ),
            // The regex error quotes the pattern; the warning does not
            // (the length and the reason are enough to find it).
            Self::Invalid => {
                f.write_str("the pattern ignored: not a valid regular expression, or too complex")
            }
            Self::MatchesEmpty => f.write_str("the pattern ignored: it matches the empty string"),
        }
    }
}

/// Compiles one rule within the bounds.
fn compile(rule: &SmartRule) -> Result<CompiledRule, RuleError> {
    if rule.regex.len() > MAX_SMART_RULE_PATTERN_LEN {
        return Err(RuleError::TooLong(rule.regex.len()));
    }
    let regex = RegexBuilder::new(&rule.regex)
        .size_limit(SMART_RULE_SIZE_LIMIT)
        // Bounds the lazy DFA's cache. No test can tell it from the
        // default (#351 m3): past its limit the regex crate falls back to a
        // slower engine with the same matches, so the mutant that drops it
        // survives; it bounds memory, not the result.
        .dfa_size_limit(SMART_RULE_SIZE_LIMIT)
        .nest_limit(SMART_RULE_NEST_LIMIT)
        .build()
        .map_err(|_| RuleError::Invalid)?;
    if regex.is_match("") {
        return Err(RuleError::MatchesEmpty);
    }
    Ok(CompiledRule {
        regex,
        precision: rule.precision,
        check: check_for(&rule.regex),
    })
}

/// The built-in rules, compiled once per process.
static DEFAULT_RULES: LazyLock<Arc<SmartRules>> = LazyLock::new(|| {
    Arc::new(SmartRules {
        rules: default_rules()
            .iter()
            .filter_map(|rule| compile(rule).ok())
            .collect(),
    })
});

/// [`DEFAULT_SMART_RULES`] as rules.
pub fn default_rules() -> Vec<SmartRule> {
    DEFAULT_SMART_RULES
        .iter()
        .map(|&(regex, precision)| SmartRule::new(regex, precision))
        .collect()
}

/// The compiled form of a [`Selection`]'s rules, kept with the rules it was
/// compiled from: shared by the clones of a configuration, ignored by its
/// comparison and debug output.
#[derive(Default)]
struct Compiled(Mutex<Option<(Vec<SmartRule>, Arc<SmartRules>)>>);

impl Clone for Compiled {
    fn clone(&self) -> Self {
        let held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Self(Mutex::new(held.clone()))
    }
}

impl fmt::Debug for Compiled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Compiled(..)")
    }
}

impl PartialEq for Compiled {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

/// Selection and Copy settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Selection {
    /// The rules a double-click tries before selecting the word: each a
    /// table with a `regex` (the Rust `regex` crate's syntax: no
    /// look-around, no back-references) and a `precision` (`"very_low"`,
    /// `"low"`, `"normal"`, `"high"` or `"very_high"`). Of the matches that
    /// cover the clicked character, the one with the highest precision
    /// weight times length wins (one level is worth a thousand times the
    /// length; then the more precise, the longer, the earlier one, the
    /// earlier rule); with none, the word is selected. Matching looks at
    /// most 8192 characters around the click along the logical line (4096
    /// before it); where the line goes on past an edge of that window, a
    /// match starting or ending in the unbroken run of non-blank
    /// characters at that edge is not taken.
    ///
    /// Unset, the built-in rules apply: file paths, `http`, `https`,
    /// `ssh`, `telnet` and `mailto` URLs (a URL loses trailing
    /// punctuation and unbalanced closing brackets), email addresses,
    /// double-quoted strings, C++ `a::b` and dotted `a.b.c` names,
    /// Objective-C selectors, git hashes, and IPv4 and IPv6 addresses
    /// (ADR 0042 lists their patterns). A list replaces them all. At most
    /// 64 rules are read; a rule whose pattern is longer than 1024 bytes,
    /// does not compile, is too complex or matches the empty string is
    /// ignored with a warning.
    pub smart_rules: Option<Vec<SmartRule>>,
    /// Whether Copy drops the blanks after the last text of each line (and
    /// blank lines at the end of the selection). Off, each row keeps its
    /// blanks up to the selection's end (a whole row for every row but the
    /// last), up to the last row with text.
    pub trim_trailing_whitespace: bool,
    #[serde(skip)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    compiled: Compiled,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            smart_rules: None,
            trim_trailing_whitespace: true,
            compiled: Compiled::default(),
        }
    }
}

impl Selection {
    /// The smart selection rules in effect, compiled: the built-in ones
    /// when [`Selection::smart_rules`] is unset. Compiled once per set of
    /// rules (when the configuration loads, or on the first call) and
    /// shared by clones; a rule that breaks a bound is left out, as the
    /// loader's warning says.
    pub fn compiled_rules(&self) -> Arc<SmartRules> {
        let Some(rules) = &self.smart_rules else {
            return Arc::clone(&DEFAULT_RULES);
        };
        let mut held = self
            .compiled
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((source, compiled)) = held.as_ref()
            && source == rules
        {
            return Arc::clone(compiled);
        }
        let compiled = Arc::new(SmartRules {
            rules: rules
                .iter()
                .take(MAX_SMART_RULES)
                .filter_map(|rule| compile(rule).ok())
                .collect(),
        });
        *held = Some((rules.clone(), Arc::clone(&compiled)));
        compiled
    }

    /// Leaves out the rules past [`MAX_SMART_RULES`] and those that break a
    /// bound, with a warning each (one for all those past the cap), and
    /// compiles the rest.
    pub(crate) fn normalize(&mut self, warnings: &mut Vec<Warning>) {
        let Some(rules) = self.smart_rules.as_mut() else {
            return;
        };
        if rules.len() > MAX_SMART_RULES {
            warnings.push(Warning::OutOfRange {
                path: "selection.smart_rules".into(),
                detail: format!(
                    "{} rules past the first {MAX_SMART_RULES} ignored",
                    rules.len() - MAX_SMART_RULES
                ),
            });
            rules.truncate(MAX_SMART_RULES);
        }
        let mut compiled = Vec::with_capacity(rules.len());
        let mut index = 0usize;
        rules.retain(|rule| {
            let at = index;
            index += 1;
            match compile(rule) {
                Ok(rule) => {
                    compiled.push(rule);
                    true
                }
                Err(error) => {
                    warnings.push(Warning::OutOfRange {
                        path: format!("selection.smart_rules[{at}].regex"),
                        detail: error.to_string(),
                    });
                    false
                }
            }
        });
        let compiled = Arc::new(SmartRules { rules: compiled });
        *self
            .compiled
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((rules.clone(), compiled));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    fn rules_toml(rules: &[String]) -> String {
        let mut text = String::from("[selection]\n");
        for rule in rules {
            text.push_str("[[selection.smart_rules]]\nregex = ");
            text.push_str(&toml::Value::String(rule.clone()).to_string());
            text.push('\n');
        }
        text
    }

    #[test]
    fn every_default_rule_compiles_with_its_check() {
        assert_eq!(DEFAULT_RULES.len(), DEFAULT_SMART_RULES.len());
        let checks: Vec<RuleCheck> = DEFAULT_RULES
            .rules()
            .iter()
            .map(CompiledRule::check)
            .collect();
        assert_eq!(
            checks.iter().filter(|&&c| c != RuleCheck::None).count(),
            3,
            "{checks:?}"
        );
        assert_eq!(checks[7], RuleCheck::UrlTrail);
        assert_eq!(checks[13], RuleCheck::NotBeforeHexOrColon);
        assert_eq!(checks[14], RuleCheck::NotAfterAlnumOrColon);
    }

    #[test]
    fn unset_rules_are_the_defaults_and_a_list_replaces_them() {
        let config = Config::default();
        assert!(config.selection.trim_trailing_whitespace);
        assert_eq!(config.selection.compiled_rules().len(), 15);
        let loaded = Config::from_toml_lenient(&rules_toml(&["x+".into()])).unwrap();
        assert_eq!(loaded.warnings, []);
        let rules = loaded.config.selection.compiled_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules.rules()[0].precision(), Precision::Normal);
        assert_eq!(rules.rules()[0].check(), RuleCheck::None);
    }

    #[test]
    fn precision_and_trim_parse() {
        let text = "[selection]\ntrim_trailing_whitespace = false\n[[selection.smart_rules]]\nregex = 'a+'\nprecision = 'very_high'\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        assert_eq!(loaded.warnings, []);
        assert!(!loaded.config.selection.trim_trailing_whitespace);
        assert_eq!(
            loaded.config.selection.smart_rules,
            Some(vec![SmartRule::new("a+", Precision::VeryHigh)])
        );
    }

    /// S5: a flood of rules keeps the first 64, with one warning.
    #[test]
    fn rules_past_the_cap_are_dropped_with_one_warning() {
        let rules: Vec<String> = (0..=MAX_SMART_RULES).map(|i| format!("x{i}")).collect();
        let loaded = Config::from_toml_lenient(&rules_toml(&rules)).unwrap();
        assert_eq!(
            loaded
                .warnings
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["`selection.smart_rules` out of range: 1 rules past the first 64 ignored"]
        );
        let kept = loaded.config.selection.smart_rules.as_deref().unwrap();
        assert_eq!(kept.len(), MAX_SMART_RULES);
        assert_eq!(kept[63].regex, "x63");
        assert_eq!(
            loaded.config.selection.compiled_rules().len(),
            MAX_SMART_RULES
        );

        let many: Vec<String> = (0..5000).map(|i| format!("y{i}")).collect();
        let loaded = Config::from_toml_lenient(&rules_toml(&many)).unwrap();
        assert_eq!(loaded.warnings.len(), 1);
        assert_eq!(
            loaded.config.selection.compiled_rules().len(),
            MAX_SMART_RULES
        );
    }

    /// S5: 1024 bytes pass, 1025 are refused; the warning quotes nothing of
    /// the pattern.
    #[test]
    fn a_pattern_over_1024_bytes_is_refused() {
        let at_cap = "a".repeat(MAX_SMART_RULE_PATTERN_LEN);
        let over = format!("<{}", "a".repeat(MAX_SMART_RULE_PATTERN_LEN));
        let loaded =
            Config::from_toml_lenient(&rules_toml(&[at_cap.clone(), over, "b".into()])).unwrap();
        let shown: Vec<String> = loaded.warnings.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "`selection.smart_rules[1].regex` out of range: a pattern of 1025 bytes ignored (at most 1024)"
            ]
        );
        let kept: Vec<&str> = loaded
            .config
            .selection
            .smart_rules
            .as_deref()
            .unwrap()
            .iter()
            .map(|rule| rule.regex.as_str())
            .collect();
        assert_eq!(kept, [at_cap.as_str(), "b"]);
        assert_eq!(loaded.config.selection.compiled_rules().len(), 2);
    }

    /// S5: patterns that do not compile, blow the size limit or nest too
    /// deep are skipped; the others still work.
    #[test]
    fn invalid_and_too_complex_patterns_are_skipped() {
        let deep = format!("{}a{}", "(".repeat(60), ")".repeat(60));
        let rules = [
            "(unclosed".to_owned(),
            // About 40 MB compiled: over the 1 MiB size limit.
            r"\w{1000}\w{1000}".to_owned(),
            deep,
            "ok".to_owned(),
        ];
        let loaded = Config::from_toml_lenient(&rules_toml(&rules)).unwrap();
        let paths: Vec<String> = loaded
            .warnings
            .iter()
            .map(|w| match w {
                Warning::OutOfRange { path, detail } => {
                    assert!(!detail.contains("unclosed"), "{detail}");
                    path.clone()
                }
                other => panic!("{other}"),
            })
            .collect();
        assert_eq!(
            paths,
            [
                "selection.smart_rules[0].regex",
                "selection.smart_rules[1].regex",
                "selection.smart_rules[2].regex",
            ]
        );
        let compiled = loaded.config.selection.compiled_rules();
        assert_eq!(compiled.len(), 1);
        assert!(compiled.rules()[0].regex().is_match("ok"));
    }

    /// The size limit is the 1 MiB bound, not the regex crate's 10 MiB
    /// default: `\w{50}` compiles to between the two.
    #[test]
    fn a_pattern_over_1_mib_compiled_is_refused() {
        let mid = r"\w{50}";
        assert!(Regex::new(mid).is_ok(), "within the crate's default");
        let loaded = Config::from_toml_lenient(&rules_toml(&[mid.into(), "ok".into()])).unwrap();
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert_eq!(loaded.config.selection.compiled_rules().len(), 1);
    }

    /// Rules set without the loader (a frontend building a configuration)
    /// are capped at 64 when compiled too.
    #[test]
    fn compiling_takes_at_most_64_rules() {
        let selection = Selection {
            smart_rules: Some(
                (0..MAX_SMART_RULES + 6)
                    .map(|i| SmartRule::new(format!("x{i}"), Precision::Normal))
                    .collect(),
            ),
            ..Selection::default()
        };
        assert_eq!(selection.compiled_rules().len(), 64);
    }

    #[test]
    fn patterns_that_match_the_empty_string_are_refused() {
        let rules = [
            "a*".to_owned(),
            "^".to_owned(),
            "x?".to_owned(),
            "b".to_owned(),
        ];
        let loaded = Config::from_toml_lenient(&rules_toml(&rules)).unwrap();
        assert_eq!(loaded.warnings.len(), 3, "{:?}", loaded.warnings);
        assert!(
            loaded.warnings[0]
                .to_string()
                .ends_with("it matches the empty string")
        );
        assert_eq!(loaded.config.selection.compiled_rules().len(), 1);
    }

    #[test]
    fn compiled_rules_are_shared_until_the_rules_change() {
        let loaded = Config::from_toml_lenient(&rules_toml(&["a+".into()])).unwrap();
        let mut config = loaded.config;
        let first = config.selection.compiled_rules();
        assert!(Arc::ptr_eq(&first, &config.selection.compiled_rules()));
        let clone = config.clone();
        assert!(Arc::ptr_eq(&first, &clone.selection.compiled_rules()));
        assert_eq!(clone, config);
        config.selection.smart_rules = Some(vec![SmartRule::new("b+", Precision::High)]);
        let changed = config.selection.compiled_rules();
        assert!(!Arc::ptr_eq(&first, &changed));
        assert!(changed.rules()[0].regex().is_match("b"));
        assert!(Arc::ptr_eq(
            &Config::default().selection.compiled_rules(),
            &Config::default().selection.compiled_rules()
        ));
    }

    #[test]
    fn an_unset_list_serialises_to_nothing() {
        let table = toml::Table::try_from(Config::default()).unwrap();
        let selection = table["selection"].as_table().unwrap();
        assert_eq!(
            selection.keys().collect::<Vec<_>>(),
            ["trim_trailing_whitespace"]
        );
    }
}
