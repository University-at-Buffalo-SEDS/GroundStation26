#!/usr/bin/env python3
import subprocess
import sys
from pathlib import Path


def run(cmd: list[str], cwd: Path) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    subprocess.run(cmd, cwd=cwd, check=True)


def main() -> None:
    repo_root = Path(__file__).resolve().parent
    frontend_dir = repo_root / "frontend"

    # 1) Build the frontend WASM bundle
    try:
        run(
            ["wasm-pack", "build", "--target", "web", "--release", "--out-dir", "dist/pkg"],
            cwd=frontend_dir,
        )
    except subprocess.CalledProcessError as e:
        print("Frontend build failed.", file=sys.stderr)
        sys.exit(e.returncode)

    # 2) Run the backend (from workspace root so -p works)
    try:
        run(
            ["cargo", "run", "-p", "groundstation_backend"],
            cwd=repo_root,
        )
    except subprocess.CalledProcessError as e:
        print("Backend exited with error.", file=sys.stderr)
        sys.exit(e.returncode)


if __name__ == "__main__":
    main()
