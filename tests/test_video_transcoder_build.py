"""The explicit helper action must not enter station/frontend build paths."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import build


class VideoTranscoderBuildTests(unittest.TestCase):
    def test_native_command_is_locked_own_workspace_release_or_debug_on_each_os(self):
        root = Path(build.__file__).resolve().parent
        for system in ("Linux", "Darwin", "Windows"):
            for debug in (False, True):
                with self.subTest(system=system, debug=debug), patch.object(build.platform, "system", return_value=system), patch.object(build, "run") as run:
                    build.build_video_transcoder(root, debug)
                expected = ["cargo", "build", "--locked", "--manifest-path", str(root / "video_transcoder/Cargo.toml")]
                if not debug:
                    expected.append("--release")
                run.assert_called_once_with(expected, cwd=root)

    def test_missing_tracked_lock_refuses_before_cargo(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(build, "run") as run:
            with self.assertRaisesRegex(FileNotFoundError, "Restore the tracked"):
                build.build_video_transcoder(Path(directory), False)
            run.assert_not_called()

    def test_helper_selector_never_prepares_frontend_or_backend(self):
        for flags in ([], ["debug"]):
            with self.subTest(flags=flags), patch.object(sys, "argv", ["build.py", "video_transcoder", *flags]), patch.object(build, "run") as run, patch.object(build, "prepare_browser_asset_source") as source, patch.object(build, "_run_frontend_build") as frontend, patch.object(build, "_run_script") as backend, patch.object(build, "build_docker") as docker, patch.object(build, "_configure_log_file"):
                build.main()
                run.assert_called_once()
                source.assert_not_called()
                frontend.assert_not_called()
                backend.assert_not_called()
                docker.assert_not_called()

    def test_station_flags_are_rejected_before_network_prep_log_or_cargo(self):
        for flag in ("testing", "hitl-mode", "test-fire-mode", "rrls", "pi_build", "no_pi", "docker", "plain", "max_size", "existing", "--frontend-dev", "--backend-only", "frontend_web", "web"):
            with self.subTest(flag=flag), patch.object(sys, "argv", ["build.py", "video_transcoder", flag]), patch.object(build, "run") as run, patch.object(build, "prepare_browser_asset_source") as source, patch.object(build, "_configure_log_file") as log, patch.object(build, "_run_script") as backend:
                with self.assertRaises(SystemExit) as error:
                    build.main()
                self.assertEqual(error.exception.code, 1)
                run.assert_not_called()
                source.assert_not_called()
                log.assert_not_called()
                backend.assert_not_called()

    def test_real_cli_handles_spaced_paths_logs_and_propagates_cargo_failure(self):
        with tempfile.TemporaryDirectory(prefix="helper build spaces ") as directory:
            root = Path(directory)
            script = root / "build.py"
            shutil.copyfile(build.__file__, script)
            helper = root / "video_transcoder"
            helper.mkdir()
            # Real tracked manifest/lock, no fake package or target tree.
            original = Path(build.__file__).resolve().parent / "video_transcoder"
            for name in ("Cargo.toml", "Cargo.lock"):
                shutil.copyfile(original / name, helper / name)
            tools = root / "tools"
            tools.mkdir()
            capture = root / "capture.json"
            fake = tools / "fake_cargo.py"
            fake.write_text("import json,os,sys\nfrom pathlib import Path\nPath(os.environ['HELPER_CAPTURE']).write_text(json.dumps({'argv':sys.argv[1:],'cwd':os.getcwd()}))\nprint('FAKE_CARGO_CONTRACT_ONLY')\nsys.exit(int(os.environ.get('HELPER_EXIT','0')))\n")
            if os.name == "nt":
                (tools / "cargo.cmd").write_text(f'@"{sys.executable}" "{fake}" %*\n')
            else:
                cargo = tools / "cargo"
                cargo.write_text(f'#!{sys.executable}\nexec(compile(open({str(fake)!r}).read(), {str(fake)!r}, "exec"))\n')
                cargo.chmod(0o700)
            env = os.environ.copy()
            env.update(PATH=str(tools) + os.pathsep + env.get("PATH", ""), HELPER_CAPTURE=str(capture), PYTHONDONTWRITEBYTECODE="1")
            log = root / "helper build.log"
            result = subprocess.run([sys.executable, str(script), "video_transcoder", f"log={log}"], cwd=root, env=env, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            actual = json.loads(capture.read_text())
            self.assertEqual(actual, {"argv": ["build", "--locked", "--manifest-path", str(helper / "Cargo.toml"), "--release"], "cwd": str(root)})
            self.assertIn("FAKE_CARGO_CONTRACT_ONLY", log.read_text())
            env["HELPER_EXIT"] = "17"
            result = subprocess.run([sys.executable, str(script), "video_transcoder", "debug"], cwd=root, env=env, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 17)
            self.assertNotIn("--release", json.loads(capture.read_text())["argv"])
            self.assertIn("Exit    : 17", result.stderr)


if __name__ == "__main__":
    unittest.main()
