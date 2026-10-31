//! URL detection in terminal text, for command-hover and command-click
//! (ADR 0025).
//!
//! [`url_at`] finds the URL around one character of a logical line (a line
//! with its soft-wrapped rows joined, ADR 0009) by rules U1 to U9 of
//! `tests/fixtures/selection_spec.notes.md`, without a regular-expression
//! dependency:
//!
//! - **U1** the scheme is `http://`, `https://`, `ftp://`, `file://` or
//!   `mailto:`, ASCII case-insensitive, and is not preceded by an
//!   alphanumeric, `+`, `-` or `.` (so `xhttp://` is no link);
//! - **U2** the URL is returned as typed;
//! - **U3** it runs from the scheme to the first character that is not an
//!   RFC 3986 character (`A-Za-z0-9-._~:/?#[]@!$&'()*+,;=%`) or a non-ASCII
//!   alphanumeric (IRIs): whitespace, controls, `<>"` and the like end it;
//! - **U4** a trailing `.` `,` `;` `:` `!` `?` `'` `"` is stripped, again and
//!   again;
//! - **U5** a trailing `)` or `]` is stripped while the URL holds more of
//!   that closer than of its opener (`Foo_(bar)` keeps its parenthesis,
//!   `(url)` loses the outer one); U4 and U5 repeat until neither applies;
//! - **U6** `http`, `https` and `ftp` need a host character right after
//!   `://` (not `/`, `?`, `#` or the end); `file` may have an empty host
//!   (`file:///etc/hosts`); every URL has something after its scheme;
//! - **U7** `mailto:` needs text, `@`, text; bare e-mail addresses are not
//!   links;
//! - **U8** soft wraps continue a URL (the caller passes the logical line)
//!   and hard line ends stop it;
//! - **U9** a candidate (from the scheme to the end of U3, before U4 and
//!   U5) longer than [`MAX_URL_LEN`] characters is not a link, which also
//!   bounds the cost of one lookup (see [`url_at`] for the bound).
//!
//! Positions are `char` indices into the line. A caller that maps cells to
//! characters passes one `char` per character cell (a grapheme cluster's
//! base scalar, for its class) and keeps its own map back to the cells.

use std::ops::Range;

/// The longest candidate that can be a link (U9), in characters: the
/// 2083-byte URI cap VTE and iTerm2 apply to OSC 8 hyperlinks.
pub const MAX_URL_LEN: usize = 2083;

/// The schemes detected (U1), each with what must follow its name.
const SCHEMES: [(&str, Scheme); 5] = [
    ("https://", Scheme::Host),
    ("http://", Scheme::Host),
    ("ftp://", Scheme::Host),
    ("file://", Scheme::File),
    ("mailto:", Scheme::Mailto),
];

/// What a scheme requires after its prefix (U6, U7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scheme {
    /// A host character first.
    Host,
    /// Anything, an empty host included.
    File,
    /// Text, `@`, text.
    Mailto,
}

/// Whether `c` can be part of a URL (U3): an RFC 3986 character or a
/// non-ASCII alphanumeric.
#[must_use]
pub fn is_url_char(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c)
    } else {
        c.is_alphanumeric()
    }
}

/// The URL that contains character `at` of `line`, as a range of `char`
/// indices into `line` (end exclusive), or `None` when there is none.
///
/// When several candidates contain `at` (a URL quoted inside another's
/// query), the one that starts first and passes every rule wins. Never
/// panics, whatever `line` and `at` are: an `at` past the end gives
/// `None`.
///
/// # Cost
///
/// Reads at most [`MAX_URL_LEN`] characters to either side of `at` (the
/// run of URL characters around it, bounded by U9) and does O(n) work on
/// that window of n characters plus O(log n) per scheme start in it: the
/// bracket counts U5 needs come from prefix sums taken once, not from a
/// recount per stripped character. A lookup in a long line costs what one
/// in a short line does.
#[must_use]
pub fn url_at(line: &[char], at: usize) -> Option<Range<usize>> {
    if !line.get(at).copied().is_some_and(is_url_char) {
        return None;
    }
    // The run of URL characters around `at`, bounded by the cap: any URL
    // containing `at` starts after `at - MAX_URL_LEN` and ends before
    // `at + MAX_URL_LEN`.
    let lowest = at.saturating_sub(MAX_URL_LEN - 1);
    let run_start = line[lowest..at]
        .iter()
        .rposition(|&c| !is_url_char(c))
        .map_or(lowest, |i| lowest + i + 1);
    let highest = line.len().min(at.saturating_add(MAX_URL_LEN + 1));
    let run_end = line[at..highest]
        .iter()
        .position(|&c| !is_url_char(c))
        .map_or(highest, |i| at + i);
    let window = Window::new(&line[run_start..run_end], at - run_start);
    let before = |start: usize| (start > 0).then(|| line[start - 1]);
    (0..=at - run_start).find_map(|start| {
        window
            .candidate(start, before(run_start + start))
            .map(|span| run_start + span.start..run_start + span.end)
    })
}

