# ADR 0038: Scrollback search: stepped scan, caps, lock budget

**Status:** accepted · 2026-11-06 · amended 2026-11-07 (#46b: per-piece step bound, find bar)

## Context

Issue #46 asks for find in the scrollback: a literal or regex query, a case
toggle, highlighted matches, stepping between them with the view following,
and "copy all matches". The find bar is #46b (T8); this decision covers the
core: the matcher (`tessera-search`), the per-session search state
(`tessera-app-core/src/search.rs`), the overlays fed to the renderer and the
C API (`tessera-ffi/src/search.rs`).

The behaviour is pinned by the spec-researcher's fixture (110 cases, wave 13,
from Alacritty, kitty, iTerm2, Ghostty and Terminal.app),
ported to `crates/tessera-search/tests/search_spec.rs` and
`crates/tessera-app-core/tests/search.rs`. The lead adopted its ten
decisions with D6, D7 and D16.

A history holds up to 10 000 000 lines (ADR 0032), and the terminal lock is
shared with the PTY reader and the frame capture, so a search cannot hold
the lock for a whole scan (security rule S6), and every table it fills from
terminal output needs a cap (S5).

## Decision

**Search unit.** The logical line: the rows a soft wrap joins, cells read
directly (wide characters once, a cluster with its combining marks, the
blanks of a wrapped row kept, trailing blanks of the last row dropped). A
match never crosses a hard line end. A regex match maps back to cells, so
an overlay covers whole wide characters and clusters. A logical line longer
than the segment period (`segment_period(cols)`, 128 rows at 80 columns,
fewer on wide screens so a segment of one-byte text stays under 64 KiB) is
searched in segments cut at fixed absolute rows; a match across a cut is
not found. Multi-byte text (Cyrillic, box drawing, combining marks) can make
a segment's text longer than `MAX_HAYSTACK_BYTES` (64 KiB): the segment is
then searched in pieces of at most that many bytes, each refilled from the
cell the previous piece's cap left out and starting behind a cut marker, so
every cell is searched; a match across a piece cut is not found either.

**What a pattern sees at a cut.** At a row cut and at a piece cut the text
is broken by a cut marker, NUL, which no cell holds. A match that would
straddle a cut is dropped. `^` and `$` do not match at a cut. NUL is a
non-word character, so `\b` is true and `\B` false at every cut, whatever
the cells on either side hold: a pattern with `\b` or `\B` can report false
matches at a cut, not only miss them (for example `\be` matches an `e` that
follows a letter across a cut). Searching each piece with its real
neighbouring cell instead is backlog issue #318.

