#!/usr/bin/env python3
import argparse
import os
import signal
import subprocess
import sys
import time
import platform
from pathlib import Path
from backend.build import refresh_sedsnet_dependency
from build import prepare_browser_asset_source


def warn_if_db_sidecars_present(repo_root: Path) -> None:
    db = repo_root / "data" / "groundstation.db"
    sidecars = [
        Path(f"{db}-wal"),
        Path(f"{db}-shm"),
        Path(f"{db}.wal"),
        Path(f"{db}.shm"),
        Path(f"{db}-journal"),
        Path(f"{db}.journal"),
    ]
    # Give the backend a brief window to finish shutdown cleanup.
    deadline = time.monotonic() + 2.0
    lingering = {p for p in sidecars if p.exists()}
    while lingering and time.monotonic() < deadline:
        time.sleep(0.1)
        lingering = {p for p in sidecars if p.exists()}

    if lingering:
        print(
            "Warning: SQLite sidecar files still present after backend exit: "
            + ", ".join(str(p) for p in sorted(lingering)),
            file=sys.stderr,
        )


def run(cmd: list[str], cwd: Path, env: dict[str, str] | None = None) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    proc = subprocess.Popen(cmd, cwd=cwd, env=env)
    code: int
    try:
        code = proc.wait()
    except KeyboardInterrupt:
        print("\nInterrupt received, requesting graceful backend shutdown...")
        if proc.poll() is None:
            proc.send_signal(signal.SIGINT)
            try:
                code = proc.wait(timeout=12)
            except subprocess.TimeoutExpired:
                proc.terminate()
                try:
                    code = proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    code = proc.wait()
        else:
            code = proc.returncode
    finally:
        warn_if_db_sidecars_present(cwd)

    if code != 0:
        raise subprocess.CalledProcessError(code, cmd)


def is_raspberry_pi() -> bool:
    if platform.system() != "Linux":
        return False
    for path in (
        Path("/sys/firmware/devicetree/base/model"),
        Path("/proc/device-tree/model"),
    ):
        try:
            if "raspberry pi" in path.read_text(errors="ignore").lower():
                return True
        except FileNotFoundError:
            continue
    return False


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build frontend and run groundstation backend."
    )
    parser.add_argument(
        "mode",
        nargs="?",
        choices=["testing", "hitl-mode", "test-fire-mode", "flight-sim", "debug"],
        help="Legacy positional mode. Use 'testing', 'hitl-mode', 'test-fire-mode', or 'debug'.",
    )
    parser.add_argument("--flight-sim", action="store_true", help="Run isolated FlightSim models; no physical transports or outputs.")
    parser.add_argument("--flight-sim-users", type=Path, help="Explicit read-only users.json snapshot for this isolated session; sessions/proofs are never copied.")
    parser.add_argument("--flight-sim-data-dir", type=Path, help="Parent for fresh isolated FlightSim sessions (existing sessions are never reused).")
    parser.add_argument(
        "--testing",
        action="store_true",
        help="Start the backend in testing mode (GS_STATION_MODE=testing).",
    )
    parser.add_argument(
        "--hitl-mode",
        action="store_true",
        help="Start the backend in HITL mode (GS_STATION_MODE=hitl).",
    )
    parser.add_argument(
        "--test-fire-mode",
        action="store_true",
        help="Start the backend in test-fire mode (GS_STATION_MODE=test_fire).",
    )
    parser.add_argument(
        "--debug",
        action="store_true",
        help="Build and run in debug mode for faster compile times.",
    )
    parser.add_argument(
        "--backend-only-build", "--backend-only",
        action="store_true",
        help="Reuse the existing WebUI unless --frontend-dev explicitly requests rebuilding it.",
    )
    parser.add_argument("--rrls", action="store_true", help="Run the headless recovery receiver/relay with local web and mobile access.")
    parser.add_argument("--frontend-dev", "--frontend_dev", action="store_true", help="Build the WebUI from the frontend dev branch (also accepts frontend_dev, like build.py).")
    parser.add_argument("--switchable-modes", metavar="MODES", help="Supervise the backend so a Simulation change from the UI can restart it (other mode switches happen in process). MODES (normal,hitl,test_fire,testing or all) is accepted for older callers; one binary holds every mode. The usual mode flag picks the default when no switch was saved.")
    parser.add_argument("--pairing", type=Path, metavar="FILE", help="Station pairing.json from pair_stations.py (sets GS_STATION_PAIRING_FILE). Without it, a pairing made in the UI is used.")
    arguments=["--frontend-dev" if argument in {"frontend_dev", "frontend-dev"} else argument for argument in sys.argv[1:]]
    args=parser.parse_args(arguments)
    return args


