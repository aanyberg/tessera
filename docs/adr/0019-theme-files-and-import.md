# ADR 0019: Theme files, bundled themes and scheme import

**Status:** accepted · 2026-10-28

## Context

ADR 0004 puts themes in `themes/*.toml` next to `config.toml` and plans an
`.itermcolors` importer. Issue #41 makes `appearance.theme` resolve to
colours, ships ten themes, imports iTerm2 and base16 schemes, and re-themes
open views when the configuration changes. The Appearance pane that lists
and imports themes is #40; this ADR covers the core.

## Decision

**File format.** A theme file is flat TOML and the file is the theme:
`name`, `foreground`, `background`, `cursor`, `selection` and `ansi`, an
array of exactly 16 colours (black, red, green, yellow, blue, magenta,
cyan, white, then the bright eight). Colours are `#rrggbb` or `#rrggbbaa`
sRGB strings, hex in either case; nothing else is a colour (no bare hex,
no `#rgb`, no names, no padding). Serialisation is lowercase and drops the
alpha digits when opaque. `cursor` defaults to the foreground and
`selection` to the foreground at alpha `0x40`, so a hand-written minimal
theme works; `name` defaults to the file stem when loaded from a file.
Unknown keys are ignored. Every error names its key (`ansi[3]`), and a
file is read only if it is a regular file of at most 64 KiB.

**Lookup.** `ConfigStore::theme()` (via `Theme::resolve`) tries, in order:
a `themes/*.toml` file whose stem matches `appearance.theme`, one whose
`name` key matches, a bundled theme, and finally Tessera Dark with
`Warning::ThemeNotFound`. Names match ignoring case or by slug: letters
and digits of any script, lowercased; each `+` spelled out as the word
`plus` (so `Dracula+` is `dracula-plus` and cannot shadow Dracula); every
other run of characters one `-`. So `solarized-dark` finds Solarized Dark,
`Rosé Pine` slugs to `rosé-pine`, `solarizeddark` finds nothing, and a
user file shadows a bundled theme of the same name. Unicode is not
normalised (no dependency for it): a name in NFC (`é` precomposed) and the
same name in NFD (`e` plus a combining accent, which is not a letter and
separates) are different names; each matches its own form. A requested
name longer than 256 bytes (`MAX_THEME_NAME_LEN`) is not compared at all,
so a huge `appearance.theme` cannot make every lookup slow. Files are taken in file-name order so the result
is deterministic; hidden files and non-`.toml` files are skipped. A user
file that matches by stem but does not load adds `Warning::ThemeInvalid`
and the search goes on, so a typo in the user's own Dracula falls back to
the bundled Dracula with the reason shown rather than silently. Theme
warnings are kept apart from the config load's and surfaced, after them,
by `ConfigStore::last_warnings()`. `appearance.theme` is a name, never a
path: the lookup lists `themes/` and compares names, and never joins the
value onto a path, so `../config` matches nothing outside `themes/`; only
an empty name is not looked up. The lookup reads the directory on every call; it is meant for configuration
changes, not frames, and parses at most 256 files when it falls back to
matching `name` keys.

**Bundled themes are compiled in** (`include_str!` of
`crates/tessera-config/themes/*.toml`) rather than copied into the user's
`themes/` on first launch. Shipping them in the binary means an upgrade
fixes a bundled palette for everyone, the user's directory holds only the
user's themes, and a missing or read-only support directory still has ten
themes. A test parses each, pins their names and order with an `insta`
snapshot, and checks Tessera Dark against `tessera_render::Theme::default()`
byte for byte, so the default theme changes nothing on screen.

The ten and their sources (each file carries an attribution header):

| Theme | Source | Licence |
|---|---|---|
| Tessera Dark | the renderer's previous default (xterm colours) | project |
| Tessera Light | original palette (foreground and ANSI 1-6 meet WCAG AA on its background, tested) | project |
| Solarized Dark, Solarized Light | Ethan Schoonover, altercation/solarized | MIT |
| Dracula | dracula/dracula-theme | MIT |
| Nord | nordtheme/nord (Arctic Ice Studio, Sven Greb) | MIT |
| Gruvbox Dark | morhetz/gruvbox (no licence file; the README says MIT/X11) | MIT/X11 |
| One Dark | atom/one-dark-syntax (GitHub Inc.), alacritty port's ANSI | MIT |
| Tokyo Night | enkia/tokyo-night-vscode-theme; ANSI from folke/tokyonight.nvim (night) | MIT; Apache-2.0 |
| Catppuccin Mocha | catppuccin/catppuccin (official alacritty/ghostty ports) | MIT |

