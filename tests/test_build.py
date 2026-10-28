import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

import build


class FrontendBranchTests(unittest.TestCase):
    def test_docker_cache_tracks_selected_frontend_branch_tip(self) -> None:
        for dev, branch in [(False, 'main'), (True, 'dev')]:
            with self.subTest(branch=branch), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                revision = 'a' * 40
                with patch.object(build, 'get_compose_base_cmd', return_value=['docker', 'compose']), patch.object(build, 'run_capture', return_value=f'{revision}\trefs/heads/{branch}') as capture, patch.object(build, 'run') as run:
                    build.build_docker(root, False, False, False, dev)
                capture.assert_called_once_with(['git', 'ls-remote', '--heads', build.FRONTEND_REPO_URL, f'refs/heads/{branch}'], cwd=root)
                command = run.call_args.args[0]
                self.assertIn(f'FRONTEND_SOURCE_REVISION={revision}', command)
                self.assertEqual('FRONTEND_DEV=TRUE' in command, dev)

    def test_docker_rejects_missing_frontend_branch_before_build(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.object(build, 'get_compose_base_cmd', return_value=['docker', 'compose']), patch.object(build, 'run_capture', return_value=''), patch.object(build, 'run') as run:
            with self.assertRaisesRegex(RuntimeError, 'Unable to resolve frontend branch'):
                build.build_docker(Path(tmp), False, False, False, False)
            run.assert_not_called()

    def test_frontend_source_defaults_to_stable_main(self) -> None:
        self.assertEqual(build._frontend_branch(False), "main")

    def test_frontend_dev_flag_selects_dev(self) -> None:
        self.assertEqual(build._frontend_branch(True), "dev")

    def test_new_checkout_clones_the_selected_branch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.object(build, "run") as run:
            checkout = Path(tmp) / "frontend"
            build._ensure_frontend_checkout(checkout, "dev")

        run.assert_called_once_with(
            [
                "git",
                "clone",
                "--depth",
                "1",
                "--branch",
                "dev",
                build.FRONTEND_REPO_URL,
                str(checkout),
            ],
            cwd=checkout.parent,
        )

    def test_existing_checkout_switches_to_selected_branch_before_pull(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.object(build, "run") as run, patch.object(
            build,
            "run_capture",
            side_effect=[
                "true",
                build.FRONTEND_REPO_URL,
                "",
                "main\ndev",
            ],
        ):
            checkout = Path(tmp)
            build._ensure_frontend_checkout(checkout, "dev")

        self.assertEqual(
            run.call_args_list,
            [
                call(
                    ["git", "-C", str(checkout), "fetch", "origin", "dev:refs/remotes/origin/dev"],
                    cwd=checkout,
                ),
                call(["git", "-C", str(checkout), "switch", "dev"], cwd=checkout),
                call(["git", "-C", str(checkout), "pull", "--ff-only", "origin", "dev"], cwd=checkout),
            ],
        )

    def test_single_branch_checkout_creates_selected_branch_without_tracking(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.object(build, "run") as run, patch.object(
            build,
            "run_capture",
            side_effect=[
                "true",
                build.FRONTEND_REPO_URL,
                "",
                "main",
            ],
        ):
            checkout = Path(tmp)
            build._ensure_frontend_checkout(checkout, "dev")

        self.assertIn(
            call(
                [
                    "git",
                    "-C",
                    str(checkout),
                    "switch",
                    "--no-track",
                    "-c",
                    "dev",
                    "origin/dev",
                ],
                cwd=checkout,
            ),
            run.call_args_list,
        )


if __name__ == "__main__":
    unittest.main()
