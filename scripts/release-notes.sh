#!/usr/bin/env bash
# Render the git-cliff release notes for one tag that points at HEAD.
#
#   scripts/release-notes.sh v0.2.0 > notes.md
#
# `git-cliff --current` cannot be used by the release workflow: it fails with
# "No tag exists for the current commit" as soon as HEAD carries more than one
# release tag (the promote-an-rc flow tags the rc commit a second time as the
# final version). This script instead names the tag explicitly, ignores every
# other release tag on HEAD, and renders the range from the previous release
# tag (see below for how it is chosen) to HEAD. Both the workflow and
# `just release-notes` use it; scripts/release-notes-test.sh checks the
# tag selection in a throwaway repository.
set -euo pipefail

tag="${1:?usage: release-notes.sh <tag>}"

if ! git tag --points-at HEAD | grep -Fqx -e "$tag"; then
    echo "release-notes: tag '$tag' does not point at HEAD" >&2
    exit 1
fi

# Previous release, chosen by the kind of $tag:
#   - a final release (no "-") covers everything since the previous *final*
#     release, so only final tags are candidates and the rc tags on the way
#     are skipped: v0.2.0 .. v0.3.0-rc.1 .. v0.3.0-rc.2 = v0.3.0 renders
#     v0.2.0..v0.3.0; the first final (v0.1.0 promoted from v0.1.0-rc.2)
#     has no candidate and renders the whole history;
#   - a pre-release covers everything since the nearest release tag of any
#     kind: v0.3.0-rc.2 renders v0.3.0-rc.1..v0.3.0-rc.2.
# None at all means the whole history belongs to this tag. When the chosen
# commit carries several release tags (a promoted rc), prefer the final one.
describe=(git describe --tags --abbrev=0 --match 'v[0-9]*')
if [[ "$tag" != *-* ]]; then
    describe+=(--exclude 'v[0-9]*-*')
fi
prev="$("${describe[@]}" HEAD^ 2>/dev/null || true)"
if [[ -n "$prev" ]]; then
    candidates="$(git tag --points-at "$(git rev-list -n1 "$prev")" | grep -E '^v[0-9]' || true)"
    final="$(printf '%s\n' "$candidates" | grep -v -e '-' | sort -V | tail -n1 || true)"
    prev="${final:-$(printf '%s\n' "$candidates" | sort -V | tail -n1)}"
fi

# git-cliff keeps one tag per commit from its own date-ordered tag list and
# cannot be told which, so ignore the other release tags on HEAD (otherwise
# one of them, not $tag, would be the heading) and every tag not reachable
# from HEAD (a newer tag on another branch would otherwise become the previous
# release of a backport). For a final release also ignore every pre-release
# tag, or git-cliff would split the range at the rcs on the way. The previous
# tag chosen above is handed to the template through TESSERA_PREVIOUS_TAG
# instead of ignoring its siblings, which would untag that commit altogether.
ignored="$( {
    git tag --points-at HEAD | grep -Fvx -e "$tag" || true
    git tag --no-merged HEAD
    if [[ "$tag" != *-* ]]; then git tag --list 'v[0-9]*-*'; fi
} | grep -E '^v[0-9]' | sort -u || true)"
args=(--tag "$tag" --strip all)
if [[ -n "$ignored" ]]; then
    regex="^($(printf '%s\n' "$ignored" | sed 's/[.]/\\./g' | paste -sd'|' -))\$"
    args+=(--ignore-tags "$regex")
fi

export TESSERA_PREVIOUS_TAG="$prev"
if [[ -n "$prev" ]]; then
    exec git-cliff "${args[@]}" "$prev..HEAD"
else
    exec git-cliff "${args[@]}"
fi
