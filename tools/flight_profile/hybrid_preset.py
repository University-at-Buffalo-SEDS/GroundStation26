#!/usr/bin/env python3
"""Compact model files from the team's motor design bundle (.myrap) for the hybrid motor model.

Input: the unzipped bundle (preset.yaml + artifacts/cea_surface.npz + artifacts/literal_material_field.npz),
"Current single-port motor baseline", format hybrid_sim_fixed_motor_replay_preset_v1; the preset itself is
committed as motor_design/current_single_port_baseline.preset.yaml. The npz files stay out of git (13.5 MB);
this script records their sha256 and writes what the station and the simulator need:

  motors/sweet_v6_cea.json         NASA CEA surface sliced to the preset nozzle's expansion ratio: c*, chamber
                                   gamma, vacuum C_F and exit pressure ratio over chamber pressure (all 20) x
                                   O/F (all 12) x N2O enthalpy (9 liquid + 6 vapour of 36) x ABS mass fraction of
                                   the fuel (all 7). Validated against the full surface at the dropped enthalpies
                                   and expansion-ratio neighbours (max errors in the file).
  motors/sweet_v6_grain.json       ABS share of the port surface vs radius, from the voxelised gyroid print
                                   (literal_material_field.npz, 0.35 mm voxels), renormalised so the grain holds
                                   the preset's ABS mass.

Needs numpy and PyYAML (jupiter: .workspace-progress venv); the motor model itself needs neither.

  python3 tools/flight_profile/hybrid_preset.py /path/to/unzipped/myrap
"""
from __future__ import annotations

import hashlib
import json
import math
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
CEA_OUT = HERE / 'motors' / 'sweet_v6_cea.json'
GRAIN_OUT = HERE / 'motors' / 'sweet_v6_grain.json'
LIQUID_H = list(range(0, 18, 2)) + [17]  # liquid N2O states (about -14..+30 C); every other one, plus the warmest
VAPOUR_H = [18, 21, 24, 27, 30, 35]  # saturated-vapour states (the tail)


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def sig(x, n=6):
    return float(f'{x:.{n}g}')


