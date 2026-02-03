#!/usr/bin/env python3
import multiprocessing as mp
import os
import platform
import plistlib
import re
import shutil
import subprocess
import sys
from pathlib import Path
from subprocess import DEVNULL
from typing import Optional

APP_NAME = "GroundStation 26"
LEGACY_APP_NAME = "GroundstationFrontend"
DIST_DIRNAME = "dist"
APP_BUNDLE_NAME = f"{APP_NAME}.app"
LEGACY_APP_BUNDLE_NAME = f"{LEGACY_APP_NAME}.app"


def run(cmd: list[str], cwd: Path, env: Optional[dict[str, str]] = None) -> None:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    merged = os.environ.copy()
    if env:
        merged.update(env)
    subprocess.run(cmd, cwd=cwd, check=True, env=merged)


def run_capture(cmd: list[str], cwd: Path) -> str:
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    out = subprocess.check_output(cmd, cwd=cwd)
    return out.decode("utf-8", errors="replace")


def run_script(path: Path, cwd: Path, env: Optional[dict[str, str]] = None) -> None:
    if not path.exists():
        raise FileNotFoundError(f"Script not found: {path}")
    if not path.is_file():
        raise FileNotFoundError(f"Not a file: {path}")
    run(["bash", str(path)], cwd=cwd, env=env)


def _list_connected_ios_device_ids(frontend_dir: Path) -> list[str]:
    """
    Return a list of connected iPhone/iPad UDIDs as reported by ios-deploy --detect.

    ios-deploy sometimes prints iPads as "unknownos". We'll include those as long as
    they are NOT watches and look like a real iOS/iPadOS device entry.
    """
    if platform.system() != "Darwin":
        print("Error: iOS device deploy requires macOS.", file=sys.stderr)
        sys.exit(1)

    try:
        out = run_capture(["ios-deploy", "--detect"], cwd=frontend_dir)
    except subprocess.CalledProcessError:
        print(
            "Warning: failed to run `ios-deploy --detect`; falling back to single-device deploy.",
            file=sys.stderr,
        )
        return []
    except FileNotFoundError:
        print("Error: ios-deploy not found. Install it first (e.g. `brew install ios-deploy`).", file=sys.stderr)
        sys.exit(1)

    ids: list[str] = []

    # Example:
    # [....] Found <UDID> (D74AP, iPhone 14 Pro Max, iphoneos, arm64e, 26.2, 23C55) a.k.a. 'Rylan’s iPhone' ...
    # [....] Found <UDID> (..., unknownos, ...) a.k.a. 'Rylan’s iPad' ...
    pat = re.compile(r"\bFound\s+([0-9A-Fa-f-]+)\s+\(([^)]*)\)(.*)$")

    for line in out.splitlines():
        m = pat.search(line)
        if not m:
            continue

        udid = m.group(1).strip()
        meta = m.group(2).lower()
        tail = m.group(3).lower()

        # Always ignore watches / companion proxy entries
        if "watch" in meta or "watch" in tail or "companion" in tail:
            continue

        # Preferred: explicit iphoneos/ipados
        if "iphoneos" in meta or "ipados" in meta:
            ids.append(udid)
            continue

        # Fallback: ios-deploy sometimes prints "unknownos" for iPads
        # Include it if there's an alias and it smells like a phone/tablet entry.
        if "unknownos" in meta or "uknownos" in meta:
            # If the alias contains ipad/iphone, accept
            if "a.k.a." in tail and ("ipad" in tail or "iphone" in tail):
                ids.append(udid)
                continue

            # Or accept if it's connected through USB and NOT a watch
            if "connected through usb" in tail and "a.k.a." in tail:
                ids.append(udid)
                continue

    # Dedup while preserving order
    seen = set()
    deduped: list[str] = []
    for d in ids:
        if d not in seen:
            seen.add(d)
            deduped.append(d)

    return deduped


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


