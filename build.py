#!/usr/bin/env python3
import hashlib
import errno
from contextlib import contextmanager
import re
import tempfile
import time
import json
import multiprocessing as mp
import os
import platform
import shutil
import signal
import subprocess
import sys
from pathlib import Path
from subprocess import DEVNULL
from typing import Optional

LOG_FILE: Optional[Path] = None
INTERRUPTED_EXIT_CODE = 130
FRONTEND_REPO_URL = "https://github.com/Rylan-Meilutis/Seds-Ground-Station-Frontend"
FRONTEND_CHECKOUT_ENV = "GS26_FRONTEND_CHECKOUT_DIR"
FRONTEND_STABLE_BRANCH = "main"
FRONTEND_DEV_BRANCH = "dev"
WASM_OPT_FAILURE_HINTS = (
    "wasm-opt failed",
    "error parsing wasm",
    "unsupported version of dwarf",
    "compile unit size was incorrect",
    "invalid code after misc prefix",
)


def _append_log(line: str) -> None:
    if LOG_FILE is None:
        return
    with LOG_FILE.open("a", encoding="utf-8") as f:
        f.write(line)


def run(cmd: list[str], cwd: Path) -> None:
    cmd = [str(part) for part in cmd]
    cmd_line = f"Running: {' '.join(cmd)} (cwd={cwd})"
    print(cmd_line)
    _append_log(cmd_line + "\n")
    if LOG_FILE is None:
        try:
            subprocess.run(cmd, cwd=cwd, check=True)
        except KeyboardInterrupt:
            raise
        return
    proc = subprocess.Popen(
        cmd,
        cwd=cwd,
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


def run_capture(cmd: list[str], cwd: Path) -> str:
    cmd = [str(part) for part in cmd]
    print(f"Running: {' '.join(cmd)} (cwd={cwd})")
    out = subprocess.check_output(cmd, cwd=cwd, text=True)
    return out.strip()


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


def is_raspberry_pi() -> bool:
    if platform.system() != "Linux":
        return False
    for path in (
            Path("/sys/firmware/devicetree/base/model"),
            Path("/proc/device-tree/model"),
    ):
        try:
            txt = path.read_text(errors="ignore").lower()
            if "raspberry pi" in txt:
                return True
        except FileNotFoundError:
            continue
    return False


def no_parallel_requested() -> bool:
    return os.environ.get("GROUNDSTATION_NO_PARALLEL", "").strip().lower() in {"1", "true", "yes", "on"}


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


def in_docker_build() -> bool:
    if no_parallel_requested():
        return True
    return is_container()


def get_compose_base_cmd() -> list[str]:
    try:
        subprocess.run(["docker", "compose", "version"], stdout=DEVNULL, stderr=DEVNULL, check=True)
        return ["docker", "compose"]
    except (FileNotFoundError, subprocess.CalledProcessError):
        pass
    try:
        subprocess.run(["docker-compose", "version"], stdout=DEVNULL, stderr=DEVNULL, check=True)
        return ["docker-compose"]
    except (FileNotFoundError, subprocess.CalledProcessError):
        print(
            "Error: Neither 'docker compose' nor 'docker-compose' is available.\nPlease install Docker and Docker "
            "Compose.",
            file=sys.stderr,
        )
        sys.exit(1)


def build_docker(
        repo_root: Path,
        pi_build: bool,
        testing: bool,
        plain_progress: bool,
        frontend_dev: bool,
) -> None:
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
    if frontend_dev:
        print("Frontend development source (docker) → passing --build-arg FRONTEND_DEV=TRUE")
        cmd.extend(["--build-arg", "FRONTEND_DEV=TRUE"])
    # Docker cannot see remote UI changes when deciding whether to reuse a
    # build layer. Include the selected branch tip in that layer's cache key.
    branch = _frontend_branch(frontend_dev)
    tip = run_capture(["git", "ls-remote", "--heads", FRONTEND_REPO_URL, f"refs/heads/{branch}"], cwd=repo_root)
    fields = tip.split()
    if len(fields) != 2 or not re.fullmatch(r"[0-9a-f]{40}", fields[0]) or fields[1] != f"refs/heads/{branch}":
        raise RuntimeError(f"Unable to resolve frontend branch {branch} for the Docker build")
    cmd.extend(["--build-arg", f"FRONTEND_SOURCE_REVISION={fields[0]}"])
    run(cmd, cwd=repo_root)


def _run_script(repo_root: Path, script: Path, args: list[str]) -> None:
    run([sys.executable, str(script), *args], cwd=repo_root)


def _frontend_source_files(checkout: Path) -> list[Path]:
    # Git's exclusions omit generated Cargo/Dioxus artifacts without modifying
    # immutable inputs. Non-git source distributions use the same cache exclusions.
    result = subprocess.run(["git", "-C", str(checkout), "rev-parse", "--show-toplevel"],
                            capture_output=True, text=True)
    if result.returncode == 0 and Path(result.stdout.strip()).resolve() == checkout:
        names = subprocess.check_output(["git", "-C", str(checkout), "ls-files", "-z",
                                         "--cached", "--others", "--exclude-standard"])
        files = sorted({checkout / os.fsdecode(n) for n in names.split(b"\0") if n})
        if len(files) > 100000:
            raise RuntimeError("Frontend source exceeds 100000-file cache identity limit")
        return [path for path in files if not any(part in {"target", "dist", ".git", "node_modules"} for part in path.relative_to(checkout).parts) and path.resolve().is_relative_to(checkout)]
    ignored = {"target", "dist", ".git", "node_modules", "__pycache__"}
    files = []
    for base, dirs, names in os.walk(checkout):
        dirs[:] = sorted(d for d in dirs if d not in ignored)
        files.extend(Path(base) / name for name in sorted(names))
        if len(files) > 100000:
            raise RuntimeError("Frontend source exceeds 100000-file cache identity limit")
    return sorted(path for path in files if path.resolve().is_relative_to(checkout))


def _frontend_package(manifest: Path) -> dict:
    # Cargo parses the manifest on Python 3.10 too. Unlike metadata/clean against
    # the original graph, read-manifest does not resolve dependencies or write a lock.
    if manifest.stat().st_size > 1024 * 1024:
        raise RuntimeError("Frontend Cargo manifest exceeds 1 MiB")
    with tempfile.TemporaryFile() as output:
        subprocess.run(["cargo", "read-manifest", "--quiet", "--manifest-path", str(manifest)],
                       stdout=output, check=True)
        if output.tell() > 8 * 1024 * 1024:
            raise RuntimeError("Frontend package metadata exceeds 8 MiB")
        output.seek(0)
        value = json.load(output)
    package = {key: value[key] for key in ("name", "version")}
    package["targets"] = [{key: item[key] for key in ("name", "kind", "crate_types")}
                          for item in value["targets"]]
    _validate_frontend_package(package)
    return package


def _validate_frontend_package(package: dict) -> None:
    for field in ("name", "version"):
        if not isinstance(package.get(field), str) or not re.fullmatch(r"[A-Za-z0-9_.+\-]{1,200}", package[field]):
            raise RuntimeError("Invalid frontend package descriptor")
    targets = package.get("targets")
    if not isinstance(targets, list) or not 1 <= len(targets) <= 256:
        raise RuntimeError("Invalid frontend target descriptor")
    for item in targets:
        if not re.fullmatch(r"[A-Za-z0-9_\-]{1,200}", item.get("name", "")):
            raise RuntimeError("Invalid frontend target name")
        if item.get("kind") not in [[k] for k in ("bin", "lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro", "example", "test", "bench", "custom-build")]:
            raise RuntimeError("Unsupported frontend target kind")
        if not isinstance(item.get("crate_types"), list) or not all(k in ("bin", "lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro") for k in item["crate_types"]):
            raise RuntimeError("Invalid frontend crate types")


def _clean_frontend_package(target: Path, package: dict) -> None:
    # Resolve no dependency graph and write no files in the immutable checkout.
    # Cargo still owns artifact selection/removal, including custom target names.
    _validate_frontend_package(package)
    with tempfile.TemporaryDirectory(prefix="gs-frontend-clean-") as temporary:
        root = Path(temporary)
        lines = ["[package]", "name = " + json.dumps(package["name"]),
                 "version = " + json.dumps(package["version"]), 'edition = "2021"']
        for index, item in enumerate(package["targets"]):
            kind = item["kind"][0]
            if kind == "custom-build":
                (root / "build.rs").write_text("fn main() {}\n", encoding="utf-8")
                continue
            section = {"bin": "[[bin]]", "example": "[[example]]", "test": "[[test]]",
                       "bench": "[[bench]]"}.get(kind, "[lib]")
            source = f"target{index}.rs"
            (root / source).write_text("fn main() {}\n", encoding="utf-8")
            lines.extend([section, "name = " + json.dumps(item["name"]), "path = " + json.dumps(source)])
            if section == "[lib]":
                lines.append("crate-type = " + json.dumps(item["crate_types"]))
        (root / "Cargo.toml").write_text("\n".join(lines) + "\n", encoding="utf-8")
        subprocess.run(["cargo", "clean", "--offline", "--manifest-path", str(root / "Cargo.toml"),
                        "--package", package["name"], "--target-dir", str(target)], check=True)


def _frontend_dependency_locks(checkout: Path, manifests: list[Path]) -> dict:
    paths = {checkout / "Cargo.lock"} | {path.parent / "Cargo.lock" for path in manifests}
    result = {}
    for path in sorted(paths):
        if not path.resolve().is_relative_to(checkout):
            raise RuntimeError("Frontend lockfile resolves outside its checkout")
        value = None
        if path.is_file():
            digest = hashlib.sha256()
            with path.open("rb") as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    digest.update(chunk)
            value = digest.hexdigest()
        result[str(path.relative_to(checkout))] = value
    return result


@contextmanager
def _frontend_public_attempt(checkout: Path, reuse: bool, lifecycle: dict, destination=None):
    candidates = [checkout / "dist/public", checkout / "frontend/dist/public"]
    # Validate all paths before moving any prior output.
    for public in candidates:
        if not public.resolve().is_relative_to(checkout) or public.is_symlink():
            raise RuntimeError("Frontend public output must be a checkout-local directory")
        if destination is not None and public.resolve() == destination.resolve():
            raise RuntimeError("Frontend build output must differ from the published GS destination")
        if public.exists() and not public.is_dir():
            raise RuntimeError("Frontend public output is not a directory")
    if reuse:
        yield
        return
    backups = []
    succeeded = False
    attempt_started = False
    try:
        for public in candidates:
            if public.exists():
                backup = Path(tempfile.mkdtemp(prefix=".gs-public-prior-", dir=public.parent))
                try:
                    public.rename(backup / "public")
                except BaseException:
                    shutil.rmtree(backup)
                    raise
                backups.append((public, backup))
        attempt_started = True
        yield
        succeeded = True
    finally:
        # An uncertain live writer retains its output/quarantine and marker for
        # manual recovery. Never restore directories under a possible writer.
        if succeeded or not attempt_started or lifecycle["stopped"]:
            if not succeeded:
                if attempt_started:
                    for public in candidates:
                        if not public.parent.resolve().is_relative_to(checkout):
                            raise RuntimeError("Frontend output parent escaped its checkout")
                        if public.is_symlink() or public.is_file():
                            public.unlink()
                        elif public.is_dir():
                            shutil.rmtree(public)
                for public, backup in backups:
                    (backup / "public").rename(public)
            for public, backup in backups:
                shutil.rmtree(backup)


class FrontendBuildInterrupted(RuntimeError):
    """An ownership marker is evidence of uncertainty, never a PID kill ticket."""
    def __init__(self, message: str, kind: str):
        super().__init__(message)
        self.kind = kind


def _frontend_recovery_target(shared: Path) -> Path:
    target = shared.with_name(shared.name + "-recovery")
    if target.is_symlink() or (target.exists() and not target.is_dir()):
        raise RuntimeError("Fixed frontend recovery target must be an independent local directory")
    owner = target.parent / ("." + target.name + ".gs-frontend-recovery-owner.json")
    expected = {"version": 1, "normal_target": str(shared), "recovery_target": str(target)}
    with _frontend_cache_lock(target.parent / ("." + target.name + ".gs-frontend-selection.lock")):
        interrupted = target.parent / ("." + target.name + ".gs-frontend-in-progress.json")
        if interrupted.exists() or interrupted.is_symlink():
            raise RuntimeError("The fixed frontend recovery source/target is also interrupted or active. "
                               "All writers and markers were preserved; no further caches were allocated. "
                               "Wait for the active build or use an explicit separate source and target.")
        if owner.is_symlink():
            raise RuntimeError("Frontend recovery target ownership is uncertain")
        if owner.exists():
            if owner.stat().st_size > 4096 or json.loads(owner.read_text(encoding="utf-8")) != expected:
                raise RuntimeError("Frontend recovery target ownership differs")
        else:
            if target.exists():
                raise RuntimeError("Fixed frontend recovery target already exists without owned provenance; preserve it and use a separate cache")
            with owner.open("x", encoding="utf-8") as output:
                json.dump(expected, output)
                output.flush()
                os.fsync(output.fileno())
    return target


def _frontend_recovery_checkout(checkout: Path) -> Path:
    """Fresh committed source only; never copy uncertain generated output."""
    checkout = checkout.resolve()
    if _frontend_shared_target(checkout) is None:
        raise RuntimeError("Automatic frontend isolation requires a managed immutable snapshot")
    revision = checkout.name
    actual = run_capture(["git", "-C", str(checkout), "rev-parse", "HEAD"], cwd=checkout).strip()
    top = run_capture(["git", "-C", str(checkout), "rev-parse", "--show-toplevel"], cwd=checkout).strip()
    if actual != revision or Path(top).resolve() != checkout:
        raise RuntimeError("Interrupted frontend is not its claimed immutable Git revision")
    root = checkout.parent / ".gs-frontend-recovery-sources"
    if root.is_symlink() or (root.exists() and not root.is_dir()):
        raise RuntimeError("Frontend recovery source cache is not a local directory")
    with _frontend_cache_lock(checkout.parent / ".gs-frontend-recovery-sources.lock"):
        root.mkdir(exist_ok=True)
        source = root / revision
        expected = {"version": 1, "original": str(checkout), "revision": revision}
        if source.exists() or source.is_symlink():
            metadata = source / ".git/gs-frontend-recovery.json"
            if source.is_symlink() or metadata.is_symlink() or not metadata.is_file() or metadata.stat().st_size > 4096:
                raise RuntimeError("Frontend recovery snapshot is not a verified owned snapshot")
            if json.loads(metadata.read_text(encoding="utf-8")) != expected:
                raise RuntimeError("Frontend recovery snapshot ownership differs")
        else:
            # Count only this bounded recovery cache, not normal revision history.
            # Interrupted staging directories remain evidence and consume a slot.
            if sum(1 for path in root.iterdir() if path.is_dir() or path.is_symlink()) >= 8:
                raise RuntimeError("Frontend recovery source cache reached its 8-slot limit; preserve uncertain writers and use a separate cache")
            sizes = run_capture(["git", "-C", str(checkout), "ls-tree", "-r", "-l", revision], cwd=checkout)
            total = sum(int(parts[3]) for line in sizes.splitlines() if len(parts := line.split(None, 4)) >= 4 and parts[3].isdigit())
            if total > 256 * 1024 * 1024:
                raise RuntimeError("Frontend recovery source exceeds the 256 MiB committed-source limit")
            staging = Path(tempfile.mkdtemp(prefix=".snapshot-", dir=root))
            clone = staging / "source"
            # file:// plus depth1 avoids copying history, targets, ignored locks,
            # output/quarantines, hardlinks, or Git object alternates.
            # Failure leaves bounded staging evidence; do not delete under an
            # uncertain clone subprocess after interruption.
            run(["git", "clone", "--no-hardlinks", "--no-checkout", "--depth", "1",
                 "--no-tags", "--single-branch", checkout.as_uri(), str(clone)], cwd=root)
            run(["git", "-C", str(clone), "checkout", "--detach", revision], cwd=root)
            with (clone / ".git/gs-frontend-recovery.json").open("x", encoding="utf-8") as output:
                json.dump(expected, output)
                output.flush()
                os.fsync(output.fileno())
            clone.rename(source)
            staging.rmdir()
        if (source / ".git").is_symlink() or (source / ".git/objects/info/alternates").exists():
            raise RuntimeError("Frontend recovery Git metadata is not independent")
        actual = run_capture(["git", "-C", str(source), "rev-parse", "HEAD"], cwd=source).strip()
        top = run_capture(["git", "-C", str(source), "rev-parse", "--show-toplevel"], cwd=source).strip()
        if Path(top).resolve() != source.resolve():
            raise RuntimeError("Frontend recovery Git root differs")
        dirty = run_capture(["git", "-C", str(source), "status", "--porcelain", "--untracked-files=no"], cwd=source)
        if actual != revision or dirty:
            raise RuntimeError("Frontend recovery snapshot changed; no automatic overwrite is safe")
        _browser_asset_digest(source)
        return source


def _run_frontend_script_with_wasm_opt_fallback(
        script: Path, checkout_dir: Path, args: list[str], finish=None, publication_destination=None,
        target_override: Optional[Path] = None) -> None:
    checkout = checkout_dir.resolve()
    # Generated checkout output is shared even when callers choose different
    # targets and publish to different destinations. Global order is always
    # destination (public wrapper), checkout output, then compiler target.
    lease = checkout.parent / ("." + checkout.name + ".gs-frontend-output.lock")
    with _frontend_cache_lock(lease):
        source_marker = checkout.parent / ("." + checkout.name + ".gs-frontend-output-in-progress.json")
        if source_marker.exists() or source_marker.is_symlink():
            raise FrontendBuildInterrupted(f"interrupted/uncertain frontend checkout output: {source_marker}. "
                               "Use a separate checkout and CARGO_TARGET_DIR, or confirm all prior writers "
                               "have stopped before removing its source/target markers and restoring quarantines.", "source")
        _run_frontend_script_with_source_lease(script, checkout, args, finish,
                                               publication_destination, source_marker, target_override)


def _run_frontend_script_with_source_lease(
        script: Path, checkout: Path, args: list[str], finish, publication_destination, source_marker: Path,
        target_override: Optional[Path] = None) -> None:
    manifest = script.resolve().parent / "Cargo.toml"
    if not manifest.resolve().is_relative_to(checkout):
        raise RuntimeError("Frontend application manifest must be inside its checkout")
    application = _frontend_package(manifest)
    explicit = os.environ.get("CARGO_TARGET_DIR")
    target = target_override if target_override is not None else (Path(explicit).expanduser() if explicit else (_frontend_shared_target(checkout) or manifest.parent / "target"))
    if not target.is_absolute():
        target = manifest.parent / target
    target = target.resolve()
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(target)
    # One lease covers invalidation, compilation, fallback, destination sync and
    # provenance. Direct callers and the normal wrapper use this same lock once.
    with _frontend_cache_lock(target.parent / ("." + target.name + ".gs-frontend-build.lock")):
        marker = target.parent / ("." + target.name + ".gs-frontend-in-progress.json")
        if marker.exists() or marker.is_symlink():
            raise FrontendBuildInterrupted(f"Frontend target has an interrupted/uncertain build: {marker}. "
                               "Use a separate checkout and CARGO_TARGET_DIR, or confirm all prior build descendants "
                               "have stopped before manually removing this marker. No stale PID is killed.", "target")
        files = _frontend_source_files(checkout)
        digest = hashlib.sha256()
        for path in files:
            digest.update(str(path.relative_to(checkout)).encode() + b"\0")
            if path.is_file():
                with path.open("rb") as stream:
                    for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                        digest.update(chunk)
            else:
                digest.update(b"missing")
        manifests = [path for path in files if path.name == "Cargo.toml" and path.is_file()
                     and path.resolve().is_relative_to(checkout)]
        if len(manifests) > 128:
            raise RuntimeError("Frontend exceeds 128 checkout-local Cargo packages")
        packages = [_frontend_package(path) for path in manifests]
        head = subprocess.run(["git", "-C", str(checkout), "rev-parse", "--verify", "HEAD"],
                              capture_output=True, text=True)
        revision = head.stdout.strip() if head.returncode == 0 else None
        locks_before = _frontend_dependency_locks(checkout, manifests)
        identity = {"version": 2, "source": str(checkout), "revision": revision,
                    "digest": digest.hexdigest(), "application": application,
                    "packages": packages, "dependency_locks": locks_before}
        receipt = target / ".gs-frontend-source.json"
        try:
            if receipt.stat().st_size > 2 * 1024 * 1024:
                raise RuntimeError("Frontend provenance exceeds 2 MiB")
            previous = json.loads(receipt.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            previous = None
        previous_identity = dict(previous) if isinstance(previous, dict) else previous
        if isinstance(previous_identity, dict):
            previous_identity.pop("lock_before_build", None)
        reuse = "existing" in args
        if reuse and previous_identity != identity:
            raise RuntimeError("Existing frontend output has no matching source/dependency provenance; rebuild without existing")
        lifecycle = {"stopped": False}
        for ownership_marker in (marker, source_marker):
            with ownership_marker.open("x", encoding="utf-8") as output:
                json.dump({"version": 1, "source": str(checkout), "target": str(target), "pid": os.getpid()}, output)
                output.flush()
                os.fsync(output.fileno())
        try:
            receipt.unlink(missing_ok=True)
            if previous is not None:
                if not isinstance(previous, dict) or not isinstance(previous.get("packages"), list) or len(previous["packages"]) > 128:
                    raise RuntimeError("Invalid previous frontend provenance")
                for package in previous["packages"] + [previous.get("application", {})]:
                    _validate_frontend_package(package)
            if previous_identity != identity:
                selected = packages + ((previous or {}).get("packages", []))
                seen = set()
                for package in selected:
                    key = json.dumps(package, sort_keys=True)
                    if key not in seen:
                        _clean_frontend_package(target, package)
                        seen.add(key)
                names = {application["name"]}
                if previous:
                    names.add(previous["application"]["name"])
                for name in names:
                    cache = target / "dx" / name
                    if cache.exists():
                        shutil.rmtree(cache)
            with _frontend_public_attempt(checkout, reuse, lifecycle, publication_destination):
                _execute_frontend_script_with_wasm_opt_fallback(script, checkout, args, environment, lifecycle, allow_fallback=not reuse)
                if finish is not None:
                    finish()
                identity["lock_before_build"] = locks_before
                identity["dependency_locks"] = _frontend_dependency_locks(checkout, manifests)
                temporary = None
                try:
                    with tempfile.NamedTemporaryFile(mode="w", dir=target, prefix=".gs-source-", delete=False,
                                                     encoding="utf-8") as output:
                        temporary = Path(output.name)
                        json.dump(identity, output, sort_keys=True)
                        output.flush()
                        os.fsync(output.fileno())
                    temporary.replace(receipt)
                finally:
                    if temporary is not None:
                        temporary.unlink(missing_ok=True)
        finally:
            if lifecycle["stopped"]:
                marker.unlink(missing_ok=True)
                source_marker.unlink(missing_ok=True)


def _stop_frontend_process_tree(proc) -> None:
    if os.name == "nt":
        # Only this owned PID/tree; never kill tools or services by name.
        result = subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                                stdout=DEVNULL, stderr=subprocess.STDOUT)
        if result.returncode and proc.poll() is None:
            proc.kill()
        proc.wait()
        raise RuntimeError("Windows frontend tree cleanup is unqualified; target remains interrupted")
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    finally:
        # The group leader can already have exited while a descendant still
        # writes Cargo artifacts. Always retire the owned group before unlocking.
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.wait()


def _remote_build():
    sys.path.insert(0, str(Path(__file__).resolve().parent / "scripts"))
    import remote_build
    return remote_build


def _execute_frontend_script_with_wasm_opt_fallback(script: Path, checkout_dir: Path, args: list[str], environment: dict[str, str], lifecycle: dict, allow_fallback: bool = True) -> None:
    # Reusing existing output compiles nothing, so it stays local.
    if _remote_build().requested() and "existing" not in args:
        try:
            _remote_build().build_frontend(checkout_dir, script, args)
        finally:
            lifecycle["stopped"] = True
        return
    cmd = [sys.executable, str(script), *args]
    cmd_line = f"Running: {' '.join(str(part) for part in cmd)} (cwd={checkout_dir})"
    print(cmd_line)
    _append_log(cmd_line + "\n")

    proc = subprocess.Popen(
        [str(part) for part in cmd],
        env=environment,
        cwd=checkout_dir,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
        start_new_session=os.name != "nt",
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0,
    )
    assert proc.stdout is not None

    wasm_opt_failed = False
    try:
        for line in proc.stdout:
            print(line, end="")
            _append_log(line)
            if any(hint in line.lower() for hint in WASM_OPT_FAILURE_HINTS):
                wasm_opt_failed = True
        rc = proc.wait()
    except BaseException:
        _stop_frontend_process_tree(proc)
        lifecycle["stopped"] = True
        raise
    finally:
        proc.stdout.close()

    if os.name != "nt":
        _stop_frontend_process_tree(proc)
    lifecycle["stopped"] = True
    if rc == 0:
        return

    if not allow_fallback:
        raise subprocess.CalledProcessError(rc, [str(part) for part in cmd])
    public_dir = _resolve_external_public_dir(checkout_dir) if wasm_opt_failed else None
    if public_dir is not None and public_dir.is_dir() and any(public_dir.iterdir()) and wasm_opt_failed:
        warning = (
            "Warning: frontend build completed but wasm-opt post-processing failed. "
            "Continuing with the unoptimized wasm bundle because dist/public was produced."
        )
        print(warning)
        _append_log(warning + "\n")
        return

    raise subprocess.CalledProcessError(rc, [str(part) for part in cmd])


def _normalize_git_url(url: str) -> str:
    normalized = url.strip().rstrip("/")
    if normalized.endswith(".git"):
        normalized = normalized[:-4]
    return normalized.lower()


def _default_frontend_checkout_dir() -> Path:
    override = os.environ.get(FRONTEND_CHECKOUT_ENV, "").strip()
    if override:
        path = Path(override).expanduser()
        return path if path.is_absolute() else Path.cwd() / path
    if platform.system() == "Darwin":
        return Path.home() / "Library" / "Caches" / "GroundStation26" / "frontend-source"
    if platform.system() == "Windows":
        local_app_data = os.environ.get("LOCALAPPDATA", "").strip()
        if local_app_data:
            return Path(local_app_data) / "GroundStation26" / "frontend-source"
    return Path.home() / ".cache" / "groundstation26" / "frontend-source"


def _frontend_checkout_dir() -> Path:
    return _default_frontend_checkout_dir()


def _frontend_branch(frontend_dev: bool) -> str:
    return FRONTEND_DEV_BRANCH if frontend_dev else FRONTEND_STABLE_BRANCH


def _frontend_sync_dir(repo_root: Path) -> Path:
    return repo_root / "frontend"


def _frontend_sync_public_dir(repo_root: Path) -> Path:
    return _frontend_sync_dir(repo_root) / "dist" / "public"


def _resolve_external_frontend_script(checkout_dir: Path) -> Path:
    candidates = [
        checkout_dir / "build.py",
        checkout_dir / "frontend" / "build.py",
    ]
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    raise FileNotFoundError(
        f"Failed to find a frontend build script in {checkout_dir}. "
        "Expected build.py or frontend/build.py."
    )


def _resolve_external_public_dir(checkout_dir: Path) -> Path:
    candidates = [
        checkout_dir / "dist" / "public",
        checkout_dir / "frontend" / "dist" / "public",
    ]
    for candidate in candidates:
        if candidate.is_dir():
            return candidate
    raise FileNotFoundError(
        f"Failed to find built frontend assets in {checkout_dir}. "
        "Expected dist/public or frontend/dist/public."
    )


def _resolve_external_favicon(checkout_dir: Path) -> Optional[Path]:
    candidate_names = ("icon.png", "favicon.png", "favicon.ico")
    candidate_dirs = (
        checkout_dir,
        checkout_dir / "frontend",
        checkout_dir / "assets",
        checkout_dir / "public",
        checkout_dir / "static",
        checkout_dir / "dist",
        checkout_dir / "dist" / "public",
        checkout_dir / "frontend" / "assets",
        checkout_dir / "frontend" / "public",
        checkout_dir / "frontend" / "static",
        checkout_dir / "frontend" / "dist",
        checkout_dir / "frontend" / "dist" / "public",
    )
    for directory in candidate_dirs:
        for name in candidate_names:
            candidate = directory / name
            if candidate.is_file():
                return candidate
    return None


def _ensure_frontend_checkout(checkout_dir: Path, branch: str) -> None:
    if not checkout_dir.exists():
        checkout_dir.parent.mkdir(parents=True, exist_ok=True)
        run(
            ["git", "clone", "--depth", "1", "--branch", branch, FRONTEND_REPO_URL, str(checkout_dir)],
            cwd=checkout_dir.parent,
        )
        return

    if not checkout_dir.is_dir():
        raise RuntimeError(f"Frontend checkout path exists but is not a directory: {checkout_dir}")

    inside_work_tree = run_capture(
        ["git", "-C", str(checkout_dir), "rev-parse", "--is-inside-work-tree"],
        cwd=checkout_dir,
    )
    if inside_work_tree.strip().lower() != "true":
        raise RuntimeError(f"Frontend checkout path is not a git worktree: {checkout_dir}")

    origin_url = run_capture(
        ["git", "-C", str(checkout_dir), "remote", "get-url", "origin"],
        cwd=checkout_dir,
    )
    if _normalize_git_url(origin_url) != _normalize_git_url(FRONTEND_REPO_URL):
        raise RuntimeError(
            f"Frontend checkout remote does not match {FRONTEND_REPO_URL}: {origin_url}"
        )

    status = run_capture(
        ["git", "-C", str(checkout_dir), "status", "--porcelain"],
        cwd=checkout_dir,
    )
    if status:
        raise RuntimeError(
            f"Frontend checkout has local changes; refusing to pull latest from origin: {checkout_dir}"
        )

    run(
        ["git", "-C", str(checkout_dir), "fetch", "origin", f"{branch}:refs/remotes/origin/{branch}"],
        cwd=checkout_dir,
    )
    local_branches = run_capture(
        ["git", "-C", str(checkout_dir), "branch", "--format=%(refname:short)"],
        cwd=checkout_dir,
    ).splitlines()
    if branch in local_branches:
        run(["git", "-C", str(checkout_dir), "switch", branch], cwd=checkout_dir)
    else:
        run(
            ["git", "-C", str(checkout_dir), "switch", "--no-track", "-c", branch, f"origin/{branch}"],
            cwd=checkout_dir,
        )
    run(["git", "-C", str(checkout_dir), "pull", "--ff-only", "origin", branch], cwd=checkout_dir)


@contextmanager
def _frontend_cache_lock(path: Path):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+b") as lock:
        if os.name == "nt":
            import msvcrt
            if lock.seek(0, os.SEEK_END) == 0:
                lock.write(b"0")
                lock.flush()
            while True:
                try:
                    lock.seek(0)
                    msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
                    break
                except OSError as error:
                    if error.errno not in {errno.EAGAIN, errno.EACCES, errno.EBUSY, errno.EDEADLK}:
                        raise
                    time.sleep(0.1)
            try:
                yield
            finally:
                lock.seek(0)
                msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
        else:
            import fcntl
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
            try:
                yield
            finally:
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)


