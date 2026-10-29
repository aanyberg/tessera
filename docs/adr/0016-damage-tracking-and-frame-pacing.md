# ADR 0016: Damage tracking, frame pacing and whole-cluster shaping

**Status:** accepted · 2026-10-28

## Context

Until issue #21 every frame re-shaped every visible row and rebuilt and
uploaded the whole instance buffer (ADR 0010), even when nothing had
changed, and `App::render_view` drew whatever the frontend asked for, as
often as it asked. The issue asks for three things:

- re-shape and re-upload only the rows that changed, and skip a frame
  without touching the GPU when nothing changed (idle GPU usage zero);
- coalesce bursts of output, so `cat` of a large file renders at display
  rate while input stays responsive;
- the hot-path follow-ups deferred from #20 (sRGB decode per colour, per-row
  vector reuse, `instance_capacity`, `CursorState`), from #4 (shape whole
  grapheme clusters, not their base character) and from #19 (shaping
  tests and edge cases).

The constraints:

- ADR 0014's lock rule: the session's reader thread applies output under
  the terminal lock, so the frontend may hold it only to copy the screen,
  never while shaping or waiting for a drawable.
- `App::render_view` calls `view.capture(term.grid())` with a shared
  `&Grid` and then `view.render(focused)`; it is owned by another task
  this wave and stays byte-identical. `tessera-term` is also out of scope,
  so the emulator cannot be changed to report damage itself.
- The print path (`Grid::put_char` for every character of output) must
  not get measurably slower: `Terminal::advance` may cost at most 2% more
  instructions on any line of the wave-5 corpus.
