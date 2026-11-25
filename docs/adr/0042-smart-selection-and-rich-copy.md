# ADR 0042: Smart selection rules and Copy with Formatting

**Status:** accepted · 2026-11-08 · amended 2026-11-13 (#351: click budget, path marks,
rich-copy output); amended 2026-11-18 (#433: a per-match click budget, single-walk
rich copy); amended 2026-11-24 (#557: the click clock read every 16 matches)

## Context

Issue #47 asks for two selection features that iTerm2, kitty and
Terminal.app users expect: a double-click that selects a whole path, URL,
email address, hash or IP address instead of the run of word characters
under the pointer, and a copy that keeps the colours and attributes of the
text so it pastes styled into a mail or a document. Both read text the
child wrote: a double-click runs patterns over it, and the formatted copy
turns it into HTML and RTF that another application parses. Both therefore
need caps (security rule S5) and the copy needs escaping no hostile cell
text can get past.

The behaviour is pinned by the spec-researcher's fixture (wave 13, spec-t9,
from iTerm2's SmartSelectionRules.plist, kitty, Alacritty and the HTML and
RTF specifications), copied to
`crates/tessera-app-core/tests/fixtures/smart_selection_spec.txt` and run
by `crates/tessera-app-core/tests/smart_selection.rs`. The lead adopted its
decisions as D10 and D15 (wave 13). Selection itself is ADR 0025.

## Decision

**The gesture (D15).** A double-click is the smart gesture: the core
selects the best smart-selection match that covers the clicked character,
and the word (ADR 0025) when none does. Only the anchor is smart: a drag
after the double-click extends by words, and a shift-click adjusts by
words, as before. A triple-click still selects the line. There is no
separate modifier gesture and no quad-click.

**Rules (`[selection] smart_rules`).** A list of `{ regex, precision }`
tables in the `regex` crate's syntax; precision is `very_low`, `low`,
`normal`, `high` or `very_high`, with iTerm2's weights (10, 10^3, 10^6,
10^9, 10^12 after scaling by a million). Leaving the key out uses the
built-in rules; writing it replaces them (an empty list turns smart
selection off). The built-in rules, in order:

| # | Rule | Precision |
|---|---|---|
| 0 | C++ names `(?:[A-Za-z0-9_]+::)+[A-Za-z0-9_]+` | normal |
| 1 | Paths `~?/?(?:[\p{L}\p{N}._-]+/+)+[\p{L}\p{N}._-]+/?` | normal |
| 2 | Double-quoted strings `@?"(?:[^"\\]\|\\.)*"` | normal |
| 3 | Dotted names `(?:[\p{L}\p{M}\p{N}._]+\.)+[\p{L}\p{M}\p{N}._]+` | normal |
| 4 | `mailto:` | normal |
| 5 | `@selector(...)` | high |
| 6 | Email addresses, TLD of 2 to 4 letters (D15) | high |
| 7 | `http` / `https` URLs, then U4/U5 trimming | very high |
| 8 | `ssh:` | very high |
| 9 | `telnet:` | very high |
| 10 | Git object names: 40 or 64 hex digits, or 7 to 12 | normal |
| 11 | IPv4 addresses, each octet 0-255 | high |
| 12-14 | IPv6: eight groups; compressed from a group (`fe80::1`), not followed by a hex digit or `:`; from `::` (`::1`), not after a letter, digit or `:` | high |

iTerm2's `\S+` catch-all is left out (D15), so a double-click on ordinary
text keeps selecting the word. The URL rule is one pattern plus the URL
detection's trailing-punctuation rules U4 and U5 (`url.rs`, reused, not
duplicated): a trailing `.,;:!?'"` is cut, and so is a `)` or `]` while
unbalanced. The `regex` crate has no look-around, so the IPv6 boundaries
and the URL trim are checks the matcher runs after the regex
(`RuleCheck`); a rule gets a check only when its pattern is byte for byte
a built-in one, so a user copy of the defaults behaves the same and a
user's own pattern gets none.

**Choosing a match.** Each rule finds its leftmost-first, non-overlapping
matches in a window of the logical line (soft wraps joined); a match that
does not cover the clicked character, is empty or fails its check does not
count. The best match has the highest weight × length in characters; ties
go to the higher precision, then the longer match, then the earlier start,
then the earlier rule (D15). Where the line goes on past an edge of the
window, a match that starts (at the left edge) or ends (at the right) in
the unbroken run of non-blank characters at that edge is dropped rather
than cut, since the regex may have found only part of a longer match, so a
click never selects half of something. A rule whose matches span blanks
(a quoted string) can still pair up differently in a window than on the
whole line.

**`[selection] trim_trailing_whitespace`.** `true` (the default) is Copy's
behaviour from ADR 0025: the blanks after each line's last text are
dropped. `false` keeps every selected cell as a space up to the last row
that holds text, and drops the blank rows after it (D15); a tab still
copies as the spaces it moved over. The setting applies to Copy and to
Copy with Formatting alike.

**Copy with Formatting (D10).** A new registry command
`copy_with_formatting` (Edit menu, right after Copy, no default chord)
puts the selection on the pasteboard in one write as RTF, HTML and the
plain string. The core makes and escapes both formats
(`tessera-app-core/src/rich_text.rs`); the host puts the bytes on the
pasteboard as they are and never parses or imports HTML.

- Runs: neighbouring cells with the same look are one run. A run carries
  the foreground and background colour resolved through the session's
  theme and OSC 10/11/4 overrides; the default foreground and background
  get no colour (D15), so the paste takes the document's. Inverse swaps
  the two, with the defaults then resolved to the theme's colours. Bold,
  italic, any underline style and strikethrough are written; dim, blink,
  hidden and overline are not. Bold is not brightened.
