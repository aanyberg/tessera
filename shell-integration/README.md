# Shell integration

Scripts for zsh, bash and fish that emit FinalTerm OSC 133 prompt marks and
OSC 7 working-directory reports, which `tessera-shell-integration` decodes
into semantic marks. Lands in M5. They will be auto-injected per profile and
also installable manually.

## `terminfo/`

The optional `tessera` terminfo entry (see
[ADR 0005](../docs/adr/0005-terminfo-and-term.md)). Tessera sets
`TERM=xterm-256color` by default; the entry exists for people who want
terminfo-driven applications (vim, tmux, ncurses) to see the kitty keyboard
protocol, styled underlines and synchronized output without runtime queries.

```sh
just terminfo-check            # compile into a temp dir, list extended caps
just terminfo-install          # tic -x into ~/.terminfo
just terminfo-ssh user@host    # same, on a remote host, source piped over ssh
```

Then set `TERM=tessera` in the profile (or `export TERM=tessera` to try it).
