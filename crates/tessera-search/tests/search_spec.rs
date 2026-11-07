//! The scrollback search spec fixture (wave 13, T3 #46a), ported by hand
//! from the spec researcher's `search_spec_fixture.cases`: each row feeds
//! its input through a real terminal at the row's width and searches the
//! screen's rows as one run, like the application core does with the rows
//! it copies.
//!
//! Matches are `[start_row, start_col, end_row, end_col]` with `end_col`
//! exclusive on the row of the last cell. Lead decision D16 changes the
//! fixture in two places: the regex size limit is 10 MiB, so `\w{100}`
//! (D25) compiles and the boundary (D25b) is `\w{209}` / `\w{210}`.
//! Smart case (C04, C05) is the find bar's: the rows compute the bool from
//! the query the way it will. Navigation (E), chunking (F01, F02), the caps
//! that need a scrollback (D30-D33) and the sequence D13 are tested in
//! `tessera-app-core/tests/search.rs`.

use std::ops::ControlFlow;

use tessera_search::{
    Haystack, MAX_QUERY_BYTES, Match, Query, QueryError, QueryOptions, Rows, scan, segment_period,
};
use tessera_term::Terminal;

/// What a row expects.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// These matches, in document order.
    Matches(&'static [[u64; 4]]),
    /// The query is refused for this reason.
    Error(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    TooLong,
    Syntax,
    TooComplex,
}

#[derive(Debug, Clone, Copy)]
enum Case {
    Sensitive,
    Insensitive,
    /// Insensitive unless the query has an uppercase letter (the find bar's
    /// rule, computed from the query as T8 will).
    Smart,
}

struct Row {
    id: &'static str,
    cols: u16,
    input: &'static str,
    query: &'static str,
    regex: bool,
    case: Case,
    expect: Expect,
}

const LIT: bool = false;
const RE: bool = true;
use Case::{Insensitive as CI, Sensitive as CS, Smart};
use Expect::{Error as Err_, Matches as M};
use Refusal::{Syntax, TooComplex, TooLong};

const ZWJ_PAIR: &str = "\u{1F468}\u{200D}\u{1F469}";

#[rustfmt::skip]
const ROWS: &[Row] = &[
    // A. Wrapped logical lines.
    Row { id: "A01-no-wrap-single-match", cols: 20, input: "hello world", query: "world", regex: LIT, case: CS, expect: M(&[[0, 6, 0, 11]]) },
    Row { id: "A02-match-spans-soft-wrap", cols: 5, input: "hello world", query: "o wor", regex: LIT, case: CS, expect: M(&[[0, 4, 1, 4]]) },
    Row { id: "A03-match-spans-three-rows", cols: 4, input: "abcdefghij", query: "cdefghi", regex: LIT, case: CS, expect: M(&[[0, 2, 2, 1]]) },
    Row { id: "A04-match-ends-exactly-at-row-end", cols: 5, input: "abcdefgh", query: "cde", regex: LIT, case: CS, expect: M(&[[0, 2, 0, 5]]) },
    Row { id: "A05-match-starts-at-row-start-after-wrap", cols: 5, input: "abcdefgh", query: "fg", regex: LIT, case: CS, expect: M(&[[1, 0, 1, 2]]) },
    Row { id: "A06-space-at-wrap-boundary-is-content", cols: 4, input: "foo bar", query: "foo bar", regex: LIT, case: CS, expect: M(&[[0, 0, 1, 3]]) },
    Row { id: "A07-hard-break-does-not-join", cols: 5, input: "abc\r\ndef", query: "cd", regex: LIT, case: CS, expect: M(&[]) },
    Row { id: "A08-regex-whitespace-does-not-cross-hard-break", cols: 10, input: "foo\r\nbar", query: r"foo\s+bar", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "A09-regex-whitespace-crosses-soft-wrap", cols: 4, input: "foo bar", query: r"foo\s+bar", regex: RE, case: CS, expect: M(&[[0, 0, 1, 3]]) },
    Row { id: "A10-caret-anchors-logical-line-start", cols: 5, input: "abcdefgh", query: "^f", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "A11-caret-matches-first-row-only", cols: 5, input: "abcdefgh", query: "^abc", regex: RE, case: CS, expect: M(&[[0, 0, 0, 3]]) },
    Row { id: "A12-dollar-anchors-logical-line-end", cols: 5, input: "abcdefgh", query: "h$", regex: RE, case: CS, expect: M(&[[1, 2, 1, 3]]) },
    Row { id: "A13-dollar-not-at-row-end-of-wrapped-row", cols: 5, input: "abcdefgh", query: "e$", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "A14-anchored-both-ends-whole-line", cols: 5, input: "abcdefgh", query: "^abcdefgh$", regex: RE, case: CS, expect: M(&[[0, 0, 1, 3]]) },
    Row { id: "A15-trailing-blanks-of-last-row-are-not-text", cols: 10, input: "abc", query: "c ", regex: LIT, case: CS, expect: M(&[]) },
    Row { id: "A16-typed-trailing-space-is-not-distinguishable", cols: 10, input: "abc ", query: "c ", regex: LIT, case: CS, expect: M(&[]) },
    Row { id: "A17-interior-blank-cells-are-spaces", cols: 10, input: "a\x1b[3Cb", query: "a   b", regex: LIT, case: CS, expect: M(&[[0, 0, 0, 5]]) },
    Row { id: "A18-two-matches-one-logical-line-both-across-wrap", cols: 3, input: "abcabcabc", query: "ca", regex: LIT, case: CS, expect: M(&[[0, 2, 1, 1], [1, 2, 2, 1]]) },
    Row { id: "A19-overlapping-literal-matches-are-nonoverlapping", cols: 20, input: "aaaa", query: "aa", regex: LIT, case: CS, expect: M(&[[0, 0, 0, 2], [0, 2, 0, 4]]) },
    // A20 (reflow) is `a20_width_reflow_same_matches` below.

    // B. Wide characters, clusters, column mapping.
    Row { id: "B01-wide-char-covers-two-columns", cols: 10, input: "ab中文cd", query: "中", regex: LIT, case: CS, expect: M(&[[0, 2, 0, 4]]) },
    Row { id: "B02-second-wide-char", cols: 10, input: "ab中文cd", query: "文", regex: LIT, case: CS, expect: M(&[[0, 4, 0, 6]]) },
    Row { id: "B03-match-after-wide-chars-uses-columns-not-chars", cols: 10, input: "ab中文cd", query: "cd", regex: LIT, case: CS, expect: M(&[[0, 6, 0, 8]]) },
    Row { id: "B04-match-across-wide-chars", cols: 10, input: "ab中文cd", query: "b中文c", regex: LIT, case: CS, expect: M(&[[0, 1, 0, 7]]) },
    Row { id: "B05-regex-dot-matches-one-wide-char-as-two-columns", cols: 10, input: "中文", query: "^.$", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "B06-regex-dot-star-wide", cols: 10, input: "x中文y", query: "x.*y", regex: RE, case: CS, expect: M(&[[0, 0, 0, 6]]) },
    Row { id: "B07-wide-char-pushed-to-next-row-pad-cell-excluded", cols: 5, input: "abcd中e", query: "d中", regex: LIT, case: CS, expect: M(&[[0, 3, 1, 2]]) },
    Row { id: "B08-pad-cell-does-not-break-literal-across-it", cols: 5, input: "abcd中e", query: "abcd中e", regex: LIT, case: CS, expect: M(&[[0, 0, 1, 3]]) },
    Row { id: "B09-pad-cell-is-not-a-space", cols: 5, input: "abcd中e", query: "d ", regex: LIT, case: CS, expect: M(&[]) },
    Row { id: "B10-combining-mark-one-cell", cols: 10, input: "cafe\u{301} ok", query: "ok", regex: LIT, case: CS, expect: M(&[[0, 5, 0, 7]]) },
    Row { id: "B11-query-with-combining-mark-matches-cluster-cell", cols: 10, input: "cafe\u{301} ok", query: "e\u{301}", regex: LIT, case: CS, expect: M(&[[0, 3, 0, 4]]) },
    Row { id: "B12-base-letter-query-inside-cluster-expands-to-cell", cols: 10, input: "cafe\u{301}", query: "e", regex: LIT, case: CS, expect: M(&[[0, 3, 0, 4]]) },
    Row { id: "B13-dot-on-cluster-gives-one-match", cols: 10, input: "e\u{301}", query: ".", regex: RE, case: CS, expect: M(&[[0, 0, 0, 1]]) },
    Row { id: "B14-no-normalisation-precomposed-vs-decomposed", cols: 10, input: "cafe\u{301}", query: "caf\u{e9}", regex: LIT, case: CS, expect: M(&[]) },
    Row { id: "B15-precomposed-matches-precomposed", cols: 10, input: "caf\u{e9}", query: "caf\u{e9}", regex: LIT, case: CS, expect: M(&[[0, 0, 0, 4]]) },
    Row { id: "B16-emoji-zwj-cluster-is-one-wide-cell", cols: 10, input: "a\u{1F468}\u{200D}\u{1F469}b", query: "b", regex: LIT, case: CS, expect: M(&[[0, 3, 0, 4]]) },
    Row { id: "B17-match-whole-emoji-cluster", cols: 10, input: "a\u{1F468}\u{200D}\u{1F469}b", query: ZWJ_PAIR, regex: LIT, case: CS, expect: M(&[[0, 1, 0, 3]]) },
    Row { id: "B18-wide-cluster-spanning-wrap-edge-cell-index-after-spacer", cols: 6, input: "ab中文中x", query: "文中", regex: LIT, case: CS, expect: M(&[[0, 4, 1, 2]]) },
    Row { id: "B19-fullwidth-latin-not-folded-to-ascii", cols: 10, input: "\u{FF21}\u{FF22}\u{FF23}", query: "abc", regex: LIT, case: CI, expect: M(&[]) },
    Row { id: "B20-fullwidth-case-insensitive-folds-within-fullwidth", cols: 10, input: "\u{FF21}\u{FF22}\u{FF23}", query: "\u{FF41}\u{FF42}\u{FF43}", regex: LIT, case: CI, expect: M(&[[0, 0, 0, 6]]) },

    // C. Case handling.
    Row { id: "C01-insensitive-ascii", cols: 20, input: "Hello HELLO hello", query: "hello", regex: LIT, case: CI, expect: M(&[[0, 0, 0, 5], [0, 6, 0, 11], [0, 12, 0, 17]]) },
    Row { id: "C02-sensitive-ascii", cols: 20, input: "Hello HELLO hello", query: "hello", regex: LIT, case: CS, expect: M(&[[0, 12, 0, 17]]) },
    Row { id: "C03-sensitive-uppercase-query", cols: 20, input: "Hello HELLO hello", query: "Hello", regex: LIT, case: CS, expect: M(&[[0, 0, 0, 5]]) },
    Row { id: "C04-smart-lowercase-query-is-insensitive", cols: 20, input: "Hello HELLO hello", query: "hello", regex: LIT, case: Smart, expect: M(&[[0, 0, 0, 5], [0, 6, 0, 11], [0, 12, 0, 17]]) },
    Row { id: "C05-smart-uppercase-query-is-sensitive", cols: 20, input: "Hello HELLO hello", query: "Hello", regex: LIT, case: Smart, expect: M(&[[0, 0, 0, 5]]) },
    Row { id: "C06-unicode-simple-folding-accents", cols: 20, input: "\u{C9}COLE \u{E9}cole", query: "\u{E9}cole", regex: LIT, case: CI, expect: M(&[[0, 0, 0, 5], [0, 6, 0, 11]]) },
    Row { id: "C07-unicode-folding-greek-sigma", cols: 20, input: "ΣΟΦΙΑ σοφια ς", query: "σοφια", regex: LIT, case: CI, expect: M(&[[0, 0, 0, 5], [0, 6, 0, 11]]) },
    Row { id: "C08-no-full-case-folding-sharp-s", cols: 30, input: "STRASSE strasse STRA\u{DF}E", query: "stra\u{DF}e", regex: LIT, case: CI, expect: M(&[[0, 16, 0, 22]]) },
    Row { id: "C09-kelvin-sign-folds-to-k", cols: 10, input: "\u{212A} K k", query: "k", regex: LIT, case: CI, expect: M(&[[0, 0, 0, 1], [0, 2, 0, 3], [0, 4, 0, 5]]) },
    Row { id: "C10-turkish-dotted-i-not-folded-to-i", cols: 10, input: "\u{130} \u{131} I i", query: "i", regex: LIT, case: CI, expect: M(&[[0, 4, 0, 5], [0, 6, 0, 7]]) },
    Row { id: "C11-regex-inline-flag-overrides-toggle", cols: 10, input: "ABC", query: "(?i)abc", regex: RE, case: CS, expect: M(&[[0, 0, 0, 3]]) },
    Row { id: "C12-regex-escape-class-case-insensitive", cols: 10, input: "ABC abc", query: "[a-c]+", regex: RE, case: CI, expect: M(&[[0, 0, 0, 3], [0, 4, 0, 7]]) },
    Row { id: "C13-literal-metacharacters-are-escaped", cols: 20, input: "a.c abc a+c", query: "a.c", regex: LIT, case: CS, expect: M(&[[0, 0, 0, 3]]) },
    Row { id: "C14-literal-parens-brackets-backslash-no-error", cols: 20, input: "f(x) [0] \\n", query: "(x) [0] \\", regex: LIT, case: CS, expect: M(&[[0, 1, 0, 10]]) },

    // D. Regex flavour, degenerate patterns, errors, caps.
    Row { id: "D01-empty-query-idle", cols: 10, input: "abc", query: "", regex: LIT, case: CS, expect: M(&[]) },
    Row { id: "D02-empty-regex-query-idle", cols: 10, input: "abc", query: "", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "D03-zero-width-only-patterns-yield-nothing", cols: 10, input: "ab cd", query: "^", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "D03-dollar", cols: 10, input: "ab cd", query: "$", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "D03-word-boundary", cols: 10, input: "ab cd", query: r"\b", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "D03-empty-group", cols: 10, input: "ab cd", query: "(?:)", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "D03-star-of-absent", cols: 10, input: "ab", query: "x*", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "D04-star-skips-empty-keeps-nonempty", cols: 10, input: "baab", query: "a*", regex: RE, case: CS, expect: M(&[[0, 1, 0, 3]]) },
    Row { id: "D05-whitespace-only-query-is-valid-literal", cols: 10, input: "a b  c", query: " ", regex: LIT, case: CS, expect: M(&[[0, 1, 0, 2], [0, 3, 0, 4], [0, 4, 0, 5]]) },
    Row { id: "D06-inline-flag-only-pattern", cols: 10, input: "abc", query: "(?i)", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "D07-invalid-regex-unclosed-group", cols: 10, input: "abc", query: "(", regex: RE, case: CS, expect: Err_(Syntax) },
    Row { id: "D08-invalid-regex-unclosed-class", cols: 10, input: "abc", query: "[", regex: RE, case: CS, expect: Err_(Syntax) },
    Row { id: "D09-invalid-regex-trailing-backslash", cols: 10, input: "abc", query: "\\", regex: RE, case: CS, expect: Err_(Syntax) },
    Row { id: "D10-backreference-is-error", cols: 10, input: "aa", query: r"(a)\1", regex: RE, case: CS, expect: Err_(Syntax) },
    Row { id: "D11-lookaround-is-error", cols: 10, input: "aa", query: "(?=a)a", regex: RE, case: CS, expect: Err_(Syntax) },
    Row { id: "D12-repeated-quantifier-is-accepted", cols: 10, input: "aaa", query: "a**", regex: RE, case: CS, expect: M(&[[0, 0, 0, 3]]) },
    Row { id: "D13-valid-half", cols: 10, input: "abc", query: "b", regex: RE, case: CS, expect: M(&[[0, 1, 0, 2]]) },
    Row { id: "D24-size-limit-counted-repetition", cols: 80, input: "x", query: "a{1000}{1000}", regex: RE, case: CS, expect: Err_(TooComplex) },
    // D16: at 10 MiB `\w{100}` compiles.
    Row { id: "D25-w100-compiles-at-10-mib", cols: 80, input: "x", query: r"\w{100}", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "D25b-w209-ok", cols: 80, input: "x", query: r"\w{209}", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "D25b-w210-refused", cols: 80, input: "x", query: r"\w{210}", regex: RE, case: CS, expect: Err_(TooComplex) },
    Row { id: "D26-ascii-class-repeat-is-cheap", cols: 80, input: "x", query: r"(?-u:\w){5000}", regex: RE, case: CS, expect: M(&[]) },

    // G. Catastrophic patterns (the small ones; the big ones are below).
    Row { id: "G02-nested-star-linear", cols: 80, input: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", query: "(a*)*b", regex: RE, case: CS, expect: M(&[]) },
    Row { id: "G03-alternation-overlap-linear", cols: 80, input: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", query: "(a|aa)+$", regex: RE, case: CS, expect: M(&[[0, 0, 0, 40]]) },
    Row { id: "G05-dot-star-lazy-no-newline", cols: 80, input: "a1b\r\na2b", query: "a.*b", regex: RE, case: CS, expect: M(&[[0, 0, 0, 3], [1, 0, 1, 3]]) },
    Row { id: "G06-dotall-flag-cannot-bridge-hard-lines", cols: 80, input: "a1b\r\na2b", query: "(?s)a.*b", regex: RE, case: CS, expect: M(&[[0, 0, 0, 3], [1, 0, 1, 3]]) },
    Row { id: "G07-multiline-flag-inert", cols: 80, input: "ab\r\ncd", query: "(?m)^c", regex: RE, case: CS, expect: M(&[[1, 0, 1, 1]]) },
];

/// Every match of `query` over the screen of `terminal`, in document
/// order, as `[start_row, start_col, end_row, end_col]`.
fn matches_on(terminal: &Terminal, query: &Query) -> Vec<[u64; 4]> {
    let grid = terminal.grid();
    let mut found = Vec::new();
    let at = Rows {
        first: 0,
        run_above: 0,
        more_below: false,
    };
    let _ = scan(
        query,
        grid.visible_lines(),
        at,
        segment_period(grid.cols()),
        &mut Haystack::new(),
        |m: Match| {
            found.push([
                m.start_line,
                u64::from(m.start_col),
                m.end_line,
                u64::from(m.end_col),
            ]);
            ControlFlow::Continue(())
        },
    );
    found.sort_unstable();
    found
}

fn terminal(cols: u16, input: &str) -> Terminal {
    let mut terminal = Terminal::new(24, cols);
    terminal.advance(input.as_bytes());
    terminal
}

fn options(row: &Row) -> QueryOptions {
    let case_sensitive = match row.case {
        Case::Sensitive => true,
        Case::Insensitive => false,
        Case::Smart => row.query.chars().any(char::is_uppercase),
    };
    QueryOptions {
        regex: row.regex,
        case_sensitive,
    }
}

fn refusal(error: &QueryError) -> Refusal {
    match error {
        QueryError::TooLong { .. } => TooLong,
        QueryError::Syntax(_) => Syntax,
        QueryError::TooComplex => TooComplex,
    }
}

#[test]
fn search_spec_fixture() {
    let mut failures = Vec::new();
    for row in ROWS {
        let terminal = terminal(row.cols, row.input);
        let got = Query::new(row.query, options(row));
        let ok = match (row.expect, &got) {
            (Expect::Matches(want), Ok(Some(query))) => matches_on(&terminal, query) == want,
            (Expect::Matches(want), Ok(None)) => want.is_empty() && row.query.is_empty(),
            (Expect::Error(want), Err(error)) => refusal(error) == want,
            _ => false,
        };
        if !ok {
            let detail = match &got {
                Ok(Some(query)) => format!("{:?}", matches_on(&terminal, query)),
                Ok(None) => "idle".to_owned(),
                Err(error) => format!("{:?}", refusal(error)),
            };
            failures.push(format!("{}: want {:?}, got {detail}", row.id, row.expect));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a20_width_reflow_same_matches() {
    let query = Query::new("o w", QueryOptions::default()).unwrap().unwrap();
    let mut terminal = terminal(5, "hello world");
    assert_eq!(
        matches_on(&terminal, &query),
        [[0, 4, 1, 2]],
        "at 5 columns"
    );
    terminal.resize(24, 8);
    assert_eq!(
        matches_on(&terminal, &query),
        [[0, 4, 0, 7]],
        "at 8 columns"
    );
    terminal.resize(24, 3);
    // Rows `hel|lo |wor|ld`: the match ends after `w` at (2, 0). The
    // fixture's hand-derived [[1, 1, 3, 1]] counts one row too many.
    assert_eq!(
        matches_on(&terminal, &query),
        [[1, 1, 2, 1]],
        "at 3 columns"
    );
}

/// D20-D23, D27, D28: the query caps, by bytes, and the nest limit.
#[test]
fn query_caps() {
    let options = QueryOptions::default();
    let at_cap = "z".repeat(MAX_QUERY_BYTES);
    assert!(matches!(Query::new(&at_cap, options), Ok(Some(_))), "D20");
    let over = "z".repeat(MAX_QUERY_BYTES + 1);
    assert_eq!(
        Query::new(&over, options).unwrap_err(),
        QueryError::TooLong { bytes: 1025 },
        "D21"
    );
    // 513 two-byte characters: 1026 bytes, refused though only 513 chars.
    let wide = "\u{e9}".repeat(513);
    assert!(
        matches!(
            Query::new(&wide, options),
            Err(QueryError::TooLong { bytes: 1026 })
        ),
        "D22"
    );
    let ci = QueryOptions {
        regex: false,
        case_sensitive: false,
    };
    assert!(
        matches!(Query::new(&"\u{e9}".repeat(512), ci), Ok(Some(_))),
        "D23"
    );

    let re = QueryOptions {
        regex: true,
        case_sensitive: true,
    };
    let nested = |depth: usize| format!("{}a{}", "(".repeat(depth), ")".repeat(depth));
    assert!(
        matches!(Query::new(&nested(250), re), Ok(Some(_))),
        "D27: 250 deep compiles"
    );
    assert!(
        matches!(Query::new(&nested(251), re), Err(QueryError::Syntax(_))),
        "D27"
    );
    // 1023 bytes of `\w+`: refused by the size limit, quickly (never a hang).
    let heavy = r"\w+".repeat(341);
    let started = std::time::Instant::now();
    let result = Query::new(&heavy, re);
    assert!(
        matches!(result, Err(QueryError::TooComplex)),
        "D28: {result:?}"
    );
    assert!(
        started.elapsed().as_secs() < 5,
        "D28 took {:?}",
        started.elapsed()
    );
}

/// A refusal's message never panics and says what is wrong; the syntax
/// message is the regex crate's.
#[test]
fn refusals_explain_themselves() {
    let re = QueryOptions {
        regex: true,
        case_sensitive: true,
    };
    let syntax = Query::new("(", re).unwrap_err();
    assert!(syntax.to_string().contains("unclosed group"), "{syntax}");
    assert!(QueryError::TooComplex.to_string().contains("(?-u:\\w)"));
    assert!(
        QueryError::TooLong { bytes: 2000 }
            .to_string()
            .contains("1024"),
        "names the cap"
    );
}

/// G01: a nested quantifier over 2 000 000 cells of one wrapped line
/// finds nothing, in linear time.
#[test]
fn g01_nested_quantifier_is_linear() {
    let mut terminal = Terminal::new(24, 2048);
    terminal.set_max_scrollback(2000);
    let x = "x".repeat(2048);
    for _ in 0..977 {
        terminal.advance(x.as_bytes());
    }
    let query = Query::new(
        "(x+x+)+y",
        QueryOptions {
            regex: true,
            case_sensitive: true,
        },
    )
    .unwrap()
    .unwrap();
    let grid = terminal.grid();
    let rows: Vec<_> = grid.logical_lines().cloned().collect();
    assert_eq!(rows.len(), 977, "one line wrapped over 977 rows");
    assert!(rows[..976].iter().all(|row| row.wrapped));
    let mut found = 0;
    let started = std::time::Instant::now();
    let _ = scan(
        &query,
        &rows,
        Rows {
            first: 0,
            run_above: 0,
            more_below: false,
        },
        segment_period(2048),
        &mut Haystack::new(),
        |_| {
            found += 1;
            ControlFlow::Continue(())
        },
    );
    assert_eq!(found, 0);
    assert!(
        started.elapsed().as_secs() < 10,
        "took {:?}",
        started.elapsed()
    );
}

/// G04 and F03: the quadratic case of iterating matches stays bounded by
/// the segment cap: 20 000 single-letter matches of `a+b|a` on one line.
#[test]
fn g04_quadratic_iteration_is_bounded() {
    let terminal = terminal(2048, &"a".repeat(20_000));
    let query = Query::new(
        "a+b|a",
        QueryOptions {
            regex: true,
            case_sensitive: true,
        },
    )
    .unwrap()
    .unwrap();
    let started = std::time::Instant::now();
    let found = matches_on(&terminal, &query);
    let elapsed = started.elapsed();
    assert_eq!(found.len(), 20_000);
    assert_eq!(found[0], [0, 0, 0, 1]);
    assert_eq!(found[19_999], [9, 1567, 9, 1568]);
    // The release bar is 1 s; a debug build gets ten times that.
    let bar = if cfg!(debug_assertions) { 10 } else { 1 };
    assert!(elapsed.as_secs() < bar, "took {elapsed:?}");
}
