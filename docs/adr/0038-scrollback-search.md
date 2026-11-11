# ADR 0038: Scrollback search: stepped scan, caps, lock budget

**Status:** accepted · 2026-11-06 · amended 2026-11-07 (#46b: per-piece step bound, find bar) · amended
2026-11-08 (#331, #318: a ceiling per piece, lines skipped, stale truncation, reveal)

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
resumes at the next piece. A zero budget runs at most one piece.

**A ceiling per piece (amended in #331).** A piece's regex time depends on
the pattern: `\w*X\b|\w` takes 19 s to 32 s on a 64 KiB piece of
combining marks, Cyrillic or CJK, and before this amendment one step took
all of it. The engine now reads the clock after every regex run inside a
piece, and once before the first, and stops at the piece's deadline
(reading it every 16 runs, as first amended, let one step run 16 slow
calls: about 12 s in the app; #331 round 1). A literal query (not a
regex) reads it every 16 runs (`LITERAL_RUNS_PER_CLOCK_READ`, #378): a
literal's run ends at its match, so 16 of them scan disjoint text, no more
than one run over the piece; reading the clock after each match cost about
a quarter of a dense literal search:

- The step's first piece may take `max(4 × budget, MIN_PIECE_TIME)`
  (16 ms; a zero budget still searches). One that runs out is searched
  again from its start in halves: that piece is retried at half its own
  size (`PieceLimit::halved_from`, so the first retry already cuts a piece
  far below the limit), and the piece limit (`PieceLimit`, bytes and
  cells) for the rest of the search drops one halving from what it was,
  so a short piece late from a stall does not cut every later line to the
  floor (#368). Both go down to one row of cells and `MIN_PIECE_BYTES`
  (1 KiB). A piece searched again that runs out again is slow at its size,
  not stalled, so later pieces follow it: the search's limit becomes its
  next half, or its own limit when it is skipped (#378). Such a piece no
  larger than that floor that already had the floor's deadline (below) is
  skipped at once: searched again, it would be the same run. Above the
  floor, a piece late the first time is always searched again, even when
  four budgets (62.5 ms or more) are the floor's deadline: one stall does
  not skip a row (#378).
- At that floor the first piece may take `max(4 × budget,
  LINE_TIME_LIMIT)` (250 ms). One that still runs out is skipped with the
  matches it found before the stop, and the status's `lines_skipped` says
  so (the count reads "some lines skipped" in the find bar) until those
  lines are gone. "Line" here is one row, or 1 KiB of it. A row whose
  single regex run outlasts that deadline is skipped the same way.
- Halved pieces cut a logical line inside where the full limit would not,
  and a match across such a cut is not found. A segment cut at a halved
  limit therefore counts as skipped too (`lines_skipped`), so the count is
  never short without saying so.
- A later piece of the step gets the step's own deadline; one that runs
  out is not halved but runs first in the next step, at a first piece's
  deadline.

So a step returns within `max(4 × budget, 250 ms)` plus one regex call
(`no_step_outlasts_four_budgets_and_a_quarter_second`, release only, checks
each step against its own first piece's deadline plus its longest call and
25 ms of slack). The
deadline is checked after the last call of a piece too, the one that finds
nothing more: before that (#331 adversarial run, F9c), a piece whose last
call ran past the deadline counted as on time and was never halved or
skipped. That call is the residual cost, accepted: it is not interruptible
and is bounded by bytes, not time. It runs on at most one piece's text, 64
KiB until a slow piece halves the limit, at most `MIN_PIECE_BYTES` once at
the floor, and costs up to that length times the program's size. No time
bound holds for it; measured (release, 4 ms budget):

| Case | Worst step |
|---|---|
| `(?:e{1,400}){1,250}\b\d`, one 8000-byte line of `e` (load 6-15) | 7.3 s for the first call, then 3.5 s, 1.6 s and 0.63 s as the retries halve the line itself (14.4 s in all); before halving from the piece's own size, four 7.3 s steps in a row (halving 64 KiB to 8 KiB did not cut the line), 36.5 s in all; 20.5 s in one step before the F9c fix |
| same pattern, rows of 1100 `e` at the floor | 0.65-0.70 s a step, one 1 KiB call each, every such row skipped and reported |
| same pattern, 3000 `e` edited one key at a time | 2.7 s for the first search, 43 ms a step on later edits (the limit stays halved); before the fix 2.5 s on every edit |
| `(?:.{1,200}){1,40}\b\d`, 8000 bytes of prose, edited | 0.70 s first, 22 ms later; before the fix 0.7 s on every edit |
| `(?:[\x{434}\x{4e2d}e\x{301}]{1,200}){1,40}\b\|\w`, 64 KiB of combining marks, Cyrillic and CJK | 0.5-0.6 s at load 13, 1.22 s at load 35, 2.1-2.9 s in probes at load 35-50 |
| `(?:\w{1,40}){1,5}\b\d` (no match), same texts | 0.11-0.13 s, halved to 8 KiB |

So a pathological pattern costs about 0.65-0.9 s per KiB in one call: the
first call of a query can block the main thread for many seconds, and
each retry at half the slow piece's size about half as long; after that the search
runs at the floor, and a row whose one call outlasts the 250 ms is skipped
and reported. `\w*X\b|\w` stays within 17 ms a step at budgets 0 and 4 ms
(205 ms at 50 ms), its longest call 30 ms. Moving the step off the main
thread would remove the residual from the frame; it is the follow-up. A piece's matches are kept apart until it
finishes or is skipped, so a piece searched again is never counted twice.
Under heavy load the wall clock can halve a piece that was not slow; the
search is then slower, and long lines it cuts are reported as skipped.

A chunk's matches are filed when its last piece has run, so the count
grows chunk by chunk and never shows half a chunk; what changed in the
terminal meanwhile is caught by the next plan, as between a copy and its
search.

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
older-history scan stops. `truncated` (amended in #318) clears once the
lines whose matches gave way are gone (ED 3, eviction, a fresh pass of the
screen or the alternate screen), as a fresh search would report it.
"Copy all matches" returns the matches' text in
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
a copy, not a walk of the matches. Lock order: the search lock, then the
view table, then the selection and the terminal (amended in #318:
`render_view` takes the session's search lock before the view table, so a
step running on another pane's search never stalls every frame on that
table); the terminal lock is never held while the search lock is waited
for, and the reader thread never takes it.

**C API.** `tessera_session_search_start` (pattern as pointer and length,
UTF-8 checked, the length checked before the bytes are read; `options` bits
`TESSERA_SEARCH_REGEX`, `TESSERA_SEARCH_CASE_SENSITIVE`; a
`TesseraSearchRefusal` and an optional message buffer), `_step` (budget in
microseconds, clamped to `TESSERA_SEARCH_MAX_STEP_MICROS`), `_older`,
`_newer`, `_reveal` (amended in #331: scrolls to the current match without
moving it), `_status`, `_clear` and `_matches_text` (the selection's
copy-out convention). The status carries `matches`, `current`, `active`,
`done`, `truncated` and `lines_skipped`. A refused pattern returns
`TESSERA_ERROR_INVALID_ARGUMENT`.

**Logs (S8).** Neither the query nor match text is logged: the core error
for a refused query carries no source (its `Debug` form neither, #318),
and the C layer logs only the refusal kind.

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
  and the view scrolls to it with one `reveal` call (amended in #331); a
  move asked for before the first match waits for it.
- Case: smart case by default (case-insensitive unless the query has an
  uppercase letter; in regex mode an escaped character and a `\p{…}`
  class name do not count), or match case, or ignore case. Smart case is
  the app's: the core gets a plain case-sensitive flag.
- Typing is debounced (120 ms) before the query reaches the core; Return
  and ⌘G send it at once.
- Stepping: the pane's display link makes one `step` per refresh with a
  4 ms budget while the bar is open and the status is not `done` or output
  arrived since; a step returns within 16 ms, or 250 ms at the halving
  floor (`max(4 × budget, 250 ms)`, see A ceiling per piece), plus one
  regex call. With the bar
  closed the search is cleared and the display link makes no search call.
- Use Selection for Find takes the selection's first line, drops control,
  format and separator characters (S3), escapes it in regex mode and cuts
  it to 1024 bytes; regex escaping is per unicode scalar (a `.` with a
  combining mark is still escaped, #331); with the bar closed it sets the
  query ⌘G searches for, as other Mac apps do. Copy All Matches puts
  `matches_text` on the pasteboard and says in the bar when it was cut at
  1 MiB, or, past the match cap, how many of "more than 100 000" it
  copied.
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
| The same after the ceiling per piece (#331, one probe, load 11 to 6) | 0.992 to 1.013 of main (bar 1.02) |
| A step a frame after one line of output, #331 vs main, both searching (`bs_`) | 0.990 to 1.004 |

Active-search cost (#318), re-measured in #331 (2026-11-08, load 4 to 17):
each row is head with a search against head without one, so it is what a
search costs, not a regression, and has no bar.

| What | Result |
|---|---|
| `render_view` scrolling one line a frame, a search showing its matches (`v_`, 240 000 frames) | 0.99 to 1.46; ascii +3.2 k instructions a frame |
| `render_view` idle, the same search (`i_`, below the instruction floor even at 240 000 frames) | 1.32 to 1.78; ascii +1.5 k instructions a frame |
| Output of one line a frame with a step a frame (`s_`) | 1.42 to 2.70; ascii +106 k instructions a frame |
| A line redrawn in place a frame with a step a frame (`r_`) | 1.40 to 2.53; ascii +59 k instructions a frame |

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
  renderer's compare and draw, a few thousand instructions a frame (well
  under a microsecond). There is no ratio bar on an active search (amended
  in #331, replacing the 1.15 bar it could not meet): Measured gives its
  cost. A step after output copies and scans the rows the output touched,
  which costs about as much again as the output's own parse: the step's
  budget (D6) and the ceiling per piece bound it, and with no output and
  nothing left to scan a step is one short look at the terminal.
- A pattern too slow for one row within 250 ms leaves that row's matches
  out, and says so (`lines_skipped`), rather than stalling the frame; the
  count is then a lower bound. The pieces halved for one slow line stay
  small for the rest of the search, which costs speed on later text and
  can lose matches across the cuts in its long lines; those lines are
  reported as skipped too. A halving depends on the wall clock, so under
  load a live search and a fresh one can differ there; both say so.
- A match across a segment cut of a very long logical line (more than the
  segment period's rows) is missed. The cut rows are fixed, so the result
  does not change as output arrives. So is a match across a piece cut of a
  segment with more than 64 KiB of text; at the full limit a piece cut
  depends only on the segment's cells, so it does not move as output
  arrives either (cuts at a halved limit are reported, above).
- Under a continuous flood, catch-up of new history has priority, so the
  older-history scan (and `done`) can lag until output pauses.
- `search_status` between a resize and the next step reports the old
  counts; the overlays are already gone.
- The find bar (#46b, see Find bar) makes no search call per frame while it
  is closed, and at most one 4 ms step per frame while it is open.
- Searches are per pane and the find bar opens on the focused pane only, so
  two panes searching cost two steps a frame; a closed bar's search is
  cleared, so only open bars cost anything.
