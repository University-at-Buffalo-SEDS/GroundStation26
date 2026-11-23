"""tools/firmware_sim_e2e.py helpers against a fake station (no simulator needed)."""
import importlib.util
import pathlib
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("firmware_sim_e2e", ROOT / "tools" / "firmware_sim_e2e.py")
e2e = importlib.util.module_from_spec(spec)
sys.modules["firmware_sim_e2e"] = e2e
spec.loader.exec_module(e2e)


class FakeStation:
    def __init__(self, replies=None, reports=None, default=(200, "ok")):
        self.replies = list(replies or [])
        self.default = default
        self.reports = list(reports or [])
        self.sent = []
        self.logged = []

    def command(self, name):
        self.sent.append(name)
        return self.replies.pop(0) if self.replies else self.default

    def api(self, path, body=None):
        if path == "/api/burn_reports":
            return 200, {"reports": self.reports.pop(0) if self.reports else []}
        return 404, None

    def log(self, what, **extra):
        self.logged.append((what, extra))

    wait = e2e.Station.wait


class Helpers(unittest.TestCase):
    def setUp(self):
        self.sleep = e2e.time.sleep
        e2e.time.sleep = lambda s: None

    def tearDown(self):
        e2e.time.sleep = self.sleep

    def test_a_refused_gse_command_is_sent_again_until_accepted(self):
        st = FakeStation([(403, "a fresh tank PT sample is required"), (403, "again"), (200, "ok")])
        self.assertTrue(e2e.command_accepted(st, "ValveSelfTest", timeout_s=5))
        self.assertEqual(st.sent, ["ValveSelfTest"] * 3)

    def test_a_command_never_accepted_times_out(self):
        st = FakeStation(default=(403, "no"))
        self.assertFalse(e2e.command_accepted(st, "NitrogenTest", timeout_s=0.01))
        self.assertIn("timeout", [w for w, _ in st.logged])

    def test_burn_report_check_needs_thrust_and_impulse(self):
        checks = []
        result = {}
        report = {"id": "r1", "peak_thrust_n": 1800.0, "total_impulse_ns": 9000.0}
        st = FakeStation(reports=[[], [report]])
        e2e.burn_report(st, lambda name, ok, **kw: checks.append((name, bool(ok))), result)
        self.assertEqual(result["burn_report"], report)
        self.assertEqual(checks, [("the station computes the burn report (thrust and impulse)", True)])
        checks.clear()
        st = FakeStation(reports=[[{"id": "r2", "peak_thrust_n": 0.0, "total_impulse_ns": 0.0}]])
        e2e.burn_report(st, lambda name, ok, **kw: checks.append((name, bool(ok))), {})
        self.assertEqual(checks[0][1], False)


if __name__ == "__main__":
    unittest.main()