/// A trailing character U4 strips.
fn is_trailing_punctuation(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"')
}

/// The run of URL characters around the point of a lookup, with what
/// every candidate in it needs precomputed once (O(n)).
struct Window<'a> {
    text: &'a [char],
    /// The point's index in `text`: a span must end after it.
    at: usize,
    /// `paren[i]`: `)` minus `(` in `text[..i]`; `square[i]` the same for
    /// `]` and `[`.
    paren: Vec<i32>,
    square: Vec<i32>,
    /// `next_at[i]`: the first `@` at or after `i` (`text.len()` if none).
    next_at: Vec<usize>,
    /// Where U4 and U5 stop for every candidate, whatever its start: the
    /// end after the last character of `text` that is neither trailing
    /// punctuation nor a closer, if that end still contains the point.
    stop: Option<usize>,
    /// The ends `e` above `stop` where `text[e - 1]` is `)`, from the
    /// last down, each with the least `paren[e]` of it and every end above
    /// it: a candidate whose `paren[start]` is at least that stops at the
    /// first such end (U5 keeps a balanced closer).
    parens: Vec<(usize, i32)>,
    /// The same for `]`.
    squares: Vec<(usize, i32)>,
}

impl<'a> Window<'a> {
    fn new(text: &'a [char], at: usize) -> Self {
        let n = text.len();
        steps(n as u64);
        let mut paren = Vec::with_capacity(n + 1);
        let mut square = Vec::with_capacity(n + 1);
        let (mut p, mut q) = (0, 0);
        paren.push(p);
        square.push(q);
        for &c in text {
            match c {
                ')' => p += 1,
                '(' => p -= 1,
                ']' => q += 1,
                '[' => q -= 1,
                _ => {}
            }
            paren.push(p);
            square.push(q);
        }
        let mut next_at = vec![n; n + 1];
        for i in (0..n).rev() {
            next_at[i] = if text[i] == '@' { i } else { next_at[i + 1] };
        }
        // The walk from the end down to the point's end (`at + 1`).
        let (mut stop, mut parens, mut squares) = (None, Vec::new(), Vec::new());
        let mut least = (i32::MAX, i32::MAX);
        for end in (at + 1..=n).rev() {
            match text[end - 1] {
                c if is_trailing_punctuation(c) => {}
                ')' => {
                    least.0 = least.0.min(paren[end]);
                    parens.push((end, least.0));
                }
                ']' => {
                    least.1 = least.1.min(square[end]);
                    squares.push((end, least.1));
                }
                _ => {
                    stop = Some(end);
                    break;
                }
            }
        }
        Self {
            text,
            at,
            paren,
            square,
            next_at,
            stop,
            parens,
            squares,
        }
    }

    /// The end U4 and U5 trim the candidate starting at `start` to, if it
    /// still contains the point. O(log n).
    fn trimmed_end(&self, start: usize) -> Option<usize> {
        // The first end, walking down, at which a closer is balanced:
        // `)` at `end` stays when `paren[end] - paren[start] <= 0`.
        let first = |closers: &[(usize, i32)], threshold: i32| {
            let k = closers.partition_point(|&(_, least)| least > threshold);
            steps(u64::from(closers.len().bit_width()));
            closers.get(k).map(|&(end, _)| end)
        };
        [
            self.stop,
            first(&self.parens, self.paren[start]),
            first(&self.squares, self.square[start]),
        ]
        .into_iter()
        .flatten()
        .max()
    }

