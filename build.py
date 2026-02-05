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
from typing import Optional, Literal

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
    run_script(script, cwd=frontend_dir)


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
# Signing helpers (Python owns)
# -----------------------------
SignKind = Literal["development", "distribution"]


def _stat_mtime_epoch(p: Path) -> int:
    try:
        return int(p.stat().st_mtime)
    except Exception:
        return 0


def pick_newest_mobileprovision(frontend_dir: Path) -> Path:
    static_dir = frontend_dir / "static"
    profiles = sorted(static_dir.glob("*.mobileprovision"))
    if not profiles:
        raise FileNotFoundError(f"No provisioning profiles found in: {static_dir} (*.mobileprovision)")
    profiles.sort(key=_stat_mtime_epoch, reverse=True)
    return profiles[0]


def _parse_identity_lines(sign_kind: SignKind, output: str) -> list[tuple[str, str]]:
    """
    Return [(sha1, name), ...] filtered to Apple Development / Apple Distribution.
    """
    if sign_kind == "development":
        want_prefix = "Apple Development:"
    else:
        want_prefix = "Apple Distribution:"

    # Example line:
    #  1) 0123ABCD... "Apple Development: you@example.com (TEAMID)"
    pat = re.compile(r'^\s*\d+\)\s*([0-9A-Fa-f]{40})\s+"([^"]+)"\s*$')
    out: list[tuple[str, str]] = []
    for line in output.splitlines():
        m = pat.match(line)
        if not m:
            continue
        sha1 = m.group(1)
        name = m.group(2)
        if name.startswith(want_prefix):
            out.append((sha1, name))
    return out


def pick_codesign_identity_sha1(sign_kind: SignKind, *, team_id: str = "") -> str:
    """
    Pick an unambiguous signing identity by SHA-1.
    We choose the identity with the latest notAfter date.
    """
    if platform.system() != "Darwin":
        raise RuntimeError("Codesigning requires macOS.")

    try:
        raw = run_capture(["security", "find-identity", "-v", "-p", "codesigning"], cwd=Path("."))
    except subprocess.CalledProcessError as e:
        raise RuntimeError("Failed to run security find-identity") from e

    candidates = _parse_identity_lines(sign_kind, raw)
    if team_id:
        candidates = [(sha, nm) for (sha, nm) in candidates if f"({team_id})" in nm]

    if not candidates:
        raise RuntimeError(f"No codesigning identities found for {sign_kind!r} (team filter={team_id!r}).")

    best_sha = ""
    best_epoch = -1

    for sha1, name in candidates:
        # Extract PEM for this cert, then read end date
        try:
            pem = run_capture(["security", "find-certificate", "-a", "-Z", "-p", "-c", name], cwd=Path("."))
        except subprocess.CalledProcessError:
            continue

        # find-certificate may output multiple certs; pick the PEM block that matches SHA-1
        # We'll do a simple scan: locate the SHA-1 header then take the next PEM block.
        lines = pem.splitlines()
        want = False
        in_pem = False
        pem_block: list[str] = []
        for ln in lines:
            if ln.startswith("SHA-1 hash: "):
                want = sha1.lower() in ln.lower()
                in_pem = False
                pem_block = []
                continue
            if want and "-----BEGIN CERTIFICATE-----" in ln:
                in_pem = True
            if want and in_pem:
                pem_block.append(ln)
            if want and in_pem and "-----END CERTIFICATE-----" in ln:
                break

        if not pem_block:
            continue

        # notAfter parsing via openssl
        try:
            end = run_capture(
                ["openssl", "x509", "-noout", "-enddate"],
                cwd=Path("."),
            )
        except Exception:
            # Fallback: write pem to temp and read
            import tempfile

            with tempfile.NamedTemporaryFile("w", delete=False) as f:
                f.write("\n".join(pem_block) + "\n")
                tmp = f.name
            try:
                end = subprocess.check_output(["openssl", "x509", "-noout", "-enddate", "-in", tmp]).decode()
            finally:
                try:
                    os.unlink(tmp)
                except Exception:
                    pass

        # Normalize: notAfter=...
        end = end.strip()
        if end.startswith("notAfter="):
            end = end[len("notAfter=") :].strip()

        # Parse to epoch using python datetime (handles double-space day)
        from datetime import datetime, timezone

        fmts = ["%b %d %H:%M:%S %Y %Z", "%b  %d %H:%M:%S %Y %Z", "%b %e %H:%M:%S %Y %Z"]
        epoch = None
        for fmt in fmts:
            try:
                dt = datetime.strptime(end, fmt)
                if dt.tzinfo is None:
                    dt = dt.replace(tzinfo=timezone.utc)
                epoch = int(dt.timestamp())
                break
            except Exception:
                pass
        if epoch is None:
            continue

        if epoch > best_epoch:
            best_epoch = epoch
            best_sha = sha1

    if not best_sha:
        # fallback: first candidate
        best_sha = candidates[0][0]

    return best_sha


