# ADR 0048: Notifications when a long command finishes

**Status:** accepted · 2026-11-22

## Context

Issue #470 asks for a system notification when a long command finishes in a
pane the user is not looking at, as kitty (`notify_on_cmd_finish`) and
Ghostty (`notify-on-command-finish`) have. Shell integration already marks
each command with OSC 133 C (it starts) and D (it ended, with its exit
status), and the core turns them into `SessionEvent::CommandStarted` and
`CommandFinished { exit_code, duration }`, at most four command events per
chunk of output (ADR 0039, S6). Nothing reaches the app yet.

A system notification is a new place that shows child-controlled text, and
it outlives the app in Notification Center, so it is a new surface (S9) and
needs this record first. The core keeps no command text (ADR 0021, 0039,
0047): there is none to show.

## Decision

**D1. Focus is pane-level, judged at the finish.** A pane has the keyboard
when the app is active, its window is key, its window is its tab group's
selected one (or has no group) and it is that window's focused pane.
Anything else is unfocused: a background tab of the key window (F03), a
split pane that is not the focused one (F02), any pane while another app is
frontmost (F04) or its window is minimised or on another Space (F05). Focus
is judged when the command finishes, not when it starts (F08). It reaches
the decision through one seam (`CommandNotifier.Focus`), which tests
replace: none reads the real `NSApp.isActive`.

**D2. The text is the tab title, never the command.** Title: "Command
succeeded" (exit status 0), "Command failed" (any other) or "Command
finished" (the D mark carried no status). Body: "Took <duration>", then ",
exit status <n>" when it is known; the duration is whole seconds rounded
down in hours, minutes and seconds ("1h 2m 5s", "59s"). Subtitle: the title
the pane's tab shows while that pane is focused (with the remote-host
prefix of ADR 0022 when it is on), or none when the pane has no title of
its own. Nothing from the output or the grid is read (S2).

**D3. The finish-time title.** The subtitle is the pane's title when the
command finishes. A snapshot taken at the start (the shell's title for the
running command) is a follow-up.

**D4. The threshold compares durations.** A finish qualifies when its
duration is at least `command_finished_after` seconds (`>=`, comparing
`Duration`s, never rounded seconds). The exit status never matters.

**D5. The configuration fails closed.** `[notifications] command_finished`
is `"unfocused"` or `"off"`; any other string (a typo, another case) warns
and means off. `command_finished_after` is a `u32`: 0 and anything above
86400 clamp to 1 or 86400 with one warning; a negative, fractional, string
or larger value fails the load as other `u32` keys do. Both are app-wide:
no profile changes them, and a profile's `notifications` table is an
unknown key.

**D6. Off disables everything.** With `"off"` nothing is posted and no tab
is marked.

**D7. The limit limits posts, not marks.** At most
`MAX_NOTIFICATIONS_PER_MINUTE = 10` notifications are posted in any 60 s,
app-wide (a sliding window over at most 10 stored instants,
`tessera_app_core::NotifyLimiter`). A qualifying finish past the limit
still marks its tab. The dropped count is logged once, as a number, with
the next post allowed. A finish that does not qualify never uses the
budget.

**D8. Marks are per pane.** A qualifying finish marks its pane; the tab
shows a mark while any of its panes is marked; a pane's mark clears when
that pane gets the keyboard (it is focused, its tab selected or its window
made key, D1). Selecting a tab clears the mark only when the marked pane is
the tab's focused one; a marked split pane keeps its mark until it is
focused.

**D9. No sound.** Permission is asked for alerts only (`[.alert]`), and a
notification shown while the app is frontmost is a banner and a list entry
(`willPresent` returns `[.banner, .list]`; without it macOS shows nothing
for a frontmost app, which is the background-tab case).

**D10. Delivered notifications go when they are seen.** A pane's delivered
notification is removed when the pane gets the keyboard, and all of the
app's when it quits. A pane has at most one in Notification Center: a new
finish replaces its last one (one identifier per session).