    /// The URL whose candidate starts at `start` (`before` is the
    /// character before it in the line) and runs to the end of the window
    /// (U3), after U1, U4 to U7 and U9, if it contains the point.
    fn candidate(&self, start: usize, before: Option<char>) -> Option<Range<usize>> {
        steps(8);
        if self.text.len() - start > MAX_URL_LEN {
            return None;
        }
        if before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return None;
        }
        let text = &self.text[start..];
        let (prefix, scheme) = SCHEMES.iter().copied().find(|(prefix, _)| {
            text.len() >= prefix.len()
                && text
                    .iter()
                    .zip(prefix.chars())
                    .all(|(&c, p)| c.eq_ignore_ascii_case(&p))
        })?;
        let end = self.trimmed_end(start)?;
        debug_assert!(end > self.at);
        let rest = start + prefix.len();
        if rest > end {
            return None;
        }
        let valid = match scheme {
            Scheme::Host => rest < end && !matches!(self.text[rest], '/' | '?' | '#'),
            Scheme::File => rest < end,
            Scheme::Mailto => {
                let at = self.next_at[rest];
                at > rest && at + 1 < end
            }
        };
        valid.then_some(start..end)
    }
}

#[cfg(test)]
thread_local!(static STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) });

/// Counts `n` steps of work, for the test that bounds a lookup's cost.
#[cfg(test)]
fn steps(n: u64) {
    STEPS.with(|s| s.set(s.get() + n));
}