def is_container() -> bool:
    """
    Detect if we are actually inside a container.

    IMPORTANT:
    - This is intentionally NOT the same as "no parallel requested".
    - Used to decide whether to run cargo prefetch/build before dx bundle.
    """
    if Path("/.dockerenv").exists():
        return True

    try:
        cgroup = Path("/proc/1/cgroup")
        if cgroup.exists():
            txt = cgroup.read_text(errors="ignore").lower()
            if "docker" in txt or "containerd" in txt or "kubepods" in txt:
                return True
    except Exception:
        pass

    return False


def no_parallel_requested() -> bool:
    """
    User override: force sequential build even on host.

    This should NOT imply "we are in docker".
    """
    return os.environ.get("GROUNDSTATION_NO_PARALLEL", "").strip().lower() in {
        "1",
        "true",
        "yes",
        "on",
    }


def in_docker_build() -> bool:
    """
    Best-effort detection that we're running inside a Docker build/container
    OR the user forced single-thread mode.

    Existing behavior preserved (sequential build when forced).
    """
    if no_parallel_requested():
        return True
    return is_container()


def get_compose_base_cmd() -> list[str]:
    """
    Return the base command for docker compose, preferring `docker compose`
    but falling back to `docker-compose` if needed.
    Exits with an error if neither is available.
    """
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


def build_docker(repo_root: Path, pi_build: bool, testing: bool) -> None:
    """
    Build using docker compose. If pi_build is True, pass PI_BUILD as a build-arg.
    """
    compose_cmd = get_compose_base_cmd()
    cmd: list[str] = [*compose_cmd, "build"]

    if pi_build:
        print("Pi build (docker) → passing --build-arg PI_BUILD=TRUE")
        cmd.extend(["--build-arg", "PI_BUILD=TRUE"])
    if testing:
        print("Testing mode (docker) → passing --build-arg TESTING=TRUE")
        cmd.extend(["--build-arg", "TESTING=TRUE"])

    run(cmd, cwd=repo_root)


def patch_plist(frontend_dir: Path) -> None:
    """
    Run frontend/scripts/patch_plist.sh (used to patch macOS/iOS Info.plist, etc.)
    """
    script = frontend_dir / "scripts" / "patch_plist.sh"
    run_script(script, cwd=frontend_dir)


def dist_dir(frontend_dir: Path) -> Path:
    return frontend_dir / DIST_DIRNAME


def app_bundle_path(frontend_dir: Path) -> Path:
    """
    Return the built app bundle path, preferring the new bundle name but falling
    back to legacy bundle name if needed.
    """
    dist = dist_dir(frontend_dir)
    preferred = dist / APP_BUNDLE_NAME
    legacy = dist / LEGACY_APP_BUNDLE_NAME
    if preferred.exists():
        return preferred
    if legacy.exists():
        return legacy
    return preferred


def clear_app_bundle(frontend_dir: Path) -> None:
    """
    Clear out the dist/*.app bundle before building so old artifacts don't linger.
    """
    dist = dist_dir(frontend_dir)
    bundles = [dist / APP_BUNDLE_NAME, dist / LEGACY_APP_BUNDLE_NAME]
    for bundle in bundles:
        if bundle.exists():
            print(f"Removing existing app bundle: {bundle}")
            shutil.rmtree(bundle)


