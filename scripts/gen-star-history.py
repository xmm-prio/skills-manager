#!/usr/bin/env python3
"""Generate a self-hosted, hand-drawn Star History SVG for the README.

Why self-hosted (and why this is now the *only* reliable option):
Since 2026-06-30 GitHub restricts the stargazer timeline ("who starred and when")
to a repo's own owner/collaborators — https://www.star-history.com/blog/github-stargazer-api-restriction
Anyone else (including star-history.com's shared token pool) gets `Not Found`,
which is why the live star-history embed is broken for this repo. The owner can
still read it with a scoped token, so we pull the timeline with the locally
authenticated `gh` CLI and render a static SVG committed to the repo. Viewers
load a plain image — zero GitHub API calls, nothing to rate-limit or break.

Refresh anytime:  python3 scripts/gen-star-history.py
Requires: `gh` authenticated as the repo owner with a scoped token (a no-scope
token no longer works, even on your own repo). The handwritten look uses the
bundled OFL font in scripts/fonts/, embedded into the SVG so it renders anywhere.
"""

import base64
import json
import math
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

REPO = "xingkongliang/skills-manager"
ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "assets" / "star-history.svg"
FONT = Path(__file__).resolve().parent / "fonts" / "PatrickHand-Regular.woff2"

W, H = 800, 440
L, R, T, B = 84, 36, 84, 74  # plot margins
PW, PH = W - L - R, H - T - B

INK = "#1f2328"
RED = "#e0553e"
GRID = "#1f2328"
MUTED = "#8a9199"
GREEN = "#41c463"
BADGE = "#6b5cff"
FONT_STACK = "'PatrickHand','Comic Sans MS','Comic Neue',cursive"


class Rng:
    """Deterministic LCG so the same star data yields a byte-identical SVG."""

    def __init__(self, seed=0x51EED):
        self.s = seed & 0x7FFFFFFF

    def next(self):
        self.s = (1103515245 * self.s + 12345) & 0x7FFFFFFF
        return self.s / 0x7FFFFFFF

    def uni(self, a, b):
        return a + (b - a) * self.next()


