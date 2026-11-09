import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import remote_build  # noqa: E402


class HostSpecTests(unittest.TestCase):
    def test_accepts_password_user_and_bare_host_forms(self) -> None:
        for spec, destination, password in [
            ("rylan:s3cr:et@10.0.0.5", "rylan@10.0.0.5", "s3cr:et"),
            ("rylan@jupiter", "rylan@jupiter", None),
            ("jupiter", "jupiter", None),
            ("rylan:@jupiter", "rylan@jupiter", ""),
        ]:
            with self.subTest(spec=spec):
                remote = remote_build.Remote(spec)
                self.assertEqual(remote.destination, destination)
                self.assertEqual(remote.password, password)

    def test_rejects_empty_or_malformed_hosts(self) -> None:
        for spec in ["", "rylan@", "bad host"]:
            with self.subTest(spec=spec), self.assertRaises(SystemExit):
                remote_build.Remote(spec)

    def test_password_reaches_ssh_only_through_askpass(self) -> None:
        remote = remote_build.Remote("rylan:Pa55word@jupiter")
        try:
            env = remote.env()
            self.assertEqual(env[remote_build.PASSWORD_ENV], "Pa55word")
            self.assertEqual(env["SSH_ASKPASS_REQUIRE"], "force")
            askpass = Path(env["SSH_ASKPASS"])
            self.assertNotIn("Pa55word", askpass.read_text())
            self.assertEqual(subprocess.run([str(askpass)], env=env, capture_output=True, text=True).stdout,
                             "Pa55word\n")
            self.assertNotIn("Pa55word", " ".join(remote.ssh_options()))
        finally:
            remote.close()
        self.assertFalse(askpass.exists())


class SharedConnectionTests(unittest.TestCase):
    def test_parallel_builds_share_one_authenticated_connection(self) -> None:
        # Separate connections made the backend and frontend builds prompt for
        # the password on the same terminal at once, splitting the keystrokes.
        first, second = remote_build.Remote("rylan@jupiter"), remote_build.Remote("rylan@jupiter")
        option = lambda remote: next(o for o in remote.ssh_options() if o.startswith("ControlPath="))
        self.assertEqual(option(first), option(second))
        self.assertIn("ControlMaster=auto", first.ssh_options())


class GuardTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("setsid") and shutil.which("bash"), "needs Linux setsid")
    def test_remote_work_stops_when_the_local_side_goes_away(self) -> None:
        marker = f"gsrb-guard-test-{os.getpid()}"
        guard = subprocess.Popen(["bash", "-c", remote_build.GUARD, "guard", f"sleep 60 # {marker}", ""],
                                 stdin=subprocess.PIPE, stdout=subprocess.DEVNULL)
        alive = lambda: subprocess.run(["pgrep", "-f", marker], capture_output=True).returncode == 0
        deadline = time.monotonic() + 5
        while not alive():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.05)
        guard.stdin.close()  # what Ctrl-C, a kill or a lost connection does
        self.assertNotEqual(guard.wait(timeout=10), 0)
        deadline = time.monotonic() + 5
        while alive():
            self.assertLess(time.monotonic(), deadline, "remote work outlived its build")
            time.sleep(0.05)

    @unittest.skipUnless(shutil.which("setsid") and shutil.which("bash"), "needs Linux setsid")
    def test_finished_remote_work_returns_its_status(self) -> None:
        guard = subprocess.run(["bash", "-c", remote_build.GUARD, "guard", "exit 7", ""],
                               stdin=subprocess.PIPE, timeout=10)
        self.assertEqual(guard.returncode, 7)


class FrontendCopyBackTests(unittest.TestCase):
    def test_copy_back_touches_only_dist_public(self) -> None:
        # Mirroring the whole remote dist/ deleted the prior output the local
        # wrapper had set aside in dist/, and its cleanup then failed.
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {remote_build.ENV: "jupiter"}):
            checkout = Path(tmp)
            (checkout / "build.py").write_text("")
            prior = checkout / "dist/.gs-public-prior-x/public"
            prior.mkdir(parents=True)
            with patch.object(remote_build, "_image", return_value="img"), \
                    patch.object(remote_build.Remote, "push"), \
                    patch.object(remote_build, "_container"), \
                    patch.object(remote_build.Remote, "pull") as pull:
                remote_build.build_frontend(checkout, checkout / "build.py", ["frontend_web"])
            source, local = pull.call_args.args[:2]
            self.assertTrue(source.endswith("/dist/public/"), source)
            self.assertEqual(Path(local), (checkout / "dist/public").resolve())
            self.assertTrue(prior.is_dir())


class ArgumentTests(unittest.TestCase):
    def test_takes_every_remote_form_and_keeps_other_arguments(self) -> None:
        for args, spec in [
            (["hitl-mode", "--remote", "Rylan:PaSs@h"], "Rylan:PaSs@h"),
            (["--remote=h", "hitl-mode"], "h"),
            (["remote=user@h", "hitl-mode"], "user@h"),
        ]:
            with self.subTest(args=args):
                rest, taken = remote_build.take_argument(args)
                self.assertEqual(rest, ["hitl-mode"])
                self.assertEqual(taken, spec)
        self.assertEqual(remote_build.take_argument(["debug"]), (["debug"], None))
        with self.assertRaises(SystemExit):
            remote_build.take_argument(["--remote"])


class CheckoutFileTests(unittest.TestCase):
    def test_sends_git_tracked_force_added_and_lock_files_but_not_output(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            git = lambda *args: subprocess.run(["git", "-C", tmp, *args], check=True, capture_output=True)
            git("init", "-q")
            (root / ".gitignore").write_text("vendor/\ntarget/\nCargo.lock\n")
            for name, text in [("Cargo.toml", "[workspace]\n"), ("Cargo.lock", "lock\n"), ("src/main.rs", ""),
                               ("vendor/asset.js", "x"), ("target/out", "x"), ("new.txt", "")]:
                (root / name).parent.mkdir(parents=True, exist_ok=True)
                (root / name).write_text(text)
            git("add", ".gitignore", "Cargo.toml", "src/main.rs")
            git("add", "-f", "vendor/asset.js")
            files = set(remote_build._checkout_files(root))
        self.assertEqual(files, {".gitignore", "Cargo.toml", "Cargo.lock", "src/main.rs", "vendor/asset.js", "new.txt"})


class BuildWrapperTests(unittest.TestCase):
    def test_backend_build_uses_remote_when_requested(self) -> None:
        import importlib.util
        # Loaded by path: the top-level build.py shares the module name.
        spec = importlib.util.spec_from_file_location(
            "backend_build", Path(__file__).resolve().parents[1] / "backend/build.py")
        backend_build = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(backend_build)
        with patch.dict(os.environ, {remote_build.ENV: "jupiter"}), \
                patch.object(backend_build, "refresh_sedsnet_dependency"), \
                patch.object(backend_build, "run") as run, \
                patch.object(remote_build, "build_backend") as remote:
            backend_build.build_backend(Path("backend"), True, False, False, False, False, False)
        run.assert_not_called()
        remote.assert_called_once_with(Path("."), ["raspberry_pi"], False)


if __name__ == "__main__":
    unittest.main()
