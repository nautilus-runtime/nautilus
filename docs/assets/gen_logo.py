#!/usr/bin/env python3
"""Regenerate nautilus logo SVGs (v10 — proportional wall + chamber septa).
Shell wall = filled band whose thickness is proportional to radius (outer
whorls fat, inner thin — the reviewer-approved geometry). Septa = full-span
arcs across the band, LOG-spaced (dense at core), none on the outer body
chamber; hard aperture cut keeps the silhouette round.
Run: python3 gen_logo.py"""
import math, os

# --- parameters ---
GROW = 2.2
LOG_B = math.log(GROW) / (2 * math.pi)
VIEW = 200
MARGIN = 10
A = 4.6
BAND = 0.40                      # wall thickness fraction; < 1-1/GROW=0.545 keeps visible channel
FLOOR = 3.5                      # min wall in fitted units (keeps innermost printable)
LIGHT = "#CE422B"
DARK_STROKE, DARK_SLIT = "#FF8A73", "#1a1412"

def pt(t):
    r = A * math.exp(LOG_B * t)
    return (r * math.cos(t), r * math.sin(t))

def spiral(t0, t1, k=1.0, n=700):
    return [(pt(t)[0]*k, pt(t)[1]*k) for t in (t0 + (t1 - t0) * i / n for i in range(n + 1))]

def make_fit(allp):
    xs = [p[0] for p in allp]; ys = [p[1] for p in allp]
    s = min((VIEW - 2*MARGIN) / (max(xs)-min(xs)), (VIEW - 2*MARGIN) / (max(ys)-min(ys)))
    ox = VIEW/2 - (min(xs)+max(xs))/2 * s
    oy = VIEW/2 - (min(ys)+max(ys))/2 * s
    return s, ox, oy

def fxy(s, ox, oy, x, y):
    return (round(x*s+ox,1), round(y*s+oy,1))

def path(pts, step=1):
    pts = pts[::step] if step > 1 else pts
    return f"M {pts[0][0]} {pts[0][1]}" + "".join(f" L {x} {y}" for x, y in pts[1:])

def band_path(s, ox, oy, t0, t1, floor):
    """Filled wall band; thickness = max(BAND*r, floor) fitted units."""
    outer, inner = [], []
    n = 700
    for i in range(n + 1):
        t = t0 + (t1 - t0) * i / n
        r = A * math.exp(LOG_B * t)
        x, y = r * math.cos(t), r * math.sin(t)
        thick = max(BAND * r * s, floor)
        rr = max(r * s - thick, 0.8)
        outer.append(fxy(s, ox, oy, x, y))
        inner.append(fxy(s, ox, oy, x * rr/(r*s), y * rr/(r*s)))
    return (path(outer) + f" L {inner[-1][0]} {inner[-1][1]}" + "".join(
        f" L {x} {y}" for x, y in reversed(inner[:-1])) + " Z")

def septa(s, ox, oy, t0, t1, count, width, floor, keeplast=0.12):
    """Full-span septa arcs across the wall band, distributed PER FULL TURN
    (m bars per turn, so every VISIBLE whorl gets septa). Start after the
    innermost whorl (kept solid = clean core), skip the final open body
    chamber. Bow BACKWARD (convex toward the core, like real nautilus
    septa)."""
    out = []
    ta = t0 + 1.25 * math.pi     # keep the innermost whorl solid (clean center)
    tb = t1 - keeplast * 2 * math.pi - 0.18
    if tb <= ta: return ""
    per = count / ((tb - ta) / (2 * math.pi))   # bars per full turn
    step = 2 * math.pi / max(per, 1)
    i = 0
    a = ta + step / 2
    while a < tb:
        r = A * math.exp(LOG_B * a)
        thick = max(BAND * r * s, floor)
        rr = r * s
        r0 = max(rr - thick, 0.8)
        r1 = rr * 0.96                  # stop just short of outer wall
        ixx, iyy = fxy(s, ox, oy, math.cos(a)*r0/s, math.sin(a)*r0/s)
        exx, eyy = fxy(s, ox, oy, math.cos(a)*r1/s, math.sin(a)*r1/s)
        rm = (r0 + r1) / 2
        cxx, cyy = fxy(s, ox, oy, math.cos(a-0.20)*rm/s, math.sin(a-0.20)*rm/s)
        out.append(f"M {ixx} {iyy} Q {cxx} {cyy} {exx} {eyy}")
        a += step
        i += 1
    return "\n      ".join(out)

def template(d_band, d_septa, stroke, slit, sw):
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {VIEW} {VIEW}">
  <path d="{d_band}" fill="{stroke}"/>
  <path d="{d_septa}" stroke="{slit}" stroke-width="{sw}" fill="none" stroke-linecap="round"/>
</svg>
'''

# ---- full mark: 3.0 turns ----
T0, T1 = -0.2 * math.pi, 5.8 * math.pi
s, ox, oy = make_fit(spiral(T0, T1))
d_band = band_path(s, ox, oy, T0, T1, FLOOR)
d_septa = septa(s, ox, oy, T0, T1, count=8, width=5.0, floor=FLOOR, keeplast=0.12)

svg_full = template(d_band, d_septa, LIGHT, "#ffffff", 5.0)
svg_dark = template(d_band, d_septa, DARK_STROKE, DARK_SLIT, 5.0)

# ---- icon: same aperture orientation as full (T1 ≡ logo T1 mod 2π), 2.5 turns ----
IT0, IT1 = 5.8 * math.pi - 2 * math.pi - 5 * math.pi, 5.8 * math.pi - 2 * math.pi  # -1.2π .. 3.8π
si, oxi, oyi = make_fit(spiral(IT0, IT1))
d_iband = band_path(si, oxi, oyi, IT0, IT1, 5.0)
d_isepta = septa(si, oxi, oyi, IT0, IT1, count=5, width=5.2, floor=5.0, keeplast=0.12)

svg_icon = template(d_iband, d_isepta, LIGHT, "#ffffff", 5.2)

# ---- 32px cut: 2.0 turns, 3 bold septa ----
J0, J1 = 4.0 * math.pi - 2 * math.pi - 4 * math.pi, 4.0 * math.pi - 2 * math.pi  # -2π .. 2π
sj, oxj, oyj = make_fit(spiral(J0, J1))
d_jband = band_path(sj, oxj, oyj, J0, J1, 8.0)
d_jsepta = septa(sj, oxj, oyj, J0, J1, count=3, width=5.6, floor=8.0, keeplast=0.1)

svg_32 = template(d_jband, d_jsepta, LIGHT, "#ffffff", 5.6)

# ---- 16px cut: 1.5 turns, no septa ----
K0, K1 = 0.35 * math.pi, 3.35 * math.pi
sk, oxk, oyk = make_fit(spiral(K0, K1))
d_kband = band_path(sk, oxk, oyk, K0, K1, 13.0)

svg_16 = template(d_kband, "", LIGHT, "#ffffff", 5.0)

out = os.path.dirname(os.path.abspath(__file__))
for name, content in [("logo.svg", svg_full), ("logo-dark.svg", svg_dark), ("logo-icon.svg", svg_icon),
                      ("icon-32.svg", svg_32), ("icon-16.svg", svg_16)]:
    with open(os.path.join(out, name), "w") as f:
        f.write(content)
    print("wrote", name)