def _immutable_frontend_checkout(checkout: Path, branch: str) -> Path:
    revisions = checkout.with_name(checkout.name + "-revisions")
    with _frontend_cache_lock(checkout.with_name(checkout.name + ".fetch.lock")):
        _ensure_frontend_checkout(checkout, branch)
        revision = run_capture(["git", "-C", str(checkout), "rev-parse", "HEAD"], cwd=checkout).strip()
        if not re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", revision):
            raise RuntimeError("Invalid frontend Git revision")
        source = revisions / revision
        if not source.exists():
            revisions.mkdir(parents=True, exist_ok=True)
            with tempfile.TemporaryDirectory(prefix=".snapshot-", dir=revisions) as temporary:
                clone = Path(temporary) / "source"
                run(["git", "clone", "--no-hardlinks", "--no-checkout", str(checkout.resolve()), str(clone)], cwd=revisions)
                run(["git", "-C", str(clone), "checkout", "--detach", revision], cwd=clone)
                _browser_asset_digest(clone)
                clone.rename(source)
        actual = run_capture(["git", "-C", str(source), "rev-parse", "HEAD"], cwd=source).strip()
        dirty = run_capture(["git", "-C", str(source), "status", "--porcelain", "--untracked-files=no"], cwd=source)
        if actual != revision or dirty:
            raise RuntimeError("Cached frontend revision has changed; restore it before rebuilding")
        _browser_asset_digest(source)
        print(f"Frontend revision: {revision}")
        return source


