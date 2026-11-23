#!/usr/bin/env python3
"""Paraffin (and ABS) / N2O thermochemistry table for the hybrid motor model.

Chemical equilibrium as NASA CEA computes it (Gordon & McBride, NASA RP-1311,
1994): adiabatic, isobaric combustion (HP equilibrium) and isentropic
expansion with shifting equilibrium to the throat (SP equilibrium, the throat
is where the mass flux peaks). Thermodynamic data: the NASA Glenn
coefficients CEA itself uses (McBride, Zehe & Gordon, NASA/TP-2002-211556),
as shipped with Cantera (nasa_gas.yaml, nasa_condensed.yaml, including solid
carbon for the fuel-rich end). Cantera does the minimisation; this script
only builds the reactant enthalpy and finds the throat.

Reactants:
  * N2O liquid, saturated at 0 deg C: the ideal-gas N2O enthalpy (NASA Glenn,
    dHf(298) = +82.05 kJ/mol) minus the ESDU 91022 latent heat at 273.15 K.
    Over -20..+30 deg C the c* changes by under 0.5 %.
  * Paraffin wax as n-C32H66 (s), dHf = -224.2 kcal/mol (-938.1 kJ/mol), the
    composition and enthalpy used for paraffin in Karabeyoglu, Cantwell et al.
    (Stanford; e.g. AIAA 2003-4589, JPP 20(6) 2004). Group additivity
    (n-alkane gas -20.6 kJ/mol per CH2 plus sublimation) gives -940 kJ/mol.
  * ABS as C3.85H4.85N0.43, dHf = +62.63 kJ/mol, the formula Whitmore et al.
    (Utah State, JPP 29(3) 2013) use for ABS in CEA.

Output: motors/paraffin_n2o_cstar.json, a grid of O/F x Pc with c* (m/s),
the effective throat gamma (from the throat pressure ratio), the chamber
temperature and the molar mass, for paraffin and for ABS. The motor model
interpolates bilinearly (O/F linear, Pc in log).

Needs cantera (pip install cantera); the motor model itself does not.

  python3 tools/flight_profile/hybrid_cstar.py          # rewrite the table
"""
from __future__ import annotations

import json
import math
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE / 'motors' / 'paraffin_n2o_cstar.json'

OF_GRID = [1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0, 5.5, 6.0, 6.5, 7.0, 7.5, 8.0, 8.5, 9.0, 10.0, 11.0,
           12.0, 14.0, 16.0, 20.0, 30.0, 50.0, 100.0]
PC_GRID_BAR = [1.5, 3.0, 5.0, 10.0, 20.0, 35.0, 50.0]

# ESDU 91022 (as in n2o.rs / native_physics.rs)
TC, PCRIT, RHOC = 309.57, 7.251e6, 452.0


def n2o_latent_j_per_kg(t):
    x = 1.0 - t / TC
    hl = -200.0 + 116.043 * x ** (1 / 3) - 917.225 * x ** (2 / 3) + 794.779 * x - 589.587 * x ** (4 / 3)
    hv = -200.0 + 440.055 * x ** (1 / 3) - 459.701 * x ** (2 / 3) + 434.081 * x - 485.338 * x ** (4 / 3)
    return (hv - hl) * 1000.0


FUELS = {
    # name: (elements per formula unit, molar mass kg/mol, dHf J/mol)
    'paraffin': ({'C': 32.0, 'H': 66.0}, None, -938.1e3),
    'abs': ({'C': 3.85, 'H': 4.85, 'N': 0.43}, None, 62.63e3),
}
AW = {'C': 12.011e-3, 'H': 1.008e-3, 'O': 15.999e-3, 'N': 14.007e-3}


