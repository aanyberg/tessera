# ADR 0013: Style layout for underline colour and styles, CSI sub-parameters as a bitmask

**Status:** accepted · 2026-10-28

## Context

Issue #7 makes `tessera-term` apply SGR (`CSI Pm m`): bold, dim, italic,
the underline styles single, double, curly, dotted and dashed (`4:0` to
`4:5`, kitty's extension), blink, inverse, hidden, strikethrough, 16, 256
and 24-bit foreground and background, and underline colour (`58` / `59`).
Two things stood in the way:

- `tessera_grid::Style` was `{ fg: Color, bg: Color, flags: u16 }`, 10
  bytes, inside a 16-byte `Cell` (ADR 0008). It had nowhere to keep an
  underline colour, and only bits 3 (`UNDERLINE`) and 8
  (`DOUBLE_UNDERLINE`) for the underline shape.
- `tessera-vt` flattened CSI sub-parameters (`params.iter().flatten()`),
  so `CSI 4:3 m` (curly underline) and `CSI 4;3 m` (underline, italic)
  reached the emulator as the same `[4, 3]`, and `38:2::1:2:3` could not be
  told from `38;2;0;1;2;3`.

The expected behaviour is pinned by
`crates/tessera-term/tests/fixtures/sgr_spec.txt` (276 pen rows and
33 cell blocks, each citing xterm, ECMA-48, kitty, WezTerm or Alacritty)
and a colour-table script checked cell by cell against an oracle
generated from the colour numbers. The milestone lead adopted the
fixture's recommended decisions; this ADR records the model.

## Decision

### `Style` gains `underline: Color`; `Cell` grows from 16 to 20 bytes

```rust
pub struct Style { fg: Color, bg: Color, underline: Color, flags: u16 }
```

`Color` is 4 bytes (a tag and three bytes), so `Style` is 14 bytes, inside
the issue's bar of 16. `Cell` is a 4-byte `char`, the `Style` and the
1-byte `CellFlags`: 19 bytes, padded to 20 by the `char`'s alignment. Size
tests assert `size_of::<Style>() <= 16` and `size_of::<Cell>() == 20`.

The user confirmed the trade-off. Three 4-byte colours, the flags and a
`char` cannot share 16 bytes without repacking `Color` (for example a
3-byte RGB with the tag folded into spare index values), which would touch
every reader of `Color` in `tessera-render` and is out of scope. The
alternative, parsing SGR 58/59 and dropping the colour, was rejected
because the issue names underline colour and every terminal that supports
curly underlines (kitty, WezTerm, Alacritty, iTerm2) supports coloured
ones, which is what spell-checkers and LSP diagnostics in editors use.

The cost is memory and bandwidth. Every write moves 25 % more bytes per
cell. An 80-column line asks the allocator for 1 600 instead of 1 280
bytes of cells; macOS's allocator hands 1 280 out exactly but rounds 1 600
up to 1 792, so a plain scrollback line costs 1 856 bytes allocated (by
`malloc_size`) instead of 1 344 (38 % more): about 17.7 MiB instead of
12.8 MiB for the default 10 000 lines. `scrollback.rs` carries the measured table. ADR 0008 is
amended by one line.

### Underline shape lives in the flags, at most one shape bit

`Style::UNDERLINE` (bit 3) means "some underline". Exactly one of
`DOUBLE_UNDERLINE` (bit 8), and the new `CURLY_UNDERLINE` (9),
`DOTTED_UNDERLINE` (10) and `DASHED_UNDERLINE` (11) may accompany it; none
means single. `Style::UNDERLINE_STYLE_MASK` covers all five bits.
`Style::underline_style() -> UnderlineStyle` (`None`, `Single`, `Double`,
`Curly`, `Dotted`, `Dashed`) reads the shape and
`Style::with_underline_style` sets it, clearing the other shape bits, so
the invariant holds by construction; a property test feeds random SGR
lists and checks it on the pen and on every printed cell.

Keeping bits 3 and 8 means the renderer, which draws a single line for
bit 3 and a double one for bit 8, needs no change: curly, dotted and
dashed underlines draw as single lines, and the underline colour is not
yet used (the line takes the foreground colour), until #22 draws them.
The decoration pass groups runs by `UNDERLINE | DOUBLE_UNDERLINE |
STRIKETHROUGH`, so a curly cell next to a single one joins its run; that
is harmless while both draw the same way and is #22's to split.

`Style::flags` bits stay attributes only; width and cluster markers stay
in `CellFlags` (ADR 0008).

### `tessera-vt` reports sub-parameters as a `u32` bitmask

`Action::Csi` gains `subparams: u32`: bit `i` is set when `params[i]` was
introduced by `:`. `params` stays the flat `Vec<u16>`, so every existing
`&[u16]` consumer is untouched. `CSI 4:3 m` is `[4, 3]` with `0b10`,
`CSI 38:2::1:2:3 m` is `[38, 2, 0, 1, 2, 3]` with `0b11_1110`, and
`CSI 1;4:3;5 m` is `[1, 4, 3, 5]` with `0b100`. `vte` keeps at most 32
entries (sub-parameters included) and flags a longer list `ignore`, which
`tessera-term` drops whole, so every kept entry has a bit. The mask costs
no allocation; nested vectors (`Vec<Vec<u16>>` or a `SmallVec` per group)
would have allocated per group on every SGR, the hottest CSI there is.
`Action::DcsStart` keeps flattened parameters only; nothing reads
sub-parameters there yet.

### SGR semantics

xterm is the reference for 0-9, 21-29, 30-49, 90-107 and 38/48; xterm has
no 58/59 and ignores `4:x`, so kitty is the reference for those,
cross-checked with WezTerm and Alacritty. In short (the module docs of
`tessera-term/src/sgr.rs` list every parameter):

- Parameters apply left to right; an empty list or slot is `0`, a reset of
  fg, bg, underline colour and every flag. Unknown numbers are ignored and
  parsing continues.
- Bold and dim may both be set; 22 clears both. 21 is double underline
  (xterm, kitty, WezTerm; Alacritty's "cancel bold" is the odd one out).
  24 clears every underline style. 5 and 6 are the same blink flag.
- `4:0`..`4:5` select none, single, double, curly, dotted, dashed; `4:n`
  with `n > 5` is single (WezTerm, Alacritty; kitty clamps to dashed).
- `38`, `48`, `58` with `;` read the next flattened parameters: a kind,
  then one index (`5`) or three components (`2`). Too few values set
  nothing and end the list (xterm consumes what exists); an unknown kind is
  skipped with its introducer and parsing continues (xterm, Alacritty;
  kitty drops the whole SGR). The `;` form has no colour-space id.
- With `:` the colour reads only its own group: `5:n`; `2:r:g:b` when
  exactly four sub-parameters follow the introducer, `2:id:r:g:b` (id
  ignored, extras dropped) when five or more do. This is how xterm, kitty
  and Alacritty disambiguate; `38:2:1:2:3:4` is `rgb(2,3,4)`.
- An index or component above 255 makes that colour a no-op; its
  parameters are still consumed (xterm, WezTerm, Alacritty; kitty masks
  with `& 0xFF`).
- A `:` group headed by anything but 4, 38, 48 or 58 is ignored whole
  (xterm, Alacritty): `1:2` is neither bold nor dim, `0:1` is no reset.
- `Cell::blank_with` (erase, scroll, insert) keeps only the pen's
  background, never its underline colour; `Cell::is_blank` requires a
  default underline colour. DECSC/DECRC save and restore the whole
  `Style`, underline colour included.
- Any SGR clears REP's remembered character, as every non-print action
  does (xterm).

Accepted divergences, documented as `free` rows: `vte` turns an empty slot
into 0, so `38;5;m` is index 0 where xterm drops the colour, and mixed
separators (`38;5:196`) are accepted as a by-product of the flattened
reader.

No palette lives in `tessera-grid` or `tessera-term`; colours stay
`Indexed` or `Rgb` and `tessera-render` resolves them (ADR 0010).

## Consequences

- Every grid and scrollback line costs 25 % more cell memory (38 % more
  allocated, by `malloc_size`, for an 80-column line on macOS), and every
  cell write and scroll moves 25 % more bytes; refilling a recycled line on scroll is where it
  shows. Measured against the merge base on the shared eleven-line
  corpus, the larger cell alone cost 6 to 9 % more retired instructions on
  plain text; handling `Action::Print` ahead of the sequence match in
  `State::apply` (it no longer runs the action's drop glue) won back about
  half, leaving at most +3.5 % instructions and wall-clock ratios within
  the issue's 1.10 bar. The table below is from the final code: release
  build with the workspace profile, each line plus CRLF repeated N times
  into a fresh 80x24 `Terminal` in 4 KiB chunks, base and branch runs
  interleaved for 7 pairs (cjk 21, emoji 15), best per side; instructions
  and cycles from `/usr/bin/time -l`. The machine was heavily loaded, so
  instructions are the stable column.

  | line | N | wall | cycles | instructions |
  |---|---|---|---|---|
  | ascii | 32 000 | 0.804 | 1.024 | 1.035 |
  | latin_diacritics | 64 000 | 0.930 | 0.903 | 1.030 |
  | cjk | 32 000 | 1.013 | 1.045 | 1.032 |
  | box_drawing | 64 000 | 0.952 | 0.929 | 1.032 |
  | hebrew | 64 000 | 0.849 | 1.033 | 1.033 |
  | arabic | 32 000 | 0.943 | 0.977 | 1.035 |
  | punctuation | 32 000 | 0.963 | 1.009 | 1.033 |
  | braille | 64 000 | 0.943 | 0.930 | 1.031 |
  | emoji | 16 000 | 0.976 | 0.937 | 1.007 |
  | sgr_heavy | 16 000 | 0.932 | 0.956 | 1.002 |
  | combining_heavy | 16 000 | 0.931 | 0.982 | 1.010 |

  sgr_heavy against the same branch with `m` routed to the ignore arm
  (the cost of applying SGR at all): wall 1.083, cycles 1.053,
  instructions 1.060.
- `Style` struct literals must name `underline` or use
  `..Style::DEFAULT`; the render tests and sources were updated.
- The renderer has what #22 needs to draw curly, dotted and dashed
  underlines in their own colour without another layout change.
- Further SGR attributes (53 overline, 73-75 super/subscript) have no flag
  yet; there are four free bits (12-15); #81 adds them. Advertising `Su`,
  `Smulx` and `Setulc` in the terminfo entry (ADR 0005) is a follow-up.
  DECSTR resets the whole pen, underline colour and shape included, since
  #80.
