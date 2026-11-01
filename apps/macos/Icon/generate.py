#!/usr/bin/env python3
"""Generates the Tessera app icon: a terminal prompt laid in gold smalti on a
lapis field, Ravenna style, with an ivory and terracotta border band.

Deterministic (fixed seed). Usage: generate.py <out.svg>
"""
import math
import random
import sys

SIZE = 1024
# macOS icon grid: 824 pt body inset 100 pt, continuous-corner radius ~185.
BX, BY, BW, BR = 100, 100, 824, 185
S = 26.0  # tile pitch
GAP = 3.6  # grout width
JIT = 1.6  # corner jitter
GROUT = "#2c303c"

IVORY = ["#efe6d2", "#e8dcc2", "#f4ecdc", "#e2d4b6", "#ebe0c9"]
TERRA = ["#b5532f", "#a8482a", "#c0603a", "#9c4126", "#b85a36"]
LAPIS = ["#1d3561", "#213c6d", "#1a2f57", "#26467c", "#1f3866", "#2a4a80", "#182b50"]
GOLD = ["#d9aa45", "#cf9c36", "#e4bb5c", "#c48e2c", "#dcb150", "#b98429", "#ebc76e"]
LAPIS_DEEP = ["#16284a", "#142445", "#1a2d52", "#132140"]

rng = random.Random(1923)


def shade(hex_color, amount):
    r, g, b = (int(hex_color[i : i + 2], 16) for i in (1, 3, 5))
    f = 1 + amount
    return "#%02x%02x%02x" % tuple(max(0, min(255, round(c * f))) for c in (r, g, b))


def pick(palette, var=0.05):
    return shade(rng.choice(palette), rng.uniform(-var, var))


def body_sd(x, y):
    """Signed distance into the rounded body (positive inside)."""
    cx, cy = BX + BW / 2, BY + BW / 2
    h = BW / 2 - BR
    qx, qy = abs(x - cx) - h, abs(y - cy) - h
    outside = math.hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0)
    return BR - outside


def seg_dist(px, py, ax, ay, bx, by):
    dx, dy = bx - ax, by - ay
    t = max(0, min(1, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))


tiles = []  # (points, color)


def clip_halfplane(pts, n, c):
    """Sutherland-Hodgman: keep the part of pts with n.p <= c."""
    out = []
    for i, p in enumerate(pts):
        q = pts[(i + 1) % len(pts)]
        dp = n[0] * p[0] + n[1] * p[1] - c
        dq = n[0] * q[0] + n[1] * q[1] - c
        if dp <= 0:
            out.append(p)
        if (dp <= 0) != (dq <= 0):
            t = dp / (dp - dq)
            out.append((p[0] + t * (q[0] - p[0]), p[1] + t * (q[1] - p[1])))
    return out


def area(pts):
    return abs(sum(p[0] * q[1] - q[0] * p[1] for p, q in zip(pts, pts[1:] + pts[:1]))) / 2


def quad(cx, cy, w, h, angle, color, clip=None, accept=None):
    ca, sa = math.cos(angle), math.sin(angle)
    pts = []
    for ux, uy in ((-w / 2, -h / 2), (w / 2, -h / 2), (w / 2, h / 2), (-w / 2, h / 2)):
        ux += rng.uniform(-JIT, JIT)
        uy += rng.uniform(-JIT, JIT)
        pts.append((cx + ux * ca - uy * sa, cy + ux * sa + uy * ca))
    full = area(pts)
    if clip:
        pts = clip_halfplane(pts, clip[:2], clip[2])
        if len(pts) < 3 or area(pts) < 0.3 * full:
            return False
    if accept and not accept(pts):
        return False
    tiles.append((pts, color))
    return True


# --- the figure: ">" and "_" -------------------------------------------------
W = 3 * S  # stroke width: three rows of tesserae
A1, APEX, A2 = (318, 318), (536, 512), (318, 706)
U1, U2 = (590, 706), (740, 706)
FIG = [(A1, APEX), (APEX, A2), (U1, U2)]


def fig_dist(x, y):
    return min(seg_dist(x, y, *a, *b) for a, b in FIG)