def extract_entitlements_from_profile(profile: Path, out_plist: Path) -> None:
    """
    Decode .mobileprovision -> plist, then extract Entitlements -> xml plist.
    """
    import tempfile

    with tempfile.NamedTemporaryFile("w", delete=False) as f:
        profile_plist = Path(f.name)

    try:
        # Decode CMS
        subprocess.run(
            ["security", "cms", "-D", "-i", str(profile)],
            check=True,
            stdout=profile_plist.open("w"),
            stderr=DEVNULL,
        )
        # Extract Entitlements
        subprocess.run(
            ["/usr/bin/plutil", "-extract", "Entitlements", "xml1", "-o", str(out_plist), str(profile_plist)],
            check=True,
            stdout=DEVNULL,
            stderr=DEVNULL,
        )
        if not out_plist.exists() or out_plist.stat().st_size == 0:
            raise RuntimeError(f"Entitlements plist is empty: {out_plist}")
    finally:
        try:
            profile_plist.unlink()
        except Exception:
            pass


def sign_ios_app(frontend_dir: Path, *, sign_kind: SignKind) -> None:
    """
    Codesign dist/*.app using newest profile in frontend/static and matching identity.
    """
    if platform.system() != "Darwin":
        print("Error: iOS signing requires macOS.", file=sys.stderr)
        sys.exit(1)

    app = app_bundle_path(frontend_dir)
    if not app.exists():
        raise FileNotFoundError(f"App bundle not found: {app}")

    team_id = os.environ.get("GS26_TEAM_ID", "").strip()
    profile = pick_newest_mobileprovision(frontend_dir)

    # Always embed the selected profile when we sign (needed for device install / ad-hoc style)
    embedded = app / "embedded.mobileprovision"
    print(f"Embedding profile: {profile} -> {embedded}")
    shutil.copyfile(profile, embedded)

    # Extract entitlements
    entitlements = Path("/tmp/gs26-entitlements.plist")
    extract_entitlements_from_profile(profile, entitlements)

    # Choose identity
    sha1 = pick_codesign_identity_sha1(sign_kind, team_id=team_id)
    print(f"Using codesign identity ({sign_kind}): {sha1}")

    # Remove old signature
    codesig = app / "_CodeSignature"
    if codesig.exists():
        shutil.rmtree(codesig)

    # Sign
    subprocess.run(
        [
            "codesign",
            "--force",
            "--deep",
            "--timestamp=none",
            "--sign",
            sha1,
            "--entitlements",
            str(entitlements),
            str(app),
        ],
        check=True,
    )

    # Verify
    subprocess.run(["codesign", "--verify", "--deep", "--strict", "--verbose=4", str(app)], check=True)
    print("✅ Signed successfully")


def build_frontend(frontend_dir: Path, platform_name: Optional[str] = None, *, rust_target: Optional[str] = None) -> None:
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


def deploy_ios(frontend_dir: Path) -> None:
    bundle = app_bundle_path(frontend_dir)
    if not bundle.exists():
        print(f"Error: iOS app bundle not found at: {bundle}", file=sys.stderr)
        sys.exit(1)

    device_ids = _list_connected_ios_device_ids(frontend_dir)

    if not device_ids:
        print("No device IDs detected via `ios-deploy --detect`; falling back to single-device deploy.", file=sys.stderr)
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
    try:
        run(["ios-deploy", "--bundle", str(bundle)], cwd=frontend_dir)
    except subprocess.CalledProcessError:
        run(["ios-deploy", "--bundle", str(bundle)], cwd=frontend_dir)


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
    print("  ./build.py ios_deploy              # build ios + patch + SIGN (Dev) + deploy to device")
    print("  ./build.py ios_sign                # SIGN (Dev) the existing dist app (no deploy)")
    print("  ./build.py ios_dist_sign           # SIGN (Distribution) existing dist app (no deploy)")
    print("")
    print("Env:")
    print("  GS26_TEAM_ID=TEAMID                # optional filter when picking identity")
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
            build_frontend(frontend_dir, platform_name="ios", rust_target="aarch64-apple-ios")
            sign_ios_app(frontend_dir, sign_kind="development")
            deploy_ios(frontend_dir)
            return

        if action == "ios_sign":
            sign_ios_app(frontend_dir, sign_kind="development")
            return

        if action == "ios_dist_sign":
            sign_ios_app(frontend_dir, sign_kind="distribution")
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
