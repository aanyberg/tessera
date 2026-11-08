# ADR 0042: Smart selection rules and Copy with Formatting

**Status:** accepted · 2026-11-08

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
| 3 | Dotted names `(?:[\p{L}\p{N}._]+\.)+[\p{L}\p{N}._]+` | normal |
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
| Pattern | 1024 bytes (`MAX_SMART_RULE_PATTERN_LEN`); compiled program and lazy DFA cache 1 MiB each (`SMART_RULE_SIZE_LIMIT`), nesting 50 (`SMART_RULE_NEST_LIMIT`); a rule that breaks one, does not compile or matches the empty string is left out with a warning |
| Scan | 8192 characters of the logical line around the click (`SMART_SCAN_LEN`: 4096 before it, the rest after) |
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
  large: the scan, the pattern and the number of rules are capped. Its
  cost is bounded only by those caps, not by a time limit: a rule that
  matches often near the click (64 such rules on a long line) can take a
  double-click to hundreds of milliseconds. The built-in rules on any
  text a program prints stay around a millisecond.
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