def build():
    import cantera as ct
    gas_names = ['CO', 'CO2', 'H2O', 'H2', 'O2', 'OH', 'H', 'O', 'N2', 'NO', 'N', 'CH4', 'C2H2,acetylene', 'C2H4',
                 'HCN', 'NH3', 'HO2', 'NO2', 'N2O', 'C', 'HCO', 'CH3', 'CN', 'NH', 'NH2', 'H2O2', 'C2H6', 'C2H']
    allg = {s.name: s for s in ct.Species.list_from_file('nasa_gas.yaml')}
    gas = ct.Solution(thermo='ideal-gas', species=[allg[n] for n in gas_names])
    gr = ct.Solution(thermo='fixed-stoichiometry',
                     species=[s for s in ct.Species.list_from_file('nasa_condensed.yaml') if s.name == 'C(gr)'])
    n2o = ct.Solution(thermo='ideal-gas', species=[allg['N2O']])
    t_ox = 273.15
    n2o.TP = t_ox, 101325.0
    m_n2o = n2o.mean_molecular_weight / 1000.0
    h_ox = n2o.enthalpy_mass - n2o_latent_j_per_kg(t_ox)  # J/kg, liquid at 0 C

    def point(fuel, of, pc):
        el, _, dhf = FUELS[fuel]
        mw_f = sum(AW[k] * v for k, v in el.items())
        mf, mo = 1.0 / (1.0 + of), of / (1.0 + of)
        nf, no = mf / mw_f, mo / m_n2o
        atoms = {'C': nf * el.get('C', 0.0), 'H': nf * el.get('H', 0.0), 'N': nf * el.get('N', 0.0) + 2 * no, 'O': no}
        h0 = mf * dhf / mw_f + mo * h_ox  # J per kg of mixture (1 kg basis)
        mix = ct.Mixture([(gas, 1.0), (gr, 0.0)])
        moles = [0.0] * mix.n_species
        moles[mix.species_index(0, 'N2')] = atoms['N'] / 2
        moles[mix.species_index(0, 'H2')] = atoms['H'] / 2
        moles[mix.species_index(0, 'O2')] = atoms['O'] / 2
        moles[mix.species_index(1, 'C(gr)')] = atoms['C']
        mix.species_moles = moles

        def h_at(t, p):
            mix.T, mix.P = t, p
            mix.equilibrate('TP', max_steps=2000)
            return None

        def total_h():
            # J per kg: the species amounts are kmol for a 1000 kg basis, Cantera molar enthalpies are J/kmol
            pm = mix.phase_moles()
            return pm[0] * gas.enthalpy_mole / 1000.0 + pm[1] * gr.enthalpy_mole / 1000.0

        lo, hi = 300.0, 4500.0
        for _ in range(60):
            mid = 0.5 * (lo + hi)
            h_at(mid, pc)
            if total_h() < h0:
                lo = mid
            else:
                hi = mid
        tc = 0.5 * (lo + hi)
        h_at(tc, pc)
        s0 = (mix.phase_moles()[0] * gas.entropy_mole + mix.phase_moles()[1] * gr.entropy_mole) / 1000.0
        mw_gas = gas.mean_molecular_weight

        def state_at(p):
            # isentrope: find T at p with S = s0 (equilibrium), return (h, gas volume per kg)
            lo, hi = 200.0, tc + 50.0
            for _ in range(50):
                mid = 0.5 * (lo + hi)
                h_at(mid, p)
                s = (mix.phase_moles()[0] * gas.entropy_mole + mix.phase_moles()[1] * gr.entropy_mole) / 1000.0
                if s < s0:
                    lo = mid
                else:
                    hi = mid
            h_at(0.5 * (lo + hi), p)
            v = mix.phase_moles()[0] / 1000.0 * ct.gas_constant * mix.T / p  # m^3 per kg (species moles are kmol for a 1000 kg basis)
            return total_h(), v

        def flux(p):
            h, v = state_at(p)
            u = math.sqrt(max(2.0 * (h0 - h), 0.0))
            return u / v

        a, b = math.log(0.35 * pc), math.log(0.85 * pc)
        gr_ = (math.sqrt(5) - 1) / 2
        c, d = b - gr_ * (b - a), a + gr_ * (b - a)
        fc, fd = flux(math.exp(c)), flux(math.exp(d))
        for _ in range(30):
            if fc > fd:
                b, d, fd = d, c, fc
                c = b - gr_ * (b - a)
                fc = flux(math.exp(c))
            else:
                a, c, fc = c, d, fd
                d = a + gr_ * (b - a)
                fd = flux(math.exp(d))
        pt = math.exp(0.5 * (a + b))
        g = flux(pt)
        if g <= 0.0:
            return None
        cstar = pc / g
        ratio = pt / pc
        # gamma with (2/(g+1))^(g/(g-1)) = pt/pc
        glo, ghi = 1.01, 1.7
        for _ in range(60):
            gm = 0.5 * (glo + ghi)
            if (2 / (gm + 1)) ** (gm / (gm - 1)) > ratio:
                glo = gm
            else:
                ghi = gm
        gamma = 0.5 * (glo + ghi)
        if not (600.0 < cstar < 2500.0 and 1.05 < gamma < 1.45):
            return None  # the throat search failed (soot-rich, cold mixtures): filled from a neighbour below
        return {'cstar': round(cstar, 2), 'gamma': round(gamma, 4), 'tc_k': round(tc, 1),
                'mw': round(mw_gas, 3)}

    tables = {}
    repaired = []
    for fuel in FUELS:
        rows = []
        for of in OF_GRID:
            row = []
            for pb in PC_GRID_BAR:
                row.append(point(fuel, of, pb * 1e5))
            rows.append(row)
            print(fuel, of, [r and r['cstar'] for r in row], file=sys.stderr)
        # A failed point takes the nearest valid chamber pressure at the same O/F (c* varies < 3 % across the
        # grid), else the nearest O/F at the same pressure.
        valid = [[r is not None for r in row] for row in rows]
        for i, row in enumerate(rows):
            for j, r in enumerate(row):
                if valid[i][j]:
                    continue
                same = [k for k in range(len(row)) if valid[i][k]]
                if same:
                    src = rows[i][min(same, key=lambda k: abs(k - j))]
                else:
                    near = [k for k in range(len(rows)) if valid[k][j]]
                    src = rows[min(near, key=lambda k: abs(k - i))][j]
                rows[i][j] = dict(src)
                repaired.append([fuel, OF_GRID[i], PC_GRID_BAR[j]])
        tables[fuel] = {k: [[r[k] for r in row] for row in rows] for k in ('cstar', 'gamma', 'tc_k', 'mw')}
    return {
        '_about': 'Generated by tools/flight_profile/hybrid_cstar.py (Cantera equilibrium with the NASA Glenn '
                  'thermodynamic data CEA uses; HP combustion, shifting-equilibrium expansion to the throat). '
                  'cstar m/s (ideal, before the c* efficiency), gamma = effective throat isentropic exponent, '
                  'tc_k chamber temperature, mw gas molar mass g/mol. N2O liquid at 0 C; paraffin n-C32H66(s) '
                  'dHf -938.1 kJ/mol; ABS C3.85H4.85N0.43 dHf +62.63 kJ/mol. Do not edit by hand.',
        'of': OF_GRID,
        'pc_pa': [p * 1e5 for p in PC_GRID_BAR],
        'fuels': tables,
        'repaired': repaired,
    }


def main(argv):
    data = build()
    OUT.write_text(json.dumps(data, separators=(',', ':')) + '\n')
    print(f'wrote {OUT}')
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
