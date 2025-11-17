#!/usr/bin/env python3
import subprocess
import sys
from pathlib import Path
import multiprocessing as mp


def run(cmd: list[str], cwd: Path) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    subprocess.run(cmd, cwd=cwd, check=True)


def build_frontend(frontend_dir: Path) -> None:
    try:
        run(
            ["wasm-pack", "build", "--target", "web", "--release", "--out-dir", "dist/pkg"],
            cwd=frontend_dir,
        )
    except subprocess.CalledProcessError as e:
        print("Frontend build failed.", file=sys.stderr)
        sys.exit(e.returncode)


def build_backend(backend_dir: Path) -> None:
    try:
        run(
            ["cargo", "build", "--release", "-p", "groundstation_backend"],
            cwd=backend_dir,
        )
    except subprocess.CalledProcessError as e:
        print("Backend exited with error.", file=sys.stderr)
        sys.exit(e.returncode)


def main() -> None:
    repo_root = Path(__file__).resolve().parent
    frontend_dir = repo_root / "frontend"
    backend_dir = repo_root / "backend"

    # 1) Build the frontend WASM bundle
    bfe = mp.Process(target=build_frontend, args=(frontend_dir,))
    # 2) Run the backend (from workspace root so -p works)
    bbe = mp.Process(target=build_backend, args=(backend_dir,))
    bfe.start()
    bbe.start()
    bfe.join()
    bbe.join()

if __name__ == "__main__":
    main()
