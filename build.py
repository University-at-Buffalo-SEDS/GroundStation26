#!/usr/bin/env python3
import multiprocessing as mp
import os
import platform
import subprocess
import sys
from pathlib import Path
from subprocess import DEVNULL
from typing import Optional


def run(cmd: list[str], cwd: Path, env: Optional[dict[str, str]] = None) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    merged = os.environ.copy()
    if env:
        merged.update(env)
    subprocess.run(cmd, cwd=cwd, check=True, env=merged)


def run_script(path: Path, cwd: Path, env: Optional[dict[str, str]] = None) -> None:
    if not path.exists():
        raise FileNotFoundError(f"Script not found: {path}")
    if not path.is_file():
        raise FileNotFoundError(f"Not a file: {path}")
    run(["bash", str(path)], cwd=cwd, env=env)


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


def patch_plist(frontend_dir: Path) -> None:
    """
    Run frontend/scripts/patch_plist.sh (used to patch macOS/iOS Info.plist, etc.)
    """
    script = frontend_dir / "scripts" / "patch_plist.sh"
    run_script(script, cwd=frontend_dir)


def build_frontend(
    frontend_dir: Path,
    platform_name: Optional[str] = None,
    *,
    rust_target: Optional[str] = None,
) -> None:
    """
    Build the frontend.

    - platform_name: passed to dx --platform (e.g. "ios", "web", "macos")
    - rust_target: passed to dx --target (e.g. "aarch64-apple-ios")
    """
    try:
        cmd = ["dx", "bundle", "--release"]

        if platform_name:
            cmd.extend(["--platform", platform_name])
        else:
            cmd.extend(["--platform", "web"])

        if rust_target:
            cmd.extend(["--target", rust_target])

        run(cmd, cwd=frontend_dir)

        # Patch plist only for iOS bundles (device or sim).
        if platform_name == "ios":
            patch_plist(frontend_dir)

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
    else:
        if is_raspberry_pi():
            print("Detected Raspberry Pi → enabling `raspberry_pi` feature.")
            cmd.extend(["--features", "raspberry_pi"])
        else:
            print("Not running on Raspberry Pi → building without `raspberry_pi` feature.")

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
    print("  ./build.py                         # local: build frontend+backend (parallel)")
    print("  ./build.py pi_build                # local: backend w/ raspberry_pi feature")
    print("  ./build.py no_pi                   # local: backend w/o raspberry_pi feature")
    print("  ./build.py testing                 # local: backend w/ testing feature")
    print("  ./build.py docker [pi_build|no_pi] [testing]")
    print("")
    print("Frontend-only OS builds:")
    print("  ./build.py ios                     # iPhoneOS device build (aarch64-apple-ios)")
    print("  ./build.py ios_sim                 # iOS simulator build (aarch64-apple-ios-sim)")
    print("  ./build.py macos")
    print("  ./build.py windows")
    print("  ./build.py android")
    print("  ./build.py linux")
    sys.exit(1)


def main() -> None:
    force_pi = False
    force_no_pi = False
    docker_mode = False
    testing_mode = False

    frontend_only_platform: Optional[str] = None
    frontend_rust_target: Optional[str] = None

    args = [a.strip().lower() for a in sys.argv[1:]]

    if len(args) > 4:
        print("Error: Too many arguments.", file=sys.stderr)
        print_usage()

    # Frontend-only modes map to dx --platform; ios_sim is still platform ios but different rust target.
    frontend_platform_map = {
        "ios": ("ios", "aarch64-apple-ios"),
        "ios_sim": ("ios", "aarch64-apple-ios-sim"),
        "macos": ("macos", None),
        "windows": ("windows", None),
        "android": ("android", None),
        "linux": ("linux", None),
    }

    for arg in args:
        if arg == "pi_build":
            force_pi = True
        elif arg == "no_pi":
            force_no_pi = True
        elif arg == "docker":
            docker_mode = True
        elif arg == "testing":
            testing_mode = True
        elif arg in frontend_platform_map:
            if frontend_only_platform is not None:
                print("Error: Only one frontend-only platform may be specified.", file=sys.stderr)
                print_usage()
            frontend_only_platform, frontend_rust_target = frontend_platform_map[arg]
        else:
            print(f"Error: Invalid argument '{arg}'.", file=sys.stderr)
            print_usage()

    if force_pi and force_no_pi:
        print("Error: Cannot specify both 'pi_build' and 'no_pi'.", file=sys.stderr)
        sys.exit(1)

    # If user picked a frontend-only platform, forbid mixing with backend/docker flags.
    if frontend_only_platform is not None:
        if docker_mode or force_pi or force_no_pi or testing_mode:
            print(
                "Error: Frontend-only builds (ios/ios_sim/macos/windows/android/linux) cannot be combined "
                "with docker/pi_build/no_pi/testing.",
                file=sys.stderr,
            )
            print_usage()

    repo_root = Path(__file__).resolve().parent
    frontend_dir = repo_root / "frontend"
    backend_dir = repo_root / "backend"

    # Frontend-only build mode
    if frontend_only_platform is not None:
        build_frontend(frontend_dir, platform_name=frontend_only_platform, rust_target=frontend_rust_target)
        return

    # Docker mode
    if docker_mode:
        if force_pi and force_no_pi:
            print("Error: Cannot specify both 'pi_build' and 'no_pi' in docker mode.", file=sys.stderr)
            sys.exit(1)

        if force_no_pi:
            print("Docker mode: no_pi override supplied → PI_BUILD will NOT be set, even on Raspberry Pi.")
            pi_build_flag = False
        else:
            if not force_pi and is_raspberry_pi():
                print("Docker mode: detected Raspberry Pi host → enabling PI_BUILD build arg.")
                force_pi = True
            elif force_pi:
                print("Docker mode: pi_build override supplied → enabling PI_BUILD build arg.")
            else:
                print("Docker mode: not on Raspberry Pi and no pi_build override → PI_BUILD will not be set.")
            pi_build_flag = force_pi

        build_docker(repo_root, pi_build=pi_build_flag)
        return

    # Normal local build mode: frontend & backend in parallel
    bfe = mp.Process(target=build_frontend, args=(frontend_dir, None))
    bbe = mp.Process(target=build_backend, args=(backend_dir, force_pi, force_no_pi, testing_mode))

    bfe.start()
    bbe.start()
    bfe.join()
    bbe.join()


if __name__ == "__main__":
    main()