def _prebuild_frontend_for_container(frontend_dir: Path) -> None:
    """
    When running inside a container (Docker build/container), dx bundle can stall
    at `cargo metadata` if the network/index isn't ready.
    Prime cargo first (network allowed) then run dx bundle.

    NOTE: This is ONLY for real containers, NOT for host single-thread mode.
    """
    print("Container detected → priming cargo for frontend before dx bundle")
    # fetch is a cheap way to force index/network setup
    run(["cargo", "fetch"], cwd=frontend_dir)
    # build the frontend crate so metadata/deps are definitely warm
    run(["cargo", "build", "--release", "-p", "groundstation_frontend"], cwd=frontend_dir)


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

    New behavior:
    - If inside a REAL container, run cargo fetch + cargo build(frontend) first, then dx bundle.
    - Host builds (even sequential forced via GROUNDSTATION_NO_PARALLEL) are unchanged.
    """
    try:
        clear_app_bundle(frontend_dir)

        if is_container():
            _prebuild_frontend_for_container(frontend_dir)

        cmd = ["dx", "bundle", "--release"]

        if platform_name:
            cmd.extend(["--platform", platform_name])
        else:
            cmd.extend(["--platform", "web"])

        if rust_target:
            cmd.extend(["--target", rust_target])

        run(cmd, cwd=frontend_dir)

        # Patch plist for iOS bundles (device or sim)
        if platform_name == "ios":
            patch_plist(frontend_dir)

    except subprocess.CalledProcessError as e:
        print("Frontend build failed.", file=sys.stderr)
        sys.exit(e.returncode)


def deploy_ios(frontend_dir: Path) -> None:
    """
    Deploy an already-built iOS .app to ALL connected devices using ios-deploy.
    """
    bundle = app_bundle_path(frontend_dir)
    if not bundle.exists():
        print(f"Error: iOS app bundle not found at: {bundle}", file=sys.stderr)
        print("Build it first with: ./build.py ios (or ./build.py ios_deploy)", file=sys.stderr)
        sys.exit(1)

    device_ids = _list_connected_ios_device_ids(frontend_dir)

    # If we couldn't detect devices (or ios-deploy output format changed),
    # fall back to the old behavior (ios-deploy picks a device, waiting if needed).
    if not device_ids:
        print(
            "No device IDs detected via `ios-deploy --detect`; falling back to single-device deploy.",
            file=sys.stderr,
        )
        _deploy_ios_single(frontend_dir, bundle)
        return

    print(f"Deploying to {len(device_ids)} connected iOS device(s): {', '.join(device_ids)}")

    failures: list[tuple[str, int]] = []
    for udid in device_ids:
        print(f"\n=== Deploying to device {udid} ===")
        try:
            run(["ios-deploy", "--id", udid, "--bundle", str(bundle)], cwd=frontend_dir)
        except subprocess.CalledProcessError as e:
            failures.append((udid, e.returncode))

    if failures:
        print("\nOne or more device deploys failed:", file=sys.stderr)
        for udid, code in failures:
            print(f"  - {udid}: exit code {code}", file=sys.stderr)
        sys.exit(1)


def _deploy_ios_single(frontend_dir: Path, bundle: Path) -> None:
    """
    Single-device deploy that waits for a device to be connected.
    """
    try:
        run(["ios-deploy", "--bundle", str(bundle)], cwd=frontend_dir)
    except subprocess.CalledProcessError:
        print(
            "Warning: `ios-deploy --wait-for-device` failed; retrying without the flag.",
            file=sys.stderr,
        )
        run(["ios-deploy", "--bundle", str(bundle)], cwd=frontend_dir)


def _read_bundle_identifier(app_bundle: Path) -> Optional[str]:
    plist_path = app_bundle / "Info.plist"
    try:
        with plist_path.open("rb") as f:
            info = plistlib.load(f)
        bid = info.get("CFBundleIdentifier")
        if isinstance(bid, str) and bid.strip():
            return bid.strip()
    except FileNotFoundError:
        return None
    except Exception:
        return None
    return None


def _open_simulator_app(frontend_dir: Path) -> None:
    if platform.system() != "Darwin":
        return
    try:
        run(["open", "-a", "Simulator"], cwd=frontend_dir)
    except Exception:
        pass


def _pick_or_boot_simulator_udid(frontend_dir: Path) -> str:
    """
    Returns a UDID of a booted simulator. If none are booted, best-effort boots
    the first available iPhone simulator device.
    """
    if platform.system() != "Darwin":
        print("Error: iOS simulator install requires macOS (xcrun).", file=sys.stderr)
        sys.exit(1)

    # Prefer an already-booted device
    try:
        out = run_capture(["xcrun", "simctl", "list", "devices", "booted"], cwd=frontend_dir)
        for line in out.splitlines():
            line = line.strip()
            if "(Booted)" in line and "(" in line and ")" in line:
                parts = line.split("(")
                for p in parts:
                    cand = p.split(")")[0].strip()
                    if "-" in cand and len(cand) >= 20:
                        return cand
    except subprocess.CalledProcessError:
        pass

    # None booted: boot first available iPhone
    try:
        out = run_capture(["xcrun", "simctl", "list", "devices"], cwd=frontend_dir)
    except subprocess.CalledProcessError as e:
        print("Error: failed to list simulators via xcrun simctl.", file=sys.stderr)
        sys.exit(e.returncode)

    chosen: Optional[str] = None
    for line in out.splitlines():
        t = line.strip()
        if t.startswith("iPhone ") and "(Shutdown)" in t and "(" in t and ")" in t:
            parts = t.split("(")
            for p in parts:
                cand = p.split(")")[0].strip()
                if "-" in cand and len(cand) >= 20:
                    chosen = cand
                    break
        if chosen:
            break

    if not chosen:
        print(
            "Error: no booted simulator found and couldn't find a Shutdown iPhone simulator to boot.\n"
            "Open Simulator.app and create/boot a device, then re-run.",
            file=sys.stderr,
        )
        sys.exit(1)

    _open_simulator_app(frontend_dir)
    try:
        run(["xcrun", "simctl", "boot", chosen], cwd=frontend_dir)
    except subprocess.CalledProcessError:
        pass

    return chosen


def deploy_ios_sim(frontend_dir: Path) -> None:
    """
    Install the built iOS simulator .app into the current simulator (booted),
    and auto-launch it.
    """
    bundle = app_bundle_path(frontend_dir)
    if not bundle.exists():
        print(f"Error: iOS sim app bundle not found at: {bundle}", file=sys.stderr)
        print("Build it first with: ./build.py ios_sim (or ./build.py ios_sim_install)", file=sys.stderr)
        sys.exit(1)

    udid = _pick_or_boot_simulator_udid(frontend_dir)

    try:
        run(["xcrun", "simctl", "install", udid, str(bundle)], cwd=frontend_dir)
    except subprocess.CalledProcessError as e:
        print("Error: failed to install app into simulator.", file=sys.stderr)
        sys.exit(e.returncode)

    bundle_id = _read_bundle_identifier(bundle)
    if not bundle_id:
        print(
            "Installed into simulator, but could not read CFBundleIdentifier to auto-launch.\n"
            f"Tip: ensure {bundle / 'Info.plist'} has CFBundleIdentifier, or launch manually in Simulator.",
            file=sys.stderr,
        )
        return

    try:
        run(["xcrun", "simctl", "launch", udid, bundle_id], cwd=frontend_dir)
    except subprocess.CalledProcessError as e:
        print(
            "Installed into simulator, but auto-launch failed.\n"
            f"Try launching manually, or run: xcrun simctl launch {udid} {bundle_id}",
            file=sys.stderr,
        )
        sys.exit(e.returncode)


def deploy_macos(frontend_dir: Path) -> None:
    """
    Copy the built macOS .app bundle into the user's Applications folder (~/Applications).
    """
    src = app_bundle_path(frontend_dir)
    if not src.exists():
        print(f"Error: macOS app bundle not found at: {src}", file=sys.stderr)
        print("Build it first with: ./build.py macos (or ./build.py macos_deploy)", file=sys.stderr)
        sys.exit(1)

    user_apps = Path.home() / "Applications"
    user_apps.mkdir(parents=True, exist_ok=True)

    dst = user_apps / APP_BUNDLE_NAME
    if dst.exists():
        print(f"Removing existing installed app: {dst}")
        shutil.rmtree(dst)

    print(f"Copying app bundle to: {dst}")
    shutil.copytree(src, dst)


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
    print("")
    print("Frontend deploy actions:")
    print("  ./build.py ios_deploy              # build ios + deploy to device via ios-deploy")
    print("  ./build.py ios_sim_install         # build ios_sim + install into Simulator + auto-launch")
    print("  ./build.py macos_deploy            # build macos + copy .app to ~/Applications")
    print("")
    print("Environment overrides:")
    print("  GROUNDSTATION_NO_PARALLEL=1         # force sequential builds (useful in Docker)")
    sys.exit(1)


def main() -> None:
    force_pi = False
    force_no_pi = False
    docker_mode = False
    testing_mode = False

    frontend_only_platform: Optional[str] = None
    frontend_rust_target: Optional[str] = None
    frontend_deploy_action: Optional[str] = None  # "ios" | "macos" | "ios_sim_install"

    args = [a.strip().lower() for a in sys.argv[1:]]

    if len(args) > 4:
        print("Error: Too many arguments.", file=sys.stderr)
        print_usage()

    frontend_platform_map = {
        "ios": ("ios", "aarch64-apple-ios"),
        "ios_sim": ("ios", "aarch64-apple-ios-sim"),
        "macos": ("macos", None),
        "windows": ("windows", None),
        "android": ("android", None),
        "linux": ("linux", None),
    }

    deploy_map = {
        "ios_deploy": "ios",
        "ios_sim_install": "ios_sim_install",
        "macos_deploy": "macos",
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
        elif arg in deploy_map:
            if frontend_deploy_action is not None or frontend_only_platform is not None:
                print("Error: Only one frontend action (build OR deploy) may be specified.", file=sys.stderr)
                print_usage()
            frontend_deploy_action = deploy_map[arg]
        elif arg in frontend_platform_map:
            if frontend_only_platform is not None or frontend_deploy_action is not None:
                print("Error: Only one frontend action (build OR deploy) may be specified.", file=sys.stderr)
                print_usage()
            frontend_only_platform, frontend_rust_target = frontend_platform_map[arg]
        else:
            print(f"Error: Invalid argument '{arg}'.", file=sys.stderr)
            print_usage()

    if force_pi and force_no_pi:
        print("Error: Cannot specify both 'pi_build' and 'no_pi'.", file=sys.stderr)
        sys.exit(1)

    repo_root = Path(__file__).resolve().parent
    frontend_dir = repo_root / "frontend"
    backend_dir = repo_root / "backend"

    # Frontend deploy mode (build + deploy)
    if frontend_deploy_action is not None:
        if docker_mode or force_pi or force_no_pi or testing_mode:
            print(
                "Error: Frontend deploy actions cannot be combined with docker/pi_build/no_pi/testing.",
                file=sys.stderr,
            )
            print_usage()

        if frontend_deploy_action == "ios":
            build_frontend(frontend_dir, platform_name="ios", rust_target="aarch64-apple-ios")
            deploy_ios(frontend_dir)
            return

        if frontend_deploy_action == "ios_sim_install":
            build_frontend(frontend_dir, platform_name="ios", rust_target="aarch64-apple-ios-sim")
            deploy_ios_sim(frontend_dir)
            return

        if frontend_deploy_action == "macos":
            build_frontend(frontend_dir, platform_name="macos", rust_target=None)
            deploy_macos(frontend_dir)
            return

        print("Error: Unknown deploy action.", file=sys.stderr)
        sys.exit(1)

    # Frontend-only build mode
    if frontend_only_platform is not None:
        if docker_mode or force_pi or force_no_pi or testing_mode:
            print(
                "Error: Frontend-only builds cannot be combined with docker/pi_build/no_pi/testing.",
                file=sys.stderr,
            )
            print_usage()
        build_frontend(frontend_dir, platform_name=frontend_only_platform, rust_target=frontend_rust_target)
        return

    # Docker mode (compose build) - already single-threaded here
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

        build_docker(repo_root=repo_root, pi_build=pi_build_flag, testing=testing_mode)
        return

    # Normal local build mode:
    # - parallel on host
    # - sequential when running inside docker build/container (avoids cargo/dx contention)
    if in_docker_build():
        print("Sequential build")
        build_frontend(frontend_dir, None)
        build_backend(backend_dir, force_pi, force_no_pi, testing_mode)
        return

    # Parallel host build
    bfe = mp.Process(target=build_frontend, args=(frontend_dir, None))
    bbe = mp.Process(target=build_backend, args=(backend_dir, force_pi, force_no_pi, testing_mode))

    bfe.start()
    bbe.start()
    bfe.join()
    bbe.join()


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\n\nexiting...")
        exit(0)
