"""Hybrid motor model (tools/flight_profile/hybrid_motor.py, docs/hybrid-motor-model.md).

Runs with `python3 -m pytest tests/test_hybrid_motor.py` or `python3 -m unittest tests.test_hybrid_motor`.
"""
from __future__ import annotations

import json
import math
import random
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOLS = ROOT / 'tools' / 'flight_profile'
sys.path.insert(0, str(TOOLS))

import hybrid_motor as hm  # noqa: E402
import hybrid_stl  # noqa: E402
import hybrid_vectors  # noqa: E402

SPEC = hm.load_spec()
DATA = hm.load_data()


def rel(a, b):
    return abs(a / b - 1.0)


class N2OProperties(unittest.TestCase):
    def test_saturation_points_match_nist(self):
        # NIST Chemistry WebBook: 20 C 5.06 MPa, liquid 786, vapour 159 kg/m^3; 0 C 3.13 MPa, liquid 906; -20 C 1.81 MPa
        self.assertLess(rel(hm.psat(293.15), 5.06e6), 0.01)
        self.assertLess(rel(hm.rho_l(293.15), 786.0), 0.01)
        self.assertLess(rel(hm.rho_v(293.15), 159.0), 0.03)
        self.assertLess(rel(hm.psat(273.15), 3.13e6), 0.01)
        self.assertLess(rel(hm.rho_l(273.15), 906.0), 0.01)
        self.assertLess(rel(hm.psat(253.15), 1.81e6), 0.015)
        self.assertAlmostEqual(hm.tsat(hm.psat(280.0)), 280.0, places=5)

    def test_enthalpy_matches_the_cea_surface_axis(self):
        # The surface's liquid states run about -14..+30 C over 16036..17169 cal/mol.
        self.assertLess(abs(hm.n2o_enthalpy_cal_mol(259.0) - 16036.0), 60.0)
        self.assertLess(abs(hm.n2o_enthalpy_cal_mol(303.0) - 17169.0), 80.0)


class Injector(unittest.TestCase):
    def test_dyer_is_spi_above_the_vapour_pressure_and_between_spi_and_hem_below(self):
        t = 280.0
        pv = hm.psat(t)
        ch = hm.hem_choke(t, pv + 1e6)
        spi = math.sqrt(2 * hm.rho_l(t) * 0.5e6)
        self.assertAlmostEqual(hm.dyer_flux(t, pv + 1e6, pv + 0.5e6, ch), spi, places=6)
        ch = hm.hem_choke(t, pv)
        g = hm.dyer_flux(t, pv, 1.5e6, ch)
        spi = math.sqrt(2 * hm.rho_l(t) * (pv - 1.5e6))
        self.assertLess(ch[0], g)
        self.assertLess(g, spi)
        # more pressure drop, more flow; HEM chokes
        self.assertGreater(hm.dyer_flux(t, pv, 1.0e6, ch), g)
        self.assertAlmostEqual(hm.hem_flux(t, pv, ch[1]), ch[0], places=6)

    def test_choked_gas_flow(self):
        g1 = hm.gas_flux(2e6, 40.0, 1.3, 1e5)
        g2 = hm.gas_flux(2e6, 40.0, 1.3, 5e5)
        self.assertAlmostEqual(g1, g2, places=9)  # both choked
        self.assertLess(hm.gas_flux(2e6, 40.0, 1.3, 1.9e6), g1)


