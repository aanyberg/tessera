# Selection, OSC 8 and cmd-click: researcher notes (issue #30, 2026-10-31)

Companions: `selection_spec.txt` (this directory, 116 rows incl. 37 URL rows), `crates/tessera-term/tests/fixtures/hyperlink_spec.txt`
(39 rows), `apps/macos/Tests/TesseraTests/Fixtures/selection_gestures.txt` (40 rows). Everything below was read from source or docs
(fetched 2026-10-31), nothing was run. "iTerm2 does X" = the master source says so; no iTerm2 binary was available.

## Sources
- OSC 8 spec (Egmont Koblinger): https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda
- iTerm2 docs: https://iterm2.com/documentation-preferences-general.html (Selection),
  https://iterm2.com/documentation-smart-selection.html
- iTerm2 source (gnachman/iTerm2 master): `sources/Selection/iTermSelection.m`, `iTermSelectionScrollHelper.m`,
  `sources/ContentAnalysis/iTermWordExtractor.m`, `iTermTextExtractor.m`, `sources/TerminalView/PTYMouseHandler.m`,
  `sources/Settings/iTermPreferences.m`, `sources/Drawing/iTermTextDrawingHelper.m`, `sources/VT100Screen/*`
- kitty: https://sw.kovidgoyal.net/kitty/conf/ ; `kitty/line.c`, `kitty/unicode-data.h`, `kitty/screen.c`, `kitty/hyperlink.c`
- WezTerm: https://wezterm.org/config/lua/config/hyperlink_rules.html, `selection_word_boundary`; `wezterm-escape-parser/src/hyperlink.rs`
- Ghostty: `src/config/url.zig`, `src/terminal/osc/parsers/hyperlink.zig`
- VTE: `src/vteseq.cc` (set_current_hyperlink), `src/vtedefines.hh`
- Alacritty: `alacritty_terminal/src/term/mod.rs` (bounds_to_string, SEMANTIC_ESCAPE_CHARS)
- Not read: xterm selection code, Terminal.app (closed source; behaviour quoted only where the briefs name it).

## Answers to the lead's questions

### Does iTerm2 extend highlights to the window edge past the last column? (decides #124)
**No, by default.** Selection is applied per cell (`selectedIndexes` into the background runs, iTermTextDrawingHelper.m L441-452).
The side margins are painted by `drawMarginsForLine` (L955-970) with `colorForMarginsUsingDominant:NO` = the profile's default
background. Only when the advanced setting `extendBackgroundColorIntoMargins` is on AND the screen is in soft alternate-screen mode
(L645) is the first/last background RUN's colour carried into the margin. So on the primary screen the highlight stops at the last
column and both paddings stay clear: **D-EDGE option (a) matches iTerm2.** No override needed.

### URL detection rules (for `url.rs`, no regex dependency)
- U1 Scheme: `http`, `https`, `ftp`, `file`, `mailto`, ASCII case-insensitive, must not be preceded by an alphanumeric, `+`, `-` or `.`
  (J26). `http`/`https`/`ftp`/`file` need `://`; `mailto` needs `:`. Other schemes (ssh, git, tel, magnet, javascript, ...) are NOT detected
  even though kitty (`url_prefixes`), Ghostty and WezTerm detect more (J27).
- U2 Case: return the text as typed (J25).
- U3 Characters: the candidate runs from the scheme to the first character that is whitespace, a control, or one of `<>"` backtick `\` `^`
  `{` `}` `|`; allowed are RFC 3986 characters (`A-Za-z0-9-._~:/?#[]@!$&'()*+,;=%`) and any non-ASCII alphanumeric (IRIs, J30).
  The ideographic full stop and other non-alphanumeric Unicode ends it (J29).