def main() -> None:
    args = parse_args()
    if args.rrls:
        os.environ["GS_RRLS_ROLE"] = "rrls"
    flight_sim_mode = args.flight_sim or args.mode == "flight-sim"
    testing_mode = args.testing or args.mode == "testing"
    hitl_mode = args.hitl_mode or args.mode == "hitl-mode"
    test_fire_mode = args.test_fire_mode or args.mode == "test-fire-mode"
    debug_mode = args.debug or args.mode == "debug"
    if sum([testing_mode, hitl_mode, test_fire_mode, flight_sim_mode]) > 1:
        print("Error: testing, hitl-mode, test-fire-mode, and flight-sim are mutually exclusive.", file=sys.stderr)
        sys.exit(2)

    if flight_sim_mode and args.rrls:
        raise ValueError("FlightSim cannot run as RRLS or proxy physical commands")
    if args.flight_sim_users and not flight_sim_mode:
        raise ValueError("--flight-sim-users requires --flight-sim")
    if args.flight_sim_data_dir and not flight_sim_mode:
        raise ValueError("--flight-sim-data-dir requires --flight-sim")
    if args.pairing:
        if flight_sim_mode:
            raise ValueError("FlightSim does not use station pairing")
        pairing = args.pairing.expanduser().resolve()
        if not pairing.is_file():
            print(f"Error: pairing file not found: {pairing}", file=sys.stderr)
            sys.exit(2)
        os.environ["GS_STATION_PAIRING_FILE"] = str(pairing)
    if args.switchable_modes:
        if flight_sim_mode:
            raise ValueError("FlightSim is an isolated session; it does not use switchable station modes")
        return run_switchable(args, testing_mode, hitl_mode, test_fire_mode, debug_mode)
    cmd = ["cargo", "run"]
    if not debug_mode:
        cmd.append("--release")
    cmd.extend(["-p", "groundstation_backend"])
    features: list[str] = []
    if is_raspberry_pi() and not args.rrls and not flight_sim_mode:
        features.append("raspberry_pi")
    # One binary holds every mode; the flag picks the boot mode and wins over
    # a selection saved by an earlier in-process switch.
    boot_mode = boot_mode_for(testing_mode or flight_sim_mode, hitl_mode, test_fire_mode)
    os.environ["GS_STATION_MODE"] = boot_mode
    print(f"Station mode at start: {boot_mode}")
    if features:
        cmd.extend(["--features", ",".join(features)])
    repo_root = Path(__file__).resolve().parent
    if args.frontend_dev or not args.backend_only_build:
        frontend_cmd = [sys.executable, str(repo_root / "build.py"), "frontend_web"]
        if args.frontend_dev:
            frontend_cmd.append("--frontend-dev")
        if debug_mode:
            frontend_cmd.append("debug")
        print(f"Running: {' '.join(frontend_cmd)} (cwd={repo_root})")
        subprocess.run(frontend_cmd, cwd=repo_root, check=True)
    # The child WebUI build records the exact station-asset source. Reuse it
    # here rather than independently fetching a potentially newer frontend.
    prepare_browser_asset_source(repo_root, frontend_dev=args.frontend_dev, refresh=False)
    try:
        refresh_sedsnet_dependency(repo_root)
        if flight_sim_mode:
            from flight_sim_runtime import prepare_environment
            root, env = prepare_environment(args.flight_sim_data_dir or Path.home() / ".groundstation26" / "flight_sim", dict(os.environ), repo_root / "frontend/dist/public", args.flight_sim_users)
            cmd.extend(["--manifest-path", str(repo_root / "Cargo.toml")])
            print(f"FlightSim — SIMULATED OUTPUTS; isolated session: {root}")
            if not args.flight_sim_users:
                print(f"Accounts are isolated. Provision with: {sys.executable} {repo_root / 'backend/tools/users_config_tui.py'} --file {root / 'users/users.json'}")
            run(cmd, cwd=root, env=env)
        else:
            run(cmd, cwd=repo_root)
    except subprocess.CalledProcessError as e:
        print("Backend exited with error.", file=sys.stderr)
        sys.exit(e.returncode)


