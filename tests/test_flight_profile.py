"""tools/flight_profile: the physical mock-flight generator and the committed
IGT datasets in test-data/igt (docs/integrated-ground-test.md section 11)."""
import importlib.util
import json
import math
import random
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOLS = ROOT / 'tools'
DATA = ROOT / 'test-data' / 'igt'
sys.path.insert(0, str(TOOLS))
sys.path.insert(0, str(TOOLS / 'flight_profile'))

try:
    import numpy  # noqa: F401
    HAVE_NUMPY = True
except ImportError:
    HAVE_NUMPY = False

import igt_csv  # noqa: E402

VARIANTS = [v['name'] for v in json.loads((TOOLS / 'flight_profile/variants.json').read_text())['variants']]


def limits():
    return igt_csv.Limits(31.758, -102.046, 890.016)


class CommittedDatasets(unittest.TestCase):
    def test_every_variant_is_committed_and_passes_the_fc_checks(self):
        for name in VARIANTS:
            with self.subTest(name):
                self.assertEqual(igt_csv.check_set(DATA / name, limits()), [])
                s = json.loads((DATA / name / 'summary.json').read_text())
                self.assertLess(s['total_bytes'], 1_500_000)

    def test_nominal_reaches_10k_ft_with_the_fc_events_in_order(self):
        s = json.loads((DATA / 'nominal_10k' / 'summary.json').read_text())
        self.assertAlmostEqual(s['apogee_m_agl'], 3048.0, delta=15)
        self.assertLess(9.0, s['liftoff_s'])
        order = [s['liftoff_s'], s['burnout_s'], s['apogee_s'], s['drogue_s'], s['main_s'], s['landing_s'],
                 s['data_end_s']]
        self.assertEqual(order, sorted(order))
        self.assertAlmostEqual(s['main_alt_m'], 457.2, delta=10)
        self.assertGreaterEqual(s['data_end_s'] - s['landing_s'], 25)
        self.assertAlmostEqual(s['drogue_rate_mps'], 25, delta=2)
        self.assertAlmostEqual(s['main_rate_mps'], 6, delta=1)


@unittest.skipUnless(HAVE_NUMPY, 'numpy not installed')
class Generator(unittest.TestCase):
    def test_regenerating_a_committed_set_is_byte_identical(self):
        import generate
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(generate.main(['--variant', 'measured_motor', '--out', tmp]), 0)
            for f in ('IMU0.CSV', 'BARO0.CSV', 'GPS0.CSV'):
                self.assertEqual((Path(tmp) / 'IGT' / f).read_bytes(),
                                 (DATA / 'measured_motor' / 'IGT' / f).read_bytes(), f)

    def test_specific_force_integrates_back_to_the_trajectory(self):
        """The IMU is f = a - g by construction: integrating it through the
        recorded attitude reproduces the truth velocity (what the FC EKF does)."""
        import flightsim as fs
        import generate
        cfg = json.loads((TOOLS / 'flight_profile/vehicle.json').read_text())
        tr = fs.fly(cfg, random.Random(5), generate.load_motor(cfg))
        vel = [0.0, 0.0, 0.0]
        end = int(tr.events['apogee_s'] * 1000)
        for k in range(end):
            g = fs.gravity(cfg['environment']['site_lat_deg'], cfg['environment']['site_msl_m'] + tr.pos[k][2])
            f_body = fs.qrot_inv(tr.q[k], tr.f_world[k])
            f_world = fs.qrot(tr.q[k], f_body)
            for i in range(3):
                vel[i] += (f_world[i] - (g if i == 2 else 0.0)) * 0.001
        self.assertLess(math.dist(vel, tr.vel[end]), 0.5)

    def test_off_nominal_glitches_land_in_the_data(self):
        rows = (DATA / 'off_nominal' / 'IGT' / 'IMU0.CSV').read_text().splitlines()[1:]
        peak = max(abs(float(x)) for r in rows for x in r.split(',')[1:4])
        self.assertAlmostEqual(peak, 235.35, delta=0.01)   # clipped at the BMI088 24 g range


if __name__ == '__main__':
    unittest.main()
