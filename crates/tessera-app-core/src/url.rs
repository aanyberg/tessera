//! URL detection in terminal text, for command-hover and command-click
//! (ADR 0025).
//!
//! [`url_at`] finds the URL around one character of a logical line (a line
//! with its soft-wrapped rows joined, ADR 0009) by rules U1 to U9 of
//! `tests/fixtures/selection_spec.notes.md`, without a regular-expression
//! dependency:
//!
//! - **U1** the scheme is `http://`, `https://`, `ftp://`, `file://` or
//!   `mailto:`, ASCII case-insensitive, and is not preceded by an ASCII
//!   alphanumeric, `+`, `-` or `.` (so `xhttp://` is no link, while
//!   `詳細はhttps://…` is one: CJK prose has no spaces before a URL);
//! - **U2** the URL is returned as typed;
//! - **U3** it runs from the scheme to the first character that is not an
//!   RFC 3986 character (`A-Za-z0-9-._~:/?#[]@!$&'()*+,;=%`) or a non-ASCII
//!   alphanumeric (IRIs): whitespace, controls, `<>"` and the like end it.
//!   After the host (the text from the scheme to the first `/`, `?` or `#`),
//!   a CJK character also ends it (see [`is_cjk`]): Han, kana and Hangul
//!   letters and CJK or fullwidth punctuation, so `https://a.jp/を参照` stops
//!   before `を`. In the host a CJK character ends it only right after an
//!   ASCII label or port (letters, digits and `-` after a `.` or `:` of the
//!   host): `https://example.comを参照` stops before `を`, while a label
//!   that starts with CJK keeps it (`https://例え.jp/`,
//!   `https://www.例え.テスト`). A path written in raw CJK (`/wiki/日本`) is
//!   cut where the CJK starts; a percent-encoded one is not. The zero-width
//!   joiner and the bidirectional controls are no URL characters, so they
//!   end it too (see [`is_hidden_url_end`] for the cells that hide them;
//!   other zero-width scalars can stay in a link's text);
//! - **U4** a trailing `.` `,` `;` `:` `!` `?` `'` `"` is stripped, again and
//!   again, and so is a trailing `-` of a URL that a CJK character ended
//!   (`https://a.jp-日本` is `https://a.jp`);
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

/// The most characters from a scheme's first to its `:` (`mailto:`).
pub(crate) const COLON_REACH: usize = 6;

/// The most distinct candidate ends one lookup can have ([`Window`]'s
/// `trims`).
const MAX_TRIMS: usize = 5;

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

/// Bit `b` is set when ASCII `b` is an RFC 3986 character
/// (`A-Za-z0-9-._~:/?#[]@!$&'()*+,;=%`).
const ASCII_URL_CHARS: u128 = {
    let class = b"-._~:/?#[]@!$&'()*+,;=%";
    let mut mask = 0u128;
    let mut b = 0u8;
    while b < 128 {
        if b.is_ascii_alphanumeric() {
            mask |= 1 << b;
        }
        b += 1;
    }
    let mut i = 0;
    while i < class.len() {
        mask |= 1 << class[i];
        i += 1;
    }
    mask
};

/// Whether `c` can be part of a URL (U3): an RFC 3986 character or a
/// non-ASCII alphanumeric.
#[must_use]
pub fn is_url_char(c: char) -> bool {
    if c.is_ascii() {
        // One bit test, not a scan of the class string: the hover walk
        // asks this of every cell it reads.
        (ASCII_URL_CHARS >> u32::from(c)) & 1 != 0
    } else {
        c.is_alphanumeric()
    }
}

/// Whether `c` ends a URL after its host (U3): a character of one of the
/// blocks of Han ideographs (with their radicals, strokes and
/// compatibility forms), Hiragana, Katakana (halfwidth included),
/// Bopomofo (U+3100-U+312F, extended U+31A0-U+31BF), Hangul (jamo,
/// syllables, halfwidth), CJK symbols and punctuation (U+3000-U+303F,
/// enclosed and compatibility U+3200-U+33FF) or the fullwidth and
/// halfwidth forms (U+FF00-U+FFEF). Blocks, not Unicode scripts: no table
/// dependency, and a block's punctuation and letters are treated alike.
#[must_use]
pub fn is_cjk(c: char) -> bool {
    // Most of what this is asked about is ASCII: one compare.
    if u32::from(c) < 0x1100 {
        return false;
    }
    matches!(
        u32::from(c),
        0x1100..=0x11FF
            | 0x2E80..=0x2FDF
            | 0x3000..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7FF
            | 0xF900..=0xFAFF
            | 0xFF00..=0xFFEF
            | 0x1_B000..=0x1_B16F
            | 0x2_0000..=0x3_FFFF
    )
}

