# Shell integration

Scripts for zsh (5.1+), bash (3.2+, so the macOS `/bin/bash` too) and fish
(3.2+) that tell Tessera where prompts, commands and their output start
(`FinalTerm` OSC 133 marks `A`, `B`, `C`, `D;<status>`), which directory the
shell is in (OSC 7, `file://<host><percent-encoded path>`), and, once at
startup, which script version is running
(`OSC 1337 ; ShellIntegrationVersion=1 ; shell=<name>`). The
`tessera-shell-integration` crate decodes them; the design is
[ADR 0021](../docs/adr/0021-shell-integration-injection.md).

| File | Shell |
|---|---|
| `zsh/.zshenv`, `zsh/tessera-integration` | zsh |
| `bash/tessera.bash` | bash |
| `fish/vendor_conf.d/tessera-shell-integration.fish` | fish |

## Automatic (the default)

Tessera loads the scripts into the zsh, bash and fish it starts without
installing anything: the scripts are compiled into Tessera and written to
`~/Library/Application Support/Tessera/shell-integration/<version>/` the
first time a shell needs them, and the shell is pointed at them through
its environment:

- **zsh**: `ZDOTDIR` points at our `zsh/` directory, whose `.zshenv` puts
  your `ZDOTDIR` back, sources your `.zshenv` and loads the integration;
  zsh then reads your `.zprofile`, `.zshrc` and `.zlogin` as usual. If you
  have none of `.zshenv`, `.zprofile`, `.zshrc` or `.zlogin`, Tessera leaves
  zsh alone so its new-user setup can run: `touch ~/.zshrc` turns the
  integration on.
