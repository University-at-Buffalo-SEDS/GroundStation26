import tempfile
import unittest
from pathlib import Path
from unittest.mock import call, patch

import build


class FrontendBranchTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