**D11. The mark is a dot in the tab.** `NSWindowTab.accessoryView` holds a
small accent-coloured dot (accessibility description "Command finished");
`window.title`, which the Window menu and the saved window state use, is
untouched.

**Where the decision runs.** In the core. The FFI forwards each
`CommandFinished` as `on_session_command_finished(user_data, session_id,
exit_known, exit_code, duration_ms)` (the duration saturating at
`u64::MAX` ms; `CommandStarted` does not cross; a command still running
when the child exits makes none). The app computes `focused` (D1) and asks
`tessera_app_command_finished_decision(app, duration_ms, focused)`, which
checks the configuration in effect now and then the limiter, and answers 0
(nothing), 1 (mark only) or 2 (mark and post). The app does not
re-implement the threshold. The bridge coalesces finishes per session like
its other events, delivered before the exit, but keeps the longest finish
of a delivery (the latest of equals), so a long command followed at once by
a short one is the one decided on. With two commands in one PTY read, the
core's per-chunk coalescing of marks (ADR 0039) loses neither: the short
command's `C` ends the long one (exit status unknown) before its own `D`
ends the short one.

**Permission.** The app never asks at launch. The first post that finds the
permission undecided asks once; posts made while the prompt is up post when
it is answered; later posts never ask again. A refusal, or the
notifications-not-allowed error, is logged once with its code, never
retried, and the tab is still marked. `UNUserNotificationCenter.current()`
is resolved at each use, never in an initialiser or a static, because a
process without an app bundle raises an exception there. The centre's
delegate is a separate class (`NotificationDelegate`, not the main-actor
application delegate: macOS calls it on a queue of its own), installed
before launching finishes so a click that launched the app reaches it.

**Clicks.** A click on a notification reads the session id from its
`userInfo` (the only thing there), brings the app to the front and selects
that session's window, tab and pane. A session not open in this process (a
previous launch's notification, a closed tab's) only brings the app to the
front.

**Default.** Off. kitty and Ghostty default to off, and on would make the
first long command in a background tab raise the system permission prompt
unasked. Recorded on #470 for the user to revise: turning it on by default
is a one-line change of the default and of its test (C01).

## Consequences

- Sanitising (S3): the subtitle drops C0 controls (line feeds included),
  DEL, C1 controls, bidirectional formatting characters and line and
  paragraph separators, even though titles already arrive sanitised from
  the core (ADR 0022); a title with nothing visible left (white space,
  default-ignorable characters, combining marks) shows no subtitle; it is
  cut at `MAX_NOTIFICATION_TEXT_BYTES = 256` bytes of UTF-8 on a character
  boundary.
- `userInfo` holds the session id only, never text; logs carry outcomes,
  codes and counts, never the title, subtitle or body (S8).
- Per-byte work (S6): finishes are at most four command events per chunk
  in the core, one pending finish per session in the bridge, one decision
  per delivery, and at most 10 posts a minute.
- Known limit: a locked screen, a screen saver or a sleeping display still
  counts as focused (F06): AppKit reports the window key and the app active,
  and there is no occlusion check.
- Known limit: an ad-hoc signed build (the fallback release, every local
  build) may be refused notifications, and each rebuild may reset the
  decision. That is treated as a refusal (logged once, marks still shown),
  not a bug; the release's manual check (#589 E4) records whether it is
  granted.
- Known limit: with three or more commands in one PTY read (typed ahead
  while the reader lags), coalescing keeps only the last mark of each kind,
  so the middle commands' finishes are lost and only the longest finish of
  the read is reported, possibly with a later command's exit status (a
  failed long command can read as succeeded).
- Known limit: a window with one tab has no tab bar, so the mark is not
  visible there (a marked split pane of it included); only the notification
  shows. `window.title` stays untouched (D11).
- The app's real-launch behaviour (the prompt, a banner while frontmost,
  the mark under `.preferred` tabbing, a click) cannot be tested in the
  hosted tests, whose console is locked; it is in #589 E4.