- `FrameInput` keeps #20's shape (ADR 0010); new fields need defaults so
  existing callers (golden tests, `tessera-devtool`, #23's dev window)
  keep the full-frame path.

## Decision

### The grid records damage as per-row generations

`tessera-grid` gains `damage.rs`: a `DamageTracker` inside `Grid` with a
generation counter, one stamp per visible row (the generation of its last
change), a whole-screen stamp, and a flag for the cursor's row. Readers
see it as

```rust
pub enum DamageRows { None, Rows(RangeInclusive<u16>), All }
pub struct Damage { pub rows: DamageRows, pub generation: u64 }

impl Grid {
    pub fn damage(&self) -> Damage;              // since the last take
    pub fn take_damage(&mut self) -> Damage;     // ...and start over
    pub fn damage_since(&self, generation: u64) -> DamageRows;
}
```

`damage_since` is what the view uses: it holds only `&Grid` (see the
constraint above), so it cannot reset anything. It remembers the
generation of its last capture and asks what changed after it. A
generation the grid never handed out (another grid's, or `0`) answers
`All`: every grid starts its generations in its own range of 2^40, taken
from a process-wide counter, and `Grid::reset` keeps its tracker, so a
reader that stays with one grid never mistakes a new screen for an
unchanged one. The ranges are not unique forever: the counter wraps after
2^24 trackers, and every new grid and every clone takes one (RIS takes
none since #107: it resets in place and keeps its tracker), so the wrap
needs 2^24 grids or clones; a tracker past 2^40 stamps also runs into the
next range. A reader that moved between grids could then
take a stale generation for a recent one. None does today: a view reads
one session's grid, and that grid lives as long as the session. A cloned grid
starts a fresh range with everything damaged (the clone and the original
diverge, so sharing a range would let one's generation look recent to the
other). `Grid::damage_generation` returns the current generation without
computing rows, for the view's capture.

What each primitive records:

| Change | Damage |
|---|---|
| Write at the cursor (`put_char`, `put_cell`, wide characters, cluster attachment, padding), ICH, DCH, ECH, EL | the cursor row: one store of the flag, no stamp |
| Cursor move within a row (CR, CUF, CUB, CHA, HT, CBT) | the cursor row (flag) |
| Cursor move to another row (CUP, CUU, CUD, LF, RI, DECRC) | the row it leaves (stamped) and the row it arrives on (the flag is set again for it) |
| Soft wrap | the wrapped row (flag, then stamped as the cursor leaves) |
| Scroll, IL, DL, ED below/above of part of the screen | the rows of the region or range |
| Full-screen scroll (SU, LF at the bottom), ED 2, alternate screen enter or leave, resize, RIS | `All` |
| `Grid::line_mut(row)` | that row, whatever the caller does with it |
| Pen, tab stops, scroll region, scrollback cap, the saved-cursor slot (DECSC, `reset_saved_cursor`) | nothing (no visible change) |

The cursor-row flag is what keeps the print path cheap: writing a
character is one byte store on top of what `put_char` did before; the
flag becomes a stamp only when the cursor leaves the row or an owner
takes the damage. Arriving on a row sets the flag for it again, so a
`&Grid` reader sees the cursor row as damaged from the moment the cursor
arrives until it leaves (or an owner takes the damage), whether anything
on it changed or not; a reader that wants exact answers compares that row
itself (the view does). The answer is a superset by design: false
positives are allowed, false negatives would leave stale cells on screen.

A property test (`crates/tessera-grid/tests/damage.rs`, 1024 cases of up
to 120 operations over every primitive, including wide characters,
clusters, regions, the alternate screen, resizes, resets and `line_mut`)
checks at every checkpoint that every row whose cells, cluster text or
wrap flag changed, and the rows the cursor left and arrived on, are inside
both the `damage_since` and the `take_damage` answers. The owner takes the
damage at every checkpoint, which also folds the sticky cursor-row flag,
and a third of the operations run isolated: fill a row with text, put the
cursor on it, checkpoint, apply one edit, checkpoint. So an edit whose
recording call is missing cannot hide behind damage an earlier operation
recorded. Removing the recording call of any of these fails it: printing
(`put_cell`), wide characters, cluster attachment, padding the last
column, ICH, DCH, ECH, EL, IL, DL, both scrolls, ED 2, entering or
leaving the alternate screen, resize, reset and `line_mut`. The one call
whose removal survives is the flag set inside the soft wrap: every caller
writes on the row (and sets the flag) right after the wrap, or the line
feed stamps it, so it is redundant, kept as a guard. (Round 1 of the
review found that the first version of the test, which took the damage
only at every other checkpoint, let the ICH, DCH, ECH and attachment
removals survive behind the sticky flag.)

**Why not diff in the view?** Comparing every cell of the screen on each
capture costs O(cells) under the terminal lock on every frame, also when
one character changed, and does not say which rows to skip shaping
without the same comparison. The generations cost O(rows) per capture (a
scan of 24 to 1024 stamps) plus a comparison of the reported rows only.

**Why not a single dirty range with `take_damage`?** It needs `&mut Grid`
at capture, which `render_view` does not have, and only one consumer
could ever take it.

### The view computes damage under the lock and paces frames

`View::capture` (under the terminal lock) asks `damage_since`, compares
the reported rows with its snapshot copy, copies the ones that differ
(cells and their `GraphemeTable`), and adds the rows that really changed
to the damage pending for the next frame. A different grid size copies
everything in one straight copy, without comparing or pre-filling, which
is also the path of every full-screen scroll and resize. The comparison
drops the grid's false positives, so an idle screen produces no damage.
Partial captures cost what their reported rows cost. A full capture in
a release build (best of 30, Apple M4, loaded machine) takes 3 µs at
60x200 cells of text, 15 µs at 96x430 (text or blank) and 1.0 ms at the
largest grid, 1024x2048, about what #20's copy took. The first version of
this code compared every row on the full path too and filled the buffer
with defaults first: 18 µs, 80 µs and 4.7 ms on the same screens (the
adversarial review found it; a test now pins that the full path compares
no row and copies exactly). A screen made only of clusters (`e` + U+0301
in every cell) costs one `Box<str>` clone per cluster: 0.55 ms at 96x430,
up to about 1.2 ms on a loaded machine. That is a known cost of copying
cluster text under the lock; a shared, reference-counted table would
remove it if it ever matters. A `yes` flood's captures stay under 1 ms in a
debug build on a loaded machine.

`View::render` (after the lock is released) returns
`FrameStatus::Skipped` without calling the surface, so without acquiring
a drawable, when nothing changed since the last presented frame: no
pending damage, the same cursor position and focus, and
`Renderer::needs_redraw()` false (no frame drawn yet, a failed frame, a
theme change, or a row waiting for the fallback font index). The previous
drawable stays on the layer, so an idle terminal does no GPU work.

It also presents at most one frame per `MIN_FRAME_INTERVAL` =
`Duration::from_micros(4166)` (1/240 s, the fastest Mac display). A call
that comes too early captures (the snapshot always holds the newest
screen) and returns `Skipped`; its damage carries over. Frames are paced
on a schedule rather than from the last present: each presented frame is
due an interval after the previous one's due time, may come up to
`FRAME_SLACK` early and still count, and its due time never lags the
clock by more than the slack, so an idle view does not bank frames. The
first and last of `n` frames are then at least `(n - 1)·I − 2·slack`
apart.

The slack is half the interval (`Duration::from_micros(2083)`), so over
any window `T` at most `T / I + 2` frames are presented, and a render is
paced only when it is the third within one interval: its gap and the
previous one add up to less than `I`. A display link of at most 240 Hz
never does that unless it delivers a tick more than a whole period late,
so no Mac display is ever paced. A simulation of the schedule over 100 000
ticks at 60, 120, 144, 165 and 240 Hz paced none with ticks on time, late
by up to 3 ms, or late by up to half or 95% of a period; it paced some
(0.9% at 144 Hz to 18% at 240 Hz) only with ticks late by up to 8 ms,
more than a period above 120 Hz, which reorders them. A unit test runs
the schedule over hand-picked late and early ticks and 20 000
pseudo-random ones below 95% of a period at each of those rates; it fails
with the first version's constants (1/120 s with a 1 ms slack, where one
tick 3 ms late at 120 Hz paced the next on-time tick, and a 144 Hz link
was paced on a sixth of its ticks with a half-interval slack).

So pacing no longer shapes what a display-link host sees. It bounds a
host that renders on every output notification (a development window
like #23's) to 240 frames per second, and damage tracking keeps each of
those frames to the rows that changed. The flood test feeds 10 000 reads
of 4 KiB of `yes` output while capturing and rendering in a loop for two
seconds, in a debug build: 481 frames presented on an idle machine (240
per second, at the limit) and about 350 under load, inside the required
60 to 482 (`2 s / I + 2`); without pacing about 1 000 are presented, so
removing it fails the test.

**`Skipped` has three causes, and a host cannot tell them apart yet.**
`FrameStatus::Skipped` (and `TESSERA_FRAME_STATUS_SKIPPED`) means nothing
was drawn by this call: nothing changed, the call was paced, or the
surface had nothing to draw into. A host that renders from a display link
of up to 240 Hz is never paced and need not care: its next tick draws
whatever is pending. A host that renders faster, or from output
notifications, and stops after a `Skipped` would leave a paced frame's
content undrawn; it must schedule another render after any skip. The lead
decided (rounds 1 and 2 of the review) to keep the enum and the C ABI
unchanged for now, document the three causes on every `Skipped`, and
raise the limit to 240 Hz with a half-interval slack; a distinct paced
status (#101) is the proper fix. Residual risk: a host that renders
faster than 240 Hz, or in bursts closer than about half an interval, and
then idles (no display link, no further notification) shows the paced
call's content late, until something renders again.

**The development window's retry.** `tessera-devwin` (#23) renders on
every output and input event and retried every `Skipped` frame one refresh
(16 ms) later while visible, which was right when `Skipped` only meant "the
surface was not ready". With the new causes, one render with nothing new
(an OS redraw request, a key a keybinding consumed, a no-op resize) started
a retry loop that never ended: measured on a debug build after a single
such redraw, 50 renders a second, every one of them `Skipped`, for as long
as the window stayed idle (no GPU work, but 0.79 s of CPU in 15 s against
0.40 s). The lead chose to cap it in #21: the window retries at most two
skipped frames in a row (a presented frame, new output, input, a resize,
an occlusion change or a re-attached surface start the count over), and
the same measurement then shows two retries and no further renders. One
retry suffices for a paced frame, because 16 ms is longer than the frame
interval, so the retry is due; a unit test pins the policy and that
inequality. A surface that stays unavailable for longer than two retries
waits for the next event, acceptable for a development tool. A debug
build of the window also draws fewer frames during a flood than main's did
(118 against 200 over 60 MB of output, measured by the adversarial review):
each presented frame re-shapes every row a full-screen scroll damaged, at
debug speed. A release build keeps up. A distinct
paced status (#101) lets every host drop these heuristics.

**Why the view owns pacing.** The renderer has no clock (the cursor's
blink is the host's, ADR 0010), `render_view` cannot change this wave, and
the view is the one place that sees both the capture and the presented
frame. Pacing there also covers a host that renders on every output
notification rather than from a display link. The reader thread is never
involved: it only takes the terminal lock for `advance`, and the view
holds that lock only to copy.

### The renderer keeps every row's work between frames

`FrameInput` gains two fields, both defaulted by `FrameInput::new`:

- `damage: DamageRows` (default `All`): the rows that changed since the
  last frame this renderer drew. The renderer trusts it: other rows must
  hold the same cells and clusters as then.
- `clusters: &[GraphemeTable]` (default empty): each row's cluster text.

`FrameInput` is no longer `Copy` (a `RangeInclusive` is not); it is
`Clone`. Nothing used the copy.

`Renderer` keeps, per visible row, its shaped glyphs (one vector reused
from frame to frame) and the instances of its background, glyph and
decoration quads (three reused vectors). The instance buffer is laid out
as one section per pass, in draw order, of one slot per row, then a slot
for the cursor; a slot holds its row's instances followed by empty ones
(zero-sized quads that rasterise nothing), so each pass is still one draw
over its section and the draw order of ADR 0010 holds. A slot has room to
grow (the next power of two, at least 4 background, 32 glyph and 4
decoration instances); a row that outgrows its slot lays the buffer out
again and uploads it whole. A CPU mirror of the buffer means a frame
uploads only the slots it rebuilt, merged into one write per pass for
consecutive rows, plus the cursor's slot when it changed. A failed frame
or a full rebuild clears the mirror, so the next frame lays the buffer out
and uploads all of it: otherwise a slot the mirror recorded but the GPU
never received (a failed upload) could be skipped as unchanged later (a
test injects a failing upload and checks the cursor still moves).

Every frame still clears the target and draws all instances: a swapchain
image does not keep the previous frame's pixels, and drawing cached
instances is cheap; what damage saves is the shaping, building and
upload.

A frame re-shapes the damaged rows, and every row when the layout
(target size, cell metrics, padding), the shaper's pixel size, the theme
or the number of rows changed, after a failed frame, or for `All`. It
also re-shapes a row whose length changed, and a row that drew a
`.notdef` glyph (glyph id 0, from a font face) while the fallback index
was still building, once `Fonts::fallback_index_ready()` turns true, as
ADR 0007 asked of a renderer that caches shaped rows. Rows kept from an
earlier frame hold atlas texel positions, so when the atlas evicts or
resets during a frame (the counters in `AtlasStats` change), every row's
instances are rebuilt once from its cached glyphs (no re-shaping); a full
rebuild touches every visible glyph, so it cannot evict one a row still
uses. An `AtlasFull` keeps ADR 0010's policy: one reset per frame, then
an error.

`Renderer::stats()` (`RenderStats`) counts frames, rows shaped, rows
built, instances uploaded and re-layouts, for tests and diagnostics.

Fast path and reference agree: a test renders 40 steps of random edits
(text, CJK, emoji, combining marks, colours, inverse, hidden text,
cursor shapes and focus) incrementally, with the grid's own damage, and
compares every frame byte for byte with a fresh renderer drawing the full
frame; another forces evictions in a 96x96 atlas while one row stays
unchanged; another checks that a theme change redraws every row. Each of
these fails when damaged rows are not uploaded, when the damage is
ignored, or when an eviction during a frame does not rebuild the kept
rows.

### Hot-path follow-ups from #20

- **sRGB tables.** `palette.rs` resolves 24-bit channels through a
  256-entry sRGB-to-linear table and dimmed colours through tables built
  once per palette (the 256 indexed colours, the defaults, and a dimmed
  table per sRGB byte); a test checks both bit for bit against the
  transfer function (`Rgba::from_srgb8`, `dim`) for every byte, and
  another that `palette.rs` and the passes never call `powf`, decode sRGB
  or dim on their own. `CellColors::foreground` and `background` resolve
  only the slot a pass needs; a test compares them with #20's formula for
  every combination of default, indexed and 24-bit colours with inverse
  and dim.
- **Per-row vector reuse.** A counting allocator (`tests/alloc.rs`,
  counting the test thread only) shows a full frame of 24 rows allocating
  7 704 times on frames 2 to 6 alike (debug build), which is the shaping
  of 24 rows (318 allocations each, inside harfrust and the shaper's run
  buffers) plus 60 per frame plus one write per pass; an undamaged frame
  allocates 60. One allocation more per row fails the test, as does
  dropping the reuse. Measuring it found that slots were uploaded row by
  row (three writes per row, each with a staging buffer); they are now
  merged into one write per pass. An unchanged cursor slot and unchanged
  globals are not uploaded at all.
- **The counting allocator is the one `unsafe` outside `tessera-pty` and
  `tessera-ffi`**: a `GlobalAlloc` in that integration test, a test
  binary of its own that forwards to `System` with `SAFETY` comments and
  is never linked into a library. The lead accepted the exception
  (round 1 of the review); CONTRIBUTING's unsafe rule names it.
- **`instance_capacity`.** It was already raised after the new buffer
  validated; the growth is now a helper (`grow_buffer`) whose unit test
  makes the creation fail and checks that the buffer and capacity are
  unchanged and that the next attempt grows.
- **`CursorState` is `#[non_exhaustive]`** with `CursorState::new` and
  `with_visible`, so #22 can add the two-column cursor's field.

### Whole grapheme clusters are shaped

`Shaper::shape_line_into(cells, clusters, out)` (and
`Shaper::shape_grid_line(&Line)`) shape a `CLUSTER` cell from its whole
text in the line's `GraphemeTable`; `shape_line(cells)` keeps the
base-only behaviour for callers without the table. The run text holds each
cell's full cluster, so harfrust sees the sequence and its cluster maps
back to the base cell, and the glyphs are centred in the cell or the two
cells of a wide cluster as before. The face is the base character's when
it covers every visible scalar of the cluster (ZWJ and variation
selectors do not count), else the face of the first scalar it misses when
that one covers them all. A VS16 asks for emoji presentation: the colour
emoji family is tried first, before even the primary (Menlo has a text
`❤`); a VS15 tries it last. That needs `Inner::resolve_presented` in
`font.rs`, the only change there.

Results (Menlo 13 pt, macOS 26.6): `e` + U+0301 composes to Menlo's own
`é`; `a` + U+0338 draws the base and the overlay in one cell; the ZWJ
family, `👍🏽`, `🇸🇪` and `❤️` are each one Apple Color Emoji glyph over two
cells. `tests/golden/clusters.png` (font tolerance) pins them, and fails
when only base characters are drawn.

### The #19 follow-ups

Taken, with font-independent tests in `shaping.rs`:

- Raw glyph to cluster grouping is a pure function (`group_clusters`)
  tested with synthetic shaper output: one glyph per cell, a base and its
  mark, a merged multi-cell cluster, the last cluster spanning to the end
  of the run, a cluster inside a multi-byte cell.
- Cascadia's bold `<<=` and `<$>` partition as `[(0, 2), (2, 1)]`: pinned
  with synthetic clusters.

Still open:

- **A combining mark on the last character of a leading-drawer ligature**
  (Cascadia `->` with U+0301 on `>`) makes that character's cluster draw,
  so it owns its cell and the ligature's logical span shrinks to one
  cell. Drawing is unaffected; only cursor and selection extents change.
  The behaviour is pinned by a synthetic test so a fix shows up; deciding
  whether a mark-carrying spacer should stay with its ligature belongs
  with #22's selection, which is what consumes spans.
- **Italic switching family** when the primary's italic face lacks a
  character (Menlo Italic has no `Ǟ`, the regular face does, and the
  chain finds Arial Unicode MS instead of synthesising italic from Menlo
  Regular). The fix is in `font.rs`'s fallback order (try the family's
  upright face with synthetic italic before the next family) and needs a
  font-independent seam there to test it; it is a behaviour change to
  ADR 0007's resolution order, not a damage-tracking concern.
- **Powerline symbols** (U+E0A0..U+E0D4, private use) fall back through
  the coverage index to hidden Apple fonts instead of drawing tofu. The
  right answer is probably to draw them procedurally like box drawing
  (ADR 0007), which is a feature with its own design.

### Amendment 2026-10-29: RIS resets in place from the damage record (#107)

`Grid::reset` (RIS, `ESC c`) built a fresh grid and moved the scrollback
and the tracker into it: one allocation per row, every cell written. A
64 KiB read of 32768 `ESC c` on a 96x430 terminal held the terminal lock
about 568 ms, so the UI thread's snapshot waited that long.

`reset` now works in place and allocates nothing at an unchanged size.
Rewriting every cell in place is not enough: it measured 650 ms for the
same read, slower than the rebuild, because a 96x430 screen is 41 280
cells of 20 bytes per reset. Instead the grid remembers the damage
generation right after its last reset (`BlankedAt`, opaque in `Debug`
like the tracker) and asks `DamageTracker::since` which rows changed
after it. Only those rows are cleared; every other row is still the
default blank that reset left, because no primitive changes a visible
row without recording it (the no-false-negatives property this ADR
already requires and `tests/damage.rs` checks). A reset in a row after
another therefore costs O(rows) for the stamp scan plus the cursor's
row, not O(rows x cols). The reset is the whole screen otherwise: when
the alternate screen is active (its rows are not the ones reset blanked),
after a clone (another tracker's range answers `All`), after a resize or
anything else that records `All`, and when the generation has saturated
at `u64::MAX`.

Relying on damage for content, not just for redraws, makes a missing
damage call a correctness bug in RIS as well as a stale frame. Two
checks guard it: debug builds assert after every reset that every row is
the initial blank (so every test that sends RIS checks the damage record
of everything before it), and a property test compares the in-place
reset with the old rebuild after random histories (alternate screen,
regions, saved cursors, tab stops, marks, pens, caps, clones, taken
damage, resets in a row). The result is otherwise unchanged: the same
state as `Grid::with_scrollback` of the same size and cap with the old
scrollback attached, its newest line ended, the tracker kept and the
whole screen damaged.

Measured with `cargo xtask ab bench` against the merge base (case
`ris_flood`: 40 rounds of 200 output lines then one 32768-RIS read, on
96x430 with a 10 000-line cap; instructions retired, best of 15 pairs):
head/base 0.019 (291.0 G instructions on the merge base, 5.64 G on
the branch; wall time 22.2 s against 0.34 s, 555 ms against 8.4 ms per
32768-RIS read). The `#[ignore]`d test `ris_flood_on_a_large_terminal_is_fast`
in `tessera-term` holds one such read under 60 ms in release (best of 5:
8.4 ms).

`Grid::clear_to_cursor` (⌘K, "Clear to Start" in Terminal.app, kitty's
`clear_terminal to_cursor`) shares nothing with reset but its damage: it
records `All`, since it moves every row up.

## Measurements

`cargo xtask ab` against the merge base, instructions retired, best of 15
interleaved process pairs, Apple M4, macOS 26.6, load average 7 to 49
(other agents building). Corpus lines as in the wave-5 performance
corpus.

**`Terminal::advance`** (the grid's bookkeeping is on the print path), a
24x80 terminal with 10 000 lines of scrollback fed in 4 KiB reads; 8 MiB
per line, 16 MiB for `latin_diacritics` and `braille` and 32 MiB for
`box_drawing` so the case clears the harness's size floor:

| line | head/base |
|---|---|
| ascii | 1.008 |
| latin_diacritics | 1.008 |
| cjk | 1.004 |
| box_drawing | 1.007 |
| hebrew | 1.003 |
| arabic | 1.001 |
| punctuation | 1.006 |
| braille | 1.007 |
| emoji | 1.002 |
| sgr_heavy | 1.004 |
| combining_heavy | 1.002 |

**Renderer**, a 60x200 terminal filled with the line, Menlo 13 pt at 2x,
into an offscreen target, readback checksums equal on both sides (head
passes no cluster text, so both draw the same pixels). Each process waits
for the GPU after every frame, so no frame is billed for a backlog of
earlier ones, and each case runs at 100 and at 300 frames: the per-frame
cost is the difference over 200 frames, which removes everything a
process pays once (font database, GPU device, filling the terminal, the
first frame, the readback). "No damage" is head drawing frames with
`DamageRows::None` against main's full frames; "one row" rewrites one row
per frame on both sides (head with that row as damage), compared with
main drawing the same content in full.

| line | full frame, main | full: head/main | no damage: head/main | one row: head/main |
|---|---|---|---|---|
| ascii | 85.9e6 | 1.022 | 0.0044 | 0.041 |
| latin_diacritics | 89.3e6 | 1.025 | 0.0048 | |
| cjk | 247.5e6 | 1.008 | 0.0017 | 0.025 |
| box_drawing | 11.7e6 | 0.950 | 0.038 | |
| hebrew | 95.8e6 | 1.022 | 0.0041 | |
| arabic | 164.8e6 | 1.014 | 0.0025 | |
| punctuation | 84.5e6 | 1.026 | 0.0050 | |
| braille | 78.0e6 | 1.026 | 0.0053 | |
| emoji | 123.1e6 | 1.012 | 0.0034 | 0.045 |
| sgr_heavy | 56.5e6 | 1.007 | 0.0076 | 0.057 |
| combining_heavy | 91.0e6 | 1.028 | 0.0046 | |

Every bar holds (full ≤ 1.05, no damage ≤ 0.05, one row ≤ 0.15). A drawn
frame without damage costs about 0.4e6 instructions (recording and
submitting the render pass); it is largest against `box_drawing`, whose
full frame is cheap because its glyphs are drawn by the built-in box
drawing rather than shaped. In the app such a frame never reaches the
renderer: `View::render` returns `Skipped` first. The `box_drawing` cases
at 100 frames are below the harness's size floor on their own; the
difference between the two frame counts is what the table uses.

The first version of this table ran the frames back to back without
waiting for the GPU and divided whole-process counts; the GPU backlog
that builds up in such a loop was billed to the CPU side, which made a
no-damage `box_drawing` frame look like 0.13 of a full one. The synced
two-count method is the one to re-run.

The full frame costs 1 to 3% more: rows are built into per-row vectors and
copied into the buffer mirror, and glyph slots carry headroom.

## Consequences

- `App::render_view` is unchanged: damage, skipping and pacing live in
  `View::capture` and `View::render`.
- `DamageRows` is one range, so rows 2 and 40 changing re-shape 2..=40.
  The view's comparison makes the range exact at its ends; a set of rows
  is a later refinement if vim-style status-line updates show up in
  profiles.
- The scrollback viewport (#31) lives in app-core; when it changes what
  `Snapshot::capture` copies, a changed scroll offset must mark the view's
  damage `All`.
- #22 (selection, overlays) builds on the per-row slots: a selection pass
  can be another section, or overlay instances can join the cursor slot.
  Overline drawing (#81) also lands there.
- Tests can run a `View` headless (`View::offscreen`, test-only), which
  the skip and pacing tests use.
- A grid's generations stay in its own range for 2^40 stamps, hours of a
  full-speed flood (a stamp is per line or escape sequence, not per
  character). Past that a reader that switched from one grid to another
  could mistake a stale generation for a recent one; no reader switches
  grids today (a view belongs to one session, whose grid lives as long as
  it does).
- Supersedes ADR 0010's "every frame reshapes every visible row" and the
  base-character limitation in ADR 0008 ("Only the base scalar of a
  cluster is shaped") and ADR 0014 ("#21 extends `Snapshot::capture` with
  each line's grapheme-cluster text").
