import importlib.util
import os
import subprocess
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("backend_build_refresh", ROOT / "backend/build.py")
BUILD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BUILD)


class DependencyRefreshTests(unittest.TestCase):
    @mock.patch.dict(os.environ, {}, clear=True)
    def test_refresh_uses_major_range_without_exact_version(self):
        with mock.patch.object(BUILD.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "")) as run:
            BUILD.refresh_sedsnet_dependency(ROOT)
        self.assertEqual(run.call_args.args[0], ["cargo", "update", "--package", "SEDSnet"])
        self.assertEqual(run.call_args.kwargs["cwd"], ROOT)

    @mock.patch.dict(os.environ, {"CARGO_NET_OFFLINE": "true"})
    def test_explicit_offline_build_keeps_existing_resolution(self):
        with mock.patch.object(BUILD.subprocess, "run") as run:
            BUILD.refresh_sedsnet_dependency(ROOT)
        run.assert_not_called()

    @mock.patch.dict(os.environ, {}, clear=True)
    def test_network_failure_and_timeout_leave_cached_build_available(self):
        for outcome in [subprocess.CompletedProcess([], 1, "network unavailable"),
                        subprocess.TimeoutExpired("cargo", 60)]:
            with self.subTest(outcome=type(outcome).__name__):
                with mock.patch.object(BUILD.subprocess, "run") as run:
                    if isinstance(outcome, Exception): run.side_effect = outcome
                    else: run.return_value = outcome
                    BUILD.refresh_sedsnet_dependency(ROOT)


if __name__ == "__main__":
    unittest.main()
