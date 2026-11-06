# Debugging

Two dev-only tools show what the emulator and renderer do without Xcode or the Swift app. Neither is shipped or
linked by the app. The contributor workflow is in [`CONTRIBUTING.md`](../CONTRIBUTING.md).

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
