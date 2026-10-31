# #10 spec notes (reports_spec.txt, osc_color_spec.txt) — researcher, 2026-10-31

Sources read in full this run: xterm ctlseqs.txt and xterm-snapshots master (charproc.c, misc.c, version.h, xterm.man; patch 412),
vim `term.c`, tmux `tty-keys.c` / `tty.c`, neovim `tui.c`, fish `terminal.rs`. kitty, WezTerm, Ghostty, Alacritty, iTerm2 statements
are marked "(from memory)" in the fixtures; DEC STD 070 / VT510 manual were not re-fetched (rows cite them for the numbering only).

## (a) Identity

Recommendation: **not xterm-compatible by identity**, but xterm-shaped replies.
- XTVERSION `DCS > | Tessera <ver> ST` (space, no parenthesis: tmux matches `XTerm(`, `kitty(`, `foot(`, `iTerm2 `, `WezTerm `,
  `mintty ` prefixes and would enable features for them; an unknown name makes tmux fall back to terminfo + DA1).
- DA1 `CSI ? 6L ; 22 c` (L = `conformance_level()`), level 1 `CSI ? 1 ; 2 c`; only implemented features advertised (tmux reads 4, 21,
  28, 52 and would enable sixel/margins/rect-fill/clipboard-read; none exist yet). Default level 4 -> `CSI ? 64 ; 22 c`.
- DA2 `CSI > 1 ; <Pv> ; 0 c`. Never Pp 77/84/85 (tmux: mintty/tmux/urxvt) and not Pp 1 + Pv 4000..4009 (vim: kitty).
  **Pv is the open question (D-DA2PV):** vim reads it: <95 no drag, 95..276 X10 drag, >=277 SGR mouse, >=2500 "not an xterm". Recommended
  `10000 + minor*100 + patch` (0.1.0 -> 10100).
- DA3 `DCS ! | 00000000 ST` at level >= 4 (xterm DECRPTUI).

Extra probes after the identity replies (sources above):
- **vim**: `CSI > c` (t_RV) first; with Pv >= 141 and `xtermcodes` it then sends **XTGETTCAP `DCS + q ... ST`** (we stay silent: not
  blocking, vim moves on). Also `CSI ? u` (kitty), `CSI ? 12 $ p` (DECRQM blink), `DCS $ q SP q ST` (cursor style), `CSI 6 n` (width probe),
  `CSI 22;2 t`/`23;2 t` and `22;1 t`/`23;1 t` (title stack), `CSI 13 t` (only for `getwinpos()`; silent by D-WIN2 -> vim waits its
  timeout), `OSC 10;? / 11;?` with ST (background).
- **tmux**: `CSI c`, `CSI > c`, `CSI > q`, `CSI ? 2026 $ p`, OSC 10/11 queries (ST). All three identity replies must be well-formed or tmux
  retries/times out.
- **neovim**: `CSI ? u CSI c` (kitty then DA1 sentinel), `OSC 11;? BEL CSI 5 n`, DECRQM ?69 ?2026 ?2027 ?2031 ?2048 (needs a reply for EACH,
  0 = unsupported), `CSI 0 m CSI 4:3 m DCS $ q m ST` (reply must contain `4:3`), `CSI 22;0 t`/`23;0 t`, final `CSI c`.
- **fish**: `CSI ? u`, `CSI 0 c`, `CSI > 0 q`, `CSI 6 n`, `OSC 11;? ST`, `CSI ? 2031 h`.
- fzf (`CSI 6 n`), less, htop: from memory only; no probe beyond CPR expected. The `probe_replies` live test will show it.

## (b) conformance_level() 5