def _frontend_shared_target(checkout: Path) -> Optional[Path]:
    if checkout.parent.name.endswith("-revisions") and re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", checkout.name):
        return checkout.parent / "target"
    return None


def _sync_frontend_public_assets(repo_root: Path, checkout_dir: Path) -> None:
    src_public_dir = _resolve_external_public_dir(checkout_dir)
    dst_public_dir = _frontend_sync_public_dir(repo_root)
    dst_public_dir.parent.mkdir(parents=True, exist_ok=True)
    if dst_public_dir.exists():
        shutil.rmtree(dst_public_dir)
    print(f"Syncing frontend web assets: {src_public_dir} -> {dst_public_dir}")
    shutil.copytree(src_public_dir, dst_public_dir)
    favicon = _resolve_external_favicon(checkout_dir)
    if favicon is not None:
        dst_favicon = dst_public_dir / favicon.name
        if favicon.resolve() != dst_favicon.resolve():
            print(f"Syncing frontend favicon: {favicon} -> {dst_favicon}")
            shutil.copy2(favicon, dst_favicon)


BROWSER_ASSET_SOURCE_ENV = "GS_FRONTEND_SOURCE_DIR"


def _browser_asset_digest(source: Path) -> str:
    source = source.resolve()
    preparer = source / "scripts/browser_assets.py"
    if preparer.is_file():
        run([sys.executable, str(preparer), "--root", str(source)], cwd=source)
    manifest_path = source / "station_ui/manifest.json"
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise RuntimeError(f"Selected frontend has no valid station assets: {source}. Update that checkout or rebuild the WebUI.") from error
    if manifest.get("version") != 1 or not isinstance(manifest.get("exports"), list):
        raise RuntimeError("Unsupported frontend station asset manifest")
    exports = manifest["exports"]
    if not exports or len(exports) > 64:
        raise RuntimeError("Frontend station asset exports must contain 1–64 entries")
    digest = hashlib.sha256(manifest_path.read_bytes())
    for asset in exports:
        relative = Path(asset["path"])
        if relative.is_absolute() or any(part in {".", ".."} for part in relative.parts):
            raise RuntimeError("Frontend station asset paths must be relative")
        path = (source / relative).resolve()
        if not path.is_relative_to(source) or not path.is_file():
            raise RuntimeError("Frontend station asset is missing or outside its checkout")
        digest.update(str(relative).encode("utf-8"))
        digest.update(b"\0")
        digest.update(path.read_bytes())
    return digest.hexdigest()


