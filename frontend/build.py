#!/usr/bin/env python3
import errno
import gzip
import json
import os
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import time
import zipfile

import platform

try:
    import tomllib  # py3.11+
except ImportError:  # pragma: no cover
    tomllib = None

from pathlib import Path
from subprocess import DEVNULL
from typing import Optional, Literal, BinaryIO, cast

APP_NAME = "UBSEDS GS"
WINDOWS_APP_NAME = "UBSEDS GroundStation"
ANDROID_APP_NAME = "SEDS GS"
MACOS_ALT_APP_NAME = "SEDS GS"
LINUX_PACKAGE_NAME = "ubseds-groundstation"
LEGACY_APP_NAME = "GroundstationFrontend"
DIST_DIRNAME = "dist"
APP_BUNDLE_NAME = f"{APP_NAME}.app"
MACOS_ALT_APP_BUNDLE_NAME = f"{MACOS_ALT_APP_NAME}.app"
LEGACY_APP_BUNDLE_NAME = f"{LEGACY_APP_NAME}.app"

# fixed provisioning profile path (repo-local)
FIXED_MOBILEPROVISION_REL = Path("Groundstation_26.mobileprovision")

LOG_FILE: Optional[Path] = None
INTERRUPTED_EXIT_CODE = 130


def _append_log(line: str) -> None:
    if LOG_FILE is None:
        return
    with LOG_FILE.open("a", encoding="utf-8") as f:
        f.write(line)


def _cmd_to_str(cmd: object) -> str:
    if isinstance(cmd, (list, tuple)):
        return " ".join(str(x) for x in cmd)
    return str(cmd)


def _print_command_failure(context: str, err: subprocess.CalledProcessError, cwd: Path) -> None:
    print(f"\nError: {context} failed.", file=sys.stderr)
    print(f"  Command : {_cmd_to_str(err.cmd)}", file=sys.stderr)
    print(f"  CWD     : {cwd}", file=sys.stderr)
    print(f"  Exit    : {err.returncode}", file=sys.stderr)
    if LOG_FILE is not None:
        print(f"  Log file: {LOG_FILE}", file=sys.stderr)

    cmd_s = _cmd_to_str(err.cmd).lower()
    if "dx bundle" in cmd_s:
        print("Hint: check that `dx` is installed and matches your project setup.", file=sys.stderr)
        print("      If needed: `cargo install dioxus-cli`.", file=sys.stderr)
    elif "cargo build" in cmd_s:
        print("Hint: run `cargo build` directly in the same cwd for full compiler diagnostics.", file=sys.stderr)
    elif "docker" in cmd_s:
        print("Hint: verify Docker daemon is running and build context paths are valid.", file=sys.stderr)


def _print_missing_tool(context: str, err: FileNotFoundError, cwd: Path) -> None:
    missing = err.filename or "<unknown>"
    print(f"\nError: {context} could not start because a required tool is missing.", file=sys.stderr)
    print(f"  Missing : {missing}", file=sys.stderr)
    details = str(err).strip()
    if details and details != missing:
        print(f"  Details : {details}", file=sys.stderr)
    print(f"  CWD     : {cwd}", file=sys.stderr)
    if LOG_FILE is not None:
        print(f"  Log file: {LOG_FILE}", file=sys.stderr)

    low = str(missing).lower()
    if low.endswith("/dx") or low == "dx":
        print("Hint: install Dioxus CLI and ensure `dx` is on PATH.", file=sys.stderr)
        print("      Example: `cargo install dioxus-cli`", file=sys.stderr)
    elif low == "cargo":
        print("Hint: install Rust via rustup and ensure cargo is on PATH.", file=sys.stderr)
        print("      Example: https://rustup.rs", file=sys.stderr)
    elif low in {"bash", "/bin/bash"}:
        print("Hint: bash is required by parts of the build scripts.", file=sys.stderr)


def run(cmd: list[str], cwd: Path, env: Optional[dict[str, str]] = None) -> None:
    cmd = [str(part) for part in cmd]
    cmd_line = f"Running: {' '.join(cmd)} (cwd={cwd})"
    print(cmd_line)
    _append_log(cmd_line + "\n")
    merged = os.environ.copy()
    if env:
        merged.update(env)
    if LOG_FILE is None:
        try:
            subprocess.run(cmd, cwd=cwd, check=True, env=merged)
        except KeyboardInterrupt:
            raise
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
    try:
        for line in proc.stdout:
            print(line, end="")
            _append_log(line)
        rc = proc.wait()
    except KeyboardInterrupt:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        raise
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


def patch_plist(frontend_dir: Path, app_dir: Optional[Path] = None) -> None:
    script = frontend_dir / "scripts" / "patch_plist.sh"
    version = _read_frontend_version(frontend_dir)
    build = _read_dioxus_build(frontend_dir)
    env = {
        "APP_VERSION": version,
        "APP_BUILD": build,
    }
    if app_dir is not None:
        env["APP_DIR"] = str(app_dir)
    run_script(script, cwd=frontend_dir, env=env)


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
    alt = dist / MACOS_ALT_APP_BUNDLE_NAME
    legacy = dist / LEGACY_APP_BUNDLE_NAME
    if preferred.exists():
        return preferred
    if alt.exists():
        return alt
    if legacy.exists():
        return legacy
    return preferred


def _stage_app_bundle_from_dx(
        frontend_dir: Path,
        *,
        platform_name: str,
        preferred_bundle_name: str,
) -> Optional[Path]:
    dist = dist_dir(frontend_dir)
    dist.mkdir(parents=True, exist_ok=True)
    preferred = dist / preferred_bundle_name
    pkg_name = _frontend_package_name(frontend_dir)
    target_root = frontend_dir.parent / "target" / "dx" / pkg_name
    candidates = [
        target_root / "release" / platform_name / LEGACY_APP_BUNDLE_NAME,
        target_root / "release" / platform_name / APP_BUNDLE_NAME,
        target_root / "release" / platform_name / MACOS_ALT_APP_BUNDLE_NAME,
        target_root / "debug" / platform_name / LEGACY_APP_BUNDLE_NAME,
        target_root / "debug" / platform_name / APP_BUNDLE_NAME,
        target_root / "debug" / platform_name / MACOS_ALT_APP_BUNDLE_NAME,
    ]

    for src in candidates:
        if not src.exists():
            continue
        print(f"Staging {platform_name} app bundle: {src} -> {preferred}")
        if preferred.exists():
            shutil.rmtree(preferred)
        shutil.copytree(src, preferred, symlinks=True)
        return preferred
    return None


def rename_macos_app_bundle(frontend_dir: Path) -> Optional[Path]:
    dist = dist_dir(frontend_dir)
    preferred = dist / APP_BUNDLE_NAME
    alt = dist / MACOS_ALT_APP_BUNDLE_NAME
    legacy = dist / LEGACY_APP_BUNDLE_NAME

    if preferred.exists():
        return preferred
    if alt.exists():
        print(f"Renaming macOS app bundle: {alt.name} -> {preferred.name}")
        if preferred.exists():
            shutil.rmtree(preferred)
        alt.rename(preferred)
        return preferred
    if legacy.exists():
        print(f"Renaming macOS app bundle: {legacy.name} -> {preferred.name}")
        if preferred.exists():
            shutil.rmtree(preferred)
        legacy.rename(preferred)
        return preferred
    return _stage_app_bundle_from_dx(
        frontend_dir,
        platform_name="macos",
        preferred_bundle_name=APP_BUNDLE_NAME,
    )


