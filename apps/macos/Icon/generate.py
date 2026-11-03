#!/usr/bin/env python3
"""Generates the Tessera app icon as an Icon Composer document (Liquid Glass).

The theme of the original mosaic icon, reduced to a few large tesserae so the
system's glass material does the work: a terminal prompt ">_" in gold and
terracotta glass tiles over a lapis field of large, softly varied tiles.

Writes <out>.icon/icon.json and <out>.icon/Assets/*.svg. Deterministic (fixed
seed). Usage: generate.py <out.icon>
"""
import json
import math
import os
import random
import sys

SIZE = 1024  # Icon Composer canvas; the system applies the squircle mask.
rng = random.Random(1923)

LAPIS = ["#23427a", "#25467f", "#213f75", "#274a84"]


def shade(hex_color, amount):
    r, g, b = (int(hex_color[i : i + 2], 16) for i in (1, 3, 5))
    f = 1 + amount
    return "#%02x%02x%02x" % tuple(max(0, min(255, round(c * f))) for c in (r, g, b))


def pick(palette, var):
    return shade(rng.choice(palette), rng.uniform(-var, var))


def tile(cx, cy, side, angle_deg, radius, color):
    """A rounded square tessera centred on (cx, cy), rotated by angle_deg."""
    h = side / 2
    return (
        f'<rect x="{cx - h:.1f}" y="{cy - h:.1f}" width="{side:.1f}" height="{side:.1f}" '
        f'rx="{radius:.1f}" fill="{color}" transform="rotate({angle_deg:.2f} {cx:.1f} {cy:.1f})"/>'
    )


def svg(body):
    return "\n".join(
        [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SIZE} {SIZE}" '
         f'width="{SIZE}" height="{SIZE}">', *body, "</svg>"]
    )


# --- field: a straight grid of lapis tesserae (opus tessellatum), slightly
# irregular like hand-set stone, bleeding past the mask. The grout is the
# background fill, a deeper lapis, so the field reads as texture, not tile.
N = 10
PITCH = SIZE / N
FGAP = 9
field = []
for gy in range(-1, N + 1):
    for gx in range(-1, N + 1):
        cx = (gx + 0.5) * PITCH + rng.uniform(-2.5, 2.5)
        cy = (gy + 0.5) * PITCH + rng.uniform(-2.5, 2.5)
        # lighter toward the top left, like light falling across the wall
        lift = 0.12 * (0.5 - (cx + cy) / (2 * SIZE))
        col = shade(pick(LAPIS, 0.025), lift)
        side = PITCH - FGAP + rng.uniform(-2, 2)
        field.append(tile(cx, cy, side, rng.uniform(-2.5, 2.5), 12, col))

# --- figure: ">" as five tesserae on the 45 degree diagonals, "_" as a slab --
SIDE = 136  # tessera side
STEP = 156  # centre spacing along an arm (SIDE + grout)
RAD = 26
APEX = (556, 512)
d = STEP / math.sqrt(2)  # per-tile offset in x and y along a 45 degree arm
GOLD_FILL = "url(#gold)"
chevron = [tile(APEX[0], APEX[1], SIDE, 45, RAD, GOLD_FILL)]
for k in range(1, 3):
    for sy in (-1, 1):
        chevron.append(tile(APEX[0] - k * d, APEX[1] + sy * k * d, SIDE, 45, RAD, GOLD_FILL))

# the cursor: one long tessera, its bottom level with the chevron's lowest point
base_y = APEX[1] + 2 * d + SIDE / math.sqrt(2)
CW, CH = 196, 96
cursor = [f'<rect x="648" y="{base_y - CH:.1f}" width="{CW}" height="{CH}" rx="{RAD}" '
          f'fill="url(#terra)"/>']

# One gradient per glyph in canvas space, lit from above, so the figure reads
# as one piece of glass rather than tiles of different shades.
def grad(gid, top, bottom):
    return (f'<defs><linearGradient id="{gid}" gradientUnits="userSpaceOnUse" x1="0" y1="200" '
            f'x2="0" y2="840"><stop offset="0" stop-color="{top}"/>'
            f'<stop offset="1" stop-color="{bottom}"/></linearGradient></defs>')

chevron.insert(0, grad("gold", "#f2c55a", "#d08f27"))
cursor.insert(0, grad("terra", "#ee8457", "#d2623a"))

# --- emit ----------------------------------------------------------------------
out = sys.argv[1]
os.makedirs(os.path.join(out, "Assets"), exist_ok=True)
assets = {"field.svg": field, "chevron.svg": chevron, "cursor.svg": cursor}
for name, body in assets.items():
    with open(os.path.join(out, "Assets", name), "w") as f:
        f.write(svg(body) + "\n")

GROUT = "srgb:0.10196,0.20392,0.38824,1.00000"  # #1a3463
GROUT_DARK = "srgb:0.08235,0.17255,0.33725,1.00000"  # #162c56
icon = {
    "fill-specializations": [
        {"value": {"solid": GROUT}},
        # keep the lapis grout in dark mode; the system default is near black
        {"appearance": "dark", "value": {"solid": GROUT_DARK}},
    ],
    "groups": [
        {
            "name": "prompt",
            "layers": [
                {"name": "cursor", "image-name": "cursor.svg", "glass": True},
                {"name": "chevron", "image-name": "chevron.svg", "glass": True},
            ],
            "lighting": "combined",
            "shadow": {"kind": "neutral", "opacity": 0.55},
            "specular": True,
            "translucency": {"enabled": False, "value": 0.5},
        },
        {
            "name": "field",
            "layers": [{"name": "field", "image-name": "field.svg", "glass": False}],
            "shadow": {"kind": "neutral", "opacity": 0.3},
            "translucency": {"enabled": False, "value": 0.5},
        },
    ],
    "supported-platforms": {"squares": ["macOS"]},
}
with open(os.path.join(out, "icon.json"), "w") as f:
    json.dump(icon, f, indent=2)
    f.write("\n")
print(f"{len(field)} field tiles, {len(chevron) + len(cursor)} glass tiles")