def _browser_source_receipt(repo_root: Path) -> Path:
    return repo_root / "frontend/dist/station-source.json"


def prepare_browser_asset_source(repo_root: Path, frontend_dev: bool = False, refresh: bool = True) -> Path:
    explicit = os.environ.get(BROWSER_ASSET_SOURCE_ENV, "").strip()
    if explicit:
        source = Path(explicit).expanduser().resolve()
        _browser_asset_digest(source)
    elif not refresh and _browser_source_receipt(repo_root).is_file():
        receipt = json.loads(_browser_source_receipt(repo_root).read_text(encoding="utf-8"))
        source = Path(receipt["source_dir"]).resolve()
        if receipt.get("version") != 1 or _browser_asset_digest(source) != receipt.get("asset_digest"):
            raise RuntimeError("Frontend station assets changed since the WebUI build. Rebuild the WebUI before reusing it.")
    else:
        source = _frontend_checkout_dir()
        branch = _frontend_branch(frontend_dev)
        print(f"Frontend source: {FRONTEND_REPO_URL} branch {branch}")
        source = _immutable_frontend_checkout(source, branch)
    source = source.resolve()
    _browser_asset_digest(source)
    os.environ[BROWSER_ASSET_SOURCE_ENV] = str(source)
    return source


def _record_browser_asset_source(repo_root: Path, source: Path) -> None:
    receipt = _browser_source_receipt(repo_root)
    receipt.parent.mkdir(parents=True, exist_ok=True)
    value = {"version": 1, "source_dir": str(source.resolve()), "asset_digest": _browser_asset_digest(source)}
    temporary = receipt.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    temporary.replace(receipt)