def remove_legacy_app_bundle(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    preferred = dist / APP_BUNDLE_NAME
    alt = dist / MACOS_ALT_APP_BUNDLE_NAME
    legacy = dist / LEGACY_APP_BUNDLE_NAME
    if preferred.exists() and alt.exists():
        print(f"Removing alternate macOS app bundle: {alt}")
        shutil.rmtree(alt)
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


def _rename_binary_in_dir(dir_path: Path, platform_name: str) -> None:
    legacy_exe = dir_path / f"{LEGACY_APP_NAME}.exe"
    current_exe = dir_path / f"{APP_NAME}.exe"
    target_exe_name = WINDOWS_APP_NAME if platform_name == "windows" else APP_NAME
    if legacy_exe.exists():
        dst = dir_path / f"{target_exe_name}.exe"
        print(f"Renaming Windows binary: {legacy_exe} -> {dst}")
        _remove_path(dst)
        legacy_exe.rename(dst)
    elif platform_name == "windows" and current_exe.exists() and target_exe_name != APP_NAME:
        dst = dir_path / f"{target_exe_name}.exe"
        print(f"Renaming Windows binary: {current_exe} -> {dst}")
        _remove_path(dst)
        current_exe.rename(dst)

    legacy_bin = dir_path / LEGACY_APP_NAME
    current_bin = dir_path / APP_NAME
    target_bin_name = WINDOWS_APP_NAME if platform_name == "windows" else APP_NAME
    if legacy_bin.exists():
        dst = dir_path / target_bin_name
        print(f"Renaming Linux binary: {legacy_bin} -> {dst}")
        _remove_path(dst)
        legacy_bin.rename(dst)
    elif current_bin.exists() and target_bin_name != APP_NAME:
        dst = dir_path / target_bin_name
        print(f"Renaming binary: {current_bin} -> {dst}")
        _remove_path(dst)
        current_bin.rename(dst)


def _strip_version_from_filename(name: str) -> str:
    new = re.sub(r"([_-])\d+\.\d+\.\d+([_-])?", r"\1", name)
    new = new.replace("-.", ".").replace("_.", ".")
    while "__" in new:
        new = new.replace("__", "_")
    while "--" in new:
        new = new.replace("--", "-")
    new = new.replace("_-", "_").replace("-_", "-")
    return new


def _strip_android_target_suffix(name: str) -> str:
    return re.sub(r"-(aarch64|armv7|arm|x86_64|i686)-linux-android(?=\.)", "", name)


def rename_windows_linux_artifacts(frontend_dir: Path, platform_name: str) -> None:
    dist = dist_dir(frontend_dir)
    if not dist.exists():
        return

    target_name = WINDOWS_APP_NAME if platform_name == "windows" else APP_NAME
    renamed_any = False
    for item in sorted(dist.iterdir()):
        name = item.name
        if not (
                name.startswith(LEGACY_APP_NAME)
                or name.startswith(APP_NAME)
                or name.startswith(target_name)
        ):
            continue
        if name.startswith(LEGACY_APP_NAME):
            new_name = target_name + name[len(LEGACY_APP_NAME):]
            dst = dist / new_name
            print(f"Renaming {platform_name} artifact: {name} -> {new_name}")
            _remove_path(dst)
            item.rename(dst)
            item = dst
            name = new_name
            renamed_any = True
        elif name.startswith(APP_NAME) and target_name != APP_NAME:
            new_name = target_name + name[len(APP_NAME):]
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
            _rename_binary_in_dir(item, platform_name)

    _rename_binary_in_dir(dist, platform_name)

    if not renamed_any:
        print(f"Warning: no {platform_name} artifacts matched legacy name for rename.", file=sys.stderr)


def patch_linux_bundle_metadata(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    if not dist.exists():
        return

    icon_src = frontend_dir / "assets" / "icon.png"
    desktop_files = sorted(dist.rglob("*.desktop"))
    for desktop_file in desktop_files:
        try:
            original = desktop_file.read_text(encoding="utf-8")
        except OSError:
            continue

        patched_lines: list[str] = []
        saw_name = False
        saw_exec = False
        saw_icon = False

        for line in original.splitlines():
            if line.startswith("Name="):
                patched_lines.append(f"Name={WINDOWS_APP_NAME}")
                saw_name = True
            elif line.startswith("Exec="):
                patched_lines.append(f"Exec={LINUX_PACKAGE_NAME}")
                saw_exec = True
            elif line.startswith("Icon="):
                patched_lines.append(f"Icon={LINUX_PACKAGE_NAME}")
                saw_icon = True
            else:
                patched_lines.append(line)

        if not saw_name:
            patched_lines.append(f"Name={WINDOWS_APP_NAME}")
        if not saw_exec:
            patched_lines.append(f"Exec={LINUX_PACKAGE_NAME}")
        if not saw_icon:
            patched_lines.append(f"Icon={LINUX_PACKAGE_NAME}")

        patched = "\n".join(patched_lines) + "\n"
        if patched != original:
            print(f"Patching Linux desktop entry: {desktop_file}")
            desktop_file.write_text(patched, encoding="utf-8")

        if icon_src.exists():
            icon_dst = desktop_file.parent / f"{LINUX_PACKAGE_NAME}.png"
            shutil.copy2(icon_src, icon_dst)


def _windows_installer_name() -> str:
    return f"{WINDOWS_APP_NAME} Installer.exe"


def _linux_deb_name() -> str:
    return "UBSEDS GS_amd64.deb"


def _linux_rpm_name() -> str:
    return "UBSEDS GS_x86_64.rpm"


def _resolve_makensis() -> Optional[str]:
    def _recursive_find(root: Path) -> Optional[str]:
        if not root.exists():
            return None
        try:
            for candidate in root.rglob("makensis.exe"):
                if candidate.is_file():
                    return str(candidate)
            for candidate in root.rglob("makensis"):
                if candidate.is_file():
                    return str(candidate)
        except Exception:
            return None
        return None

    candidates = [
        str(_which_in_path("makensis", os.environ.get("PATH", ""))) if _which_in_path("makensis", os.environ.get("PATH",
                                                                                                                 ""))
        else None,
        str(_which_in_path("makensis.exe", os.environ.get("PATH", ""))) if _which_in_path("makensis.exe",
                                                                                          os.environ.get("PATH",
                                                                                                         "")) else None,
        "C:/Program Files (x86)/NSIS/makensis.exe",
        "C:/Program Files/NSIS/makensis.exe",
    ]
    for cand in candidates:
        if not cand:
            continue
        path = Path(cand)
        if path.exists():
            return str(path)

    search_roots: list[Path] = []
    local_app_data = os.environ.get("LOCALAPPDATA", "").strip()
    app_data = os.environ.get("APPDATA", "").strip()
    user_profile = os.environ.get("USERPROFILE", "").strip()
    if local_app_data:
        search_roots.extend([
            Path(local_app_data) / "tauri",
            Path(local_app_data) / "Tauri",
            Path(local_app_data) / "dioxus",
            Path(local_app_data) / "Dioxus",
            Path(local_app_data) / ".tauri",
        ])
    if app_data:
        search_roots.extend([
            Path(app_data) / "tauri",
            Path(app_data) / "Tauri",
            Path(app_data) / "dioxus",
            Path(app_data) / "Dioxus",
        ])
    if user_profile:
        search_roots.extend([
            Path(user_profile) / ".cache" / "tauri",
            Path(user_profile) / ".cache" / "dioxus",
        ])

    repo_root = Path(__file__).resolve().parent
    search_roots.extend([
        repo_root / "target" / ".tauri",
        repo_root / "target" / "dx",
    ])

    seen: set[Path] = set()
    for root in search_roots:
        if root in seen:
            continue
        seen.add(root)
        found = _recursive_find(root)
        if found:
            return found
    return None


def _resolve_iexpress() -> Optional[str]:
    candidates = [
        str(_which_in_path("iexpress", os.environ.get("PATH", ""))) if _which_in_path("iexpress", os.environ.get("PATH",
                                                                                                                 ""))
        else None,
        str(_which_in_path("iexpress.exe", os.environ.get("PATH", ""))) if _which_in_path("iexpress.exe",
                                                                                          os.environ.get("PATH",
                                                                                                         "")) else None,
        "C:/Windows/System32/iexpress.exe",
    ]
    for cand in candidates:
        if not cand:
            continue
        path = Path(cand)
        if path.exists():
            return str(path)
    return None


def _is_probable_windows_installer(path: Path) -> bool:
    lowered = path.name.lower()
    return (
            "installer" in lowered
            or "setup" in lowered
            or lowered.endswith(".msi")
            or lowered.startswith("uninstall")
    )


def cleanup_windows_installer_artifacts(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    if not dist.exists():
        return

    canonical = dist / _windows_installer_name()
    for item in sorted(dist.iterdir()):
        if item == canonical:
            continue
        lowered = item.name.lower()
        if item.suffix.lower() == ".msi" or "installer" in lowered or "setup" in lowered:
            print(f"Removing stale Windows installer artifact: {item.name}")
            _remove_path(item)


def prepare_windows_dist_for_bundle(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    if not dist.exists():
        return

    patterns = [
        "*.msi",
        "*setup.exe",
        f"{LEGACY_APP_NAME}*.exe",
        f"{LEGACY_APP_NAME}*.msi",
        f"{WINDOWS_APP_NAME}*.exe",
        f"{WINDOWS_APP_NAME}*.msi",
    ]
    seen: set[Path] = set()
    for pattern in patterns:
        for item in sorted(dist.glob(pattern)):
            if item in seen:
                continue
            seen.add(item)
            print(f"Removing pre-bundle Windows artifact: {item.name}")
            _remove_path(item)


def cleanup_linux_package_artifacts(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    if not dist.exists():
        return

    for item in sorted(dist.iterdir()):
        if item.suffix.lower() in {".deb", ".rpm", ".appimage", ".flatpak"} or ".pkg.tar." in item.name:
            print(f"Removing stale Linux package artifact: {item.name}")
            _remove_path(item)


def _windows_bundle_search_roots(
        frontend_dir: Path,
        rust_target: Optional[str],
        debug_mode: bool,
) -> list[Path]:
    target_root = frontend_dir.parent / "target"
    desktop_profile = "desktop-debug" if debug_mode else "desktop-release"
    profile = "debug" if debug_mode else "release"
    pkg_name = _frontend_package_name(frontend_dir)

    roots: list[Path] = [
        target_root / "dx" / pkg_name / profile / "windows",
        target_root / "dx" / pkg_name / profile / "bundle" / "windows",
        target_root / "dx" / pkg_name / "bundle" / "windows",
    ]

    dx_pkg_root = target_root / "dx" / pkg_name
    if dx_pkg_root.exists():
        for candidate in sorted(dx_pkg_root.glob("**/windows")):
            roots.append(candidate)

    if rust_target:
        roots.append(target_root / rust_target / desktop_profile)
    roots.append(target_root / desktop_profile)
    roots.append(dist_dir(frontend_dir))

    deduped: list[Path] = []
    seen: set[Path] = set()
    for root in roots:
        if root in seen:
            continue
        seen.add(root)
        deduped.append(root)
    return deduped


def _find_windows_app_exe(frontend_dir: Path, rust_target: Optional[str], debug_mode: bool) -> Path:
    preferred_names = [
        f"{WINDOWS_APP_NAME}.exe",
        f"{APP_NAME}.exe",
        f"{LEGACY_APP_NAME}.exe",
        "groundstation_frontend.exe",
    ]
    search_roots = _windows_bundle_search_roots(frontend_dir, rust_target, debug_mode)

    for root in search_roots:
        if not root.exists():
            continue
        for name in preferred_names:
            for candidate in root.rglob(name):
                if _is_probable_windows_installer(candidate):
                    continue
                return candidate

    raise FileNotFoundError(
        f"Could not find Windows app executable after bundle. Looked for {preferred_names} under: "
        + ", ".join(str(p) for p in search_roots)
    )


def _linux_architecture(rust_target: Optional[str]) -> tuple[str, str]:
    target = (rust_target or "").lower()
    if "aarch64" in target or "arm64" in target:
        return "arm64", "aarch64"
    if "armv7" in target or "armhf" in target:
        return "armhf", "armv7hl"
    return "amd64", "x86_64"


def _read_workspace_description(repo_root: Path) -> str:
    cargo_toml = repo_root / "Cargo.toml"
    raw = cargo_toml.read_text(encoding="utf-8")
    if tomllib is not None:
        data = tomllib.loads(raw)
        desc = data.get("workspace", {}).get("package", {}).get("description")
        if isinstance(desc, str) and desc.strip():
            return desc.strip()
    m = re.search(r'description\s*=\s*"([^"]+)"', raw)
    if m:
        return m.group(1)
    return "UBSEDS GroundStation"


def _find_linux_app_binary(frontend_dir: Path, rust_target: Optional[str], debug_mode: bool) -> Path:
    preferred_names = [
        LINUX_PACKAGE_NAME,
        APP_NAME,
        LEGACY_APP_NAME,
        "groundstation_frontend",
    ]
    target_root = frontend_dir.parent / "target"
    desktop_profile = "desktop-debug" if debug_mode else "desktop-release"
    effective_target = rust_target or _default_rust_target_for_frontend("linux")
    profile = "debug" if debug_mode else "release"
    pkg_name = _frontend_package_name(frontend_dir)
    search_roots: list[Path] = []
    search_roots.append(target_root / "dx" / pkg_name / profile / "linux" / "app")
    if effective_target:
        search_roots.append(target_root / effective_target / desktop_profile)
    search_roots.append(target_root / desktop_profile)
    search_roots.append(dist_dir(frontend_dir))

    for root in search_roots:
        if not root.exists():
            continue
        for name in preferred_names:
            for candidate in root.rglob(name):
                if candidate.is_file() and candidate.suffix.lower() not in {".deb", ".rpm", ".appimage"}:
                    return candidate

    raise FileNotFoundError(
        f"Could not find Linux app binary after bundle. Looked for {preferred_names} under: "
        + ", ".join(str(p) for p in search_roots)
    )


def _stage_linux_app_payload(
        frontend_dir: Path,
        rust_target: Optional[str],
        debug_mode: bool,
        *,
        appimage_mode: bool = False,
) -> tuple[tempfile.TemporaryDirectory, Path]:
    app_bin = _find_linux_app_binary(frontend_dir, rust_target, debug_mode)
    source_dir = app_bin.parent
    print(f"Staging Linux package payload from: {source_dir}")

    temp_dir = tempfile.TemporaryDirectory(prefix="gs26-linux-pkg-")
    pkg_root = Path(temp_dir.name) / "pkgroot"
    if appimage_mode:
        app_dir = pkg_root / "usr" / "bin"
        staged_bin = app_dir / LINUX_PACKAGE_NAME
    else:
        app_dir = pkg_root / "opt" / LINUX_PACKAGE_NAME
        staged_bin = app_dir / LINUX_PACKAGE_NAME
    app_dir.mkdir(parents=True, exist_ok=True)

    for item in sorted(source_dir.iterdir()):
        if item.is_dir():
            shutil.copytree(item, app_dir / item.name, dirs_exist_ok=True)
            continue
        if item.suffix.lower() in {".deb", ".rpm", ".appimage", ".pdb"}:
            continue
        if item == app_bin:
            shutil.copy2(item, staged_bin)
        else:
            shutil.copy2(item, app_dir / item.name)

    public_dir = frontend_dir / "dist" / "public"
    if public_dir.exists():
        for item in sorted(public_dir.iterdir()):
            target = app_dir / item.name
            if item.is_dir():
                shutil.copytree(item, target, dirs_exist_ok=True)
            else:
                shutil.copy2(item, target)

    bin_dir = pkg_root / "usr" / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    if not appimage_mode:
        launcher = bin_dir / LINUX_PACKAGE_NAME
        launcher.write_text(
            "\n".join([
                "#!/bin/sh",
                f'exec "/opt/{LINUX_PACKAGE_NAME}/{LINUX_PACKAGE_NAME}" "$@"',
                "",
            ]),
            encoding="utf-8",
        )
        launcher.chmod(0o755)

    applications_dir = pkg_root / "usr" / "share" / "applications"
    applications_dir.mkdir(parents=True, exist_ok=True)
    desktop_file = applications_dir / f"{LINUX_PACKAGE_NAME}.desktop"
    description = _read_workspace_description(frontend_dir.parent)
    desktop_file.write_text(
        "\n".join([
            "[Desktop Entry]",
            "Type=Application",
            f"Name={WINDOWS_APP_NAME}",
            f"Comment={description}",
            f"Exec={LINUX_PACKAGE_NAME}",
            f"Icon={LINUX_PACKAGE_NAME}",
            "Terminal=false",
            "Categories=Utility;",
            "",
        ]),
        encoding="utf-8",
    )

    pixmap_icon_src = frontend_dir / "icons" / "icon.png"
    sized_icon_src = frontend_dir / "icons" / "256x256.png"
    if not pixmap_icon_src.exists():
        pixmap_icon_src = frontend_dir / "assets" / "icon.png"
    if not sized_icon_src.exists():
        sized_icon_src = pixmap_icon_src

    if pixmap_icon_src.exists():
        pixmaps_dir = pkg_root / "usr" / "share" / "pixmaps"
        pixmaps_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(pixmap_icon_src, pixmaps_dir / f"{LINUX_PACKAGE_NAME}.png")

        icons_dir = pkg_root / "usr" / "share" / "icons" / "hicolor" / "256x256" / "apps"
        icons_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(sized_icon_src, icons_dir / f"{LINUX_PACKAGE_NAME}.png")

    return temp_dir, pkg_root


def _linux_pkg_arch(rust_target: Optional[str]) -> str:
    target = (rust_target or "").lower()
    if "aarch64" in target or "arm64" in target:
        return "aarch64"
    if "armv7" in target or "armhf" in target:
        return "armv7h"
    return "x86_64"


def _flatpak_arch(rust_target: Optional[str]) -> str:
    target = (rust_target or "").lower()
    if "aarch64" in target or "arm64" in target:
        return "aarch64"
    if "armv7" in target or "armhf" in target:
        return "arm"
    return "x86_64"


def _resolve_linuxdeploy() -> Optional[Path]:
    cache_candidate = Path.home() / ".cache" / "tauri" / "linuxdeploy-x86_64.AppImage"
    if cache_candidate.is_file():
        return cache_candidate
    binary = _which_in_path("linuxdeploy", os.environ.get("PATH", ""))
    if binary is not None:
        return Path(binary)
    return None


def _clone_tree(src: Path, dst: Path) -> None:
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(src, dst)


def _stage_windows_app_payload(
        frontend_dir: Path,
        rust_target: Optional[str],
        debug_mode: bool,
) -> tuple[tempfile.TemporaryDirectory, Path, Path]:
    app_exe = _find_windows_app_exe(frontend_dir, rust_target, debug_mode)
    source_dir = app_exe.parent
    print(f"Staging Windows installer payload from: {source_dir}")
    temp_dir = tempfile.TemporaryDirectory(prefix="gs26-win-installer-")
    stage_dir = Path(temp_dir.name) / "payload"
    stage_dir.mkdir(parents=True, exist_ok=True)

    for item in sorted(source_dir.iterdir()):
        if item == app_exe:
            shutil.copy2(item, stage_dir / f"{WINDOWS_APP_NAME}.exe")
            continue
        if _is_probable_windows_installer(item):
            continue
        if item.is_dir():
            shutil.copytree(item, stage_dir / item.name, dirs_exist_ok=True)
            continue
        if item.suffix.lower() == ".pdb":
            continue
        shutil.copy2(item, stage_dir / item.name)

    public_dir = frontend_dir / "dist" / "public"
    if public_dir.exists():
        for item in sorted(public_dir.iterdir()):
            target = stage_dir / item.name
            if item.is_dir():
                shutil.copytree(item, target, dirs_exist_ok=True)
            else:
                shutil.copy2(item, target)

    staged_exe = stage_dir / f"{WINDOWS_APP_NAME}.exe"
    if not staged_exe.exists():
        raise FileNotFoundError(f"Staged Windows app executable missing: {staged_exe}")

    return temp_dir, stage_dir, staged_exe


def _write_windows_nsis_script(
        script_path: Path,
        stage_dir: Path,
        icon_path: Path,
        installer_path: Path,
) -> None:
    script = f"""
Unicode True
SetCompressor /SOLID lzma
!include "MUI2.nsh"

Name "{WINDOWS_APP_NAME}"
OutFile "{installer_path}"
InstallDir "$LOCALAPPDATA\\{WINDOWS_APP_NAME}"
InstallDirRegKey HKCU "Software\\UBSEDS\\{WINDOWS_APP_NAME}" "InstallDir"
RequestExecutionLevel user
BrandingText "UBSEDS"

!define MUI_ABORTWARNING
!define MUI_ICON "{icon_path}"
!define MUI_UNICON "{icon_path}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "Install"
  SetOutPath "$INSTDIR"
  File /r "{stage_dir}\\*"
  WriteUninstaller "$INSTDIR\\Uninstall.exe"

  CreateDirectory "$SMPROGRAMS\\{WINDOWS_APP_NAME}"
  CreateShortcut "$SMPROGRAMS\\{WINDOWS_APP_NAME}\\{WINDOWS_APP_NAME}.lnk" "$INSTDIR\\{WINDOWS_APP_NAME}.exe"
  CreateShortcut "$SMPROGRAMS\\{WINDOWS_APP_NAME}\\Uninstall {WINDOWS_APP_NAME}.lnk" "$INSTDIR\\Uninstall.exe"

  WriteRegStr HKCU "Software\\UBSEDS\\{WINDOWS_APP_NAME}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{WINDOWS_APP_NAME}" "DisplayName" "
{WINDOWS_APP_NAME}"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{WINDOWS_APP_NAME}" "DisplayIcon" 
  "$INSTDIR\\{WINDOWS_APP_NAME}.exe"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{WINDOWS_APP_NAME}" "UninstallString" 
  "$INSTDIR\\Uninstall.exe"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{WINDOWS_APP_NAME}" "InstallLocation" 
  "$INSTDIR"
  WriteRegDWORD HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{WINDOWS_APP_NAME}" "NoModify" 1
  WriteRegDWORD HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{WINDOWS_APP_NAME}" "NoRepair" 1
SectionEnd

Section "Uninstall"
  Delete "$DESKTOP\\{WINDOWS_APP_NAME}.lnk"
  Delete "$SMPROGRAMS\\{WINDOWS_APP_NAME}\\{WINDOWS_APP_NAME}.lnk"
  Delete "$SMPROGRAMS\\{WINDOWS_APP_NAME}\\Uninstall {WINDOWS_APP_NAME}.lnk"
  RMDir "$SMPROGRAMS\\{WINDOWS_APP_NAME}"
  RMDir /r "$INSTDIR"
  DeleteRegKey HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{WINDOWS_APP_NAME}"
  DeleteRegKey HKCU "Software\\UBSEDS\\{WINDOWS_APP_NAME}"
SectionEnd
""".strip()
    script_path.write_text(script, encoding="utf-8")


def _write_windows_uninstall_script(script_path: Path) -> None:
    script = f"""
$ErrorActionPreference = "Stop"
$appName = "{WINDOWS_APP_NAME}"
$installDir = Split-Path -Parent $MyInvocation.MyCommand.Path

$startMenuDir = Join-Path $env:ProgramData "Microsoft\\Windows\\Start Menu\\Programs\\$appName"
if (Test-Path $startMenuDir) {{
    Remove-Item $startMenuDir -Recurse -Force
}}

$desktopShortcut = Join-Path ([Environment]::GetFolderPath("Desktop")) "$appName.lnk"
if (Test-Path $desktopShortcut) {{
    Remove-Item $desktopShortcut -Force
}}

$uninstallKey = "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName"
if (Test-Path $uninstallKey) {{
    Remove-Item $uninstallKey -Recurse -Force
}}

$vendorKey = "HKCU:\\Software\\UBSEDS\\$appName"
if (Test-Path $vendorKey) {{
    Remove-Item $vendorKey -Recurse -Force
}}

Set-Location ([System.IO.Path]::GetTempPath())
Remove-Item $installDir -Recurse -Force
""".strip()
    script_path.write_text(script, encoding="utf-8")


def _write_windows_install_script(script_path: Path) -> None:
    script = f"""
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Windows.Forms

$appName = "{WINDOWS_APP_NAME}"
$defaultInstallDir = Join-Path $env:LOCALAPPDATA $appName
$zipPath = Join-Path $PSScriptRoot "payload.zip"
$folderDialog = New-Object System.Windows.Forms.FolderBrowserDialog
$folderDialog.Description = "Choose install folder for $appName"
$folderDialog.SelectedPath = $defaultInstallDir
$dialogResult = $folderDialog.ShowDialog()
if ($dialogResult -ne [System.Windows.Forms.DialogResult]::OK -or [string]::IsNullOrWhiteSpace(
$folderDialog.SelectedPath)) {{
    throw "Installation cancelled"
}}
$installDir = $folderDialog.SelectedPath
$uninstallScript = Join-Path $installDir "uninstall.ps1"
$exePath = Join-Path $installDir "{WINDOWS_APP_NAME}.exe"
$startMenuDir = Join-Path $env:ProgramData "Microsoft\\Windows\\Start Menu\\Programs\\$appName"
$desktopShortcut = Join-Path ([Environment]::GetFolderPath("Desktop")) "$appName.lnk"

New-Item -ItemType Directory -Force -Path $installDir | Out-Null
Expand-Archive -LiteralPath $zipPath -DestinationPath $installDir -Force

$wsh = New-Object -ComObject WScript.Shell
New-Item -ItemType Directory -Force -Path $startMenuDir | Out-Null

$startShortcut = $wsh.CreateShortcut((Join-Path $startMenuDir "$appName.lnk"))
$startShortcut.TargetPath = $exePath
$startShortcut.WorkingDirectory = $installDir
$startShortcut.IconLocation = $exePath
$startShortcut.Save()

$desktop = $wsh.CreateShortcut($desktopShortcut)
$desktop.TargetPath = $exePath
$desktop.WorkingDirectory = $installDir
$desktop.IconLocation = $exePath
$desktop.Save()

New-Item -Path "HKCU:\\Software\\UBSEDS\\$appName" -Force | Out-Null
Set-ItemProperty -Path "HKCU:\\Software\\UBSEDS\\$appName" -Name "InstallDir" -Value $installDir

New-Item -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Force | Out-Null
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name "DisplayName" 
-Value $appName
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name "DisplayIcon" 
-Value $exePath
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name 
"InstallLocation" -Value $installDir
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name 
"UninstallString" -Value ("powershell.exe -ExecutionPolicy Bypass -File `"" + $uninstallScript + "`"")
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name "NoModify" 
-Type DWord -Value 1
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name "NoRepair" 
-Type DWord -Value 1
""".strip()
    script_path.write_text(script, encoding="utf-8")


def _write_windows_iexpress_sed(
        sed_path: Path,
        source_dir: Path,
        installer_path: Path,
) -> None:
    sed = f"""
[Version]
Class=IEXPRESS
SEDVersion=3

[Options]
PackagePurpose=InstallApp
ShowInstallProgramWindow=1
HideExtractAnimation=0
UseLongFileName=1
InsideCompressed=1
CAB_FixedSize=0
CAB_ResvCodeSigning=0
RebootMode=N
InstallPrompt=%InstallPrompt%
DisplayLicense=%DisplayLicense%
FinishMessage=%FinishMessage%
TargetName=%TargetName%
FriendlyName=%FriendlyName%
AppLaunched=%AppLaunched%
PostInstallCmd=<None>
AdminQuietInstCmd=%AdminQuietInstCmd%
UserQuietInstCmd=%UserQuietInstCmd%
SourceFiles=SourceFiles

[Strings]
InstallPrompt=Do you want to install {WINDOWS_APP_NAME}?
DisplayLicense=
FinishMessage={WINDOWS_APP_NAME} installation completed.
TargetName={installer_path}
FriendlyName={WINDOWS_APP_NAME} Installer
AppLaunched=cmd.exe /c powershell.exe -ExecutionPolicy Bypass -File install.ps1
AdminQuietInstCmd=cmd.exe /c powershell.exe -ExecutionPolicy Bypass -File install.ps1
UserQuietInstCmd=cmd.exe /c powershell.exe -ExecutionPolicy Bypass -File install.ps1
FILE0="payload.zip"
FILE1="install.ps1"

[SourceFiles]
SourceFiles0={source_dir}\\

[SourceFiles0]
%FILE0%=
%FILE1%=
""".strip()
    sed_path.write_text(sed, encoding="utf-8")


def build_manual_windows_installer(
        frontend_dir: Path,
        rust_target: Optional[str],
        debug_mode: bool,
) -> Path:
    makensis = _resolve_makensis()
    iexpress = _resolve_iexpress()
    if makensis is None and iexpress is None:
        raise FileNotFoundError(
            "Neither NSIS makensis nor Windows IExpress was found, so the Windows installer cannot be built."
        )

    icon_path = frontend_dir / "assets" / "icon.ico"
    if not icon_path.exists():
        raise FileNotFoundError(f"Windows installer icon not found: {icon_path}")

    dist = dist_dir(frontend_dir)
    dist.mkdir(parents=True, exist_ok=True)
    installer_path = dist / _windows_installer_name()

    cleanup_windows_installer_artifacts(frontend_dir)
    if installer_path.exists():
        print(f"Removing existing Windows installer artifact: {installer_path.name}")
        _remove_path(installer_path)

    temp_dir, stage_dir, _staged_exe = _stage_windows_app_payload(frontend_dir, rust_target, debug_mode)
    try:
        uninstall_script = stage_dir / "uninstall.ps1"
        _write_windows_uninstall_script(uninstall_script)

        if makensis is not None:
            script_path = Path(temp_dir.name) / "installer.nsi"
            _write_windows_nsis_script(script_path, stage_dir, icon_path, installer_path)
            run([makensis, str(script_path)], cwd=frontend_dir)
        else:
            source_dir = Path(temp_dir.name) / "iexpress-src"
            source_dir.mkdir(parents=True, exist_ok=True)
            payload_zip = source_dir / "payload.zip"
            install_script = source_dir / "install.ps1"
            _write_windows_install_script(install_script)
            shutil.make_archive(str(payload_zip.with_suffix("")), "zip", stage_dir)
            sed_path = Path(temp_dir.name) / "installer.sed"
            _write_windows_iexpress_sed(sed_path, source_dir, installer_path)
            run([iexpress, "/N", str(sed_path)], cwd=frontend_dir)
    finally:
        temp_dir.cleanup()

    if not installer_path.exists():
        raise FileNotFoundError(f"Manual Windows installer was not created: {installer_path}")

    cleanup_windows_installer_artifacts(frontend_dir)
    print(f"✅ Windows installer created: {installer_path}")
    return installer_path


def build_manual_linux_packages(
        frontend_dir: Path,
        rust_target: Optional[str],
        debug_mode: bool,
) -> None:
    dpkg_deb = _which_in_path("dpkg-deb", os.environ.get("PATH", ""))
    rpmbuild = _which_in_path("rpmbuild", os.environ.get("PATH", ""))
    if dpkg_deb is None and rpmbuild is None:
        print(
            "Warning: neither dpkg-deb nor rpmbuild was found; skipping manual .deb/.rpm packaging.",
            file=sys.stderr,
        )
        return

    cleanup_linux_package_artifacts(frontend_dir)
    temp_dir, base_pkg_root = _stage_linux_app_payload(frontend_dir, rust_target, debug_mode)
    try:
        version = _read_frontend_version(frontend_dir)
        description = _read_workspace_description(frontend_dir.parent)
        repo_url = "https://github.com/University-at-Buffalo-SEDS/GroundStation26"
        deb_arch, rpm_arch = _linux_architecture(rust_target)
        dist = dist_dir(frontend_dir)
        dist.mkdir(parents=True, exist_ok=True)

        if dpkg_deb is not None:
            deb_pkg_root = Path(temp_dir.name) / "deb-root"
            _clone_tree(base_pkg_root, deb_pkg_root)
            control = "\n".join([
                f"Package: {LINUX_PACKAGE_NAME}",
                f"Version: {version}",
                "Section: utils",
                "Priority: optional",
                f"Architecture: {deb_arch}",
                "Maintainer: UBSEDS",
                f"Description: {description}",
                "",
            ])
            debian_dir = deb_pkg_root / "DEBIAN"
            debian_dir.mkdir(parents=True, exist_ok=True)
            (debian_dir / "control").write_text(control, encoding="utf-8")
            deb_path = dist / f"{APP_NAME}_{deb_arch}.deb"
            run([str(dpkg_deb), "--build", "--root-owner-group", str(deb_pkg_root), str(deb_path)], cwd=frontend_dir)
            print(f"✅ Linux deb created: {deb_path}")
        else:
            print("Warning: dpkg-deb not found; skipping manual .deb packaging.", file=sys.stderr)

        if rpmbuild is not None:
            rpm_pkg_root = Path(temp_dir.name) / "rpm-root"
            _clone_tree(base_pkg_root, rpm_pkg_root)
            rpm_root = Path(temp_dir.name) / "rpmbuild"
            for dirname in ["BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"]:
                (rpm_root / dirname).mkdir(parents=True, exist_ok=True)

            spec_path = rpm_root / "SPECS" / f"{LINUX_PACKAGE_NAME}.spec"
            spec_path.write_text(
                "\n".join([
                    f"Name: {LINUX_PACKAGE_NAME}",
                    f"Version: {version}",
                    "Release: 1%{?dist}",
                    f"Summary: {WINDOWS_APP_NAME}",
                    "License: LicenseRef-UBSEDS",
                    f"URL: {repo_url}",
                    f"BuildArch: {rpm_arch}",
                    "AutoReqProv: no",
                    "",
                    "%description",
                    description,
                    "",
                    "%install",
                    "rm -rf %{buildroot}",
                    "mkdir -p %{buildroot}",
                    f"cp -a \"{rpm_pkg_root}\"/. %{{buildroot}}/",
                    "",
                    "%files",
                    f"/usr/bin/{LINUX_PACKAGE_NAME}",
                    f"/opt/{LINUX_PACKAGE_NAME}",
                    f"/usr/share/applications/{LINUX_PACKAGE_NAME}.desktop",
                    f"/usr/share/pixmaps/{LINUX_PACKAGE_NAME}.png",
                    f"/usr/share/icons/hicolor/256x256/apps/{LINUX_PACKAGE_NAME}.png",
                    "",
                ]),
                encoding="utf-8",
            )
            run(
                [
                    str(rpmbuild),
                    "-bb",
                    str(spec_path),
                    "--define",
                    f"_topdir {rpm_root}",
                    "--define",
                    "_build_id_links none",
                ],
                cwd=frontend_dir,
            )
            built_rpms = sorted((rpm_root / "RPMS" / rpm_arch).glob("*.rpm"))
            if not built_rpms:
                raise FileNotFoundError(f"Manual Linux rpm was not created under {(rpm_root / 'RPMS' / rpm_arch)}")
            rpm_path = dist / f"{APP_NAME}_{rpm_arch}.rpm"
            shutil.copy2(built_rpms[-1], rpm_path)
            print(f"✅ Linux rpm created: {rpm_path}")
        else:
            print("Warning: rpmbuild not found; skipping manual .rpm packaging.", file=sys.stderr)
    finally:
        temp_dir.cleanup()


def build_manual_appimage(
        frontend_dir: Path,
        rust_target: Optional[str],
        debug_mode: bool,
) -> None:
    linuxdeploy = _resolve_linuxdeploy()
    if linuxdeploy is None:
        print("Warning: linuxdeploy not found; skipping AppImage packaging.", file=sys.stderr)
        return

    temp_dir, base_pkg_root = _stage_linux_app_payload(
        frontend_dir,
        rust_target,
        debug_mode,
        appimage_mode=True,
    )
    try:
        appdir = Path(temp_dir.name) / "AppDir"
        appdir.mkdir(parents=True, exist_ok=True)
        for item in sorted(base_pkg_root.iterdir()):
            target = appdir / item.name
            if item.is_dir():
                shutil.copytree(item, target, dirs_exist_ok=True)
            else:
                shutil.copy2(item, target)

        env = os.environ.copy()
        env["APPIMAGE_EXTRACT_AND_RUN"] = "1"
        env["NO_STRIP"] = "1"
        desktop_file = appdir / "usr" / "share" / "applications" / f"{LINUX_PACKAGE_NAME}.desktop"
        icon_file = appdir / "usr" / "share" / "icons" / "hicolor" / "256x256" / "apps" / f"{LINUX_PACKAGE_NAME}.png"
        run(
            [
                str(linuxdeploy),
                "--appdir",
                str(appdir),
                "--desktop-file",
                str(desktop_file),
                "--icon-file",
                str(icon_file),
                "--output",
                "appimage",
            ],
            cwd=Path(temp_dir.name),
            env=env,
        )

        built = sorted(Path(temp_dir.name).glob("*.AppImage"))
        if not built:
            raise FileNotFoundError(f"Manual AppImage was not created under {temp_dir.name}")
        arch = _linux_pkg_arch(rust_target)
        dist = dist_dir(frontend_dir)
        dist.mkdir(parents=True, exist_ok=True)
        appimage_path = dist / f"{APP_NAME}_{arch}.AppImage"
        shutil.copy2(built[-1], appimage_path)
        print(f"✅ Linux AppImage created: {appimage_path}")
    finally:
        temp_dir.cleanup()


def build_manual_arch_package(
        frontend_dir: Path,
        rust_target: Optional[str],
        debug_mode: bool,
) -> None:
    makepkg = _which_in_path("makepkg", os.environ.get("PATH", ""))
    if makepkg is None:
        print("Warning: makepkg not found; skipping Arch package build.", file=sys.stderr)
        return

    temp_dir, base_pkg_root = _stage_linux_app_payload(frontend_dir, rust_target, debug_mode)
    try:
        version = _read_frontend_version(frontend_dir)
        description = _read_workspace_description(frontend_dir.parent)
        repo_url = "https://github.com/University-at-Buffalo-SEDS/GroundStation26"
        arch = _linux_pkg_arch(rust_target)
        build_root = Path(temp_dir.name) / "archpkg"
        src_root = build_root / "src"
        pkg_root = build_root / "pkgsrc"
        src_root.mkdir(parents=True, exist_ok=True)
        _clone_tree(base_pkg_root, pkg_root)

        pkgbuild = build_root / "PKGBUILD"
        pkgbuild.write_text(
            "\n".join([
                f"pkgname={LINUX_PACKAGE_NAME}",
                f"pkgver={version}",
                "pkgrel=1",
                f'pkgdesc="{description}"',
                f'arch=("{arch}")',
                f'url="{repo_url}"',
                'license=("custom")',
                'depends=("gtk3" "webkit2gtk-4.1")',
                "options=(!strip !debug)",
                "",
                "package() {",
                f'  cp -a "{pkg_root}/." "$pkgdir/"',
                "}",
                "",
            ]),
            encoding="utf-8",
        )

        dist = dist_dir(frontend_dir)
        dist.mkdir(parents=True, exist_ok=True)
        run(
            [
                str(makepkg),
                "--force",
                "--nodeps",
                "--cleanbuild",
                "--skippgpcheck",
                "--holdver",
            ],
            cwd=build_root,
        )

        built = sorted(build_root.glob(f"{LINUX_PACKAGE_NAME}-{version}-1-{arch}.pkg.tar.*"))
        if not built:
            raise FileNotFoundError(f"Manual Arch package was not created under {build_root}")
        built_pkg = built[-1]
        built_name = built_pkg.name
        suffix_start = built_name.find(".pkg.tar.")
        built_suffix = built_name[suffix_start:] if suffix_start != -1 else "".join(built_pkg.suffixes[-3:])
        pkg_path = dist / f"{APP_NAME}_{arch}{built_suffix}"
        shutil.copy2(built_pkg, pkg_path)
        print(f"✅ Linux Arch package created: {pkg_path}")
    finally:
        temp_dir.cleanup()


def build_manual_flatpak_package(
        frontend_dir: Path,
        rust_target: Optional[str],
        debug_mode: bool,
) -> None:
    flatpak = _which_in_path("flatpak", os.environ.get("PATH", ""))
    if flatpak is None:
        print("Warning: flatpak not found; skipping Flatpak packaging.", file=sys.stderr)
        return

    temp_dir, base_pkg_root = _stage_linux_app_payload(frontend_dir, rust_target, debug_mode)
    try:
        version = _read_frontend_version(frontend_dir)
        flatpak_arch = _flatpak_arch(rust_target)
        app_id = _bundle_identifier(frontend_dir)
        runtime = f"org.freedesktop.Platform/{flatpak_arch}/24.08"
        sdk = f"org.freedesktop.Sdk/{flatpak_arch}/24.08"
        branch = "stable"
        app_root = Path(temp_dir.name) / "flatpak-app"
        files_root = app_root / "files"
        export_repo = Path(temp_dir.name) / "flatpak-repo"
        files_root.mkdir(parents=True, exist_ok=True)

        usr_root = base_pkg_root / "usr"
        if usr_root.exists():
            for item in sorted(usr_root.iterdir()):
                target = files_root / item.name
                if item.is_dir():
                    shutil.copytree(item, target, dirs_exist_ok=True)
                else:
                    shutil.copy2(item, target)

        opt_root = base_pkg_root / "opt"
        if opt_root.exists():
            target = files_root / "opt"
            shutil.copytree(opt_root, target, dirs_exist_ok=True)

        metadata = app_root / "metadata"
        metadata.write_text(
            "\n".join([
                "[Application]",
                f"name={app_id}",
                f"runtime={runtime}",
                f"sdk={sdk}",
                f"command={LINUX_PACKAGE_NAME}",
                "",
                "[Context]",
                "shared=network;ipc;",
                "sockets=wayland;x11;fallback-x11;",
                "devices=dri;",
                "filesystems=home;",
                "",
            ]),
            encoding="utf-8",
        )

        run(
            [
                str(flatpak),
                "build-finish",
                "--allow=bluetooth",
                "--share=network",
                "--share=ipc",
                "--socket=wayland",
                "--socket=fallback-x11",
                "--socket=x11",
                "--device=dri",
                "--filesystem=home",
                str(app_root),
            ],
            cwd=frontend_dir,
        )
        run(
            [
                str(flatpak),
                "build-export",
                "--arch",
                flatpak_arch,
                str(export_repo),
                str(app_root),
                branch,
            ],
            cwd=frontend_dir,
        )

        dist = dist_dir(frontend_dir)
        dist.mkdir(parents=True, exist_ok=True)
        flatpak_path = dist / f"{APP_NAME}_{flatpak_arch}.flatpak"
        run(
            [
                str(flatpak),
                "build-bundle",
                str(export_repo),
                str(flatpak_path),
                app_id,
                branch,
                "--arch",
                flatpak_arch,
            ],
            cwd=frontend_dir,
        )
        if not flatpak_path.exists():
            raise FileNotFoundError(f"Manual Flatpak bundle was not created: {flatpak_path}")
        print(f"✅ Linux Flatpak created: {flatpak_path} (version {version})")
    finally:
        temp_dir.cleanup()


def rename_android_artifacts(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    if not dist.exists():
        return

    renamed_any = False
    for item in sorted(dist.iterdir()):
        name = item.name
        if not (
                name.startswith("app-debug")
                or name.startswith("app-release")
                or name.startswith(LEGACY_APP_NAME)
                or name.startswith(APP_NAME)
                or name.startswith(ANDROID_APP_NAME)
        ):
            continue

        if name.startswith("app-debug"):
            new_name = ANDROID_APP_NAME + name[len("app-debug"):]
        elif name.startswith("app-release"):
            new_name = ANDROID_APP_NAME + name[len("app-release"):]
        elif name.startswith(LEGACY_APP_NAME):
            new_name = ANDROID_APP_NAME + name[len(LEGACY_APP_NAME):]
        elif name.startswith(APP_NAME):
            new_name = ANDROID_APP_NAME + name[len(APP_NAME):]
        else:
            new_name = name

        if new_name != name:
            dst = dist / new_name
            print(f"Renaming android artifact: {name} -> {new_name}")
            _remove_path(dst)
            item.rename(dst)
            item = dst
            name = new_name
            renamed_any = True

        stripped = _strip_version_from_filename(name)
        stripped = _strip_android_target_suffix(stripped)
        if stripped != name:
            dst = dist / stripped
            print(f"Removing version from android artifact: {name} -> {stripped}")
            _remove_path(dst)
            item.rename(dst)
            renamed_any = True

    if not renamed_any:
        print("Warning: no android artifacts matched legacy name for rename.", file=sys.stderr)


def cleanup_android_dist_artifacts(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    if not dist.exists():
        return

    canonical_apk = f"{ANDROID_APP_NAME}.apk"
    for item in sorted(dist.iterdir()):
        if item.suffix in {".aab", ".apks"}:
            print(f"Removing android bundle artifact: {item.name}")
            _remove_path(item)
            continue
        if item.suffix == ".apk" and item.name != canonical_apk:
            print(f"Removing stale android apk artifact: {item.name}")
            _remove_path(item)


def _find_android_aab(frontend_dir: Path) -> Optional[Path]:
    dist = dist_dir(frontend_dir)
    if not dist.exists():
        return None
    candidates = sorted(dist.glob("*.aab"))
    return candidates[-1] if candidates else None


def _frontend_package_name(frontend_dir: Path) -> str:
    cargo_toml = frontend_dir / "Cargo.toml"
    if tomllib is not None and cargo_toml.exists():
        data = tomllib.loads(cargo_toml.read_text(encoding="utf-8"))
        pkg = data.get("package", {})
        name = pkg.get("name")
        if isinstance(name, str) and name.strip():
            return name.strip()
    return "groundstation_frontend"


def _generated_android_app_dir(frontend_dir: Path, debug_mode: bool) -> Path:
    profile = "debug" if debug_mode else "release"
    pkg_name = _frontend_package_name(frontend_dir)
    return frontend_dir.parent / "target" / "dx" / pkg_name / profile / "android" / "app"


def _clear_dioxus_bundle_identity_cache(
        frontend_dir: Path,
        rust_target: Optional[str],
        debug_mode: bool,
        platform_name: str,
) -> None:
    target_root = frontend_dir.parent / "target"
    profile = "debug" if debug_mode else "release"
    desktop_profile = "desktop-debug" if debug_mode else "desktop-release"
    pkg_name = _frontend_package_name(frontend_dir)
    removed: list[Path] = []

    def remove_path(path: Path) -> None:
        if not path.exists():
            return
        if path.is_dir():
            shutil.rmtree(path)
        else:
            path.unlink()
        removed.append(path)

    for path in [
        target_root / "dx" / pkg_name / profile / platform_name,
        target_root / "dx" / pkg_name / "bundle" / platform_name,
    ]:
        remove_path(path)

    target_dirs: list[Path] = []
    if rust_target:
        target_dirs.append(target_root / rust_target / desktop_profile)
    target_dirs.append(target_root / desktop_profile)

    for base in target_dirs:
        for pattern in [
            "deps/dioxus_cli_config-*",
            ".fingerprint/dioxus-cli-config-*",
            "deps/groundstation_frontend-*",
            ".fingerprint/groundstation_frontend-*",
        ]:
            for path in base.glob(pattern):
                remove_path(path)

    if removed:
        rel = ", ".join(str(p.relative_to(frontend_dir.parent)) for p in removed)
        print(f"Cleared stale {platform_name} Dioxus identity cache: {rel}")


def clear_generated_android_project(frontend_dir: Path, debug_mode: bool) -> None:
    project_dir = _generated_android_app_dir(frontend_dir, debug_mode)
    if project_dir.exists():
        print(f"Removing existing generated Android project: {project_dir}")
        last_error: Optional[OSError] = None
        for attempt in range(5):
            try:
                shutil.rmtree(project_dir)
                last_error = None
                break
            except OSError as exc:
                last_error = exc
                if exc.errno != errno.ENOTEMPTY:
                    raise
                if attempt == 4:
                    break
                time.sleep(0.2 * (attempt + 1))
        if project_dir.exists():
            shutil.rmtree(project_dir, ignore_errors=True)
        if project_dir.exists() and last_error is not None:
            raise last_error


def _merge_tree(src: Path, dst: Path) -> None:
    if not src.exists():
        return
    for path in src.rglob("*"):
        rel = path.relative_to(src)
        target = dst / rel
        if path.is_dir():
            target.mkdir(parents=True, exist_ok=True)
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)


def _kotlin_string_literal(value: str) -> str:
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def _keychain_password(service: str) -> Optional[str]:
    if platform.system() != "Darwin":
        return None
    try:
        out = subprocess.check_output(
            ["security", "find-generic-password", "-a", os.environ.get("USER", ""), "-s", service, "-w"],
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        return None
    value = out.decode("utf-8", errors="replace").strip()
    return value or None


def _android_signing_settings(frontend_dir: Path) -> dict[str, str]:
    repo_root = frontend_dir.parent
    defaults = {
        "ANDROID_KEYSTORE_PATH": str((Path.home() / "keys" / "groundstation-upload.jks").expanduser()),
        "ANDROID_KEY_ALIAS": "upload",
        "ANDROID_KEYSTORE_TYPE": "JKS",
    }
    settings: dict[str, str] = {}
    for key, default_value in defaults.items():
        value = os.environ.get(key, "").strip() or default_value
        settings[key] = value

    keystore_path = Path(settings["ANDROID_KEYSTORE_PATH"]).expanduser()
    if not keystore_path.is_absolute():
        keystore_path = (repo_root / keystore_path).resolve()
    settings["ANDROID_KEYSTORE_PATH"] = str(keystore_path)

    settings["ANDROID_KEYSTORE_PASSWORD"] = (
            os.environ.get("ANDROID_KEYSTORE_PASSWORD", "").strip()
            or _keychain_password("gs26-android-keystore-pass")
            or ""
    )
    settings["ANDROID_KEY_PASSWORD"] = (
            os.environ.get("ANDROID_KEY_PASSWORD", "").strip()
            or _keychain_password("gs26-android-key-pass")
            or settings["ANDROID_KEYSTORE_PASSWORD"]
    )
    return settings


def _android_sdk_levels() -> tuple[int, int, int]:
    min_sdk = int(os.environ.get("ANDROID_MIN_SDK", "24").strip() or "24")
    target_sdk = int(os.environ.get("ANDROID_TARGET_SDK", "35").strip() or "35")
    compile_sdk = int(os.environ.get("ANDROID_COMPILE_SDK", str(target_sdk)).strip() or str(target_sdk))
    return min_sdk, target_sdk, compile_sdk


def _configure_android_app_gradle(frontend_dir: Path, project_dir: Path) -> None:
    gradle_file = project_dir / "app" / "build.gradle.kts"
    if not gradle_file.exists():
        raise FileNotFoundError(f"Android app Gradle file not found: {gradle_file}")

    min_sdk, target_sdk, compile_sdk = _android_sdk_levels()
    version_name = _read_frontend_version(frontend_dir)
    version_code = _read_dioxus_build(frontend_dir)
    raw = gradle_file.read_text(encoding="utf-8")
    raw = re.sub(r"compileSdk\s*=\s*\d+", f"compileSdk = {compile_sdk}", raw)
    raw = re.sub(r"minSdk\s*=\s*\d+", f"minSdk = {min_sdk}", raw)
    raw = re.sub(r"targetSdk\s*=\s*\d+", f"targetSdk = {target_sdk}", raw)
    raw = re.sub(r'versionCode\s*=\s*\d+', f"versionCode = {int(version_code)}", raw)
    raw = re.sub(r'versionName\s*=\s*"[^"]+"', f'versionName = "{version_name}"', raw)
    gradle_file.write_text(raw, encoding="utf-8")
    print(
        "Configured Android app Gradle values: "
        f"minSdk={min_sdk}, targetSdk={target_sdk}, compileSdk={compile_sdk}, "
        f"versionCode={version_code}, versionName={version_name}"
    )


def _configure_android_signing(frontend_dir: Path, project_dir: Path) -> bool:
    settings = _android_signing_settings(frontend_dir)
    keystore_raw = settings["ANDROID_KEYSTORE_PATH"].strip()
    alias = settings["ANDROID_KEY_ALIAS"].strip()
    store_password = settings["ANDROID_KEYSTORE_PASSWORD"].strip()
    key_password = settings["ANDROID_KEY_PASSWORD"].strip() or store_password
    store_type = settings["ANDROID_KEYSTORE_TYPE"].strip()

    if not Path(keystore_raw).exists() and not any(
            os.environ.get(key, "").strip()
            for key in [
                "ANDROID_KEYSTORE_PATH",
                "ANDROID_KEY_ALIAS",
                "ANDROID_KEYSTORE_PASSWORD",
                "ANDROID_KEY_PASSWORD",
                "ANDROID_KEYSTORE_TYPE",
            ]
    ):
        return False

    missing: list[str] = []
    if not keystore_raw:
        missing.append("ANDROID_KEYSTORE_PATH")
    if not alias:
        missing.append("ANDROID_KEY_ALIAS")
    if not store_password:
        missing.append("ANDROID_KEYSTORE_PASSWORD")
    if not key_password:
        missing.append("ANDROID_KEY_PASSWORD")
    if missing:
        raise RuntimeError("Android signing is partially configured. Missing: " + ", ".join(missing))

    keystore_path = Path(keystore_raw).expanduser()
    if not keystore_path.is_absolute():
        keystore_path = (frontend_dir.parent / keystore_path).resolve()
    if not keystore_path.exists():
        raise FileNotFoundError(f"Android keystore not found: {keystore_path}")

    gradle_file = project_dir / "app" / "build.gradle.kts"
    if not gradle_file.exists():
        raise FileNotFoundError(f"Android app Gradle file not found: {gradle_file}")

    raw = gradle_file.read_text(encoding="utf-8")
    signing_block = "\n".join([
        "    signingConfigs {",
        "        create(\"release\") {",
        f"            storeFile = file({_kotlin_string_literal(str(keystore_path))})",
        f"            storePassword = {_kotlin_string_literal(store_password)}",
        f"            keyAlias = {_kotlin_string_literal(alias)}",
        f"            keyPassword = {_kotlin_string_literal(key_password)}",
        *([f"            storeType = {_kotlin_string_literal(store_type)}"] if store_type else []),
        "        }",
        "    }",
    ])
    if "signingConfigs {" not in raw:
        marker = "    buildTypes {\n"
        if marker not in raw:
            raise RuntimeError(f"Could not find Android buildTypes block in {gradle_file}")
        raw = raw.replace(marker, signing_block + "\n" + marker, 1)

    release_marker = "        getByName(\"release\") {\n"
    if release_marker not in raw:
        raise RuntimeError(f"Could not find Android release build type in {gradle_file}")
    release_block = "\n".join([
        release_marker.rstrip("\n"),
        "            signingConfig = signingConfigs.getByName(\"release\")",
    ]) + "\n"
    if "signingConfig = signingConfigs.getByName(\"release\")" not in raw:
        raw = raw.replace(release_marker, release_block, 1)

    gradle_file.write_text(raw, encoding="utf-8")
    print(f"Configured Android release signing with keystore: {keystore_path}")
    return True


def patch_generated_android_project(frontend_dir: Path, debug_mode: bool) -> Path:
    project_dir = _generated_android_app_dir(frontend_dir, debug_mode)
    app_src_main = project_dir / "app" / "src" / "main"
    if not app_src_main.exists():
        raise FileNotFoundError(f"Generated Android app sources not found: {app_src_main}")

    overlay_root = frontend_dir / "platform" / "android"
    for stale in [
        app_src_main / "java" / "com" / "ubseds" / "gs26",
        app_src_main / "kotlin" / "com" / "ubseds" / "gs26",
    ]:
        if stale.exists():
            shutil.rmtree(stale)

    manifest_src = overlay_root / "AndroidManifest.xml"
    if manifest_src.exists():
        shutil.copy2(manifest_src, app_src_main / "AndroidManifest.xml")

    _merge_tree(overlay_root / "res", app_src_main / "res")
    _merge_tree(overlay_root / "java", app_src_main / "java")
    _merge_tree(overlay_root / "kotlin", app_src_main / "kotlin")
    _ensure_android_icon_compat(frontend_dir, app_src_main)
    _configure_android_app_gradle(frontend_dir, project_dir)
    _configure_android_signing(frontend_dir, project_dir)
    proguard_src = overlay_root / "proguard-rules.pro"
    if proguard_src.exists():
        shutil.copy2(proguard_src, project_dir / "app" / "proguard-rules.pro")

    patch_script = frontend_dir.parent / "scripts" / "patch_android_webview_logging.py"
    if debug_mode and patch_script.exists():
        run([sys.executable, str(patch_script)], cwd=frontend_dir.parent)
    return project_dir


def rebuild_patched_android_bundle(frontend_dir: Path, debug_mode: bool, env: Optional[dict[str, str]]) -> Path:
    project_dir = patch_generated_android_project(frontend_dir, debug_mode)
    gradlew = project_dir / "gradlew"
    if not gradlew.exists():
        raise FileNotFoundError(f"Gradle wrapper not found: {gradlew}")

    task = "bundleDebug" if debug_mode else "bundleRelease"
    for stale_dir in [
        project_dir / "app" / "build",
        project_dir / "build",
    ]:
        if stale_dir.exists():
            print(f"Removing stale Android Gradle output: {stale_dir}")
            shutil.rmtree(stale_dir, ignore_errors=True)
    run([str(gradlew), task], cwd=project_dir, env=env)

    outputs_dir = project_dir / "app" / "build" / "outputs" / "bundle" / ("debug" if debug_mode else "release")
    bundles = sorted(outputs_dir.glob("*.aab"))
    if not bundles:
        raise FileNotFoundError(f"No rebuilt Android bundle found in {outputs_dir}")

    rebuilt = bundles[-1]
    dist = dist_dir(frontend_dir)
    dist.mkdir(parents=True, exist_ok=True)
    dst = dist / rebuilt.name
    shutil.copy2(rebuilt, dst)
    return dst


def _resolve_bundletool_command() -> Optional[list[str]]:
    bundletool_jar = os.environ.get("BUNDLETOOL_JAR", "").strip()
    if bundletool_jar:
        jar_path = Path(bundletool_jar).expanduser()
        if jar_path.is_file():
            return ["java", "-jar", str(jar_path)]

    bundletool_bin = shutil.which("bundletool")
    if bundletool_bin:
        return [bundletool_bin]

    return None


def build_android_universal_apk(frontend_dir: Path) -> Path:
    aab = _find_android_aab(frontend_dir)
    if aab is None:
        raise FileNotFoundError("No Android .aab artifact found in frontend/dist")

    bundletool_cmd = _resolve_bundletool_command()
    if bundletool_cmd is None:
        raise FileNotFoundError(
            "bundletool not found. Install bundletool or set BUNDLETOOL_JAR=/path/to/bundletool.jar"
        )

    dist = dist_dir(frontend_dir)
    apks_path = dist / f"{aab.stem}.apks"
    apk_path = dist / f"{aab.stem}.apk"

    cmd = bundletool_cmd + [
        "build-apks",
        f"--bundle={aab}",
        f"--output={apks_path}",
        "--mode=universal",
        "--overwrite",
    ]
    run(cmd, cwd=frontend_dir)

    with zipfile.ZipFile(apks_path, "r") as zf:
        apk_member = next((n for n in zf.namelist() if n.endswith("universal.apk")), None)
        if apk_member is None:
            raise RuntimeError(f"bundletool output did not contain universal.apk: {apks_path}")
        with zf.open(apk_member) as src, apk_path.open("wb") as raw_dst:
            dst = cast(BinaryIO, raw_dst)
            shutil.copyfileobj(src, dst)

    rename_android_artifacts(frontend_dir)
    cleanup_android_dist_artifacts(frontend_dir)
    final_apk = dist / f"{ANDROID_APP_NAME}.apk"
    if not final_apk.exists():
        final_apks = sorted(dist.glob("*.apk"))
        final_apk = final_apks[-1] if final_apks else apk_path
    print(f"✅ Android APK created: {final_apk}")
    return final_apk


def _resolve_adb(env: Optional[dict[str, str]] = None) -> str:
    merged = dict(os.environ)
    if env:
        merged.update(env)
    adb_path = _which_in_path("adb", str(merged.get("PATH", "")))
    adb = str(adb_path) if adb_path is not None else None
    if adb:
        return adb
    raise FileNotFoundError("adb not found on PATH")


def _list_adb_devices(frontend_dir: Path, env: Optional[dict[str, str]] = None) -> list[str]:
    adb = _resolve_adb(env)
    out = run_capture([adb, "devices"], cwd=frontend_dir, env=env)
    serials: list[str] = []
    for line in out.splitlines():
        line = line.strip()
        if not line or line.startswith("List of devices attached"):
            continue
        parts = line.split()
        if len(parts) >= 2 and parts[1] == "device":
            serials.append(parts[0])
    return serials


def install_android_apk(frontend_dir: Path, apk_path: Optional[Path] = None) -> tuple[str, Path]:
    env = _ensure_android_env(frontend_dir, None)
    devices = _list_adb_devices(frontend_dir, env)
    if not devices:
        raise RuntimeError("No Android emulator/device found. Start an emulator or connect a device first.")
    if len(devices) > 1:
        raise RuntimeError(f"Multiple Android devices found: {', '.join(devices)}. Leave only one connected.")

    adb = _resolve_adb(env)
    apk = apk_path
    if apk is None:
        candidates = sorted(dist_dir(frontend_dir).glob("*.apk"))
        apk = candidates[-1] if candidates else None
    if apk is None or not apk.exists():
        raise FileNotFoundError("No Android .apk artifact found in frontend/dist")

    serial = devices[0]
    run([adb, "-s", serial, "install", "-r", str(apk)], cwd=frontend_dir, env=env)
    print(f"✅ Installed Android APK on {serial}: {apk}")
    return serial, apk


def _bundle_identifier(frontend_dir: Path) -> str:
    dioxus_toml = frontend_dir / "Dioxus.toml"
    if not dioxus_toml.exists():
        raise FileNotFoundError(f"Dioxus.toml not found: {dioxus_toml}")
    if tomllib is None:
        raise RuntimeError("Python tomllib is required to read Dioxus.toml")
    with dioxus_toml.open("rb") as f:
        data = tomllib.load(f)
    bundle = data.get("bundle") or {}
    identifier = bundle.get("identifier")
    if not identifier:
        raise RuntimeError(f"bundle.identifier missing in {dioxus_toml}")
    return str(identifier)


def _sanitize_screenshot_stem(name: str) -> str:
    stem = re.sub(r"[^A-Za-z0-9._-]+", "_", name.strip())
    stem = stem.strip("._-")
    return stem or "screenshot"


def _parse_screenshot_delay(raw: Optional[str]) -> float:
    if raw is None:
        return 1.5
    try:
        value = float(raw)
    except ValueError as exc:
        raise RuntimeError(f"Invalid screenshot_delay '{raw}'") from exc
    if value < 0:
        raise RuntimeError("screenshot_delay must be >= 0")
    return value


def _resolve_screenshot_output_dir(repo_root: Path, raw: Optional[str]) -> Path:
    if raw:
        out = Path(raw)
        if not out.is_absolute():
            out = repo_root / out
    else:
        out = repo_root / "artifacts" / "screenshots"
    out.mkdir(parents=True, exist_ok=True)
    return out


def _parse_size_arg(raw: Optional[str], *, default: tuple[int, int], label: str) -> tuple[int, int]:
    if raw is None:
        return default
    m = re.fullmatch(r"\s*(\d{2,5})[xX](\d{2,5})\s*", raw)
    if not m:
        raise RuntimeError(f"Invalid {label} '{raw}', expected WIDTHxHEIGHT")
    width = int(m.group(1))
    height = int(m.group(2))
    if width < 200 or height < 200:
        raise RuntimeError(f"{label} must be at least 200x200")
    return width, height


def _write_screenshot_manifest(output_dir: Path, lines: list[str]) -> None:
    manifest = output_dir / "manifest.txt"
    manifest.write_text("\n".join(lines).strip() + "\n", encoding="utf-8")


def _osascript_capture(lines: list[str], cwd: Path) -> str:
    cmd = ["osascript"]
    for line in lines:
        cmd.extend(["-e", line])
    return run_capture(cmd, cwd=cwd).strip()


def _kill_app(process_name: str, cwd: Path) -> None:
    try:
        _osascript_capture([f'tell application "{process_name}" to quit'], cwd)
    except subprocess.CalledProcessError:
        pass


def _macos_bundle_info(app: Path) -> tuple[str, str]:
    plist_path = app / "Contents" / "Info.plist"
    if not plist_path.exists():
        raise FileNotFoundError(f"Info.plist not found in app bundle: {plist_path}")
    with plist_path.open("rb") as f:
        info = plistlib.load(f)
    bundle_id = info.get("CFBundleIdentifier")
    executable = info.get("CFBundleExecutable")
    if not bundle_id:
        raise RuntimeError(f"CFBundleIdentifier missing in {plist_path}")
    if not executable:
        raise RuntimeError(f"CFBundleExecutable missing in {plist_path}")
    return str(bundle_id), str(executable)


def _kill_macos_app(bundle_id: str, executable: str, cwd: Path) -> None:
    try:
        _osascript_capture([f'tell application id "{bundle_id}" to quit'], cwd)
    except subprocess.CalledProcessError:
        pass
    try:
        run(["pkill", "-x", executable], cwd=cwd)
    except subprocess.CalledProcessError:
        pass


def _open_macos_app_for_capture(frontend_dir: Path) -> tuple[Path, str, str, int]:
    if platform.system() != "Darwin":
        print("Error: macOS screenshot requires macOS.", file=sys.stderr)
        sys.exit(1)

    app = app_bundle_path(frontend_dir)
    if not app.exists():
        raise FileNotFoundError(f"App bundle not found: {app}")

    bundle_id, executable = _macos_bundle_info(app)
    _kill_macos_app(bundle_id, executable, frontend_dir)
    run(["open", "-na", str(app)], cwd=frontend_dir)
    pid = -1
    for _ in range(60):
        try:
            out = run_capture(["pgrep", "-n", "-x", executable], cwd=frontend_dir).strip()
            if out:
                pid = int(out.splitlines()[-1].strip())
                break
        except subprocess.CalledProcessError:
            pass
        time.sleep(0.1)
    if pid <= 0:
        raise RuntimeError(f"Timed out waiting for macOS app process '{executable}'")
    return app, bundle_id, executable, pid


def _resize_macos_capture_window(
        frontend_dir: Path,
        process_pid: int,
        *,
        window_size: tuple[int, int],
        origin: tuple[int, int] = (80, 80),
) -> tuple[int, int, int, int]:
    width, height = window_size
    origin_x, origin_y = origin
    bounds = _osascript_capture(
        [
            'tell application "System Events"',
            f'set targetProc to first process whose unix id is {process_pid}',
            'tell targetProc',
            'set frontmost to true',
            'repeat 60 times',
            'if (count of windows) > 0 then exit repeat',
            'delay 0.1',
            'end repeat',
            'if (count of windows) is 0 then error "App window did not appear in time"',
            f'set position of front window to {{{origin_x}, {origin_y}}}',
            f'set size of front window to {{{width}, {height}}}',
            'delay 0.15',
            'set winPos to position of front window',
            'set winSize to size of front window',
            'return (item 1 of winPos as string) & "," & (item 2 of winPos as string) & "," & (item 1 of winSize as '
            'string) & "," & (item 2 of winSize as string)',
            'end tell',
            'end tell',
        ],
        frontend_dir,
    )
    try:
        x_str, y_str, w_str, h_str = [part.strip() for part in bounds.split(",", 3)]
        return int(x_str), int(y_str), int(w_str), int(h_str)
    except Exception as exc:
        raise RuntimeError(f"Failed to parse macOS window bounds: {bounds}") from exc


def _capture_macos_window_region(frontend_dir: Path, output_path: Path, region: tuple[int, int, int, int]) -> None:
    x, y, w, h = region
    run(["screencapture", "-x", "-R", f"{x},{y},{w},{h}", str(output_path)], cwd=frontend_dir)


def _fit_preview_window_size(
        target_size: tuple[int, int],
        *,
        max_width: int = 1600,
        max_height: int = 1400,
) -> tuple[int, int]:
    target_width, target_height = target_size
    scale = min(max_width / target_width, max_height / target_height, 1.0)
    width = max(320, int(round(target_width * scale)))
    height = max(320, int(round(target_height * scale)))
    return width, height


def _capture_macos_app_content(
        frontend_dir: Path,
        output_path: Path,
        *,
        window_region: tuple[int, int, int, int],
        target_size: tuple[int, int],
        top_chrome_px: int = 32,
) -> None:
    x, y, w, h = window_region
    content_y = y + top_chrome_px
    content_h = h - top_chrome_px
    if content_h <= 0:
        raise RuntimeError(f"Invalid content height after cropping macOS chrome: {window_region}")

    with tempfile.TemporaryDirectory(prefix="gs26-shot-") as temp_dir_name:
        raw_path = Path(temp_dir_name) / "raw.png"
        _capture_macos_window_region(frontend_dir, raw_path, (x, content_y, w, content_h))
        target_width, target_height = target_size
        run(
            ["sips", "-z", str(target_height), str(target_width), str(raw_path), "--out", str(output_path)],
            cwd=frontend_dir,
        )


def _adb_shell_capture(frontend_dir: Path, serial: str, env: Optional[dict[str, str]], *args: str) -> str:
    adb = _resolve_adb(env)
    return run_capture([adb, "-s", serial, "shell", *args], cwd=frontend_dir, env=env)


def _android_override_value(wm_output: str, label: str) -> Optional[str]:
    pat = re.compile(rf"{label}:\s*([0-9x]+)", re.IGNORECASE)
    m = pat.search(wm_output)
    return m.group(1) if m else None


def _restore_android_screen_config(
        frontend_dir: Path,
        serial: str,
        env: Optional[dict[str, str]],
        *,
        prior_size_override: Optional[str],
        prior_density_override: Optional[str],
) -> None:
    adb = _resolve_adb(env)
    if prior_size_override:
        run([adb, "-s", serial, "shell", "wm", "size", prior_size_override], cwd=frontend_dir, env=env)
    else:
        run([adb, "-s", serial, "shell", "wm", "size", "reset"], cwd=frontend_dir, env=env)

    if prior_density_override:
        run([adb, "-s", serial, "shell", "wm", "density", prior_density_override], cwd=frontend_dir, env=env)
    else:
        run([adb, "-s", serial, "shell", "wm", "density", "reset"], cwd=frontend_dir, env=env)


def capture_android_screenshot(
        frontend_dir: Path,
        *,
        output_dir: Path,
        delay_seconds: float,
        filename_stem: Optional[str] = None,
        screen_size: Optional[tuple[int, int]] = None,
) -> Path:
    env = _ensure_android_env(frontend_dir, None)
    devices = _list_adb_devices(frontend_dir, env)
    if not devices:
        raise RuntimeError("No Android emulator/device found. Start an emulator or connect a device first.")
    if len(devices) > 1:
        raise RuntimeError(f"Multiple Android devices found: {', '.join(devices)}. Leave only one connected.")

    adb = _resolve_adb(env)
    serial = devices[0]
    package_id = _bundle_identifier(frontend_dir)
    remote_path = f"/sdcard/{_sanitize_screenshot_stem(filename_stem or package_id)}.png"
    local_name = f"{_sanitize_screenshot_stem(filename_stem or package_id)}.png"
    local_path = output_dir / local_name
    prior_size_override = _android_override_value(
        _adb_shell_capture(frontend_dir, serial, env, "wm", "size"),
        "Override size",
    )
    prior_density_override = _android_override_value(
        _adb_shell_capture(frontend_dir, serial, env, "wm", "density"),
        "Override density",
    )

    try:
        if screen_size:
            width, height = screen_size
            print(f"Setting Android capture size on {serial} to {width}x{height}")
            run([adb, "-s", serial, "shell", "wm", "size", f"{width}x{height}"], cwd=frontend_dir, env=env)

        print(f"Launching Android app on {serial}: {package_id}")
        run(
            [adb, "-s", serial, "shell", "monkey", "-p", package_id, "-c", "android.intent.category.LAUNCHER", "1"],
            cwd=frontend_dir,
            env=env,
        )
        if delay_seconds > 0:
            print(f"Waiting {delay_seconds:.1f}s before Android screenshot capture")
            time.sleep(delay_seconds)

        run([adb, "-s", serial, "shell", "rm", "-f", remote_path], cwd=frontend_dir, env=env)
        run([adb, "-s", serial, "shell", "screencap", "-p", remote_path], cwd=frontend_dir, env=env)
        run([adb, "-s", serial, "pull", remote_path, str(local_path)], cwd=frontend_dir, env=env)
        run([adb, "-s", serial, "shell", "rm", "-f", remote_path], cwd=frontend_dir, env=env)
    finally:
        _restore_android_screen_config(
            frontend_dir,
            serial,
            env,
            prior_size_override=prior_size_override,
            prior_density_override=prior_density_override,
        )

    print(f"Android screenshot saved: {local_path}")
    return local_path


def clear_app_bundle(frontend_dir: Path) -> None:
    dist = dist_dir(frontend_dir)
    bundles = [dist / APP_BUNDLE_NAME, dist / MACOS_ALT_APP_BUNDLE_NAME, dist / LEGACY_APP_BUNDLE_NAME]
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
    exe_str = os.fspath(exe)

    def _is_executable(path: Path) -> bool:
        try:
            return path.exists() and os.access(path, os.X_OK)
        except OSError:
            return False

    exes = [exe_str]
    if os.name == "nt":
        pathext = os.environ.get("PATHEXT", ".COM;.EXE;.BAT;.CMD")
        for ext in pathext.split(";"):
            ext = ext.strip()
            if not ext:
                continue
            if not ext.startswith("."):
                ext = f".{ext}"
            exes.append(f"{exe_str}{ext.lower()}")
            exes.append(f"{exe_str}{ext.upper()}")

    for raw_dir in path_value.split(os.pathsep):
        if not raw_dir:
            continue
        for name in exes:
            candidate = Path(raw_dir) / name
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


def _parse_semver_triplet(version: str) -> Optional[tuple[int, int, int]]:
    m = re.search(r"(\d+)\.(\d+)\.(\d+)", version)
    if not m:
        return None
    return int(m.group(1)), int(m.group(2)), int(m.group(3))


def _wasm_bindgen_cli_version(bin_path: Path, frontend_dir: Path, env: Optional[dict[str, str]]) -> Optional[str]:
    try:
        out = run_capture([str(bin_path), "--version"], cwd=frontend_dir, env=env).strip()
    except Exception:
        return None
    m = re.search(r"(\d+\.\d+\.\d+)", out)
    return m.group(1) if m else None


def _required_wasm_bindgen_cli_version(frontend_dir: Path) -> Optional[str]:
    # Explicit override wins.
    override = os.environ.get("GS_WASM_BINDGEN_CLI_VERSION", "").strip()
    if override:
        return override

    lock_path = frontend_dir.parent / "Cargo.lock"
    if not lock_path.exists():
        return None

    raw = lock_path.read_text(encoding="utf-8", errors="replace")
    versions: list[str] = []

    if tomllib is not None:
        try:
            data = tomllib.loads(raw)
            for pkg in data.get("package", []):
                if pkg.get("name") == "wasm-bindgen":
                    v = str(pkg.get("version", "")).strip()
                    if v:
                        versions.append(v)
        except Exception:
            pass

    if not versions:
        # Fallback parser for lockfile text if tomllib is unavailable/failed.
        blocks = raw.split("[[package]]")
        for block in blocks:
            if 'name = "wasm-bindgen"' not in block:
                continue
            m = re.search(r'version\s*=\s*"([^"]+)"', block)
            if m:
                versions.append(m.group(1))

    if not versions:
        return None

    versions = sorted(
        set(versions),
        key=lambda v: (_parse_semver_triplet(v) or (0, 0, 0), v),
    )
    return versions[-1]


def _ensure_wasm_bindgen_cli(frontend_dir: Path, env: Optional[dict[str, str]]) -> Optional[Path]:
    required = _required_wasm_bindgen_cli_version(frontend_dir)
    path_value = (env or {}).get("PATH", os.environ.get("PATH", ""))
    installed_path = _find_wasm_bindgen(path_value)

    if required is None:
        return installed_path

    installed_version = (
        _wasm_bindgen_cli_version(installed_path, frontend_dir, env) if installed_path else None
    )

    if installed_version == required and installed_path is not None:
        return installed_path

    if installed_path is None:
        print(f"wasm-bindgen-cli not found; installing required version {required}")
        run(
            ["cargo", "install", "--locked", "wasm-bindgen-cli", "--version", required],
            cwd=frontend_dir.parent,
            env=env,
        )
    else:
        print(
            f"wasm-bindgen-cli version mismatch (have {installed_version or 'unknown'}, "
            f"want {required}); reinstalling"
        )
        run(
            ["cargo", "install", "--locked", "wasm-bindgen-cli", "--version", required, "--force"],
            cwd=frontend_dir.parent,
            env=env,
        )

    refreshed_path = _find_wasm_bindgen((env or {}).get("PATH", os.environ.get("PATH", "")))
    if refreshed_path is None:
        raise RuntimeError("wasm-bindgen-cli install completed but `wasm-bindgen` is still not on PATH")
    return refreshed_path


def _tail_log_text(max_bytes: int = 131072) -> str:
    if LOG_FILE is None or not LOG_FILE.exists():
        return ""
    try:
        size = LOG_FILE.stat().st_size
        with LOG_FILE.open("rb") as f:
            if size > max_bytes:
                f.seek(size - max_bytes)
            data = f.read()
        return data.decode("utf-8", errors="replace")
    except OSError:
        return ""


def _extract_wasm_bindgen_version_hint(text: str) -> Optional[str]:
    patterns = [
        r"wasm-bindgen(?:-cli)?[^\d]*(\d+\.\d+\.\d+)",
        r"update to [`'\"]?wasm-bindgen[`'\"]?\s+v?(\d+\.\d+\.\d+)",
        r"requires wasm-bindgen(?:-cli)?\s+v?(\d+\.\d+\.\d+)",
    ]
    for pat in patterns:
        m = re.search(pat, text, re.IGNORECASE)
        if m:
            return m.group(1)
    return None


def _looks_like_wasm_bindgen_failure(text: str) -> bool:
    low = text.lower()
    if "wasm-bindgen" not in low:
        return False
    signals = [
        "version",
        "mismatch",
        "incompatible",
        "please update",
        "older versions",
        "out of date",
    ]
    return any(s in low for s in signals)


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
    if os.name == "nt":
        candidates.extend(
            [
                Path(str(Path.home() / ".cargo" / "bin" / "dx.exe")),
                Path(str(Path.home() / ".cargo" / "bin" / "dx.cmd")),
                Path(str(Path.home() / ".cargo" / "bin" / "dx.bat")),
            ]
        )
    for cand in candidates:
        try:
            is_executable = cand.exists() and os.access(cand, os.X_OK)
        except OSError:
            is_executable = False
        if is_executable:
            return cand
    return None


def _npm_global_bin_dir(cwd: Path) -> Optional[Path]:
    try:
        prefix = subprocess.check_output(
            ["npm", "prefix", "-g"],
            cwd=cwd,
            env=os.environ,
            text=True,
        ).strip()
    except Exception:
        return None
    if not prefix:
        return None
    bin_dir = Path(prefix) / "bin"
    return bin_dir if bin_dir.exists() else None


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
    Dioxus can reuse cached web/public asset dirs from both crate-local and
    workspace-root target/ paths, which may repopulate stale hashed assets
    into dist/public on rebuild.
    """
    workspace_root = frontend_dir.parent
    target_dirs = {
        frontend_dir / "target",
        workspace_root / "target",
    }

    removed = 0
    for target_dir in target_dirs:
        if not target_dir.exists():
            continue
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
        str(frontend_dir / "node_modules" / ".bin"),
        "/usr/local/sbin",
        "/usr/local/bin",
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
        "/opt/binaryen/bin",
    ]
    npm_global_bin = _npm_global_bin_dir(frontend_dir)
    if npm_global_bin is not None:
        extra_paths.insert(1, str(npm_global_bin))

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

    app = _stage_app_bundle_from_dx(
        frontend_dir,
        platform_name="ios",
        preferred_bundle_name=APP_BUNDLE_NAME,
    ) or app_bundle_path(frontend_dir)
    if not app.exists():
        raise FileNotFoundError(f"App bundle not found: {app}")

    patch_plist(frontend_dir, app)

    profile = fixed_mobileprovision_path(frontend_dir)

    signer = frontend_dir / "scripts" / "ios_package_sign.sh"
    if not signer.exists():
        raise FileNotFoundError(f"Missing signer script: {signer}")

    dist = frontend_dir / "dist"
    ipas_dir = frontend_dir / "dist" / "ipas"
    ipas_dir.mkdir(parents=True, exist_ok=True)

    for stale_ipa in sorted(dist.glob("*.ipa")):
        print(f"Removing stale IPA artifact: {stale_ipa}")
        stale_ipa.unlink()

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

    app = _stage_app_bundle_from_dx(
        frontend_dir,
        platform_name="ios",
        preferred_bundle_name=APP_BUNDLE_NAME,
    ) or app_bundle_path(frontend_dir)
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


def capture_ios_sim_screenshot(
        frontend_dir: Path,
        *,
        output_dir: Path,
        delay_seconds: float,
        filename_stem: Optional[str] = None,
) -> Path:
    if platform.system() != "Darwin":
        print("Error: iOS simulator screenshot requires macOS.", file=sys.stderr)
        sys.exit(1)

    app = _stage_app_bundle_from_dx(
        frontend_dir,
        platform_name="ios",
        preferred_bundle_name=APP_BUNDLE_NAME,
    ) or app_bundle_path(frontend_dir)
    if not app.exists():
        raise FileNotFoundError(f"Simulator app bundle not found: {app}")

    udid, bundle_id = ios_sim_deploy(frontend_dir)
    if delay_seconds > 0:
        print(f"Waiting {delay_seconds:.1f}s before iOS simulator screenshot capture")
        time.sleep(delay_seconds)

    local_name = f"{_sanitize_screenshot_stem(filename_stem or bundle_id)}.png"
    local_path = output_dir / local_name
    run(["xcrun", "simctl", "io", udid, "screenshot", str(local_path)], cwd=frontend_dir)
    print(f"iOS simulator screenshot saved: {local_path}")
    return local_path


def capture_macos_screenshot(
        frontend_dir: Path,
        *,
        output_dir: Path,
        delay_seconds: float,
        filename_stem: Optional[str] = None,
        window_size: tuple[int, int] = (1440, 900),
) -> Path:
    _app, bundle_id, executable, process_pid = _open_macos_app_for_capture(frontend_dir)
    local_name = f"{_sanitize_screenshot_stem(filename_stem or 'macos')}.png"
    local_path = output_dir / local_name
    preview_size = _fit_preview_window_size(window_size)
    bounds = _resize_macos_capture_window(frontend_dir, process_pid, window_size=preview_size)
    if delay_seconds > 0:
        print(f"Waiting {delay_seconds:.1f}s before macOS screenshot capture")
        time.sleep(delay_seconds)
    _capture_macos_app_content(frontend_dir, local_path, window_region=bounds, target_size=window_size)
    _kill_macos_app(bundle_id, executable, frontend_dir)
    print(f"macOS screenshot saved: {local_path}")
    return local_path


def capture_publisher_screenshots(
        frontend_dir: Path,
        *,
        debug_mode: bool,
        max_size_mode: bool,
        use_existing: bool,
        output_dir: Path,
        delay_seconds: float,
        desktop_window_size: tuple[int, int],
        ios_window_size: tuple[int, int],
        android_window_size: tuple[int, int],
) -> list[Path]:
    if platform.system() != "Darwin":
        raise RuntimeError("publisher_screenshots currently requires macOS because it captures the macOS app window.")

    desktop_dir = output_dir / "desktop"
    ios_dir = output_dir / "ios"
    android_dir = output_dir / "android"
    for path in (desktop_dir, ios_dir, android_dir):
        path.mkdir(parents=True, exist_ok=True)

    if not use_existing:
        build_frontend(
            frontend_dir,
            platform_name="macos",
            rust_target=None,
            debug_mode=debug_mode,
            max_size=max_size_mode,
        )
    _app, bundle_id, executable, process_pid = _open_macos_app_for_capture(frontend_dir)
    captures = [
        ("desktop", desktop_dir / "publisher-desktop.png", desktop_window_size),
        ("ios-phone", ios_dir / "publisher-ios-phone.png", ios_window_size),
        ("ios-tablet", ios_dir / "publisher-ios-tablet.png", (2048, 2732)),
        ("android-phone", android_dir / "publisher-android-phone.png", android_window_size),
        ("android-tablet7", android_dir / "publisher-android-tablet7.png", (1600, 2560)),
        ("android-tablet10", android_dir / "publisher-android-tablet10.png", (1920, 3072)),
    ]
    results: list[Path] = []
    try:
        for label, output_path, target_size in captures:
            preview_size = _fit_preview_window_size(target_size)
            print(f"Capturing {label} publisher screenshot at {target_size[0]}x{target_size[1]}")
            bounds = _resize_macos_capture_window(frontend_dir, process_pid, window_size=preview_size)
            if delay_seconds > 0:
                print(f"Waiting {delay_seconds:.1f}s before {label} screenshot capture")
                time.sleep(delay_seconds)
            _capture_macos_app_content(frontend_dir, output_path, window_region=bounds, target_size=target_size)
            results.append(output_path)
    finally:
        _kill_macos_app(bundle_id, executable, frontend_dir)

    manifest_lines = [
        f"desktop={results[0]}",
        f"ios_phone={results[1]}",
        f"ios_tablet={results[2]}",
        f"android_phone={results[3]}",
        f"android_tablet7={results[4]}",
        f"android_tablet10={results[5]}",
        f"desktop_window={desktop_window_size[0]}x{desktop_window_size[1]}",
        f"ios_window={ios_window_size[0]}x{ios_window_size[1]}",
        f"android_window={android_window_size[0]}x{android_window_size[1]}",
        "ios_tablet_window=2048x2732",
        "android_tablet7_window=1600x2560",
        "android_tablet10_window=1920x3072",
        f"delay_seconds={delay_seconds:.1f}",
    ]
    _write_screenshot_manifest(output_dir, manifest_lines)
    return results


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
    return os.environ.get("GS26_WINDOWS_TARGET", "x86_64-pc-windows-msvc").strip()


def _detect_android_sdk_root() -> Optional[Path]:
    candidates = [
        os.environ.get("ANDROID_SDK_ROOT", "").strip(),
        os.environ.get("ANDROID_HOME", "").strip(),
        str(Path.home() / "Library" / "Android" / "sdk"),
        str(Path.home() / "Android" / "Sdk"),
        "/Library/Android/sdk",
    ]
    for candidate in candidates:
        if not candidate:
            continue
        path = Path(candidate).expanduser()
        if path.is_dir():
            return path
    return None


def _detect_android_ndk_root(sdk_root: Optional[Path]) -> Optional[Path]:
    env_candidates = [
        os.environ.get("ANDROID_NDK_ROOT", "").strip(),
        os.environ.get("ANDROID_NDK_HOME", "").strip(),
        os.environ.get("NDK_HOME", "").strip(),
    ]
    for candidate in env_candidates:
        if not candidate:
            continue
        path = Path(candidate).expanduser()
        if path.is_dir():
            return path

    if sdk_root is None:
        return None

    versioned_ndk = sdk_root / "ndk"
    if versioned_ndk.is_dir():
        versions = sorted((p for p in versioned_ndk.iterdir() if p.is_dir()), key=lambda p: p.name)
        if versions:
            return versions[-1]

    ndk_bundle = sdk_root / "ndk-bundle"
    if ndk_bundle.is_dir():
        return ndk_bundle

    return None


def _android_tool_paths(sdk_root: Path, ndk_root: Optional[Path]) -> list[Path]:
    paths: list[Path] = []
    for rel in [
        Path("platform-tools"),
        Path("emulator"),
        Path("cmdline-tools/latest/bin"),
        Path("cmdline-tools/bin"),
        Path("tools/bin"),
        Path("build-tools"),
    ]:
        base = sdk_root / rel
        if base.is_dir():
            if rel == Path("build-tools"):
                versions = sorted((p for p in base.iterdir() if p.is_dir()), key=lambda p: p.name)
                if versions:
                    paths.append(versions[-1])
            else:
                paths.append(base)

    if ndk_root is not None:
        for rel in [Path("toolchains/llvm/prebuilt/darwin-x86_64/bin"),
                    Path("toolchains/llvm/prebuilt/darwin-arm64/bin")]:
            base = ndk_root / rel
            if base.is_dir():
                paths.append(base)
    return paths


def _ensure_android_env(frontend_dir: Path, env: Optional[dict[str, str]]) -> dict[str, str]:
    merged = dict(env or os.environ.copy())
    sdk_root = _detect_android_sdk_root()
    if sdk_root is None:
        print(
            "Warning: Android SDK not found. Set ANDROID_SDK_ROOT or install it under ~/Library/Android/sdk.",
            file=sys.stderr,
        )
        return merged

    ndk_root = _detect_android_ndk_root(sdk_root)
    merged["ANDROID_SDK_ROOT"] = str(sdk_root)
    merged["ANDROID_HOME"] = str(sdk_root)
    if ndk_root is not None:
        merged["ANDROID_NDK_ROOT"] = str(ndk_root)
        merged["ANDROID_NDK_HOME"] = str(ndk_root)
        merged["NDK_HOME"] = str(ndk_root)
    else:
        print(
            f"Warning: Android NDK not found under {sdk_root}. Native Android builds may fail until it is installed.",
            file=sys.stderr,
        )

    tool_paths = [str(p) for p in _android_tool_paths(sdk_root, ndk_root)]
    if tool_paths:
        merged["PATH"] = os.pathsep.join(tool_paths + [merged.get("PATH", "")]).rstrip(os.pathsep)

    print(f"Using Android SDK: {sdk_root}")
    if ndk_root is not None:
        print(f"Using Android NDK: {ndk_root}")
    return merged


def _default_rust_target_for_frontend(platform_name: Optional[str]) -> Optional[str]:
    if platform_name is None or platform_name == "web":
        return None
    if platform_name == "macos":
        return _host_macos_target()
    if platform_name == "windows":
        return _windows_target_default()
    if platform_name == "linux":
        machine = platform.machine().lower()
        if machine in {"x86_64", "amd64"}:
            return "x86_64-unknown-linux-gnu"
        if machine in {"aarch64", "arm64"}:
            return "aarch64-unknown-linux-gnu"
        if machine.startswith("armv7"):
            return "armv7-unknown-linux-gnueabihf"
    return None


def _ensure_windows_icon_compat(frontend_dir: Path) -> None:
    """
    Some dx/windows bundle paths resolve to legacy/default icon locations.
    Create compatibility paths so canonicalize does not fail on Windows.
    """
    src_png = frontend_dir / "assets" / "icon.png"
    if not src_png.exists():
        print(f"Warning: Windows icon source not found: {src_png}", file=sys.stderr)
        return

    icons_dir = frontend_dir / "icons"
    icons_dir.mkdir(parents=True, exist_ok=True)
    src_ico = frontend_dir / "assets" / "icon.ico"
    dst_ico = icons_dir / "icon.ico"

    if src_ico.exists():
        if not dst_ico.exists():
            dst_ico.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src_ico, dst_ico)
        return

    if src_ico.exists() or dst_ico.exists():
        return

    try:
        from PIL import Image  # type: ignore

        img = Image.open(src_png)
        # Include common Windows icon sizes.
        sizes = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]
        img.save(dst_ico, format="ICO", sizes=sizes)
        generated = True
    except Exception:
        generated = False

    if not generated:
        # Last-resort fallback if PIL is unavailable.
        # This may not produce a valid ICO for all tooling.
        shutil.copy2(src_png, dst_ico)
        print(
            "Warning: Pillow not available; copied PNG bytes to icon.ico. "
            "Install Pillow for a proper Windows icon.",
            file=sys.stderr,
        )


def _ensure_bundle_icon_compat(frontend_dir: Path) -> None:
    src_png = frontend_dir / "assets" / "icon.png"
    if not src_png.exists():
        print(f"Warning: bundle icon source not found: {src_png}", file=sys.stderr)
        return

    icons_dir = frontend_dir / "icons"
    icons_dir.mkdir(parents=True, exist_ok=True)

    src_ico = frontend_dir / "assets" / "icon.ico"
    dst_ico = icons_dir / "icon.ico"
    if src_ico.exists() and not dst_ico.exists():
        shutil.copy2(src_ico, dst_ico)

    try:
        from PIL import Image  # type: ignore
    except Exception:
        print(
            "Warning: Pillow not available; cannot generate desktop bundle icon set.",
            file=sys.stderr,
        )
        fallback_targets = [
            icons_dir / "32x32.png",
            icons_dir / "64x64.png",
            icons_dir / "128x128.png",
            icons_dir / "128x128@2x.png",
            icons_dir / "256x256.png",
            icons_dir / "512x512.png",
            icons_dir / "icon.png",
        ]
        for target in fallback_targets:
            if not target.exists():
                shutil.copy2(src_png, target)
        return

    try:
        img = Image.open(src_png).convert("RGBA")
    except Exception as exc:
        print(f"Warning: failed to open bundle icon source {src_png}: {exc}", file=sys.stderr)
        return

    icon_targets = {
        "32x32.png": 32,
        "64x64.png": 64,
        "128x128.png": 128,
        "128x128@2x.png": 256,
        "256x256.png": 256,
        "512x512.png": 512,
        "icon.png": 512,
    }
    for filename, size in icon_targets.items():
        target = icons_dir / filename
        if target.exists():
            continue
        img.resize((size, size), Image.LANCZOS).save(target, format="PNG")

    if not dst_ico.exists():
        try:
            img.save(dst_ico, format="ICO",
                     sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
        except Exception as exc:
            print(f"Warning: failed generating bundle icon ICO {dst_ico}: {exc}", file=sys.stderr)

    dst_icns = icons_dir / "icon.icns"
    if not dst_icns.exists() and platform.system() == "Darwin":
        iconutil = shutil.which("iconutil")
        if iconutil is None:
            print("Warning: iconutil not available; cannot generate macOS .icns icon.", file=sys.stderr)
            return

        with tempfile.TemporaryDirectory(prefix="gs26-iconset-") as tmp:
            iconset_dir = Path(tmp) / "AppIcon.iconset"
            iconset_dir.mkdir(parents=True, exist_ok=True)
            macos_icon_targets = {
                "icon_16x16.png": 16,
                "icon_16x16@2x.png": 32,
                "icon_32x32.png": 32,
                "icon_32x32@2x.png": 64,
                "icon_128x128.png": 128,
                "icon_128x128@2x.png": 256,
                "icon_256x256.png": 256,
                "icon_256x256@2x.png": 512,
                "icon_512x512.png": 512,
                "icon_512x512@2x.png": 1024,
            }
            for filename, size in macos_icon_targets.items():
                img.resize((size, size), Image.LANCZOS).save(iconset_dir / filename, format="PNG")
            try:
                run([iconutil, "-c", "icns", str(iconset_dir), "-o", str(dst_icns)], cwd=frontend_dir)
            except subprocess.CalledProcessError as exc:
                print(f"Warning: failed generating macOS icon {dst_icns}: {exc}", file=sys.stderr)


def _patch_macos_bundle_icon(frontend_dir: Path) -> None:
    app = rename_macos_app_bundle(frontend_dir) or app_bundle_path(frontend_dir)
    if not app.exists():
        return

    icon_src = frontend_dir / "icons" / "icon.icns"
    if not icon_src.exists():
        return

    resources_dir = app / "Contents" / "Resources"
    resources_dir.mkdir(parents=True, exist_ok=True)
    icon_dst = resources_dir / "icon.icns"
    if not icon_dst.exists() or icon_src.read_bytes() != icon_dst.read_bytes():
        shutil.copy2(icon_src, icon_dst)

    plist_path = app / "Contents" / "Info.plist"
    if not plist_path.exists():
        return

    with plist_path.open("rb") as f:
        info = plistlib.load(f)

    if info.get("CFBundleIconFile") != "icon.icns":
        info["CFBundleIconFile"] = "icon.icns"
        with plist_path.open("wb") as f:
            plistlib.dump(info, f, sort_keys=False)


def _ensure_android_icon_compat(frontend_dir: Path, app_src_main: Path) -> None:
    src_png = frontend_dir / "assets" / "icon_1024x1024.png"
    if not src_png.exists():
        src_png = frontend_dir / "assets" / "icon.png"
    if not src_png.exists():
        print(f"Warning: Android icon source not found: {src_png}", file=sys.stderr)
        return

    try:
        from PIL import Image  # type: ignore
    except Exception:
        print(
            "Warning: Pillow not available; leaving generated Android launcher icon unchanged.",
            file=sys.stderr,
        )
        return

    try:
        img = Image.open(src_png).convert("RGBA")
    except Exception as exc:
        print(f"Warning: failed to open Android icon source {src_png}: {exc}", file=sys.stderr)
        return

    mipmap_sizes = {
        "mipmap-mdpi": 48,
        "mipmap-hdpi": 72,
        "mipmap-xhdpi": 96,
        "mipmap-xxhdpi": 144,
        "mipmap-xxxhdpi": 192,
    }
    foreground_sizes = {
        "mipmap-mdpi": 108,
        "mipmap-hdpi": 162,
        "mipmap-xhdpi": 216,
        "mipmap-xxhdpi": 324,
        "mipmap-xxxhdpi": 432,
    }

    res_dir = app_src_main / "res"
    for folder, size in mipmap_sizes.items():
        out_dir = res_dir / folder
        out_dir.mkdir(parents=True, exist_ok=True)
        target = out_dir / "ic_launcher.webp"
        img.resize((size, size), Image.LANCZOS).save(target, format="WEBP", quality=100)

    for folder, size in foreground_sizes.items():
        out_dir = res_dir / folder
        out_dir.mkdir(parents=True, exist_ok=True)
        target = out_dir / "ic_launcher_foreground.webp"
        img.resize((size, size), Image.LANCZOS).save(
            target, format="WEBP", quality=100
        )

    drawable_dir = res_dir / "drawable"
    drawable_dir.mkdir(parents=True, exist_ok=True)
    foreground_xml = """<?xml version="1.0" encoding="utf-8"?>
<bitmap xmlns:android="http://schemas.android.com/apk/res/android"
    android:gravity="center"
    android:src="@mipmap/ic_launcher_foreground" />
"""
    foreground_xml_path = drawable_dir / "ic_launcher_foreground.xml"
    foreground_xml_path.write_text(foreground_xml, encoding="utf-8")

    drawable_v24_dir = res_dir / "drawable-v24"
    drawable_v24_dir.mkdir(parents=True, exist_ok=True)
    foreground_v24_xml_path = drawable_v24_dir / "ic_launcher_foreground.xml"
    foreground_v24_xml_path.write_text(foreground_xml, encoding="utf-8")

    background_xml = """<?xml version="1.0" encoding="utf-8"?>
<shape xmlns:android="http://schemas.android.com/apk/res/android" android:shape="rectangle">
    <solid android:color="#0B1220" />
</shape>
"""
    background_xml_path = drawable_dir / "ic_launcher_background.xml"
    background_xml_path.write_text(background_xml, encoding="utf-8")


def build_frontend(
        frontend_dir: Path,
        platform_name: Optional[str] = None,
        *,
        rust_target: Optional[str] = None,
        debug_mode: bool = False,
        max_size: bool = False,
        android_package_type: Optional[str] = None,
) -> None:
    try:
        linux_bundle_partial = False
        public_dir = frontend_dir / "dist" / "public"
        is_web_build = platform_name in {None, "web"}

        if is_web_build and public_dir.exists():
            print(f"Removing existing public artifacts: {public_dir}")
            shutil.rmtree(public_dir)
        if is_web_build:
            _clear_dx_web_cache(frontend_dir)
        elif platform_name == "android":
            clear_generated_android_project(frontend_dir, debug_mode)
        else:
            _ensure_bundle_icon_compat(frontend_dir)

        if not is_web_build:
            clear_app_bundle(frontend_dir)

        env = _dx_bundle_env(frontend_dir) if (is_container() or in_docker_build()) else None
        if platform_name == "android":
            env = _ensure_android_env(frontend_dir, env)
        elif platform_name == "windows":
            if env is None:
                env = os.environ.copy()
            env["DIOXUS_PRODUCT_NAME"] = WINDOWS_APP_NAME
            env["DIOXUS_APP_TITLE"] = WINDOWS_APP_NAME
        elif platform_name == "linux":
            if env is None:
                env = os.environ.copy()
            env["DIOXUS_PRODUCT_NAME"] = LINUX_PACKAGE_NAME
            env["DIOXUS_APP_TITLE"] = WINDOWS_APP_NAME
            env.setdefault("APPIMAGE_EXTRACT_AND_RUN", "1")
            env.setdefault("NO_STRIP", "1")

        ensured_wasm_bindgen = _ensure_wasm_bindgen_cli(frontend_dir, env)
        if env is not None and ensured_wasm_bindgen is not None:
            env["WASM_BINDGEN"] = str(ensured_wasm_bindgen)
            env["DIOXUS_WASM_BINDGEN"] = str(ensured_wasm_bindgen)
            env["DIOXUS_WASM_BINDGEN_PATH"] = str(ensured_wasm_bindgen)

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
                if is_ios_sim_target:
                    cmd.extend(["--package-types", "ios"])
                else:
                    cmd.extend(["--package-types", "ipa"])
                    cmd.extend(["--device", "true"])
            elif platform_name == "windows":
                _ensure_windows_icon_compat(frontend_dir)
                cmd.extend(["--windows-subsystem", "WINDOWS"])
        else:
            cmd.extend(["--platform", "web"])

        if not rust_target:
            rust_target = _default_rust_target_for_frontend(platform_name)

        if platform_name in {"windows", "linux"}:
            _clear_dioxus_bundle_identity_cache(frontend_dir, rust_target, debug_mode, platform_name)
        if platform_name == "windows":
            prepare_windows_dist_for_bundle(frontend_dir)
        elif platform_name == "linux":
            cleanup_linux_package_artifacts(frontend_dir)

        if rust_target:
            cmd.extend(["--target", rust_target])

        try:
            run(cmd, cwd=frontend_dir, env=env)
        except subprocess.CalledProcessError:
            err_text = _tail_log_text()
            if not err_text:
                # Fallback: try to grab current stderr context if no logfile is configured.
                err_text = "dx bundle failed"

            if _looks_like_wasm_bindgen_failure(err_text):
                hinted = _extract_wasm_bindgen_version_hint(err_text)
                prior_override = os.environ.get("GS_WASM_BINDGEN_CLI_VERSION")
                if hinted:
                    os.environ["GS_WASM_BINDGEN_CLI_VERSION"] = hinted
                try:
                    ensured = _ensure_wasm_bindgen_cli(frontend_dir, env)
                    if env is not None and ensured is not None:
                        env["WASM_BINDGEN"] = str(ensured)
                        env["DIOXUS_WASM_BINDGEN"] = str(ensured)
                        env["DIOXUS_WASM_BINDGEN_PATH"] = str(ensured)
                    print("Retrying frontend build after wasm-bindgen-cli fix")
                    run(cmd, cwd=frontend_dir, env=env)
                finally:
                    if hinted:
                        if prior_override is None:
                            os.environ.pop("GS_WASM_BINDGEN_CLI_VERSION", None)
                        else:
                            os.environ["GS_WASM_BINDGEN_CLI_VERSION"] = prior_override
            elif (
                    platform_name == "linux"
                    and (frontend_dir.parent / "target" / "dx" / _frontend_package_name(frontend_dir) / (
                    "debug" if debug_mode else "release") / "linux" / "app").exists()
            ):
                print(
                    "Warning: dx linux bundler failed after staging the app payload; falling back to manual AppImage "
                    "packaging.",
                    file=sys.stderr,
                )
                linux_bundle_partial = True
            else:
                raise

        if platform_name in {None, "web"}:
            _manual_optimize_web_wasm(frontend_dir, env, max_size=max_size)
            _prune_stale_hashed_assets(frontend_dir)
            _compress_web_assets(frontend_dir, env)
        elif platform_name == "android":
            rebuild_patched_android_bundle(frontend_dir, debug_mode, env)

        if platform_name == "macos":
            rename_macos_app_bundle(frontend_dir)
            remove_legacy_app_bundle(frontend_dir)
            remove_legacy_dmgs(frontend_dir)
            _patch_macos_bundle_icon(frontend_dir)
            sign_macos_app_and_dmg(frontend_dir)
        elif platform_name in {"windows", "linux"}:
            if not (platform_name == "linux" and linux_bundle_partial):
                rename_windows_linux_artifacts(frontend_dir, platform_name)
            if platform_name == "windows":
                build_manual_windows_installer(frontend_dir, rust_target, debug_mode)
            else:
                if not linux_bundle_partial:
                    patch_linux_bundle_metadata(frontend_dir)
                build_manual_appimage(frontend_dir, rust_target, debug_mode)
                build_manual_linux_packages(frontend_dir, rust_target, debug_mode)
                build_manual_arch_package(frontend_dir, rust_target, debug_mode)
                build_manual_flatpak_package(frontend_dir, rust_target, debug_mode)
        elif platform_name == "android":
            rename_android_artifacts(frontend_dir)
            if android_package_type != "aab":
                build_android_universal_apk(frontend_dir)

        if platform_name == "ios":
            staged_app = _stage_app_bundle_from_dx(
                frontend_dir,
                platform_name="ios",
                preferred_bundle_name=APP_BUNDLE_NAME,
            ) or app_bundle_path(frontend_dir)
            patch_plist(frontend_dir, staged_app)

    except FileNotFoundError as e:
        _print_missing_tool("Frontend build", e, frontend_dir)
        sys.exit(127)
    except subprocess.CalledProcessError as e:
        _print_command_failure("Frontend build", e, frontend_dir)
        sys.exit(e.returncode)


def _configure_log_file(repo_root: Path, log_file_arg: Optional[str]) -> None:
    global LOG_FILE
    if not log_file_arg:
        return
    log_path = Path(log_file_arg)
    if not log_path.is_absolute():
        log_path = repo_root / log_path
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_path.write_text("", encoding="utf-8")
    LOG_FILE = log_path
    print(f"Logging command output to: {LOG_FILE}")


def print_usage(exit_code: int = 1) -> None:
    print("Frontend build script")
    print("")
    print("Usage:")
    print("  ./frontend/build.py frontend_web|web [debug] [max_size] [existing] [log=<path>]")
    print("  ./frontend/build.py ios|ios_sim|macos|windows|android|linux [debug] [existing] [log=<path>]")
    print("  ./frontend/build.py android [apk|aab] [debug] [existing] [log=<path>]")
    print("")
    print("Frontend packaging and deploy actions:")
    print("  ./frontend/build.py ios_deploy [debug] [existing]")
    print("  ./frontend/build.py ios_sim_deploy [debug] [existing]")
    print(
        "  ./frontend/build.py ios_sim_screenshot [debug] [existing] [screenshot_delay=<seconds>] ["
        "screenshot_out=<path>] [screenshot_name=<name>]")
    print("  ./frontend/build.py ios_sign [debug] [existing]")
    print("  ./frontend/build.py ios_dist_sign [debug] [existing]")
    print("  ./frontend/build.py android_install [debug] [existing]")
    print(
        "  ./frontend/build.py android_screenshot [debug] [existing] [screenshot_delay=<seconds>] ["
        "screenshot_out=<path>] [screenshot_name=<name>]")
    print(
        "  ./frontend/build.py publisher_screenshots [debug] [existing] [screenshot_delay=<seconds>] ["
        "screenshot_out=<path>] [desktop_window=<width>x<height>] [ios_window=<width>x<height>] ["
        "android_window=<width>x<height>]")
    print("  ./frontend/build.py macos_deploy [debug] [existing]")
    print("  ./frontend/build.py macos_sign [debug] [existing]")
    print("  ./frontend/build.py macos_notarize [debug] [existing]")
    print("")
    print("What this script owns:")
    print("  - Dioxus frontend builds for web/desktop/mobile")
    print("  - wasm optimization and compression for web")
    print("  - iOS packaging/signing")
    print("  - Android bundle/APK generation and install")
    print("  - macOS signing/notarization/deploy")
    print("  - Windows/Linux packaging helpers (AppImage, deb, rpm, Arch, Flatpak)")
    print("  - frontend icon compatibility and asset generation")
    print("")
    print("Options:")
    print("  debug                             # skip --release")
    print("  max_size                          # extra wasm size optimization for web")
    print("  existing                          # reuse existing build artifacts when action allows it")
    print("  apk|aab                           # Android package type")
    print("  log=<path>                        # tee command output into a log file")
    print("  screenshot_delay=<seconds>        # wait before capturing screenshot")
    print("  screenshot_out=<path>             # output directory for screenshot actions")
    print("  screenshot_name=<name>            # screenshot filename stem")
    print("  desktop_window=<width>x<height>   # desktop output size for publisher screenshot")
    print("  ios_window=<width>x<height>       # iPhone output size for publisher screenshot set")
    print("  android_window=<width>x<height>   # Android phone output size for publisher screenshot set")
    print("")
    print("Environment:")
    print("  CERT_REGEX=...                    # override cert regex for signer script")
    print("  CERT_PICK=newest|first            # override cert selection for signer script")
    print("  MACOS_ENTITLEMENTS=...            # optional entitlements file for macOS codesign")
    print("  NOTARY_PROFILE=...                # notarytool keychain profile")
    print("  NOTARY_APPLE_ID=...               # notarytool Apple ID")
    print("  NOTARY_TEAM_ID=...                # notarytool team ID")
    print("  NOTARY_PASSWORD=...               # notarytool app-specific password")
    print("  GS_WASM_BINDGEN_CLI_VERSION=...   # force wasm-bindgen-cli version")
    print("  GS26_WINDOWS_TARGET=...           # override windows Rust target")
    print("  GS26_MACOS_TARGET=...             # override macOS Rust target")
    print("  ANDROID_KEYSTORE_PATH=...         # defaults to ~/keys/groundstation-upload.jks")
    print("  ANDROID_KEY_ALIAS=...             # defaults to upload")
    print("  ANDROID_KEYSTORE_PASSWORD=...     # defaults from macOS Keychain service gs26-android-keystore-pass")
    print("  ANDROID_KEY_PASSWORD=...          # defaults from macOS Keychain service gs26-android-key-pass")
    print("  ANDROID_KEYSTORE_TYPE=...         # defaults to JKS")
    print("  ANDROID_MIN_SDK=...               # defaults to 24")
    print("  ANDROID_TARGET_SDK=...            # defaults to 35")
    print("  ANDROID_COMPILE_SDK=...           # defaults to ANDROID_TARGET_SDK")
    print("")
    print("Provisioning profile path:")
    print(f"  {FIXED_MOBILEPROVISION_REL}")
    sys.exit(exit_code)


def main() -> None:
    raw_args = [a.strip() for a in sys.argv[1:]]
    if not raw_args or any(a in {"-h", "--help", "help"} for a in raw_args):
        print_usage(0 if raw_args else 1)

    debug_mode = False
    max_size_mode = False
    use_existing = False
    log_file_arg: Optional[str] = None
    android_package_type: Optional[str] = None
    screenshot_delay_arg: Optional[str] = None
    screenshot_out_arg: Optional[str] = None
    screenshot_name_arg: Optional[str] = None
    desktop_window_arg: Optional[str] = None
    ios_window_arg: Optional[str] = None
    android_window_arg: Optional[str] = None
    frontend_only_platform: Optional[str] = None
    frontend_rust_target: Optional[str] = None
    action: Optional[str] = None

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
        "android_install",
        "android_screenshot",
        "publisher_screenshots",
        "ios_deploy",
        "ios_sim_deploy",
        "ios_sim_screenshot",
        "ios_sign",
        "ios_dist_sign",
        "macos_deploy",
        "macos_sign",
        "macos_notarize",
    }

    for raw_arg in raw_args:
        arg = raw_arg.lower()
        if arg == "debug":
            debug_mode = True
        elif arg == "max_size":
            max_size_mode = True
        elif arg == "existing":
            use_existing = True
        elif arg in {"apk", "aab"}:
            android_package_type = arg
        elif arg.startswith("log="):
            value = raw_arg.split("=", 1)[1].strip()
            if not value:
                print("Error: log= requires a filepath.", file=sys.stderr)
                print_usage()
            log_file_arg = value
        elif arg.startswith("screenshot_delay="):
            value = raw_arg.split("=", 1)[1].strip()
            if not value:
                print("Error: screenshot_delay= requires a number of seconds.", file=sys.stderr)
                print_usage()
            screenshot_delay_arg = value
        elif arg.startswith("screenshot_out="):
            value = raw_arg.split("=", 1)[1].strip()
            if not value:
                print("Error: screenshot_out= requires a directory path.", file=sys.stderr)
                print_usage()
            screenshot_out_arg = value
        elif arg.startswith("screenshot_name="):
            value = raw_arg.split("=", 1)[1].strip()
            if not value:
                print("Error: screenshot_name= requires a filename stem.", file=sys.stderr)
                print_usage()
            screenshot_name_arg = value
        elif arg.startswith("desktop_window="):
            value = raw_arg.split("=", 1)[1].strip()
            if not value:
                print("Error: desktop_window= requires WIDTHxHEIGHT.", file=sys.stderr)
                print_usage()
            desktop_window_arg = value
        elif arg.startswith("ios_window="):
            value = raw_arg.split("=", 1)[1].strip()
            if not value:
                print("Error: ios_window= requires WIDTHxHEIGHT.", file=sys.stderr)
                print_usage()
            ios_window_arg = value
        elif arg.startswith("android_window=") or arg.startswith("android_screen="):
            value = raw_arg.split("=", 1)[1].strip()
            if not value:
                print("Error: android_window= requires WIDTHxHEIGHT.", file=sys.stderr)
                print_usage()
            android_window_arg = value
        elif arg in actions:
            if action or frontend_only_platform:
                print("Error: Only one frontend action/build may be specified.", file=sys.stderr)
                print_usage()
            action = arg
        elif arg in frontend_platform_map:
            if action or frontend_only_platform:
                print("Error: Only one frontend action/build may be specified.", file=sys.stderr)
                print_usage()
            frontend_only_platform, frontend_rust_target = frontend_platform_map[arg]
        else:
            print(f"Error: Invalid argument '{arg}'.", file=sys.stderr)
            print_usage()

    repo_root = Path(__file__).resolve().parents[1]
    frontend_dir = repo_root / "frontend"
    _configure_log_file(repo_root, log_file_arg)
    screenshot_delay = _parse_screenshot_delay(screenshot_delay_arg)
    screenshot_out_dir = _resolve_screenshot_output_dir(repo_root, screenshot_out_arg)
    desktop_window_size = _parse_size_arg(
        desktop_window_arg,
        default=(1440, 900),
        label="desktop_window",
    )
    ios_window_size = _parse_size_arg(
        ios_window_arg,
        default=(1290, 2796),
        label="ios_window",
    )
    android_window_size = _parse_size_arg(
        android_window_arg,
        default=(1080, 1920),
        label="android_window",
    )

    if action:
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
            print(f"Distribution IPA created: {ipa}")
            return

        if action == "android_install":
            apk_path: Optional[Path] = None
            if not use_existing:
                build_frontend(
                    frontend_dir,
                    platform_name="android",
                    rust_target=None,
                    debug_mode=debug_mode,
                    max_size=max_size_mode,
                    android_package_type="apk",
                )
                apk_candidates = sorted(dist_dir(frontend_dir).glob("*.apk"))
                apk_path = apk_candidates[-1] if apk_candidates else None
            serial, installed_apk = install_android_apk(frontend_dir, apk_path=apk_path)
            print(f"Android install complete ({serial}) for {installed_apk.name}")
            return

        if action == "android_screenshot":
            apk_path: Optional[Path] = None
            if not use_existing:
                build_frontend(
                    frontend_dir,
                    platform_name="android",
                    rust_target=None,
                    debug_mode=debug_mode,
                    max_size=max_size_mode,
                    android_package_type="apk",
                )
                apk_candidates = sorted(dist_dir(frontend_dir).glob("*.apk"))
                apk_path = apk_candidates[-1] if apk_candidates else None
            serial, installed_apk = install_android_apk(frontend_dir, apk_path=apk_path)
            screenshot_path = capture_android_screenshot(
                frontend_dir,
                output_dir=screenshot_out_dir,
                delay_seconds=screenshot_delay,
                filename_stem=screenshot_name_arg,
            )
            print(f"Android screenshot complete ({serial}) for {installed_apk.name}: {screenshot_path}")
            return

        if action == "publisher_screenshots":
            results = capture_publisher_screenshots(
                frontend_dir,
                debug_mode=debug_mode,
                max_size_mode=max_size_mode,
                use_existing=use_existing,
                output_dir=screenshot_out_dir,
                delay_seconds=screenshot_delay,
                desktop_window_size=desktop_window_size,
                ios_window_size=ios_window_size,
                android_window_size=android_window_size,
            )
            print("Publisher screenshots complete:")
            for path in results:
                print(f"  {path}")
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
            print(f"Simulator deploy complete ({udid}) for {bundle_id}")
            return

        if action == "ios_sim_screenshot":
            if not use_existing:
                build_frontend(
                    frontend_dir,
                    platform_name="ios",
                    rust_target="aarch64-apple-ios-sim",
                    debug_mode=debug_mode,
                    max_size=max_size_mode,
                )
            screenshot_path = capture_ios_sim_screenshot(
                frontend_dir,
                output_dir=screenshot_out_dir,
                delay_seconds=screenshot_delay,
                filename_stem=screenshot_name_arg,
            )
            print(f"iOS simulator screenshot complete: {screenshot_path}")
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
            print(f"Dev IPA created: {ipa}")
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
            print(f"Distribution IPA created: {ipa}")
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
            print(f"Installed into /Applications: {deployed}")
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
            print("Signed macOS app and dmg")
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
            print("Notarized macOS artifact")
            return

        print("Error: unknown action", file=sys.stderr)
        sys.exit(1)

    if frontend_only_platform is None:
        print("Error: expected a frontend platform or frontend action.", file=sys.stderr)
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
        android_package_type=android_package_type,
    )


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nFrontend build interrupted.", file=sys.stderr)
        sys.exit(INTERRUPTED_EXIT_CODE)
    except FileNotFoundError as e:
        missing = e.filename or "<unknown>"
        print("\nError: frontend build failed because a required tool/file is missing.", file=sys.stderr)
        print(f"  Missing: {missing}", file=sys.stderr)
        sys.exit(127)
    except subprocess.CalledProcessError as e:
        print("\nError: frontend build command failed.", file=sys.stderr)
        print(f"  Command : {' '.join(str(x) for x in e.cmd)}", file=sys.stderr)
        print(f"  Exit    : {e.returncode}", file=sys.stderr)
        sys.exit(e.returncode)
