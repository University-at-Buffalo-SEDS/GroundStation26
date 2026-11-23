"""The apogee planner's reference cases (backend/src/apogee_planner/fixtures)
and model copies must follow tools/flight_profile."""
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOLS = ROOT / 'tools' / 'flight_profile'
PLANNER = ROOT / 'backend' / 'src' / 'apogee_planner'


class ApogeeReference(unittest.TestCase):
    def test_reference_cases_are_current(self):
        run = subprocess.run([sys.executable, str(TOOLS / 'planner_reference.py'), '--check'],
                             capture_output=True, text=True)
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)

    def test_model_copies_match(self):
        self.assertEqual((PLANNER / 'model' / 'vehicle.json').read_text(), (TOOLS / 'vehicle.json').read_text())
        self.assertEqual((PLANNER / 'model' / 'testfire_20261116.csv').read_text(),
                         (TOOLS / 'motors' / 'testfire_20261116.csv').read_text())

    def test_thrust_extraction_reproduces_the_committed_curve(self):
        sys.path.insert(0, str(TOOLS))
        import extract_thrust
        curves, summary = extract_thrust.extract()
        self.assertAlmostEqual(summary['variants']['nominal']['total_impulse_ns'], 9192.0, delta=1.0)
        self.assertAlmostEqual(summary['variants']['kg1000_only']['total_impulse_ns'], 7105.6, delta=1.0)
        self.assertGreater(summary['kg50_term_ns'], 2000)
        rows = [line for line in (TOOLS / 'motors' / 'testfire_20261116.csv').read_text().splitlines()
                if line and line[0].isdigit()]
        self.assertEqual(len(rows), len(curves['nominal']))


if __name__ == '__main__':
    unittest.main()