- U4 Trailing trim, repeated until stable: strip a final `.` `,` `;` `:` `!` `?` `'` `"` (J02, J03, J11, J31, J32). NOT stripped: `/` `-` `_` `~` `=`
  `&` `%` `*` `)` `]` (unless U5) (J13, J14). kitty strips every trailing punctuation class except `/ & - ) ] } *` (`can_strip_from_end_of_url`);
  Tessera uses a smaller, fixed set so `a=&b=` and `x_` survive. WezTerm's bare rule would drop a trailing `=` (J14 ambiguity).
- U5 Brackets: after U4, strip a final `)` / `]` / `>` only while the candidate contains more of that closer than of its opener (J04-J08, J15, J16).
  Wikipedia-style `Foo_(bar)` stays (J05); `(url)` loses the outer paren (J06). Unbalanced openers are kept (J16). kitty instead stops at the first closer
  when the URL is preceded by the matching opener (`get_url_sentinel`), which truncates Wikipedia links inside parentheses: documented difference.
  Ghostty's header states exactly the two cases this rule solves.
- U6 Host: for http/https/ftp the first character after `://` must be a host character (not `/`, `?`, `#`, end): rejects `http://`, `http:///x` (J17, J18, J37).
  `file` allows an empty host (J19, J20). Open-time allowlist is separate (below).
- U7 mailto: `mailto:` + text + `@` + text. Bare emails are NOT linked (J22, J23); WezTerm rule 6 and iTerm2 smart selection link them.
- U8 Soft wraps continue a URL across rows; a hard line end stops it (I03, I04). kitty needs `next_line_starts_with_url_chars`; Tessera works on the logical line.
- U9 Cap: a candidate longer than 2083 chars is not a link (J35, J36), the OSC 8 URI cap, bounding per-hover cost.

### Which schemes may cmd-click open (D-OPEN)
Draft confirmed with one addition: **http, https, mailto, ftp; `file` only when the host is empty, `localhost` or this Mac's host name** (gist: "match the
hostname against the local host ... localhost and an empty hostname count as local"). Everything else is ignored and logged. Reasons: the gist
suggests whitelisting well-known schemes and asking for others; Koblinger himself suggested `file`, `http`, `https` as the minimum, so `ftp` and `mailto`
are the lead's additions (both are read-only hand-offs to a registered handler). Apply the SAME rule to OSC 8 and detected URLs. Compare the host
case-insensitively and ignore a trailing dot; percent-decode nothing before the scheme check; parse with `URL(string:)` and reject if it returns nil
(a URI with raw bytes needs percent-encoding first, L16). Rows: gestures L06-L10.

### Hover tooltip (D-HOVER-TIP)
The gist's security section recommends showing the URI before opening ("showing the URI first"). Of the references read: VTE emits a hover-URI signal
for the host application to show (vteseq.cc L261-263 `emit_hyperlink_hover_uri_changed`), kitty has `show_hyperlink_targets` (default **never**, kitty conf page).
I did NOT verify iTerm2 or WezTerm tooltips (not in the files read). So "every reference shows the target" is not shown => **no tooltip in 0.1.0** per the brief's
own condition. D-RENDER (no default underline for OSC 8 text) is consistent with VTE/kitty source (`OPT(underline_hyperlinks) == UNDERLINE_ALWAYS` is an opt-in in kitty;
its default value was not re-fetched).

## Decisions needed from the milestone lead
Recommended choice first. Rows that depend on a decision name it.

- **D1 whole-cell vs half-cell drag.** iTerm2 snaps to half cells (a drag that starts in the right half of a cell begins at the next cell). Recommend
  whole cells and "extend only when the pointer is on a different cell" (A03/A04, gestures P03/P04).
