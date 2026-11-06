#!/usr/bin/env bash
# Regenerates the app icon:
# - Resources/AppIcon.icon, the Icon Composer document (Liquid Glass) that
#   Xcode 26 or later compiles, with its own flat fallback for macOS 15;
# - the PNGs in Resources/Assets.xcassets/AppIcon.appiconset, the same design
#   as Icon Composer renders it, for Xcode 16 (CI and release pin Xcode 16.4),
#   which ignores the .icon. Both are named AppIcon; Xcode 26 prefers the .icon.
# Needs python3, rsvg-convert (`brew install librsvg`) and Xcode 26 or later
# (Icon Composer's ictool). The layout is seeded, so the output is reproducible
# for a given ictool and rsvg-convert; other versions may differ in a few bytes.
# Last checked with Xcode 27.0 (27A5252f) and its Icon Composer 2.0 (ictool),
# rsvg-convert 2.63.4 (librsvg, Homebrew) on cairo 1.18.6, pango 1.58.2.
# With PREVIEW=<dir>, also renders the dark, tinted and clear appearances there.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
icon="$here/../Resources/AppIcon.icon"
iconset="$here/../Resources/Assets.xcassets/AppIcon.appiconset"
ictool="$(xcode-select -p)/../Applications/Icon Composer.app/Contents/Executables/ictool"
[[ -x "$ictool" ]] || {
    echo "render.sh: needs Xcode 26 or later selected (no ictool at $ictool)" >&2
    exit 1
}
command -v rsvg-convert > /dev/null || { echo "render.sh: needs rsvg-convert" >&2; exit 1; }
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

rm -rf "$icon"
python3 "$here/generate.py" "$icon"

render() { # <rendition> <out.png>
    "$ictool" "$icon" --export-image --output-file "$2" --platform macOS --rendition "$1" \
        --width 1024 --height 1024 --scale 1 > /dev/null
}

# Legacy icon: the masked render on the macOS grid (824 pt body inset 100 pt),
# with the drop shadow the system adds to Liquid Glass icons.
render Default "$tmp/full.png"
cat > "$tmp/legacy.svg" <<EOF
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 1024 1024" width="1024" height="1024">
<filter id="shadow" x="-10%" y="-10%" width="120%" height="125%"><feDropShadow dx="0" dy="10" stdDeviation="12" flood-color="#000" flood-opacity="0.3"/></filter>
<image x="100" y="100" width="824" height="824" xlink:href="$tmp/full.png" filter="url(#shadow)"/>
</svg>
EOF
for px in 16 32 64 128 256 512 1024; do
    rsvg-convert -w "$px" -h "$px" "$tmp/legacy.svg" -o "$iconset/icon_$px.png"
done

if [[ -n "${PREVIEW:-}" ]]; then
    mkdir -p "$PREVIEW"
    for r in Default Dark TintedLight ClearDark; do
        render "$r" "$PREVIEW/AppIcon-$r.png"
    done
fi