def _run_frontend_build(
        repo_root: Path,
        debug_mode: bool,
        max_size_mode: bool,
        use_existing: bool,
        log_file_arg: Optional[str],
        frontend_dev: bool,
        checkout_dir: Optional[Path] = None,
) -> None:
    if checkout_dir is None:
        checkout_dir = prepare_browser_asset_source(repo_root, frontend_dev, refresh=True)

    script = _resolve_external_frontend_script(checkout_dir)
    args = ["frontend_web"]
    if debug_mode:
        args.append("debug")
    if max_size_mode:
        args.append("max_size")
    if use_existing:
        args.append("existing")
    if log_file_arg:
        log_path = Path(log_file_arg)
        if not log_path.is_absolute():
            log_path = repo_root / log_path
        args.append(f"log={log_path}")

    def selected_build(source: Path, selected_script: Path, target_override=None) -> None:
        def finish() -> None:
            _sync_frontend_public_assets(repo_root, source)
            _record_browser_asset_source(repo_root, source)
        kwargs = {"publication_destination": _frontend_sync_public_dir(repo_root)}
        if target_override is not None:
            kwargs["target_override"] = target_override
        _run_frontend_script_with_wasm_opt_fallback(selected_script, source, args, finish, **kwargs)

    # Global order: destination, checkout output, then target lease. The callback takes
    # neither lock. Different targets cannot race while publishing one WebUI.
    with _frontend_cache_lock(repo_root / "frontend" / ".web-build.lock"):
        try:
            selected_build(checkout_dir, script)
        except FrontendBuildInterrupted as error:
            shared = _frontend_shared_target(checkout_dir.resolve())
            # Preserve explicit target/source intent and reuse-only requests.
            # Neither marker contents nor a stale PID can establish safe cleanup.
            if shared is None or os.environ.get("CARGO_TARGET_DIR") or use_existing:
                raise
            recovery_target = _frontend_recovery_target(shared)
            source = _frontend_recovery_checkout(checkout_dir)
            message = f"Preserving interrupted frontend output/cache. One isolated retry: source={source}, target={recovery_target}"
            print(message)
            _append_log(message + "\n")
            try:
                selected_build(source, _resolve_external_frontend_script(source), recovery_target)
            except FrontendBuildInterrupted as isolated:
                raise RuntimeError("The fixed frontend recovery source/target is also interrupted. "
                                   "All markers, writers and quarantines were preserved; no further caches were allocated. "
                                   "Confirm prior writers stopped before manual cleanup, or use an explicit separate source and target.") from isolated
            # Keep backend asset preparation and the published source receipt on
            # the effective successful source, not the uncertain original output.
            os.environ[BROWSER_ASSET_SOURCE_ENV] = str(source.resolve())


