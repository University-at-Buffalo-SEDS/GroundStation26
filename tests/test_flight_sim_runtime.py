import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("isolated_flight_sim_runtime", ROOT / "flight_sim_runtime.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class FlightSimEnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.assets = self.base / "published public"
        self.assets.mkdir()
        (self.assets / "index.html").write_text("<!doctype html><title>fixture</title>")

    def test_fresh_sessions_never_reuse_recording_or_recovery_data(self):
        old, env = module.prepare_environment(self.base / "sessions", {}, self.assets)
        (old / "proof/launch_clock.json").write_text("physical test sentinel")
        new, new_env = module.prepare_environment(self.base / "sessions", {}, self.assets)
        self.assertNotEqual(old, new)
        self.assertFalse((new / "proof/launch_clock.json").exists())
        for key, rel in module.PATHS.items():
            self.assertEqual(Path(new_env[key]), new / rel)
        self.assertEqual(new_env["GS_RRLS_ROLE"], "off")
        self.assertEqual(new_env["GS_VIDEO_MANAGED"], "0")
        self.assertEqual(new_env["GS_VIDEO_DISCOVERY"], "0")
        self.assertEqual(new_env["GS_BIND_ADDRESS"], "127.0.0.1:3000")
        self.assertEqual(new_env["GS_HTTPS_BIND_ADDRESS"], "off")
        self.assertEqual((new / "frontend/dist/public/index.html").read_text(), (self.assets / "index.html").read_text())
        self.assertEqual((old / "proof/launch_clock.json").read_text(), "physical test sentinel")

    def test_physical_proxy_transport_and_clock_selection_refused_before_session_created(self):
        for key in ["GS_RRLS_ROLE", "GS_RRLS_RADIO_PORT", "GS_STATION_PAIRING_FILE", "GS_COMMS_LINK_CONFIG", "GS_RADIO_LINK_CONFIG", "GS_GPSD_ADDR", "GS_SIMULATED_SERIAL_PTY", "GS_SIM_CLOCK_FILE"]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                module.prepare_environment(self.base / "sessions", {key: "physical"}, self.assets)
        self.assertFalse((self.base / "sessions").exists())

    def test_missing_ui_bundle_refused_before_allocating_session(self):
        (self.assets / "index.html").unlink()
        with self.assertRaises(ValueError):
            module.prepare_environment(self.base / "sessions", {}, self.assets)
        self.assertFalse((self.base / "sessions").exists())

    def test_only_build_asset_keys_inherit_and_paths_with_spaces_work(self):
        root, env = module.prepare_environment(self.base / "sim space", {"GS_USERS_FILE": "/physical/users", "GS_FLIGHT_STATE_DEFAULT": "15", "GS_FRONTEND_SOURCE_DIR": "/build-only/source", "PATH": os.defpath}, self.assets)
        self.assertEqual(env["GS_USERS_FILE"], str(root / "users/users.json"))
        self.assertNotIn("GS_FLIGHT_STATE_DEFAULT", env)
        self.assertEqual(env["GS_FRONTEND_SOURCE_DIR"], "/build-only/source")
        self.assertEqual(env["PATH"], os.defpath)



class FlightSimAccountSnapshotTests(unittest.TestCase):
    def test_explicit_snapshot_is_config_only_and_input_is_unchanged(self):
        import json
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            assets = base / "public"
            assets.mkdir()
            (assets / "index.html").write_text("fixture")
            users = base / "explicit.json"
            raw = b'{"version":1,"users":[]}'
            users.write_bytes(raw)
            root, env = module.prepare_environment(base / "sessions", {}, assets, users)
            self.assertEqual(Path(env["GS_USERS_FILE"]).read_bytes(), raw)
            self.assertEqual(users.read_bytes(), raw)
            self.assertFalse((root / "data/users.db").exists())
            self.assertEqual(Path(env["GS_USERS_FILE"]).stat().st_mode & 0o777, 0o600)
            for contents in [b'{"token":"must not copy","users":[]}', b'not json', b'{"users":[{}]}', b'{"users":[{"username":"fixture","password":{},"session_token":"must-not-copy"}]}']:
                users.write_bytes(contents)
                with self.assertRaises(ValueError):
                    module.read_users_snapshot(users)
            users.write_bytes(raw)
            link = base / "symlink.json"
            link.symlink_to(users)
            with self.assertRaises(OSError):
                module.read_users_snapshot(link)

class FlightSimCliTests(unittest.TestCase):
    def test_explicit_and_legacy_arguments_parse_without_enabling_hardware(self):
        import run_groundstation as runner
        for argv in [["run_groundstation.py", "--flight-sim"], ["run_groundstation.py", "flight-sim"]]:
            with self.subTest(argv=argv), patch("sys.argv", argv):
                args = runner.parse_args()
                self.assertTrue(args.flight_sim or args.mode == "flight-sim")
                self.assertFalse(args.hitl_mode or args.test_fire_mode or args.testing or args.rrls)
        for option, field in [("--hitl-mode", "hitl_mode"), ("--test-fire-mode", "test_fire_mode"), ("--testing", "testing")]:
            with self.subTest(option=option), patch("sys.argv", ["run_groundstation.py", option]):
                args = runner.parse_args()
                self.assertTrue(getattr(args, field))
                self.assertFalse(args.flight_sim)

if __name__ == "__main__":
    unittest.main()
