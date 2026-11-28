#!/usr/bin/env python3
"""Extract numeric telemetry from a GroundStation recording CSV export
(/api/recordings/{id}/csv, optionally gzipped) into a compact .npz:
one array pair per (sender, data_type): <key>__t (received ms, int64),
<key>__s (source ms), <key>__v (float64 matrix, NaN padded).
Usage: extract.py RECORDING.csv[.gz] OUT.npz
"""
import csv, gzip, json, sys, collections
import numpy as np

# Exports made before the ox tank PT rename name it FUEL_TANK_PRESSURE.
LEGACY_TYPE_NAMES = {'FUEL_TANK_PRESSURE': 'OX_TANK_PRESSURE'}

def main(src, out):
    op = gzip.open if src.endswith('.gz') else open
    t = collections.defaultdict(list); s = collections.defaultdict(list); v = collections.defaultdict(list)
    text = collections.defaultdict(list)
    csv.field_size_limit(1 << 24)
    with op(src, 'rt', newline='') as f:
        r = csv.reader(f); next(r)
        for row in r:
            key = f"{row[3]}|{LEGACY_TYPE_NAMES.get(row[4], row[4])}"
            try:
                vals = json.loads(row[5]) if row[5] else []
            except ValueError:
                vals = []
            if not isinstance(vals, list):
                vals = [vals]
            nums = [float(x) if isinstance(x, (int, float)) else np.nan for x in vals]
            if not nums:
                if len(text[key]) < 2000:
                    text[key].append((int(row[1]), row[6][:200]))
                continue
            t[key].append(int(row[1])); s[key].append(int(row[2]) if row[2] else -1); v[key].append(nums)
    arrays = {}
    for key in t:
        w = max(len(x) for x in v[key])
        m = np.full((len(v[key]), w), np.nan)
        for i, x in enumerate(v[key]):
            m[i, :len(x)] = x
        arrays[key + '__t'] = np.asarray(t[key], np.int64)
        arrays[key + '__s'] = np.asarray(s[key], np.int64)
        arrays[key + '__v'] = m
    arrays['__text__'] = np.array(json.dumps(text))
    np.savez_compressed(out, **arrays)

if __name__ == '__main__':
    main(sys.argv[1], sys.argv[2])
