# ADR 0010: Render passes, frame input and the atlas-full policy

**Status:** accepted · 2026-10-27

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
    pub selection: Option<Selection>,      // start, end (data only, #22 draws it)
    pub overlays: &'a [Overlay],           // uninhabited until #22
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
unfocused frame draws every shape as `HollowBlock`. The cursor spans one
column in this release; two-column cursors on wide cells come with #22,
once #4's `Cell::is_wide()` is in.

### Four passes, one instance buffer, one pipeline

Each frame clears the target to `Theme::background` and draws, in order:

1. **background**: one quad per horizontal run of cells whose background
   differs from the cleared target (an explicit background, or any cell
   with inverse video);
2. **glyph**: one quad per `ShapedGlyph`, at
   `cell origin + whole-pixel x offset + bitmap bearing`, tinted with the
   owning cell's foreground (coverage layer) or drawn as is (colour
   layer). `HIDDEN` cells draw no glyph and are shaped as blanks, so no
   ligature reaches across a hidden boundary (a hidden `>` must not turn
   a visible `=` into an arrow); bold and italic were already chosen by
   the shaper;
3. **decoration**: underline, double underline and strikethrough bars, one
   quad per bar per run of cells with the same decorations and color;
   hidden cells draw none;
4. **cursor**: block (the cursor color over the cell, then the glyphs
   covering the cell redrawn in the cell's background color, clipped to
   the cell), beam (a stroke at the left edge), underline (a stroke at the
   bottom), hollow block (four strokes). Strokes are one pixel per unit of
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
| 56 | `kind: u32` | 0 solid, 1 coverage glyph, 2 colour glyph |
| 60 | padding | |

One WGSL shader (`shaders/quad.wgsl`) expands a triangle strip per
instance and branches on `kind`. Glyphs sit on whole pixels and map one
texel to one pixel, so the atlas is read with `textureLoad` (exact, no
sampler, no filtering at glyph edges). The clip rectangle exists for the
block cursor's redrawn glyph, which must not spill out of the cell when it
is a ligature or a wide character; everything else is unclipped. A single
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
  `Theme::cursor.a` is the cursor's opacity; `Theme::selection.a` will be
  the selection's (#22). Only the default background carries the theme's
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
skips them like the other GPU suites. The images are about 130 KB in
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
- Selection and overlays are accepted but not drawn until #22.
- Two block-cursor edge cases are left for #22: a cell whose background
  equals the cursor color hides the redrawn glyph (the cursor needs a
  contrast rule or a configurable cursor-text color), and underlines and
  other decorations are covered by the block, not redrawn on top of it.
