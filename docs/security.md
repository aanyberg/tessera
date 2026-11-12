# Security and resource model

What Tessera trusts, what it defends, and the rules every change keeps. Critics check a branch against the rules
below (S1-S9) and the adversarial tester attacks them. A branch that breaks a rule, or makes an existing gap worse,
gets a BLOCKER ("a safety problem"); a gap the branch does not touch is listed under "Known gaps" and never blocks
it. A change that adds an attack surface or moves a bound updates this file in the same PR.

## Trust boundaries

| Source | Trust | Why |
|---|---|---|
| Bytes from the child (PTY output) | **untrusted** | Anything can print: a remote host over `ssh`, a `cat`ed file, `curl` output, a compromised dependency's build log. |
| Text the user pastes | semi-trusted | The user chose to paste it, but it may come from a web page with hidden control characters. |
| Imported themes (`.itermcolors`, `tessera-devtool theme import`) | **untrusted** | Downloaded files. |
| `config.toml`, theme files, shell integration scripts on disk | trusted, parsed defensively | Only the same user can write them; a same-user attacker already owns the shell. Parsers still cap sizes so a broken file cannot hang the app. |
| Control socket clients, plugins (backlog: #53, #48, #58) | **untrusted** until consent | ADR 0002 capabilities and ADR 0003 consent; not built yet. |
| Same-uid local processes | trusted | Out of scope: they can already `ptrace` the shell. |

Assets, in order: the user's input stream (an attacker who can inject bytes into it runs commands), the clipboard,
terminal content (confidentiality: it can hold secrets), and availability (memory, CPU, the main thread).

## Attack surfaces

A change touches an attack surface when it changes any of these. The planner names the rules such a task keeps,
the adversarial run for it counts as risky (`docs/software-factory.md`, "Adversarial cost levers"), and it is never
a low-risk change.

| Surface | Code | Rules |
|---|---|---|
| Escape sequences from the child: parser, OSC, DCS, replies, titles, clipboard, marks, cwd | `tessera-vt`, `tessera-term`, the session event path in `tessera-app-core` | S1, S2, S3, S5, S6 |
| Paste and clipboard, Copy with Formatting (child text turned into HTML and RTF, ADR 0042) | `tessera-app-core` input and paste, `rich_text.rs`, the app's pasteboard bridge | S4, S5 |
| Child-controlled text in the UI: titles, process names, URLs | `tessera-app-core` (`url.rs`), `apps/macos` | S3 |
| The C ABI | `tessera-ffi`, the TesseraKit wrapper | S7 |
| Spawning the child: environment, fds, signals, foreground job | `tessera-pty`, `tessera-shell-integration` | S5, S7 |
| Parsing and writing files: config, themes, imports. Settings write-back (ADR 0040) writes `config.toml` and `themes/` only through a new temporary file in the target's directory, renamed (or hard-linked, for a theme) only while the file is byte for byte what was read, through a symlink to its target, with the mode kept; a hard-linked, read-only or non-regular file is not written; every edit passes the lenient loader without a new warning first | `tessera-config` (`edit.rs`), `tessera-devtool`, the config exports in `tessera-ffi` (`config.rs`) | S3, S5, S7, S8 |
| Terminal text for assistive clients (ADR 0041): the view's text, cursor and selection as an `AXTextArea`, announcements of new output | `tessera-app-core` (`accessibility.rs`), `tessera-ffi` (`accessibility.rs`), `apps/macos/Sources/Tessera/TerminalSurfaceView+Accessibility.swift` | S3, S5, S6, S8 |
| The private WindowServer call `CGSSetWindowBackgroundBlurRadius`, resolved with `dlsym` (ADR 0045): its input is the clamped `blur_radius` (an integer, 0..=64, see "Current bounds") and a window number; no child-controlled data reaches it, and the log carries the radius only | `apps/macos` (`WindowBlurRadius.swift`) | S7, S8 |
| Logging anywhere | all crates and the app | S8 |
| New surfaces: links, control socket, plugins, images, triggers | ADR first | S9 |

## Rules

- **S1. No echo into the input.** Replies to the child's queries (ADR 0024) never copy bytes the child sent: they
  hold fixed strings, terminal state and numbers parsed from the request (DECRQM echoes the mode number). An
  over-long or unknown request gets a fixed answer or none.
- **S2. Nothing is read back.** The clipboard is never reported (OSC 52 `?`), the title is never reported
  (`CSI 20/21 t`), and no new query returns user data without an ADR that argues it.
- **S3. Untrusted text in the UI is sanitised.** Titles drop bidi, C0, C1 and DEL characters and are capped
  (ADR 0022); process names are sanitised before display (ADR 0031). Any new place that shows child-controlled text
  (tab, window, notification, menu, dialog) does the same.
- **S4. Paste is sanitised.** Every C0 control except TAB, CR and LF, plus DEL and the C1 range, is removed before
  brackets are added, so a paste cannot end bracketed paste early (`crates/tessera-app-core/src/input.rs`).
  Without bracketed paste, line breaks become CR. Only `tessera_session_write` is raw, by design, and the app never
  feeds it clipboard text.
- **S5. Every buffer filled from untrusted input has a named cap.** A new queue, buffer, stack, table or cache fed
  by the child, a file or a client states its cap as a constant, and a test floods it past the cap. Unbounded
  growth is a BLOCKER even when "nobody would send that".
- **S6. Per-byte work is bounded.** Every callback or event emitted per input byte (bell, title, clipboard, marks,
  replies) is bounded in rate and memory, and an expensive sequence does not hold the terminal lock long (#117).
- **S7. Memory safety.** `unsafe` only in `tessera-pty` and `tessera-ffi`, each block with a `// SAFETY:` comment;
  the counting test allocators in `crates/tessera-render/tests/alloc.rs`, `crates/tessera-grid/tests/reset.rs`,
  `crates/tessera-term/tests/osc8_alloc.rs`, `crates/tessera-term/tests/mouse_alloc.rs`,
  `crates/tessera-term/tests/screen_clears_alloc.rs` and
  `crates/tessera-app-core/tests/keybindings.rs` are the exceptions (CONTRIBUTING.md); `scripts/verify-branch.sh`
  checks the rule mechanically from `scripts/unsafe-exceptions.txt`, which must list exactly these files. Every FFI export that can panic runs under `ffi_guard!` (`catch_unwind`);
  every pointer and length argument is checked; sessions cross the boundary as ids, never pointers.
- **S8. No terminal content in logs.** Logs carry lengths, ids, parameters and paths, never output, input, titles,
  clipboard text or environment values. Child-controlled bytes that must be logged are capped and escaped.
- **S9. New surfaces need an ADR first.** Opening links, the control socket, plugins, images, triggers and anything
  that acts on child output gets its trust decision (consent, capability, allow list) in an ADR before the code.

## Current bounds

| Input | Cap | Where |
|---|---|---|
| OSC string in the parser | **none** (#213) | `crates/tessera-vt/src/lib.rs` |
| OSC parameters | 16 | `crates/tessera-term/src/osc.rs` |
| Title | 1024 bytes, dropped whole when longer; stack depth 10; last title per chunk | `crates/tessera-term/src/osc.rs`, `crates/tessera-term/src/reports.rs`, `crates/tessera-app-core/src/session.rs` |
| OSC 52 write | 1 MiB decoded, strict base64, UTF-8; last write per chunk; 10 pasteboard writes per second app-wide, latest held | `crates/tessera-term/src/osc.rs`, `crates/tessera-app-core/src/session.rs`, `apps/macos/Sources/TesseraKit/CallbackBridge.swift` |
| OSC 7 / OSC 1337 | 4096 bytes | `crates/tessera-term/src/lib.rs` |
| Remote host in the title (#205, ADR 0022) | host 253 bytes (`MAX_REMOTE_HOST_BYTES`, `TESSERA_MAX_REMOTE_HOST_LEN`), dropped whole when longer (shown `⇢ ssh`); control, bidi-control and line-separator characters removed (S3); one decision per chunk, `SessionEvent::Remote` only on change (S6), one pending state per session in the bridge; the `ssh` probe (foreground job, else session leader; names at most 64 bytes, compared, never shown) at most once per second per session, on output only; at most 4 local host names remembered per session (`MAX_LOCAL_HOST_NAMES`); logs carry the host's length only (S8) | `crates/tessera-app-core/src/remote.rs`, `crates/tessera-app-core/src/session.rs`, `crates/tessera-ffi/src/callbacks.rs`, `apps/macos/Sources/TesseraKit/CallbackBridge.swift` |
| DCS | streamed in 4 KiB chunks; DECRQSS request 64 bytes | `crates/tessera-vt/src/lib.rs`, `crates/tessera-term/src/dcs.rs` |
| Grapheme cluster | 32 scalars | `crates/tessera-grid/src/grapheme.rs` |
| Grid | 2048 columns × 1024 rows | `crates/tessera-app-core/src/view.rs`, `crates/tessera-ffi/src/session.rs` |
| Scrollback | `[terminal] scrollback_lines` (default 10 000), applied when a session starts; capped at `Terminal::MAX_SCROLLBACK_LINES` (10 000 000); a width change re-wraps the whole history under the terminal lock (~0.2 ms per 1 000 lines, 2-4 s and ~18 GB at the cap; user-chosen, documented in docs/config.md) | `crates/tessera-config/src/lib.rs`, `crates/tessera-app-core/src/lib.rs` (`App::attach`), `crates/tessera-grid/src/grid.rs` |
| Window blur radius (#390, ADR 0045) | `[appearance] blur_radius` 0..=64 (`Appearance::MAX_BLUR_RADIUS`): above 64 clamped with one warning that names only the number; a negative, fractional or non-integer value is a parse error, as for `scrollback_lines`; ignored with a warning in a profile; reported by `tessera_app_blur_radius` already clamped, and the host clamps again before it reaches the window server | `crates/tessera-config/src/lib.rs`, `crates/tessera-config/src/load.rs`, `crates/tessera-ffi/src/app.rs` |
| PTY read queue / write queue / pending input | 64 × 64 KiB / 1 MiB / 16 MiB | `crates/tessera-pty/src/native.rs`, `crates/tessera-app-core/src/session.rs` |
| Foreground process name | 64 bytes | `crates/tessera-pty/src/foreground.rs` |
| Kitty keyboard stack | 8 | `crates/tessera-term/src/modes.rs` |
| OSC colour overrides | 259 slots (256 palette entries, foreground, background, cursor), fixed; replies bounded by the outbox | `crates/tessera-term/src/osc_color.rs` (`MAX_COLOR_OVERRIDES`) |
| OSC colour name | 22 bytes (the longest X11 name), rejected before the lookup; static table, no allocation | `crates/tessera-term/src/color_names.rs` (`MAX_NAME_LEN`) |
| Config / theme / import file | 16 MiB / 64 KiB (256 files) / 1 MiB | `crates/tessera-config/src/` |
| Imported plist | 4096 values, 256 KiB of text | `crates/tessera-config/src/import.rs` |
| Symbolic links followed to the configuration directory | 8 links per watcher (`MAX_DIR_LINKS_FOLLOWED`), one extra kqueue descriptor per directory holding one; more, or a cycle, watches none and polls `config.toml`'s metadata instead | `crates/tessera-config/src/watch.rs` |
| Unloadable theme files followed | 8 extra kqueue descriptors per watcher (`MAX_UNLOADABLE_FOLLOWED`), newest first, only while the configured theme is not found as a loadable user file | `crates/tessera-config/src/theme.rs`, `crates/tessera-config/src/watch.rs` |
| URL candidate | 2083 characters | `crates/tessera-app-core/src/url.rs` |
| Smart selection rules (`[selection] smart_rules`) | 64 rules (`MAX_SMART_RULES`), later ones dropped with one warning; pattern 1024 bytes (`MAX_SMART_RULE_PATTERN_LEN`); compiled program and lazy DFA cache 1 MiB each (`SMART_RULE_SIZE_LIMIT`), nesting 50 (`SMART_RULE_NEST_LIMIT`); a rule past a bound, that does not compile or that matches the empty string is left out with a warning; compiled once per configuration load (ADR 0042) | `crates/tessera-config/src/selection.rs` |
| Smart selection scan (double-click) | 8192 characters of the logical line around the click (`SMART_SCAN_LEN`, 4096 before it); a match in the non-blank run at a window edge where the line goes on is dropped, never cut (ADR 0042) | `crates/tessera-app-core/src/selection.rs` |
| Copy with Formatting | 16 MiB of HTML and of RTF each (`MAX_RICH_COPY_LEN`), counted while the runs are collected; RTF colour table 4096 entries (`MAX_RTF_COLORS`); past either the formatted copy is refused and only the plain text is copied; every markup character and every scalar above U+007E escaped, C0 and DEL dropped, no links or external resources (ADR 0042) | `crates/tessera-app-core/src/rich_text.rs`, `crates/tessera-ffi/src/selection.rs` |
| OSC 8 link table | 65 536 live links (`MAX_LIVE_LINKS`), URI 2 083 bytes, id 250 bytes, 16 MiB of URI + id text (`MAX_LINK_TABLE_BYTES`); the id index loses an entry with its link's last cell; a URI with C0, DEL, C1 or bidi formatting controls is refused | `crates/tessera-grid/src/hyperlink.rs`, `crates/tessera-term/src/osc.rs` |
| Search query | 1024 bytes (`MAX_QUERY_BYTES`); a regex program of 10 MiB (`REGEX_SIZE_LIMIT`) and a lazy DFA cache of 4 MiB (`DFA_SIZE_LIMIT`); longer or larger is refused, never cut | `crates/tessera-search/src/engine.rs` |
| Search haystack | 64 KiB of text per scan (`MAX_HAYSTACK_BYTES`); a logical line is cut into segments of at most 128 rows (`MAX_SEGMENT_PERIOD`, fewer on wide screens) | `crates/tessera-search/src/engine.rs`, `crates/tessera-app-core/src/search.rs` |
| Search scan | 256 rows and about 64 Ki cells copied per hold of the terminal lock (`SEARCH_LINES_PER_LOCK`); the C step budget at most 1 s (`TESSERA_SEARCH_MAX_STEP_MICROS`), checked inside each piece after every regex call (every 16 calls of a literal query, which end at their matches): a step returns within `max(4 × budget, 250 ms)` (`LINE_TIME_LIMIT`) plus one regex call, which is not interruptible and is bounded by bytes, not time: it runs on at most one piece's text (64 KiB, or 1 KiB once halved to the floor) and costs up to that length times the program's size. With a pathological pattern a call costs about 0.65-0.9 s per KiB, so the first call of a query can block the main thread for many seconds, and its retries at half the slow piece's size about half as long each (7.3 s, then 3.5, 1.6 and 0.6 s, measured on one 8 KB line; 20.5 s in one step before #331's F9c fix); no time bound holds for it. Search off the main thread is the follow-up (ADR 0038); a piece too slow for its deadline is searched again at half its own size, down to one row and 1 KiB (`MIN_PIECE_BYTES`), a row still too slow there, even in one call (0.65-0.7 s measured on 1 KiB), is skipped (above the floor, a row late only once, as from a stall, is searched again first), and a line the halves cut counts as skipped too (a match across the cut is lost), all reported (`lines_skipped`); the find bar steps with 4 ms a frame | `crates/tessera-search/src/engine.rs`, `crates/tessera-app-core/src/search.rs`, `crates/tessera-ffi/src/search.rs` |
| Search matches | 100 000 (`MAX_MATCHES`), the newest kept; "copy all matches" at most 1 MiB (`MAX_MATCHES_TEXT_BYTES`); one search holds about 16 MB measured, plus up to one piece's matches before they are filed (`Active::found`, at most one per byte of a 64 KiB piece: about 64 Ki matches, roughly 1.5 MB), at most 5.2 MB for the copy of the screen rows it last scanned (`MAX_KEPT_CELLS`) and one window's overlays, and peaks near 40 MB while a pattern near the size limit compiles (ADR 0038) | `crates/tessera-app-core/src/search.rs` |
| OSC 133 mark requests | 4 per `advance` call, the last mark of each kind (`MAX_MARK_REQUESTS`), coalesced again per chunk of PTY output; never replied to (ADR 0039) | `crates/tessera-term/src/lib.rs`, `crates/tessera-app-core/src/session.rs` |
| Command records | 1000 per session (`MAX_COMMAND_RECORDS`), the oldest dropped; metadata only, no command text (ADR 0039) | `crates/tessera-app-core/src/commands.rs` |
| Prompt scan (Previous / Next Prompt, Select Last Output) | 1 000 000 lines per call (`MAX_PROMPT_SCAN_LINES`), 256 lines per hold of the terminal lock (`PROMPT_SCAN_LINES_PER_LOCK`); a prompt further away is not found (ADR 0039) | `crates/tessera-app-core/src/commands.rs` |
| Exit-status gutter and its hover (#45b) | the rows in view plus at most 1024 lines below them per scan (`MAX_GUTTER_SCAN_LINES`), under the terminal lock the frame capture already holds and only when the lines it reads moved; at most one mark per row in view whatever the lines hold (S6); the hover's duration from at most `MAX_COMMAND_RECORDS` records; nothing logged (S8) (ADR 0039) | `crates/tessera-app-core/src/commands.rs`, `crates/tessera-app-core/src/view.rs`, `crates/tessera-render/src/passes/gutter.rs`, `crates/tessera-ffi/src/prompts.rs` |
| `[keys]` table | 512 entries (`MAX_KEY_BINDINGS`), later ones dropped with one warning; chord 64 bytes (`MAX_CHORD_LEN`), checked before the grammar; command name or parameter 64 bytes (`MAX_COMMAND_NAME_LEN`); `send_text` 4096 bytes after unescaping (`MAX_SEND_TEXT_LEN`), checked while decoding; at most one warning per entry plus one for the overflow, each escaped and its key capped (S3) | `crates/tessera-config/src/keys.rs`, `crates/tessera-app-core/src/command.rs` |
| Key map JSON (`tessera_app_keymap_json`) and `on_config_applied` | One entry per registry command (fixed), chords from the capped table; deliveries to the app coalesced to one per main-actor turn with the latest generation | `crates/tessera-ffi/src/keymap.rs`, `apps/macos/Sources/TesseraKit/CallbackBridge.swift` |
| Config edit (`ConfigEditor`, `tessera_config_set` / `_remove`) | key path 1024 bytes, 16 steps, index 65 535 (`MAX_KEY_PATH_LEN`, `MAX_KEY_PATH_STEPS`, `MAX_KEY_PATH_INDEX`); value nesting 32 (`MAX_VALUE_DEPTH`); value JSON 64 KiB (`MAX_CONFIG_VALUE_LEN`); edited file 16 MiB, refused before it is written; user message cut to the caller's buffer; logs carry key paths only, never values (S8) | `crates/tessera-config/src/edit.rs`, `crates/tessera-ffi/src/config.rs` |
| Theme list / import (`tessera_theme_list`, `tessera_theme_import`) | names 128 bytes (`MAX_DISPLAY_NAME_LEN`) with C0, C1, DEL and bidi formatting characters removed (S3); 256 user files (`MAX_THEME_FILES`) of the first 4096 directory entries (`MAX_THEME_DIR_ENTRIES`); import 1 MiB (`MAX_IMPORT_LEN`) and the importers' caps; id 64 bytes (`TESSERA_THEME_ID_LEN`); path 1024 bytes (`TESSERA_MAX_THEME_PATH_LEN`) | `crates/tessera-config/src/edit.rs`, `crates/tessera-ffi/src/config.rs` |
| Profiles (`[[profiles]]`, ADR 0037) | 256 entries (`MAX_PROFILES`), later ones dropped with one warning; name 128 bytes (`MAX_PROFILE_NAME_LEN`), cut on a character boundary with a warning; `SpawnOptions.profile` read at most 129 bytes, longer or not UTF-8 refused (`TESSERA_MAX_PROFILE_NAME_LEN`, S7); a missing working directory's warning escapes the path and never carries the command (S8); menu titles 64 characters with control, format (but ZWJ), separator, private-use and unassigned characters removed (S3) | `crates/tessera-config/src/profile.rs`, `crates/tessera-app-core/src/profile.rs`, `crates/tessera-ffi/src/session.rs`, `apps/macos/Sources/Tessera/MainMenu.swift` |
| Accessible text (ADR 0041) | 4 Mi UTF-16 units of cell text per snapshot (`MAX_TEXT_UNITS`, `TESSERA_MAX_ACCESSIBLE_TEXT_UNITS`), later cells add none, plus a 4-byte offset per cell whatever the text (8 MiB at 1024x2048), so about 16 MiB per snapshot at most and two held per view; the terminal lock held only to copy the view's rows; NUL as a space, C0, DEL and C1 as U+FFFD, bidi formatting controls removed, line and paragraph separators as spaces (S3); never logged (S8) | `crates/tessera-app-core/src/accessibility.rs`, `crates/tessera-ffi/src/accessibility.rs` |
| Announcements and accessibility notifications (ADR 0041) | 1024 characters per announcement, the newest kept (`MAX_ANNOUNCEMENT_CHARS`, `TESSERA_MAX_ANNOUNCEMENT_CHARS`); only while VoiceOver is on; per view one settle 0.15 s after the last output or 1 s after the first, at most 10 posts in any second, one timer pending and two snapshots held; without VoiceOver an output callback only drops the cached snapshot (S6) | `crates/tessera-app-core/src/accessibility.rs`, `apps/macos/Sources/Tessera/TerminalSurfaceView+Accessibility.swift` |
| Saved window state (ADR 0031, ADR 0035) | 1 MiB per value (`WindowStateStore.maxBytes`), checked before decoding, a larger one ignored and never saved; 64 tabs per window (`WindowState.maxTabs`), the `tabs` list counted before a tab is decoded, more makes the value unreadable, saved trimmed to 64; 64 sessions restored in all (`maxRestoredSessions`); a tree of at most 16 panes and 15 splits deep (`PaneLayout.maxDepth`, the decoder refusing nesting past `maxCodingPathLength`), each ratio finite and inside (0, 1), else that tab restores one pane; a value over the size or tab cap opens the default window instead (S5) | `apps/macos/Sources/Tessera/WindowStateStore.swift` |
| Settings window text (ADR 0044) | profile, theme and plugin names shown through `MainMenu.profileTitle` (64 characters, control, format (but ZWJ), separator, private-use and unassigned characters removed); the core's and the importer's messages through `SettingsModel.sanitisedMessage`: control, format, separator, private-use and unassigned characters removed, white space runs as one space, at most 400 characters (`maxMessageLength`); the Keys pane's chords, command titles, conflicts and key-map warnings (at most 20 listed, `KeysModel.maxWarnings`) through the same; only the error kind is logged, never the message (S3, S8) | `apps/macos/Sources/Tessera/Settings/SettingsModel.swift`, `apps/macos/Sources/Tessera/Settings/KeysPane.swift`, `apps/macos/Sources/Tessera/MainMenu.swift` |

## Memory and leaks

What runs today: allocation-counting tests (`crates/tessera-render/tests/alloc.rs`,
`crates/tessera-grid/tests/reset.rs`), the release-only timing bars under an output flood
(`docs/software-factory.md`, "Hang protection"), Thread Sanitizer on the app's XCTest suites, and nightly
libFuzzer runs of the vt, grid and term targets.

What every change must keep:

- Closing a session, tab or window frees what it allocated: no closed session left in a map, no Swift retain cycle
  between a view, its window controller, its `TesseraSession` and a callback's `user_data`.
- Lazy one-time caches (font atlas, shaping cache) are bounded and documented as such.
- A per-session structure that grows with output is capped (S5) and its cap is in the table above.

Tracked: churn and leak checks for Rust, FFI and the app (#214); FFI fuzzing, ASan and Miri for the `unsafe` crates
(#215); memory-per-session and startup budgets (#216).

## Known gaps and open decisions

- The OSC buffer in the parser is unbounded (#213).
- OSC 52 writes are on by default with no notice (only a `config.toml` that exists but does not load at launch, or
  sits behind a configuration directory symlink whose target is missing, turns them off until it loads: the store
  fails closed, `crates/tessera-config/src/store.rs`), multi-line paste without
  bracketed paste runs every line, the link-opening scheme policy is undecided, and the child inherits the app's
  whole environment (#217).
- An unknown OSC command's bytes are traced uncapped (`crates/tessera-term/src/osc.rs`; S8). No subscriber is
  installed in the app today, so nothing is written.
- The app ships Developer ID signed and notarized when the six release secrets exist, ad-hoc signed and not
  notarized otherwise, always with the App Sandbox off (a terminal must spawn arbitrary programs) and hardened
  runtime on (#50). The signing secrets reach only the release job, on tag pushes from the main repository; fork
  and `pull_request` runs never see them and never sign.
- portable-pty's `close_random_fds` allocates between `fork` and `exec`; not known to be exploitable, noted for the
  pty owner.