#[cfg(not(test))]
fn steps(_: u64) {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    fn found(s: &str, at: usize) -> Option<String> {
        let line = chars(s);
        url_at(&line, at).map(|span| line[span].iter().collect())
    }

    /// The reference rules for one candidate `line[start..end]`, written
    /// straight from U1 and U4 to U9: the brackets recounted for every
    /// stripped character.
    fn candidate(line: &[char], start: usize, end: usize) -> Option<Range<usize>> {
        if end - start > MAX_URL_LEN {
            return None;
        }
        if start > 0 {
            let before = line[start - 1];
            if before.is_alphanumeric() || matches!(before, '+' | '-' | '.') {
                return None;
            }
        }
        let text = &line[start..end];
        let (prefix, scheme) = SCHEMES.iter().copied().find(|(prefix, _)| {
            text.len() >= prefix.len()
                && text
                    .iter()
                    .zip(prefix.chars())
                    .all(|(&c, p)| c.eq_ignore_ascii_case(&p))
        })?;
        let mut len = text.len();
        loop {
            match text[..len].last() {
                Some(&c) if is_trailing_punctuation(c) => len -= 1,
                Some(&closer @ (')' | ']')) => {
                    let opener = if closer == ')' { '(' } else { '[' };
                    let count = |c| text[..len].iter().filter(|&&x| x == c).count();
                    if count(closer) > count(opener) {
                        len -= 1;
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        let rest = text.get(prefix.len()..len)?;
        let valid = match scheme {
            Scheme::Host => rest.first().is_some_and(|c| !matches!(c, '/' | '?' | '#')),
            Scheme::File => !rest.is_empty(),
            Scheme::Mailto => rest
                .iter()
                .position(|&c| c == '@')
                .is_some_and(|at| at > 0 && at + 1 < rest.len()),
        };
        valid.then_some(start..start + len)
    }

    /// The reference: every start from the beginning of the line, each
    /// candidate running to the first non-URL character however far, no
    /// window.
    fn reference(line: &[char], at: usize) -> Option<Range<usize>> {
        if at >= line.len() {
            return None;
        }
        (0..=at).find_map(|start| {
            let end = line[start..]
                .iter()
                .position(|&c| !is_url_char(c))
                .map_or(line.len(), |i| start + i);
            if end <= at {
                return None;
            }
            let span = candidate(line, start, end)?;
            span.contains(&at).then_some(span)
        })
    }

    #[test]
    fn the_cap_bounds_the_window_on_both_sides() {
        // A huge token of URL characters: no URL, and the window keeps
        // the lookup short.
        let line: Vec<char> = std::iter::repeat_n('a', 1_000_000).collect();
        assert_eq!(url_at(&line, 500_000), None);
        // A URL right at the end of a long run of slashes is found.
        let mut line: Vec<char> = std::iter::repeat_n('/', 10_000).collect();
        line.extend(chars("https://a.bc/x"));
        assert_eq!(url_at(&line, 10_005), Some(10_000..10_014));
    }

    /// Character steps of one `url_at` call.
    fn cost(line: &[char], at: usize) -> u64 {
        STEPS.with(|s| s.set(0));
        let _ = url_at(line, at);
        STEPS.with(std::cell::Cell::get)
    }

    #[test]
    fn a_lookup_costs_steps_linear_in_its_window() {
        // Many scheme starts followed by unbalanced closers: every
        // candidate strips the closers. A lookup must stay linear in the
        // characters it reads (at most 2 x MAX_URL_LEN + 2), wherever the
        // point is and however the line mixes them.
        for (schemes, closers) in [(110, 381), (220, 763), (60, 1700), (340, 40)] {
            let mut line = chars(&"ftp://".repeat(schemes));
            line.extend(std::iter::repeat_n(')', closers));
            line.extend(chars(" ok"));
            let window = (schemes * 6 + closers) as u64;
            for at in [
                0,
                line.len() / 2,
                schemes * 6 - 1,
                schemes * 6 + closers - 1,
            ] {
                let steps = cost(&line, at);
                assert!(
                    steps <= 64 * window,
                    "{schemes} schemes, {closers} closers, at {at}: {steps} steps for {window} chars"
                );
            }
        }
    }

    #[test]
    fn nested_and_rejected_candidates() {
        // The outer URL wins over one quoted in its query.
        assert_eq!(
            found("https://a.bc/?u=https://d.ef/", 20).as_deref(),
            Some("https://a.bc/?u=https://d.ef/")
        );
        // An outer candidate that fails a rule leaves the inner one.
        assert_eq!(
            found("http:///https://d.ef", 12).as_deref(),
            Some("https://d.ef")
        );
        assert_eq!(found("file://", 2), None);
        assert_eq!(found("mailto:@b", 2), None);
        assert_eq!(found("mailto:a@", 2), None);
        assert_eq!(found("https://.", 2), None);
        assert_eq!(found("", 0), None);
        assert_eq!(found("https://a.bc", 99), None);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// Detection never panics; every span lies within the line,
        /// contains `at`, starts with a supported scheme and is within the
        /// cap; and the windowed lookup agrees with the reference.
        #[test]
        fn detection_is_total_and_agrees_with_the_reference(
            parts in proptest::collection::vec(
                prop_oneof![
                    Just("https://".to_owned()),
                    Just("HTTP://".to_owned()),
                    Just("ftp://".to_owned()),
                    Just("file://".to_owned()),
                    Just("mailto:".to_owned()),
                    "[a-c@./:()\\[\\]?,'\" \u{4e16}\u{3002}x+-]{0,6}",
                ],
                0..12,
            ),
            at in 0usize..80,
        ) {
            let line: Vec<char> = parts.concat().chars().collect();
            let got = url_at(&line, at);
            prop_assert_eq!(got.clone(), reference(&line, at));
            if let Some(span) = got {
                prop_assert!(span.end <= line.len() && span.contains(&at));
                prop_assert!(span.len() <= MAX_URL_LEN);
                let text: String = line[span].iter().collect::<String>().to_ascii_lowercase();
                prop_assert!(SCHEMES.iter().any(|(p, _)| text.starts_with(p)), "{}", text);
            }
        }

        /// Arbitrary characters, positions past the end included.
        #[test]
        fn arbitrary_text_never_panics(text in "\\PC{0,64}", at in 0usize..70) {
            let line: Vec<char> = text.chars().collect();
            prop_assert_eq!(url_at(&line, at), reference(&line, at));
        }
    }
}
