# Golden images

`tests/golden.rs` renders the grid passes headless and compares the result
with the PNGs in this directory. Everything is drawn at 2x with Menlo 13 pt
(16 × 30 px cells, 8 px padding) into an `Rgba8UnormSrgb` offscreen target
with `Theme::default()`; the PNGs are 8-bit RGBA, sRGB-encoded.

| File | Content | Tolerance |
| --- | --- | --- |
| `screen.png` | 40 × 12 cells: every ANSI foreground on every ANSI background, color-cube samples, bold / dim / italic, underline / double underline / strikethrough, inverse / hidden, 24-bit color, box drawing, a block cursor | font |
| `cursors.png` | block, beam and underline cursors over text, focused (first three strips) and unfocused (last three, all hollow) | font |
| `geometry.png` | the same screen and strips with every character replaced by a space: backgrounds, decorations and cursors only | exact |

**Font tolerance:** in every 16 × 16 pixel tile at most 4 pixels, and in
the whole image at most 0.5% of the pixels, may differ from the golden by
more than 16/255 in any channel. Glyph edges come from the system's Menlo
and the GPU's blending, which can shift slightly between macOS versions
(local machines vs the `macos-15` CI runner) without anything being
wrong; that noise is a few pixels spread along edges. A real change to
one glyph (a missing cursor redraw, hidden text drawn, bold or dim lost)
puts dozens of pixels into one tile and fails, even though it is far
below the whole-image budget. A failure names the worst tile's position.

**Exact:** `geometry.png` has no glyphs, only axis-aligned quads on whole
pixels in colors resolved on the CPU, so it must match byte for byte. A
color or geometry bug cannot hide behind the font tolerance there.

## Regenerating

After an intended change to what the passes draw:

```sh
TESSERA_UPDATE_GOLDENS=1 cargo nextest run -p tessera-render --test golden
```

This rewrites every golden instead of comparing. Look at the new images
(and the `git diff --stat` sizes) before committing them: a golden is only
as good as the review it got.

When a comparison fails, the test writes the rendered image to
`target/tmp/<name>.actual.png` (Cargo's `CARGO_TARGET_TMPDIR`) and prints
the path, so it can be compared with the golden side by side.

`TESSERA_SKIP_GPU_TESTS=1` skips the suite on machines without a GPU, as for
`headless_clear.rs`.

If CI disagrees with goldens produced locally beyond the font tolerance,
the fallback is a per-platform golden set, recorded in ADR 0010; the exact
`geometry.png` must still agree everywhere.
