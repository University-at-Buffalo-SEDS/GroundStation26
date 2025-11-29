#!/usr/bin/env python3
import multiprocessing as mp
import platform
import subprocess
import sys
from pathlib import Path


def run(cmd: list[str], cwd: Path) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    subprocess.run(cmd, cwd=cwd, check=True)


def is_raspberry_pi() -> bool:
    """Return True if this looks like a Raspberry Pi."""
    if platform.system() != "Linux":
        return False

    candidates = [
        Path("/sys/firmware/devicetree/base/model"),
        Path("/proc/device-tree/model"),
    ]

    for path in candidates:
        try:
            txt = path.read_text(errors="ignore").lower()
            if "raspberry pi" in txt:
                return True
        except FileNotFoundError:
            continue

    return False


def build_frontend(frontend_dir: Path) -> None:
    try:
        run(
            ["wasm-pack", "build", "--target", "web", "--release", "--out-dir", "dist/pkg"],
            cwd=frontend_dir,
        )
    except subprocess.CalledProcessError as e:
        print("Frontend build failed.", file=sys.stderr)
        sys.exit(e.returncode)


def build_backend(backend_dir: Path, force_pi: bool) -> None:
    cmd = ["cargo", "build", "--release", "-p", "groundstation_backend"]

    if force_pi:
        print("pi_build argument supplied → forcing `raspberry_pi` feature.")
        cmd.extend(["--features", "raspberry_pi"])

    else:
        if is_raspberry_pi():
            print("Detected Raspberry Pi → enabling `raspberry_pi` feature.")
            cmd.extend(["--features", "raspberry_pi"])
        else:
            print("Not running on Raspberry Pi → building without `raspberry_pi` feature.")

    try:
        run(cmd, cwd=backend_dir)
    except subprocess.CalledProcessError as e:
        print("Backend exited with error.", file=sys.stderr)
        sys.exit(e.returncode)


def main() -> None:
    # ----------------------
    # Argument parsing logic
    # ----------------------
    force_pi = False

    if len(sys.argv) == 2:
        arg = sys.argv[1].strip().lower()
        if arg == "pi_build":
            force_pi = True
        else:
            print(f"Error: Invalid argument '{arg}'. Valid option: pi_build")
            sys.exit(1)

    elif len(sys.argv) > 2:
        print("Error: Too many arguments. Valid usage:")
        print("  ./build.py")
        print("  ./build.py pi_build")
        sys.exit(1)

    # ----------------------
    repo_root = Path(__file__).resolve().parent
    frontend_dir = repo_root / "frontend"
    backend_dir = repo_root / "backend"

    # Run frontend & backend in parallel
    bfe = mp.Process(target=build_frontend, args=(frontend_dir,))
    bbe = mp.Process(target=build_backend, args=(backend_dir, force_pi))

    bfe.start()
    bbe.start()
    bfe.join()
    bbe.join()


if __name__ == "__main__":
    main()