- **D2 trailing blanks.** Tessera's `Cell::BLANK` is `' '` (no NUL state), so written trailing spaces cannot be told from never-written cells. Recommend trim
  all trailing spaces of every hard line end (not at soft wraps, H05; not inside a block row's interior). iTerm2 and kitty (`strip_trailing_spaces never`)
  keep written spaces: documented deviation.
- **D4 double-click on blank space past the line's text** selects nothing (C09). iTerm2's Null-class path was not conclusive.
- **D5 shift-click is a new core call.** Brief API has only begin/extend. iTerm2 shift-click moves the **nearer endpoint** (tie: start) and keeps
  mode and block-ness (E01-E08); the brief draft says "keeps the anchor", which differs when the click lands before the start. Recommend adding
  `App::select_adjust(id, Point)` + `tessera_session_select_adjust(app, id, col, row)`; no-op without a selection (E05).
- **D6 selection clearing is stricter than iTerm2.** iTerm2's source clears on SD (`terminalScrollDown`), clear screen (`terminalClearScreen`) and width
  change (`+Resizing.m` L121) and was not seen clearing on ordinary overwrites (only call sites in two of its files were read). D-CLEAR (clear on a CHANGE of a
  selected live row) keeps Copy honest. **D6b:** compare row CONTENT, not damage, so a repainting full-screen program does not clear it every frame (G05).
  Resize clears on any resize (G08; iTerm2 only confirmed for width).
- **D7 tabs copy as spaces** (H01). iTerm2 and Alacritty copy a real tab; the grid keeps no tab characters.
- **D8 all trailing newlines from blank rows at the end of a selection are dropped** (H03, F03). Alacritty drops one; iTerm2 drops the last terminal newline (CopyLastNewline off).
- **D9 smart selection (iTerm2 quad-click) deferred;** click 4+ behaves as triple click (gestures P08).
- **D10 autoscroll pace.** iTerm2: first tick 100 ms, each tick x0.95 (floor 1 ms), lines per tick = clamp(elapsed/delay, 1, 100), the endpoint moves to the revealed
  edge row at the pointer's column, independent of how far outside the pointer is (IT-scroll L39-L100). Recommend copying it with a floor of 10 ms and a fake clock in
  tests (gestures P16-P19).
- **D11 Copy of an empty selection** (Select All on an empty terminal, block over blanks) leaves the pasteboard alone (gestures K04).
- **D12 not in 0.1.0:** cmd-click on file paths (iTerm2 semantic history), cmd-drag non-contiguous selection, text drag-and-drop of a selection, option-click moves cursor
  (iTerm2 default YES). Cmd-drag does nothing (gestures L07, L11).
- **D13 first-mouse click** only activates the window (AppKit default). iTerm2 has a cmd-click exception (gestures M02).
- **D14 bare emails** are not linked (J23).
- **D15 URL cap** 2083 chars (J35/J36).
- **D-G1 block selection modifier. The issue/brief say "option-drag block"; iTerm2's rectangular modifier is Command+Option** (`kRectangularSelectionModifiers`,
  PTYMouseHandler.m L29-30; plain Option is an ordinary selection there and Option-click moves the cursor). Terminal.app and WezTerm use plain Option; kitty
  and Alacritty use Ctrl-based chords. Recommend: block when Option OR Command+Option is held at mouseDown (gestures P12-P14). `#12` (wave 9) will decide Option vs mouse
  reporting; Option is the macOS "bypass reporting" key in iTerm2 and Terminal.app.
- **D-L1..D-L5 (OSC 8)** recommended in `hyperlink_spec.txt` footer: params without `=` ignored (L05); empty URI always closes (L10); URI over 2083 bytes closes the active link,
  id over 250 bytes is dropped (L13, L14); no-id links are identified per OSC 8 open (L33); DECSTR and screen switches clear the active link (L41, L42).

## Where the issue/briefs are wrong or need correcting
1. **"option-drag block"**: iTerm2 uses Cmd+Option (D-G1).
2. **30-core: "shift-click extend keeps the anchor"**: iTerm2 moves the nearer endpoint (D5).
3. **30-core: tentative word chars `/-+\~_.`** are right (iTermPreferences.m L751) BUT iTerm2's class model also makes an other-class character (e.g. `,`) select exactly one
   character on double-click (C04), and `: = @ ? & % #` are NOT word characters (C05), unlike kitty.
