# ADR 0042: Smart selection rules and Copy with Formatting

**Status:** accepted · 2026-11-08 · amended 2026-11-13 (#351: click budget, path marks,
rich-copy output); amended 2026-11-18 (#433: a per-match click budget, single-walk
rich copy)

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
| Pattern | 1024 bytes (`MAX_SMART_RULE_PATTERN_LEN`); compiled program and lazy DFA cache 1 MiB each (`SMART_RULE_SIZE_LIMIT`), nesting 50 (`SMART_RULE_NEST_LIMIT`); a rule that breaks one, does not compile or matches the empty string is left out with a warning naming its index |
| Scan | 8192 characters of the logical line around the click (`SMART_SCAN_LEN`: 4096 before it, the rest after) and 32 KiB (`SMART_SCAN_BYTES`: 16 KiB before it, 16 KiB from it on, #433) |
| Click time | 50 ms (`SMART_RULE_BUDGET`), checked before each rule and after each match, the first rule included (#433) |
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
measured. Its ceiling is that crate's limits: its lazy DFA 1 MiB per
direction (`dfa_size_limit`), the bounded backtracker's 256 KiB and a PikeVM
cache linear in the program's states (the program within 1 MiB,
`size_limit`), estimated at most about 4 MiB per rule, not measured. With
64 fallback rules and one clicking thread the worst case is then about
128 + 64 x 4 = 384 MiB.

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
