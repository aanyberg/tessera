# ADR 0010: Render passes, frame input and the atlas-full policy

**Status:** accepted · 2026-10-27 · amended 2026-10-29 (#22: overlay
pass, selection and overlays, underline styles, overline, superscript and
subscript, cursor redraw rules; see "Amendment: #22" at the end) ·
amended 2026-11-03 (#124: highlights stop at the grid's last column, a
cursor on a cut wide character's spacer; see "Amendment: #124") ·
amended 2026-11-08 (#45b: the exit-status gutter, a sixth draw; see
"Amendment: #45b")

## Context

ADR 0007 gave `tessera-render` a shaper and a glyph atlas. Issue #20 draws
with them: cell backgrounds, glyphs, decorations and the cursor, into a
swapchain image or a headless texture. Four things needed deciding:

- what the renderer is given each frame, so that #21 (damage tracking),
  #22 (selection, overlays) and #31 (app-core viewport) can build on it
  without changing its shape;
- how the passes are laid out on the GPU;
- how cell colors become pixels (palette, inverse, dim, opacity, sRGB);
- what happens when the atlas is full. ADR 0007 returns
  `RenderError::AtlasFull` when a frame's glyphs do not fit after
  eviction, and #19 measured that shelf fragmentation could cause it at
  about 10% use under churn.

It also needed a test that catches regressions in all of that headlessly
on the `macos-15` CI runner, whose macOS differs from developers'.

## Decision

### Frame input: rows of cells, not a grid

`Renderer::render` takes a `FrameInput`:

```rust
pub struct FrameInput<'a> {
    pub rows: &'a [&'a [Cell]],
    pub cursor: Option<CursorState>,       // row, col, shape, visible
    pub selection: Option<Selection>,      // start, end, kind (linear or block)
    pub overlays: &'a [Overlay],           // search matches, link underlines
    pub focused: bool,
}
```

It is decoupled from `tessera_grid::Grid`: app-core (#31) chooses the
visible rows (scrollback included) and hands over slices. The renderer
never learns about viewports or scroll offsets. Rows may be of any length;
cells and rows that start outside the target are not drawn. The struct is
`#[non_exhaustive]` and built with `FrameInput::new(rows)` plus `with_*`
methods, so #21 and #22 can add fields without breaking callers.

The cursor's blink phase belongs to the host: it flips
`CursorState::visible` on its own clock; the renderer has none.
`CursorShape` is `Block`, `Beam`, `Underline` and `HollowBlock`; an
unfocused frame draws every shape as `HollowBlock`. On either half of a
wide character the cursor spans both columns (#22); the renderer reads
that from the cells, so `CursorState` has no width field.

### Five passes, one instance buffer, one pipeline

Each frame clears the target to `Theme::background` and draws, in order
(the overlay pass was added by #22, see the amendment):

1. **background**: one quad per horizontal run of cells whose background
   differs from the cleared target (an explicit background, or any cell
   with inverse video);
2. **overlay**: the selection, then search matches and link underlines,
   alpha-blended over the backgrounds;
3. **glyph**: one quad per `ShapedGlyph`, at
   `cell origin + whole-pixel x offset + bitmap bearing`, tinted with the
   owning cell's foreground (coverage layer) or drawn as is (colour
   layer). `HIDDEN` cells draw no glyph and are shaped as blanks, so no
   ligature reaches across a hidden boundary (a hidden `>` must not turn
   a visible `=` into an arrow); bold and italic were already chosen by
   the shaper;
4. **decoration**: underlines in five styles, overline and strikethrough,
   one quad per bar per run of cells with the same decorations and
   colors; hidden cells draw none;
5. **cursor**: block (the cursor color over the cursor's cells, then the
   glyphs reaching into them and the cell's decorations redrawn in the
   cell's background color, clipped to the cursor's cells), beam (a
   stroke at the left edge), underline (a stroke at the bottom), hollow
   block (four strokes). Strokes are one pixel per unit of
   backing scale. The redrawn glyph is scaled by the cursor color's alpha
   like the block itself, so a cursor with alpha 0 draws exactly nothing
   and an opaque one exactly inverts the cell. In between it only
   approximates a blend of the inverted cell with the plain one: with
   cursor color `K`, alpha `a`, glyph coverage `c` and plain pixel `P`,
   the result exceeds the true blend `a·inverted + (1−a)·P` by
   `a·c·(1−a)·(K − P)`, because the cursor color stays under the redrawn
   glyph. At `a = 0.5` the default theme's glyph core lands at 150 where a
   true blend gives 168. An exact blend needs the plain cell's color in
   the shader and is not worth it for a translucent cursor.

All passes emit the same 64-byte instance into one vertex buffer that is
rebuilt every frame (damage tracking is #21); each pass is a range of it
and one `draw(0..4, range)` with a debug group. The buffer grows to the
next power of two a frame needs, capped at the device's
`max_buffer_size`; a frame needing more returns
`RenderError::FrameTooLarge` without drawing, and a new buffer or
pipeline replaces the old one only after it validated, so a failed frame
never leaves the renderer unusable. The instance is

| Offset | Field | Meaning |
| --- | --- | --- |
| 0 | `rect: [f32; 4]` | x, y, width, height in physical pixels |
| 16 | `clip: [f32; 4]` | x0, y0, x1, y1; fragments outside are discarded |
| 32 | `color: [f32; 4]` | linear, premultiplied RGBA |
| 48 | `texel: [f32; 2]` | atlas texel of the rect's top-left (glyphs) |
| 56 | `kind: u32` | 0 solid, 1 coverage glyph, 2 colour glyph, 3 dashes, 4 wave (#22) |
| 60 | `extra: f32` | glyphs: atlas texels per pixel, 1 at their own size, more for superscript and subscript; dashes and waves: the x their pattern's phase starts at, the grid's left edge (#22) |

One WGSL shader (`shaders/quad.wgsl`) expands a triangle strip per
instance and branches on `kind`. Glyphs sit on whole pixels and map one
texel to one pixel, so the atlas is read with `textureLoad` (exact, no
filtering at glyph edges); the smaller superscript and subscript glyphs
are filtered bilinearly by the shader itself, from four loads (#22). No
sampler is bound. The clip rectangle exists for the
block cursor's redraw, which must not spill out of the cursor's one or
two cells when the glyph is a ligature reaching into a neighbour or has
ink beyond its cell; everything else is unclipped. The clip is exactly
the cursor's cells: a test fails when it is removed (a ligature's redraw
paints its other cell) or shrunk by one pixel (a full block `█` under
the cursor leaves cursor-coloured columns). A single
pipeline (one per target format, created on first use) keeps the passes'
order a matter of draw order.

Separate pipelines per pass were the alternative. They would avoid a
branch per fragment and the clip test, but they need four shaders and
four vertex layouts for quads that differ in one fetch. Should profiling
show the branch matters, the passes are already separate ranges.

### Colors

- `Color::Default` is `Theme::foreground` or `Theme::background` by slot
  (`ColorSlot`); `Indexed(0..16)` is `Theme::ansi`; `Indexed(16..=231)`
  is the xterm 6×6×6 cube (levels 0, 95, 135, 175, 215, 255) and
  `Indexed(232..=255)` its 24 greys (8 to 238 in steps of 10), computed in
  `palette.rs`, not stored in `Theme`; `Rgb` is taken as sRGB bytes.
  `Palette` caches all 256 for a frame; `Theme::resolve` answers the same
  question without the cache.
- Inverse video swaps the resolved foreground and background, so a cell
  with default colors becomes `foreground` on `background`. Dim multiplies
  the sRGB-encoded foreground channels by 2/3 (what xterm and Alacritty
  do), after inverse.
- There is deliberately no bold-is-bright: bold selects a heavier face (or
  synthesis) and never changes `Indexed(0..8)` to its bright twin.
- Everything is linear `Rgba` on the CPU, as for `RenderSurface::clear`:
  an sRGB target (what `RenderSurface` prefers and the goldens use)
  encodes on write and blends in linear light; a linear target stores the
  values as they are.
- Targets hold **premultiplied alpha, end to end**. Every quad's color is
  premultiplied and blends with `(One, OneMinusSrcAlpha)`; colour glyphs
  are already premultiplied in the atlas (ADR 0007); and every clear
  (the frame's clear to `Theme::background`, `Gpu::clear_view`,
  `RenderSurface::clear`, `OffscreenTarget::clear`) writes
  `Rgba::to_clear_color()`, i.e. `(r·a, g·a, b·a, a)`. So a translucent
  default cell and a glyph blended over it are in the same convention.
  This matches the compositor: Core Animation treats a non-opaque
  `CAMetalLayer`'s contents as premultiplied, and that is what wgpu's
  Metal backend offers as `CompositeAlphaMode::PostMultiplied`
  (wgpu-hal 30 maps that mode to `layer.opaque = false` and nothing else;
  the name does not change how the bytes are read). A straight
  clear would composite a `#808080` background at `a = 0.5` as if it were
  `#ffffff` at half opacity.
- Opacity: `Theme::background.a` below 1 makes the cleared target, and so
  every default-background cell and the padding, translucent. Cells with
  an explicit or inverse background are opaque, and glyphs and
  decorations always are, so text stays solid on a translucent window.
  `Theme::cursor.a` is the cursor's opacity; `Theme::selection.a`,
  `Theme::search_match.a` and `Theme::search_match_current.a` are the
  highlights' (#22). Only the default background carries the theme's
  alpha: `Color::Default` in the foreground slot resolves opaque.

### Decoration geometry from the cell

The shaper does not expose the face's underline position and thickness,
and `font.rs` belonged to #4 in this wave, so the geometry comes from
`CellMetrics`: the thickness is the cell height / 16, rounded (1 px at
1x, 2 px at 2x for Menlo 13 pt); the underline is centred in the space
below the baseline, at least a pixel under it; the double underline is two
bars one thickness apart, centred the same way; the strikethrough is
centred 30% of the baseline height above the baseline, about half a
monospaced face's x-height. Every bar stays inside its row. Using the
face's own metrics is a later refinement that needs a `font.rs` accessor.
#22 adds the curly, dotted and dashed underlines and the overline from
the same geometry (see the amendment).

### Atlas-full policy: one reset per frame

When `get_or_insert` returns `AtlasFull` during a frame, the renderer
calls `GlyphAtlas::recover_from_full`: it empties both layers, starts a
new generation, re-inserts the glyphs already used in this frame tallest
first, and the renderer rebuilds the frame's instances (every lookup is
then a hit, and no slot from before the reset survives). This is allowed
once per frame: a second `AtlasFull` in the same frame is returned to the
caller, and the frame is not drawn. `AtlasStats::full_resets` counts the
resets, so tests can prove there is at most one per frame and that a
frame which cannot fit at all ends in an error, not a loop.

Investigating the fragmentation also found its main cause: a shelf is
opened at the glyph's height rounded up to 4 texels, but an occupied
shelf only accepted glyphs at least 2/3 of its height measured against
the *unrounded* height, so a 5-texel glyph opened an 8-texel shelf that
the next 5-texel glyph refused. Each such glyph took a shelf of its own.
The packer now compares against the rounded height, so a shelf always
takes more glyphs of the height it was opened for. This amends the
"at most 1.5× its height" rule in ADR 0007 to "at most 1.5× its rounded
height". Resets became rare after the fix: 7 in the 120-frame churn test
(18 glyphs per frame on a 64×64 layer, Menlo 13 pt at 1x, macOS 26.6),
and every one of those frames renders the same pixels as a renderer whose
atlas never fills. The re-insertion order breaks ties on the glyph's old
position, so the count does not depend on hash-map iteration order.

Growing the atlas (a second page or a larger texture) remains the real
fix for frames that need more glyphs than one layer holds, as ADR 0007
says.

### Golden tests with a font tolerance and an exact twin

`tests/golden.rs` renders headless into an `Rgba8UnormSrgb`
`OffscreenTarget` at 2x with Menlo 13 pt (installed on every macOS,
including the CI runner) and compares with PNGs in `tests/golden/`:

- `screen.png` (every ANSI foreground on every ANSI background, cube
  samples, every attribute, 24-bit color, box drawing, a block cursor) and
  `cursors.png` (three shapes, focused and unfocused) use a **localised
  font tolerance**: in every 16×16-pixel tile at most **4 pixels**, and in
  the whole image at most 0.5% of pixels, may differ by more than 16/255
  in any channel. That absorbs anti-aliasing noise along glyph edges
  between macOS versions. A whole-image budget alone was not enough: 0.5%
  is about a thousand pixels, more than any one attribute's ink, so a
  lost block-cursor redraw (about 120 px), drawn hidden text, or stripped
  bold, italic, dim or inverse all passed it. Any of these now puts
  dozens of differing pixels into one tile.
- `geometry.png` is the same content with every character replaced by a
  space (backgrounds, decorations, cursors only) and must match **exactly**,
  so a color or geometry bug cannot hide behind the font tolerance.

Direct pixel tests back the goldens up: hidden cells render exactly like
blank cells with the same background; the block cursor's cell holds
background-colored ink and nothing brighter than the cursor; bold,
italic, dim and inverse each change more than 100 pixels of a word, dim
darkens and inverse lightens the cell; a ligature never crosses a hidden
cell (when Cascadia Code, JetBrains Mono or Fira Code is installed), and
a hidden base never takes a visible combining mark with it (Menlo, so
this runs on CI where none of those fonts exist). Each of the mutations
above, plus removing the hidden blanking (caught on every machine by the
Menlo combining-mark test), ignoring the cursor's alpha in the redraw and
dropping the instance limit, makes at least one test fail.

`TESSERA_UPDATE_GOLDENS=1` rewrites the goldens; `TESSERA_SKIP_GPU_TESTS=1`
skips them like the other GPU suites. The images are about 230 KB in
total. If CI ever disagrees beyond the tolerance, the accepted fallback is
a per-platform golden set for the font goldens, recorded here; the exact
golden must agree everywhere.

## Consequences

- `bytemuck` (already in the graph through `wgpu`) is a direct dependency
  for the `Pod` instance; `png` is a dev-dependency for the goldens.
- The renderer owns its `Shaper` and atlas: one `Renderer` per terminal
  view. Its backing scale (padding, strokes) is the shaper's, so the
  frontend keeps it in step with the surface through
  `Renderer::set_font_size`. `Theme`'s font fields are what the frontend
  builds the shaper from; `Renderer::set_theme` does not re-read them.
- `RenderSurface::render` and `OffscreenTarget::render` share
  `Renderer::render`, which draws into any texture view. FFI wiring
  (`tessera_surface_render`) is #25.
- Every frame reshapes every visible row and rebuilds the whole instance
  buffer. That is the baseline #21's damage tracking improves on; because
  rows are reshaped each frame, the `.notdef` re-shape note in ADR 0007
  (rows drawn before the fallback index is ready) is satisfied for now and
  becomes #21's concern once shaped rows are cached.
- A glyph whose bitmap is larger than an atlas layer is skipped (logged at
  debug level) rather than failing the frame.
- Selection and overlays were accepted but not drawn until #22, which
  draws them in the overlay pass.
- One block-cursor edge case remains: a cell whose background equals the
  cursor color hides the redrawn glyph (the cursor needs a contrast rule
  or a configurable cursor-text color). The other one #20 left, underlines
  and other decorations covered by the block, is fixed by #22: they are
  redrawn in the cursor's text color.

## Amendment: #22 (2026-10-29)

### The overlay pass

A fifth pass, **overlay**, sits between background and glyph. Its quads
are alpha-blended (premultiplied, like everything else) over the cell
backgrounds, and the glyphs are drawn on top in their own colors. So a
selection is a highlight, not inverse video (kitty with
`selection_foreground none`, Alacritty's default), and shaping never sees
it: selecting part of a ligature cannot split it. That closes the ADR 0016
open item about a mark-carrying last character shrinking a ligature's
span: selection does not consume spans at all, and the block cursor
selects what to redraw by ink as well as by span (below). `PassRanges`
has five sections; the instance buffer has four per-row sections
(background, overlay, glyph, decoration) and the cursor slot.

### Selection and overlay semantics

```rust
pub struct Selection { pub start: Point, pub end: Point, pub kind: SelectionKind }
pub enum SelectionKind { Linear, Block }
pub struct Overlay { pub kind: OverlayKind, pub row: u16, pub cols: Range<u16> }
#[non_exhaustive] pub enum OverlayKind { Match, CurrentMatch, Underline }
```

- **Linear**: from the earlier point's cell to the end of its row, every
  row in between whole (over all grid columns, also past a short row's
  last cell; never into the padding), and the last row up to the later
  point's cell, inclusive. The grid's columns are the frame's longest
  row, at most the visible columns (#124). Wrapped rows are just
  consecutive rows; the renderer needs no wrap flags. **Block**: columns
  `min..=max` on rows `min..=max`.
- A span (selection or overlay) that starts on the spacer of a wide
  character, or ends on its wide half, is widened to the whole character.
  Anything outside the frame (past the grid's last column) is clipped;
  nothing panics on `u16::MAX`.
- `Match` and `CurrentMatch` fill their cells with `Theme::search_match`
  (`#ffd866`, alpha 0.6) and `Theme::search_match_current` (`#ff9f43`,
  alpha 0.8); theme files may set both. `Underline` is a bar one point
  high (one pixel per unit of backing scale) at the underline position, in
  each covered cell's foreground (a hovered link, #30).
- Overlapping overlays of one kind draw their **union**: a cell under
  three overlapping matches is highlighted once, as under one, so a
  highlight's opacity never depends on how many matches overlap (search
  for `aa` in `aaaa`), and the current match is never darkened by a
  second match on the same cells. Different kinds stack in a fixed order,
  bottom to top, whatever the slice order: selection, matches, the current
  match, link underlines. The current match is thus always on top of the
  matches, and both on top of the selection. This is a stronger contract
  than the issue's "in slice order", and the search bar (#46) relies on
  it: it may pass matches in any order and overlapping, and must not
  expect a later overlay to cover an earlier one of the same kind.
  Drawing the selection under the matches is a choice to revisit with
  #46: Alacritty gives the selection priority over search highlights.
- The union also bounds the work by the screen rather than by the caller.
  After it, each kind has at most one span per two visible columns; a
  filled kind is one quad per span and the link underline one bar per run
  of cells with one foreground, so a row emits at most about twice its
  width of overlay instances. The overlay pass also stops at four per
  visible column as a guard. Measured in a release build on a 60x200
  screen with a full-screen selection that changes every frame:

  | overlays | per frame | peak memory |
  |---|---|---|
  | none | 1.1 ms | 32 MB |
  | 800 full-width per row (48 000), over alternating foregrounds | 2.6 ms | 36 MB |
  | 5 000 full-width per row (300 000) | 5.6 ms | 41 MB |
  | 1 000 000 five cells wide | 17 ms | 57 MB |

  The first version capped spans per row instead, which did not bound
  instances: full-width link underlines over alternating foregrounds gave
  one bar per cell per span, `FrameTooLarge` at 800 per row (review of
  #22). The remaining cost of a flood is reading the slice, which the
  caller produced.

### The renderer diffs highlight coverage per row

`FrameInput::damage` stays a statement about grid content. The renderer
keeps each row's coverage (its list of spans) from the previous frame and
compares it with the new one; a row whose coverage changed rebuilds and
uploads only its overlay slot, without shaping or rebuilding its other
passes (`RenderStats::overlay_rows_built` counts these). A damaged row
rebuilds its overlay with the rest, so a link underline follows its
cells' new colors. When neither the previous frame nor this one has a
selection or overlay, the comparison is skipped entirely, so frames
without highlights cost nothing extra per row. Making app-core track
overlay damage was rejected: it would duplicate the state, and #30
(selection) and #46 (search) would each have to get it right.

### Underline styles, overline, superscript and subscript

- `Style::underline_style()` picks single, double, curly, dotted or
  dashed. Curly is a sine of period one cell width and amplitude
  `max(1 px, thickness)` (less only when the cell is too short), in a band
  centred on the single underline; dotted is thickness-sized dots at twice
  the thickness; dashed is dashes a third of a cell long with a sixth of
  a cell between them; double is two bars one thickness apart. Curly,
  dotted and dashed are one quad per run, drawn by the shader (`kind` 4
  and 3), so they cost no more instances than a single underline. Their
  phase is counted from the grid's left edge, not from each run, so dots
  and dashes keep their pitch across runs of different colors and under
  the cursor also when the cell width is odd. Unit
  tests pin every quad at 1x and 2x and keep all of them inside the cell
  at `MIN_FONT_SIZE_PT` and 1x.
- Underlines use the underline color (SGR 58), else the foreground; DIM
  dims it, INVERSE does not swap it. The overline (SGR 53) is a bar of
  underline thickness at the top of the cell in the same color. The
  strikethrough stays in the foreground. A run is keyed by the decoration
  flags and both colors.
- Superscript and subscript reuse the glyph's atlas bitmap at 0.7 of its
  size, scaled about the horizontal centre of the glyph's cells and about
  the baseline, then moved up by 0.3 of the cell height (superscript) or
  down by 0.2 (subscript, so a glyph sitting on the baseline ends on
  baseline + 0.2 cell), and clipped to their row so a lowered descender
  or a raised accent never paints into a neighbouring row (which would
  not redraw it away unless damaged itself). Nothing is rasterised again;
  the shader filters the bitmap bilinearly itself: four `textureLoad`s of
  the texels around the position inside the glyph's own bitmap, clamped
  to its edge texels so atlas neighbours never bleed in, weighted by that
  position alone. So the same glyph draws the same pixels in any atlas
  slot. A hardware sampler did not: its fixed-point filter weights come
  from the absolute atlas coordinate, so the same glyph differed by one
  step between slots and an incremental frame could differ from a fresh
  one after an eviction (review of #22, found by a differential fuzz).
  With both flags set, superscript wins (mintty).

### Cursor redraw rules

- A block cursor on either half of a wide character covers both columns
  and redraws the whole glyph.
- What it redraws: every glyph of the row whose cells include a cursor
  cell (the whole ligature under a cursor on its second cell), plus every
  glyph near the cursor whose ink reaches into it (a ligature whose last
  character carries a mark and so owns its own cell, an italic overhang).
  All of it is clipped to the cursor's cells.
- The glyphs and the cell's underlines, overline and strikethrough are
  redrawn in the cursor's text color: the cell's background after
  INVERSE (so an inverse cell's underline shows), dimmed when the cell is
  DIM (as its foreground would be), scaled by the cursor's alpha. A
  hovered link's underline (an overlay) under the cursor is redrawn the
  same way. Colour glyphs (emoji) keep their colors.
- A wide character in the last visible column, whose right half is past
  the target, is shaped as a blank and not drawn (it would draw into the
  padding); its background still is.

### Known limits

- ~~Highlights reach the right edge of the window, not the left.~~ and
  ~~A cursor on a cut wide character's spacer draws nothing.~~ Both
  resolved by #124, see "Amendment: #124".

### Measurements

`cargo xtask ab` against the merge base (`bfea6f9`), instructions
retired, best of 15 interleaved process pairs, Apple M4, macOS 26.6,
load average 2 to 7 (re-measured after review round 1). ADR 0016's
synced method: each process waits for the GPU after every frame and
draws full frames (`DamageRows::All`) of the corpus line, Menlo 13 pt at
2x; the per-frame cost is the difference between two frame counts (50
and 150 at 60x200, 100 and 400 at 24x80), which removes everything a
process pays once. "Full selection" draws a selection over the whole
screen on head (in a transparent selection colour, so both sides draw
the same pixels) against main, which takes the selection as data only.

| line | 60x200: main per frame | head/main | 24x80: main per frame | head/main |
|---|---|---|---|---|
| ascii | 87.74e6 | 1.002 | 14.56e6 | 1.002 |
| latin_diacritics | 91.38e6 | 1.003 | 15.12e6 | 1.001 |
| cjk | 249.37e6 | 1.000 | 40.63e6 | 1.000 |
| box_drawing | 11.10e6 | 1.011 | 2.28e6 | 1.009–1.026 |
| hebrew | 112.60e6 | 1.001 | 18.38e6 | 1.002 |
| arabic | 190.79e6 | 1.001 | 30.51e6 | 1.000 |
| punctuation | 86.60e6 | 1.001 | 14.65e6 | 1.001 |
| braille | 79.95e6 | 1.003 | 13.41e6 | 1.000 |
| emoji | 146.93e6 | 1.000 | 23.46e6 | 1.001 |
| sgr_heavy | 92.01e6 | 1.007 | 15.22e6 | 1.007 |
| combining_heavy | 210.45e6 | 1.002 | 34.42e6 | 1.002 |
| ascii, full selection | 87.75e6 | 1.002 | 14.56e6 | 1.003 |
| cjk, full selection | 249.40e6 | 1.000 | 40.63e6 | 1.001 |

The bars (1.03 without highlights, 1.25 with a full-screen selection)
hold on every line. `box_drawing` at 24x80, the cheapest frame per cell,
depends most on the method: 1.009 with 100 and 400 frames here, and
1.012 to 1.026 in the review's own runs with other frame counts.
`sgr_heavy`'s pixels differ by design: main drew its curly underline as a
straight bar. The first version cost 1.05x on
`box_drawing`, whose frames are cheap per cell, about 55 instructions per
cell on every line: the decoration pass ran its run bookkeeping over every
cell of rows without any decoration. It now skips such rows after one
scan. Review round 1 briefly brought it back to 1.04: splitting `prepare` in
two (to hand a reused vector back) changed what the compiler inlined; the
reused vector now lives in the renderer instead.

## Amendment: #124 (2026-11-03)

### Highlights stop at the grid's last column

`Layout::visible_cols` counts every column that starts inside the
target, including one that starts in the right padding or in the pixels
left over after the last whole column. Until #124 a selection's inner
rows, a block selection and an overlay to `u16::MAX` covered all visible
columns, so they painted over the right margin out to the window's edge
while the left margin stayed the background. Now that selection ships
(#30) that asymmetry is visible, so highlights stop at the **grid's**
last column instead: the overlay pass clips every span to the frame's
longest row (at most the visible columns). The frame carries no grid
width, and app-core hands every row at the grid's width, so the longest
row is the grid width; a short row in an otherwise full frame is still
highlighted to the grid's edge. Both paddings, and the leftover pixels,
stay the theme background. This is what iTerm2 does on the primary screen
(selection per cell, margins in the profile background; #30 research).
Cell backgrounds (SGR) are unchanged: they only ever covered cells.

`selection_linear.png` changed in 480 pixels: x = 392 to 399 on rows 0
and 1 (y = 8 to 67), the right padding beside the first row's tail and
the wrapped middle row, which were the selection colour and are now the
background. No other golden changed.
`highlights_stop_at_the_grids_last_column` (golden.rs) checks every pixel
of a row across the target for linear and block selections and both match
kinds, with padding and 5 leftover pixels.

### A cursor on a cut wide character's spacer

When a wide character starts in the last visible column and its spacer's
column is past the target (a transient during a resize), a cursor
reported on the spacer now draws the visible half, exactly like a cursor
on the wide half (one column); a cursor further right still draws
nothing.

### A test accessor for the stroke

`Renderer::cursor_stroke_px` (`#[doc(hidden)]`, not a stable API) returns
the stroke the cursor pass draws with (one pixel per point of backing
scale, at least one), so app-core's cursor pixel test reads it instead of
measuring it (#168 item 6).

### Measurements (#124)

`cargo xtask ab` against main (`e7004b4`), instructions retired, best of
7 interleaved process pairs, Apple M4, load average 7 to 45. Full frames
(`DamageRows::All`, GPU waited for after each) of a 60x200 screen filled
with the corpus line, Menlo 13 pt at 2x, in a target with the padding and
5 leftover pixels, so one more column starts in the right margin; per
frame is the difference of 30 and 90 frames (100 and 300 for
`box_drawing`). "Full selection" adds a linear selection over the whole
screen, which main draws into the margin and head does not.

| line | main per frame | head/main | full selection: main per frame | head/main |
|---|---|---|---|---|
| ascii | 89.06e6 | 1.000 | 89.07e6 | 1.002 |
| latin_diacritics | 88.22e6 | 0.999 | 87.90e6 | 1.002 |
| cjk | 244.18e6 | 1.000 | 242.74e6 | 1.002 |
| box_drawing | 12.39e6 | 0.981 | 12.12e6 | 0.999 |
| hebrew | 87.00e6 | 1.001 | 86.87e6 | 1.001 |
| arabic | 163.80e6 | 1.000 | 163.71e6 | 0.999 |
| punctuation | 66.15e6 | 1.001 | 66.21e6 | 1.001 |
| braille | 68.77e6 | 1.000 | 68.74e6 | 1.000 |
| emoji | 175.95e6 | 1.003 | 175.91e6 | 1.003 |
| sgr_heavy | 74.52e6 | 1.002 | 74.49e6 | 1.001 |
| combining_heavy | 132.11e6 | 1.000 | 131.85e6 | 1.001 |

The bar (1.02) holds on every line. The change costs one pass over the
rows' lengths per frame, and only while something is highlighted.
`box_drawing`'s 100-frame runs are below `ab bench`'s size floor, so its
per-frame ratio is the noisiest; its 300-frame whole-process ratios are
0.990 and 0.999.


## Amendment: #246 and #247 (2026-11-05)

### The highlight width rule in the public docs (#246)

`Selection`, `Overlay` and `FrameInput::rows` now say what the #124
amendment decided: highlights stop at the frame's longest *visible* row
(rows past the target's bottom do not count), so producers pass every row
at the grid's width. `rows_past_the_bottom_do_not_widen_highlights`
(overlay.rs) pins that the overlay pass measures only the visible rows; it
fails when `grid_cols` is handed all of `FrameInput::rows`.

### The cursor's draw covers its instances, not its slot (#247)

The cursor pass's draw range is the cursor's instances, no longer its
whole slot. The slot is roomy (at least 16 instances, the rest `EMPTY`),
so until #247 every frame drew it even with no cursor, and a frame with
nothing else to draw still issued one draw of zero-sized quads. A fresh
renderer, whose first layout is empty, issued none for the same frame.

That mattered for one reason, found by the #124 adversarial run: on
Apple's Metal GPU (M4, macOS 27.2), a render pass that draws anything, even
degenerate quads, stores a translucent sRGB clear colour differently from
a pass that only clears. With the background `(0.816, 0.059, 0.933)` at
alpha 0.42 the premultiplied green, 0.0247 linear, encodes to 43.55: 44
from a clear-only pass, 43 from a pass with a draw. So a view whose rows
went from some to none (a target smaller than its padding, after a scale
change from 3 to 1.75) differed from a fresh view by one step of green in
every pixel. The clear value itself is computed once
(`Rgba::to_clear_color`) and was identical on both paths.

Both paths now issue the same passes for the same frame: a frame with no
visible row and no cursor draws nothing but the clear, and the
fresh-versus-incremental comparisons stay bit-exact, with no tolerance.
The draw loop skips empty ranges itself, so nothing depends on how a
backend treats a zero-instance draw. On Metal such a draw changes no
pixel, so no image comparison can pin the skip:
`the_draw_loop_skips_empty_ranges` (renderer.rs, #265) counts the draw
calls a frame issues, through a counter compiled into test builds only,
and fails without the skip (a hidden cursor's frame issues five draws, not
four; a frame without visible rows five, not none). Frames that draw
anything are unchanged: all ten goldens render byte-identical to main's.
`a_frame_without_rows_after_a_scale_change_matches_a_fresh_renderer`
(damage.rs) fails on main; `the_cursor_draw_covers_only_its_instances`
(renderer.rs) pins that a shown cursor is drawn, a hidden one issues no
draw although its slot stays, and a frame without visible rows draws no
pass.

The clear-only versus drawing pass difference itself remains a property
of the GPU: in a window, a frame without visible rows (only possible
while the target is smaller than the padding) may show its translucent
background one step off the padding of a frame with rows. Invisible, and
not worth an extra draw per frame.

### Measurements (#247)

`cargo xtask ab` against main (`75a122e`), instructions retired, best of
7 interleaved process pairs, Apple M4, load average 6 to 18; #124's
`frame_build` cases (full frames of a 60x200 screen of the corpus line,
Menlo 13 pt at 2x, GPU waited for after every frame), the per-frame cost
the difference of 30 and 90 frames (100 and 300 for `box_drawing`). The
checksums include the last frame's pixels, equal on both sides.

| line | main per frame | head/main | full selection: main per frame | head/main |
|---|---|---|---|---|
| ascii | 89.42e6 | 0.995 | 89.20e6 | 1.000 |
| latin_diacritics | 88.06e6 | 0.998 | 88.30e6 | 1.000 |
| cjk | 243.09e6 | 1.000 | 242.87e6 | 1.000 |
| box_drawing | 12.13e6 | 0.997 | 12.13e6 | 1.000 |
| hebrew | 87.09e6 | 0.997 | 86.84e6 | 1.001 |
| arabic | 163.69e6 | 1.001 | 163.90e6 | 0.999 |
| punctuation | 66.18e6 | 1.002 | 66.22e6 | 1.001 |
| braille | 68.77e6 | 1.000 | 68.77e6 | 0.998 |
| emoji | 175.98e6 | 1.000 | 176.79e6 | 0.997 |
| sgr_heavy | 74.50e6 | 0.999 | 74.46e6 | 0.996 |
| combining_heavy | 131.99e6 | 0.999 | 132.05e6 | 0.999 |

Every line is within noise of 1.00. Full selection on `punctuation` first
read 1.023 from one inflated base run; 15 pairs gave 1.001.

## Amendment: #45b (2026-11-08)

### The exit-status gutter

`FrameInput::gutter` (`with_gutter`) is a slice of `GutterMark { row,
status }`, `status` a `CommandStatus`: `Success`, `Failure` or `Unknown`
(`CommandStatus::of` maps exit status 0, any other status, and none). The
producer (the app core's snapshot, ADR 0039) finds the rows; the renderer
only draws them, so a frame without marks costs one empty check.

**Geometry.** One solid bar per mark in the left padding: as tall as its
row, half the padding wide (at least one pixel), centred in it, clipped
to the padding column and the target, so a mark never covers a cell. No
padding, no marks. At most one bar per row that starts inside the target,
whatever the slice holds: marks are read in row order and a repeated,
out-of-order or off-screen row draws nothing, so a producer bug or a
flood costs at most a screenful of quads.

**Colours.** `Theme::mark_color(status)`: `mark_success` and
`mark_failure` (from the theme file, ADR 0019) or the palette's `ansi[2]`
and `ansi[1]`; `ansi[8]` for an unknown status. Always opaque, and from
the theme's own palette, not a program's palette overrides: output does
not recolour the marks.

**Draw and damage.** The bars are pushed into the cursor's slot after the
cursor's instances and drawn as a sixth range (`PassRanges::gutter`),
after the cursor, with the same pipeline; an empty range is skipped like
any other (#247), so a frame without marks issues the same draws as
before. The slot is rebuilt every frame, so the marks take no part in the
per-row damage; when a mark on every row outgrows the slot, the next
frame lays the buffer out again once (`Slots::fits`). The producer tells
the view when the marks changed without any row in view changing (a `D`
below the view's last row), so a frame is presented for it.

**Tests.** `passes/gutter.rs` pins the geometry, the clip, the
one-per-row rule and the zero padding; `renderer.rs` the sixth range and
its draw counts; `tests/overlay.rs` the pixels of a success and a failure
bar beside text that keeps its own.

### Measurements (#45b)

`cargo xtask ab` against the merge base (ae6a701), 15 interleaved pairs at
load 5-13, head / base. A case renders 40 full frames of a 60x200 screen of
one wave-13 corpus line (box drawing 120, braille 80, to clear the probe's
size floor), Menlo 13 pt at 2x, and waits for the GPU after each frame.
`none`: no gutter on either side, pixels compared. `every row`: head draws a
mark on each of the 60 rows, base none. Bars 1.02 (none) and 1.05 (every
row); all pass.

| line | none: instructions | none: cycles | every row: instructions | every row: cycles |
|---|---|---|---|---|
| ascii | 1.001 | 1.006 | 1.002 | 1.002 |
| latin_diacritics | 1.000 | 0.996 | 1.001 | 1.000 |
| cjk | 0.999 | 0.998 | 1.000 | 1.000 |
| box_drawing | 1.000 | 0.996 | 1.000 | 1.002 |
| hebrew | 1.001 | 1.001 | 1.001 | 1.007 |
| arabic | 1.000 | 0.998 | 1.000 | 1.003 |
| punctuation | 1.001 | 0.995 | 1.000 | 0.997 |
| braille | 1.000 | 1.001 | 0.999 | 0.997 |
| emoji | 1.000 | 1.010 | 1.000 | 1.005 |
| sgr_heavy | 1.003 | 1.012 | 1.000 | 1.005 |
| combining_heavy | 1.001 | 1.013 | 1.001 | 0.996 |

