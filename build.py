#!/usr/bin/env python3
import multiprocessing as mp
import os
import platform
import subprocess
import sys
from pathlib import Path
from subprocess import DEVNULL
from typing import Optional

LOG_FILE: Optional[Path] = None
INTERRUPTED_EXIT_CODE = 130


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


def _frontend_script(repo_root: Path) -> Path:
    return repo_root / "frontend" / "build.py"


def _backend_script(repo_root: Path) -> Path:
    return repo_root / "backend" / "build.py"


def _frontend_args(
        *,
        platform_name: Optional[str],
        debug_mode: bool,
        max_size_mode: bool,
        android_package_type: Optional[str],
        use_existing: bool,
        action: Optional[str] = None,
        log_file_arg: Optional[str] = None,
        screenshot_delay_arg: Optional[str] = None,
        screenshot_out_arg: Optional[str] = None,
        screenshot_name_arg: Optional[str] = None,
        desktop_window_arg: Optional[str] = None,
        ios_window_arg: Optional[str] = None,
        android_window_arg: Optional[str] = None,
) -> list[str]:
    args: list[str] = []
    if action is not None:
        args.append(action)
    elif platform_name is not None:
        args.append("frontend_web" if platform_name == "web" else platform_name)
    if debug_mode:
        args.append("debug")
    if max_size_mode:
        args.append("max_size")
    if android_package_type:
        args.append(android_package_type)
    if use_existing:
        args.append("existing")
    if log_file_arg:
        args.append(f"log={log_file_arg}")
    if screenshot_delay_arg:
        args.append(f"screenshot_delay={screenshot_delay_arg}")
    if screenshot_out_arg:
        args.append(f"screenshot_out={screenshot_out_arg}")
    if screenshot_name_arg:
        args.append(f"screenshot_name={screenshot_name_arg}")
    if desktop_window_arg:
        args.append(f"desktop_window={desktop_window_arg}")
    if ios_window_arg:
        args.append(f"ios_window={ios_window_arg}")
    if android_window_arg:
        args.append(f"android_window={android_window_arg}")
    return args