class Thermochemistry(unittest.TestCase):
    def test_cea_table_peak_and_independent_cantera_cross_check(self):
        h = hm.n2o_enthalpy_cal_mol(273.15)
        c, *_ = DATA.cea.lookup(7.0, 3.4e6, h, 0.0)
        self.assertLess(rel(c, 1600.0), 0.02)
        # hybrid_cstar.py (Cantera, NASA Glenn data, C32H66) vs the team's CEA surface (C25H52 surrogate)
        ct = json.loads((TOOLS / 'motors' / 'paraffin_n2o_cstar.json').read_text())
        for of in (5.0, 7.0, 10.0):
            i = ct['of'].index(of)
            j = ct['pc_pa'].index(2e6)
            mine = ct['fuels']['paraffin']['cstar'][i][j]
            theirs = DATA.cea.lookup(of, 2e6, h, 0.0)[0]
            self.assertLess(rel(mine, theirs), 0.02, (of, mine, theirs))

    def test_compact_table_validation(self):
        v = json.loads((TOOLS / 'motors' / 'sweet_v6_cea.json').read_text())['validation_rel_error']
        for k in ('cstar', 'gamma', 'cf_vac', 'pe_pc'):
            self.assertLess(v[k]['p99'], 1e-3, k)

    def test_nozzle_cf(self):
        cf, _ = hm.thrust_coefficient(1.2, 4.9, 2.5e6, 1.0, 1.0)
        self.assertLess(rel(cf, 1.66), 0.02)  # Sutton & Biblarz fig. 3-7
        sea, _ = hm.thrust_coefficient(1.2, 4.9, 2.5e6, hm.ATM, 1.0)
        self.assertLess(sea, cf)
        # overexpanded with separation still gives thrust
        self.assertGreater(hm.thrust_coefficient(1.2, 4.9, 3e5, hm.ATM, 1.0)[0], 0.0)


class Regression(unittest.TestCase):
    def test_rate_is_the_area_weighted_material_laws(self):
        p = hm.params(SPEC, grain='cylindrical_preset')
        m = hm.HybridMotor(p, DATA, 6.0, 15.0)
        mdot, wabs, rdot = m.fuel_rates(1.5)
        area, perim, f = m.grain.at(0.0)
        g = 1.5 / area
        want = (1 - f) * p['paraffin_a'] * g ** p['paraffin_n'] + f * p['abs_a'] * g ** p['abs_n']
        self.assertAlmostEqual(rdot, want, places=12)
        dens = 900 * (1 - f) + 1020 * f
        self.assertAlmostEqual(mdot, dens * perim * 0.3302 * rdot, places=9)
        self.assertAlmostEqual(wabs, 1020 * f / dens, places=12)


class GrainGeometry(unittest.TestCase):
    def test_circle_is_analytic(self):
        g = DATA.grain(hm.grain_config(SPEC, 'cylindrical_preset'))
        for w in (0.0, 0.005, 0.012):
            a, p, _ = g.at(w)
            self.assertLess(rel(a, math.pi * (0.0254 + w) ** 2), 1e-5)
            self.assertLess(rel(p, 2 * math.pi * (0.0254 + w)), 0.01)
        self.assertLess(rel(g.mass, 0.9186 + 0.1883), 0.002, 'the preset paraffin and ABS masses')

    def test_star_reproduces_the_stl(self):
        stl = hybrid_stl.analyse()
        cfg = hm.grain_config(SPEC, SPEC['flight_grain'])
        self.assertEqual(stl['points'], cfg['star_points'])
        self.assertLess(abs(stl['tip_radius_mm'] / 1000 - cfg['star_tip_radius_m']), 1e-5)
        self.assertLess(abs(stl['root_radius_mm'] / 1000 - cfg['star_root_radius_m']), 1e-5)
        self.assertLess(abs(stl['length_mm'] / 1000 - cfg['grain_length_m']), 1e-5)
        g = DATA.grain(cfg)
        self.assertLess(rel(g.at(0.0)[0], stl['port_area_mm2'] * 1e-6), 0.002)
        self.assertLess(rel(g.perim[0], stl['port_perimeter_mm'] * 1e-3), 0.02)
        self.assertLess(abs(g.w_tip - stl['web_beyond_tips_mm'] / 1000), 1e-4)
        # offset fronts close the sharp valleys faster than their radial depth: the true web is the largest
        # distance from the OD to the outline, which is less than the radial valley depth
        self.assertLess(g.w_max, stl['web_behind_valleys_mm'] / 1000)
        self.assertGreater(g.w_max, g.w_tip)

    def test_star_offsets_match_a_raster_reference(self):
        cfg = hm.grain_config(SPEC, SPEC['flight_grain'])
        g = DATA.grain(cfg)
        rng = random.Random(3)
        for w in (0.004, 0.012, 0.022):
            inside = 0
            n = 40000
            for _ in range(n):
                x, y = rng.uniform(-g.r_out, g.r_out), rng.uniform(-g.r_out, g.r_out)
                if math.hypot(x, y) <= g.r_out and g._dist_core(x, y) <= w:
                    inside += 1
            ref = inside / n * (2 * g.r_out) ** 2
            self.assertLess(rel(g.at(w)[0], ref), 0.03, w)
        # tips touch the liner at w_tip; the liner is fully exposed at burnout
        self.assertEqual(g.wall_fraction(0.9 * g.w_tip), 0.0)
        self.assertGreater(g.wall_fraction(g.w_tip + 0.002), 0.0)
        self.assertLess(g.at(0.97 * g.w_max)[1], 0.3 * g.perim[0], 'sliver: the perimeter collapses')


