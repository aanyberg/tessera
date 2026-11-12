# Debugging

Two dev-only tools show what the emulator and renderer do without Xcode or the Swift app. Neither is shipped or
linked by the app. The contributor workflow is in [`CONTRIBUTING.md`](../CONTRIBUTING.md). The last section records
what a live resize does to the process's memory footprint, so a footprint budget in a test is not mistaken for a leak.

## Screen dumps with `tessera-devtool`

`tessera-devtool` runs a command in a headless terminal and prints what the
emulator drew, without the app:

```sh
cargo run -p tessera-devtool -- run 'ls --color' --cols 80 --rows 24
cargo run -p tessera-devtool -- run 'printf "a\tb\n"' --cols 20 --rows 3 --format text
```

The command string runs as `/bin/sh -c -- <command>` in a real PTY (so quote
it as one argument; a leading `-` is part of the command), with the environment every Tessera session gets
(`TERM=xterm-256color`, `COLORTERM=truecolor`, …) and the current directory.
Nothing is written to its input. Output feeds `tessera_term::Terminal`; when
the command exits, the final grid is printed in one of three formats:

- `--format ansi` (default): the screen re-encoded as SGR colours and
  attributes, for viewing in a real terminal. Each row ends with a reset.
- `--format text`: plain text, one line per row, trailing blanks trimmed, a
  `\` after a row that soft-wraps. These are the rules of the emulator's
  snapshot helper (`render_screen`, which always marks the cursor; add
  `--cursor` to match it), so a dump can be pasted into a test.
- `--format cells`: one line per cell, `row:col "text" markers style`, to see
  wide-character spacers, grapheme clusters and exact colours.

`--scrollback` prints the history above the screen (as negative rows in
`cells`); `--cursor` marks the cursor (a `|` after its cell in `text`,
`<cursor` in `cells`).

The tool waits about `--timeout` seconds (default 10, fractions allowed),
even while the command floods the terminal; the timeout also covers the
emulator processing the output, so a command that exits while the tool is
still behind on its output counts as timed out. When the timeout expires the
tool sends `SIGHUP` to the command's shell, gives it 200 ms to exit, then
sends `SIGKILL`, keeps reading for up to one more second, prints the grid and
exits with `124`. Deadlines are checked between slices of about 512 bytes of
output, so at extreme sizes (a 2000-column `a ESC[65535b`, DECALN on a
2000x2000 screen) a single slice can take seconds and stretch the bound. Only
the shell is signalled: `tessera-pty` has no process-group signal, so
processes the command started are left to the hangup the kernel sends when
the shell (the session leader) dies. Otherwise the tool exits with the
command's status (`128 + signal` if a signal ended it); a closed stdout
(`| head`) does not change that. `125` means the tool itself failed.

Memory grows with `--cols`: the default 10 000 lines of scrollback are kept
at full width even without `--scrollback` (about 1.25 MiB per line at 65535
columns), and `--format cells` builds its whole output in memory. Inherited
`COLUMNS` and `LINES` are passed to the command unchanged, and programs that
read them believe them over `--cols`/`--rows`; unset them for such programs.

The formatters are a library too (`tessera_devtool::dump::{ansi, text,
cells}`), so tests can dump a `Grid` they built by hand. The crate is
dev-only: `publish = false`, and a test checks that `tessera-ffi` and
`tessera-app-core` never link it.

## The development window

`just devwin` (`cargo run -p tessera-devwin`) opens a winit window running
your login shell through `tessera_app_core::App`: the same views, renderer,
keyboard encoder and session events the app uses, without Xcode or the Swift
frontend. Use it to see renderer or emulator changes interactively, and the
devtool when you need a reproducible dump of one command's screen for a test
or a bug report. Cmd-V pastes (Ctrl+Shift+V off macOS, read through `pbpaste`,
`wl-paste` or `xclip` on a helper thread, killed after 2 s), keys typed while
it is read wait for it, other Cmd chords are dropped, the left Option key is
Alt and the right one composes (so Nordic and German layouts can still type `@
[ ] { } | \ $`), the window title follows the shell (OSC 0/2), and the
bell is printed on stderr. Dead keys
should work (`´` then `e` types `é`; see the manual check below), but there is
no IME, so no CJK input: allowing winit's IME would let macOS's press-and-hold
accent menu replace key repeat (kitty and WezTerm turn
`ApplePressAndHoldEnabled` off, which needs Objective-C calls), and composed
input belongs to the app's terminal view (#27). It reads and watches your
configuration like the app (`config.toml` and the theme in use; a
re-theme shows from its next redraw, since it does not listen for the
appearance callback), has no tabs, selection or copy, and like the devtool is
dev-only: a test checks that `tessera-ffi` and `tessera-app-core` never link
it or winit. It installs no signal handlers: Ctrl-C in the terminal that
launched it, or `kill`, ends it without closing the session, and a child that
ignores `SIGHUP` is orphaned; close the window instead. A frame that
drew nothing is retried about a refresh later only when it left content
pending (paced above 240 frames per second, or the surface not ready), never
when nothing changed, so the window neither loses a paced frame nor wakes
every refresh while idle (ADR 0016). A debug build draws visibly
fewer frames during a flood (`cat` of a large file); judge rendering speed
with `cargo run --release -p tessera-devwin`.

**Manual check (priority, not yet done):** the dead-key handling assumes
AppKit composes a dead key into the next key event without an IME. Events
posted to the process with `CGEventPostToPid` suggest AppKit keeps no dead-key
state between events, in which case `´ e` gives a plain `e` and `^ Space` a
space; a locked screen cannot settle it. Before relying on dead keys, switch
to the German layout, run `just devwin`, start `cat -v` and type `´ e`, `^
Space` and right-Option+n then Space. If they come out unaccented, devwin
needs its own `UCKeyTranslate`-based dead-key state (as Zed and kitty keep) or
an IME. Known limit: with the left Option as Meta, a US key whose Option layer
is a dead key (`M-u`, `M-e`, `M-n`, `M-i`, `` M-` ``) may leave a composition
pending, so the next key can come out accented.

