#!/usr/bin/env python3
import argparse
import signal
import subprocess
import sys
import time
import platform
from pathlib import Path


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


def run(cmd: list[str], cwd: Path) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    proc = subprocess.Popen(cmd, cwd=cwd)
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
        choices=["testing", "hitl-mode", "debug"],
        help="Legacy positional mode. Use 'testing', 'hitl-mode', or 'debug'.",
    )
    parser.add_argument(
        "--testing",
        action="store_true",
        help="Enable backend 'testing' feature.",
    )
    parser.add_argument(
        "--hitl-mode",
        action="store_true",
        help="Enable backend 'hitl_mode' feature.",
    )
    parser.add_argument(
        "--debug",
        action="store_true",
        help="Build and run in debug mode for faster compile times.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    testing_mode = args.testing or args.mode == "testing"
    hitl_mode = args.hitl_mode or args.mode == "hitl-mode"
    debug_mode = args.debug or args.mode == "debug"
    if testing_mode and hitl_mode:
        print("Error: testing mode and hitl-mode are mutually exclusive.", file=sys.stderr)
        sys.exit(2)

    cmd = ["cargo", "run"]
    if not debug_mode:
        cmd.append("--release")
    cmd.extend(["-p", "groundstation_backend"])
    features: list[str] = []
    if is_raspberry_pi():
        features.append("raspberry_pi")
    if testing_mode:
        features.append("testing")
    if hitl_mode:
        features.append("hitl_mode")
    if features:
        cmd.extend(["--features", ",".join(features)])
    repo_root = Path(__file__).resolve().parent
    frontend_cmd = [sys.executable, str(repo_root / "frontend" / "build.py"), "frontend_web"]
    if debug_mode:
        frontend_cmd.append("debug")
    print(f"Running: {' '.join(frontend_cmd)} (cwd={repo_root})")
    subprocess.run(frontend_cmd, cwd=repo_root, check=True)
    try:
        run(cmd, cwd=repo_root)
    except subprocess.CalledProcessError as e:
        print("Backend exited with error.", file=sys.stderr)
        sys.exit(e.returncode)


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