**Query.** Literal or Rust `regex` syntax (no look-around, no
backreferences), case-sensitive or not (Unicode simple case folding; a
regex's `(?i)` wins). Refused, never truncated or panicking: longer than
`MAX_QUERY_BYTES` (1024), invalid syntax (the message quotes the pattern),
or a program larger than `REGEX_SIZE_LIMIT` (10 MiB, D16: `\w{209}`
compiles, `\w{210}` does not). The lazy DFA cache is capped at
`DFA_SIZE_LIMIT` (4 MiB); past it the regex crate falls back to a slower
engine. An empty query is no search.

**Stepped, not threaded (D6).** `App::search_step(id, budget)` scans chunk
after chunk until nothing is left or the budget is spent (at least one
chunk). A chunk takes the terminal lock only to copy at most
`SEARCH_LINES_PER_LOCK` (256) rows, fewer on wide screens (about 64 Ki
cells), and runs the regex on the copy after releasing it. The frontend
calls it from its display link while the status is not `done`, and again
when output arrives. Order: newly scrolled-in history first (catch-up), then
the screen region (looked at again only when the terminal changed: the
generation counters, the cursor row and a copy of the cursor's row), then
older history newest to oldest. No thread, channel or wake-up is added.

The budget (and the C API's cap, `TESSERA_SEARCH_MAX_STEP_MICROS`) is
checked between pieces (amended in #46b). A chunk runs one regex pass per
64 KiB piece of each segment it holds, up to about 128 pieces on text of
heavy combining marks, so the chunk is kept across steps and its scan
resumes at the next piece: a step runs at least one piece and overruns its
budget by at most one piece's regex time. That time depends on the
pattern and has no fixed ceiling: seconds for `\w*X|\w` on such text
(`a_step_ends_within_one_piece_of_its_budget` checks the bound of one
piece, not a time; before, a step took 28 s to 91 s). A chunk's matches are filed when its last piece has
run, so the count grows chunk by chunk and never shows half a chunk; what
changed in the terminal meanwhile is caught by the next plan, as between a
copy and its search. A zero budget runs at most one piece.

**Screen passes reuse what did not change.** A segment's text depends only
on its rows and on whether it continues above and below, so a pass keeps a
copy of the rows it scanned (when the region is at most `MAX_KEPT_CELLS`,
128 Ki cells) and the cuts it made. The next pass takes a segment's matches
from the last pass, without a search, when the cut is the same and every
row is unchanged. A row is known unchanged without a compare when the
screen has not scrolled, the history was not cleared, the same screen is
showing and the grid's damage since that pass (`Grid::damage_since`,
ADR 0016: never omits a changed row) does not cover it; otherwise it is
compared with its copy (characters, cell flags, clusters, wrap; not
styles). Output that rewrites a few rows then costs the copy and scan of
those rows, not the screen's. Reuse is off once a cap gave matches way.

**Keys and liveness.** Matches are keyed by absolute line (ADR 0023's
numbering, shifted by a constant origin because a resize can re-wrap the
history into more rows than were pushed). History lines never change, so
they are scanned once; evicted lines drop their matches (when eviction
cuts a wrapped line, what is left of it is searched again as a line of
its own, as a fresh search reads it); clear scrollback
(ED 3) and RIS drop them through the history-clear generation; a resize
starts the search over (the history is re-wrapped). On the alternate
screen only its matches are shown and counted; the primary history's are
kept for when the application leaves it.

**Navigation (D16).** The initial current match is the newest. "Older"
(⌘G in the find bar) steps toward older output and "newer" (⌘⇧G) toward
newer; both wrap. A move scrolls the view to centre the match when it is
not in view (`ScrollTarget::Offset`), never on the alternate screen. New
output keeps the current match where it is.

**Caps (S5).** `MAX_MATCHES` = 100 000: the newest are kept and the
status's `truncated` makes the count read "more than"; once full the
older-history scan stops. "Copy all matches" returns the matches' text in
document order joined by newlines, at most `MAX_MATCHES_TEXT_BYTES` (1 MiB),
stopping before the match that would pass it; it re-reads the terminal 256
lines per lock hold.

**Overlays (D7).** `render_view` asks the search for the overlays of the
rows the frame shows (`Match`, `CurrentMatch`) after the terminal lock is
released; they join the selection's decorations, so a changed match set
redraws and an unchanged one does not. They draw over the selection, as on
main. The last overlays are kept with the window they were computed for and
handed out again while the window is the same and the search did not
change (any start, clear, step or move drops them), so an idle frame costs
a copy, not a walk of the matches. Lock order: the search lock before the terminal lock, never inside
it; the reader thread never takes it.

**C API.** `tessera_session_search_start` (pattern as pointer and length,
UTF-8 checked, the length checked before the bytes are read; `options` bits
`TESSERA_SEARCH_REGEX`, `TESSERA_SEARCH_CASE_SENSITIVE`; a
`TesseraSearchRefusal` and an optional message buffer), `_step` (budget in
microseconds, clamped to `TESSERA_SEARCH_MAX_STEP_MICROS`), `_older`,
`_newer`, `_status`, `_clear` and `_matches_text` (the selection's copy-out
convention). A refused pattern returns `TESSERA_ERROR_INVALID_ARGUMENT`.

**Logs (S8).** Neither the query nor match text is logged: the core error
for a refused query carries no source, and the C layer logs only the
refusal kind.

**Find bar (#46b, amended).** The app's find bar is a small panel floating
in the top trailing corner of the focused pane, over the terminal (clear of
the scroll bar): it takes no layout slot and never resizes the grid. It
holds the query field, the count ("3 of 12", "more than 100 000" when
`truncated`, "No matches"), or a refused regex's reason (the core's
message, which quotes the pattern, made safe to show per S3), older and
newer buttons, a case control and a regex toggle. Each pane has its own
search (its own query, toggles and current match), kept while the user
switches pane or tab; closing a pane frees its search with its session.

- Keys (D16a): Return in the field and Find Next (⌘G) move to the older
  match; Shift-Return and Find Previous (⌘⇧G) to the newer one; both wrap.
  Escape in the field closes the bar, clears the search (its highlights go)
  and gives the keyboard back to the terminal. ⌘G or ⌘⇧G with the bar
  closed opens it with the pane's last query. The commands go through the
  key map (`find`, `find_next`, `find_previous`, `use_selection_for_find`
  on ⌘E, `copy_find_matches` with no chord) and Edit > Find.
- A new query makes its newest match current as soon as a step finds it,
  and the view scrolls to it; a move asked for before the first match waits
  for it.
- Case: smart case by default (case-insensitive unless the query has an
  uppercase letter; in regex mode an escaped character and a `\p{…}`
  class name do not count), or match case, or ignore case. Smart case is
  the app's: the core gets a plain case-sensitive flag.
- Typing is debounced (120 ms) before the query reaches the core; Return
  and ⌘G send it at once.
- Stepping: the pane's display link makes one `step` per refresh with a
  4 ms budget while the bar is open and the status is not `done` or output
  arrived since; a step overruns it by at most one piece. With the bar
  closed the search is cleared and the display link makes no search call.
- Use Selection for Find takes the selection's first line, drops control,
  format and separator characters (S3), escapes it in regex mode and cuts
  it to 1024 bytes; with the bar closed it sets the query ⌘G searches for,
  as other Mac apps do. Copy All Matches puts `matches_text` on the
  pasteboard and says in the bar when it was cut at 1 MiB.
- Neither the query nor match text is logged (S8).

## Measured

Release, Apple silicon, wave load 13 to 25 (`ab` rows: instruction counts, net of the probe overhead):

| What | Result |
|---|---|
| Terminal lock hold per step, 10 000 × 2048 history under a flood (`search_lock_hold_is_bounded`) | p50 5 µs, p99 46 µs (bar 1 ms) |
| Full scan of 1 000 000 lines × 80 columns, no match (`a_million_lines_without_a_match_scan_within_two_seconds`) | 0.28 s (bar 2 s) |
| The same with a match per line, to the 100 000 cap | 0.03 s |
| Compiling `\w{209}` (the largest accepted Unicode class repeat) | 28 ms, 38.6 MB peak, 11.7 MB kept |
| Memory held by one search, literal, at the 100 000-match cap | 3.6 MB |
| Memory held by one search, `\w{209}` on 2048 columns | 16.3 MB |
| `render_view`, no search active, 11 corpus lines (`cargo xtask ab`, three fresh probes) | 0.989 to 1.015 of main (bar 1.02) |
| `render_view` scrolling one line a frame, a search showing 36 matches (`v_`) | 1.07 to 1.43 of the same without a search (bar 1.15); ascii +3.4 k instructions a frame |
| `render_view` idle, the same search (`i_`) | 1.27 to 1.70; ascii +1.6 k instructions on a 5.9 k frame |
| Output of one line a frame with a step a frame (`s_`) | 1.76 to 2.43; ascii +87 k instructions a frame |
| A line redrawn in place a frame with a step a frame (`r_`) | 1.68 to 1.94; ascii +57 k instructions a frame |

The per-search total is bounded by: the compiled program (≤ 10 MiB plus the
regex crate's overhead), two lazy DFA caches (≤ 4 MiB each), the match
list (≤ 100 000 × 24 B, 3.1 MB with the deque's growth), the row copy
(≤ 64 Ki cells × 20 B, 1.3 MB), the haystack (≤ 64 KiB of text and its
cell map; a run of plain ASCII cells is one entry), the last pass's kept
rows and their spare allocations (≤ 2 × 128 Ki cells × 20 B, 5.2 MB) and
the last overlays (one window's worth). Compilation of a pattern near the size limit peaks at about
40 MB transiently and runs on the calling thread under the search lock.

## Consequences

- A search costs nothing per frame while none is active; with one active,
  each frame adds a step and the overlays of the rows shown. The overlays
  cost about 25 instructions per visible match in the search plus the
  renderer's compare and draw, so a frame showing many matches misses the
  1.15 bar by a few thousand instructions (well under a microsecond). A
  step after output copies and scans the rows the output touched, which
  costs about as much again as the output's own parse: the step's budget
  (D6) bounds it to within one piece (see Stepped), and with no output and
  nothing left to scan a step is one short look at the terminal.
- A match across a segment cut of a very long logical line (more than the
  segment period's rows) is missed. The cut rows are fixed, so the result
  does not change as output arrives. So is a match across a piece cut of a
  segment with more than 64 KiB of text; a piece cut depends only on the
  segment's cells, so it does not move as output arrives either.
- Under a continuous flood, catch-up of new history has priority, so the
  older-history scan (and `done`) can lag until output pauses.
- `search_status` between a resize and the next step reports the old
  counts; the overlays are already gone.
- The find bar (#46b, see Find bar) makes no search call per frame while it
  is closed, and at most one 4 ms step per frame while it is open.
- Searches are per pane and the find bar opens on the focused pane only, so
  two panes searching cost two steps a frame; a closed bar's search is
  cleared, so only open bars cost anything.
