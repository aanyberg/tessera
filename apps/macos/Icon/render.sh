#!/usr/bin/env bash
# Regenerates the app icon: Icon/AppIcon.svg and the PNGs in
# Resources/Assets.xcassets/AppIcon.appiconset. Needs python3 and rsvg-convert
# (`brew install librsvg`). The layout is seeded, so the output is reproducible.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
set="$here/../Resources/Assets.xcassets/AppIcon.appiconset"
python3 "$here/generate.py" "$here/AppIcon.svg"
for px in 16 32 64 128 256 512 1024; do
    rsvg-convert -w "$px" -h "$px" "$here/AppIcon.svg" -o "$set/icon_$px.png"
done
