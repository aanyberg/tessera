# ADR 0007: cosmic-text shaping, a dual R8 + RGBA8 glyph atlas, procedural box drawing

**Status:** accepted · 2026-10-27

## Context

`tessera-render` must turn a row of grid cells into positioned glyphs and
keep their bitmaps on the GPU (issue #19); the passes in #20 draw from
that. The requirements are a terminal's, not a text layout engine's:

- Every glyph is anchored to a cell. Cells are whole device pixels wide
  and tall, so a font whose advance is 7.83 px must not drift by a pixel
  every eight columns.
- Programming ligatures (`->`, `==` in JetBrains Mono, Fira Code) and
  contextual alternates must apply within a run of same-styled text.
- A character the primary font lacks is drawn from a fallback chain:
  configured families, then system CJK and symbol faces, then colour emoji.
- Bold and italic use the family's own faces when it has them and are
  synthesised otherwise.
- Box-drawing characters in adjacent cells must join without seams.
- The crate stays `#![forbid(unsafe_code)]`, depends only on `tessera-grid`
  internally, and its dependencies are under the licences `deny.toml`
  allows (MIT, Apache, BSD, ISC, Zlib, Unicode, MPL). One advisory is
  accepted as an exception; see "Unmaintained ttf-parser" below.

## Decision

### Shaping and font discovery: cosmic-text

`cosmic-text` 0.19 (MIT OR Apache-2.0) is the font stack, with default
features off and `std` + `swash` on, so `fontconfig` is not compiled in.
It brings `fontdb` (system font discovery: on macOS it scans the system,
library and user font directories directly), `harfrust` (a pure-Rust
HarfBuzz port) and `swash` (rasterisation, colour bitmaps and COLR).

- *HarfBuzz via C bindings* would need a C toolchain in CI and `unsafe`
  outside the two crates that may have it.
- *Core Text* would need `objc2`/`core-foundation` bindings, ties the core
  to macOS (ADR 0001 keeps it portable), and gives less control over
  cluster mapping than HarfBuzz-style output.
- *cosmic-text's own layout* (`Buffer`, `ShapeLine`) does line breaking,
  bidi and word splitting we do not need, decides fallback per word, and
  only offers quarter-pixel bins without synthetic bold. We use its
  `FontSystem` and `Font` (the database, face cache and per-face `harfrust`
  shaper) and drive `harfrust` and `swash` directly. `swash` is listed as a
  direct dependency at the version cosmic-text already pulls in, which adds
  no crate to the graph.

`Fonts` chooses a face per character before shaping: the primary
family (the configured one, else Menlo, else any monospaced face), then
the configured fallbacks, then the system list (`SYSTEM_FALLBACK`: Apple
Symbols, Hiragino Sans, PingFang SC, Hiragino Sans GB, Apple SD Gothic Neo,
Arial Unicode MS), with Apple Color Emoji tried *before* the system list
for characters with default emoji presentation and *after* it otherwise.
A character none of them covers is looked up in a coverage index of every
installed face except LastResort. The index is built once, on a
background thread started by `Fonts::new` from a clone of the font
database, by walking each face's character map in database order
(about 20 ms in a release build, 120 ms in debug, on a stock macOS
install) and kept as sorted code-point ranges, so misses are a binary
search. The index records the first covering face, often a Bold cut, so
the style is drawn from that face's family: the face for the requested
style if it covers the character, else (for italic) the upright face at
the requested weight, else the Regular face, each with the synthesis it
needs, and only then the recorded face itself. The render path never
builds it and never holds the `Fonts` lock while it is built; until it
is ready, a character the chain misses draws the primary face's
`.notdef` and is not cached, so it resolves properly
once the index appears. `Fonts::fallback_index_ready()` turns `true`
(once, for good) when it does; a renderer that caches shaped rows (#20)
must watch it and re-shape the rows that held `.notdef` glyphs (glyph
id 0), or they keep the tofu until something else redraws them.
Terminal output is untrusted, and a per-character scan of every face
cost about 11 ms per character. Per-character results are cached, and
the cache is emptied when it reaches 65,536 entries. Runs are then
maximal sequences of non-blank cells with the same face, the same
bold/italic and no wide character; each run is shaped left to right on
its own. Spaces end runs: no ligature spans one, and blank cells emit no
glyph.

Each cluster is placed relative to its first cell and centred in the
cells it spans (two columns per cell for East Asian wide characters),
which is what keeps text on the grid. Fonts implement programming
ligatures as contextual alternates in one of three ways:

- **Trailing drawer** (JetBrains Mono, Fira Code): an empty spacer glyph
  per leading character, each in its own cluster, then one glyph whose
  ink reaches back (left) over the spacers.
- **Leading drawer** (Cascadia Code): one glyph that draws the whole
  ligature and whose ink reaches forward (right), followed by an empty
  spacer glyph per remaining character (`<!--` is one drawing glyph and
  three spacers). The ink may barely enter the last spacer: the
  compressed `...` reaches only about a fifth of a cell into its third
  cell at 13 pt.
- **Merged cluster** (Monaspace): the shaper merges the characters into
  one cluster holding the empty spacers and the drawing glyph.

The output is built as a **partition of the run's cells**, so the glyph
spans are in order, never overlap, and cover every cell that a drawing
cluster or its assigned spacers occupy exactly once, by construction
rather than by a distance threshold:

1. Each shaper cluster is classified. A *drawing* cluster has a glyph
   with ink; it owns all of its cells, so the spacers of a merged
   cluster belong to the glyph inside it and an overhanging neighbour
   (an italic `W`) can never absorb it. A *spacer* cluster has only
   empty glyphs although its characters' own (character-map) glyphs
   draw: the shaper substituted them away. A *blank* cluster draws
   nothing even unshaped (no-break space, zero-width characters) and
   owns nothing. An *ignorable* cluster has only empty glyphs and only
   characters the shaper hides (harfrust's default-ignorable set: soft
   hyphen, ZWSP, BOM, variation selectors); it owns nothing whatever its
   nominal glyph draws. harfrust deliberately leaves U+115F, U+1160,
   U+180F, U+3164, U+FFA0 and U+1BCA0..1BCA3 visible although Unicode
   marks them default-ignorable, so they are ordinary characters here.
2. Each maximal sequence of spacer clusters (with any ignorable
   clusters inside or around it, which take part in the split but are
   never assigned) is split once between the nearest non-ignorable
   drawing clusters before and after it, since the shaper also looks
   through ignorables (`-`, ZWSP, `>` is still an arrow): the left part goes to
   the previous one, the rest to the next, so every owner's cells stay
   contiguous. A drawing cluster counts as a *ligature* when it spans
   several cells or its glyph is not the character's nominal glyph; a
   plain letter never takes spacers from a ligature neighbour. With one
   candidate side, all spacers go to it. With two, the split maximises
   the ink each side lays over the cells it receives; ties go to the
   side whose ink covers more of the sequence, and if neither side's
   ink reaches it at all, to the next cluster (spacers before the
   drawing glyph are the common convention).
3. The drawing glyphs are emitted on their owner's cell range. Several
   glyphs drawing one range (a base and its marks) are consecutive and
   repeat the same `cell_index` and `cell_span`.

A `cell_span` is the glyph's logical extent, used for cursor and
selection; it is not the ink bounds. Italic overhang, Cascadia's `...`
and wide characters draw beyond it, so damage tracking must pad the
range or use the atlas bitmap's bounds. Cells holding only hidden
ignorables (U+200B, U+00AD) are never owned on their own: they are
covered when they fall inside a ligature's range or merge into a base's
grapheme cluster (U+FE0F, U+200D, U+17B4 under `MonotoneGraphemes`), and
otherwise belong to no glyph, as do blank cells and spacers with no
drawing neighbour. A
span above 1 is not always a ligature: a base and a combining mark in
their own cells form one cluster over both.

So the renderer sees `->` as one two-cell glyph and `...` as one
three-cell glyph whatever technique the font uses. A glyph in its own
cluster keeps its cell even when it is part of the ligature's artwork
(Cascadia's bold `<<=` reports `<<` over two cells and `=` over one),
which still partitions the cells. Debug builds assert the partition on
every `shape_line`; a test sweeps 21 operators in 9 contexts across 4
styles, 8–36 pt and 1x, 1.5x and 2x in every installed ligature face
(Cascadia Code, JetBrains Maple Mono, Monaspace Argon, Neon, Radon,
Xenon and Krypton), with a reduced size set in debug builds.

### Subpixel positioning: thirds of a pixel

Horizontal glyph offsets are quantised to 1/3 px. The whole-pixel part
positions the bitmap and the fraction (0, 1/3 or 2/3) is part of the
`GlyphKey`, so a glyph has at most three bitmaps per size. Three phases
remove visible jitter in centred glyphs at Retina sizes. Quarters (what
cosmic-text uses) add a third more atlas entries for no visible gain.
Vertical offsets are whole pixels so baselines stay crisp. Colour and
builtin glyphs always use phase 0.

### Atlas: two layers, shelf packing, generation LRU

`GlyphAtlas` owns two square textures of the same size (1024² by
default):

- `R8Unorm` holds coverage for outline glyphs, synthetic styles and builtin
  box drawing. One byte per texel is a quarter of the memory of RGBA, and
  the glyph pass tints it with the cell colour.
- `Rgba8UnormSrgb` holds colour glyphs (emoji) with premultiplied
  alpha. Sampling through an sRGB view hands the shader linear colour,
  matching the sRGB swapchain configured by `RenderSurface`. Because the
  sampler decodes each channel on its own, sbix PNGs are premultiplied in
  *linear* light on upload: each stored byte is `encode(decode(c) · a)`,
  so a sample decodes to linear premultiplied colour and #20 blends it
  with `One, OneMinusSrcAlpha` and no tint. (A multiply on the encoded
  bytes would decode to `decode(c · a)`, darker at partial alpha.) COLR
  glyphs are composited by swash in encoded space and stored as they come;
  their edges keep that small error. Apple Color Emoji is sbix.
- The layer a glyph lands in is decided by what swash renders, not by the
  face: a colour face's glyph without colour data comes back as coverage
  and goes to `R8Unorm`. `AtlasSlot::layer` therefore picks the texture
  and whether to tint; `ShapedGlyph::is_color` is only a face-level hint.

Each layer is packed in shelves (rows whose heights are rounded up to 4
texels; a glyph only joins an occupied shelf at most 1.5× its height),
with a free list per shelf so evicted glyphs' space is reused. Adjacent
empty shelves merge and empty shelves at the top go back to open space,
so a layer emptied by eviction can take any glyph that fits the texture.
A glyph placed in an empty shelf taller than it needs splits the shelf:
the glyph's shelf keeps the rounded glyph height and the rest becomes a
new empty shelf below it, so merged space stays usable by glyphs of the
usual height.
Every glyph keeps one empty texel of padding on each side so linear
sampling never picks up a neighbour.

Eviction is least-recently-used at frame granularity: `begin_frame()`
increments a generation counter, every hit or insert stamps the entry with
it, and when a layer has no room the atlas sorts that layer's entries
from older generations once and evicts them a whole generation at a time,
oldest first, retrying after each, until the glyph fits.
Entries used in the current generation are never evicted; if the glyph
still does not fit, `get_or_insert` returns `RenderError::AtlasFull`
rather than corrupting a glyph the frame is about to draw. A per-frame
counter needs no linked list or extra crate, and one sort per evicting
insert keeps it `O(n log n)` however many generations it evicts.

Keys whose bitmap cannot fit are rejected before rasterising: a builtin
glyph whose cell exceeds the layer, or an outline whose em square is more
than twice the layer. `Shaper` also caps the backing scale at 8, so the
pixel size is at most 512 pt × 8. At the other end, `sanitize_scale`
raises a positive scale to at least 0.25 and `Shaper` keeps the em at
1 px or more: a pixel size that rounds to zero 64ths would make swash
return unscaled outlines (a `.` hundreds of pixels across). A bad scale over FFI is an error, not an
allocation of terabytes.

`Shaper::set_font_size` bumps an epoch that belongs to that shaper (each
`Shaper` holds its own `Fonts` handle with its own counter, sharing the
font database); every atlas built from `Shaper::fonts()` clears itself at
its next `begin_frame`, while atlases of other windows sharing the
database keep their glyphs. The clear never happens in the
middle of a frame, so slots handed out during a frame stay valid. Keys
also carry the pixel size and cell geometry, so a stale bitmap can never
be returned for a new size.

### Synthesis

Bold is real when the matched face has weight ≥ 600 or a `wght` axis that
reaches 600 (the axis is then set to 700); otherwise the outline is
emboldened by `size_px / 24` px (clamped to 0.33–2 px). Italic is real
when the matched face is italic or
oblique; otherwise the outline is skewed 14°, the angle Core Text and
cosmic-text use. The same rules, from one helper, apply to every fallback
face, including faces found through the coverage index, and the flags
travel with each glyph (`synthetic_bold`, `synthetic_italic`) and in its
atlas key. Colour faces (`sbix`, `COLR`, `CBDT`) are never synthesised
and are always drawn at their own weight: a bold or italic emoji has the
same face, flags and atlas key as a regular one, as in every reference
terminal, so it is rasterised and cached once.

### Box drawing and block elements are drawn, not rasterised

U+2500–U+259F are drawn procedurally into a bitmap of exactly one cell:
light lines are about one pixel per 8 px of cell width, heavy lines twice
that, double lines two light lines with a light gap. Arms run from the cell
edge through a junction sized to the thickest crossing arm. Double
junctions are a three-line band with the middle stripe erased, which gives
correct corners, tees and crosses for any mix of single and double arms.
Rounded corners and diagonals are anti-aliased; dashes, eighths, quadrants
and shades are rectangles. Font glyphs for these characters follow the
font's em box rather than the rounded cell, so they leave gaps or overlap
between rows. Kitty, WezTerm, Alacritty and Ghostty draw them themselves
for the same reason.

### Unmaintained ttf-parser (RUSTSEC-2026-0192)

`cosmic-text` 0.19 depends on `fontdb` 0.23, which depends on
`ttf-parser` 0.25.1. RustSec lists `ttf-parser` as unmaintained, with no
fixed release, and recommends `skrifa`. No released `cosmic-text` avoids
it, and the alternatives above are worse (C bindings or a macOS-only
core).

The milestone lead accepted this as an exception: the root `deny.toml`
lists RUSTSEC-2026-0192 under `[advisories] ignore`, with a reason that
points here. The advisory concerns maintenance, not a known
vulnerability, and the parser only reads installed system fonts, not
fonts from terminal output.

Revisit trigger: when `cosmic-text` (or `fontdb`) moves to
`skrifa`/`read-fonts`, upgrade and remove the `ignore` entry. Revisit
sooner if RustSec publishes a vulnerability against `ttf-parser`, since an
`ignore` keyed to this advisory id does not cover a new one.

## Consequences

- One `Fonts` per process is the intended shape: loading the database
  takes a noticeable fraction of a second in a debug build.
  `Shaper::with_fonts` lets windows share it; it is `Clone + Send + Sync`
  behind a mutex that is held per line shaped or per glyph rasterised.
  Each shaper gets its own invalidation counter, so its atlas must be
  built with `GlyphAtlas::for_shaper` (or from `Shaper::fonts()`); an
  atlas built from the shared handle is never cleared by that shaper's
  `set_font_size`.
- The grid does not model wide cells yet. Wide characters are shaped on
  their own and centred in two columns, but still occupy one `Cell` and no
  spacer follows them, so two adjacent wide characters (`中文`) currently
  draw overlapping glyphs. `Shaper::shape_line` assumes the next cell is a
  spacer. When the emulator adds spacer cells, `cell_span` stays 1 and only
  the run splitting needs to skip spacers.
- Fallback is per character, not per grapheme cluster. Combining sequences
  and ZWJ emoji need grapheme-aware cells in `tessera-grid` first.
- Wide-character detection and emoji presentation use a condensed table of
  East Asian Wide/Fullwidth ranges, not the full Unicode data.
- Atlas growth (a second page, or a larger texture) is not implemented; a
  frame that needs more distinct glyphs than one layer holds gets
  `AtlasFull` for the excess. At 1024² and 13 pt on Retina that is roughly
  1,500 distinct glyphs per frame, but the layer size is fixed and not
  derived from the cell size, so at very large zoom (around 100 pt on
  Retina, cells about 120×230 px) a layer holds only a few dozen glyphs
  and ordinary screens lose glyphs. Large font sizes are out of scope
  until atlas growth (or sizing the layer from `CellMetrics`) lands.
  A glyph whose bitmap is larger than a whole layer is remembered as
  rejected and not rasterised again until the next `clear`.
- Every glyph upload runs inside a validation error scope, which costs one
  scope pop per atlas miss. Misses are rare after the first frame.