def _backend_script(repo_root: Path) -> Path:
    return repo_root / "backend" / "build.py"


def _backend_args(
        *,
        force_pi: bool,
        force_no_pi: bool,
        testing_mode: bool,
        hitl_mode: bool,
        test_fire_mode: bool,
        debug_mode: bool,
        log_file_arg: Optional[str] = None,
) -> list[str]:
    args: list[str] = []
    if force_pi:
        args.append("pi_build")
    if force_no_pi:
        args.append("no_pi")
    # One backend binary holds every station mode; the mode is chosen at run
    # time (GS_STATION_MODE or the UI), so mode arguments add no features.
    del testing_mode, hitl_mode, test_fire_mode
    if debug_mode:
        args.append("debug")
    if log_file_arg:
        args.append(f"log={log_file_arg}")
    return args


def build_video_transcoder(repo_root: Path, debug_mode: bool) -> None:
    """Build the standalone native helper without fetching/building the station."""
    manifest = repo_root / "video_transcoder" / "Cargo.toml"
    lock = manifest.with_name("Cargo.lock")
    if not lock.is_file():
        raise FileNotFoundError(
            errno.ENOENT,
            "Restore the tracked video_transcoder/Cargo.lock before building the helper",
            str(lock),
        )
    command = ["cargo", "build", "--locked", "--manifest-path", str(manifest)]
    if not debug_mode:
        command.append("--release")
    run(command, cwd=repo_root)


