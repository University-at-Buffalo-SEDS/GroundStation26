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


class StationPathTests(unittest.TestCase):
    def test_backend_compiles_at_the_station_checkout_path(self) -> None:
        # The station finds config, layout and users through
        # env!("CARGO_MANIFEST_DIR"); a binary built at another path cannot
        # find them and starts without users or GSE configuration.
        with tempfile.TemporaryDirectory() as tmp, \
                patch.dict(os.environ, {remote_build.ENV: "jupiter", "GS_FRONTEND_SOURCE_DIR": tmp}):
            repo = Path(tmp).resolve()
            with patch.object(remote_build, "_check_local_target"), \
                    patch.object(remote_build, "_image", return_value="img"), \
                    patch.object(remote_build.Remote, "push"), \
                    patch.object(remote_build.Remote, "pull"), \
                    patch.object(remote_build.Remote, "run") as run, \
                    patch.object(Path, "replace"), patch.object(Path, "chmod"):
                remote_build.build_backend(repo, [], False)
            command = run.call_args.args[0]
            self.assertIn(f":{repo} ", command)
            self.assertIn(f"-w {repo}/backend ", command)
            self.assertIn(f"GS_FRONTEND_SOURCE_DIR={repo}", command)


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


class SimBundleTests(unittest.TestCase):
    def test_sim_bundle_stages_sources_builds_in_the_container_and_installs(self) -> None:
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {remote_build.ENV: "rylan:Secret@jupiter"}):
            repo = Path(tmp).resolve()
            local_runs = []
            with patch.object(remote_build, "_image", return_value="img"), \
                    patch.object(remote_build.Remote, "push") as push, \
                    patch.object(remote_build.Remote, "rsync") as rsync, \
                    patch.object(remote_build.Remote, "pull") as pull, \
                    patch.object(remote_build.Remote, "run") as run, \
                    patch.object(remote_build.subprocess, "run", side_effect=lambda cmd, **kw: local_runs.append(cmd)):
                out = remote_build.build_sim_bundle(repo, abi="armhf")
            # sources staged locally as plain trees, then sent; the checkout pushed as for a station build
            self.assertEqual(local_runs[0][2:4], ["stage", "--out"])
            push.assert_called_once()
            self.assertIn("-sim-sources/", rsync.call_args.args[1])
            command = run.call_args.args[0]
            self.assertIn("build --sources /build/", command)
            self.assertIn("bundle --out /build/firmware-sim-bundle.tar.gz", command)
            self.assertNotIn("Secret", command, "no credentials reach the remote command")
            self.assertIn("FIRMWARE_SIM_HOST=armhf", command)
            self.assertIn("FIRMWARE_SIM_COORDINATOR_TARGET=aarch64-unknown-linux-gnu", command)
            self.assertIn("/build/sim-cache-armhf", command)
            self.assertEqual(pull.call_args.args[1], repo / "target" / "firmware-sim-bundle.tar.gz")
            self.assertEqual(local_runs[-1][2], "install")
            self.assertEqual(out, repo / "target" / "firmware-sim-bundle.tar.gz")

    def test_builder_image_carries_the_native_sim_toolchain_and_keeps_the_station_one(self) -> None:
        for need in ("gcc-i686-linux-gnu", "libc6-dev-i386-cross", "gcc-arm-none-eabi", "ninja-build", "i686-unknown-linux-gnu", "gcc-arm-linux-gnueabihf",
                     "armv7-unknown-linux-gnueabihf",
                     "gcc-aarch64-linux-gnu", "aarch64-unknown-linux-gnu", "wasm32-unknown-unknown"):
            self.assertIn(need, remote_build.DOCKERFILE)
        # Ubuntu's gcc-multilib conflicts with the aarch64/armhf cross compilers, so apt cannot install both.
        self.assertNotIn("gcc-multilib", remote_build.DOCKERFILE)


class ConnectTimeoutTests(unittest.TestCase):
    def test_ssh_options_bound_the_connect(self) -> None:
        with patch.dict(os.environ, {"GS_REMOTE_CONNECT_TIMEOUT": "7"}):
            options = remote_build.Remote("builder").ssh_options()
        self.assertIn("ConnectTimeout=7", options)
        self.assertIn("ConnectionAttempts=1", options)

    def test_unreachable_host_fails_before_ssh_prompts(self) -> None:
        remote = remote_build.Remote("rylan@builder")
        resolved = subprocess.CompletedProcess([], 0, stdout="hostname 192.0.2.1\nport 22\nproxyjump none\n", stderr="")
        with patch.object(remote_build.subprocess, "run", side_effect=[resolved]) as run, \
                patch.object(remote_build.socket, "create_connection", side_effect=TimeoutError("timed out")):
            with self.assertRaises(SystemExit) as stop:
                remote.connect()
        self.assertIn("192.0.2.1:22 did not answer", str(stop.exception))
        # Only `ssh -G` ran: the login (`ssh ... true`) never started.
        self.assertEqual(run.call_count, 1)
        self.assertEqual(run.call_args.args[0][:2], ["ssh", "-G"])

    def test_proxied_host_is_left_to_ssh(self) -> None:
        remote = remote_build.Remote("builder")
        resolved = subprocess.CompletedProcess([], 0, stdout="hostname inner\nport 22\nproxyjump bastion\n", stderr="")
        with patch.object(remote_build.subprocess, "run", return_value=resolved), \
                patch.object(remote_build.socket, "create_connection") as probe:
            remote.check_reachable()
        probe.assert_not_called()


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
