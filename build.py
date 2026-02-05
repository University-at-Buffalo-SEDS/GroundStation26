#!/usr/bin/env python3
import multiprocessing as mp
import os
import platform
import re
import shutil
import subprocess
import sys
try:
    import tomllib  # py3.11+
except ImportError:  # pragma: no cover - fallback for older pythons
    tomllib = None
from pathlib import Path
from subprocess import DEVNULL
from typing import Optional, Literal

APP_NAME = "GroundStation 26"
LEGACY_APP_NAME = "GroundstationFrontend"
DIST_DIRNAME = "dist"
APP_BUNDLE_NAME = f"{APP_NAME}.app"
LEGACY_APP_BUNDLE_NAME = f"{LEGACY_APP_NAME}.app"

# NEW: fixed provisioning profile path (repo-local)
FIXED_MOBILEPROVISION_REL = Path("Groundstation_26.mobileprovision")


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
    pat = re.compile(r"\bFound\s+([0-9A-Fa-f-]+)\s+\(([^)]*)\)(.*)$")

    for line in out.splitlines():
        m = pat.search(line)
        if not m:
            continue

        udid = m.group(1).strip()
        meta = m.group(2).lower()
        tail = m.group(3).lower()

        if "watch" in meta or "watch" in tail or "companion" in tail:
            continue

        if "iphoneos" in meta or "ipados" in meta:
            ids.append(udid)
            continue

        if "unknownos" in meta or "uknownos" in meta:
            if "a.k.a." in tail and ("ipad" in tail or "iphone" in tail):
                ids.append(udid)
                continue
            if "connected through usb" in tail and "a.k.a." in tail:
                ids.append(udid)
                continue

    seen = set()
    deduped: list[str] = []
    for d in ids:
        if d not in seen:
            seen.add(d)
            deduped.append(d)

    return deduped


def is_raspberry_pi() -> bool:
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
    return os.environ.get("GROUNDSTATION_NO_PARALLEL", "").strip().lower() in {
        "1",
        "true",
        "yes",
        "on",
    }


def in_docker_build() -> bool:
    if no_parallel_requested():
        return True
    return is_container()


def get_compose_base_cmd() -> list[str]:
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
    script = frontend_dir / "scripts" / "patch_plist.sh"
    version = _read_frontend_version(frontend_dir)
    run_script(script, cwd=frontend_dir, env={"APP_VERSION": version})


def _read_frontend_version(frontend_dir: Path) -> str:
    cargo_toml = frontend_dir / "Cargo.toml"
    raw = cargo_toml.read_text(encoding="utf-8")

    if tomllib is not None:
        data = tomllib.loads(raw)
        version = data.get("package", {}).get("version")
        if version:
            return str(version)

    in_package = False
    for line in raw.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_package = stripped == "[package]"
            continue
        if in_package:
            m = re.match(r'version\s*=\s*"([^"]+)"\s*$', stripped)
            if m:
                return m.group(1)

    raise ValueError(f"Failed to read frontend version from: {cargo_toml}")


def dist_dir(frontend_dir: Path) -> Path:
    return frontend_dir / DIST_DIRNAME


def app_bundle_path(frontend_dir: Path) -> Path:
    dist = dist_dir(frontend_dir)
    preferred = dist / APP_BUNDLE_NAME
    legacy = dist / LEGACY_APP_BUNDLE_NAME
    if preferred.exists():
        return preferred
    if legacy.exists():
        return legacy
    return preferred