All are compatible with Tessera's MIT OR Apache-2.0. `cargo-deny` does not
see data files, so this table is the record.

**Colour spaces in the iTerm2 importer.** An `.itermcolors` colour has
`Red/Green/Blue Component` floats and an optional `Alpha Component` and
`Color Space`. Tessera decodes it the way iTerm2 does
(`NSDictionary+iTerm.m`, `colorValueForKey:`): `sRGB` is taken as is;
`P3` is Display P3, converted to sRGB (sRGB transfer curve, then a 3x3
matrix, out-of-gamut values clamped); anything else, *including a missing
key* and differently cased spellings, is AppKit's "calibrated" RGB, which
is Apple Generic RGB: gamma 461/256 (about 1.8), then a 3x3 matrix to
linear sRGB, then the sRGB curve. The matrices are the spec fixture's
rules R3/R4, checked against AppKit within 1/255. This matters in
practice: older presets such as Solarized Dark Patched carry no
`Color Space`, and reading them as sRGB gives visibly wrong colours
(`#d11c24` for Solarized red instead of `#dc322f`). Components outside 0..1
are clamped, bytes round half up (`floor(x * 255 + 0.5)`), and a missing
cursor or selection follows the theme-file defaults. The 16 ANSI colours,
foreground and background are required, and so are the three components
of each colour: a missing component, or one that is a string, fails the
import (`MissingKey`, `BadComponent`) where iTerm2 would substitute a
default; a scheme that broken is better reported than half applied.

The property list is read through serde into a fixed three-level model
(root dictionary, colour dictionary, scalars) instead of `plist::Value`:
`plist::Value` builds nested values recursively and overflows a 2 MiB
stack on a few tens of thousands of nested arrays, in XML and binary
alike. Collections a colour scheme never holds (a root array, arrays
anywhere, dictionaries inside a colour) are errors rather than drained,
and every value and dictionary entry is charged to a per-import budget of
4096 values and 256 KiB of strings, failing with
`ImportError::TooComplex` when it runs out. The budget is what stops
amplification: a binary plist may reference one object from many places,
and `plist` rejects only cycles, so a few kilobytes can otherwise describe
a cube of shared arrays or dictionaries (N^3 visits; 35 s for 4.9 KB before
the budget) or one big string repeated thousands of times (a 260 KB file
reached 1 GB RSS). Integer dictionary keys cost no text, so the value
count is a separate limit from the text bytes, and each has its own test.
Only `Color Space` strings are kept, up to 32 bytes. Inputs are capped at
1 MiB. The budget lives in a thread-local that the import sets and clears,
because `plist::from_bytes` takes no `DeserializeSeed` and the
`plist::Deserializer` that would accept one is behind `plist`'s unstable
feature. Refusing collections makes the importer stricter than iTerm2,
which ignores unknown keys whatever they hold: a file with an unknown key
holding an array, or a dictionary inside a colour, is rejected. No
published scheme has one, and accepting them would mean draining
collections the budget would then have to cover.

**base16.** Both published formats are read by a hand parser (no YAML
dependency): the legacy flat files (`scheme:`, `base00: "002b36"`) and the
current tinted-theming files (`system: "base16"`, `name:`, colours with `#`
under `palette:`). Values may be bare or quoted; `# comments` follow YAML
(a `#` after whitespace, outside quotes). Every value is a string, so
`base00: 100e10` stays a colour rather than becoming a float. Other
nesting, lists, anchors, flow collections, duplicate keys and tab
indentation are errors naming the line, as is any `system:` other than
`base16`. The styling guide defines roles, not terminal colours; the
mapping is the one every base16 terminal template uses (tinted-shell):
ANSI 0-7 = base00, 08, 0B, 0A, 0D, 0E, 0C, 05; ANSI 8-15 = base03, 08,
0B, 0A, 0D, 0E, 0C, 07; foreground and cursor base05, background base00,
selection base02.

