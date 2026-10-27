# ADR 0005: `TERM=xterm-256color` by default, optional `tessera` terminfo

**Status:** accepted · 2026-10-27

## Context

Every child process reads `TERM` to pick a terminfo entry, and the entry
decides what ncurses, vim, tmux and friends believe the terminal can do.
Tessera implements more than xterm: the kitty keyboard protocol, styled and
coloured underlines (SGR 4:n, 58), synchronized output (DEC 2026), focus
reporting, OSC 52. There is no way to advertise those through the stock
`xterm-256color` entry.

Three strategies are in use today. kitty ships `xterm-kitty` and sets it
unconditionally, which gives the best in-terminal experience and the worst
SSH experience: the first `ssh` to a host without the entry greets you with
`'xterm-kitty': unknown terminal type` and a broken `clear`. WezTerm and
ghostty ship their own entry (`wezterm`, `xterm-ghostty`) but default `TERM`
to `xterm-256color`; the custom entry is opt-in. iTerm2 ships no entry and
defaults to `xterm-256color`. All of them rely on the fact that modern
applications discover the interesting capabilities at runtime (kitty
keyboard via `CSI ? u`, synchronized output via `DECRQM 2026`, 24-bit colour
via `COLORTERM`) rather than from terminfo.

## Decision

1. **`TERM=xterm-256color` is the default** for every profile. It is the one
   value that works on every host, container and appliance a user will SSH
   into, and the capabilities that matter most are query-driven anyway.

2. **An optional `tessera` terminfo entry** lives in
   `shell-integration/terminfo/tessera.terminfo`. It is self-contained (no
   `use=xterm-256color`) so it compiles on a bare remote host, and it adds
   the user-defined capabilities that kitty, WezTerm, foot and tmux agree
   on: `fullkbd` (kitty keyboard protocol), `Smulx`/`Setulc`/`Su` (styled
   and coloured underlines), `Tc`/`setrgbf`/`setrgbb` (24-bit colour),
   `Sync` (synchronized output), `XF`/`kxIN`/`kxOUT` (focus events),
   `Ss`/`Se`, `Cs`/`Cr`, `Ms`, `smol`/`rmol`, bracketed paste and SGR mouse.
   The entry is named plainly `tessera`, not `xterm-tessera`: the `xterm-`
   prefix makes some applications assume xterm's exact behaviour, and the
   kitty keyboard protocol is deliberately not that.

3. **Installation is a script, not magic.** `just terminfo-install` compiles
   the entry into `~/.terminfo`; `just terminfo-ssh user@host` streams the
   source over ssh into the remote `~/.terminfo` (`tic -x -o ~/.terminfo
   /dev/stdin`), which needs nothing but `tic` on the other side; `just
   terminfo-check` compiles into a temp dir and fails if an extended
   capability is missing. The profile setting `term` (string, default
   `"xterm-256color"`) selects the value; a user who has installed the entry
   where they need it sets `term = "tessera"`.

4. **Environment the PTY must inject** (owned by `tessera-pty`, issue #16;
   this ADR only fixes the contract):

   | Variable | Value |
   |---|---|
   | `TERM` | the profile's `term`, default `xterm-256color` |
   | `COLORTERM` | `truecolor` |
   | `TERM_PROGRAM` | `Tessera` |
   | `TERM_PROGRAM_VERSION` | `CARGO_PKG_VERSION` of the core |
   | `TERMINFO` | the bundled `terminfo/` directory, only when `TERM` is `tessera` |
   | `TESSERA_SOCKET`, `TESSERA_SESSION_ID` | per ADR 0003 |

   `TERMINFO` lets the local shell find the entry without any install step;
   it is not forwarded by ssh, which is exactly why the install script
   exists. The app bundle ships the compiled entry under
   `Contents/Resources/terminfo/` (built by `tic -x` at package time, not
   checked in).

5. **Runtime queries stay authoritative.** Tessera answers `CSI ? u`,
   `DECRQM 2026`, XTVERSION and primary DA truthfully regardless of `TERM`.
   Applications that only trust terminfo gain nothing until the user opts
   into `tessera`; that is an accepted trade.

## Consequences

- Zero SSH breakage out of the box, same as iTerm2 and WezTerm. Users who
  want vim's `Smulx` curly underlines or tmux's `Sync` detection without
  querying run one `just` target per host.
- `colors#256` with `pairs#32767` is kept instead of `RGB` /
  `colors#0x1000000`, because the latter needs the ncurses 6.1+ extended
  number format and old `tic` binaries (macOS ships 6.0) truncate it.
  Direct colour is advertised through `Tc`, `setrgbf`/`setrgbb` and
  `COLORTERM` instead, which is what tmux and neovim actually check.
- The terminfo file is the second source of truth for what `tessera-term`
  implements. A capability may only be added to the entry once the emulator
  passes a test for it; the reverse is not required.
- Rejected: defaulting to `tessera` (kitty's SSH problem); reusing
  `xterm-kitty` (lies about identity, and applications keyed on that name
  assume kitty's graphics protocol); `tmux-256color`/`screen-256color`
  (different key tables); injecting the entry via a shell-integration hook
  on every ssh (surprising, and runs code on hosts the user did not ask us
  to touch). Kitty's `ssh` kitten approach remains possible later as a
  plugin.