def lay_stroke(a, b, rows, offset0, palette, var, extend=(0, 0), clip=None, accept=None):
    ax, ay = a
    bx, by = b
    L = math.hypot(bx - ax, by - ay)
    ux, uy = (bx - ax) / L, (by - ay) / L
    nx, ny = -uy, ux
    ang = math.atan2(uy, ux)
    u0, u1 = -extend[0], L + extend[1]
    n = max(1, round((u1 - u0) / S))
    step = (u1 - u0) / n
    for r in range(rows):
        v = offset0 + r * S
        shift = step / 2 if r % 2 else 0  # stagger alternate courses
        for i in range(-1, n + 1):
            u = u0 + shift + (i + 0.5) * step
            if u - step / 2 < u0 - 0.1 or u + step / 2 > u1 + 0.1:
                # half tile at a staggered course end
                lo, hi = max(u - step / 2, u0), min(u + step / 2, u1)
                if hi - lo < step * 0.4:
                    continue
                u, ln = (lo + hi) / 2, hi - lo
            else:
                ln = step
            cx, cy = ax + ux * u + nx * v, ay + uy * u + ny * v
            quad(cx, cy, ln - GAP, S - GAP, ang + rng.uniform(-0.04, 0.04), pick(palette, var),
                 clip=clip, accept=accept)


# Chevron arms, mitred at the apex: tesserae cut on the bisector y = APEX.y.
UPPER = (0, 1, APEX[1] - GAP / 2)
LOWER = (0, -1, -(APEX[1] + GAP / 2))
MITER = W * 1.2
for (a, b), clip in (((A1, APEX), UPPER), ((A2, APEX), LOWER)):
    lay_stroke(a, b, 3, -S, GOLD, 0.06, extend=(0, MITER), clip=clip)
lay_stroke(U1, U2, 3, -S, GOLD, 0.06)

# Andamento: one deep-lapis course outlining the figure.
clear_of_fig = lambda pts: min(fig_dist(*p) for p in pts) > W / 2 - 0.5
for (a, b), clip in (((A1, APEX), UPPER), ((A2, APEX), LOWER)):
    for side in (-1, 1):
        lay_stroke(a, b, 1, side * (W / 2 + S / 2), LAPIS_DEEP, 0.05,
                   extend=(S, MITER + S), clip=clip, accept=clear_of_fig)
for side in (-1, 1):
    lay_stroke(U1, U2, 1, side * (W / 2 + S / 2), LAPIS_DEEP, 0.05, extend=(S, S), accept=clear_of_fig)
# end caps: a short perpendicular course closing each stroke end
for p, q in ((A1, APEX), (A2, APEX), (U1, U2), (U2, U1)):
    L = math.hypot(q[0] - p[0], q[1] - p[1])
    ux, uy = (q[0] - p[0]) / L, (q[1] - p[1]) / L
    cx, cy = p[0] - ux * S / 2, p[1] - uy * S / 2
    nx, ny = -uy, ux
    lay_stroke((cx - nx * W / 2, cy - ny * W / 2), (cx + nx * W / 2, cy + ny * W / 2), 1, 0,
               LAPIS_DEEP, 0.05, accept=clear_of_fig)