**Import tool.** `tessera-devtool theme import <file> [--name] [--out]
[--force]` picks the importer by extension, writes `<out>/<slug>.toml` and
refuses to replace a file without `--force`. Every failure exits 125, the
tool's failure code. Any importing frontend (the Appearance pane, #40)
follows the same rules:

- Read the scheme with `read_scheme_file`: regular files only, opened
  without blocking on a FIFO, never read past `MAX_IMPORT_LEN`.
- Imported names are cut to `MAX_THEME_NAME_LEN` (256 bytes, on a
  character boundary; `clip_theme_name` for a name the user types), so a
  theme file written for any name stays far below `MAX_THEME_FILE_LEN`
  even when every character needs a TOML escape.
- Replacing (`--force`) uses `write_atomic`. Creating without replacing
  writes a temporary file of the call's own, flushes it and hard-links it
  to the target; the link fails rather than replaces, so the creation is
  exclusive and atomic and two concurrent imports cannot overwrite each
  other. A check for the file followed by a rename is not exclusive and
  must not be used (the Appearance pane included). On a filesystem
  without hard links (FAT, some network shares) the file is created with
  `create_new` and written in place: still exclusive, not atomic.

**Live re-theming goes through `Renderer::set_theme`.** On
`ConfigEvent::Changed` the application calls `App::apply_theme` with
`ConfigStore::theme()`. It maps the theme with `theme_for(config, theme)`
(sRGB bytes decoded to linear; the background's alpha multiplied by
`appearance.opacity`), hands it to every view's renderer through
`Renderer::set_theme`, and keeps it for views attached later. Nothing is
rebuilt: no new renderer, surface or font database, so a theme change is
cheap and keeps the views' state. Fonts are not reloaded here; a font
family change takes the font reload path (#29). The amendment below
replaces `apply_theme` with `App::apply_config` and makes the `App` call it
itself.

## Consequences

- ~~The watcher follows `config.toml` only~~ and ~~a view's surface keeps
  the transparency mode it was attached with~~: both resolved by #100; see
  the amendment below.
- Known limits:
  - FSKit FAT32/exFAT and HFS+ volumes store file names decomposed (NFD).
    A hand-written theme file with no `name` key, whose stem was typed
    precomposed (NFC), is therefore not found by the NFC name on those
    volumes, since the stem read back is NFD (see the normalisation note
    under Lookup). Imported themes always carry a `name` key and resolve
    through it.
  - The base16 block-scalar skip is laxer than YAML: tab-led lines, and
    lines dedented below the block's own content but still deeper than its
    key, are swallowed as part of the block where a YAML parser would
    reject the file. It never changes a value YAML would read (checked
    against libyaml on all 361 tinted-theming schemes).
  - A hand-written theme whose `name` is longer than `MAX_THEME_NAME_LEN`
    (256 bytes) cannot be selected by that full name, because longer lookup
    names are not compared. It is found by its file stem, or by any name
    within the cap that has the same slug (the slug itself, or the first
    256 bytes of the name). Imported names are clipped to the cap, so this
    affects hand-written files only.
- Follow-ups collected in #100: watching `themes/` for edits; wiring
  `ConfigEvent::Changed` to `App::apply_theme` and the configured theme at
  app start in the frontend; the transparency mode of existing views on a
  translucent re-theme (above); Unicode normalisation of theme names (NFC
  and NFD spellings are different names today). #100 resolved all but the
  last, which stays open (it needs a normalisation table, and no new
  dependency was taken for it).
- iTerm2's `(Light)`/`(Dark)` dual-mode keys are ignored for now; only the
  plain keys are read.
- Bundled themes cannot be edited in place; copying one into `themes/`
  (same name) overrides it.

## Amendment 2026-10-29: live re-theme and the alpha-mode switch (#100)

**`App::apply_config(&Config, &Theme)` replaces `apply_theme`** (kept as
`apply_config(&app.config(), theme)` for existing callers). It builds the
renderer theme from the *new* configuration (colours, the background's
alpha times the new `appearance.opacity`, font family and size). The
configuration has no line-height or padding keys, so those stay the
renderer's defaults. The construction-time configuration no longer leaks
into a re-theme: the `App`'s configuration is the live one and
`App::config()` returns it. The `App`'s own watch calls `apply_config`
(ADR 0006): on every `config.toml` change, and on a `themes/` change when
the re-resolved theme differs from the one in use (compared with
`PartialEq`), so an edit of a theme file that is not in use changes nothing.
A theme file in use that does not load (an editor's half-written save, a
typo; the lookup reports `Warning::ThemeInvalid`, or the file the colours
came from still exists but fails to load) keeps the colours in use, logged,
instead of falling back to the bundled or default theme. The save that fixes
it applies, since the watcher keeps following the file (ADR 0006). The
watch's start-up catch-up follows the same rule, so a read during a save
does not flash the defaults either; the save's own event applies it. A
broken file that matches the name only by its stem does not hold back the
theme in use when the lookup still finds the very file the colours came
from (by its `name` key).
A configured `font_size` that changed sets every view to it (a zoom the user
applied included, as Terminal.app does on a profile change) and resizes
each session after the views lock is released. Any other change keeps each
view's size. A font family change still applies only to views attached
after the font database is (re)loaded (#29).

**The surface follows the background's opacity.** `View::set_appearance`
compares the new background's alpha with 1.0 and asks the surface for the
matching compositing mode (`RenderSurface::request_transparent`). The
switch itself (`RenderSurface::set_transparent`: `choose_alpha_mode` of the
surface's capabilities, then `configure`) happens at the start of the
view's next frame (in the surface's frame acquire, so `View::render` is
unchanged). `apply_config` runs on the watch's thread, and
reconfiguring sets `CAMetalLayer` properties (`opaque`, pixel format,
drawable size), which belong on the main thread that draws (ADR 0014). An
already attached view therefore turns translucent (or opaque) on its next
frame instead of showing the premultiplied colour on an opaque surface. A
test with a real `CAMetalLayer` checks the mode and the layer's `opaque`
flag both ways. An offscreen test checks that a live translucent re-theme
draws the same pixels as a view attached translucent from the start
(`#282a36` at alpha `0x80`).

**The host is told once per applied change.** After an `apply_config` that
changed what the views draw (colours, opacity or font size), the observer's
`on_appearance_changed(opaque)` runs on the watch's thread. In the C API
that is `TesseraCallbacks.on_appearance_changed`, the table's new last
field. The host makes its windows opaque or translucent to match and asks
every view for a frame. It is called on every such change, not only when
`opaque` flips: the app's display link idles when nothing changed, so a
colour-only re-theme of a quiet terminal would otherwise not be drawn
until its next output. The host needs nothing else: colours stay in the
core, and the Preferences pane (#40) goes through the file. In the app,
TesseraKit coalesces the callbacks into `appearanceDidChange(opaque:)` on
the main actor, and the session router calls every tab's
`TerminalSurfaceView.applyLayerOpacity(opaque:)`, which sets
`window.isOpaque` and the window's background colour (invalidating its
shadow) and requests a frame. It leaves `metalLayer.isOpaque` to the core,
which sets it when it reconfigures the surface at the start of that frame:
set earlier, an opaque layer would show the last translucent frame, whose
colours are premultiplied, darkened until the next frame. A view without a
surface (its first attach failed) keeps its window as it is.

The callback is never called with the views lock or a session's delivery
lock held. An observer may call back into the `App` (views, sizes and
metrics, configuration, `apply_config`, closing a session, even stopping
or restarting the watch from the watch's own thread, which then does not
wait for itself); a test does all of these. It must not replace the
observer (`set_observer` waits for the call) or drop the last `Arc<App>`
(dropping replaces the observer); in the C API it must not call
`tessera_app_free`, and, as for every callback there, no `tessera_*`
function at all. A watch that is stopped or replaced drops the events
still queued for it, so an older change never lands after a newer one.

**Where the themes come from.** `themes/` is next to `config.toml`: in
`~/Library/Application Support/Tessera`, or in the directory the
`TESSERA_CONFIG_DIR` environment variable names (unset by default; ADR
0004). A font *family* change is still not applied live; it stays on #29's
font reload path. The configuration has no line-height or padding keys, so
those stay at the renderer's defaults.
