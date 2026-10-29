# Golden images

`tests/golden.rs` renders the grid passes headless and compares the result
with the PNGs in this directory. Everything is drawn at 2x with Menlo 13 pt
(16 × 30 px cells, 8 px padding) into an `Rgba8UnormSrgb` offscreen target
with `Theme::default()`; the PNGs are 8-bit RGBA, sRGB-encoded.

| File | Content | Tolerance |
| --- | --- | --- |
| `screen.png` | 40 × 12 cells: every ANSI foreground on every ANSI background, color-cube samples, bold / dim / italic, underline / double underline / strikethrough, inverse / hidden, 24-bit color, box drawing, a block cursor | font |
| `cursors.png` | block, beam and underline cursors over text, focused (first three strips) and unfocused (last three, all hollow) | font |
| `clusters.png` | 24 × 1 cells: `e` + U+0301, a ZWJ family (two cells), a VS16 heart, a hand with a skin tone, `a` + U+0338, each drawn whole from the line's cluster text (#21) | font |
| `geometry.png` | the same screen and strips with every character replaced by a space: backgrounds, decorations and cursors only | exact |
| `selection_linear.png` | 24 × 3 cells, one wrapped line: a linear selection starting on the right half of a wide character (the whole character is selected), covering the whole middle row and ending on the left half of another (its right half too); selection blended over the background, text in its own colour (#22) | font |
| `selection_block.png` | 24 × 4 cells of mixed-width text: a block selection whose edges cut through wide characters, which are taken whole (#22) | font |
| `overlays.png` | 24 × 3 cells: two search matches (one over an explicit background), the current match over a selection, a hovered link underlined in its own colour (#22) | font |
| `underlines.png` | 24 × 5 cells: single, double, curly, dotted and dashed underlines, each in the foreground and in an SGR 58 colour (#22) | font |
| `scripts.png` | 24 × 4 cells: overline (foreground, and SGR 58 with an underline), superscript, subscript, and both flags at once drawn as superscript (#22) | font |
| `cursor_wide.png` | five strips of a block cursor: on a wide character, on its spacer (the same image), on the second cell of `=>`, on a built-in full block `█` (redrawn edge to edge) and on inverse underlined text (the underline redrawn in the cursor's text colour) (#22) | font |

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

**Fonts.** The six #22 goldens need Menlo alone (it ships with every
macOS, the `macos-15` CI runner included) plus the renderer's built-in box
drawing and block elements: no CJK, Hangul or emoji text, whose glyphs come
from fallback fonts (Hiragino, Apple SD Gothic Neo, Apple Color Emoji) that
change between macOS versions. Their wide characters are cells built by
hand, a Menlo `W` with the WIDE flag and its SPACER, which is all the
renderer looks at; the behaviour of real wide characters, emoji under the
cursor and the wide-character rules of selections are pinned by the
structural and pixel tests in `tests/overlay.rs` and `src/passes/`
instead. `clusters.png` (#21) is the one golden that draws fallback-font
glyphs (emoji and combining marks) and has passed on `macos-15` since #21.

**Tolerance.** All six use the font tolerance above (at most 4 pixels per
16 × 16 tile and 0.5% of the image over 16/255); their geometry (the
highlight rectangles, the underline patterns) is also pinned exactly by
the pixel tests in `tests/overlay.rs` and the unit tests in
`src/passes/`.

When a comparison fails, the test writes the rendered image to
`target/tmp/<name>.actual.png` (Cargo's `CARGO_TARGET_TMPDIR`) and prints
the path, so it can be compared with the golden side by side.

`TESSERA_SKIP_GPU_TESTS=1` skips the suite on machines without a GPU, as for
`headless_clear.rs`.

If CI disagrees with goldens produced locally beyond the font tolerance,
the fallback is a per-platform golden set, recorded in ADR 0010; the exact
`geometry.png` must still agree everywhere.