def build(bundle: Path):
    import numpy as np
    import yaml
    preset = yaml.safe_load((bundle / 'preset.yaml').read_text())
    case = preset['case_snapshot_si']
    cea_path = bundle / preset['cea_surface']['member']
    field_path = bundle / preset['literal_replay']['material_field']['member']
    assert sha(cea_path) == preset['cea_surface']['sha256'], 'cea_surface.npz does not match the preset sha256'
    assert sha(field_path) == preset['literal_replay']['material_field']['sha256'], 'material field sha256'
    z = np.load(cea_path)
    eps_axis = z['expansion_ratios']
    noz = case['nozzle']
    eps = (noz['exit_diameter_m'] / noz['throat_diameter_m']) ** 2
    j = int(np.searchsorted(eps_axis, eps)) - 1
    w = (eps - eps_axis[j]) / (eps_axis[j + 1] - eps_axis[j])

    def at_eps(arr):  # linear in eps for cf, log-linear for the exit pressure ratio
        return arr[..., j] * (1 - w) + arr[..., j + 1] * w

    cf = at_eps(z['cf_vacuum'])
    pe = np.exp(np.log(z['exit_pressure_ratio'][..., j]) * (1 - w) + np.log(z['exit_pressure_ratio'][..., j + 1]) * w)
    # The surface's C_F / exit-pressure columns flip between two values (about 1.38 and 1.30 at this expansion
    # ratio) in isolated cells (see full_surface_glitches): a 3-point median along the enthalpy axis, within each
    # phase block, repairs them (c* and gamma have no such cells).
    def despike(arr):
        out = arr.copy()
        for lo, hi in ((0, 18), (18, 36)):
            for i in range(lo, hi):
                a0, a1 = max(lo, i - 1), min(hi - 1, i + 1)
                if a1 - a0 == 2:
                    out[:, :, i, :] = np.median(arr[:, :, a0:a1 + 1, :], axis=2)
        return out
    cf_raw, pe_raw = cf, pe
    cf, pe = despike(cf), despike(pe)
    repaired = int(np.sum(np.abs(cf_raw / cf - 1) > 0.01))
    hs = LIQUID_H + VAPOUR_H
    fields = {'cstar': z['cstar_m_s'], 'gamma': z['chamber_gamma'], 'cf_vac': cf, 'pe_pc': pe}
    h_axis = z['oxidizer_assigned_enthalpies_cal_mol']

    # validation: interpolate the compact table (linear in h between kept states) at the dropped states
    def interp_h(arr, hv):
        kept = np.array([h_axis[i] for i in hs])
        order = np.argsort(kept)
        kept = kept[order]
        sub = arr[:, :, [hs[i] for i in order], :]
        k = int(np.clip(np.searchsorted(kept, hv) - 1, 0, len(kept) - 2))
        u = (hv - kept[k]) / (kept[k + 1] - kept[k])
        return sub[:, :, k, :] * (1 - u) + sub[:, :, k + 1, :] * u
    errs = {}
    for name, arr in fields.items():
        rel = []
        for i in range(36):
            block = LIQUID_H if i <= 17 else VAPOUR_H
            if i in hs or not (min(h_axis[b] for b in block) <= h_axis[i] <= max(h_axis[b] for b in block)):
                continue
            rel.append(np.abs(interp_h(arr, h_axis[i]) / arr[:, :, i, :] - 1).ravel())
        rel = np.concatenate(rel)
        errs[name] = {'max': float(rel.max()), 'p99': float(np.percentile(rel, 99)), 'p999': float(np.percentile(rel, 99.9))}
    # isolated CEA glitches in the full surface: cells more than 1 % off the mean of their enthalpy neighbours
    glitches = []
    cfv = z['cf_vacuum'][..., j]
    for i in range(1, 35):
        if i in (17, 18):
            continue
        dev = np.abs(cfv[:, :, i, :] / (0.5 * (cfv[:, :, i - 1, :] + cfv[:, :, i + 1, :])) - 1)
        for ix in zip(*np.nonzero(dev > 0.01)):
            glitches.append({'pc_pa': float(z['pressures_pa'][ix[0]]), 'of': float(z['of_ratios'][ix[1]]),
                             'h_index': i, 'abs': float(z['fuel_abs_mass_fractions'][ix[2]]),
                             'kept': i in hs, 'cf_vac': float(cfv[ix[0], ix[1], i, ix[2]])})
    # expansion-ratio interpolation error: estimate eps_axis[j+1] from j and j+2 (same scheme)
    k2 = j + 2
    mid = z['cf_vacuum'][..., j] * 0.5 + z['cf_vacuum'][..., k2] * 0.5
    errs['cf_vac_eps_interp'] = float(np.max(np.abs(mid / z['cf_vacuum'][..., j + 1] - 1)))

    def flat(arr):
        sub = arr[:, :, hs, :]
        return [sig(x, 5) for x in sub.ravel().tolist()]
    manifest = json.loads(str(z['manifest']))
    cea = {
        '_about': 'Generated by tools/flight_profile/hybrid_preset.py from the motor design bundle '
                  '"Current single-port motor baseline" (artifacts/cea_surface.npz, NASA CEA FCEA2). Arrays are '
                  'row-major [pc][of][h][abs]. Do not edit by hand.',
        'source': {'member': preset['cea_surface']['member'], 'sha256': preset['cea_surface']['sha256'],
                   'source_name': preset['cea_surface']['source_name'], 'format': manifest['format'],
                   'cea_executable_sha256': manifest['cea_executable_sha256'],
                   'cea_thermo_library_sha256': manifest['cea_thermo_library_sha256'],
                   'fuel': f"C{manifest['fuel_carbon_atoms']:g}H{manifest['fuel_hydrogen_atoms']:g} "
                           f"dHf {manifest['fuel_heat_of_formation_cal_mol']} cal/mol (paraffin surrogate)",
                   'abs': f"{manifest['abs_fuel_name']} C3.85H4.85N0.43 dHf {manifest['abs_fuel_heat_of_formation_cal_mol']:.1f} cal/mol",
                   'n2o_enthalpy_reference': {k: manifest[k] for k in manifest if k.startswith('n2o_')},
                   'preset_fingerprint_sha256': preset['fingerprint_sha256']},
        'expansion_ratio': sig(eps, 8),
        'expansion_ratio_source': f"preset nozzle: exit {noz['exit_diameter_m']} m / throat {noz['throat_diameter_m']} m; "
                                  f"interpolated between the surface's {eps_axis[j]} and {eps_axis[j + 1]}",
        'pc_pa': [sig(x, 10) for x in z['pressures_pa'].tolist()],
        'of': z['of_ratios'].tolist(),
        'h_cal_mol': [sig(h_axis[i], 9) for i in hs],
        'h_phase': ['liquid'] * len(LIQUID_H) + ['vapour'] * len(VAPOUR_H),
        'abs_fraction': z['fuel_abs_mass_fractions'].tolist(),
        'fields': {k: flat(v) for k, v in fields.items()},
        'validation_rel_error': {k: ({kk: sig(vv, 3) for kk, vv in v.items()} if isinstance(v, dict) else sig(v, 3)) for k, v in errs.items()},
        'validation_note': 'compact (linear in enthalpy between kept states) vs the full surface at the dropped states; the max is set by isolated CEA cells listed in full_surface_glitches (the p99 is the interpolation error)',
        'full_surface_glitches': glitches,
        'despiked_cells': repaired,
    }
    # grain: ABS share of the surface vs radius
    f = np.load(field_path)
    occ = np.unpackbits(f['packed_occupancy'], axis=2)[:, :, :int(f['shape_zyx'][2])]
    o, v = f['origin_xyz_m'], f['voxel_size_xyz_m']
    xs = o[0] + (np.arange(occ.shape[2]) + 0.5) * v[0]
    ys = o[1] + (np.arange(occ.shape[1]) + 0.5) * v[1]
    X, Y = np.meshgrid(xs, ys)
    R = np.hypot(X, Y)
    share = occ.mean(axis=0)
    g = case['grain']
    r0, r1 = g['initial_port_diameter_m'] / 2, g['outer_diameter_m'] / 2
    width = 0.0007
    edges = np.arange(r0, r1 + 1e-9, width)
    if edges[-1] < r1 - 1e-6:
        edges = np.append(edges, r1)
    raw = []
    for a, b in zip(edges, edges[1:]):
        m = (R >= a) & (R < b)
        raw.append(float(share[m].mean()) if m.any() else 0.0)
    # renormalise so the annulus holds the preset ABS mass
    vol = math.pi * (r1 ** 2 - r0 ** 2) * g['length_m']
    abs_vol = g['abs_matrix_mass_kg'] / preset['grain_recipe']['abs_density_kg_m3']
    raw_vol = sum(fr * math.pi * (b ** 2 - a ** 2) * g['length_m'] for fr, a, b in zip(raw, edges, edges[1:]))
    k = abs_vol / raw_vol
    grain = {
        '_about': 'Generated by tools/flight_profile/hybrid_preset.py: ABS (gyroid matrix) share of the grain '
                  'volume, hence of the port surface, in radial shells from the initial port to the grain OD, '
                  'averaged over the print height and angle. Do not edit by hand.',
        'source': {'member': preset['literal_replay']['material_field']['member'],
                   'sha256': preset['literal_replay']['material_field']['sha256'],
                   'print': preset['literal_replay']['source_name'], 'voxel_m': v.tolist()},
        'r_edges_m': [sig(x, 8) for x in edges.tolist()],
        'abs_share': [sig(min(fr * k, 1.0), 5) for fr in raw],
        'raw_voxel_share': [sig(fr, 5) for fr in raw],
        'renormalised_by': sig(k, 5),
        'renormalisation': f'voxel ABS volume {raw_vol * 1e6:.1f} cm^3 scaled to the preset {g["abs_matrix_mass_kg"]:.4f} kg '
                           f'at {preset["grain_recipe"]["abs_density_kg_m3"]} kg/m^3 ({abs_vol * 1e6:.1f} cm^3); the '
                           'field reports a 9 % voxelisation volume error',
    }
    return cea, grain


def main(argv):
    if not argv:
        print(__doc__)
        return 2
    cea, grain = build(Path(argv[0]))
    CEA_OUT.write_text(json.dumps(cea, separators=(',', ':')) + '\n')
    GRAIN_OUT.write_text(json.dumps(grain, indent=1) + '\n')
    print(f'wrote {CEA_OUT} ({CEA_OUT.stat().st_size} B) and {GRAIN_OUT}')
    print('validation', cea['validation_rel_error'], 'glitches', len(cea['full_surface_glitches']), 'despiked', cea['despiked_cells'])
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
