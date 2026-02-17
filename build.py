#!/usr/bin/env python3
import gzip
import json
import multiprocessing as mp
import os
import platform
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile

try:
    import tomllib  # py3.11+
except ImportError:  # pragma: no cover
    tomllib = None

from pathlib import Path
from subprocess import DEVNULL
from typing import Optional, Literal

APP_NAME = "UBSEDS GS"
LEGACY_APP_NAME = "GroundstationFrontend"
DIST_DIRNAME = "dist"
APP_BUNDLE_NAME = f"{APP_NAME}.app"
LEGACY_APP_BUNDLE_NAME = f"{LEGACY_APP_NAME}.app"

# fixed provisioning profile path (repo-local)
FIXED_MOBILEPROVISION_REL = Path("Groundstation_26.mobileprovision")

LOG_FILE: Optional[Path] = None


def _append_log(line: str) -> None:
    if LOG_FILE is None:
        return
    with LOG_FILE.open("a", encoding="utf-8") as f:
        f.write(line)


def run(cmd: list[str], cwd: Path, env: Optional[dict[str, str]] = None) -> None:
    cmd = [str(part) for part in cmd]
    cmd_line = f"Running: {' '.join(cmd)} (cwd={cwd})"
    print(cmd_line)
    _append_log(cmd_line + "\n")
    merged = os.environ.copy()
    if env:
        merged.update(env)
    if LOG_FILE is None:
        subprocess.run(cmd, cwd=cwd, check=True, env=merged)
        return

    proc = subprocess.Popen(
        cmd,
        cwd=cwd,
        env=merged,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )
    assert proc.stdout is not None
    for line in proc.stdout:
        print(line, end="")
        _append_log(line)
    rc = proc.wait()
    if rc != 0:
        raise subprocess.CalledProcessError(rc, cmd)


def run_capture(cmd: list[str], cwd: Path, env: Optional[dict[str, str]] = None) -> str:
    cmd = [str(part) for part in cmd]
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    out = subprocess.check_output(cmd, cwd=cwd, env=(os.environ | (env or {})))
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


def build_docker(repo_root: Path, pi_build: bool, testing: bool, plain_progress: bool) -> None:
    compose_cmd = get_compose_base_cmd()
    cmd: list[str] = [*compose_cmd, "build"]
    if plain_progress and compose_cmd == ["docker", "compose"]:
        cmd.extend(["--progress", "plain"])

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
    build = _read_dioxus_build(frontend_dir)
    run_script(
        script,
        cwd=frontend_dir,
        env={
            "APP_VERSION": version,
            "APP_BUILD": build,
        },
    )


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


def _read_dioxus_build(frontend_dir: Path) -> str:
    dioxus_toml = frontend_dir / "Dioxus.toml"
    raw = dioxus_toml.read_text(encoding="utf-8")
    in_application = False
    for line in raw.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_application = stripped == "[application]"
            continue
        if in_application:
            m = re.match(r'build\s*=\s*"([^"]+)"\s*$', stripped)
            if m:
                return m.group(1)
    raise ValueError(f"Failed to read [application].build from: {dioxus_toml}")


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


def rename_macos_app_bundle(frontend_dir: Path) -> Optional[Path]:
    dist = dist_dir(frontend_dir)
    preferred = dist / APP_BUNDLE_NAME
    legacy = dist / LEGACY_APP_BUNDLE_NAME

    if preferred.exists():
        return preferred
    if legacy.exists():
        print(f"Renaming macOS app bundle: {legacy.name} -> {preferred.name}")
        if preferred.exists():
            shutil.rmtree(preferred)
        legacy.rename(preferred)
        return preferred
    return None


