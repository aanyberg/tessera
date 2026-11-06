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
| Paste and clipboard | `tessera-app-core` input and paste, the app's pasteboard bridge | S4 |
| Child-controlled text in the UI: titles, process names, URLs | `tessera-app-core` (`url.rs`), `apps/macos` | S3 |
| The C ABI | `tessera-ffi`, the TesseraKit wrapper | S7 |
| Spawning the child: environment, fds, signals, foreground job | `tessera-pty`, `tessera-shell-integration` | S5, S7 |
| Parsing files: config, themes, imports | `tessera-config`, `tessera-devtool` | S5 |
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
  `crates/tessera-term/tests/osc8_alloc.rs` and `crates/tessera-term/tests/mouse_alloc.rs` are the exceptions (CONTRIBUTING.md). Every FFI export that can panic runs under `ffi_guard!` (`catch_unwind`);
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
| DCS | streamed in 4 KiB chunks; DECRQSS request 64 bytes | `crates/tessera-vt/src/lib.rs`, `crates/tessera-term/src/dcs.rs` |
| Grapheme cluster | 32 scalars | `crates/tessera-grid/src/grapheme.rs` |
| Grid | 2048 columns × 1024 rows | `crates/tessera-app-core/src/view.rs`, `crates/tessera-ffi/src/session.rs` |
| Scrollback | `[terminal] scrollback_lines` (default 10 000), applied when a session starts; capped at `Terminal::MAX_SCROLLBACK_LINES` (10 000 000); a width change re-wraps the whole history under the terminal lock (~0.2 ms per 1 000 lines, 2-4 s and ~18 GB at the cap; user-chosen, documented in docs/config.md) | `crates/tessera-config/src/lib.rs`, `crates/tessera-app-core/src/lib.rs` (`App::attach`), `crates/tessera-grid/src/grid.rs` |
| PTY read queue / write queue / pending input | 64 × 64 KiB / 1 MiB / 16 MiB | `crates/tessera-pty/src/native.rs`, `crates/tessera-app-core/src/session.rs` |
| Foreground process name | 64 bytes | `crates/tessera-pty/src/foreground.rs` |
| Kitty keyboard stack | 8 | `crates/tessera-term/src/modes.rs` |
| OSC colour overrides | 259 slots (256 palette entries, foreground, background, cursor), fixed; replies bounded by the outbox | `crates/tessera-term/src/osc_color.rs` (`MAX_COLOR_OVERRIDES`) |
| OSC colour name | 22 bytes (the longest X11 name), rejected before the lookup; static table, no allocation | `crates/tessera-term/src/color_names.rs` (`MAX_NAME_LEN`) |
| Config / theme / import file | 16 MiB / 64 KiB (256 files) / 1 MiB | `crates/tessera-config/src/` |
| Imported plist | 4096 values, 256 KiB of text | `crates/tessera-config/src/import.rs` |
| Unloadable theme files followed | 8 extra kqueue descriptors per watcher (`MAX_UNLOADABLE_FOLLOWED`), newest first, only while the configured theme is not found as a loadable user file | `crates/tessera-config/src/theme.rs`, `crates/tessera-config/src/watch.rs` |
| URL candidate | 2083 characters | `crates/tessera-app-core/src/url.rs` |
| OSC 8 link table | 65 536 live links (`MAX_LIVE_LINKS`), URI 2 083 bytes, id 250 bytes, 16 MiB of URI + id text (`MAX_LINK_TABLE_BYTES`); the id index loses an entry with its link's last cell; a URI with C0, DEL, C1 or bidi formatting controls is refused | `crates/tessera-grid/src/hyperlink.rs`, `crates/tessera-term/src/osc.rs` |

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
- OSC 52 writes are on by default with no notice (only a `config.toml` that exists but does not load at launch turns
  them off until it loads: the store fails closed, `crates/tessera-config/src/store.rs`), multi-line paste without
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