- HTML: a UTF-8 document whose body is one
  `<pre style="font-family:Menlo,monospace">`, one `<span style=...>`
  (`color`, `background-color`, `font-weight`, `font-style`,
  `text-decoration`) per styled run, a line feed per line break. Every
  `<`, `>`, `&`, `"`, `'`, `\`, `{` and `}` and every scalar above U+007E
  is a named or numeric character reference, so the document is ASCII;
  there is no `<a>`, no `href`, no `src`, no `url(` and no external
  resource of any kind, OSC 8 links included (the copy carries the text
  only).
- RTF: `{\rtf1\ansi\ansicpg1252\deff0` with a one-font table (`Menlo`,
  `\fs24`, D15), a colour table in first-use order, `\cfN`, `\cbN`, `\b`,
  `\i`, `\ul` and `\strike` in groups, `\par` per line break; no `\field`,
  `\pict`, `\object` or `\*` destination. `\`, `{` and `}` are escaped
  and every scalar above U+007E is `\uN?` (signed UTF-16 units, surrogate
  pairs for the astral planes).
- Control characters in cell text (C0, DEL) are dropped from both
  formats; line breaks come only from the selection's rows, and a tab is
  the blank cells it moved over, as in the plain text.
- The plain string is exactly what Copy copies.

The C API is `tessera_session_selection_rich_text(app, id, html, rtf,
plain)` with three `TesseraTextBuffer { buf, cap, len }`: all three are
written or none (`TESSERA_ERROR_INVALID_ARGUMENT` with every `len` set when
one is short), and a copy past the cap is `TESSERA_ERROR_REFUSED` with only
`plain` written.

**Bounds (S5).**

| Input | Cap |
|---|---|
| Rules | 64 (`MAX_SMART_RULES`), later ones dropped with one warning |
| Pattern | 1024 bytes (`MAX_SMART_RULE_PATTERN_LEN`); compiled program 1 MiB (`SMART_RULE_SIZE_LIMIT`), each lazy DFA cache of the regex crate's 512 KiB (`SMART_RULE_REGEX_CACHE_BYTES`, #649), nesting 50 (`SMART_RULE_NEST_LIMIT`); a rule that breaks one, does not compile or matches the empty string is left out with a warning naming its index |
| Scan | 8192 characters of the logical line around the click (`SMART_SCAN_LEN`: 4096 before it, the rest after) and 32 KiB (`SMART_SCAN_BYTES`: 16 KiB before it, 16 KiB from it on, #433) |
| Click time | 50 ms (`SMART_RULE_BUDGET`), checked before each rule, the first rule included (#433), and after each costly search and every 16th match (`SMART_CLOCK_STRIDE`, #557) |
| Formatted copy | 16 MiB of HTML and of RTF (`MAX_RICH_COPY_LEN`), counted while the runs are built; past it the formatted copy is refused and the plain copy still works |
| RTF colour table | 4096 entries (`MAX_RTF_COLORS`); more is refused like a copy past the size cap |

The rules are compiled once, when the configuration loads, and shared by
every session; a double-click compiles nothing. A double-click with 64
rules on a 2048-column line takes well under the 5 ms bar in release
builds (a test asserts it).

## Consequences

- Double-clicking a path in `git status`, a URL, a commit hash or an
  address selects all of it; elsewhere a double-click selects what it did.
- A user rule can select almost anything, but it cannot make a double-click
  large or slow: the scan, the pattern and the number of rules are capped,
  and the rules share a time budget checked after every match (#351,
  #433). The built-in rules on any text a program prints stay around a
  millisecond.
- Rich text pastes with the terminal's colours on a white page as well as
  a dark one, since default colours are left to the document.
- A double-click near the far end of a 8192-character line may select only
  the word where a longer match runs past the window.
- A custom rule never gets the URL trim or the IPv6 checks unless it is a
  byte-for-byte copy of the built-in pattern.

## Alternatives considered

- **iTerm2's `\S+` rule.** Turns every double-click into "select up to the
  next blank", which is not what the word rules of ADR 0025 promise.
- **A modifier gesture for smart selection (kitty's).** A second gesture to
  learn; the double-click is where users look for it.
- **Building the RTF and HTML in Swift** (`NSAttributedString`). The host
  would have to receive the styled runs over the C API and would end up
  parsing or importing HTML; D10 keeps generation and escaping in one place
  the tests can attack.
- **Cutting a too-large copy.** A formatted copy silently missing its end
  is worse than a plain one; the plain copy is still whole.

## Amendment: click budget, path marks and rich-copy output (#351, 2026-11-13)

- **Click budget.** The rules of one double-click share 50 ms of wall time
  (`SMART_RULE_BUDGET`), checked before each rule after the first: a rule
  not started by then is skipped, and the best match of the rules that ran
  wins (with none, the word). One debug log carries the counts only (rules
  run, skipped, total; S8). The 64-rule cap bounds count, not time: 64
  rules like `[a-z]+X|[a-z]` took ~425 ms a click on 8192 characters
  before; now a click stops after the rule running at 50 ms. The first rule
  always runs, so a click costs at most 50 ms plus one rule.
- **Paths keep combining marks.** The built-in path rule is
  `~?/?(?:[\p{L}\p{M}\p{N}._-]+/+)+[\p{L}\p{M}\p{N}._-]+/?`: a decomposed
  (NFD) accent, as macOS file names often hold, no longer cuts a path
  (fixture SS-C4b). The other built-in rules are unchanged.
- **`<pre>` and a leading blank line.** The HTML always has a line feed
  right after the `<pre ...>` open tag. An HTML parser drops one line feed
  there, so a selection starting with a blank line keeps it.
- **Output sized up front** (superseded by the single walk of #433, below).
  Each format is written twice: first counted
  (nothing stored, the cap checked), then into a string of exactly that
  length, at most `MAX_RICH_COPY_LEN`. A 14 MB copy allocates its output
  once instead of doubling into it, and an over-cap copy is refused before
  any output is allocated.
- **Fixture TR-3** is `abc  def  `, consistent with TR-9 and D-TRIMALL
  (#47 round 2 m6).
- **Pairing at a window edge.** A rule that matches across blanks (the
  quoted-string rule) can pair its delimiters differently inside the scan
  window than on the whole line. The edge-run drop does not catch that,
  because the run at the edge is not the match's own.
- **Cocoa's RTF reader** turns U+FFFD into `?`, drops U+202A-U+202E, and
  shifts some colours slightly (229 becomes `#eaeaea`; a white `\cb` is
  dropped). These are platform limits: the RTF is as written, and the HTML
  form keeps the exact colours and characters.
- **Mutants left alive, by design.** The rule compiler's `dfa_size_limit`
  bounds the lazy DFA's cache. Past the limit the regex crate falls back to
  a slower engine with the same matches, so no test can kill the mutant
  that drops it. It bounds memory, not results.

## Amendment: a click bounded by a per-match budget, single-walk rich copy (#433, #430, 2026-11-18)

**Where the 18 s went (#433).** The rule `(?:[^X]{0,50}){0,20}X|a` compiles
to 1 042 424 bytes, just under the 1 MiB program bound. Its lazy DFA
thrashes in a 1 MiB cache and the regex crate falls back to the PikeVM,
whose cost per byte grows with the program (4 to 46 µs a byte here). On a
line of `a`s every character is a match, each found after the PikeVM
looks up to ~1000 bytes ahead: about 4097 matches before the click at
~10 ms each, 41 s a click on 8192 characters. The budget was checked
between rules only, and the first rule was exempt, so nothing stopped it.
A click now costs at most the budget plus one match attempt, and that
attempt is bounded only by the scan window and the program size (about
1 s at worst, about 3 s with capture groups, more under load; below):

- **Program size.** `SMART_RULE_SIZE_LIMIT` stays 1 MiB, for the program
  and for the lazy DFA cache. A smaller bound cannot bound the cost:
  at 256 KiB it refused common rules (`\w{2,6}`, `\w+@\w+\.\w{2,6}`,
  `\S{1,200}`, `\w{10}`, `\p{L}{2,20}`), while nested ASCII repetition
  such as `(?:[a-z]{0,50}){0,20}X|a` fits in ~100 KiB and still thrashes
  the DFA. The largest built-in rule (paths) compiles to ~104 KiB, and a
  test checks that every built-in rule fits half the bound.
- **Budget per match.** `SMART_RULE_BUDGET` (50 ms) is checked before each
  rule and after each match a rule finds, the first rule included: a rule
  that has spent it stops at the match it found, the rules after it are
  skipped, and the best match so far wins. The debug log adds the match
  count (S8: counts only).
- **Scan bytes.** The window is also capped at 32 KiB
  (`SMART_SCAN_BYTES`: 16 KiB before the click, 16 KiB from it on). Text of
  one scalar a cell never reaches it; 32-scalar clusters (128 bytes a cell)
  would otherwise make an 8192-character window 1 MiB.

**What stays unbounded: one attempt.** The budget is checked after each
match, so a rule that finds no match in the window costs one search over
the whole of it, which only the window and the program size bound. The
worst case measured without capture groups is
`(?:[e\x{301}]{0,60}){0,20}[XY]` on combining text: about 1 s (985 ms in
the round-1 review's probe; 206-768 ms in the release test on this
machine). Capture groups make each byte about 3x slower once the lazy DFA
gives up: `(?:(e?)(\x{301}?)){0,2560}[XY]` compiles to 1 044 824 bytes,
under the bound, and one click on 32 KiB of `e` and U+0301 took 2.2-3.0 s
idle and up to 6.2 s at load 14 in the adversarial probe (2.2 s on
decomposed text and 4.7 s on 32-scalar clusters in the release test, at
load 53), all with the terminal lock held. A click therefore costs at
most the budget plus about 1 s for a rule without captures, and about
3 s, more under load, for one with them. Bounding each attempt by construction (a hybrid DFA that
gives up, or matching on a worker thread) is a 0.3.0 follow-up.

Measured with the release test `hostile_smart_rules_are_refused_or_bounded`
(20 000-character lines of ASCII, Latin, emoji, 32-scalar clusters and
repeated literals, plus decomposed `e` and U+0301; fastest of three
clicks): the #433 rule stops at the budget on ASCII (~51 ms) and takes
72-237 ms on the other texts, where it rarely matches; `\p{L}{0,1000}X|a`
compiles to more than 1 MiB and is refused at load; the other nested
counted repetition stops at the budget (~51 ms on ASCII, under 0.4 ms
elsewhere); the no-match rules above take 206-768 ms and, with capture
groups, 2.2-4.7 s on combining and decomposed text;
an alternation of ~200 literals, `.` and a 1024-byte `a?a?...XY|a` stay
under 4 ms. The test's bars are 100 ms where a rule matches at every
character, 10 s for the capture-group rule and 3 s otherwise.

**Dotted names keep combining marks** (#430 m5): the built-in rule is
`(?:[\p{L}\p{M}\p{N}._]+\.)+[\p{L}\p{M}\p{N}._]+`, as the path rule
became in #351.

**Single-walk rich copy** (#433 adversarial M3). Copy with Formatting
walked the HTML twice (counted, then written) and the RTF body four times
(colour table and body, both counted then written). Each format is now
written in one walk of the text into one reservation: an upper bound of
its length computed from the runs' lengths (not their text), at most
`MAX_RICH_COPY_LEN`. The RTF colour table is built first from the looks
alone. The pages of the reservation the output does not reach are never
touched, so memory follows the output, not the bound. A debug assertion
checks that the output never outgrows its reservation, and a test checks
the bounds on the text that writes the most bytes per byte in each format.

**Measurements** (`cargo xtask ab bench` against bd80cde, 7 interleaved
pairs, best instructions retired net of process overhead, release, load
20-40). Hover (`link_at` at every column of a line holding a URL and a
path): ASCII 1.007, Latin 1.003, CJK 1.002, box drawing 1.003, Hebrew
1.004, Arabic 1.003, punctuation 1.014, braille 1.004, emoji 1.002, SGR
1.003, combining 1.003. Double-click with the built-in rules at every
column of the same lines: ASCII 1.018, Latin 1.006, CJK 1.005, box drawing
1.007, Hebrew 1.006, Arabic 1.004, punctuation 1.021, braille 1.006,
emoji 1.005, SGR 1.017, combining 1.008; checksums equal on every line.
Cycles on the click cases run about 3 points above instructions: the
clock read after each match costs that, while instructions stay within
the bar. Copy with Formatting of a 5000-row selection in runs of five cells
(11.5 MB of HTML, 4.9 MB of RTF), ten copies: 2.59x fewer instructions and
2.25x less wall time than before, the corpus setup subtracted; the peak
resident set the ten copies add is the same (62.6 MB against 62.7 MB,
median of 11). Differential runs against the base (20 000 random styled
selections for both formats, 5000 random lines for hover and click with
combining marks left out, which dotted names now keep) found no
difference. Hostile hovers (#430 m6, record only): a URL of zero-width
characters 0.999; 1978 cells of `e` and a combining mark after a URL
0.988, now no link (3956 characters, over the cap).

**Mutants left alive, by design** (#430 m7). These mutants change how much
work is done, not any result, so no test can kill them:

- `started` in `hidden_end_can_link`: with no scheme start up to the point,
  `url_at` found no candidate and a hidden end cannot create one, so
  dropping the guard only reads clusters the answer does not need.
- The blocks guard (`url::blocks_scheme(chars[s - 1])`) there: a cell
  before a start that does not block the scheme leaves U1 passing either
  way, so checking it too only reads one more cluster.
- `dfa_size_limit` in the rule compiler (see the #351 amendment).
- `>=` turned into `>` in the budget checks of `smart_at`: the time
  compared adds the real time elapsed since the click to the cost the
  tests charge, and that time is never zero, so the sum never equals the
  budget and both comparisons stop at the same match.

The `index > 0` exemption is gone.

## Amendment: the warning for a pattern over the size limit (#433, 2026-11-18)

A rule whose pattern compiles past `SMART_RULE_SIZE_LIMIT`
(`regex::Error::CompiledTooBig`) has its own warning: it says the pattern is
too large once compiled, usually from counted repetition of a Unicode class,
and suggests ASCII classes or `(?-u)`. Any other compile failure keeps the
existing warning. Neither quotes the pattern (S8).

## Amendment: each rule attempt bounded by construction (#518, 2026-11-22)

The #433 budget stopped a click between matches, but one match attempt of
the regex crate was bounded only by the window and the program size: about
1 s for a rule with no match in the window, 3 s and more with capture
groups. Each attempt's memory is now bounded by construction (one cache
fill at most); its time follows from that only empirically:
`SMART_RULE_ATTEMPT_BOUND` is the worst attempt measured with a margin, not
a proven bound.

**Engine.** `tessera-app-core` (`smart_engine.rs`) builds, per rule, a lazy
DFA of `regex-automata` (`hybrid::regex::Regex`, the engine the regex crate
is built on, a workspace dependency with no new package) from
`rule.regex().as_str()`, with the nest and NFA size limits of the rule
compiler, a 1 MiB cache per direction (`SMART_RULE_CACHE_BYTES`,
`skip_cache_capacity_check`), `unicode_word_boundary` and the literal
prefilter the regex crate would use, and searched with
`util::iter::Searcher::try_advance`. It is told to give up the first time
its cache would be cleared (`minimum_cache_clear_count(Some(0))`), so one
search costs at most one cache fill: `SMART_RULE_ATTEMPT_BOUND` (50 ms) is
that bound with a 2x margin over the worst attempt observed under load.
The `regex::Regex` of `tessera-config` stays the judge of what loads and is
the fallback; every rule that loads also builds a DFA (tested). The engine
table is built by the first click (of any granularity) after a load that
changed the rules; a reload whose rules are the same patterns, precisions
and checks in the same order keeps the engines and their warm caches
(tested), so an unrelated configuration edit costs no rebuild. No thread is
added.

**Fallback.** The DFA cannot decide a Unicode `\b` on non-ASCII text: it
quits. A rule with a Unicode word boundary on a window that is not ASCII
skips the DFA, and a DFA that quits goes on, and both run the regex
crate's `find_iter` (the matches already reported left out), but only when
the captures-on NFA's states times the window's bytes stay within
`SMART_FALLBACK_UNITS` (8 000 000, measured at most 22 ms). Over the gate
the rule has no match for that click. Without a fallback 8 of the 15
built-in rules would stop working on any line that is not ASCII.

**Warm caches.** A cold cache per click costs about 6x on the 64-rule
click, so each rule keeps its cache between clicks behind a leaf mutex
locked for the attempt only. A search that gives up on a used cache resets
it and retries once, cold; a cold give-up is the rule's answer for that
click. A warm cache never has more room than a cold one, so a search's
outcome is the same whatever ran before: the same click on the same text
gives the same selection on a fresh app and after other clicks (tested).
These caches hold at most 64 rules x 2 directions x 1 MiB = 128 MiB, the
same ceiling the regex crate's cache pool had. That is not the whole worst
case: a rule that takes the fallback also keeps the regex crate's own cache
in its pool, one per thread that clicks, about 0.6 MiB per `\b` rule
measured. Its ceiling is that crate's limits plus the slot gate of #590
(the #649 amendment names each term):
three lazy DFA caches (forward, reverse and reverse-inner) of 512 KiB each
(`SMART_RULE_REGEX_CACHE_BYTES`, the rule compiler's `dfa_size_limit`),
the bounded backtracker's 256 KiB visited set, the PikeVM's slot tables
within `SMART_FALLBACK_SLOT_BYTES` (512 KiB) and its sparse sets and stack
(32 bytes per state, at most 16 384 states within that cap, 512 KiB):
1536 + 256 + 512 + 512 KiB, 2.75 MiB per rule.
With 64 fallback rules and one clicking thread the worst case is then
128 + 64 x 2.75 = 304 MiB, under the 384 MiB bar;
each further thread that clicks adds 176 MiB (two threads 480 MiB).
The most measured for 64 rules on one thread is 187 MiB kept (see the #649
amendment).

**Behaviour change.** A rule whose search gives up (its cache would
thrash) or whose fallback is over the gate selects nothing for that click,
instead of selecting late; the other rules and the word selection still
apply. It is logged once per rule index, at warn level, the index and the
reason only (S8), never again for that index while the configuration
stands.

**Bounds.** A click holds the terminal and selection locks at most
`SMART_RULE_BUDGET` plus one attempt, and never more than rules x
(`SMART_RULE_BUDGET` + `SMART_RULE_ATTEMPT_BOUND`), for a single caller: the
Swift main thread is the only one that clicks today. Each rule's cache lock
is shared by every session, so concurrent clicks from several threads
serialise on it while each holds its own session's locks: 16 threads on one
thrashing rule measured p99 243 ms, max 315 ms (one thread 12 ms), and the
wait also spends the click's wall budget, so later rules are skipped. The hostile test of
#433 now loads its rules through `tessera-config` and asserts, on the
thread's CPU clock in release builds, at most 100 ms per click for every
rule and 50 ms for the rules that cannot match, over seven texts including
random text in nine scripts; its 3 s and 20 s bars are gone. A rule over
the gate selects nothing in under 1 ms. A differential test and a proptest
compare the DFA with `find_iter` for the built-in rules and 36 tricky
patterns over ASCII, Latin, CJK and emoji, combining and random windows.

**First click.** The engines are built once per set of rules, by the
first click after the rules change, outside the session's locks: one
parse per rule, then the forward and reverse programs and DFAs from it,
about 1.1 M instructions per rule. Over main, the first click costs 15-18 M
instructions more with the 15 built-in rules (CJK lines included, where
the fallback's captures program is compiled at first use) and 72-76 M with
64 rules; on the thread's CPU clock (release, fastest of 3) 1.05 ms and
9.4 ms. A release test holds the built-in rules' first click under 10 ms
and another records the 64-rule one. After it, clicks run on warm caches.

**Known limits.** The fallback gate also drops ordinary rules, not only
hostile ones: a Unicode-`\w` rule such as
`\b[\w.+-]+@[\w-]+\.[\w.-]+\b` (962 states) is over the gate on a
window of more than about 8 KB that is not ASCII, so on a long CJK line
(a mail address followed by 1000 x `日本語 `) a double-click selects the
word, `user`, not the address; at 500 repetitions it still selects the
address. Concurrent clicks from more than one thread serialise on the
rules' cache locks (see Bounds).

**A/B.** Measured on steady state by lead decision: one warm-up click,
then 2000 to 12 000 clicks per process (enough to clear `cargo xtask ab`'s
size floor), instructions, three fresh probes, bar 1.10, the warm-up click
counted. At a load of 7-8, head/base: 64 rules on a 2048-column `/a` line
1.053-1.054, the 15 built-in rules on an ASCII line 1.011-1.012 and on a
2048-column `日/` line 1.003, checksums equal.

## Amendment: the click clock read every 16 matches (#557, 2026-11-24)

The budget read the clock after every match. A rule that matches at every
character spends a good part of its click on those reads, and since #518
most matches come from a DFA search on cached states, which costs about as
much as the read. The clock is now read after a match only when its search
may have been costly (the first search on a cold cache, one that gave up,
reset its cache and ran again cold, or any match of the fallback), or when
the match is the click's 16th, 32nd, ... (`SMART_CLOCK_STRIDE`). The
searches between two reads neither reset the cache nor fall back, so
together they add at most one cache fill (the states added since the last
reset fit in it) and a pass over the window each: a click still costs the
budget plus one attempt, plus fewer than 16 such searches (microseconds),
and a rule matching everywhere stops within the budget plus 16 matches.
`.` on the hostile test's 20 000-character lines took 117-282 µs a click
(release, thread CPU, fastest of 3) against 164-369 µs before.

The hostile test's bars now come from each row: a rule that finds matches
on that text gets the budget plus one attempt (100 ms), one that finds none
one attempt (50 ms), from the matches its clicks found rather than a flag
per rule.

## Amendment: what "by construction" bounds (#590, 2026-11-24)

The #518 amendment's title says each rule attempt is bounded by
construction. That holds for an attempt's memory only: a search fills its
rule's cache at most once cold, after at most one reset, before it gives
up, so the cache never holds more than `SMART_RULE_CACHE_BYTES` per
direction (the regex crate's pool for the fallback has its own ceiling,
under Warm caches). An attempt's
time is not bounded by construction; it follows from the one fill only as
measured (`SMART_RULE_ATTEMPT_BOUND`). A click's time is bounded by the
attempt clock: the click budget read after every search that may have cost
an attempt (the first on a cold cache, one that reset its cache, and every
match of the fallback) and every `SMART_CLOCK_STRIDE` matches (#557), so a
click stops within `SMART_RULE_BUDGET` plus one attempt, plus fewer than
`SMART_CLOCK_STRIDE` searches on cached states.

A DFA quit on a rule without a Unicode word boundary cannot happen (that
DFA has no byte it quits on). Had it happened, the rule selected nothing
for that click, as now, but logged that its fallback was over the gate; it
now logs `its DFA quit and it has no fallback` (a test forces the quit).

**Slot gate.** The ceiling #518 gave the fallback, about 4 MiB per rule,
was wrong: the regex crate's PikeVM allocates, when the fallback first
runs, two slot tables of the captures program's states x capture slots x
8 bytes, quadratic in the pattern and kept in that crate's pool. A
1011-byte rule within `SMART_FALLBACK_UNITS`, `\bx(?:` + 250 x `(a?)` +
`){23}` (23 007 states, 502 slots), kept 177 MiB after one double-click on
a 160-column line of `éx`, 708 MiB for four such rules (the 590e review's
measurement). The fallback now
also requires states x slots x 16 bytes to be at most
`SMART_FALLBACK_SLOT_BYTES` (512 KiB, lowered from a first 2 MiB that
let 64 rules reach about 530 MiB); past it the rule selects nothing for
a click that would fall back, with the over-the-gate warning. That rule is
refused and keeps nothing in the regex crate's pool (tested: the fallback
never runs). No built-in rule has a capture group: each takes at most
21 KiB of slot tables (670 states x 2 slots, the largest program, about
25 times under the cap), and each
falls back and matches as `find_iter` on 32 KiB windows of CJK and emoji
text (tested). Measured kept heap (release, a counting allocator in a
scratch probe): a rule at the cap (2 groups x 681, 5455 states) kept
1.10 MiB on a non-ASCII line within the window gate, a thrashing `\b`
rule 0.93 MiB of its own DFA caches on an ASCII line and 0.64 MiB of the
regex crate's on a mixed one, 1.57 MiB for both, a built-in rule at most
0.14 MiB. Sixty-four rules on one thread, each scanned once: 64 at the cap
kept 70.5 MiB (peak RSS 109 MiB), 64 thrashing rules 95.6 MiB (peak RSS
299 MiB, of which 168 MiB is the 64 compiled rules themselves). By the
limits a rule kept at most about 5.3 MiB: 2 MiB of its own DFA caches and
about 3.3 MiB of the regex crate's (Warm caches), 336 MiB for 64 rules
on one clicking thread, leaving out the regex crate's reverse-inner lazy
DFA cache (the #649 amendment counts it and lowers the total).

## Amendment: the regex crate's lazy DFA caches bounded at 512 KiB (#649, 2026-11-25)

Superseded by the next amendment: the fallback no longer runs the regex
crate's engine, and the ceiling counts allocated bytes.

**The term left out.** The regex crate (1.x, on regex-automata 0.4) gives
`RegexBuilder::dfa_size_limit` to its meta engine as
`hybrid_cache_capacity`, and every lazy DFA that engine builds takes that
capacity: the forward and reverse ones of its core strategy and the
reverse one of its reverse-inner strategy (regex-automata's
`meta/wrappers.rs`, `HybridEngine` and `ReverseHybridEngine`, each with
`skip_cache_capacity_check(false)`, so a cache past it is cleared, and
after three clears at under 10 bytes a state the search gives up to the
PikeVM or backtracker, same matches). The reverse-inner cache was
therefore bounded at 1 MiB all along; the #590 ceiling only left it out.
Counted, 64 rules on one thread reached 128 + 64 x 4.25 = 400 MiB by the
limits, over the 384 MiB bar.

**The bound.** The rule compiler now gives the regex crate
`dfa_size_limit(SMART_RULE_REGEX_CACHE_BYTES)`, 512 KiB, instead of the
1 MiB program bound. Per fallback rule, one clicking thread:

| Term | Bytes |
|---|---|
| Forward, reverse and reverse-inner lazy DFA caches, `SMART_RULE_REGEX_CACHE_BYTES` each | 1536 KiB |
| Bounded backtracker's visited set (regex-automata's default `visited_capacity`) | 256 KiB |
| PikeVM slot tables, within `SMART_FALLBACK_SLOT_BYTES` | 512 KiB |
| PikeVM sparse sets and stack, 32 bytes x 16 384 states (the most within the slot cap) | 512 KiB |

That is 1536 + 256 + 512 + 512 KiB, 2.75 MiB per rule, and with the
click engine's own DFAs (64 rules x 2 x 1 MiB):
128 + 64 x 2.75 = 304 MiB, under the 384 MiB bar.
The regex crate keeps one cache per thread that searches with a rule, so
the figure assumes one clicking thread (the Swift main thread today); each
further thread that clicks adds 176 MiB (two threads 480 MiB).
A const assert in `smart_engine.rs` holds the total under the bar, and a
test checks that these phrases match the constants. Left out: the
backtracker's stack and the one-pass DFA's and capture slots (a few bytes
per slot), and the regex crate's cache from the loader's one empty-text
check per rule, which has searched nothing.

**What the figures count.** Every term is the crates' own count of a cache
(`memory_usage`, its tables' lengths). A cleared lazy DFA cache keeps its
tables' capacity, which can reach about twice that count: 64 fallback
rules filling their caches over 400 clicks kept 2.9 MiB a rule (below).

**Matches and give-ups.** A smaller cache changes no match: past it the
regex crate falls back to an engine with the same matches. No built-in
rule gives up at 512 KiB (tested: each Unicode-`\b` built-in rule, in the
meta engine's lazy DFA configuration and without its prefilter, searched
from every start its fallback searches from on 32 KiB CJK, mixed, ASCII
and multi-script windows, quits on non-ASCII and never gives up, even at
16 KiB; a 64-class filler in the same configuration does give up, so the
search would show one).

**Measured (release, a counting allocator, kept heap after the clicks).**
One `\b` rule whose fallback fills its forward cache (2^15 DFA states, 64
byte classes, 8000 characters): 1.18 MB kept, against 1.81 MB at 1 MiB and
2.94 MB at the crate's default 2 MiB; tessera-ffi's `smart_selection_heap`
test holds it under three times `SMART_RULE_REGEX_CACHE_BYTES`, so removing
the limit or raising it back to 1 MiB fails (the mutant the #351 amendment
left alive is now killed). For 64 rules, before and after:

| Set | Kept before | Kept after | Peak RSS before | Peak RSS after |
|---|---|---|---|---|
| 64 at the slot cap (`\bx(?:(a?)(a?)){681}`), 8 clicks | 47.2 MiB | 47.2 MiB | 62.9 MB | 62.9 MB |
| 32 thrashing own-DFA rules and 32 fallback fillers, 400 clicks | 90.3 MiB | 90.3 MiB | 113.9 MB | 113.0 MB |
| 64 fallback fillers (`\b(?:[ab]*a[ab]{14})`, 30 000 characters), 400 clicks | 218.2 MiB | 187.4 MiB | 236.9 MB | 189.3 MB |

The click budget skips rules, so a set's kept heap grows with the number
of clicks until every rule has filled its caches; the fillers had not
saturated at 400 clicks.

## Amendment: the fallback runs the rule's DFA and a PikeVM; the ceiling in allocated bytes (#649, 2026-11-25)

**Why.** The regex crate's bounded backtracker grows its stack without a
bound: `\b(?:ab|[qQ]|[rR]|...)+` (150 classes) on `ab` x 3990 + `é` kept
10.5 MiB for one rule, and 64 such rules 625 MiB. The lead also set the
384 MiB bar on bytes allocated, not on the crates' `memory_usage` counts,
which a filled lazy DFA cache's allocations exceed by up to 2.59 times by
construction (below).

**The fallback.** A rule that falls back no longer runs `regex::Regex`.
Each search of its `find_iter` runs the rule's own DFA until it quits (a
byte that is not ASCII at a Unicode word boundary) or gives up, and then a
`PikeVM` (`regex_automata::nfa::thompson::pikevm`) on the DFA's forward
program, which it shares; after a give-up the DFA's cache is replaced with
an empty one and the rest of the scan is the PikeVM's, so a scan fills that cache once at most.
That is what the regex crate's meta engine does with its lazy DFA and
PikeVM, so the matches are the same; there is no backtracker, no second
compile of the program and none of the meta engine's three more lazy DFA
caches. The PikeVM's cache is one per rule behind a leaf lock (the DFA's
cache lock is taken after it), not one per thread. app-core's
regex-automata dependency gains the `nfa-pikevm` feature (the regex crate
already enables it). Built with `regex_automata::meta` and
`backtrack(false)` instead, a rule would keep a second program (about
1.1 MiB measured) and three more lazy DFA caches (6 x their capacity in
allocated bytes), so the DFA caches would have had to drop to about
192 KiB a direction to stay under the bar.

**The bound, in bytes allocated.** `SMART_RULE_CACHE_BYTES` drops from
1 MiB to 512 KiB per direction. regex-automata raises a direction's
capacity to its minimum for the program when that is larger, about 27
bytes an NFA state (two sparse sets of 8 bytes, a stack of 4, room for two
states of 5 bytes per NFA state): past 512 KiB only for a program of more
than about 19 000 states, 0.84 MiB at the most (32 768 states, all that
`SMART_RULE_SIZE_LIMIT` allows at the compiler's 32 bytes a state).
A rule whose capacity is raised in either direction has no fallback: it
matches wherever its DFA completes, on every line that is all ASCII, and
selects nothing on a line that is not when it would fall back, with one
warning per rule index when its engines are built. That is a behaviour
change, for rules such as `\b\w{20}`, whose reverse program has about
28 700 states while its forward one is within the slot gate: with a
PikeVM cache beside a raised DFA cache a rule's terms would reach
1.30 + 1.79 + 1.25 + 2 = 6.34 MiB, and 64 rules 406 MiB. No built-in rule
is raised (tested). tessera-config keeps the pattern string of a rule, not
its compiled `Regex` (`CompiledRule::pattern`): the `Regex` is dropped
after the load-time checks, which are unchanged. Per rule:

| Term | Bytes allocated |
|---|---|
| The DFA's two caches, at most 2.59 times their capacity each by construction: 2 x 2.59 x 512 KiB = 2.59 MiB | 2.59 MiB |
| The PikeVM's cache, by the slot gate 16 384 states of 80 bytes (1.25 MiB): two slot tables of two 8-byte slots, two sparse sets of two 4-byte ids, a 16-byte stack frame grown by doubling | 1.25 MiB |
| Programs: the DFA's forward and reverse NFAs, which the PikeVM shares, each within `SMART_RULE_SIZE_LIMIT`: 2 x 1 MiB = 2 MiB | 2 MiB |
| The DFA's literal prefilter, which the PikeVM shares, kept only when its count (`Prefilter::memory_usage`) is at most 128 KiB (`SMART_PREFILTER_BYTES`), with 32 KiB for the structs the count leaves out: 128 + 32 KiB = 0.156 MiB | 0.156 MiB |

So 2.59 + 1.25 + 2 + 0.156 = 5.996 MiB per rule and 64 x 5.996 = 383.76 MiB,
under the 384 MiB bar. The cache factor holds by construction. A state counts
4s + 36 + l bytes against the capacity: its transition row (4 bytes for
each of s classes, s at least 2), 16 bytes in the state list, 20 in the
state map and its own l bytes (at least 9). It allocates at most
8s + 112.15 + l: twice its row and list entry (both grow by doubling),
2.29 map buckets of 25 bytes (a std `HashMap` just after it doubles) and
its bytes behind a 16-byte `Arc` header rounded up to 8. That is at most
137.15 / 53 = 2.59 times its count; the sparse sets, stack and scratch
allocate what they count. regex-automata's `Cache::reset` keeps every
table's capacity, so a fill of many small states followed by a fill of
large ones would allocate up to three times the capacity (the adversarial
run measured 2.90 on `x(?-u:a){4200}|(?-u:b){1000}`): a give-up therefore
replaces the cache with a new one, whose tables hold one fill only
(1.01 MiB kept for that rule, programs included, against 2.08 MiB with a
reset; tested). A raised direction allocates at most its sparse sets plus
2.59 times the rest, 8 + 2.59 x 19 bytes an NFA state, 1.79 MiB at 32 768
states; such a rule has no PikeVM cache, so its terms are at most
2 x 1.79 + 2 + 0.156 = 5.74 MiB.

The prefilter term is new in the second adversarial run, which found that
no term held it. The regex crate keeps an exact literal set of up to 250
literals as the prefix it searches for, and past 64 literals regex-automata
builds an Aho-Corasick DFA for it with a 256-wide row per state: the
117-byte `\b[0-9]{2}[ab]` followed by 97 bytes of ASCII and two-byte
characters (200 literals of 100 bytes) loads, and its prefilter alone kept
19.1 MiB, 64 such rules 1222 MiB, the same on the base. A rule whose
prefilter counts more than 128 KiB is now built without one, so its DFA
searches without skipping ahead, as it does when no prefix can be found;
its matches do not change (tested). Every built-in rule keeps its
prefilter (tested). The count is the automaton's tables; what it leaves
out, the automaton's own structs and their `Arc`, came to at most 6.3 KiB
over 1005 prefilters of literal sets and prefixes (up to 250 literals of
up to 100 bytes, counting 2.8 KiB to 23.8 MiB), so the term allows 32 KiB
for it. Dropping every
prefilter was rejected: a small one is cheap and its speed is real.

The bar is for the steady state. When the rules change, the app builds
the new engines while it still holds the old ones, so a reload peaks at
both sets: 277.8 MiB over the baseline as measured for 64 rules, about
twice the ceiling at worst. Dropping the old engines first is #688.

Every cache is one per rule whatever thread clicks: the app holds one `SmartEngines`, an `Arc` that every session and thread shares and that is rebuilt only when the rules change; each rule's DFA cache and PikeVM cache sit behind that rule's own `Mutex`; and no engine keeps a per-thread pool (the regex crate's pool is searched only by tessera-config's load-time empty-match check, on the loading thread, and dropped with its `Regex`). So a further clicking thread waits for the rule's lock and adds 0 MiB; a reload while another thread clicks keeps the old engines until that click returns (only the main thread clicks today). The program term follows from the size limit: it bounds the compiler's count of each program (32 bytes a state plus its transitions), and the finished NFA is smaller than that count (regex-automata boxes the compiler's vectors). The
largest pair of programs measured over families of patterns at their
largest size that loads allocated 1.86 MiB, 0.93 MiB an NFA (a class of
64 one-byte members repeated 1820 times, found by the adversarial run;
`\b(?-u:[cegikmoqsuwyCEGIKMOQSUWY02468]){3541}` 1.73 MiB,
`\b(?-u:a){32760}` 1.50 MiB, `\b\w{20}` 1.07 MiB, `\bx[a-z]{14559}`
0.67 MiB, mixes of these with `[ab]*a[ab]{14}` 1.65 MiB). tessera-config's
compile of the rule allocated as much again, which is why it is no longer
kept: with it the term would be 4 MiB. A const assert holds the total
under the bar and a test checks these phrases against the constants.

**Matches and give-ups.** A smaller cache changes no match: past it the
regex crate falls back to an engine with the same matches. No built-in
rule gives up at 512 KiB (tested: each Unicode-`\b` built-in rule, in the
meta engine's lazy DFA configuration and without its prefilter, searched
from every start its fallback searches from on 32 KiB CJK, mixed, ASCII
and multi-script windows, quits on non-ASCII and never gives up, even at
16 KiB; a 64-class filler in the same configuration does give up, so the
search would show one).

**Measured (release, a counting allocator, kept heap after the clicks).**
One `\b` rule whose fallback fills its forward cache (2^15 DFA states, 64
byte classes, 8000 characters): 1.18 MB kept, against 1.81 MB at 1 MiB and
2.94 MB at the crate's default 2 MiB; tessera-ffi's `smart_selection_heap`
test holds it under three times `SMART_RULE_REGEX_CACHE_BYTES`, so removing
the limit or raising it back to 1 MiB fails (the mutant the #351 amendment
left alive is now killed). For 64 rules, before and after:

| Set | Kept before | Kept after | Peak RSS before | Peak RSS after |
|---|---|---|---|---|
| 64 at the slot cap (`\bx(?:(a?)(a?)){681}`), 8 clicks | 47.2 MiB | 47.2 MiB | 62.9 MB | 62.9 MB |
| 32 thrashing own-DFA rules and 32 fallback fillers, 400 clicks | 90.3 MiB | 90.3 MiB | 113.9 MB | 113.0 MB |
| 64 fallback fillers (`\b(?:[ab]*a[ab]{14})`, 30 000 characters), 400 clicks | 218.2 MiB | 187.4 MiB | 236.9 MB | 189.3 MB |

The click budget skips rules, so a set's kept heap grows with the number
of clicks until every rule has filled its caches; the fillers had not
saturated at 400 clicks.

## Amendment: the fallback runs the rule's DFA and a PikeVM; the ceiling in allocated bytes (#649, 2026-11-25)

**Why.** The regex crate's bounded backtracker grows its stack without a
bound: `\b(?:ab|[qQ]|[rR]|...)+` (150 classes) on `ab` x 3990 + `é` kept
10.5 MiB for one rule, and 64 such rules 625 MiB. The lead also set the
384 MiB bar on bytes allocated, not on the crates' `memory_usage` counts,
which a filled lazy DFA cache's allocations exceed by up to 2.59 times by
construction (below).

**The fallback.** A rule that falls back no longer runs `regex::Regex`.
Each search of its `find_iter` runs the rule's own DFA until it quits (a
byte that is not ASCII at a Unicode word boundary) or gives up, and then a
`PikeVM` (`regex_automata::nfa::thompson::pikevm`) on the DFA's forward
program, which it shares; after a give-up the DFA's cache is replaced with
an empty one and the rest of the scan is the PikeVM's, so a scan fills that cache once at most.
That is what the regex crate's meta engine does with its lazy DFA and
PikeVM, so the matches are the same; there is no backtracker, no second
compile of the program and none of the meta engine's three more lazy DFA
caches. The PikeVM's cache is one per rule behind a leaf lock (the DFA's
cache lock is taken after it), not one per thread. app-core's
regex-automata dependency gains the `nfa-pikevm` feature (the regex crate
already enables it). Built with `regex_automata::meta` and
`backtrack(false)` instead, a rule would keep a second program (about
1.1 MiB measured) and three more lazy DFA caches (6 x their capacity in
allocated bytes), so the DFA caches would have had to drop to about
192 KiB a direction to stay under the bar.

**The bound, in bytes allocated.** `SMART_RULE_CACHE_BYTES` drops from
1 MiB to 512 KiB per direction. regex-automata raises a direction's
capacity to its minimum for the program when that is larger (two sparse
sets of 8 bytes an NFA state and room for a few DFA states): past 512 KiB
only for a program of more than about 19 000 states, 0.84 MiB counted for
`\b(?-u:a){32760}`, the most states within the size limit.
tessera-config keeps the pattern string of a rule, not its compiled
`Regex` (`CompiledRule::pattern`): the `Regex` is dropped after the
load-time checks, which are unchanged. Per rule:

| Term | Bytes allocated |
|---|---|
| The DFA's two caches, up to twice their count each: 2 x 2 x 512 KiB = 2 MiB | 2 MiB |
| The PikeVM's cache, by the slot gate 16 384 states of 80 bytes (1.25 MiB): two slot tables of two 8-byte slots, two sparse sets of two 4-byte ids, a 16-byte stack frame grown by doubling | 1.25 MiB |
| Programs: the DFA's forward and reverse NFAs, which the PikeVM shares, 1.75 MiB at most as measured | 1.75 MiB |

So 2 + 1.25 + 1.75 = 5 MiB per rule and 64 x 5 = 320 MiB, under the
384 MiB bar. A program whose minimum raises the capacity has (as measured) more than
16 384 states, so it never falls back and has no PikeVM cache; its caches
allocate at most their sparse sets (allocated exactly) plus twice the
rest, 2 x (0.5 + 2 x 0.34) = 2.38 MiB, under 2 + 1.25 MiB. Every cache is one per rule whatever thread clicks: the app holds one `SmartEngines`, an `Arc` that every session and thread shares and that is rebuilt only when the rules change; each rule's DFA cache and PikeVM cache sit behind that rule's own `Mutex`; and no engine keeps a per-thread pool (the regex crate's pool is searched only by tessera-config's load-time empty-match check, on the loading thread, and dropped with its `Regex`). So a further clicking thread waits for the rule's lock and adds 0 MiB; a reload while another thread clicks keeps the old engines until that click returns (only the main thread clicks today). The program term is measured, not a limit: the
largest pair of programs over families of patterns at their largest size
within `SMART_RULE_SIZE_LIMIT` allocated 1.73 MiB
(`\b(?-u:[cegikmoqsuwyCEGIKMOQSUWY02468]){3541}`; `\b(?-u:a){32760}`
1.50 MiB, `\b\w{20}` 1.07 MiB, `\bx[a-z]{14559}` 0.67 MiB, mixes of
these with `[ab]*a[ab]{14}` 1.65 MiB). tessera-config's compile of the
rule allocated as much again, which is why it is no longer kept: with it
the term would be 3.5 MiB. A const assert holds the total under the bar
and a test checks these phrases against the constants.

**Matches and give-ups.** No built-in rule's DFA gives up at 512 KiB on
32 KiB ASCII, CJK, mixed and multi-script windows (tested from every
start a scan searches from; a 64-class filler does give up there). The
existing tests, which compare every engine with the regex crate's
`find_iter`, pass unchanged.

**Measured (release, counting allocator, kept heap after 16 clicks).**
The stack filler kept 10.50 MiB for one rule and 108.8 MiB for 64 before
(6d937db), 0.24 MiB and 5.3 MiB after. After, for 64 rules, with
tessera-config's compile still kept (73136a1): fallback fillers
(`\b(?:[ab]*a[ab]{14})`) 27.6 MiB, own-DFA fillers (`[ab]*a[ab]{14}`)
62.1 MiB, `\b\w{20}` 174.2 MiB, rules at the slot gate
(`\bx[a-z]{14559}`) 142.1 MiB. Without it, the critic's mix of a large
program and a cache filler
(`\b(?:(?-u:[cegikmoqsuwyCEGIKMOQSUWY02468]){2000}|(?-u:q){12000}|[ab]*a[ab]{14})`)
kept 2.67 MiB alone, and 64 rules mixing it with the largest programs,
`\b\w{20}`, slot-gate rules, cache fillers and stack fillers 96.3 MiB.
tessera-ffi's `smart_selection_heap` test holds the stack filler under
3.25 MiB, the mixed rule under 5 MiB, the 1820-member class within the
2 MiB program term (1.94 MiB kept after one click; it fails against the
1.75 MiB this ADR stated before), the refill under 1.5 MiB, the literal
set within its rule's 5.996 MiB (it fails with the prefilter kept) and the
mixed set of 64 under the 383.76 MiB ceiling.

**The work gate on the PikeVM.** `SMART_FALLBACK_UNITS` stays 8 000 000
states x bytes. Re-measured on the fallback's PikeVM, one search costs
4.3 ns a unit with every state live at every byte (`[ab]*a[ab]{300}` and
`[ab]*a[ab]{1000}` on random `a` and `b`), 34 ms at the gate, against
22 ms for the regex crate's engine before. A state whose class is many
single bytes costs more, since the PikeVM scans such a class one member
at a time: the adversarial run's `\b C*bC{990}` (C a class of 64 one-byte
members, written without the spaces) costs 14.8 ns a unit, about 120 ms a
search and 150 ms a click on 8000 characters, three times
`SMART_RULE_ATTEMPT_BOUND` (50 ms). That is not new (the regex crate's
engine took 115 ms on it before), but the attempt bound does not hold for
a fallback search; #687 restores it.
