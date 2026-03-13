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
import zipfile

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
LINUX_PACKAGE_NAME = "ubseds-groundstation"
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
        str(_which_in_path("makensis", os.environ.get("PATH", ""))) if _which_in_path("makensis", os.environ.get("PATH", "")) else None,
        str(_which_in_path("makensis.exe", os.environ.get("PATH", ""))) if _which_in_path("makensis.exe", os.environ.get("PATH", "")) else None,
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
        str(_which_in_path("iexpress", os.environ.get("PATH", ""))) if _which_in_path("iexpress", os.environ.get("PATH", "")) else None,
        str(_which_in_path("iexpress.exe", os.environ.get("PATH", ""))) if _which_in_path("iexpress.exe", os.environ.get("PATH", "")) else None,
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
        if item.suffix.lower() in {".deb", ".rpm"}:
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
    search_roots: list[Path] = []
    if rust_target:
        search_roots.append(target_root / rust_target / desktop_profile)
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
) -> tuple[tempfile.TemporaryDirectory, Path]:
    app_bin = _find_linux_app_binary(frontend_dir, rust_target, debug_mode)
    source_dir = app_bin.parent
    print(f"Staging Linux package payload from: {source_dir}")

    temp_dir = tempfile.TemporaryDirectory(prefix="gs26-linux-pkg-")
    pkg_root = Path(temp_dir.name) / "pkgroot"
    app_dir = pkg_root / "usr" / "lib" / LINUX_PACKAGE_NAME
    app_dir.mkdir(parents=True, exist_ok=True)

    for item in sorted(source_dir.iterdir()):
        if item.is_dir():
            shutil.copytree(item, app_dir / item.name, dirs_exist_ok=True)
            continue
        if item.suffix.lower() in {".deb", ".rpm", ".appimage", ".pdb"}:
            continue
        if item == app_bin:
            shutil.copy2(item, app_dir / LINUX_PACKAGE_NAME)
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
    launcher = bin_dir / LINUX_PACKAGE_NAME
    if launcher.exists() or launcher.is_symlink():
        launcher.unlink()
    os.symlink(f"../lib/{LINUX_PACKAGE_NAME}/{LINUX_PACKAGE_NAME}", launcher)

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

    icon_src = frontend_dir / "assets" / "icon.png"
    if icon_src.exists():
        pixmaps_dir = pkg_root / "usr" / "share" / "pixmaps"
        pixmaps_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(icon_src, pixmaps_dir / f"{LINUX_PACKAGE_NAME}.png")

        icons_dir = pkg_root / "usr" / "share" / "icons" / "hicolor" / "256x256" / "apps"
        icons_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(icon_src, icons_dir / f"{LINUX_PACKAGE_NAME}.png")

    return temp_dir, pkg_root


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
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{WINDOWS_APP_NAME}" "DisplayName" "{WINDOWS_APP_NAME}"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{WINDOWS_APP_NAME}" "DisplayIcon" "$INSTDIR\\{WINDOWS_APP_NAME}.exe"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{WINDOWS_APP_NAME}" "UninstallString" "$INSTDIR\\Uninstall.exe"
  WriteRegStr HKCU "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{WINDOWS_APP_NAME}" "InstallLocation" "$INSTDIR"
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
if ($dialogResult -ne [System.Windows.Forms.DialogResult]::OK -or [string]::IsNullOrWhiteSpace($folderDialog.SelectedPath)) {{
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
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name "DisplayName" -Value $appName
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name "DisplayIcon" -Value $exePath
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name "InstallLocation" -Value $installDir
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name "UninstallString" -Value ("powershell.exe -ExecutionPolicy Bypass -File `"" + $uninstallScript + "`"")
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name "NoModify" -Type DWord -Value 1
Set-ItemProperty -Path "HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\$appName" -Name "NoRepair" -Type DWord -Value 1
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
        raise FileNotFoundError("Neither dpkg-deb nor rpmbuild was found, so Linux packages cannot be built.")

    cleanup_linux_package_artifacts(frontend_dir)
    temp_dir, pkg_root = _stage_linux_app_payload(frontend_dir, rust_target, debug_mode)
    try:
        version = _read_frontend_version(frontend_dir)
        description = _read_workspace_description(frontend_dir.parent)
        repo_url = "https://github.com/University-at-Buffalo-SEDS/GroundStation26"
        deb_arch, rpm_arch = _linux_architecture(rust_target)
        dist = dist_dir(frontend_dir)
        dist.mkdir(parents=True, exist_ok=True)

        if dpkg_deb is not None:
            debian_dir = pkg_root / "DEBIAN"
            debian_dir.mkdir(parents=True, exist_ok=True)
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
            (debian_dir / "control").write_text(control, encoding="utf-8")
            deb_path = dist / f"{APP_NAME}_{deb_arch}.deb"
            run([str(dpkg_deb), "--build", "--root-owner-group", str(pkg_root), str(deb_path)], cwd=frontend_dir)
            print(f"✅ Linux deb created: {deb_path}")

        if rpmbuild is not None:
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
                    f"cp -a \"{pkg_root}\"/. %{buildroot}/",
                    "",
                    "%files",
                    f"/usr/bin/{LINUX_PACKAGE_NAME}",
                    f"/usr/lib/{LINUX_PACKAGE_NAME}",
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
        shutil.rmtree(project_dir)


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
    clean_task = "clean"
    run([str(gradlew), clean_task, task], cwd=project_dir, env=env)

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
        for rel in [Path("toolchains/llvm/prebuilt/darwin-x86_64/bin"), Path("toolchains/llvm/prebuilt/darwin-arm64/bin")]:
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
        dst_ico.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src_ico, dst_ico)
        return

    try:
        from PIL import Image  # type: ignore

        img = Image.open(src_png)
        # Include common Windows icon sizes.
        sizes = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]
        img.save(src_ico, format="ICO", sizes=sizes)
        shutil.copy2(src_ico, dst_ico)
        generated = True
    except Exception:
        generated = False

    if not generated:
        # Last-resort fallback if PIL is unavailable.
        # This may not produce a valid ICO for all tooling.
        shutil.copy2(src_png, src_ico)
        shutil.copy2(src_ico, dst_ico)
        print(
            "Warning: Pillow not available; copied PNG bytes to icon.ico. "
            "Install Pillow for a proper Windows icon.",
            file=sys.stderr,
        )


def _ensure_android_icon_compat(frontend_dir: Path, app_src_main: Path) -> None:
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
        for ext in ("webp", "png"):
            p = out_dir / f"ic_launcher.{ext}"
            if p.exists():
                p.unlink()
        img.resize((size, size), Image.LANCZOS).save(out_dir / "ic_launcher.webp", format="WEBP", quality=100)

    for folder, size in foreground_sizes.items():
        out_dir = res_dir / folder
        out_dir.mkdir(parents=True, exist_ok=True)
        for ext in ("webp", "png"):
            p = out_dir / f"ic_launcher_foreground.{ext}"
            if p.exists():
                p.unlink()
        img.resize((size, size), Image.LANCZOS).save(
            out_dir / "ic_launcher_foreground.webp", format="WEBP", quality=100
        )

    drawable_dir = res_dir / "drawable"
    drawable_dir.mkdir(parents=True, exist_ok=True)
    foreground_xml = """<?xml version="1.0" encoding="utf-8"?>
<bitmap xmlns:android="http://schemas.android.com/apk/res/android"
    android:gravity="center"
    android:src="@mipmap/ic_launcher_foreground" />
"""
    (drawable_dir / "ic_launcher_foreground.xml").write_text(foreground_xml, encoding="utf-8")

    drawable_v24_dir = res_dir / "drawable-v24"
    drawable_v24_dir.mkdir(parents=True, exist_ok=True)
    (drawable_v24_dir / "ic_launcher_foreground.xml").write_text(foreground_xml, encoding="utf-8")

    background_xml = """<?xml version="1.0" encoding="utf-8"?>
<shape xmlns:android="http://schemas.android.com/apk/res/android" android:shape="rectangle">
    <solid android:color="#0B1220" />
</shape>
"""
    (drawable_dir / "ic_launcher_background.xml").write_text(background_xml, encoding="utf-8")


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
        public_dir = frontend_dir / "dist" / "public"
        is_web_build = platform_name in {None, "web"}

        if is_web_build and public_dir.exists():
            print(f"Removing existing public artifacts: {public_dir}")
            shutil.rmtree(public_dir)
        if is_web_build:
            _clear_dx_web_cache(frontend_dir)
        elif platform_name == "android":
            clear_generated_android_project(frontend_dir, debug_mode)

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
                if not is_ios_sim_target:
                    cmd.extend(["--device", "true"])
            elif platform_name == "windows":
                _ensure_windows_icon_compat(frontend_dir)
                cmd.extend(["--windows-subsystem", "WINDOWS"])
            elif platform_name == "android" and android_package_type == "aab":
                cmd.extend(["--package-types", android_package_type])
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
            sign_macos_app_and_dmg(frontend_dir)
        elif platform_name in {"windows", "linux"}:
            rename_windows_linux_artifacts(frontend_dir, platform_name)
            if platform_name == "windows":
                build_manual_windows_installer(frontend_dir, rust_target, debug_mode)
            else:
                patch_linux_bundle_metadata(frontend_dir)
                build_manual_linux_packages(frontend_dir, rust_target, debug_mode)
        elif platform_name == "android":
            rename_android_artifacts(frontend_dir)
            if android_package_type != "aab":
                build_android_universal_apk(frontend_dir)

        if platform_name == "ios":
            patch_plist(frontend_dir)

    except FileNotFoundError as e:
        _print_missing_tool("Frontend build", e, frontend_dir)
        sys.exit(127)
    except subprocess.CalledProcessError as e:
        _print_command_failure("Frontend build", e, frontend_dir)
        sys.exit(e.returncode)


def build_backend(
        backend_dir: Path,
        force_pi: bool,
        force_no_pi: bool,
        testing_mode: bool,
        hitl_mode: bool,
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
    if hitl_mode:
        print("HITL mode enabled → adding `hitl_mode` feature.")
        if "--features" in cmd:
            cmd[cmd.index("--features") + 1] += ",hitl_mode"
        else:
            cmd.extend(["--features", "hitl_mode"])

    try:
        run(cmd, cwd=backend_dir)
    except FileNotFoundError as e:
        _print_missing_tool("Backend build", e, backend_dir)
        sys.exit(127)
    except subprocess.CalledProcessError as e:
        _print_command_failure("Backend build", e, backend_dir)
        sys.exit(e.returncode)


def print_usage(exit_code: int = 1) -> None:
    print("Usage:")
    print("  ./build.py                         # local: build frontend+backend (parallel)")
    print("  ./build.py pi_build                # local: backend w/ raspberry_pi feature")
    print("  ./build.py no_pi                   # local: backend w/o raspberry_pi feature")
    print("  ./build.py testing                 # local: backend w/ testing feature")
    print("  ./build.py hitl-mode               # local: backend w/ hitl_mode feature")
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
    print("  ./build.py android [apk|aab]")
    print("  ./build.py linux")
    print("  (add `debug` to frontend/local builds to skip --release)")
    print("")
    print("Frontend actions:")
    print("  ./build.py ios_deploy              # build ios + patch + package+sign (Distribution) -> IPA")
    print("  ./build.py ios_sim_deploy          # build ios_sim + install + launch in iOS simulator")
    print("  ./build.py ios_sign                # package+sign (Dev) existing dist app -> IPA")
    print("  ./build.py ios_dist_sign           # package+sign (Distribution) existing dist app -> IPA")
    print("  ./build.py android_install         # build Android APK from AAB, then install via adb")
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
    print("  GS26_WINDOWS_TARGET=...            # override windows Rust target (default x86_64-pc-windows-msvc)")
    print("  GS26_MACOS_TARGET=...              # override macos Rust target (auto-detects by default)")
    sys.exit(exit_code)


def main() -> None:
    global LOG_FILE

    force_pi = False
    force_no_pi = False
    docker_mode = False
    testing_mode = False
    hitl_mode = False
    debug_mode = False
    max_size_mode = False
    plain_mode = False
    use_existing = False
    backend_only = False
    log_file_arg: Optional[str] = None
    android_package_type: Optional[str] = None

    frontend_only_platform: Optional[str] = None
    frontend_rust_target: Optional[str] = None
    action: Optional[str] = None

    raw_args = [a.strip() for a in sys.argv[1:]]

    if any(a in {"-h", "--help", "help"} for a in raw_args):
        print_usage(0)

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
        "android_install",
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
        elif arg == "hitl-mode":
            hitl_mode = True
        elif arg == "debug":
            debug_mode = True
        elif arg == "apk":
            android_package_type = "apk"
        elif arg == "aab":
            android_package_type = "aab"
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
    if testing_mode and hitl_mode:
        print("Error: Cannot specify both 'testing' and 'hitl-mode'.", file=sys.stderr)
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
        if docker_mode or force_pi or force_no_pi or testing_mode or hitl_mode:
            print("Error: Frontend actions cannot be combined with docker/pi_build/no_pi/testing/hitl-mode.", file=sys.stderr)
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
            print(f"✅ Android install complete ({serial}) for {installed_apk.name}")
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
        if docker_mode or force_pi or force_no_pi or testing_mode or hitl_mode:
            print("Error: Frontend-only builds cannot be combined with docker/pi_build/no_pi/testing/hitl-mode.", file=sys.stderr)
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
        return

    if backend_only:
        if docker_mode:
            print("Error: backend_only cannot be combined with docker mode.", file=sys.stderr)
            print_usage()
        build_backend(backend_dir, force_pi, force_no_pi, testing_mode, hitl_mode, debug_mode)
        return

    if docker_mode:
        if hitl_mode:
            print("Error: docker mode currently does not support 'hitl-mode'.", file=sys.stderr)
            sys.exit(1)
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
        build_frontend(
            frontend_dir,
            None,
            debug_mode=debug_mode,
            max_size=max_size_mode,
            android_package_type=android_package_type,
        )
        build_backend(backend_dir, force_pi, force_no_pi, testing_mode, hitl_mode, debug_mode)
        return

    bfe = mp.Process(
        target=build_frontend,
        args=(frontend_dir, None),
        kwargs={
            "debug_mode": debug_mode,
            "max_size": max_size_mode,
            "android_package_type": android_package_type,
        },
    )
    bbe = mp.Process(
        target=build_backend,
        args=(backend_dir, force_pi, force_no_pi, testing_mode, hitl_mode, debug_mode),
    )
    bfe.start()
    bbe.start()
    bfe.join()
    bbe.join()


if __name__ == "__main__":
    try:
        main()
    except FileNotFoundError as e:
        missing = e.filename or "<unknown>"
        print("\nError: build failed because a required tool/file is missing.", file=sys.stderr)
        print(f"  Missing: {missing}", file=sys.stderr)
        if str(missing).lower() in {"cargo", "dx"}:
            print("Hint: ensure required tooling is installed and on PATH.", file=sys.stderr)
        sys.exit(127)
    except subprocess.CalledProcessError as e:
        _print_command_failure("Build", e, Path(__file__).resolve().parent)
        sys.exit(e.returncode)
    except Exception as e:
        print(f"\nError: build failed unexpectedly: {e}", file=sys.stderr)
        if LOG_FILE is not None:
            print(f"  Log file: {LOG_FILE}", file=sys.stderr)
        print("Hint: rerun with `log=build.log` to capture full command output.", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print("\n\nexiting...")
        sys.exit(0)
