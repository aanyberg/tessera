# Releasing

How a release is cut and signed. Releases are made by maintainers; contributors do not need this page. The contributor
workflow is in [`CONTRIBUTING.md`](../CONTRIBUTING.md).

## Cutting a release

Releases are cut by pushing a tag; `.github/workflows/release.yml` does the
rest. Tags are SemVer `vMAJOR.MINOR.PATCH` with an optional pre-release suffix
(`v0.1.0-rc.1`; no leading zeros); anything else fails the workflow. A suffix
marks the GitHub Release as a pre-release.

1. Make sure `main` is green and the `[workspace.package]` version in
   `Cargo.toml` (and `MARKETING_VERSION` in `apps/macos/project.yml`) matches
   the release without its pre-release suffix (`0.1.0` for `v0.1.0-rc.1`).
   The workflow refuses a tag whose commit is not on `main` or whose version
   disagrees with `Cargo.toml`.
2. Preview the notes: `just changelog v0.1.0-rc.1` (git-cliff, `cliff.toml`,
   Conventional Commits; unconventional and merge commits are dropped,
   `chore(deps)` and `chore(release)` are skipped; a commit of any type that
   carries a `Security: <summary>` trailer in its footer lands under
   *Security*). `just changelog` previews the commits *not yet tagged*, so run
   it before tagging; after tagging use `just release-notes <tag>`.
3. Tag and push: `git tag -a v0.1.0-rc.1 -m "v0.1.0-rc.1" && git push origin v0.1.0-rc.1`.

**Promoting a release candidate.** When an rc is good, tag the *same* commit
with the final version and push that tag; nothing is rebuilt from a different
commit:

```sh
git tag -a v0.1.0 -m "v0.1.0" v0.1.0-rc.3^{}   # ^{} = the commit, not the rc tag object
git push origin v0.1.0
```

The notes for `v0.1.0` cover everything since the previous *final* release
(the rc tags on the way are ignored; the first final release covers the whole
history), whereas the notes for an rc cover everything since the nearest
release tag of any kind. The GitHub Release for a final tag is not marked as a
pre-release. After tagging, `just release-notes v0.1.0` shows exactly what the
workflow will attach; `scripts/release-notes-test.sh` exercises the tag
selection in a throwaway repository.

The workflow then, on a `macos-15` runner:

- `just xcframework` – universal (arm64 + x86_64) `TesseraCore.xcframework`.
- `just archive <short> <build>` – `xcodebuild archive`, Release configuration,
  ad-hoc signed (`CODE_SIGN_IDENTITY=-`, hardened runtime). `<short>` is the
  tag without `v` and pre-release suffix (`CFBundleShortVersionString`), `<build>`
  is the workflow run number (`CFBundleVersion`).
- With the signing secrets (see *Release signing* below): re-signs `Tessera.app`
  with the Developer ID certificate and hardened runtime, notarizes it with
  `notarytool` and staples the ticket. Skipped with a notice without them.
- `just package <tag>` – `apps/macos/build/Tessera-<tag>-macos-universal.zip`
  via `ditto` so bundle metadata survives (no `__MACOSX` tree; the smoke test
  checks).
- A smoke test unzips the artifact and sets `com.apple.quarantine` as a browser
  download would. A notarized build must then pass `codesign --verify --deep
  --strict`, `spctl --assess` (source *Notarized Developer ID*) and `stapler
  validate` without any `xattr -d`; the ad-hoc fallback strips the attribute
  first. Both check architectures and version and launch the app.
- `scripts/release-notes.sh <tag>` renders the notes for the pushed tag with
  an explicit `git-cliff --tag` and commit range (previous release tag to
  `HEAD`), ignoring any other release tag on the same commit, so a promoted rc
  and backport tags each get their own heading and "Full diff" link;
  `gh release create` (or `edit` + `upload --clobber` on a re-run) attaches the
  zip and its `.sha256`.

