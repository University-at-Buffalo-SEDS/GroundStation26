#!/usr/bin/env python3
import subprocess
import sys
from pathlib import Path
from typing import Optional

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


def _print_missing_tool(context: str, err: FileNotFoundError, cwd: Path) -> None:
    missing = err.filename or "<unknown>"
    print(f"\nError: {context} could not start because a required tool is missing.", file=sys.stderr)
    print(f"  Missing : {missing}", file=sys.stderr)
    print(f"  CWD     : {cwd}", file=sys.stderr)
    if LOG_FILE is not None:
        print(f"  Log file: {LOG_FILE}", file=sys.stderr)


def run(cmd: list[str], cwd: Path) -> None:
    cmd = [str(part) for part in cmd]
    cmd_line = f"Running: {' '.join(cmd)} (cwd={cwd})"
    print(cmd_line)
    _append_log(cmd_line + "\n")
    if LOG_FILE is None:
        subprocess.run(cmd, cwd=cwd, check=True)
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
    for line in proc.stdout:
        print(line, end="")
        _append_log(line)
    rc = proc.wait()
    if rc != 0:
        raise subprocess.CalledProcessError(rc, cmd)


def is_raspberry_pi() -> bool:
    import platform

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
    print("Backend build script")
    print("")
    print("Usage:")
    print("  ./backend/build.py [pi_build|no_pi] [testing|hitl-mode] [debug] [log=<path>]")
    print("")
    print("What this script owns:")
    print("  - backend cargo builds")
    print("  - Raspberry Pi feature selection")
    print("  - testing and hitl_mode feature toggles")
    print("")
    print("Options:")
    print("  pi_build                          # force raspberry_pi feature on")
    print("  no_pi                             # force raspberry_pi feature off")
    print("  testing                           # enable backend testing feature")
    print("  hitl-mode                         # enable backend hitl_mode feature")
    print("  debug                             # build without --release")
    print("  log=<path>                        # tee command output into a log file")
    sys.exit(exit_code)


def main() -> None:
    raw_args = [a.strip() for a in sys.argv[1:]]
    if any(a in {"-h", "--help", "help"} for a in raw_args):
        print_usage(0)

    force_pi = False
    force_no_pi = False
    testing_mode = False
    hitl_mode = False
    debug_mode = False
    log_file_arg: Optional[str] = None

    for raw_arg in raw_args:
        arg = raw_arg.lower()
        if arg == "pi_build":
            force_pi = True
        elif arg == "no_pi":
            force_no_pi = True
        elif arg == "testing":
            testing_mode = True
        elif arg == "hitl-mode":
            hitl_mode = True
        elif arg == "debug":
            debug_mode = True
        elif arg.startswith("log="):
            value = raw_arg.split("=", 1)[1].strip()
            if not value:
                print("Error: log= requires a filepath.", file=sys.stderr)
                print_usage()
            log_file_arg = value
        else:
            print(f"Error: Invalid argument '{arg}'.", file=sys.stderr)
            print_usage()

    if force_pi and force_no_pi:
        print("Error: Cannot specify both 'pi_build' and 'no_pi'.", file=sys.stderr)
        sys.exit(1)
    if testing_mode and hitl_mode:
        print("Error: Cannot specify both 'testing' and 'hitl-mode'.", file=sys.stderr)
        sys.exit(1)

    repo_root = Path(__file__).resolve().parents[1]
    backend_dir = repo_root / "backend"
    _configure_log_file(repo_root, log_file_arg)
    build_backend(backend_dir, force_pi, force_no_pi, testing_mode, hitl_mode, debug_mode)


if __name__ == "__main__":
    try:
        main()
    except FileNotFoundError as e:
        missing = e.filename or "<unknown>"
        print("\nError: backend build failed because a required tool/file is missing.", file=sys.stderr)
        print(f"  Missing: {missing}", file=sys.stderr)
        sys.exit(127)
    except subprocess.CalledProcessError as e:
        print("\nError: backend build command failed.", file=sys.stderr)
        print(f"  Command : {' '.join(str(x) for x in e.cmd)}", file=sys.stderr)
        print(f"  Exit    : {e.returncode}", file=sys.stderr)
        sys.exit(e.returncode)
