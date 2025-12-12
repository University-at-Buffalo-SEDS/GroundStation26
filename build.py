#!/usr/bin/env python3
import multiprocessing as mp
import platform
import subprocess
import sys
from pathlib import Path
from subprocess import DEVNULL


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


def get_compose_base_cmd() -> list[str]:
    """
    Return the base command for docker compose, preferring `docker compose`
    but falling back to `docker-compose` if needed.
    Exits with an error if neither is available.
    """
    # Try `docker compose`
    try:
        subprocess.run(
            ["docker", "compose", "version"],
            stdout=DEVNULL,
            stderr=DEVNULL,
            check=True,
        )
        return ["docker", "compose"]
    except (FileNotFoundError, subprocess.CalledProcessError):
        pass

    # Try legacy `docker-compose`
    try:
        subprocess.run(
            ["docker-compose", "version"],
            stdout=DEVNULL,
            stderr=DEVNULL,
            check=True,
        )
        return ["docker-compose"]
    except (FileNotFoundError, subprocess.CalledProcessError):
        print(
            "Error: Neither 'docker compose' nor 'docker-compose' is available.\n"
            "Please install Docker and Docker Compose.",
            file=sys.stderr,
        )
        sys.exit(1)


def build_docker(repo_root: Path, pi_build: bool) -> None:
    """
    Build using docker compose. If pi_build is True, pass PI_BUILD as a build-arg.
    """
    compose_cmd = get_compose_base_cmd()
    cmd: list[str] = [*compose_cmd, "build"]

    if pi_build:
        # Presence of PI_BUILD is the signal for a Pi build in the Dockerfile.
        print("Pi build (docker) → passing --build-arg PI_BUILD=")
        cmd.extend(["--build-arg", "PI_BUILD="])

    run(cmd, cwd=repo_root)


def build_frontend(frontend_dir: Path) -> None:
    try:
        run(
            [
                "wasm-pack",
                "build",
                "--target",
                "web",
                "--release",
                "--out-dir",
                "dist/pkg",
            ],
            cwd=frontend_dir,
        )
    except subprocess.CalledProcessError as e:
        print("Frontend build failed.", file=sys.stderr)
        sys.exit(e.returncode)


def build_backend(backend_dir: Path, force_pi: bool, force_no_pi: bool, testing_mode: bool) -> None:
    cmd = ["cargo", "build", "--release", "-p", "groundstation_backend"]

    if force_pi and force_no_pi:
        print("Error: Both pi_build and no_pi were requested. Choose one.", file=sys.stderr)
        sys.exit(1)

    if force_pi:
        print("pi_build argument supplied → forcing `raspberry_pi` feature.")
        cmd.extend(["--features", "raspberry_pi"])
    elif force_no_pi:
        print("no_pi argument supplied → forcing build WITHOUT `raspberry_pi` feature, even on a Pi.")
        # No feature added, even if running on Pi.
    else:
        if is_raspberry_pi():
            print("Detected Raspberry Pi → enabling `raspberry_pi` feature.")
            cmd.extend(["--features", "raspberry_pi"])
        else:
            print(
                "Not running on Raspberry Pi → building without `raspberry_pi` feature."
            )
    if testing_mode:
        print("Testing mode enabled → adding `testing` feature.")
        if "--features" in cmd:
            cmd[cmd.index("--features") + 1] += ",testing"
        else:
            cmd.extend(["--features", "testing"])

    try:
        run(cmd, cwd=backend_dir)
    except subprocess.CalledProcessError as e:
        print("Backend exited with error.", file=sys.stderr)
        sys.exit(e.returncode)


def print_usage() -> None:
    print("Usage:")
    print("  ./build.py")
    print("  ./build.py pi_build")
    print("  ./build.py no_pi")
    print("  ./build.py docker")
    print("  ./build.py docker pi_build")
    print("  ./build.py docker no_pi")
    print("  ./build.py testing")
    print("  ./build.py pi_build testing")
    print("  ./build.py no_pi testing")
    print("  ./build.py docker testing")
    print("  ./build.py docker pi_build testing")
    print("  ./build.py docker no_pi testing")
    sys.exit(1)


def main() -> None:
    # ----------------------
    # Argument parsing logic
    # ----------------------
    force_pi = False
    force_no_pi = False
    docker_mode = False
    testing_mode = False

    # Accept 0, 1, or 2 args (script name + up to 2 extra)
    args = [a.strip().lower() for a in sys.argv[1:]]

    if len(args) > 3:
        print("Error: Too many arguments.", file=sys.stderr)
        print_usage()

    for arg in args:
        if arg == "pi_build":
            force_pi = True
        elif arg == "no_pi":
            force_no_pi = True
        elif arg == "docker":
            docker_mode = True
        elif arg == "testing":
            testing_mode = True
        else:
            print(f"Error: Invalid argument '{arg}'.", file=sys.stderr)
            print_usage()

    if force_pi and force_no_pi:
        print("Error: Cannot specify both 'pi_build' and 'no_pi'.", file=sys.stderr)
        sys.exit(1)

    repo_root = Path(__file__).resolve().parent
    frontend_dir = repo_root / "frontend"
    backend_dir = repo_root / "backend"

    # ----------------------
    # Docker mode
    # ----------------------
    if docker_mode:
        if force_pi and force_no_pi:
            print("Error: Cannot specify both 'pi_build' and 'no_pi' in docker mode.", file=sys.stderr)
            sys.exit(1)

        if force_no_pi:
            print(
                "Docker mode: no_pi override supplied → PI_BUILD will NOT be set, even on Raspberry Pi."
            )
            pi_build_flag = False
        else:
            if not force_pi and is_raspberry_pi():
                print(
                    "Docker mode: detected Raspberry Pi host → enabling PI_BUILD build arg."
                )
                force_pi = True
            elif force_pi:
                print(
                    "Docker mode: pi_build override supplied → enabling PI_BUILD build arg."
                )
            else:
                print(
                    "Docker mode: not on Raspberry Pi and no pi_build override → PI_BUILD will not be set."
                )

            pi_build_flag = force_pi

        build_docker(repo_root, pi_build=pi_build_flag)
        return

    # ----------------------
    # Normal local build mode
    # ----------------------
    # Run frontend & backend in parallel
    bfe = mp.Process(target=build_frontend, args=(frontend_dir,))
    bbe = mp.Process(target=build_backend, args=(backend_dir, force_pi, force_no_pi, testing_mode))

    bfe.start()
    bbe.start()
    bfe.join()
    bbe.join()


if __name__ == "__main__":
    main()