def print_usage(exit_code: int = 1) -> None:
    print("Usage:")
    print("  ./build.py                         # local: build frontend+backend (parallel)")
    print("  ./build.py pi_build                # local: backend w/ raspberry_pi feature")
    print("  ./build.py no_pi                   # local: backend w/o raspberry_pi feature")
    print("  ./build.py testing|hitl-mode|test-fire-mode  # accepted; same binary (modes are chosen at run time)")
    print("  ./build.py debug                   # local: build frontend+backend in debug mode")
    print("  ./build.py max_size                # web wasm: add wasm-opt --converge (slower, smaller)")
    print("  ./build.py --frontend-dev          # build frontend from dev instead of stable main")
    print("  ./build.py plain                   # docker only: pass --progress plain")
    print("  ./build.py log=build.log           # tee command output into a log file")
    print("  ./build.py --remote HOST [...]     # compile backend and web UI on HOST (user:password@host, user@host or host)")
    print("                                     # in an Ubuntu 24.04 container, then copy the results back (needs ssh, rsync, docker)")
    print("  ./build.py docker [pi_build|no_pi] [testing]")
    print("  ./build.py rrls                   # headless recovery backend + web UI, no local launch GPIO")
    print("  ./build.py --backend-only          # local: build backend only (backend_only also accepted)")
    print("  ./build.py video_transcoder [debug] # separate native video helper only (Linux/macOS/Windows)")
    print("  ./build.py frontend_web            # local: build frontend web only")
    print("  ./build.py web                     # alias for frontend_web")
    print("  ./backend/build.py ...             # backend-only entry point")
    print("")
    print("Frontend checkout:")
    print(f"  default checkout path is {FRONTEND_CHECKOUT_ENV} or {_frontend_checkout_dir()}")
    print("  the checkout is cloned only when absent and otherwise updated with `git pull --ff-only`")
    print("  local changes in the external checkout abort the update instead of being modified")
    print(f"  frontend source defaults to '{FRONTEND_STABLE_BRANCH}'; --frontend-dev selects '{FRONTEND_DEV_BRANCH}'")
    sys.exit(exit_code)


