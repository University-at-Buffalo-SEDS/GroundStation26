"""Station mode supervisor selection, environment and restart loop."""
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
import station_modes as sm  # noqa: E402


def install(root: Path):
    binary = root / sm.BINARY
    binary.parent.mkdir(parents=True, exist_ok=True)
    binary.write_text("#!/bin/sh\n")
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)


def selection(directory: Path, mode, simulation=False, role="primary"):
    directory.mkdir(parents=True, exist_ok=True)
    (directory / sm.SELECTION_FILE).write_text(json.dumps({
        "version": 1, "role": role, "mode": mode, "simulation": simulation,
        "target_id": mode + ("-simulation" if simulation else ""),
        "transition_id": "switch-test", "requested_by": "user:op", "requested_at_ms": 1,
    }))


class SelectionTests(unittest.TestCase):
    def test_parse_modes_accepts_launcher_spellings(self):
        self.assertEqual(sm.parse_modes("all"), list(sm.MODES))
        self.assertEqual(sm.parse_modes("hitl-mode,test-fire-mode,hitl"), ["hitl", "test_fire"])
        with self.assertRaises(ValueError):
            sm.parse_modes("flight")

    def test_no_selection_keeps_the_launcher_default(self):
        choice = sm.choose(None, ["normal", "hitl"], "hitl", rrls=False)
        self.assertEqual((choice.mode, choice.simulation, choice.reason), ("hitl", False, "default"))

    def test_selection_wins_only_when_installed_for_this_role(self):
        sel = {"version": 1, "role": "primary", "mode": "test_fire", "simulation": False}
        self.assertEqual(sm.choose(sel, ["normal", "test_fire"], "normal", False).mode, "test_fire")
        self.assertEqual(sm.choose(sel, ["normal"], "normal", False).reason, "default", "missing binary falls back")
        self.assertEqual(sm.choose(sel, ["normal", "test_fire"], "normal", True).reason, "default", "primary selection never drives an RRLS")
        bad = dict(sel, simulation="yes")
        self.assertEqual(sm.choose(bad, ["normal", "test_fire"], "normal", False).reason, "default")

    def test_simulation_is_testing_only_on_primary_and_any_build_on_rrls(self):
        sel = {"version": 1, "role": "primary", "mode": "hitl", "simulation": True}
        self.assertFalse(sm.choose(sel, ["hitl"], "hitl", False).simulation)
        sel["mode"] = "testing"
        self.assertTrue(sm.choose(sel, ["testing"], "hitl", False).simulation)
        rr = {"version": 1, "role": "rrls", "mode": "hitl", "simulation": True}
        self.assertTrue(sm.choose(rr, ["hitl"], "normal", True).simulation)

    def test_unreadable_or_unknown_selection_is_ignored(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            self.assertIsNone(sm.read_selection(directory))
            (directory / sm.SELECTION_FILE).write_text("{not json")
            self.assertIsNone(sm.read_selection(directory))
            (directory / sm.SELECTION_FILE).write_text(json.dumps({"version": 9}))
            self.assertIsNone(sm.read_selection(directory))


class EnvironmentTests(unittest.TestCase):
    def test_child_env_marks_supervision_and_passes_the_mode(self):
        choice = sm.Choice("hitl", False, "selection")
        env = sm.child_env({"PATH": "/bin", "GS_STATION_MODE_AVAILABLE": "old"}, choice, False, list(sm.MODES), Path("/x/state"), Path("/x/sim"))
        self.assertEqual(env["GS_STATION_MODE_SUPERVISOR"], "1")
        self.assertEqual(env["GS_STATION_MODE"], "hitl")
        self.assertNotIn("GS_STATION_MODE_AVAILABLE", env)
        self.assertEqual(env["GS_STATION_MODE_DIR"], "/x/state")
        self.assertNotIn("GS_SIM_MODE", env)

    def test_testing_selection_sets_internal_flight_sim_flag_but_default_does_not(self):
        on = sm.child_env({}, sm.Choice("testing", True, "selection"), False, ["testing"], Path("/s"), Path("/r"))
        off = sm.child_env({}, sm.Choice("testing", False, "selection"), False, ["testing"], Path("/s"), Path("/r"))
        default = sm.child_env({}, sm.Choice("testing", False, "default"), False, ["testing"], Path("/s"), Path("/r"))
        self.assertEqual((on["GS_SIM_MODE"], off["GS_SIM_MODE"]), ("1", "0"))
        self.assertNotIn("GS_SIM_MODE", default, "no selection keeps the current behaviour")

    def test_rrls_simulation_isolates_state_and_never_saves_a_radio_port(self):
        base = {"GS_RRLS_RADIO_PORT": "/dev/ttyUSB0", "GS_STATION_PAIRING_FILE": "/real/pairing.json"}
        sim = sm.child_env(base, sm.Choice("normal", True, "selection"), True, ["normal"], Path("/s"), Path("/s/sim"))
        self.assertEqual(sim["GS_RRLS_SIMULATED_PRIMARY"], "1")
        self.assertEqual(sim["GS_RRLS_SIM_ROOT"], "/s/sim")
        self.assertEqual(sim["GS_SIM_MODE"], "0")
        self.assertNotIn("GS_RRLS_RADIO_PORT", sim)
        for key, relative in sm.RRLS_SIM_ISOLATED.items():
            self.assertEqual(sim[key], str(Path("/s/sim") / relative))
        live = sm.child_env(dict(base, GS_RRLS_SIMULATED_PRIMARY="1"), sm.Choice("normal", False, "selection"), True, ["normal"], Path("/s"), Path("/s/sim"))
        self.assertNotIn("GS_RRLS_SIMULATED_PRIMARY", live)
        self.assertEqual(live["GS_RRLS_RADIO_PORT"], "/dev/ttyUSB0")
        self.assertEqual(live["GS_STATION_PAIRING_FILE"], "/real/pairing.json")

    def test_isolation_list_matches_the_backend(self):
        source = (ROOT / "backend/src/rrls_sim_primary.rs").read_text()
        block = source[source.index("pub(crate) const ISOLATED"):]
        block = block[:block.index("];")]
        for key, relative in sm.RRLS_SIM_ISOLATED.items():
            self.assertIn(f'("{key}", "{relative}")', block)
        self.assertEqual(block.count('("GS_'), len(sm.RRLS_SIM_ISOLATED))

    def test_restart_exit_code_matches_the_backend(self):
        source = (ROOT / "backend/src/station_mode_switch.rs").read_text()
        self.assertIn(f"pub const RESTART_EXIT_CODE: i32 = {sm.RESTART_EXIT_CODE};", source)


class BuildAndSuperviseTests(unittest.TestCase):
    def test_build_makes_one_binary_for_every_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            commands = []

            def fake_run(command, cwd, check):
                commands.append(command)
                built = repo / "target" / ("release" if "--release" in command else "debug") / sm.BINARY
                built.parent.mkdir(parents=True, exist_ok=True)
                built.write_text(",".join(command))
                built.chmod(0o755)

            root = sm.build_modes(repo, ["normal", "hitl"], pi=True, run=fake_run)
            self.assertEqual(commands, [["cargo", "build", "--release", "-p", sm.BINARY, "--features", "raspberry_pi"]])
            self.assertEqual(sm.installed_modes(root), list(sm.MODES))
            self.assertTrue((root / sm.BINARY).is_file())
            commands.clear()
            sm.build_modes(repo, None, debug=True, run=fake_run, firmware_sim=True)
            self.assertEqual(commands, [["cargo", "build", "-p", sm.BINARY, "--features", "firmware_sim"]])
            for command in commands:
                self.assertNotRegex(" ".join(command), "hitl_mode|test_fire_mode|testing")

    def test_supervisor_restarts_into_the_saved_selection(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            root = repo / "bins"
            install(root)
            state = repo / "state"
            started = []

            class Process:
                def __init__(self, argv, cwd, env):
                    started.append((env["GS_STATION_MODE"], env))
                    self.code = 0
                    if len(started) == 1:
                        selection(state, "testing", True)
                        self.code = sm.RESTART_EXIT_CODE

                def wait(self, timeout=None):
                    return self.code

            code = sm.supervise(repo, root, "hitl", False, cwd=repo, environ={"GS_STATION_MODE_DIR": str(state)}, popen=Process)
            self.assertEqual(code, 0)
            self.assertEqual([name for name, _ in started], ["hitl", "testing"])
            self.assertEqual(started[1][1]["GS_SIM_MODE"], "1")
            self.assertNotIn("GS_STATION_MODE_AVAILABLE", started[1][1])

    def test_other_exit_codes_are_returned_not_restarted(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            install(repo / "bins")

            class Crash:
                def __init__(self, *args, **kwargs):
                    pass

                def wait(self, timeout=None):
                    return 101

            self.assertEqual(sm.supervise(repo, repo / "bins", "normal", False, cwd=repo, environ={"GS_STATION_MODE_DIR": str(repo / "s")}, popen=Crash), 101)

    def test_missing_default_build_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(FileNotFoundError):
                sm.plan(Path(tmp), Path(tmp) / "bins", {}, "normal", False)

    def test_rrls_simulation_gets_a_fresh_root_each_start(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            install(repo / "bins")
            state = repo / "state"
            selection(state, "normal", True, role="rrls")
            stale = state / "rrls-simulation" / "stale.json"
            stale.parent.mkdir(parents=True)
            stale.write_text("{}")
            choice, binary, env = sm.plan(repo, repo / "bins", {"GS_STATION_MODE_DIR": str(state)}, "normal", True)
            self.assertTrue(choice.simulation)
            self.assertFalse(stale.exists())
            self.assertEqual(env["GS_RRLS_SIM_ROOT"], str((state / "rrls-simulation").resolve()))


class LauncherTests(unittest.TestCase):
    def test_switchable_modes_builds_requested_modes_and_supervises_default(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location("gs_launcher_modes", ROOT / "run_groundstation.py")
        launcher = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(launcher)
        with patch.object(sys, "argv", ["run_groundstation.py", "--hitl-mode", "--switchable-modes", "testing", "--backend-only"]), \
                patch.dict(os.environ, {}, clear=True), patch.object(launcher, "is_raspberry_pi", return_value=False), \
                patch.object(launcher, "refresh_sedsnet_dependency"), patch.object(launcher, "prepare_browser_asset_source"), \
                patch.object(sm, "build_modes", return_value=Path("/bins")) as build, patch.object(sm, "supervise", return_value=0) as supervise, \
                patch.object(launcher, "run") as direct:
            launcher.main()
        self.assertEqual(build.call_args.args[1], ["hitl", "testing"])
        self.assertEqual(supervise.call_args.args[2], "hitl")
        self.assertFalse(supervise.call_args.args[3])
        direct.assert_not_called()


if __name__ == "__main__":
    unittest.main()