def _backend_args(
        *,
        force_pi: bool,
        force_no_pi: bool,
        testing_mode: bool,
        hitl_mode: bool,
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
    print("  ./build.py debug                   # local: build frontend+backend in debug mode")
    print("  ./build.py max_size                # web wasm: add wasm-opt --converge (slower, smaller)")
    print("  ./build.py plain                   # docker only: pass --progress plain")
    print("  ./build.py log=build.log           # tee command output into a log file")
    print("  ./build.py docker [pi_build|no_pi] [testing]")
    print("  ./build.py backend_only            # local: build backend only")
    print("  ./build.py frontend_web            # local: build frontend web only")
    print("  ./frontend/build.py ...            # frontend-only entry point")
    print("  ./backend/build.py ...             # backend-only entry point")
    print("")
    print("Frontend-only builds:")
    print("  ./build.py ios")
    print("  ./build.py ios_sim")
    print("  ./build.py macos")
    print("  ./build.py windows")
    print("  ./build.py android [apk|aab]")
    print("  ./build.py linux")
    print("  Linux frontend builds emit AppImage/deb/rpm/Arch/Flatpak when supporting tools are installed")
    print("  (add `debug` to frontend/local builds to skip --release)")
    print("")
    print("Frontend actions:")
    print("  ./build.py ios_deploy")
    print("  ./build.py ios_sim_deploy")
    print("  ./build.py ios_sign")
    print("  ./build.py ios_dist_sign")
    print("  ./build.py android_install")
    print("  ./build.py publisher_screenshots")
    print("  ./build.py macos_deploy")
    print("  ./build.py macos_sign")
    print("  ./build.py macos_notarize")
    print("  ./build.py publisher_screenshots screenshot_out=artifacts/screenshots")
    print("  ./build.py publisher_screenshots desktop_window=1440x900 ios_window=1290x2796 android_window=1080x1920")
    sys.exit(exit_code)


def main() -> None:
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
    screenshot_delay_arg: Optional[str] = None
    screenshot_out_arg: Optional[str] = None
    screenshot_name_arg: Optional[str] = None
    desktop_window_arg: Optional[str] = None
    ios_window_arg: Optional[str] = None
    android_window_arg: Optional[str] = None
    frontend_only_platform: Optional[str] = None
    action: Optional[str] = None

    raw_args = [a.strip() for a in sys.argv[1:]]
    if any(a in {"-h", "--help", "help"} for a in raw_args):
        print_usage(0)

    frontend_platforms = {"ios", "ios_sim", "macos", "windows", "android", "linux", "web", "frontend_web"}
    frontend_actions = {
        "android_install",
        "ios_deploy",
        "ios_sim_deploy",
        "ios_sign",
        "ios_dist_sign",
        "macos_deploy",
        "macos_sign",
        "macos_notarize",
        "publisher_screenshots",
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
        elif arg in {"apk", "aab"}:
            android_package_type = arg
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
        elif arg in frontend_actions:
            if action or frontend_only_platform or backend_only:
                print("Error: Only one frontend action/build may be specified.", file=sys.stderr)
                print_usage()
            action = arg
        elif arg in frontend_platforms:
            if action or frontend_only_platform or backend_only:
                print("Error: Only one frontend action/build may be specified.", file=sys.stderr)
                print_usage()
            frontend_only_platform = "web" if arg in {"web", "frontend_web"} else arg
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
    _configure_log_file(repo_root, log_file_arg)

    if action:
        if docker_mode or force_pi or force_no_pi or testing_mode or hitl_mode:
            print("Error: Frontend actions cannot be combined with docker/pi_build/no_pi/testing/hitl-mode.",
                  file=sys.stderr)
            print_usage()
        _run_script(
            repo_root,
            _frontend_script(repo_root),
            _frontend_args(
                platform_name=None,
                debug_mode=debug_mode,
                max_size_mode=max_size_mode,
                android_package_type=android_package_type,
                use_existing=use_existing,
                action=action,
                log_file_arg=log_file_arg,
                screenshot_delay_arg=screenshot_delay_arg,
                screenshot_out_arg=screenshot_out_arg,
                screenshot_name_arg=screenshot_name_arg,
                desktop_window_arg=desktop_window_arg,
                ios_window_arg=ios_window_arg,
                android_window_arg=android_window_arg,
            ),
        )
        return

    if frontend_only_platform is not None:
        if docker_mode or force_pi or force_no_pi or testing_mode or hitl_mode:
            print("Error: Frontend-only builds cannot be combined with docker/pi_build/no_pi/testing/hitl-mode.",
                  file=sys.stderr)
            print_usage()
        _run_script(
            repo_root,
            _frontend_script(repo_root),
            _frontend_args(
                platform_name=frontend_only_platform,
                debug_mode=debug_mode,
                max_size_mode=max_size_mode,
                android_package_type=android_package_type,
                use_existing=use_existing,
                log_file_arg=log_file_arg,
                screenshot_delay_arg=screenshot_delay_arg,
                screenshot_out_arg=screenshot_out_arg,
                screenshot_name_arg=screenshot_name_arg,
                desktop_window_arg=desktop_window_arg,
                ios_window_arg=ios_window_arg,
                android_window_arg=android_window_arg,
            ),
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
                debug_mode=debug_mode,
                log_file_arg=log_file_arg,
            ),
        )
        return

    if docker_mode:
        if hitl_mode:
            print("Error: docker mode currently does not support 'hitl-mode'.", file=sys.stderr)
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

    frontend_args = _frontend_args(
        platform_name="web",
        debug_mode=debug_mode,
        max_size_mode=max_size_mode,
        android_package_type=android_package_type,
        use_existing=use_existing,
        log_file_arg=log_file_arg,
        screenshot_delay_arg=screenshot_delay_arg,
        screenshot_out_arg=screenshot_out_arg,
        screenshot_name_arg=screenshot_name_arg,
        desktop_window_arg=desktop_window_arg,
        ios_window_arg=ios_window_arg,
        android_window_arg=android_window_arg,
    )
    backend_args = _backend_args(
        force_pi=force_pi,
        force_no_pi=force_no_pi,
        testing_mode=testing_mode,
        hitl_mode=hitl_mode,
        debug_mode=debug_mode,
        log_file_arg=log_file_arg,
    )

    if in_docker_build():
        print("Sequential build")
        _run_script(repo_root, _frontend_script(repo_root), frontend_args)
        _run_script(repo_root, _backend_script(repo_root), backend_args)
        return

    frontend_proc = mp.Process(
        target=_run_script,
        args=(repo_root, _frontend_script(repo_root), frontend_args),
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