def main() -> None:
    force_pi = False
    force_no_pi = False
    docker_mode = False
    testing_mode = False
    hitl_mode = False
    test_fire_mode = False
    debug_mode = False
    max_size_mode = False
    plain_mode = False
    use_existing = False
    frontend_dev = False
    backend_only = False
    rrls_mode = False
    video_transcoder = False
    log_file_arg: Optional[str] = None
    frontend_only_platform: Optional[str] = None

    raw_args = [a.strip() for a in sys.argv[1:]]
    if any(a in {"-h", "--help", "help"} for a in raw_args):
        print_usage(0)
    # Before lowercasing: a password in the host is case-sensitive. The
    # backend and frontend children read it from the environment.
    raw_args, remote = _remote_build().take_argument(raw_args)
    if remote:
        os.environ[_remote_build().ENV] = remote

    frontend_platforms = {"web", "frontend_web"}

    for raw_arg in raw_args:
        arg = raw_arg.lower()
        if arg == "video_transcoder":
            video_transcoder = True
        elif arg == "rrls":
            rrls_mode = True
            force_no_pi = True
        elif arg == "pi_build":
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
        elif arg == "test-fire-mode":
            test_fire_mode = True
        elif arg == "debug":
            debug_mode = True
        elif arg == "max_size":
            max_size_mode = True
        elif arg in {"backend_only", "backend-only", "--backend_only", "--backend-only", "backend"}:
            backend_only = True
        elif arg == "existing":
            use_existing = True
        elif arg in {"--frontend-dev", "frontend-dev", "frontend_dev"}:
            frontend_dev = True
        elif arg.startswith("log="):
            value = raw_arg.split("=", 1)[1].strip()
            if not value:
                print("Error: log= requires a filepath.", file=sys.stderr)
                print_usage()
            log_file_arg = value
        elif arg in frontend_platforms:
            if frontend_only_platform or backend_only:
                print("Error: Only one frontend action/build may be specified.", file=sys.stderr)
                print_usage()
            frontend_only_platform = "web" if arg in {"web", "frontend_web"} else arg
        else:
            print(f"Error: Invalid argument '{arg}'.", file=sys.stderr)
            print_usage()

    if video_transcoder and any((
        force_pi, force_no_pi, docker_mode, testing_mode, hitl_mode,
        test_fire_mode, max_size_mode, plain_mode, use_existing, frontend_dev,
        backend_only, rrls_mode, frontend_only_platform is not None,
    )):
        print("Error: video_transcoder accepts only debug and log=; station/frontend modes are separate.", file=sys.stderr)
        print_usage()

    if rrls_mode:
        print("RRLS build: no local launch GPIO. Run with GS_RRLS_ROLE=rrls or run_groundstation.py --rrls.")
    if force_pi and force_no_pi:
        print("Error: Cannot specify both 'pi_build' and 'no_pi'.", file=sys.stderr)
        sys.exit(1)
    selected_modes = sum([testing_mode, hitl_mode, test_fire_mode])
    if selected_modes > 1:
        print("Error: Cannot specify more than one of 'testing', 'hitl-mode', and 'test-fire-mode'.", file=sys.stderr)
        sys.exit(1)
    if selected_modes and not docker_mode:
        print("Note: one backend binary holds every station mode. Start it with GS_STATION_MODE=testing|hitl|test_fire "
              "or run_groundstation.py --testing/--hitl-mode/--test-fire-mode, or switch from the UI.")

    repo_root = Path(__file__).resolve().parent
    _configure_log_file(repo_root, log_file_arg)
    if video_transcoder:
        build_video_transcoder(repo_root, debug_mode)
        return


    if frontend_only_platform is not None:
        if docker_mode or force_pi or force_no_pi or testing_mode or hitl_mode or test_fire_mode:
            print("Error: Frontend-only builds cannot be combined with docker/pi_build/no_pi/testing/hitl-mode/test-fire-mode.",
                  file=sys.stderr)
            print_usage()
        _run_frontend_build(
            repo_root=repo_root,
            debug_mode=debug_mode,
            max_size_mode=max_size_mode,
            use_existing=use_existing,
            log_file_arg=log_file_arg,
            frontend_dev=frontend_dev,
        )
        return

    if backend_only:
        if docker_mode:
            print("Error: backend_only cannot be combined with docker mode.", file=sys.stderr)
            print_usage()
        if frontend_dev:
            _run_frontend_build(repo_root, debug_mode, max_size_mode, use_existing, log_file_arg, True)
        prepare_browser_asset_source(repo_root, refresh=False)
        _run_script(
            repo_root,
            _backend_script(repo_root),
            _backend_args(
                force_pi=force_pi,
                force_no_pi=force_no_pi,
                testing_mode=testing_mode,
                hitl_mode=hitl_mode,
                test_fire_mode=test_fire_mode,
                debug_mode=debug_mode,
                log_file_arg=log_file_arg,
            ),
        )
        return

    if docker_mode:
        if hitl_mode or test_fire_mode:
            print("Error: docker images start in normal or testing mode only ('testing'); switch other modes from the UI.", file=sys.stderr)
            sys.exit(1)
        pi_build_flag = False if force_no_pi else (force_pi or is_raspberry_pi())
        use_plain = plain_mode or (LOG_FILE is not None)
        print(
            "Note: docker image builds cannot be post-processed with host wasm-opt; optimize in Dockerfile for image "
            "artifacts.")
        build_docker(
            repo_root=repo_root,
            pi_build=pi_build_flag,
            testing=testing_mode,
            plain_progress=use_plain,
            frontend_dev=frontend_dev,
        )
        return

    backend_args = _backend_args(
        force_pi=force_pi,
        force_no_pi=force_no_pi,
        testing_mode=testing_mode,
        hitl_mode=hitl_mode,
        test_fire_mode=test_fire_mode,
        debug_mode=debug_mode,
        log_file_arg=log_file_arg,
    )

    # Resolve one checkout before starting either process. Both backend exports
    # and the WebUI build read this exact source; children do not fetch again.
    selected_frontend = prepare_browser_asset_source(repo_root, frontend_dev, refresh=True)
    if in_docker_build():
        print("Sequential build")
        _run_frontend_build(
            repo_root=repo_root,
            debug_mode=debug_mode,
            max_size_mode=max_size_mode,
            use_existing=use_existing,
            log_file_arg=log_file_arg,
            frontend_dev=frontend_dev,
            checkout_dir=selected_frontend,
        )
        _run_script(repo_root, _backend_script(repo_root), backend_args)
        return

    frontend_proc = mp.Process(
        target=_run_frontend_build,
        args=(repo_root, debug_mode, max_size_mode, use_existing, log_file_arg, frontend_dev, selected_frontend),
    )
    backend_proc = mp.Process(
        target=_run_script,
        args=(repo_root, _backend_script(repo_root), backend_args),
    )
    frontend_proc.start()
    backend_proc.start()
    try:
        frontend_proc.join()
        backend_proc.join()
    except KeyboardInterrupt:
        for proc in (frontend_proc, backend_proc):
            if proc.is_alive():
                proc.terminate()
        for proc in (frontend_proc, backend_proc):
            proc.join(timeout=5)
            if proc.is_alive():
                proc.kill()
                proc.join()
        raise
    if frontend_proc.exitcode not in {0, None}:
        sys.exit(frontend_proc.exitcode or 1)
    if backend_proc.exitcode not in {0, None}:
        sys.exit(backend_proc.exitcode or 1)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nBuild interrupted.", file=sys.stderr)
        sys.exit(INTERRUPTED_EXIT_CODE)
    except FileNotFoundError as e:
        missing = e.filename or "<unknown>"
        print("\nError: build failed because a required tool/file is missing.", file=sys.stderr)
        print(f"  Missing: {missing}", file=sys.stderr)
        sys.exit(127)
    except subprocess.CalledProcessError as e:
        print("\nError: build command failed.", file=sys.stderr)
        print(f"  Command : {' '.join(str(x) for x in e.cmd)}", file=sys.stderr)
        print(f"  Exit    : {e.returncode}", file=sys.stderr)
        sys.exit(e.returncode)
