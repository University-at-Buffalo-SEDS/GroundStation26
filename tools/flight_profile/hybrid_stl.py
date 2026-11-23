#!/usr/bin/env python3
"""Star-grain parameters from the grain STL (motor_design/star_grain.stl), docs/hybrid-motor-model.md section 3.2.

Slices the (binary, mm) STL with planes normal to its long axis, chains the cut segments into loops, takes the
innermost loop as the port outline and the outermost as the grain OD, and fits the motor model's star
(N points, tip radius, root radius, sharp straight sides). Prints the parameters and how well the parametric
star reproduces the slice (area, perimeter); tests/test_hybrid_motor.py runs the same check.

  python3 tools/flight_profile/hybrid_stl.py [path/to/star_grain.stl]
"""
from __future__ import annotations

import json
import math
import struct
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
STL = HERE / 'motor_design' / 'star_grain.stl'


def read_stl(path: Path) -> list[tuple[tuple[float, float, float], ...]]:
    data = path.read_bytes()
    n = struct.unpack_from('<I', data, 80)[0]
    if 84 + 50 * n != len(data):
        raise ValueError('not a binary STL')
    tris = []
    for i in range(n):
        v = struct.unpack_from('<12f', data, 84 + 50 * i)
        tris.append(((v[3], v[4], v[5]), (v[6], v[7], v[8]), (v[9], v[10], v[11])))
    return tris


def long_axis(tris) -> int:
    lo = [min(p[k] for t in tris for p in t) for k in range(3)]
    hi = [max(p[k] for t in tris for p in t) for k in range(3)]
    return max(range(3), key=lambda k: hi[k] - lo[k])


def slice_loops(tris, axis: int, h: float) -> list[list[tuple[float, float]]]:
    """Closed loops (in the plane's other two coordinates, axis-centred later) of the cut at height h."""
    a, b = [k for k in range(3) if k != axis]
    segs = []
    for t in tris:
        pts = []
        for i in range(3):
            p, q = t[i], t[(i + 1) % 3]
            dp, dq = p[axis] - h, q[axis] - h
            if (dp < 0) != (dq < 0):
                u = dp / (dp - dq)
                pts.append((p[a] + u * (q[a] - p[a]), p[b] + u * (q[b] - p[b])))
        if len(pts) == 2:
            segs.append((pts[0], pts[1]))
    key = lambda p: (round(p[0], 4), round(p[1], 4))  # noqa: E731
    nxt = {}
    for s, e in segs:
        nxt.setdefault(key(s), []).append(e)
        nxt.setdefault(key(e), []).append(s)
    used = set()
    loops = []
    for s, e in segs:
        if (key(s), key(e)) in used:
            continue
        loop = [s, e]
        used.add((key(s), key(e)))
        used.add((key(e), key(s)))
        while True:
            cand = [c for c in nxt[key(loop[-1])] if (key(loop[-1]), key(c)) not in used]
            if not cand:
                break
            c = cand[0]
            used.add((key(loop[-1]), key(c)))
            used.add((key(c), key(loop[-1])))
            if key(c) == key(loop[0]):
                break
            loop.append(c)
        if len(loop) > 2:
            loops.append(loop)
    return loops


def area(loop) -> float:
    return abs(sum(x0 * y1 - x1 * y0 for (x0, y0), (x1, y1) in zip(loop, loop[1:] + loop[:1]))) / 2.0


def perimeter(loop) -> float:
    return sum(math.dist(p, q) for p, q in zip(loop, loop[1:] + loop[:1]))


def analyse(path: Path = STL, fraction: float = 0.5) -> dict:
    tris = read_stl(path)
    axis = long_axis(tris)
    zs = [p[axis] for t in tris for p in t]
    z0, z1 = min(zs), max(zs)
    loops = slice_loops(tris, axis, z0 + fraction * (z1 - z0))
    loops.sort(key=area)
    port, outer = loops[0], loops[-1]
    cx = sum(p[0] for p in outer) / len(outer)
    cy = sum(p[1] for p in outer) / len(outer)
    rad = [math.hypot(x - cx, y - cy) for x, y in port]
    r_tip, r_root = max(rad), min(rad)
    # points: local maxima of the radius around the outline (by angle)
    ang = sorted((math.atan2(y - cy, x - cx), math.hypot(x - cx, y - cy)) for x, y in port)
    thr = 0.5 * (r_tip + r_root)
    crossings = sum(1 for (_, r0), (_, r1) in zip(ang, ang[1:] + ang[:1]) if (r0 < thr) != (r1 < thr))
    n = crossings // 2
    od = 2.0 * max(math.hypot(x - cx, y - cy) for x, y in outer)  # the tessellated circle's vertices lie on it
    half = math.pi / n
    side = math.sqrt(r_tip ** 2 + r_root ** 2 - 2 * r_tip * r_root * math.cos(half))
    return {
        'source': str(path.name),
        'axis': 'xyz'[axis],
        'length_mm': z1 - z0,
        'slice_at_mm': z0 + fraction * (z1 - z0),
        'points': n,
        'tip_radius_mm': r_tip,
        'root_radius_mm': r_root,
        'grain_od_mm': od,
        'port_area_mm2': area(port),
        'port_perimeter_mm': perimeter(port),
        'star_area_mm2': n * r_tip * r_root * math.sin(half),
        'star_perimeter_mm': 2 * n * side,
        'web_beyond_tips_mm': od / 2 - r_tip,
        'web_behind_valleys_mm': od / 2 - r_root,
    }


def main(argv):
    path = Path(argv[0]) if argv else STL
    print(json.dumps({k: (round(v, 3) if isinstance(v, float) else v) for k, v in analyse(path).items()}, indent=1))
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