# --- the border: courses laid along the body contour -------------------------
def contour_course(d, palette, var):
    h = BW / 2 - d
    r = max(BR - d, 1.0)
    cx0, cy0 = BX + BW / 2, BY + BW / 2
    straight = 2 * (h - r)
    arc = math.pi / 2 * r
    per = 4 * (straight + arc)
    n = round(per / S)
    step = per / n
    phase = rng.uniform(0, step)
    corners = [((cx0 + h - r, cy0 - h + r), -math.pi / 2), ((cx0 + h - r, cy0 + h - r), 0.0),
               ((cx0 - h + r, cy0 + h - r), math.pi / 2), ((cx0 - h + r, cy0 - h + r), math.pi)]
    starts = [(cx0 - h + r, cy0 - h), (cx0 + h, cy0 - h + r), (cx0 + h - r, cy0 + h), (cx0 - h, cy0 + h - r)]
    dirs = [(1, 0), (0, 1), (-1, 0), (0, -1)]
    for i in range(n):
        s = (phase + i * step) % per
        side = int(s // (straight + arc))
        t = s - side * (straight + arc)
        if t < straight:
            sx, sy = starts[side]
            dx, dy = dirs[side]
            px, py, ang = sx + dx * t, sy + dy * t, math.atan2(dy, dx)
        else:
            (ccx, ccy), a0 = corners[side]
            a = a0 + (t - straight) / r
            px, py, ang = ccx + r * math.cos(a), ccy + r * math.sin(a), a + math.pi / 2
        quad(px, py, step - GAP, S - GAP, ang + rng.uniform(-0.03, 0.03), pick(palette, var))


INSET = 16
BANDS = [IVORY, TERRA, IVORY, LAPIS_DEEP]
for k, pal in enumerate(BANDS):
    contour_course(INSET + S / 2 + k * S, pal, 0.05)

# --- the field: running bond, with cut tesserae where it meets the courses ---
FIELD_MIN = INSET + len(BANDS) * S + GAP * 0.4
fits = lambda pts: (min(body_sd(*p) for p in pts) > FIELD_MIN
                    and min(fig_dist(*p) for p in pts) > W / 2 + S + GAP * 0.4)
ORG = BX + BW / 2
for gy in range(-20, 21):
    for gx in range(-20, 21):
        cx = ORG + gx * S + ((S / 2) if gy % 2 else 0) + S / 4
        cy = ORG + gy * S + S / 2
        ang = rng.uniform(-0.05, 0.05)
        if quad(cx, cy, S - GAP, S - GAP, ang, pick(LAPIS, 0.07), accept=fits):
            continue
        # cut it: halves first (either way), then quarters
        placed = False
        for halves in (((-1, 0), (1, 0)), ((0, -1), (0, 1))):
            for dx, dy in halves:
                w, h = (S / 2, S) if dx else (S, S / 2)
                placed |= quad(cx + dx * S / 4, cy + dy * S / 4, w - GAP, h - GAP, ang,
                               pick(LAPIS, 0.07), accept=fits)
            if placed:
                break
        if not placed:
            for dx, dy in ((-1, -1), (1, -1), (1, 1), (-1, 1)):
                quad(cx + dx * S / 4, cy + dy * S / 4, S / 2 - GAP, S / 2 - GAP, ang,
                     pick(LAPIS, 0.07), accept=fits)

# --- emit --------------------------------------------------------------------
out = [
    f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SIZE} {SIZE}" width="{SIZE}" height="{SIZE}">',
    "<defs>",
    f'<clipPath id="body"><rect x="{BX}" y="{BY}" width="{BW}" height="{BW}" rx="{BR}"/></clipPath>',
    '<radialGradient id="light" cx="0.32" cy="0.22" r="0.95">'
    '<stop offset="0" stop-color="#fff" stop-opacity="0.16"/>'
    '<stop offset="0.55" stop-color="#fff" stop-opacity="0"/>'
    '<stop offset="1" stop-color="#000" stop-opacity="0.28"/></radialGradient>',
    '<filter id="shadow" x="-10%" y="-10%" width="120%" height="125%">'
    '<feDropShadow dx="0" dy="10" stdDeviation="12" flood-color="#000" flood-opacity="0.35"/></filter>',
    "</defs>",
    f'<rect x="{BX}" y="{BY}" width="{BW}" height="{BW}" rx="{BR}" fill="{GROUT}" filter="url(#shadow)"/>',
    '<g clip-path="url(#body)">',
]
for pts, col in tiles:
    d = " ".join(f"{x:.1f},{y:.1f}" for x, y in pts)
    out.append(f'<polygon points="{d}" fill="{col}"/>')
out += [
    f'<rect x="{BX}" y="{BY}" width="{BW}" height="{BW}" fill="url(#light)"/>',
    "</g>",
    f'<rect x="{BX + 1}" y="{BY + 1}" width="{BW - 2}" height="{BW - 2}" rx="{BR - 1}" fill="none" '
    'stroke="#000" stroke-opacity="0.35" stroke-width="2"/>',
    "</svg>",
]
with open(sys.argv[1], "w") as f:
    f.write("\n".join(out))
print(f"{len(tiles)} tesserae")