/// Whether `c`, joined to the character before it in a grapheme cluster,
/// ends a URL there (U3): the zero-width joiner (U+200D) and the
/// bidirectional controls (U+061C, U+200E, U+200F, U+202A-U+202E,
/// U+2066-U+2069). None is a URL character, but a terminal cell keeps a
/// zero-width scalar in the cluster of the character before it, so a
/// caller that passes one `char` per cell (the cluster's base) passes a
/// cell holding one of these as a character that is not a URL character:
/// `https://go` + ZWJ + `od.com` must not open a host its text hides.
/// Other zero-width scalars that join a cluster (ZWNJ, variation
/// selectors, CGJ, tag characters) are no hidden ends: they stay in a
/// link's text, unseen, but make no host its text does not show (UTS 46
/// ignores VS and CGJ, CONTEXTJ rejects ZWNJ after Latin, tags are invalid).
#[must_use]
pub fn is_hidden_url_end(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200D}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
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
/// run of URL characters around it, bounded by U9). Without a `:` between
/// the run's start and a scheme's reach past `at` there is no candidate,
/// and the lookup ends there without allocating (a hover over plain text).
/// Otherwise it does O(n) work on that window of n characters plus
/// O(log n) per scheme start in it: the bracket counts U5 needs are
/// counted once as the lookup walks the starts, not recounted per
/// stripped character, and it allocates nothing unless a candidate ends
/// in closers. A lookup in a long line costs what one in a short line
/// does.
#[must_use]
pub fn url_at(line: &[char], at: usize) -> Option<Range<usize>> {
    if !line.get(at).copied().is_some_and(is_url_char) {
        return None;
    }
    url_around(line, at)
}

/// [`url_at`] for a point on a URL character. Kept out of line so that
/// the common miss (a hover over a space or a symbol) runs only the check
/// above, without this body's setup.
#[inline(never)]
fn url_around(line: &[char], at: usize) -> Option<Range<usize>> {
    // The run of URL characters around `at`, bounded by the cap: any URL
    // containing `at` starts after `at - MAX_URL_LEN` and ends before
    // `at + MAX_URL_LEN`.
    let lowest = at.saturating_sub(MAX_URL_LEN - 1);
    let run_start = line[lowest..at]
        .iter()
        .rposition(|&c| !is_url_char(c))
        .map_or(lowest, |i| lowest + i + 1);
    // A candidate containing `at` starts at or before it and has its
    // scheme's `:` at most `COLON_REACH` characters after its start.
    let reach = line.len().min(at + COLON_REACH + 1);
    if !line[run_start..reach].contains(&':') {
        steps((reach - run_start) as u64);
        return None;
    }
    let highest = line.len().min(at.saturating_add(MAX_URL_LEN + 1));
    let run_end = line[at..highest]
        .iter()
        .position(|&c| !is_url_char(c))
        .map_or(highest, |i| at + i);
    let mut window = Window::new(&line[run_start..run_end], at - run_start);
    loop {
        let start = run_start + window.start;
        let before = (start > 0).then(|| line[start - 1]);
        if let Some(span) = window.candidate(before) {
            return Some(run_start + span.start..run_start + span.end);
        }
        if start == at {
            return None;
        }
        window.advance();
    }
}

/// A trailing character U4 strips.
pub(crate) fn is_trailing_punctuation(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"')
}

/// The first position at or after a query where a predicate holds, for
/// queries that only move forward: matched schemes never overlap (no
/// scheme's tail starts another), so the scheme ends a lookup asks about,
/// the host ends after them and the `@` after them only increase, and the
/// scans of one lookup cover the window about once.
#[derive(Debug, Clone, Copy)]
struct Next {
    /// The last query.
    from: usize,
    /// The first match at or after `from` (the window's length if none).
    found: usize,
}

impl Next {
    const NONE: Self = Self {
        from: usize::MAX,
        found: 0,
    };