class Burns(unittest.TestCase):
    def test_calibrated_model_reproduces_the_2026_11_16_fire(self):
        p = hm.params(SPEC, grain=SPEC['fire_grain_default'])
        tc = hm.tsat((404.1 + 14.696) * hm.PSI) - 273.15
        res = hm.simulate(p, DATA, 5.65, tc, 0.0, p['test_ambient_pa'])
        sm = res['summary']
        self.assertLess(rel(sm['impulse_ns'], 9192.0), 0.08)
        self.assertLess(rel(sm['burn_time_s'], 5.65), 0.15)
        s = res['series']
        curve = [l.split(',') for l in (TOOLS / 'motors' / 'testfire_20261116.csv').read_text().splitlines()
                 if l and l[0].isdigit()]
        err = []
        for t, f in ((float(a), float(b)) for a, b in curve):
            if t >= 0.65:
                i = min(range(len(s['t'])), key=lambda j: abs(s['t'][j] - t))
                err.append((s['thrust'][i] - f) ** 2)
        self.assertLess(math.sqrt(sum(err) / len(err)), 320.0)

    def test_ambient_temperature_drives_the_motor(self):
        p = hm.params(SPEC)
        cold = hm.simulate(p, DATA, 7.0, 0.0)['summary']
        warm = hm.simulate(p, DATA, 7.0, 25.0)['summary']
        self.assertGreater(warm['peak_thrust_n'], cold['peak_thrust_n'])
        self.assertLess(warm['burn_time_s'], cold['burn_time_s'])

    def test_star_reports_liner_exposure(self):
        sm = hm.simulate(hm.params(SPEC), DATA, 8.0, 20.0)['summary']
        self.assertGreater(sm['tip_burnthrough_s'], 0.0)
        self.assertGreater(sm['liner_exposed_s'], 0.0)


class SharedVectors(unittest.TestCase):
    def test_vectors_and_backend_copies_are_current(self):
        self.assertEqual(hybrid_vectors.main(['--check']), 0, 'run tools/flight_profile/hybrid_vectors.py')
        model = ROOT / 'backend' / 'src' / 'apogee_planner' / 'model'
        for name in ('hybrid_motor.json', 'sweet_v6_cea.json', 'sweet_v6_grain.json', 'hybrid_vectors.json'):
            self.assertEqual((model / name).read_text(), (TOOLS / 'motors' / name).read_text(), name)


class FlightModel(unittest.TestCase):
    def test_flightsim_flies_the_physics_motor(self):
        import flightsim as fs
        cfg = json.loads((TOOLS / 'vehicle.json').read_text())
        cfg['environment']['gust_sigma_mps'] = 0.0
        m = fs.Motor.hybrid(8.0, 20.0, 0.0, 99000.0)
        self.assertTrue(m.prop_curve and m.exit_area_m2 > 0)
        tr = fs.fly(cfg, random.Random(1), m, record=False, stop_at_apogee=True)
        self.assertGreater(tr.events['apogee_m'], 1000.0)
        # thrust grows as the air thins
        tb = 1.0
        self.assertGreater(m.thrust_at(tb, 50000.0), m.thrust_at(tb, 99000.0))


if __name__ == '__main__':
    unittest.main()
