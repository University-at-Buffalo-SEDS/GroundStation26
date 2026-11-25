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

    def now(self):
        # a fake station clock: every look advances it half a second, so the waits end quickly
        self.clock = getattr(self, "clock", 0.0) + 0.5
        return self.clock

    def sleep(self, seconds):
        e2e.time.sleep(seconds)


class StationClock(unittest.TestCase):
    """The check's waits run on the bay's virtual clock when the station follows it, continuous with the wall."""

    def test_now_follows_the_bay_status_and_never_runs_backwards(self):
        import json, tempfile, time
        from pathlib import Path
        with tempfile.TemporaryDirectory() as d:
            st = e2e.Station("http://127.0.0.1:1", b"", Path(d))
            before = st.now()
            self.assertLess(abs(before - time.monotonic()), 1.0, "no bay status yet: the wall clock")
            st.bay_status.parent.mkdir(parents=True)
            st.bay_status.write_text(json.dumps({"virtual_ms": 10_000, "station_clock": "bay"}))
            first = st.now()
            self.assertGreaterEqual(first, before)
            self.assertLess(first - before, 2.0)
            st.bay_status.write_text(json.dumps({"virtual_ms": 40_000, "station_clock": "bay"}))
            self.assertAlmostEqual(st.now() - first, 30.0, delta=1.5)
            st.bay_status.write_text(json.dumps({"virtual_ms": 5_000, "station_clock": "bay"}))
            self.assertGreaterEqual(st.now() - first, 29.0)
            st.timeline.close()


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

    def test_burn_report_check_needs_thrust_and_impulse_and_waits_for_final(self):
        checks = []
        result = {}
        provisional = {"id": "r1", "status": "provisional", "peak_thrust_n": 1800.0, "total_impulse_ns": 9000.0}
        final = {**provisional, "status": "final"}
        st = FakeStation(reports=[[], [provisional], [provisional], [final]])
        e2e.burn_report(st, lambda name, ok, **kw: checks.append((name, bool(ok))), result)
        self.assertEqual(result["burn_report"], final)
        self.assertEqual(checks, [("the station computes the burn report (thrust and impulse)", True),
                                  ("the burn report goes final (DAQ SD launch file merged)", True)])
        checks.clear()
        st = FakeStation(reports=[[{"id": "r2", "status": "provisional", "peak_thrust_n": 0.0, "total_impulse_ns": 0.0}]])
        e2e.burn_report(st, lambda name, ok, **kw: checks.append((name, bool(ok))), {})
        self.assertEqual([ok for _, ok in checks], [False, False])

    def test_qd_drive_selection_releases_the_stepper_and_waits_for_the_ab_to_report_auxiliary(self):
        class Plumbing(FakeStation):
            def __init__(self):
                super().__init__()
                self.mode, self.settings = "stepper", []

            def api(self, path, body=None):
                if path == "/api/actuator/plumbing/settings":
                    self.settings.append(body)
                    self.mode = body["mode"]
                    return 202, {"message": "saving"}
                if path == "/api/actuator/plumbing/status":
                    return 200, {"backend_instance": "b", "reset_epoch": "r", "desired": {"revision": 3}, "pending": False,
                                 "board": {"fresh": True, "ready": True, "mode": self.mode, "generation": "2a", "epoch": 1}}
                return 200, {}
        st = Plumbing()
        checks, result = [], {}
        board = e2e.select_qd_drive(st, lambda name, ok, **kw: checks.append((name, bool(ok))), result, "auxiliary")
        self.assertEqual(board["mode"], "auxiliary")
        self.assertEqual(len(st.settings), 1)
        self.assertEqual(st.settings[0], {"mode": "auxiliary", "expected_generation": "2a", "expected_epoch": 1,
                                          "backend_instance": "b", "expected_revision": 3, "expected_reset_epoch": "r",
                                          "release_stepper": True})
        self.assertEqual(checks, [("AB fill drive set to auxiliary (the QD retract drive)", True)])

    def test_the_stepper_retract_is_the_firmwares_600_s(self):
        self.assertAlmostEqual(e2e.STEPPER_RETRACT_S, 600.0)
        self.assertEqual(e2e.AUX_RETRACT_S, 45.0)

    def test_low_battery_drains_both_packs_and_sees_the_condition_and_the_alert(self):
        class BatteryStation(FakeStation):
            def __init__(self):
                super().__init__()
                self.v = {"GB": 15.2, "PB": 7.8}
                self.posts = []

            def api(self, path, body=None):
                if path == "/api/firmware-sim/control":
                    self.posts.append(body)
                    soc = {k: e["soc"] for k, e in body["batteries"].items()}
                    self.v = {"GB": 12.4 if soc["gateway"] < 0.1 else 15.2, "PB": 5.9 if soc["power"] < 0.1 else 7.8}
                    return 200, {}
                low = self.v["GB"] <= 13.0
                if path == "/api/alert_conditions":
                    return 200, {"records": [{"key": "gateway_low_voltage:GB", "state": "active" if low else "resolved"}]}
                if path.startswith("/api/alerts"):
                    return 200, [{"message": "Critical: PB battery voltage out of range!"}] if self.v["PB"] < 6.3 else []
                return 404, None

            def plant(self):
                return {"batteries": {"gateway": {"soc": 0.6}}}

            def latest(self, data_type, sender=None):
                return {"values": [self.v[sender]]} if data_type == "BATTERY_VOLTAGE" else None

        st, checks, result = BatteryStation(), [], {}
        e2e.low_battery(st, lambda name, ok, **kw: checks.append((name, bool(ok))), result)
        self.assertEqual([ok for _, ok in checks], [True, True, True, True], checks)
        self.assertEqual(result["batteries"]["low"], {"GB": 12.4, "PB": 5.9})
        self.assertEqual(st.posts[0], {"batteries": {"gateway": {"soc": 0.02}, "power": {"soc": 0.01}}})


if __name__ == "__main__":
    unittest.main()