    /// The first `i >= q` in `text` with `pred(text[i])`, or `text.len()`.
    fn find(&mut self, text: &[char], q: usize, pred: impl Fn(char) -> bool) -> usize {
        if (self.from..=self.found).contains(&q) {
            return self.found;
        }
        let tail = text.get(q..).unwrap_or_default();
        self.from = q;
        self.found = tail
            .iter()
            .position(|&c| pred(c))
            .map_or(text.len(), |i| q + i);
        steps((self.found - q + 1) as u64);
        self.found
    }
}

/// Where U4 and U5 stop for the candidates that end (U3) at `end`.
#[derive(Debug)]
struct Trim {
    end: usize,
    /// The end after the last character of `text[..end]` that is neither
    /// trailing punctuation nor a closer, if that end still contains the
    /// point.
    stop: Option<usize>,
    /// (`stop` and the closers count a trailing `-` as punctuation when
    /// `end` is a CJK cut, U4.)
    ///
    /// The ends `e` above `stop` where `text[e - 1]` is `)`, from the
    /// last down, each with the least `)`-minus-`(` count of `text[..e]`
    /// at it and every such end above it: a candidate whose count at its
    /// start is at least that stops at the first such end (U5 keeps a
    /// balanced closer).
    parens: Vec<(usize, i32)>,
    /// The same for `]` and `[`.
    squares: Vec<(usize, i32)>,
}

/// The run of URL characters around the point of a lookup and what its
/// candidates need, found as the lookup walks the starts from the first:
/// no buffer per character, so a lookup allocates nothing unless a URL
/// ends in closers.
struct Window<'a> {
    text: &'a [char],
    /// The point's index in `text`: a span must end after it.
    at: usize,
    /// The start the walk is at, and `)` minus `(` and `]` minus `[` in
    /// `text[..start]`.
    start: usize,
    paren: i32,
    square: i32,
    /// The next `/`, `?` or `#` (a host's end), CJK character and `@`.
    sep: Next,
    cjk: Next,
    mail: Next,
    /// Where the search for the next candidate's CJK cut resumes: every
    /// position before it was no cut for an earlier candidate and is none
    /// for a later one (see [`Window::cut`]).
    cut_from: usize,
    /// The last CJK character [`Window::cut`] looked at in a host, with the
    /// `.` or `:` before the ASCII label it follows, if any.
    label: Option<(usize, Option<usize>)>,
    /// The trims of the candidate ends seen so far. The ends are few: for
    /// the starts whose scheme ends at or before the point, a candidate
    /// ends at the first CJK character after the point (its host ended
    /// before the point), or, when the host runs on past the point, at the
    /// first CJK character there that follows an ASCII label of the host
    /// or at the next one: three ends. Only starts within a scheme's
    /// length of the point (from 6 before it) add others, and as schemes
    /// are at least 6 long and never overlap there are at most two: five
    /// in all. The first is kept inline.
    trim: Option<Trim>,
    more: Vec<Trim>,
}

/// `)` minus `(` and `]` minus `[` of one character.
fn balance(c: char) -> (i32, i32) {
    match c {
        ')' => (1, 0),
        '(' => (-1, 0),
        ']' => (0, 1),
        '[' => (0, -1),
        _ => (0, 0),
    }
}

impl<'a> Window<'a> {
    fn new(text: &'a [char], at: usize) -> Self {
        Self {
            text,
            at,
            start: 0,
            paren: 0,
            square: 0,
            sep: Next::NONE,
            cjk: Next::NONE,
            mail: Next::NONE,
            cut_from: 0,
            label: None,
            trim: None,
            more: Vec::new(),
        }
    }

    /// Moves the walk to the next start.
    fn advance(&mut self) {
        let (p, q) = balance(self.text[self.start]);
        self.paren += p;
        self.square += q;
        self.start += 1;
    }