The same steps run locally:

```sh
just xcframework
just archive 0.1.0 1
just package v0.1.0-rc.1
just release-notes v0.1.0-rc.1   # after tagging HEAD
```

A failed release can be re-run from the Actions tab; it updates the existing
GitHub Release in place.

## Release signing

With all six repository secrets (Settings > Secrets and variables > Actions)
the release is Developer ID signed and notarized, and the download opens
without `xattr -dr com.apple.quarantine`:

| Secret | Content |
| --- | --- |
| `APPLE_DEVELOPER_ID_P12_BASE64` | the *Developer ID Application* certificate with its private key, exported as `.p12`, base64 on one line |
| `APPLE_DEVELOPER_ID_P12_PASSWORD` | the password chosen at export |
| `APPLE_NOTARY_KEY_P8_BASE64` | the App Store Connect API key `.p8`, base64 on one line |
| `APPLE_NOTARY_KEY_ID` | that key's Key ID |
| `APPLE_NOTARY_ISSUER_ID` | the Issuer ID shown above the App Store Connect key list |
| `APPLE_TEAM_ID` | the 10-character Team ID (developer.apple.com > Membership) |

- **Certificate.** In Xcode (Settings > Accounts > Manage Certificates) or at
  developer.apple.com create a *Developer ID Application* certificate (needs
  the Account Holder). In Keychain Access select the certificate *with its
  private key* (expand the arrow), File > Export Items, format `.p12`, set a
  password. Then `base64 -i cert.p12 | pbcopy` and paste into the secret.
- **API key.** App Store Connect > Users and Access > Integrations > App Store
  Connect API > Team Keys > generate a key with the *Developer* role. The
  `.p8` can be downloaded once: `base64 -i AuthKey_XXXX.p8 | pbcopy`.
- Delete the local `.p12` and `.p8` afterwards (or keep them in a password
  manager); the workflow never prints a secret and writes them only to the
  runner's temp directory.

What the workflow does with them: it imports the certificate into a temporary
keychain and re-signs the archived app (`--options runtime --timestamp`),
checking the Team ID against `APPLE_TEAM_ID`; submits it with
`xcrun notarytool submit --wait` (on anything but *Accepted* it prints
`notarytool log` and fails); staples the ticket; then `just package`. The
keychain and key files are deleted in a final `always()` step.

**Without the secrets** (forks, pull requests, or before they are added) the
signing, notarization and Gatekeeper checks are skipped with a notice and the
ad-hoc signed build ships as before; its release notes tell users to run
`xattr -dr com.apple.quarantine Tessera.app`. Signing never runs on forks or
`pull_request` events. If only some of the six secrets are set the release
fails, so a half-configured setup cannot silently ship an unsigned build.

**Entitlements.** The hardened runtime needs none beyond the existing
`Tessera.entitlements` (`app-sandbox` = false; the app starts the user's login
shell, so it is not sandboxed). The Metal renderer needs no JIT or
unsigned-memory entitlement, and the Rust core is linked statically into the
executable, so there is no embedded library that would need
`disable-library-validation`. Add `allow-jit`, `allow-unsigned-executable-memory`
or `disable-library-validation` only for a documented need (e.g. an embedded
third-party framework signed by another team).

**First signed release, dry run.** Add the secrets, then push a throwaway
pre-release tag on `main` (e.g. `v0.1.0-rc.1`, version matching `Cargo.toml`)
and watch *Sign*, *Notarize* and *Smoke-test*; a failure leaves no release
and can be re-run. A green run leaves a real pre-release: after checking it,
delete the GitHub Release and the tag (`gh release delete v0.1.0-rc.1
--cleanup-tag`). Download the zip from the Release in a browser and open it on
another Mac first. The smoke test fails closed if the runner has Gatekeeper
assessment disabled (`spctl --assess` then reports nothing useful), so a first
signed run failing at that check is a runner problem, not a signing problem.