## Live resize footprint

Resizing a window that renders makes the process's physical footprint (`phys_footprint`, what Activity Monitor and
the footprint budgets in the app tests read) climb by about one drawable per resize step and fall back in batches.
On CI it reached about 1.2 GB in the flood tests (#326). The controls below show that the growth is neither
Tessera's allocations nor `wgpu` freeing late, and the likely cause is macOS. Every resize gives the pane's
`CAMetalLayer` a new drawable size, and each frame presented at that size hands an IOSurface of it to the window
server. The best reading of the measurements is that macOS keeps those surfaces after the layer has dropped them and
charges them to the process. No control isolates the window server itself, such as a windowed layer presenting
through plain Metal without `wgpu`. `footprint <pid>` lists the growth as "Owned physical footprint (unmapped)",
memory the process owns but does not map. It is released in batches every 40 to 100 steps, and all of it within
0.5 to 3 s of the last resize. By then the pane has nothing new to draw and the app submits no GPU work.

The measurements come from a throwaway CI branch (run 42443391618, `macos-15`, scale 1, Debug app tests). The
window cycled through 900 × 600, 1200 × 800, 1000 × 700 and 1100 × 650 points, with 200 steps 20 ms apart and
about 200 frames presented. Growth is the peak minus the start. Settled is the footprint 3 s after the last step.

| Variant | Growth with TSan | Growth without TSan | Settled vs start |
| --- | --- | --- | --- |
| As shipped (two runs) | +115, +195 MB | +212, +197 MB | -6 to +9 MB |
| No rendering (resizes only) | +0 MB | +0 MB | -1 to 0 MB |
| Fixed size, output only | +3 MB | +0 MB | -3 to +3 MB |
| An autorelease pool around every display-link tick | +172 MB | +196 MB | -3 MB |
| `device.poll(wait)` after every resize | +104 MB | +210 MB | -3 to -2 MB |
| Frame latency 1 instead of 2 | +153 MB | +209 MB | -2 to -1 MB |
| Autorelease pool and poll | +186 MB | +212 MB | -50 to 0 MB |

The growth is linear at about 2.6 MB per presented resize step, the size of one BGRA drawable at about
1000 × 700 pixels. The "Owned physical footprint (unmapped)" category went from 3 MB in 2 regions to 91-185 MB in
35-74 regions and back. The IOSurface and IOAccelerator categories stayed flat, and so did everything Tessera
allocates (the renderer has no per-size texture other than the swapchain). Nothing the app or the renderer controls
moved the growth. An autorelease pool would drain autoreleased drawables and `device.poll(wait)` would run `wgpu`'s
deferred frees, but both left it unchanged, and so did a shorter swapchain. A bare `CAMetalLayer` that is not in a
window shows no growth (a flat 143-146 MB at scale 2, with 99 MB of it in the same category), which suggests that
window-server compositing is what holds the surfaces. A hosted app test's window that is not visible presents
nothing: a locked screen, a sleeping display, a covered window or one on another Space. So the effect can only be
seen on CI or with the test window on screen.

`ResizeFootprintTests` (in `apps/macos/Tests/TesseraTests`) bounds what remains. Over 200 steps 20 ms apart with a
rendering pane, the peak must stay below 0.9 of the bytes that keeping every step's drawable would cost. The
measured peaks reached 0.2 to 0.4 of that, and the bar leaves room for a late batch. The footprint must also be back
within 150 MB of its start no more than 10 s after the last step. The test prints the frames presented meanwhile. A
texture kept per resize (1.17 to 1.2 of the sum, never released) fails both bounds. The test skips when the window
is not visible, because nothing is drawn then. The other footprint budgets in the app tests leave live resizing out.

## Window blur radius fallback

`[appearance] blur_radius` above 0 blurs a window through the private call `CGSSetWindowBackgroundBlurRadius`
(ADR 0045), looked up once at the first window that asks for it. If the lookup fails, every window keeps the
material blur of ADR 0043 and the app logs one error, category `window`
(`log stream --predicate 'subsystem == "com.aanyberg.tessera" AND category == "window"'`). To see that fallback on a
macOS that has the call, start the app with `TESSERA_WINDOW_BLUR_RADIUS=off` in its environment, for example
`TESSERA_WINDOW_BLUR_RADIUS=off apps/macos/DerivedData/Build/Products/Debug/Tessera.app/Contents/MacOS/Tessera`.
