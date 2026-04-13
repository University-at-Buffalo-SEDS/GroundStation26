#!/usr/bin/env python3
import multiprocessing as mp
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path
from subprocess import DEVNULL
from typing import Optional

LOG_FILE: Optional[Path] = None
INTERRUPTED_EXIT_CODE = 130
FRONTEND_REPO_URL = "https://github.com/Rylan-Meilutis/Seds-Ground-Station-Frontend"
FRONTEND_CHECKOUT_ENV = "GS26_FRONTEND_CHECKOUT_DIR"
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


def _run_script(repo_root: Path, script: Path, args: list[str]) -> None:
    run([sys.executable, str(script), *args], cwd=repo_root)


def _run_frontend_script_with_wasm_opt_fallback(script: Path, checkout_dir: Path, args: list[str]) -> None:
    cmd = [sys.executable, str(script), *args]
    cmd_line = f"Running: {' '.join(str(part) for part in cmd)} (cwd={checkout_dir})"
    print(cmd_line)
    _append_log(cmd_line + "\n")

    proc = subprocess.Popen(
        [str(part) for part in cmd],
        cwd=checkout_dir,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )
    assert proc.stdout is not None

    lines: list[str] = []
    try:
        for line in proc.stdout:
            print(line, end="")
            _append_log(line)
            lines.append(line)
        rc = proc.wait()
    except KeyboardInterrupt:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        raise

    if rc == 0:
        return

    combined_output = "".join(lines).lower()
    public_dir = _resolve_external_public_dir(checkout_dir)
    if public_dir.is_dir() and any(hint in combined_output for hint in WASM_OPT_FAILURE_HINTS):
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


def _ensure_frontend_checkout(checkout_dir: Path) -> None:
    if not checkout_dir.exists():
        checkout_dir.parent.mkdir(parents=True, exist_ok=True)
        run(["git", "clone", "--depth", "1", FRONTEND_REPO_URL, str(checkout_dir)], cwd=checkout_dir.parent)
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

    run(["git", "-C", str(checkout_dir), "pull", "--ff-only"], cwd=checkout_dir)


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


def _run_frontend_build(
        repo_root: Path,
        debug_mode: bool,
        max_size_mode: bool,
        use_existing: bool,
        log_file_arg: Optional[str],
) -> None:
    checkout_dir = _frontend_checkout_dir()
    _ensure_frontend_checkout(checkout_dir)

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

    _run_frontend_script_with_wasm_opt_fallback(script, checkout_dir, args)
    _sync_frontend_public_assets(repo_root, checkout_dir)


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
    if testing_mode:
        args.append("testing")
    if hitl_mode:
        args.append("hitl-mode")
    if test_fire_mode:
        args.append("test-fire-mode")
    if debug_mode:
        args.append("debug")
    if log_file_arg:
        args.append(f"log={log_file_arg}")
    return args


def print_usage(exit_code: int = 1) -> None:
    print("Usage:")
    print("  ./build.py                         # local: build frontend+backend (parallel)")
    print("  ./build.py pi_build                # local: backend w/ raspberry_pi feature")
    print("  ./build.py no_pi                   # local: backend w/o raspberry_pi feature")
    print("  ./build.py testing                 # local: backend w/ testing feature")
    print("  ./build.py hitl-mode               # local: backend w/ hitl_mode feature")
    print("  ./build.py test-fire-mode          # local: backend w/ test_fire_mode feature")
    print("  ./build.py debug                   # local: build frontend+backend in debug mode")
    print("  ./build.py max_size                # web wasm: add wasm-opt --converge (slower, smaller)")
    print("  ./build.py plain                   # docker only: pass --progress plain")
    print("  ./build.py log=build.log           # tee command output into a log file")
    print("  ./build.py docker [pi_build|no_pi] [testing]")
    print("  ./build.py backend_only            # local: build backend only")
    print("  ./build.py frontend_web            # local: build frontend web only")
    print("  ./build.py web                     # alias for frontend_web")
    print("  ./backend/build.py ...             # backend-only entry point")
    print("")
    print("Frontend checkout:")
    print(f"  default checkout path is {FRONTEND_CHECKOUT_ENV} or {_frontend_checkout_dir()}")
    print("  the checkout is cloned only when absent and otherwise updated with `git pull --ff-only`")
    print("  local changes in the external checkout abort the update instead of being modified")
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
    backend_only = False
    log_file_arg: Optional[str] = None
    frontend_only_platform: Optional[str] = None

    raw_args = [a.strip() for a in sys.argv[1:]]
    if any(a in {"-h", "--help", "help"} for a in raw_args):
        print_usage(0)

    frontend_platforms = {"web", "frontend_web"}

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
        elif arg == "test-fire-mode":
            test_fire_mode = True
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
        elif arg in frontend_platforms:
            if frontend_only_platform or backend_only:
                print("Error: Only one frontend action/build may be specified.", file=sys.stderr)
                print_usage()
            frontend_only_platform = "web" if arg in {"web", "frontend_web"} else arg
        else:
            print(f"Error: Invalid argument '{arg}'.", file=sys.stderr)
            print_usage()

    if force_pi and force_no_pi:
        print("Error: Cannot specify both 'pi_build' and 'no_pi'.", file=sys.stderr)
        sys.exit(1)
    selected_modes = sum([testing_mode, hitl_mode, test_fire_mode])
    if selected_modes > 1:
        print("Error: Cannot specify more than one of 'testing', 'hitl-mode', and 'test-fire-mode'.", file=sys.stderr)
        sys.exit(1)

    repo_root = Path(__file__).resolve().parent
    _configure_log_file(repo_root, log_file_arg)

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
        )
        return

    if backend_only:
        if docker_mode:
            print("Error: backend_only cannot be combined with docker mode.", file=sys.stderr)
            print_usage()
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
            print("Error: docker mode currently does not support 'hitl-mode' or 'test-fire-mode'.", file=sys.stderr)
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

    if in_docker_build():
        print("Sequential build")
        _run_frontend_build(
            repo_root=repo_root,
            debug_mode=debug_mode,
            max_size_mode=max_size_mode,
            use_existing=use_existing,
            log_file_arg=log_file_arg,
        )
        _run_script(repo_root, _backend_script(repo_root), backend_args)
        return

    frontend_proc = mp.Process(
        target=_run_frontend_build,
        args=(repo_root, debug_mode, max_size_mode, use_existing, log_file_arg),
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