def fetch_starred_at(repo):
    out = subprocess.run(
        ["gh", "api", "-H", "Accept: application/vnd.github.star+json",
         "--paginate", "--slurp", f"repos/{repo}/stargazers?per_page=100"],
        capture_output=True, text=True, check=True,
    ).stdout
    stamps = [
        datetime.strptime(e["starred_at"], "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
        for page in json.loads(out) for e in page
    ]
    stamps.sort()
    return stamps


def nice_ceiling(v):
    if v <= 5:
        return 5
    base = 10 ** math.floor(math.log10(v))
    for m in (1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10):
        if m * base >= v:
            return int(m * base)
    return int(10 * base)


def nice_step(rough):
    base = 10 ** math.floor(math.log10(rough))
    f = rough / base
    for n in (1, 2, 2.5, 5, 10):
        if f <= n:
            return n * base
    return 10 * base


def month_ticks(start, end):
    ticks, y, m = [], start.year, start.month
    if start.day > 1:
        m += 1
        if m > 12:
            m, y = 1, y + 1
    while (y, m) <= (end.year, end.month):
        ticks.append(datetime(y, m, 1, tzinfo=timezone.utc))
        m += 1
        if m > 12:
            m, y = 1, y + 1
    return ticks


def fmt_k(v):
    return f"{v / 1000:g}K" if v >= 1000 else f"{int(v)}"


def roughen(pts, rng, rough=1.3, seg=15):
    """Turn a polyline into a hand-drawn wobbly one (perpendicular jitter)."""
    out = []
    for (x0, y0), (x1, y1) in zip(pts, pts[1:]):
        dx, dy = x1 - x0, y1 - y0
        dist = math.hypot(dx, dy) or 1
        nx, ny = -dy / dist, dx / dist
        steps = max(1, int(dist / seg))
        for s in range(steps):
            t = s / steps
            j = rng.uni(-rough, rough) if (out or s) else rng.uni(-rough * 0.3, rough * 0.3)
            out.append((x0 + dx * t + nx * j, y0 + dy * t + ny * j))
    out.append(pts[-1])
    return out


def path_d(pts):
    return "M " + " L ".join(f"{x:.1f},{y:.1f}" for x, y in pts)


def sketch(pts, rng, stroke, width, closed=False, passes=2, opacities=(1, 0.4)):
    if closed:
        pts = pts + [pts[0]]
    out = []
    for i in range(passes):
        d = path_d(roughen(pts, rng))
        out.append(
            f'<path d="{d}" fill="none" stroke="{stroke}" stroke-width="{width}" '
            f'stroke-linecap="round" stroke-linejoin="round" opacity="{opacities[i]}"/>'
        )
    return "".join(out)


def star_path(cx, cy, r):
    ri = r * 0.42
    pts = []
    for i in range(10):
        a = -math.pi / 2 + i * math.pi / 5
        rr = r if i % 2 == 0 else ri
        pts.append((cx + rr * math.cos(a), cy + rr * math.sin(a)))
    return "M " + " L ".join(f"{x:.1f},{y:.1f}" for x, y in pts) + " Z"


def txt(x, y, s, size, fill, anchor="start", rot=None):
    s = s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
    r = f' transform="rotate(-90 {x:.1f} {y:.1f})"' if rot else ""
    return (f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" fill="{fill}" '
            f'text-anchor="{anchor}" font-family="{FONT_STACK}"{r}>{s}</text>')


def build_svg(stamps):
    total = len(stamps)
    t0, t1 = stamps[0], stamps[-1]
    span = max((t1 - t0).total_seconds(), 1)
    ymax = nice_ceiling(total)
    rng = Rng()

    def sx(dt):
        return L + (dt - t0).total_seconds() / span * PW

    def sy(v):
        return T + PH - v / ymax * PH

    ox, oy = L, T + PH  # origin

    # Cumulative curve, downsampled to ~80 points.
    n = min(total, 80)
    curve = [(sx(stamps[round(i * (total - 1) / (n - 1))]), sy(round(i * (total - 1) / (n - 1)) + 1))
             for i in range(n)]
    curve.append((sx(t1), sy(total)))

    parts = []

    # Axes (hand-drawn), with a slight overshoot.
    parts.append(sketch([(ox, oy + 6), (ox, T - 8)], rng, GRID, 2.2))
    parts.append(sketch([(ox - 6, oy), (ox + PW + 10, oy)], rng, GRID, 2.2))

    # Y ticks + labels.
    step = nice_step(ymax / 3)
    v = step
    while v <= ymax + 1:
        yy = sy(v)
        parts.append(sketch([(ox - 7, yy), (ox, yy)], rng, GRID, 1.8, opacities=(1, 0.4)))
        parts.append(txt(ox - 13, yy + 6, fmt_k(v), 17, INK, "end"))
        v += step

    # X ticks + month labels.
    for dt in month_ticks(t0, t1):
        xx = sx(dt)
        parts.append(sketch([(xx, oy), (xx, oy + 7)], rng, GRID, 1.8, opacities=(1, 0.4)))
        parts.append(txt(xx, oy + 28, dt.strftime("%B"), 17, INK, "middle"))

    # Axis titles.
    parts.append(txt(30, T + PH / 2, "GitHub Stars", 18, INK, "middle", rot=True))
    parts.append(txt(L + PW / 2, H - 16, "Date", 19, INK, "middle"))

    # The star curve.
    parts.append(sketch(curve, rng, RED, 3, opacities=(1, 0.35)))

    # Legend box (top-left inside the plot).
    lx, ly = L + 18, T + 16
    lw, lh = int(len(REPO) * 8.1) + 52, 32
    parts.append(f'<rect x="{lx}" y="{ly}" width="{lw}" height="{lh}" rx="7" fill="#ffffff" opacity="0.92"/>')
    parts.append(sketch([(lx, ly), (lx + lw, ly), (lx + lw, ly + lh), (lx, ly + lh)],
                        rng, INK, 1.6, closed=True, opacities=(1, 0.35)))
    parts.append(f'<rect x="{lx + 13}" y="{ly + lh / 2 - 6:.0f}" width="12" height="12" rx="2" fill="{RED}"/>')
    parts.append(txt(lx + 33, ly + lh / 2 + 6, REPO, 17, INK))

    # Title: badge + "Star History".
    title = "Star History"
    tw = len(title) * 11.2
    gx = (W - (28 + 10 + tw)) / 2
    parts.append(f'<rect x="{gx:.1f}" y="24" width="28" height="28" rx="8" fill="{BADGE}"/>')
    parts.append(f'<path d="{star_path(gx + 14, 38, 8.5)}" fill="#ffffff"/>')
    parts.append(txt(gx + 38, 47, title, 24, INK))

    # Watermark.
    parts.append(f'<path d="{star_path(W - R - 128, H - 18, 6.5)}" fill="{GREEN}"/>')
    parts.append(txt(W - R, H - 13, "star-history-svg", 14, MUTED, "end"))

    font_b64 = base64.b64encode(FONT.read_bytes()).decode()
    style = (f"@font-face{{font-family:'PatrickHand';font-style:normal;"
             f"src:url(data:font/woff2;base64,{font_b64}) format('woff2');}}")

    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}">'
        f'<defs><style>{style}</style></defs>'
        f'<rect x="1" y="1" width="{W - 2}" height="{H - 2}" rx="12" fill="#ffffff" stroke="#e6e8eb"/>'
        + "".join(parts) + "</svg>\n"
    )


def main():
    repo = sys.argv[1] if len(sys.argv) > 1 else REPO
    stamps = fetch_starred_at(repo)
    if not stamps:
        print("No stargazers found.", file=sys.stderr)
        return 1
    OUT.write_text(build_svg(stamps), encoding="utf-8")
    print(f"Wrote {OUT} ({len(stamps):,} stars, latest {stamps[-1].date()})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