def build_frontend(args: argparse.Namespace, repo_root: Path, debug_mode: bool) -> None:
    if args.frontend_dev or not args.backend_only_build:
        frontend_cmd = [sys.executable, str(repo_root / "build.py"), "frontend_web"]
        if args.frontend_dev:
            frontend_cmd.append("--frontend-dev")
        if debug_mode:
            frontend_cmd.append("debug")
        print(f"Running: {' '.join(frontend_cmd)} (cwd={repo_root})")
        subprocess.run(frontend_cmd, cwd=repo_root, check=True)
    prepare_browser_asset_source(repo_root, frontend_dev=args.frontend_dev, refresh=False)


def boot_mode_for(testing_mode: bool, hitl_mode: bool, test_fire_mode: bool) -> str:
    return "testing" if testing_mode else "hitl" if hitl_mode else "test_fire" if test_fire_mode else "normal"


def run_switchable(args: argparse.Namespace, testing_mode: bool, hitl_mode: bool, test_fire_mode: bool, debug_mode: bool) -> None:
    """Build the backend once, then supervise it. Mode switches happen inside
    the backend; a confirmed Simulation change exits it with
    station_modes.RESTART_EXIT_CODE and the selection starts. Without a saved
    selection the usual mode runs."""
    import station_modes

    default_mode = boot_mode_for(testing_mode, hitl_mode, test_fire_mode)
    modes = station_modes.parse_modes(args.switchable_modes)  # validated, then ignored
    if default_mode not in modes:
        modes.insert(0, default_mode)
    repo_root = Path(__file__).resolve().parent
    build_frontend(args, repo_root, debug_mode)
    refresh_sedsnet_dependency(repo_root)
    root = station_modes.build_modes(repo_root, modes, debug=debug_mode, pi=is_raspberry_pi() and not args.rrls)
    code = station_modes.supervise(repo_root, root, default_mode, args.rrls, cwd=repo_root)
    warn_if_db_sidecars_present(repo_root)
    if code != 0:
        raise subprocess.CalledProcessError(code, [str(root / station_modes.BINARY)])


if __name__ == "__main__":
    try:
        main()
    except FileNotFoundError as e:
        missing = e.filename or "<unknown>"
        print("\nError: run_groundstation failed because a required tool/file is missing.", file=sys.stderr)
        print(f"  Missing: {missing}", file=sys.stderr)
        print("Hint: ensure Rust toolchain is installed (`cargo`) and repo paths are valid.", file=sys.stderr)
        sys.exit(127)
    except subprocess.CalledProcessError as e:
        print("\nError: run_groundstation command failed.", file=sys.stderr)
        print(f"  Command : {' '.join(str(x) for x in e.cmd)}", file=sys.stderr)
        print(f"  Exit    : {e.returncode}", file=sys.stderr)
        print("Hint: rerun the printed command directly for full output.", file=sys.stderr)
        sys.exit(e.returncode)
    except Exception as e:
        print(f"\nError: run_groundstation failed unexpectedly: {e}", file=sys.stderr)
        print("Hint: run with a clean build (`python3 build.py`) and retry.", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print("\n\nexiting...")
        exit(0)