def remove_legacy_app_bundle(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    preferred = dist / APP_BUNDLE_NAME
    legacy = dist / LEGACY_APP_BUNDLE_NAME
    if preferred.exists() and legacy.exists():
        print(f"Removing legacy macOS app bundle: {legacy}")
        shutil.rmtree(legacy)


def remove_legacy_dmgs(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    for dmg in sorted(dist.glob(f"{LEGACY_APP_NAME}*.dmg")):
        print(f"Removing legacy dmg: {dmg}")
        dmg.unlink()


def _remove_path(path: Path) -> None:
    if not path.exists():
        return
    if path.is_dir():
        shutil.rmtree(path)
    else:
        path.unlink()


def _rename_legacy_binary_in_dir(dir_path: Path) -> None:
    legacy_exe = dir_path / f"{LEGACY_APP_NAME}.exe"
    if legacy_exe.exists():
        dst = dir_path / f"{APP_NAME}.exe"
        print(f"Renaming Windows binary: {legacy_exe} -> {dst}")
        _remove_path(dst)
        legacy_exe.rename(dst)

    legacy_bin = dir_path / LEGACY_APP_NAME
    if legacy_bin.exists():
        dst = dir_path / APP_NAME
        print(f"Renaming Linux binary: {legacy_bin} -> {dst}")
        _remove_path(dst)
        legacy_bin.rename(dst)


def _strip_version_from_filename(name: str) -> str:
    new = re.sub(r"([_-])\d+\.\d+\.\d+([_-])?", r"\1", name)
    new = new.replace("-.", ".").replace("_.", ".")
    while "__" in new:
        new = new.replace("__", "_")
    while "--" in new:
        new = new.replace("--", "-")
    new = new.replace("_-", "_").replace("-_", "-")
    return new


def rename_windows_linux_artifacts(frontend_dir: Path, platform_name: str) -> None:
    dist = dist_dir(frontend_dir)
    if not dist.exists():
        return

    renamed_any = False
    for item in sorted(dist.iterdir()):
        name = item.name
        if not (name.startswith(LEGACY_APP_NAME) or name.startswith(APP_NAME)):
            continue
        if name.startswith(LEGACY_APP_NAME):
            new_name = APP_NAME + name[len(LEGACY_APP_NAME):]
            dst = dist / new_name
            print(f"Renaming {platform_name} artifact: {name} -> {new_name}")
            _remove_path(dst)
            item.rename(dst)
            item = dst
            name = new_name
            renamed_any = True

        stripped = _strip_version_from_filename(name)
        if stripped != name:
            dst = dist / stripped
            print(f"Removing version from {platform_name} artifact: {name} -> {stripped}")
            _remove_path(dst)
            item.rename(dst)
            item = dst
            renamed_any = True

        if item.is_dir():
            _rename_legacy_binary_in_dir(item)

    _rename_legacy_binary_in_dir(dist)

    if not renamed_any:
        print(f"Warning: no {platform_name} artifacts matched legacy name for rename.", file=sys.stderr)


def clear_app_bundle(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    bundles = [dist / APP_BUNDLE_NAME, dist / LEGACY_APP_BUNDLE_NAME]
    for bundle in bundles:
        if bundle.exists():
            print(f"Removing existing app bundle: {bundle}")
            shutil.rmtree(bundle)

    dmgs = [dist / f"{APP_NAME}.dmg", dist / f"{LEGACY_APP_NAME}.dmg"]
    for dmg in dmgs:
        if dmg.exists():
            print(f"Removing existing dmg: {dmg}")
            dmg.unlink()
    remove_legacy_dmgs(frontend_dir)


def rename_macos_dmg(frontend_dir: Path) -> Optional[Path]:
    dist = dist_dir(frontend_dir)
    expected = dist / f"{APP_NAME}.dmg"
    legacy = dist / f"{LEGACY_APP_NAME}.dmg"

    if expected.exists():
        return expected

    if legacy.exists():
        print(f"Renaming macOS dmg: {legacy.name} -> {expected.name}")
        legacy.rename(expected)
        return expected

    dmgs = sorted(dist.glob("*.dmg"))
    if not dmgs:
        print("Warning: no macOS .dmg found to rename.", file=sys.stderr)
        return None

    if len(dmgs) == 1:
        src = dmgs[0]
        print(f"Renaming macOS dmg: {src.name} -> {expected.name}")
        if expected.exists():
            expected.unlink()
        src.rename(expected)
        return expected

    print("Warning: multiple .dmg files found; leaving as-is.", file=sys.stderr)
    return None


def rebuild_macos_dmg(frontend_dir: Path) -> Optional[Path]:
    dist = dist_dir(frontend_dir)
    app = rename_macos_app_bundle(frontend_dir) or app_bundle_path(frontend_dir)
    if not app.exists():
        raise FileNotFoundError(f"App bundle not found: {app}")

    target = dist / f"{APP_NAME}.dmg"
    legacy = dist / f"{LEGACY_APP_NAME}.dmg"

    for dmg in [target, legacy]:
        if dmg.exists():
            dmg.unlink()

    print(f"Creating macOS dmg: {target.name}")
    with tempfile.TemporaryDirectory(prefix="gs26_dmg_") as temp_dir:
        temp_path = Path(temp_dir)
        staged_app = temp_path / APP_BUNDLE_NAME
        shutil.copytree(app, staged_app, symlinks=True)
        os.symlink("/Applications", temp_path / "Applications")
        run(
            [
                "hdiutil",
                "create",
                "-volname",
                APP_NAME,
                "-srcfolder",
                str(temp_path),
                "-ov",
                "-format",
                "UDZO",
                str(target),
            ],
            cwd=frontend_dir,
        )
    return target if target.exists() else None


def _pick_codesign_identity(frontend_dir: Path, regex: str, pick: str) -> str:
    out = run_capture(["security", "find-identity", "-v", "-p", "codesigning"], cwd=frontend_dir)
    matches: list[str] = []
    pat = re.compile(r'^\s*\d+\)\s+[0-9A-Fa-f]+\s+"([^"]+)"\s*$')
    rx = re.compile(regex)

    for line in out.splitlines():
        m = pat.match(line.strip())
        if not m:
            continue
        name = m.group(1)
        if rx.search(name):
            matches.append(name)

    if not matches:
        raise RuntimeError(f"No matching code signing identities for regex: {regex}")

    if pick == "first":
        return matches[-1]
    return matches[0]


def _macos_entitlements_path(frontend_dir: Path) -> Optional[Path]:
    ent = os.environ.get("MACOS_ENTITLEMENTS", "").strip()
    if not ent:
        return None
    p = Path(ent)
    if not p.is_absolute():
        p = frontend_dir / p
    if not p.exists():
        raise FileNotFoundError(f"Entitlements file not found: {p}")
    return p


def sign_macos_app_and_dmg(frontend_dir: Path) -> None:
    if platform.system() != "Darwin":
        print("Error: macOS signing requires macOS.", file=sys.stderr)
        sys.exit(1)

    app = rename_macos_app_bundle(frontend_dir) or app_bundle_path(frontend_dir)
    if not app.exists():
        raise FileNotFoundError(f"App bundle not found: {app}")

    cert_regex = os.environ.get("CERT_REGEX", r"^Developer ID Application:")
    cert_pick = os.environ.get("CERT_PICK", "newest")
    identity = _pick_codesign_identity(frontend_dir, cert_regex, cert_pick)
    entitlements = _macos_entitlements_path(frontend_dir)

    print(f"Signing macOS app with identity: {identity}")
    sign_cmd = [
        "codesign",
        "--force",
        "--options",
        "runtime",
        "--timestamp",
        "--sign",
        identity,
    ]
    if entitlements:
        sign_cmd.extend(["--entitlements", str(entitlements)])
    sign_cmd.extend(["--deep", str(app)])
    run(sign_cmd, cwd=frontend_dir)

    dmg = rebuild_macos_dmg(frontend_dir)
    if not dmg:
        dmg = rename_macos_dmg(frontend_dir)
    if not dmg or not dmg.exists():
        print("Warning: no macOS .dmg found to sign.", file=sys.stderr)
        return

    print(f"Signing macOS dmg with identity: {identity}")
    run(
        [
            "codesign",
            "--force",
            "--timestamp",
            "--sign",
            identity,
            str(dmg),
        ],
        cwd=frontend_dir,
    )


def notarize_macos(frontend_dir: Path) -> None:
    if platform.system() != "Darwin":
        print("Error: macOS notarization requires macOS.", file=sys.stderr)
        sys.exit(1)

    dmg = rebuild_macos_dmg(frontend_dir)
    if not dmg:
        dmg = rename_macos_dmg(frontend_dir)
    target = dmg if dmg and dmg.exists() else app_bundle_path(frontend_dir)
    if not target.exists():
        raise FileNotFoundError(f"Notarization target not found: {target}")

    profile = os.environ.get("NOTARY_PROFILE", "").strip()
    apple_id = os.environ.get("NOTARY_APPLE_ID", "").strip()
    team_id = os.environ.get("NOTARY_TEAM_ID", "").strip()
    password = os.environ.get("NOTARY_PASSWORD", "").strip()

    auth_args: list[str]
    if profile:
        auth_args = ["--keychain-profile", profile]
    elif apple_id and team_id and password:
        auth_args = ["--apple-id", apple_id, "--team-id", team_id, "--password", password]
    else:
        raise RuntimeError(
            "Missing notarization credentials. Set NOTARY_PROFILE or "
            "NOTARY_APPLE_ID + NOTARY_TEAM_ID + NOTARY_PASSWORD."
        )

    print(f"Notarizing macOS artifact: {target.name}")
    run(["xcrun", "notarytool", "submit", str(target), "--wait", *auth_args], cwd=frontend_dir)
    run(["xcrun", "stapler", "staple", str(target)], cwd=frontend_dir)


def _prebuild_frontend_for_container(frontend_dir: Path) -> None:
    # IMPORTANT: do NOT run dx bundle here (you asked not to “install another version of dioxus” or do slow tooling
    # work)
    print("Container detected → priming cargo for frontend before dx bundle")
    run(["cargo", "fetch"], cwd=frontend_dir)


def _bash_login_path(cwd: Path) -> Optional[str]:
    """
    Return PATH as seen by `bash -lc` (login-ish shell), which is usually
    what you mean by “usual bash env” in containers/dev shells.
    """
    try:
        cmd = ["bash", "-lc", "printf '%s' \"$PATH\""]
        out = subprocess.check_output(cmd, cwd=cwd, env=os.environ)
        p = out.decode("utf-8", errors="replace").strip()
        return p or None
    except Exception:
        return None


def _which_in_path(exe: str, path_value: str) -> Optional[Path]:
    def _is_executable(path: Path) -> bool:
        try:
            return path.exists() and os.access(path, os.X_OK)
        except OSError:
            return False

    for raw_dir in path_value.split(os.pathsep):
        if not raw_dir:
            continue
        candidate = Path(raw_dir) / exe
        if _is_executable(candidate):
            return candidate
    return None


def _is_root_user() -> bool:
    return bool(hasattr(os, "geteuid") and os.geteuid() == 0)


def _find_wasm_opt(path_value: str) -> Optional[Path]:
    path_wasm_opt = _which_in_path("wasm-opt", path_value)
    if path_wasm_opt:
        return path_wasm_opt

    # Fall back to explicit common locations if PATH lookup fails.
    candidates = [
        Path("/usr/local/bin/wasm-opt"),
        Path("/usr/bin/wasm-opt"),
        Path("/opt/binaryen/bin/wasm-opt"),
        Path("/usr/local/bin/binaryen/bin/wasm-opt"),
        Path(str(Path.home() / ".cargo" / "bin" / "wasm-opt")),
    ]
    if _is_root_user():
        candidates.append(Path("/root/.cargo/bin/wasm-opt"))
    for cand in candidates:
        try:
            is_executable = cand.exists() and os.access(cand, os.X_OK)
        except OSError:
            is_executable = False
        if is_executable:
            return cand
    return None


def _find_wasm_bindgen(path_value: str) -> Optional[Path]:
    candidates = [
        Path("/usr/local/bin/wasm-bindgen"),
        Path("/usr/bin/wasm-bindgen"),
        Path(str(Path.home() / ".cargo" / "bin" / "wasm-bindgen")),
    ]
    if _is_root_user():
        candidates.append(Path("/root/.cargo/bin/wasm-bindgen"))
    for cand in candidates:
        try:
            is_executable = cand.exists() and os.access(cand, os.X_OK)
        except OSError:
            is_executable = False
        if is_executable:
            return cand
    return _which_in_path("wasm-bindgen", path_value)


def _find_brotli(path_value: str) -> Optional[Path]:
    candidates = [
        Path("/usr/local/bin/brotli"),
        Path("/usr/bin/brotli"),
        Path(str(Path.home() / ".cargo" / "bin" / "brotli")),
    ]
    for cand in candidates:
        try:
            is_executable = cand.exists() and os.access(cand, os.X_OK)
        except OSError:
            is_executable = False
        if is_executable:
            return cand
    return _which_in_path("brotli", path_value)


def _find_dx(path_value: str) -> Optional[Path]:
    path_dx = _which_in_path("dx", path_value)
    if path_dx:
        return path_dx

    candidates = [
        Path("/usr/local/bin/dx"),
        Path("/usr/bin/dx"),
        Path(str(Path.home() / ".cargo" / "bin" / "dx")),
    ]
    if _is_root_user():
        candidates.append(Path("/root/.cargo/bin/dx"))
    for cand in candidates:
        try:
            is_executable = cand.exists() and os.access(cand, os.X_OK)
        except OSError:
            is_executable = False
        if is_executable:
            return cand
    return None


def _find_newest_wasm_asset(frontend_dir: Path) -> Optional[Path]:
    assets_dir = frontend_dir / "dist" / "public" / "assets"
    if not assets_dir.exists():
        return None
    candidates = sorted(
        assets_dir.glob("groundstation_frontend_bg-*.wasm"),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )
    return candidates[0] if candidates else None


def _manual_optimize_web_wasm(frontend_dir: Path, env: Optional[dict[str, str]], max_size: bool) -> None:
    path_value = (env or {}).get("PATH", os.environ.get("PATH", ""))
    wasm_opt = _find_wasm_opt(path_value)
    if not wasm_opt:
        print("Warning: wasm-opt not found; skipping manual -O3 optimization.", file=sys.stderr)
        return

    targets: list[Path] = []
    canonical = frontend_dir / "dist" / "public" / "wasm" / "groundstation_frontend_bg.wasm"
    if canonical.exists():
        targets.append(canonical)

    newest_asset = _find_newest_wasm_asset(frontend_dir)
    if newest_asset and newest_asset not in targets:
        targets.append(newest_asset)

    if not targets:
        print("Warning: no web wasm artifacts found to optimize.", file=sys.stderr)
        return

    for wasm in targets:
        opt_cmd = [
            str(wasm_opt),
            "-O3",
            "--strip-debug",
            "--strip-producers",
        ]
        if max_size:
            opt_cmd.append("--converge")
        opt_cmd.extend([str(wasm), "-o", str(wasm)])
        print(f"Manual wasm-opt {' '.join(opt_cmd[1:])}: {wasm}")
        run(opt_cmd, cwd=frontend_dir, env=env)


def _compress_web_assets(frontend_dir: Path, env: Optional[dict[str, str]]) -> None:
    public_dir = frontend_dir / "dist" / "public"
    if not public_dir.exists():
        print("Warning: dist/public not found; skipping precompression.", file=sys.stderr)
        return

    exts = {".wasm", ".js", ".css", ".html", ".json", ".svg", ".txt", ".xml", ".map"}
    path_value = (env or {}).get("PATH", os.environ.get("PATH", ""))
    brotli_bin = _find_brotli(path_value)

    gz_count = 0
    br_count = 0
    for src in public_dir.rglob("*"):
        if not src.is_file():
            continue
        if src.suffix in {".gz", ".br"}:
            continue
        if src.suffix.lower() not in exts:
            continue

        gz = src.with_name(f"{src.name}.gz")
        if (not gz.exists()) or (gz.stat().st_mtime < src.stat().st_mtime):
            gz.write_bytes(gzip.compress(src.read_bytes(), compresslevel=9, mtime=0))
            gz_count += 1

        if brotli_bin is not None:
            br = src.with_name(f"{src.name}.br")
            if (not br.exists()) or (br.stat().st_mtime < src.stat().st_mtime):
                run(
                    [str(brotli_bin), "-f", "-q", "11", "-o", str(br), str(src)],
                    cwd=frontend_dir,
                    env=env,
                )
                br_count += 1

    if brotli_bin is None:
        print("Info: `brotli` binary not found; generated gzip assets only.")
    print(f"Precompression complete: gzip={gz_count}, brotli={br_count}")


def _clear_dx_web_cache(frontend_dir: Path) -> None:
    """
    Dioxus can reuse cached web/public asset dirs from target/, which may
    repopulate stale hashed assets into dist/public on rebuild.
    """
    target_dir = frontend_dir / "target"
    if not target_dir.exists():
        return

    removed = 0
    for p in target_dir.rglob("web/public/assets"):
        if p.is_dir():
            shutil.rmtree(p, ignore_errors=True)
            removed += 1
    for p in target_dir.rglob("web/public/wasm"):
        if p.is_dir():
            shutil.rmtree(p, ignore_errors=True)
            removed += 1

    if removed:
        print(f"Cleared stale Dioxus web cache dirs: {removed}")


def _prune_stale_hashed_assets(frontend_dir: Path) -> None:
    """
    Keep only the newest hash generation per asset family in dist/public/assets.
    Example:
      groundstation_frontend-dxh<hash>.js
      groundstation_frontend_bg-dxh<hash>.wasm
    Old hash generations (and their .gz/.br variants) are removed.
    """
    assets_dir = frontend_dir / "dist" / "public" / "assets"
    if not assets_dir.exists():
        return

    hashed_re = re.compile(r"^(?P<base>.+)-dxh(?P<hash>[0-9a-f]+)(?P<ext>\.[^.]+)(?P<comp>\.gz|\.br)?$")
    # Allow short overlap between deploys/cached HTML by keeping several hash generations.
    keep_generations = int(os.environ.get("GS_ASSET_HASH_KEEP", "3"))
    keep_generations = max(1, min(keep_generations, 20))

    # key: (base, ext, hash) -> newest mtime among compressed/uncompressed siblings
    hash_mtime: dict[tuple[str, str, str], float] = {}
    parsed: list[tuple[Path, str, str, str]] = []

    for p in assets_dir.iterdir():
        if not p.is_file():
            continue
        m = hashed_re.match(p.name)
        if not m:
            continue
        base = m.group("base")
        h = m.group("hash")
        ext = m.group("ext")
        parsed.append((p, base, ext, h))
        mtime = p.stat().st_mtime
        key = (base, ext, h)
        prev = hash_mtime.get(key)
        if prev is None or mtime > prev:
            hash_mtime[key] = mtime

    # Hashes referenced by current HTML entrypoints are always kept.
    referenced_hashes: set[tuple[str, str, str]] = set()
    html_ref_re = re.compile(r"/(?:\./)?assets/([^\"'?#\s]+)")
    for html in (frontend_dir / "dist" / "public").glob("*.html"):
        try:
            txt = html.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for m in html_ref_re.finditer(txt):
            name = m.group(1)
            hm = hashed_re.match(name)
            if not hm:
                continue
            referenced_hashes.add((hm.group("base"), hm.group("ext"), hm.group("hash")))

    # Keep top-N recent hashes per (base, ext), plus any HTML-referenced hash.
    keep_hashes: set[tuple[str, str, str]] = set(referenced_hashes)
    by_family: dict[tuple[str, str], list[tuple[str, float]]] = {}
    for (base, ext, h), mtime in hash_mtime.items():
        by_family.setdefault((base, ext), []).append((h, mtime))
    for (base, ext), arr in by_family.items():
        arr.sort(key=lambda x: x[1], reverse=True)
        for h, _ in arr[:keep_generations]:
            keep_hashes.add((base, ext, h))

    removed = 0
    for p, base, ext, h in parsed:
        if (base, ext, h) not in keep_hashes:
            try:
                p.unlink()
                removed += 1
            except OSError:
                pass

    if removed:
        print(f"Pruned stale hashed assets: {removed} (kept {keep_generations} generations)")


def _dx_bundle_env(frontend_dir: Path) -> dict[str, str]:
    """
    Construct an environment that:
      - in containers: uses PATH from `bash -lc` so we match profile scripts
      - forces Dioxus CLI to *not download tools* (NO_DOWNLOADS=1)
      - points dx/wasm toolchains at your already-installed wasm-opt
    """
    base_path = os.environ.get("PATH", "")

    if is_container():
        bash_path = _bash_login_path(frontend_dir)
        if bash_path:
            base_path = bash_path

    extra_paths = [
        str(Path.home() / ".cargo" / "bin"),
        "/usr/local/sbin",
        "/usr/local/bin",
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
        "/opt/binaryen/bin",
    ]

    env: dict[str, str] = {}
    env["PATH"] = os.pathsep.join(extra_paths + [base_path])

    # CRITICAL: tell dx to trust the environment and NOT auto-download wasm-opt/wasm-bindgen, etc.
    # (Dioxus CLI supports NO_DOWNLOADS=1 and a runtime no_downloads setting).
    if in_docker_build() or is_container():
        env["NO_DOWNLOADS"] = "1"

    wasm_opt = _find_wasm_opt(env["PATH"])
    if wasm_opt:
        # Cover common env names used by toolchains.
        env["WASM_OPT"] = str(wasm_opt)
        env["WASMOPT"] = str(wasm_opt)
        env["DIOXUS_WASM_OPT"] = str(wasm_opt)
        env["DIOXUS_WASM_OPT_PATH"] = str(wasm_opt)

    wasm_bindgen = _find_wasm_bindgen(env["PATH"])
    if wasm_bindgen:
        env["WASM_BINDGEN"] = str(wasm_bindgen)
        env["DIOXUS_WASM_BINDGEN"] = str(wasm_bindgen)
        env["DIOXUS_WASM_BINDGEN_PATH"] = str(wasm_bindgen)

    return env


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
    if platform.system() != "Darwin":
        print("Error: iOS packaging/signing requires macOS.", file=sys.stderr)
        sys.exit(1)

    app = app_bundle_path(frontend_dir)
    if not app.exists():
        raise FileNotFoundError(f"App bundle not found: {app}")

    patch_plist(frontend_dir)

    profile = fixed_mobileprovision_path(frontend_dir)

    signer = frontend_dir / "scripts" / "ios_package_sign.sh"
    if not signer.exists():
        raise FileNotFoundError(f"Missing signer script: {signer}")

    ipas_dir = frontend_dir / "dist" / "ipas"
    ipas_dir.mkdir(parents=True, exist_ok=True)

    ipa_name = "UBSEDS GS.ipa"
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

    run(
        ["bash", str(signer), str(app.resolve()), str(profile.resolve()), str(ipa_out.resolve())],
        cwd=frontend_dir,
        env=env,
    )

    if not ipa_out.exists() or ipa_out.stat().st_size == 0:
        raise RuntimeError(f"IPA not created or empty: {ipa_out}")

    return ipa_out


def _simctl_booted_device_udid(frontend_dir: Path) -> Optional[str]:
    try:
        out = run_capture(["xcrun", "simctl", "list", "devices", "booted", "-j"], cwd=frontend_dir)
        data = json.loads(out)
    except Exception:
        return None

    devices = data.get("devices", {})
    for _runtime, arr in devices.items():
        for dev in arr or []:
            if dev.get("state") == "Booted":
                udid = dev.get("udid")
                if udid:
                    return str(udid)
    return None


def _simctl_first_available_iphone_udid(frontend_dir: Path) -> Optional[str]:
    try:
        out = run_capture(["xcrun", "simctl", "list", "devices", "available", "-j"], cwd=frontend_dir)
        data = json.loads(out)
    except Exception:
        return None

    devices = data.get("devices", {})
    for _runtime, arr in devices.items():
        for dev in arr or []:
            if dev.get("isAvailable") is False:
                continue
            name = str(dev.get("name", "")).lower()
            if "iphone" not in name:
                continue
            udid = dev.get("udid")
            if udid:
                return str(udid)
    return None


def _bundle_id_from_app(app: Path) -> str:
    plist_path = app / "Info.plist"
    if not plist_path.exists():
        raise FileNotFoundError(f"Info.plist not found in app bundle: {plist_path}")
    with plist_path.open("rb") as f:
        info = plistlib.load(f)
    bundle_id = info.get("CFBundleIdentifier")
    if not bundle_id:
        raise RuntimeError(f"CFBundleIdentifier missing in {plist_path}")
    return str(bundle_id)


def ios_sim_deploy(frontend_dir: Path) -> tuple[str, str]:
    if platform.system() != "Darwin":
        print("Error: iOS simulator deploy requires macOS.", file=sys.stderr)
        sys.exit(1)

    app = app_bundle_path(frontend_dir)
    if not app.exists():
        raise FileNotFoundError(f"Simulator app bundle not found: {app}")

    udid = _simctl_booted_device_udid(frontend_dir)
    if not udid:
        udid = _simctl_first_available_iphone_udid(frontend_dir)
        if not udid:
            raise RuntimeError("No available iPhone simulator found.")
        print(f"Booting iOS simulator device: {udid}")
        run(["xcrun", "simctl", "boot", udid], cwd=frontend_dir)
        run(["xcrun", "simctl", "bootstatus", udid, "-b"], cwd=frontend_dir)

    print(f"Installing app in simulator ({udid}): {app}")
    run(["xcrun", "simctl", "install", udid, str(app)], cwd=frontend_dir)

    bundle_id = _bundle_id_from_app(app)
    print(f"Launching simulator app: {bundle_id}")
    run(["xcrun", "simctl", "launch", udid, bundle_id], cwd=frontend_dir)
    return udid, bundle_id


def macos_deploy(frontend_dir: Path) -> Path:
    if platform.system() != "Darwin":
        print("Error: macos_deploy requires macOS.", file=sys.stderr)
        sys.exit(1)

    src_app = app_bundle_path(frontend_dir)
    if not src_app.exists():
        raise FileNotFoundError(f"App bundle not found: {src_app}")

    applications_dir = Path("/Applications")
    dst_app = applications_dir / APP_BUNDLE_NAME

    print(f"Deploying macOS app → {dst_app} (from {src_app.name})")

    if dst_app.exists():
        print(f"Removing existing /Applications copy: {dst_app}")
        shutil.rmtree(dst_app)

    try:
        shutil.copytree(src_app, dst_app, symlinks=True)
    except PermissionError as e:
        print(
            "Error: Permission denied copying into /Applications.\n"
            "Try one of these:\n"
            "  - Run: sudo ./build.py macos_deploy\n"
            "  - Or deploy to ~/Applications (create it) and drag-drop manually.\n"
            f"Original error: {e}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"✅ Deployed: {dst_app}")
    return dst_app


def _host_macos_target() -> str:
    override = os.environ.get("GS26_MACOS_TARGET", "").strip()
    if override:
        return override

    m = platform.machine().lower()
    if "arm" in m or "aarch64" in m:
        return "aarch64-apple-darwin"
    return "x86_64-apple-darwin"


def _windows_target_default() -> str:
    return os.environ.get("GS26_WINDOWS_TARGET", "x86_64-pc-windows-gnu").strip()


def _default_rust_target_for_frontend(platform_name: Optional[str]) -> Optional[str]:
    if platform_name is None or platform_name == "web":
        return None
    if platform_name == "macos":
        return _host_macos_target()
    if platform_name == "windows":
        return _windows_target_default()
    return None


def build_frontend(
        frontend_dir: Path,
        platform_name: Optional[str] = None,
        *,
        rust_target: Optional[str] = None,
        debug_mode: bool = False,
        max_size: bool = False,
) -> None:
    try:
        public_dir = frontend_dir / "dist" / "public"
        if public_dir.exists():
            print(f"Removing existing public artifacts: {public_dir}")
            shutil.rmtree(public_dir)
        if platform_name in {None, "web"}:
            _clear_dx_web_cache(frontend_dir)

        clear_app_bundle(frontend_dir)

        env = _dx_bundle_env(frontend_dir) if (is_container() or in_docker_build()) else None

        if is_container():
            _prebuild_frontend_for_container(frontend_dir)

            # quick sanity prints (won't install anything)
            try:
                run(["bash", "-lc", "echo $PATH"], cwd=frontend_dir, env=env)
                run(["bash", "-lc", "command -v wasm-opt && wasm-opt --version"], cwd=frontend_dir, env=env)
            except Exception:
                print("Warning: could not verify wasm-opt via bash -lc", file=sys.stderr)

        # Find dx using the same PATH we will run with (important in containers)
        dx_path = None
        if env is not None:
            dx_path = _find_dx(env["PATH"])
        else:
            dx_path = _find_dx(os.environ.get("PATH", ""))

        if dx_path:
            cmd = [str(dx_path), "bundle"]
        else:
            cmd = ["dx", "bundle"]

        if not debug_mode:
            cmd.append("--release")

        if platform_name:
            cmd.extend(["--platform", platform_name])
            if platform_name == "ios":
                is_ios_sim_target = bool(rust_target and ("ios-sim" in rust_target or "simulator" in rust_target))
                if not is_ios_sim_target:
                    cmd.extend(["--device", "true"])
            elif platform_name == "windows":
                cmd.extend(["--windows-subsystem", "WINDOWS"])
        else:
            cmd.extend(["--platform", "web"])

        if not rust_target:
            rust_target = _default_rust_target_for_frontend(platform_name)

        if rust_target:
            cmd.extend(["--target", rust_target])

        run(cmd, cwd=frontend_dir, env=env)

        if platform_name in {None, "web"}:
            _manual_optimize_web_wasm(frontend_dir, env, max_size=max_size)
            _prune_stale_hashed_assets(frontend_dir)
            _compress_web_assets(frontend_dir, env)

        if platform_name == "macos":
            rename_macos_app_bundle(frontend_dir)
            remove_legacy_app_bundle(frontend_dir)
            remove_legacy_dmgs(frontend_dir)
            sign_macos_app_and_dmg(frontend_dir)
        elif platform_name in {"windows", "linux"}:
            rename_windows_linux_artifacts(frontend_dir, platform_name)

        if platform_name == "ios":
            patch_plist(frontend_dir)

    except subprocess.CalledProcessError as e:
        print("Frontend build failed.", file=sys.stderr)
        sys.exit(e.returncode)


def build_backend(
        backend_dir: Path,
        force_pi: bool,
        force_no_pi: bool,
        testing_mode: bool,
        debug_mode: bool = False,
) -> None:
    cmd = ["cargo", "build", "-p", "groundstation_backend"]
    if not debug_mode:
        cmd.insert(2, "--release")

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
    print("  ./build.py debug                   # local: build frontend+backend in debug mode")
    print("  ./build.py max_size                # web wasm: add wasm-opt --converge (slower, smaller)")
    print("  ./build.py plain                   # docker only: pass --progress plain")
    print("  ./build.py log=build.log           # tee command output into a log file")
    print("  ./build.py docker [pi_build|no_pi] [testing]")
    print("  ./build.py backend_only            # local: build backend only")
    print("  ./build.py frontend_web            # local: build frontend web only")
    print("")
    print("Frontend-only builds:")
    print("  ./build.py ios                     # iPhoneOS build (UNSIGNED; patched)")
    print("  ./build.py ios_sim                 # iOS simulator build (patched)")
    print("  ./build.py macos")
    print("  ./build.py windows")
    print("  ./build.py android")
    print("  ./build.py linux")
    print("  (add `debug` to frontend/local builds to skip --release)")
    print("")
    print("Frontend actions:")
    print("  ./build.py ios_deploy              # build ios + patch + package+sign (Distribution) -> IPA")
    print("  ./build.py ios_sim_deploy          # build ios_sim + install + launch in iOS simulator")
    print("  ./build.py ios_sign                # package+sign (Dev) existing dist app -> IPA")
    print("  ./build.py ios_dist_sign           # package+sign (Distribution) existing dist app -> IPA")
    print("  ./build.py macos_deploy            # build macos + copy .app into /Applications")
    print("  ./build.py macos_sign              # sign existing macos app + dmg (Developer ID)")
    print("  ./build.py macos_notarize          # build macos + sign + notarize + staple")
    print("")
    print("Provisioning profile path (fixed):")
    print(f"  frontend/{FIXED_MOBILEPROVISION_REL}")
    print("")
    print("Env (optional):")
    print("  CERT_REGEX=...                     # override cert regex for signer script")
    print("  CERT_PICK=newest|first             # override cert selection for signer script")
    print("  MACOS_ENTITLEMENTS=...             # optional entitlements file for macOS codesign")
    print("  NOTARY_PROFILE=...                 # notarytool keychain profile (preferred)")
    print("  NOTARY_APPLE_ID=...                # notarytool Apple ID (alt auth)")
    print("  NOTARY_TEAM_ID=...                 # notarytool team ID (alt auth)")
    print("  NOTARY_PASSWORD=...                # notarytool app-specific password (alt auth)")
    print("  existing                           # skip build step for frontend actions/builds")
    print("  GROUNDSTATION_NO_PARALLEL=1        # force sequential build")
    print("  GS26_WINDOWS_TARGET=...            # override windows Rust target (default x86_64-pc-windows-gnu)")
    print("  GS26_MACOS_TARGET=...              # override macos Rust target (auto-detects by default)")
    sys.exit(1)


def main() -> None:
    global LOG_FILE

    force_pi = False
    force_no_pi = False
    docker_mode = False
    testing_mode = False
    debug_mode = False
    max_size_mode = False
    plain_mode = False
    use_existing = False
    backend_only = False
    log_file_arg: Optional[str] = None

    frontend_only_platform: Optional[str] = None
    frontend_rust_target: Optional[str] = None
    action: Optional[str] = None

    raw_args = [a.strip() for a in sys.argv[1:]]

    if len(raw_args) > 8:
        print("Error: Too many arguments.", file=sys.stderr)
        print_usage()

    frontend_platform_map = {
        "ios": ("ios", "aarch64-apple-ios"),
        "ios_sim": ("ios", "aarch64-apple-ios-sim"),
        "macos": ("macos", None),
        "windows": ("windows", None),
        "android": ("android", None),
        "linux": ("linux", None),
        "web": ("web", None),
        "frontend_web": ("web", None),
    }

    actions = {
        "ios_deploy",
        "ios_sim_deploy",
        "ios_sign",
        "ios_dist_sign",
        "macos_deploy",
        "macos_sign",
        "macos_notarize",
    }

    for raw_arg in raw_args:
        arg = raw_arg.lower()
        if arg == "pi_build":
            force_pi = True
        elif arg == "no_pi":
            force_no_pi = True
        elif arg == "docker":
            docker_mode = True
        elif arg == "plain":
            plain_mode = True
        elif arg == "testing":
            testing_mode = True
        elif arg == "debug":
            debug_mode = True
        elif arg == "max_size":
            max_size_mode = True
        elif arg in {"backend_only", "backend"}:
            backend_only = True
        elif arg == "existing":
            use_existing = True
        elif arg.startswith("log="):
            value = raw_arg.split("=", 1)[1].strip()
            if not value:
                print("Error: log= requires a filepath.", file=sys.stderr)
                print_usage()
            log_file_arg = value
        elif arg in actions:
            if action or frontend_only_platform or backend_only:
                print("Error: Only one frontend action/build may be specified.", file=sys.stderr)
                print_usage()
            action = arg
        elif arg in frontend_platform_map:
            if frontend_only_platform or action or backend_only:
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

    if log_file_arg:
        log_path = Path(log_file_arg)
        if not log_path.is_absolute():
            log_path = repo_root / log_path
        log_path.parent.mkdir(parents=True, exist_ok=True)
        log_path.write_text("", encoding="utf-8")
        LOG_FILE = log_path
        print(f"Logging command output to: {LOG_FILE}")

    if action:
        if docker_mode or force_pi or force_no_pi or testing_mode:
            print("Error: Frontend actions cannot be combined with docker/pi_build/no_pi/testing.", file=sys.stderr)
            print_usage()

        if action == "ios_deploy":
            if not use_existing:
                build_frontend(
                    frontend_dir,
                    platform_name="ios",
                    rust_target="aarch64-apple-ios",
                    debug_mode=debug_mode,
                    max_size=max_size_mode,
                )
            ipa = package_ios_ipa_with_script(frontend_dir, sign_kind="distribution")
            print(f"✅ Distribution IPA created: {ipa}")
            return

        if action == "ios_sim_deploy":
            if not use_existing:
                build_frontend(
                    frontend_dir,
                    platform_name="ios",
                    rust_target="aarch64-apple-ios-sim",
                    debug_mode=debug_mode,
                    max_size=max_size_mode,
                )
            udid, bundle_id = ios_sim_deploy(frontend_dir)
            print(f"✅ Simulator deploy complete ({udid}) for {bundle_id}")
            return

        if action == "iosDRY_RUN":
            return

        if action == "ios_sign":
            if not use_existing:
                build_frontend(
                    frontend_dir,
                    platform_name="ios",
                    rust_target="aarch64-apple-ios",
                    debug_mode=debug_mode,
                    max_size=max_size_mode,
                )
            ipa = package_ios_ipa_with_script(frontend_dir, sign_kind="development")
            print(f"✅ Dev IPA created: {ipa}")
            return

        if action == "ios_dist_sign":
            if not use_existing:
                build_frontend(
                    frontend_dir,
                    platform_name="ios",
                    rust_target="aarch64-apple-ios",
                    debug_mode=debug_mode,
                    max_size=max_size_mode,
                )
            ipa = package_ios_ipa_with_script(frontend_dir, sign_kind="distribution")
            print(f"✅ Distribution IPA created: {ipa}")
            return

        if action == "macos_deploy":
            if not use_existing:
                build_frontend(
                    frontend_dir,
                    platform_name="macos",
                    rust_target=None,
                    debug_mode=debug_mode,
                    max_size=max_size_mode,
                )
            sign_macos_app_and_dmg(frontend_dir)
            deployed = macos_deploy(frontend_dir)
            print(f"✅ Installed into /Applications: {deployed}")
            return

        if action == "macos_sign":
            if not use_existing:
                build_frontend(
                    frontend_dir,
                    platform_name="macos",
                    rust_target=None,
                    debug_mode=debug_mode,
                    max_size=max_size_mode,
                )
            sign_macos_app_and_dmg(frontend_dir)
            print("✅ Signed macOS app and dmg")
            return

        if action == "macos_notarize":
            if not use_existing:
                build_frontend(
                    frontend_dir,
                    platform_name="macos",
                    rust_target=None,
                    debug_mode=debug_mode,
                    max_size=max_size_mode,
                )
            notarize_macos(frontend_dir)
            print("✅ Notarized macOS artifact")
            return

        print("Error: unknown action", file=sys.stderr)
        sys.exit(1)

    if frontend_only_platform is not None:
        if docker_mode or force_pi or force_no_pi or testing_mode:
            print("Error: Frontend-only builds cannot be combined with docker/pi_build/no_pi/testing.", file=sys.stderr)
            print_usage()
        if use_existing:
            print("Skipping frontend build (existing requested).")
            return
        build_frontend(
            frontend_dir,
            platform_name=frontend_only_platform,
            rust_target=frontend_rust_target,
            debug_mode=debug_mode,
            max_size=max_size_mode,
        )
        return

    if backend_only:
        if docker_mode:
            print("Error: backend_only cannot be combined with docker mode.", file=sys.stderr)
            print_usage()
        build_backend(backend_dir, force_pi, force_no_pi, testing_mode, debug_mode)
        return

    if docker_mode:
        if force_no_pi:
            pi_build_flag = False
        else:
            if not force_pi and is_raspberry_pi():
                force_pi = True
            pi_build_flag = force_pi
        use_plain = plain_mode or (LOG_FILE is not None)
        print(
            "Note: docker image builds cannot be post-processed with host wasm-opt; optimize in Dockerfile for image "
            "artifacts.")
        build_docker(repo_root=repo_root, pi_build=pi_build_flag, testing=testing_mode, plain_progress=use_plain)
        return

    if in_docker_build():
        print("Sequential build")
        build_frontend(frontend_dir, None, debug_mode=debug_mode, max_size=max_size_mode)
        build_backend(backend_dir, force_pi, force_no_pi, testing_mode, debug_mode)
        return

    bfe = mp.Process(
        target=build_frontend,
        args=(frontend_dir, None),
        kwargs={"debug_mode": debug_mode, "max_size": max_size_mode},
    )
    bbe = mp.Process(
        target=build_backend,
        args=(backend_dir, force_pi, force_no_pi, testing_mode, debug_mode),
    )
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