- **bash**: an interactive non-login bash starts as `bash --rcfile
  <our script>`, which sources your `~/.bashrc` (or your own `--rcfile`)
  first. A login bash (Tessera's default) gets an exported `PROMPT_COMMAND`
  ending in a loader that sources the script at the first prompt, after
  your `.bash_profile` ran; the script takes the loader out again and stops
  exporting `PROMPT_COMMAND`, and a copy a prompt tool kept (starship) does
  nothing after that. A bash running a command (`-c`) or a script is left
  alone, and so is a login bash whose startup files assign
  `PROMPT_COMMAND`, make it an array (`PROMPT_COMMAND+=(...)`, oh-my-bash
  on bash 5.1+) or add to it both after a newline and after a `;`. A
  `--norc` or posix-mode bash (`--posix`, `-o posix`, `POSIXLY_CORRECT`,
  `SHELLOPTS` with `posix`) takes the loader, not `--rcfile`. On bash 3.2
  the script keeps `PIPESTATUS` for a command line that names it; the
  limits are in the script's header and ADR 0021.
- **fish**: our directory goes first in `XDG_DATA_DIRS`, so fish loads the
  `vendor_conf.d` script, which takes it out again. fish 4.0 and later mark
  prompts themselves (and 4.3 and later report the directory); the script
  then only sends the handshake.

Every injected shell gets `TESSERA_SHELL_INTEGRATION=enabled` (a login
bash gets the words in its loader instead). The scripts read it at the
first prompt, so your startup files can change it, and then unset it, so
shells you start inside a session (`exec zsh`, tmux) are not integrated
automatically.

### Turning it off

- Everywhere: `shell_integration = "off"` under `[terminal]` in
  `config.toml` (see [config.md](../docs/config.md)).
- For one shell: start it with `TESSERA_SHELL_INTEGRATION=0` in its
  environment, or set `TESSERA_SHELL_INTEGRATION=0` in your `.zshrc`,
  `.bashrc` or `config.fish` (read at the first prompt).
- Parts of it: add `no-cwd` (no OSC 7) or `no-prompt-mark` (no OSC 133) to
  the variable, e.g. `export TESSERA_SHELL_INTEGRATION="enabled no-cwd"`.
- fish: a file of your own named
  `~/.config/fish/conf.d/tessera-shell-integration.fish` replaces ours.

## Manual installation

For shells Tessera did not start (inside tmux or ssh to this machine, after
`exec zsh`), copy the files somewhere and source them yourself, at the
**end** of your startup file:

```zsh
# ~/.zshrc
export TESSERA_SHELL_INTEGRATION=enabled
autoload -Uz -- /path/to/shell-integration/zsh/tessera-integration
tessera-integration
unfunction tessera-integration
```

```bash
# ~/.bashrc
export TESSERA_SHELL_INTEGRATION=enabled
source /path/to/shell-integration/bash/tessera.bash
```

```fish
# ~/.config/fish/config.fish
set -gx TESSERA_SHELL_INTEGRATION enabled
source /path/to/shell-integration/fish/vendor_conf.d/tessera-shell-integration.fish
```

Loading a script twice installs its hooks once.

## What the scripts send

- `A` before each prompt and `B` after it, `C` when a command starts (never
  with the command line), `D;<status>` at the next prompt, and a bare `D`
  when a prompt was shown and no command ran (an empty line, Ctrl-C). No
  `D` before the first prompt. Tessera treats every `D` as the end of the
  command: it turns off the input modes (mouse, focus, cursor keys, kitty
  flags) and the alternate screen the command may have left on (ADR 0020,
  "Amendment: modes a dead program left on").
- OSC 7 at startup, at every prompt and whenever the directory changes
  (not for a subshell's `cd`). The host is the
  shell's host name reduced to `[A-Za-z0-9._-]`; the path is encoded like
  Terminal.app's (every byte outside `[/._~A-Za-z0-9-]` as `%XX`). A report
  is at most 4096 bytes (what Tessera accepts): a directory whose report
  would be longer is reported as its deepest ancestor that fits.

zsh: the marks go into `PS1`; a theme that redraws its prompt
asynchronously (powerlevel10k's instant prompt, say) can drop the `B` mark,
as with kitty.

bash hooks: `PROMPT_COMMAND` entries close commands and add the prompt
marks, and a `DEBUG` trap marks the start of the next command and then
removes itself (so it costs nothing while commands run; `$?` and `$_` are
kept). An existing `DEBUG` trap keeps running; with
[bash-preexec](https://github.com/rcaloras/bash-preexec) loaded, its hook
lists are used instead. Once loaded, `PROMPT_COMMAND` is no longer
exported, so shells started inside a session do not inherit our hooks.
On bash 5.1 and later a `PROMPT_COMMAND` that is an array (bash-preexec
0.6 or later makes one, atuin loads it; oh-my-bash appends an element) is
not integrated: the shell runs as without Tessera, with no marks
([#613](https://github.com/aanyberg/tessera/issues/613)).
Limits: a `PROMPT_COMMAND` entry added after
ours runs after the prompt marks for one prompt (it sees the command's `$?`
there but not its `PIPESTATUS`), then the script moves its own entry back
to the end, and from the next prompt on the entry sees both as without
Tessera; a `DEBUG` trap set after the script loaded is
replaced at the next prompt; and a command line that only runs a subshell,
`(cd x; make)`, is reported as a prompt with no command (a bare `D`; with
`set -T` or `extdebug` the subshell sends the `C` itself). An entry
appended with `PROMPT_COMMAND="$PROMPT_COMMAND; cmd"` keeps every mark
while `PS1` keeps our `B` mark (not with `shopt -u promptvars`).

bash with prompt tools, in a login shell:

- **starship**: it moves `PROMPT_COMMAND` into its own hook and rewrites
  `PS1` after the script loaded, so the first prompt and the first command
  are unmarked; every later prompt and command is marked.
- **bash-preexec** (0.5.0 to 0.7.0): marked from the first prompt on bash
  3.2 to 5.0; on 5.1 and later only 0.5.0, as 0.6.0 and later make
  `PROMPT_COMMAND` an array (not integrated).
- **oh-my-bash**: it adds to `PROMPT_COMMAND` as an array
  (`PROMPT_COMMAND+=(...)`), so a login bash that loads it is left alone, on
  every bash version: no marks (ADR 0021, B5h). Workaround: run a non-login
  bash (`/bin/bash -i` as the profile's command, or `bash` typed at the login
  shell), which takes the `--rcfile` route and is marked from the first
  prompt on bash 3.2 to 5.0, also with bash-preexec (on 5.1 and later its
  array is not integrated); or source `bash/tessera.bash` yourself at
  the end of `~/.bashrc`, after oh-my-bash.
- **A startup file that assigns `PROMPT_COMMAND`** (rather than adding to
  it): Tessera reads `.bash_profile` (or `.bash_login`, or `.profile`),
  `.bashrc` and the files they source by a literal `~/`, `$HOME` or absolute
  path, without running them, and starts such a login bash with no
  integration, as if Tessera did not know it. The same happens when one of
  those files is not a regular file, is over 1 MiB, cannot be read in about a
  second, or when they source more than about 30 files. Add to the variable
  instead (`PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND$'\n'}history -a"`),
  which marks everything but the first command, or source the script
  yourself. An assignment Tessera cannot see (behind a computed `source`
  path, in `eval`, or in `/etc`) still removes the loader and stays exported,
  so a child bash or `sh` that lacks a function it names prints `command not
  found` at its prompts.

powerlevel10k is zsh-only; its instant prompt is the zsh limit above.

## Checking the scripts

```sh
just shell-integration-check   # zsh -n, /bin/bash -n, fish -n, shellcheck, a startup smoke run
cargo nextest run -p tessera-shell-integration --test live   # real shells in a PTY
```

Changing a script means bumping `SCRIPTS_VERSION` in
`crates/tessera-shell-integration/src/scripts.rs` and recording the new
digest in `crates/tessera-shell-integration/tests/scripts.rs` (the test
says what to write).

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