    /// The trim of candidates ending at `end` (> the point), computed on
    /// first use: the walk from `end` down to the point's end, with the
    /// bracket counts of `text[..end]` from those at the walk's start.
    fn trim(&mut self, end: usize) -> &Trim {
        if self.trim.as_ref().is_some_and(|t| t.end == end) {
            return self.trim.as_ref().expect("checked");
        }
        if let Some(i) = self.more.iter().position(|t| t.end == end) {
            return &self.more[i];
        }
        steps((end - self.start) as u64);
        let (mut p, mut q) =
            self.text[self.start..end]
                .iter()
                .fold((self.paren, self.square), |(p, q), &c| {
                    let (dp, dq) = balance(c);
                    (p + dp, q + dq)
                });
        let (mut stop, mut parens, mut squares) = (None, Vec::new(), Vec::new());
        let mut least = (i32::MAX, i32::MAX);
        // A candidate ends inside the window only at a CJK cut (U3).
        let cut = end < self.text.len();
        for e in (self.at + 1..=end).rev() {
            steps(1);
            // `p` and `q` are the counts of `text[..e]`.
            let c = self.text[e - 1];
            match c {
                c if is_trailing_punctuation(c) || (cut && c == '-') => {}
                ')' => {
                    least.0 = least.0.min(p);
                    parens.push((e, least.0));
                }
                ']' => {
                    least.1 = least.1.min(q);
                    squares.push((e, least.1));
                }
                _ => {
                    stop = Some(e);
                    break;
                }
            }
            let (dp, dq) = balance(c);
            p -= dp;
            q -= dq;
        }
        let trim = Trim {
            end,
            stop,
            parens,
            squares,
        };
        if self.trim.is_none() {
            return self.trim.insert(trim);
        }
        debug_assert!(self.more.len() + 1 < MAX_TRIMS, "a sixth candidate end");
        self.more.push(trim);
        &self.more[self.more.len() - 1]
    }

    /// The end U4 and U5 trim the candidate at the walk's start and ending
    /// (U3) at `end` to, if it still contains the point. O(log n) once the
    /// end's trim is known.
    fn trimmed_end(&mut self, end: usize) -> Option<usize> {
        let (paren, square) = (self.paren, self.square);
        let trim = self.trim(end);
        // The first end, walking down, at which a closer is balanced:
        // `)` at `e` stays when its count at `e` minus the start's is <= 0.
        let first = |closers: &[(usize, i32)], threshold: i32| {
            let k = closers.partition_point(|&(_, least)| least > threshold);
            steps(u64::from(closers.len().bit_width()));
            closers.get(k).map(|&(end, _)| end)
        };
        [
            trim.stop,
            first(&trim.parens, paren),
            first(&trim.squares, square),
        ]
        .into_iter()
        .flatten()
        .max()
    }

    /// Where the CJK cut (U3) ends the candidate whose host runs from
    /// `rest` to `host_end`: the first CJK character from `rest` on that is
    /// past the host, or in it right after an ASCII label or port (LDH
    /// characters after a `.` or `:` at or after `rest`); the window's
    /// length if none.
    ///
    /// The candidates of one lookup ask with increasing `rest` and
    /// `host_end` (schemes never overlap), and a position that is no cut
    /// for one candidate is none for a later one (its host covers the
    /// position too, and the label's `.` or `:` is still before `rest`), so
    /// the search resumes where the last one stopped: all candidates
    /// together read the window about once.
    fn cut(&mut self, rest: usize, host_end: usize) -> usize {
        let mut i = self.cut_from.max(rest);
        loop {
            i = self.cjk.find(self.text, i, is_cjk);
            if i >= self.text.len() || i >= host_end {
                break;
            }
            let sep = match self.label {
                Some((at, sep)) if at == i => sep,
                _ => {
                    let run = self.text[..i]
                        .iter()
                        .rev()
                        .take_while(|c| c.is_ascii_alphanumeric() || **c == '-')
                        .count();
                    steps(run as u64 + 1);
                    let sep = (run > 0 && i > run)
                        .then(|| i - run - 1)
                        .filter(|&j| matches!(self.text[j], '.' | ':'));
                    self.label = Some((i, sep));
                    sep
                }
            };
            if sep.is_some_and(|j| j >= rest) {
                break;
            }
            i += 1;
        }
        self.cut_from = i;
        i
    }