def clear_app_bundle(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    bundles = [dist / APP_BUNDLE_NAME, dist / LEGACY_APP_BUNDLE_NAME]
    for bundle in bundles:
        if bundle.exists():
            print(f"Removing existing app bundle: {bundle}")
            shutil.rmtree(bundle)


def _prebuild_frontend_for_container(frontend_dir: Path) -> None:
    print("Container detected → priming cargo for frontend before dx bundle")
    run(["cargo", "fetch"], cwd=frontend_dir)
    run(["cargo", "build", "--release", "-p", "groundstation_frontend"], cwd=frontend_dir)


# -----------------------------
# Signing / packaging helpers
# -----------------------------
SignKind = Literal["development", "distribution"]


def fixed_mobileprovision_path(frontend_dir: Path) -> Path:
    p = frontend_dir / FIXED_MOBILEPROVISION_REL
    if not p.exists():
        raise FileNotFoundError(
            f"Missing provisioning profile: {p}\n"
            f"Expected at: frontend/{FIXED_MOBILEPROVISION_REL}"
        )
    return p


def package_ios_ipa_with_script(frontend_dir: Path, *, sign_kind: SignKind) -> Path:
    """
    Uses frontend/scripts/ios_package_sign.sh to:
      - embed provisioning profile from frontend/Groundstation_26.mobileprovision
      - pick signing cert by regex (no PII in repo)
      - sign + package an IPA
    Returns IPA path.
    """
    if platform.system() != "Darwin":
        print("Error: iOS packaging/signing requires macOS.", file=sys.stderr)
        sys.exit(1)

    app = app_bundle_path(frontend_dir)
    if not app.exists():
        raise FileNotFoundError(f"App bundle not found: {app}")

    # Ensure Info.plist is patched with the current version before signing.
    patch_plist(frontend_dir)

    profile = fixed_mobileprovision_path(frontend_dir)

    signer = frontend_dir / "scripts" / "ios_package_sign.sh"
    if not signer.exists():
        raise FileNotFoundError(f"Missing signer script: {signer}")

    # IMPORTANT: make output path relative to frontend_dir (since cwd=frontend_dir)
    ipas_dir = frontend_dir / "dist" / "ipas"
    ipas_dir.mkdir(parents=True, exist_ok=True)

    ipa_name = "GroundStation26.ipa"
    ipa_out = ipas_dir / ipa_name

    try:
        ipa_out.unlink()
    except FileNotFoundError:
        pass

    cert_regex = os.environ.get(
        "CERT_REGEX",
        (r"^Apple Development:" if sign_kind == "development" else r"^Apple Distribution:"),
    )
    cert_pick = os.environ.get("CERT_PICK", "newest")

    env = {
        "CERT_REGEX": cert_regex,
        "CERT_PICK": cert_pick,
    }

    # Use absolute paths so the script can't get confused by cwd.
    run(
        ["bash", str(signer), str(app.resolve()), str(profile.resolve()), str(ipa_out.resolve())],
        cwd=frontend_dir,
        env=env,
    )

    if not ipa_out.exists() or ipa_out.stat().st_size == 0:
        raise RuntimeError(f"IPA not created or empty: {ipa_out}")

    return ipa_out


def build_frontend(
        frontend_dir: Path,
        platform_name: Optional[str] = None,
        *,
        rust_target: Optional[str] = None,
) -> None:
    try:
        clear_app_bundle(frontend_dir)

        if is_container():
            _prebuild_frontend_for_container(frontend_dir)

        cmd = ["dx", "bundle", "--release"]

        if platform_name:
            cmd.extend(["--platform", platform_name])
            if platform_name == "ios":
                cmd.extend(["--device", "true"])
        else:
            cmd.extend(["--platform", "web"])

        if rust_target:
            cmd.extend(["--target", rust_target])

        run(cmd, cwd=frontend_dir)

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
    print("Frontend-only builds:")
    print("  ./build.py ios                     # iPhoneOS build (UNSIGNED; patched)")
    print("  ./build.py ios_sim                 # iOS simulator build (patched)")
    print("  ./build.py macos")
    print("  ./build.py windows")
    print("  ./build.py android")
    print("  ./build.py linux")
    print("")
    print("Frontend actions:")
    print("  ./build.py ios_deploy              # build ios + patch + package+sign (Dev) -> IPA")
    print("  ./build.py ios_sign                # package+sign (Dev) existing dist app -> IPA")
    print("  ./build.py ios_dist_sign           # package+sign (Distribution) existing dist app -> IPA")
    print("")
    print("Provisioning profile path (fixed):")
    print(f"  frontend/{FIXED_MOBILEPROVISION_REL}")
    print("")
    print("Env (optional):")
    print("  CERT_REGEX=...                     # override cert regex for signer script")
    print("  CERT_PICK=newest|first             # override cert selection for signer script")
    print("  GROUNDSTATION_NO_PARALLEL=1        # force sequential build")
    sys.exit(1)


def main() -> None:
    force_pi = False
    force_no_pi = False
    docker_mode = False
    testing_mode = False

    frontend_only_platform: Optional[str] = None
    frontend_rust_target: Optional[str] = None
    action: Optional[str] = None  # ios_deploy | ios_sign | ios_dist_sign

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

    actions = {"ios_deploy", "ios_sign", "ios_dist_sign"}

    for arg in args:
        if arg == "pi_build":
            force_pi = True
        elif arg == "no_pi":
            force_no_pi = True
        elif arg == "docker":
            docker_mode = True
        elif arg == "testing":
            testing_mode = True
        elif arg in actions:
            if action or frontend_only_platform:
                print("Error: Only one frontend action/build may be specified.", file=sys.stderr)
                print_usage()
            action = arg
        elif arg in frontend_platform_map:
            if frontend_only_platform or action:
                print("Error: Only one frontend action/build may be specified.", file=sys.stderr)
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

    # Frontend actions
    if action:
        if docker_mode or force_pi or force_no_pi or testing_mode:
            print("Error: Frontend actions cannot be combined with docker/pi_build/no_pi/testing.", file=sys.stderr)
            print_usage()

        if action == "ios_deploy":
            # NOTE: per your latest direction, this is now just "package and sign" (no local deploy)
            build_frontend(frontend_dir, platform_name="ios", rust_target="aarch64-apple-ios")
            ipa = package_ios_ipa_with_script(frontend_dir, sign_kind="distribution")
            print(f"✅ Dev IPA created: {ipa}")
            return

        if action == "ios_sign":
            ipa = package_ios_ipa_with_script(frontend_dir, sign_kind="development")
            print(f"✅ Dev IPA created: {ipa}")
            return

        if action == "ios_dist_sign":
            ipa = package_ios_ipa_with_script(frontend_dir, sign_kind="distribution")
            print(f"✅ Distribution IPA created: {ipa}")
            return

        print("Error: unknown action", file=sys.stderr)
        sys.exit(1)

    # Frontend-only build mode
    if frontend_only_platform is not None:
        if docker_mode or force_pi or force_no_pi or testing_mode:
            print("Error: Frontend-only builds cannot be combined with docker/pi_build/no_pi/testing.", file=sys.stderr)
            print_usage()
        build_frontend(frontend_dir, platform_name=frontend_only_platform, rust_target=frontend_rust_target)
        return

    # Docker mode
    if docker_mode:
        if force_no_pi:
            pi_build_flag = False
        else:
            if not force_pi and is_raspberry_pi():
                force_pi = True
            pi_build_flag = force_pi
        build_docker(repo_root=repo_root, pi_build=pi_build_flag, testing=testing_mode)
        return

    # Normal local build mode:
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
        sys.exit(0)