4. **30-links: "invalid UTF-8/control bytes in URI rejected"**: control bytes are already dropped one layer down by the OSC string rules (osc_spec.txt header item 3, L17); only
   invalid UTF-8 is a rejection (L15).
5. **30-links: "link not applied to cells written by ECH/EL/ED/ICH"** is right, but note the ACTIVE link is not closed by an erase (L22-L25).
6. **30-core: "copy_on_select off"** is a deliberate deviation: iTerm2's default is ON (`CopySelection` YES).
7. **30-core perf row on selection "head alone <= 1.05x"** is unaffected; no spec evidence either way.
8. Issue text #30 (comments): nothing wrong; "Select All, Copy" matches.

## Suggested "Done when" amendments
### 30-core
- Replace the shift-click clause with: "shift-click (`select_adjust`) moves the endpoint nearer the click (tie: the start), keeps granularity and block-ness, is a no-op
  without a selection (selection_spec E01-E08)". Add `App::select_adjust` and `tessera_session_select_adjust` to the FFI list and to `scripts/ffi-c-test.sh`.
- Add: "double-click on a character outside the word set selects exactly that character; on whitespace selects the run; past the end of the text selects nothing (C04, C02, C09)".
- Add: "text extraction drops ALL trailing blank lines of a selection (H03); PAD and spacer cells emit nothing (B07, H09)".
- Add: "clear rules compare selected-row CONTENT, not damage (G05)".
- URL clause: replace the one-line trim rule with U1-U9 above; "every `layer: url` row of the fixture passes; proptest: detect never panics, every span is within the line, and the span's text starts with a supported scheme".
- `link_at` rows I01-I08 (wrapped URL, scrolled view, wide characters) belong in `tests/selection.rs`.
- Fixture count: "all 116 rows pass (79 core, 37 url)".
### 30-links
- OSC 8 clauses: replace "URI length cap (2083 or 8192?)" with "URI > 2083 bytes closes the active link, id > 250 bytes is dropped (L13, L14)"; "invalid UTF-8 rejected, C0 bytes already dropped by the OSC layer (L15, L17)";
  add "params without `=` ignored (L05)", "empty URI always closes (L10)", "semicolons inside the URI kept (L07)", "missing second `;` ignored (L11)".
- Identity: "cells of one OSC 8 open (no id) or sharing id+URI are ONE link entity, also when non-adjacent (L28-L31, L33)".
- Reset: "RIS, DECSTR and alternate-screen switches clear the ACTIVE link; cells already written keep theirs (L40-L42)".
- Storage cap: keep the proposed 65,536 live links (kitty `UINT16_MAX`; VTE (1<<20)-2) and add a unit test for the cap and for ignored-and-logged-once.
- Fixture count: "all 39 rows pass".
### 30-app
- Gesture rows: "all 40 rows in selection_gestures.txt pass except `needs: osc8` rows until 30-links merges" (L08-L10 need OSC 8).
- Replace "option-drag block" with "block selection when Option, or Command+Option, is held at mouseDown; the mode does not change mid-drag (P12-P14)".
- Add: "double-click selects a word, triple/quad-click a whole logical line, quad-click is NOT smart selection (P05, P06, P08)".
- Autoscroll: "tick 100 ms x0.95 per tick (floor 10 ms), lines per tick = clamp(elapsed/delay, 1, 100), driven by a fake clock in tests, continues without pointer motion, stops on mouseUp (P16-P19)".
- Add: "Cmd-click that is not on a link, and Cmd-drag, do nothing (no selection, nothing sent to the PTY) (L07, L11)"; "Cmd-hover re-hit-tests on flagsChanged at the last pointer location (L05)".
- Add: "Copy of an empty selection leaves the pasteboard unchanged (K04)"; "no copy-on-select (K05)".
- D-OPEN: as above, with host comparison case-insensitive; "javascript:", "vscode:", remote-host `file:` do nothing (L08-L10).