    /// The URL whose candidate starts at the walk's start (`before` is the
    /// character before it in the line) and runs to the end of the window
    /// or the first CJK character after its host (U3), after U1, U4 to U7
    /// and U9, if it contains the point.
    fn candidate(&mut self, before: Option<char>) -> Option<Range<usize>> {
        steps(8);
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return None;
        }
        let start = self.start;
        let text = &self.text[start..];
        let (prefix, scheme) = SCHEMES.iter().copied().find(|(prefix, _)| {
            text.len() >= prefix.len()
                && text
                    .iter()
                    .zip(prefix.chars())
                    .all(|(&c, p)| c.eq_ignore_ascii_case(&p))
        })?;
        let rest = start + prefix.len();
        let host_end = self
            .sep
            .find(self.text, rest, |c| matches!(c, '/' | '?' | '#'));
        let cut = self.cut(rest, host_end);
        if cut <= self.at || cut - start > MAX_URL_LEN {
            return None;
        }
        let end = self.trimmed_end(cut)?;
        debug_assert!(end > self.at);
        if rest > end {
            return None;
        }
        let valid = match scheme {
            Scheme::Host => rest < end && !matches!(self.text[rest], '/' | '?' | '#'),
            Scheme::File => rest < end,
            Scheme::Mailto => {
                let at = self.mail.find(self.text, rest, |c| c == '@');
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

    /// The reference rules for one candidate starting at `start` whose run
    /// of URL characters ends at `end`, written straight from U1 and U3 to
    /// U9: the host and the CJK cut found by a scan, the brackets recounted
    /// for every stripped character.
    fn candidate(line: &[char], start: usize, end: usize) -> Option<Range<usize>> {
        if start > 0 {
            let before = line[start - 1];
            if before.is_ascii_alphanumeric() || matches!(before, '+' | '-' | '.') {
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
        // U3: after the host (up to the first `/`, `?` or `#`) CJK ends it;
        // in the host, CJK right after an ASCII label or port of the host
        // (LDH characters after a `.` or `:` past the scheme) ends it.
        let host_end = (prefix.len()..text.len())
            .find(|&i| matches!(text[i], '/' | '?' | '#'))
            .unwrap_or(text.len());
        let after_label = |i: usize| {
            let label = text[..i]
                .iter()
                .rposition(|c| !(c.is_ascii_alphanumeric() || *c == '-'))
                .map_or(0, |k| k + 1);
            label < i && label > prefix.len() && matches!(text[label - 1], '.' | ':')
        };
        let cut = (prefix.len()..host_end)
            .find(|&i| is_cjk(text[i]) && after_label(i))
            .or_else(|| (host_end..text.len()).find(|&i| is_cjk(text[i])))
            .unwrap_or(text.len());
        let at_cut = cut < text.len();
        let text = &text[..cut];
        if text.len() > MAX_URL_LEN {
            return None;
        }
        let mut len = text.len();
        loop {
            match text[..len].last() {
                Some(&c) if is_trailing_punctuation(c) || (at_cut && c == '-') => len -= 1,
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
        let scheme_at = |start: usize| {
            SCHEMES.iter().any(|(prefix, _)| {
                line.len() - start >= prefix.len()
                    && line[start..]
                        .iter()
                        .zip(prefix.chars())
                        .all(|(&c, p)| c.eq_ignore_ascii_case(&p))
            })
        };
        (0..=at)
            .filter(|&start| scheme_at(start))
            .find_map(|start| {
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

    #[test]
    fn the_cap_holds_wherever_the_point_is() {
        // U9 at its edge, with the point on the first, a middle and the
        // last character: the window must read one character past the cap
        // to see that a run is too long.
        for (extra, want) in [(2_070, true), (2_071, false)] {
            let line = chars(&format!("https://a.bc/{}", "a".repeat(extra)));
            for at in [0, 1, 100, line.len() - 1] {
                let span = url_at(&line, at);
                assert_eq!(span.is_some(), want, "{} chars, at {at}", line.len());
                assert_eq!(span, reference(&line, at));
            }
        }
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
    fn the_cjk_cut_reads_the_window_about_once() {
        // Many `mailto:` starts (no host separator), all sharing one long
        // host with ASCII labels and CJK after them: the cut search resumes
        // where the last candidate's stopped.
        let mut labels = chars(&"mailto:".repeat(150));
        for _ in 0..60 {
            labels.extend(chars(".abcdefghij日"));
        }
        // The same starts with a long IDN label first: every CJK character
        // in it is no cut, for every candidate.
        let mut idn = chars(&"mailto:".repeat(150));
        idn.extend(std::iter::repeat_n('日', 1_500));
        idn.extend(chars(".jp日"));
        for line in [labels, idn] {
            let window = line.len() as u64;
            for at in [0, 500, line.len() / 2, line.len() - 1] {
                let steps = cost(&line, at);
                assert!(
                    steps <= 64 * window,
                    "at {at}: {steps} steps for {window} chars"
                );
                assert_eq!(url_at(&line, at), reference(&line, at), "at {at}");
            }
        }
    }

    #[test]
    fn the_label_cache_reads_a_long_label_once() {
        // 297 `mailto:` starts sharing one host whose last label is 2 000
        // letters long before a CJK character: every candidate asks about
        // that CJK character, and without the cached label each would walk
        // the 2 000 letters back again (about 150 steps per character).
        let mut line = chars(&"mailto:".repeat(297));
        line.push('.');
        line.extend(std::iter::repeat_n('a', 2_000));
        line.push('日');
        let window = line.len() as u64;
        for at in [0, 500, line.len() / 2, line.len() - 2] {
            let steps = cost(&line, at);
            assert!(
                steps <= 20 * window,
                "at {at}: {steps} steps for {window} chars"
            );
            assert_eq!(url_at(&line, at), reference(&line, at), "at {at}");
        }
    }

    #[test]
    fn the_ascii_url_class_is_the_rfc_3986_set() {
        for b in 0u8..128 {
            let c = char::from(b);
            let want = c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c);
            assert_eq!(is_url_char(c), want, "{c:?}");
        }
    }

    #[test]
    fn hidden_url_ends_are_no_url_characters() {
        for c in [
            '\u{061C}', '\u{200D}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202E}', '\u{2066}',
            '\u{2069}',
        ] {
            assert!(is_hidden_url_end(c), "{c:?}");
            assert!(!is_url_char(c), "{c:?}");
        }
        // A combining mark stays in the cluster's URL (an IRI's `é`).
        for c in ['\u{0301}', '\u{202F}', '\u{2065}', '\u{206A}', 'a', '-'] {
            assert!(!is_hidden_url_end(c), "{c:?}");
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

    #[test]
    fn cjk_prose_around_a_url() {
        // U1: CJK before the scheme does not block it; U3: after the host
        // CJK text and punctuation end it, in the host they do not.
        let cases = [
            (
                "詳細はhttps://example.com/を参照",
                5,
                Some("https://example.com/"),
            ),
            ("https://a.jp/パス", 3, Some("https://a.jp/")),
            ("https://a.jp/x、次", 3, Some("https://a.jp/x")),
            ("https://a.jp/x！", 3, Some("https://a.jp/x")),
            ("https://例え.jp/", 9, Some("https://例え.jp/")),
            ("https://例え.jp/日本", 9, Some("https://例え.jp/")),
            ("https://a.jp?q=日本", 3, Some("https://a.jp?q=")),
            (
                "https://ja.wikipedia.org/wiki/日本",
                3,
                Some("https://ja.wikipedia.org/wiki/"),
            ),
            (
                "https://ja.wikipedia.org/wiki/%E6%97%A5",
                3,
                Some("https://ja.wikipedia.org/wiki/%E6%97%A5"),
            ),
            (
                "mailto:a@b.jp?subject=件名",
                3,
                Some("mailto:a@b.jp?subject="),
            ),
            ("éhttps://a.bc/", 3, Some("https://a.bc/")),
            // In the host, CJK right after an ASCII label or port ends it;
            // IDN labels keep theirs, and so does the user before `@`.
            (
                "詳細はhttps://example.comを参照",
                5,
                Some("https://example.com"),
            ),
            (
                "https://github.comで公開しています",
                3,
                Some("https://github.com"),
            ),
            (
                "mailto:info@example.jpまでご連絡ください",
                3,
                Some("mailto:info@example.jp"),
            ),
            ("https://a.jp:8080日本", 3, Some("https://a.jp:8080")),
            ("https://例え.テスト", 9, Some("https://例え.テスト")),
            ("https://www.例え.jp/", 3, Some("https://www.例え.jp/")),
            ("https://u:p@例え.jp/", 13, Some("https://u:p@例え.jp/")),
            ("https://abc日本.jp/", 3, Some("https://abc日本.jp/")),
            // The `:` before the label is the scheme's, not the host's.
            ("mailto:abc日本@b.jp", 3, Some("mailto:abc日本@b.jp")),
            ("https://ex-1.com한국", 3, Some("https://ex-1.com")),
            // Hangul, fullwidth Latin and an Extension B ideograph in a path.
            ("https://a.kr/에서", 3, Some("https://a.kr/")),
            ("https://a.jp/ａｂｃ", 3, Some("https://a.jp/")),
            ("https://a.jp/x\u{20000}", 3, Some("https://a.jp/x")),
            // Hangul Jamo, first and last, are CJK; the letter just below
            // the block is not.
            ("https://a.kr/x\u{1100}", 3, Some("https://a.kr/x")),
            ("https://a.kr/x\u{11FF}", 3, Some("https://a.kr/x")),
            ("https://a.kr/x\u{10FF}", 3, Some("https://a.kr/x\u{10FF}")),
            // Bopomofo and its extension are CJK.
            ("https://a.tw/ㄅㄆ", 3, Some("https://a.tw/")),
            ("https://a.twㄅㄆ", 3, Some("https://a.tw")),
            ("https://a.tw/x\u{31A0}", 3, Some("https://a.tw/x")),
            // A `-` inside the label before the CJK is the label's; a
            // trailing one is dropped at the cut, and only there.
            ("https://x.co-jp日本", 3, Some("https://x.co-jp")),
            ("https://a.jp-日本", 3, Some("https://a.jp")),
            ("https://a.jp/x--日", 3, Some("https://a.jp/x")),
            ("https://a.jp/x-.日", 3, Some("https://a.jp/x")),
            ("https://a.jp/x-", 3, Some("https://a.jp/x-")),
            ("https://a.jp/x- 日", 3, Some("https://a.jp/x-")),
            // The ZWJ and bidi controls end it.
            ("https://go\u{200D}od.com/", 3, Some("https://go")),
            ("https://a.com/\u{202E}moc.live", 3, Some("https://a.com/")),
            // The accepted costs of E1 (ADR 0025 U3): a CJK character right
            // after an ASCII label cuts an IDN label that mixes the two, and
            // a host with no `.` or `:` before its CJK (or a `.` right before
            // it) keeps the prose.
            ("https://www.a日本.jp", 3, Some("https://www.a")),
            ("https://localhostで", 3, Some("https://localhostで")),
            ("https://[::1]日本", 3, Some("https://[::1]日本")),
            ("https://example.com.を", 3, Some("https://example.com.を")),
            ("https://u:p日本@x.jp/", 3, Some("https://u:p")),
            ("mailto:a.b日本@x.jp", 3, None),
            ("xhttps://a.bc/", 3, None),
            // A point on the cut CJK text is outside the URL.
            ("https://a.jp/を参照", 14, None),
        ];
        for (text, at, want) in cases {
            assert_eq!(found(text, at).as_deref(), want, "{text} at {at}");
            assert_eq!(
                url_at(&chars(text), at),
                reference(&chars(text), at),
                "{text}"
            );
        }
    }

    #[test]
    fn candidate_ends_stay_few() {
        // Scheme starts all around the point, each host ending at another
        // separator followed by CJK: the trims cached stay within the bound
        // (the debug assertion in `Window::trim` checks it).
        let mut line = chars(&"https://a/日".repeat(30));
        line.extend(chars("mailto:x@y/日file://z/日https://w"));
        for at in 0..line.len() {
            assert_eq!(url_at(&line, at), reference(&line, at), "at {at}");
        }
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
                    "[a-c@./:()\\[\\]?#,'\" \u{4e16}\u{3002}\u{3092}\u{ff01}\u{e9}\u{43f}\u{d55c}\u{ff41}\u{20000}\u{3105}\u{200d}x+-]{0,6}",
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

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Lines of 2 000 to 4 500 characters, heavy in brackets and
        /// trailing punctuation, with scheme starts and CJK among them:
        /// longer than two caps, so the window's bounds and the cap are
        /// what the reference (no window) is held against.
        #[test]
        fn long_bracket_heavy_lines_agree_with_the_reference(
            parts in proptest::collection::vec(
                prop_oneof![
                    3 => Just("https://".to_owned()),
                    1 => Just("mailto:".to_owned()),
                    1 => Just("file://".to_owned()),
                    8 => "[a()\\[\\]/.,:?#@-]{1,40}",
                    1 => "[\u{65e5}\u{3092} ]{1,3}",
                ],
                80..140,
            ),
            points in proptest::collection::vec(0usize..4_600, 4),
        ) {
            let mut line: Vec<char> = parts.concat().chars().collect();
            line.truncate(4_500);
            while line.len() < 2_000 {
                line.push(if line.len().is_multiple_of(3) { ')' } else { 'a' });
            }
            for at in points {
                prop_assert_eq!(url_at(&line, at), reference(&line, at), "at {}", at);
            }
        }
    }
}
