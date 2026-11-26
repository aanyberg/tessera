# ADR 0050: Updates through Sparkle 2

**Status:** accepted · 2026-11-26

## Context

A release is a notarized zip on GitHub Releases (`docs/releasing.md`). A user learns of a new one by looking, and
replaces the app by hand. #186 asks how the app updates itself. The implementation is #764.

## Decision

**D1. Sparkle 2, not a custom check, not nothing.** Sparkle gives EdDSA-signed updates, the standard macOS update
window and delta updates; iTerm2 and Ghostty use it too. A custom check would mean writing the download,
verification and replace steps, and the update of a running, signed bundle, ourselves, in the one place where a
mistake replaces code on the user's machine. No updater leaves users on old, possibly vulnerable builds.

**D2. Feed.** An appcast (`appcast.xml`) at the URL in `SUFeedURL`, a release asset or a GitHub Pages file (#764
picks one; the URL is fixed once shipped, since old builds read it). The release workflow builds it on each tag
with Sparkle's `generate_appcast` over the notarized, stapled zip that `just package` produces, so the entry's
length and signature are those of the file users download, and publishes it with the release. The appcast is built
after notarization, never before.

**D3. Signing key.** The EdDSA private key is the Actions secret `SPARKLE_ED_PRIVATE_KEY`; it is never in the
repository, and the workflow hands it to `generate_appcast --ed-key-file -`, which reads it from stdin, not through a file or the log. The public key
is `SUPublicEDKey` in `Info.plist`. A build whose key does not match the appcast's signatures rejects the update.
The key is the maintainer's one-time step:
1. Run Sparkle's `generate_keys` on the maintainer's Mac; it stores the private key in the login keychain and
   prints the public key.
2. Put the public key in `SUPublicEDKey` (a commit).
3. Export the private key with `generate_keys -x <file>`, paste its content into the Actions secret
   `SPARKLE_ED_PRIVATE_KEY`, then delete the file and keep a copy in a password manager. Losing the key means no
   installed build can be updated again; shipping a new key then is a manual download.
Without the secret (forks, pull requests, before step 3) the appcast step is skipped with a notice, like signing.

**D4. Channels.** Stable by default: only items without a channel are offered. Pre-release tags (`v*-rc.N`) are
published (`generate_appcast --channel rc`) with `<sparkle:channel>rc</sparkle:channel>` and offered only when a Settings checkbox opts in (off by
default), through the updater delegate's `allowedChannels`.

**D5. Privacy.** System profiling is off (`SUEnableSystemProfiling` false), so the request carries at most the app
version, OS version and CPU type. The user agent holds the app name, app version and Sparkle version; the query
string carries data only when profiling is on. #764 confirms the exact request with a capture. It runs once a
day (`SUScheduledCheckInterval` 86400) and a Settings checkbox turns it off; "Check for Updates..." still works
then. Nothing from a terminal session reaches the request.

**D6. Signing and notarization stay as they are.** Developer ID signing and notarization (`docs/releasing.md`,
*Release signing*) do not change. Sparkle's framework and its helpers are embedded in the bundle and so are signed
inside-out by the same re-sign step before notarization, and the smoke test stays the gate. Sparkle verifies the
EdDSA signature and, on top, that the new bundle's code signature matches the running one, so an appcast on its own
cannot replace the app. The ad-hoc build (no secrets) has no `SUFeedURL` in its `Info.plist` (the archive step omits it), so it
never checks and is not offered a feed that updates it into a signed one.
`disable-library-validation` stays unneeded: the framework is signed by our team.

**D7. Transport and replay.** `SUFeedURL` is `https` and App Transport Security stays on. The EdDSA signature
covers the enclosure, so a compromised feed host cannot push code. Sparkle offers only a `sparkle:version` higher
than the running build, so a replayed old appcast cannot downgrade. The residual risk is that whoever controls the
feed withholds or freezes updates. #764 decides whether to sign the appcast itself (Sparkle 2.x supports it).

**D8. Release notes.** Shown as plain text or sanitized HTML from the appcast item; no remote content is loaded
into the update window.

**D9. Cask.** The Homebrew cask (#185) moved to 0.5.0. When it ships it sets `auto_updates true`, so `brew upgrade`
leaves an installed app to Sparkle instead of fighting it.

## Consequences

- A new trust boundary: a network fetch of the appcast and the code it points to, which replaces the app.
  `docs/security.md` lists it. It is guarded by the EdDSA key (D3) and the code-signing match (D6), and by HTTPS
  for the feed.
- The key is a single point of failure: a leaked key lets its holder sign updates (rotate the key and ship a build
  by hand); a lost one ends updates.
- The app links a third-party framework, which `cargo-deny` does not cover; the Swift package is pinned and
  updated deliberately.
- The release workflow grows a step that needs a secret; the unsecreted path is unchanged.