xterm id 420 rejects `CSI 65 " p` (`Pl <= 60 + terminal_id/100`), so xterm never reports level 5; Tessera stores it (#92). Recommendation
**D-L5**: report it as a VT525 (what xterm does with decTerminalID=525): DA1 `CSI ? 65 ; 22 c`, DECRQSS `" p` -> `65;1" p`. DA2 does not follow the
level (xterm: it follows the resource). Alternative (clamp to 4) is cheaper but makes the stored level lie.

## (c) 14t / 16t with an unknown cell size

Decision **D-CELLPX**: silent until `set_cell_pixels(w, h)` was called with both > 0 (also `0x0` and a partial zero = unknown). A zero reply
divides by zero in clients (chafa, timg, yazi); the window is normally laid out before the child writes. Cost: an image tool probing
before the first frame waits its timeout once. `18t` and `19t` never need pixels. `14;2t` (frame included), `15t`, `13t`, `11t`, `20t`, `21t`
silent (D-WIN2, security). Pixel counts saturate at 65535.

## (d) Where xterm differs from its spec (or Tessera from xterm)

| Row | xterm source says | ctlseqs / spec says | Fixture |
|---|---|---|---|
| DSR ?15 | `CSI ? 13 n` (no printer), level >= 2 | `?10n` ready / `?11n` not ready | 13 (DSR17) |
| DECXCPR | adds page `;1` at level >= 4 (or >= 3 with id >= 330) | `CSI ? r ; c R` | with page (DSR13) |
| DA1/DA2 vs DECSCL | follow resource `decTerminalID`, ignore DECSCL | VT510: DA1 follows the level | DA1 follows level (D-DA1) |
| DECRQSS `" p` at level 1 | silent (`break`) | "invalid" -> `DCS 0 $ r ST` | silent (RQ22) |
| DECRQSS malformed (`DCS $ m`) | sends **CAN (0x18)** to the host | — | silent (RQ29) |
| DECRQSS `m` underline | prints `;4` only | kitty `4:3` | `4:3` (neovim needs it) |
| DECRQM ?2026 | not known (0) | Contour spec: 1/2 | answers 1/2 |
| DECRQM SRM 12, DECANM ?2, ?4, KAM 2 | 1 / 1 / 2 / 2 (changeable) | — | 3 / 3 / 4 / 4 (we cannot change them) |
| XTVERSION | `XTerm(412)` | "text" | `Tessera <ver>` |
| 22/23 t beyond depth 10 | ring buffer: 11th pop returns a stale slot | "1 through 10" | drop oldest, 11th pop no-op |
| 14t/16t/18t allowed | yes by default (only GetIconTitle, GetWinTitle, GetChecksum... disallowed) | — | same; 14;2/15/19 differ (D-19T) |
| 16t gate | checks the *screen size in chars* permission, not its own (copy-paste quirk) | — | n/a |
| OSC 10/11/12 under DECSCNM | reports the opposite colour | — | not swapped (D-REVERSE) |
| RIS / DECSTR and colours | both reset the 256-palette (ReallyReset common part), neither resets fg/bg/cursor | brief: "RIS drops overrides" | RIS drops all, DECSTR none (D-RESET) |
| `#rgb` | left-justified: `#f00` = `#f00000` | X "Color Strings" | doubled nibble: `#ff0000` (D-HASH) |
| OSC 4 index parse | `atoi`: `1x` = 1, empty = 0 | — | strict digits |
| OSC 110..119 with data | ignored | — | ignored |
| OSC 4 pair after an error | loop breaks | — | breaks (D-STOP4) |
| OSC 4 with > 7 pairs | all | — | cut by tessera-vt (16 pieces) |

## Decisions needed from the lead (all have a recommended default in the fixtures)

1. **D-DA2PV** the number in DA2 (affects vim's mouse mode). 2. **D-HASH** `#rgb` CSS doubling vs xterm-literal. 3. **D-RESET** RIS vs DECSTR
and colours (the `// #10` in soft_reset). 4. **D-NAMES** reject X11 names in this task (follow-up) or ship a table. 5. **D-L5** level 5 reporting.
6. **D-CELLPX** silence vs zero. 7. **D-RQSS** `4:3` in the SGR reply. 8. **D-TSTACK** supersedes ADR 0022's D-TSTACK push/pop wording.
9. **D-VTBELL (scope!)**: `tessera-vt` `Action::Osc` drops `bell_terminated` (lib.rs L278), so the BEL-vs-ST echo is impossible today; the
colours brief (and DECRQSS-free reply path) needs a tessera-vt change the brief does not list. Likewise the CAN/SUB-aborted DCS/OSC cannot be
told from a finished one (rows RQ34, OQ24/25 record "answered").

## Things the issue/briefs get wrong or leave out

- 10-reports "Done when" lists `?53` among answered DSRs; xterm does not answer it (fixture: silent). It also says "title reports 20/21 t stay
  unanswered" (kept) but ADR 0022 D-TSTACK says pops re-emit nothing (superseded by D-TSTACK here).
- 10-reports: "2026 sync output must answer" is right but xterm itself answers 0; the authority is the Contour spec / tmux / neovim.
- 10-colors: "RIS drops overrides" matches kitty-style terminals, not xterm (which keeps fg/bg/cursor); "X11 names per fixture decision" -> rejected
  here; scope omits `crates/tessera-vt`.
- Row types `check_table` must count: 301 `###` headings in total (196 + 105); rows marked "Generated by the test" (TS14, RP09, OZ01..OZ03)
  have no literal bytes. Step directives the harness must add: `set cell_px`, `set title`, `set theme`, `set size`; placeholders `<VERSION>`, `<PV>`.
- Rows `TS09`, `RP03`, `OI04` were rewritten during research; the final versions are the ones in the files.

## Suggested "Done when" amendments

**10-reports**
- Replace "DSR ... ?53/?55/?56 ..." by "?6, ?15 (-> `?13n`), ?25, ?26, ?55, ?56, ?75, ?85 with xterm's level gates; ?53, ?62, ?63 silent".
- Add: "DA1 follows `conformance_level()` (level 1 `?1;2c`, 2..5 `?6L;22c`), DA2/XTVERSION per D-ID/D-DA2PV, DA3 `DCS ! | 00000000 ST` from level 4".
- Add: "`Terminal::set_cell_pixels` zero or never-set = unknown: 14t/16t silent".
- Add: "SGR DECRQSS reports underline styles as `4:n` (neovim probe)".
- Add: "a DECRQM reply for every query, 0 for unknown modes (neovim matches replies by order)".
- Add the test-harness steps (`set cell_px`, `set title`) and the `<VERSION>`/`<PV>` placeholders.

**10-colors**
- Add `crates/tessera-vt` to scope: expose `bell_terminated` on `Action::Osc`; amend ADR 0022 wording.
- "RIS drops every override; DECSTR/DECSCL keep them" (or the lead's D-RESET choice), generation bumps only on an effective change.
- "Spec forms: `rgb:` 1..4 digits, `#` 3/6/9/12 digits, nothing else; names, `rgbi:`, CIE ignored" (or ship names).
- "OSC 4/104 stop at the first malformed pair/index; chain 10;..;.. stops at 12; >7 OSC 4 pairs cut by the parser."
- "Replies never depend on DECSCNM."
