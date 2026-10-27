#!/usr/bin/env bash
# Exercise scripts/release-notes.sh in a throwaway repository.
#
#   scripts/release-notes-test.sh
#
# Builds this history (oldest first) and checks which commits each tag's
# notes cover and which "Full diff" range they link:
#
#   feat(scaffold)                       <- v0.1.0-rc.1
#   fix(a): first fix                    <- v0.1.0-rc.2 and v0.1.0 (promoted)
#   feat(b): after 0.1.0                 <- v0.2.0
#   feat(c): before rc1                  <- v0.3.0-rc.1
#   fix(b): after rc1                    <- v0.3.0-rc.2 and v0.3.0 (promoted)
#   fix(b): security fix + Security trailer
#   chore: merge ...                     (skipped by cliff.toml)
#
# Requires git and git-cliff on PATH.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"
script="$here/release-notes.sh"

work="$(mktemp -d "${TMPDIR:-/tmp}/release-notes-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT

export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null

cd "$work"
git init -q -b main .
cp "$root/cliff.toml" .
git add cliff.toml
git commit -q -m "chore: add cliff.toml"

commit() {
    # commit <subject> [extra -m args...]
    echo "$1" >> log.txt
    git add log.txt
    git commit -q -m "$@"
}

commit "feat(scaffold): initial"
git tag -a v0.1.0-rc.1 -m v0.1.0-rc.1
commit "fix(a): first fix"
git tag -a v0.1.0-rc.2 -m v0.1.0-rc.2
git tag -a v0.1.0 -m v0.1.0 'v0.1.0-rc.2^{}'
commit "feat(b): after 0.1.0"
git tag -a v0.2.0 -m v0.2.0
commit "feat(c): before rc1"
git tag -a v0.3.0-rc.1 -m v0.3.0-rc.1
commit "fix(b): after rc1"
git tag -a v0.3.0-rc.2 -m v0.3.0-rc.2
git tag -a v0.3.0 -m v0.3.0 'v0.3.0-rc.2^{}'
commit "fix(b): security fix" -m "Security: bounds check in the parser"
commit "chore: merge main into task/x"
git tag -a v0.4.0-rc.1 -m v0.4.0-rc.1

failures=0
check() {
    # check <tag> <commit-ish for HEAD> <must-contain>... -- <must-not-contain>...
    local tag="$1" at="$2"; shift 2
    git checkout -q "$at"
    local notes
    notes="$("$script" "$tag")"
    local want=1 item
    for item in "$@"; do
        if [[ "$item" == "--" ]]; then want=0; continue; fi
        if (( want )); then
            if ! grep -Fq -- "$item" <<< "$notes"; then
                echo "FAIL [$tag]: expected '$item'" >&2; failures=$((failures + 1))
            fi
        elif grep -Fq -- "$item" <<< "$notes"; then
            echo "FAIL [$tag]: did not expect '$item'" >&2; failures=$((failures + 1))
        fi
    done
    echo "--- $tag"
    echo "$notes"
}

# First final, promoted from rc.2: whole history, no Full diff.
check v0.1.0 v0.1.0 "## [0.1.0]" "Initial" "First fix" "Add cliff.toml" -- "Full diff" "v0.1.0-rc"
# An rc: since the nearest release tag of any kind.
check v0.1.0-rc.2 v0.1.0-rc.2 "## [0.1.0-rc.2]" "First fix" "v0.1.0-rc.1...v0.1.0-rc.2" -- "Initial"
# Final promoted from rc.2 with rc.1 on an earlier commit: since the previous final.
check v0.3.0 v0.3.0 "## [0.3.0]" "Before rc1" "After rc1" "v0.2.0...v0.3.0" -- "v0.3.0-rc" "After 0.1.0"
check v0.3.0-rc.2 v0.3.0-rc.2 "## [0.3.0-rc.2]" "After rc1" "v0.3.0-rc.1...v0.3.0-rc.2" -- "Before rc1"
# Security trailer groups, merge commit skipped, previous = nearest tag (the final wins over rc.2).
check v0.4.0-rc.1 v0.4.0-rc.1 "### Security" "Security fix" "v0.3.0...v0.4.0-rc.1" -- "merge" "After rc1"

if (( failures )); then
    echo "release-notes-test: $failures failure(s)" >&2
    exit 1
fi
echo "release-notes-test: ok"
