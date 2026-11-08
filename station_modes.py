#!/usr/bin/env python3
"""Station mode supervisor: one installed backend binary per mode.

Modes are compile-time Cargo features, so switching mode means restarting into
another build. The backend's /api/station-modes Confirm saves
``<state dir>/selection.json`` and exits with RESTART_EXIT_CODE; this
supervisor (or systemd, through ``exec``) then starts the selected binary.

  python3 station_modes.py build --modes all            # build every mode
  python3 station_modes.py exec [--rrls] [--default-mode hitl]   # systemd ExecStart

``run_groundstation.py --switchable-modes ...`` builds and supervises in one
step. Without a saved selection the default mode (the launcher's usual
choice) runs, so existing behaviour is unchanged.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Mapping

RESTART_EXIT_CODE = 75
# Mode id (backend station_mode::Mode, snake_case) -> backend Cargo feature,
# as build.py / backend/build.py map testing, hitl-mode and test-fire-mode.
MODE_FEATURES: dict[str, list[str]] = {
    "normal": [],
    "hitl": ["hitl_mode"],
    "test_fire": ["test_fire_mode"],
    "testing": ["testing"],
}
MODES = tuple(MODE_FEATURES)
BINARY = "groundstation_backend"
SELECTION_FILE = "selection.json"
# Mirrors backend rrls_sim_primary::ISOLATED. A simulated-primary RRLS keeps
# every paired, mode and primary-derived file inside its simulation root.
RRLS_SIM_ISOLATED = {
    "GS_STATION_PAIRING_FILE": "rrls/pairing.json",
    "GS_STATION_PAIRING_DIR": "managed-pairing",
    "GS_STATION_MODE_FILE": "station-mode.json",
    "GS_RRLS_CONFIGURATION_FILE": "retained-configuration.json",
    "GS_MISSION_SETTINGS_PATH": "mission-settings.json",
    "GS_LAUNCH_CLOCK_STATE": "launch_clock.json",
    "GS_BOARD_ABORT_HISTORY_DIR": "board_abort_history",
}


def simulation_supported(mode: str, rrls: bool) -> bool:
    """The primary's simulation is the testing build's flight model; an RRLS
    can simulate its paired primary in any build."""
    return rrls or mode == "testing"


def parse_modes(text: str) -> list[str]:
    if text.strip() == "all":
        return list(MODES)
    modes = []
    for item in text.split(","):
        item = item.strip().replace("-", "_")
        if item in ("hitl_mode", "test_fire_mode"):
            item = item.removesuffix("_mode")
        if item not in MODE_FEATURES:
            raise ValueError(f"unknown station mode {item!r}; choose from {', '.join(MODES)} or all")
        if item not in modes:
            modes.append(item)
    return modes


def binary_root(repo_root: Path, debug: bool = False) -> Path:
    return repo_root / "target" / "station-modes" / ("debug" if debug else "release")


def binary_path(repo_root: Path, mode: str, debug: bool = False) -> Path:
    return binary_root(repo_root, debug) / mode / BINARY


def installed_modes(root: Path) -> list[str]:
    return [mode for mode in MODES if (root / mode / BINARY).is_file() and os.access(root / mode / BINARY, os.X_OK)]


def state_dir(repo_root: Path, environ: Mapping[str, str]) -> Path:
    configured = environ.get("GS_STATION_MODE_DIR")
    return Path(configured) if configured else repo_root / "data" / "station-mode"


@dataclass(frozen=True)
class Choice:
    mode: str
    simulation: bool
    reason: str


def read_selection(directory: Path) -> dict | None:
    """The saved selection, or None if absent or unusable (never raises)."""
    try:
        value = json.loads((directory / SELECTION_FILE).read_text(encoding="utf-8"))
    except FileNotFoundError:
        return None
    except (OSError, ValueError) as error:
        print(f"Warning: ignoring unreadable station mode selection: {error}", file=sys.stderr)
        return None
    if not isinstance(value, dict) or value.get("version") != 1:
        print("Warning: ignoring station mode selection with an unknown version", file=sys.stderr)
        return None
    return value


def choose(selection: dict | None, installed: list[str], default_mode: str, rrls: bool, default_simulation: bool | None = None) -> Choice:
    """Pick the binary to run. Without a usable selection the launcher's
    default runs, so behaviour is unchanged until a switch is confirmed."""
    role = "rrls" if rrls else "primary"
    if selection is not None:
        mode, simulation = selection.get("mode"), selection.get("simulation")
        if selection.get("role") != role:
            pass
        elif mode not in MODE_FEATURES or not isinstance(simulation, bool):
            print("Warning: ignoring malformed station mode selection", file=sys.stderr)
        elif mode not in installed:
            print(f"Warning: selected mode {mode} is not installed; starting the default {default_mode}", file=sys.stderr)
        elif simulation and not simulation_supported(mode, rrls):
            print(f"Warning: {mode} has no simulation on this station; starting it without", file=sys.stderr)
            return Choice(mode, False, "selection (simulation unsupported)")
        else:
            return Choice(mode, simulation, "selection")
    return Choice(default_mode, bool(default_simulation), "default")


def child_env(base: Mapping[str, str], choice: Choice, rrls: bool, installed: list[str], directory: Path, sim_root: Path) -> dict[str, str]:
    env = dict(base)
    env["GS_STATION_MODE_SUPERVISOR"] = "1"
    env["GS_STATION_MODE_AVAILABLE"] = ",".join(installed)
    env["GS_STATION_MODE_DIR"] = str(directory.resolve())
    if rrls:
        env["GS_RRLS_ROLE"] = "rrls"
        if choice.reason != "default":
            # The testing model would bypass the RRLS radio path.
            env["GS_SIM_MODE"] = "0"
        if choice.simulation:
            root = sim_root.resolve()
            env["GS_RRLS_SIMULATED_PRIMARY"] = "1"
            env["GS_RRLS_SIM_ROOT"] = str(root)
            for key, relative in RRLS_SIM_ISOLATED.items():
                env[key] = str(root / relative)
            # Would be saved into the real RRLS radio settings.
            env.pop("GS_RRLS_RADIO_PORT", None)
        else:
            env.pop("GS_RRLS_SIMULATED_PRIMARY", None)
            env.pop("GS_RRLS_SIM_ROOT", None)
    elif choice.mode == "testing" and choice.reason != "default":
        env["GS_SIM_MODE"] = "1" if choice.simulation else "0"
    return env


def prepare_sim_root(root: Path) -> None:
    """Every simulated-primary start is a fresh simulated installation."""
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True, mode=0o700)


def cargo_command(mode: str, debug: bool, pi: bool) -> list[str]:
    command = ["cargo", "build", "-p", BINARY]
    if not debug:
        command.insert(2, "--release")
    features = (["raspberry_pi"] if pi else []) + MODE_FEATURES[mode]
    if features:
        command.extend(["--features", ",".join(features)])
    return command


def build_modes(repo_root: Path, modes: list[str], debug: bool = False, pi: bool = False, run: Callable = subprocess.run) -> Path:
    """Build each mode with its features and keep a copy of each binary.
    Cargo keeps per-feature artifacts, so an unchanged mode rebuilds quickly."""
    root = binary_root(repo_root, debug)
    built = repo_root / "target" / ("debug" if debug else "release") / BINARY
    for mode in modes:
        command = cargo_command(mode, debug, pi)
        print(f"Building station mode {mode}: {' '.join(command)}")
        run(command, cwd=repo_root, check=True)
        destination = root / mode / BINARY
        destination.parent.mkdir(parents=True, exist_ok=True)
        staging = destination.with_name(BINARY + ".new")
        shutil.copy2(built, staging)
        os.replace(staging, destination)
        (destination.parent / "build.json").write_text(json.dumps({"mode": mode, "features": MODE_FEATURES[mode] + (["raspberry_pi"] if pi else []), "profile": "debug" if debug else "release"}) + "\n", encoding="utf-8")
    return root


def plan(repo_root: Path, root: Path, environ: Mapping[str, str], default_mode: str, rrls: bool, default_simulation: bool | None = None) -> tuple[Choice, Path, dict[str, str]]:
    installed = installed_modes(root)
    if default_mode not in installed:
        raise FileNotFoundError(f"default station mode {default_mode} is not built under {root}")
    directory = state_dir(repo_root, environ)
    choice = choose(read_selection(directory), installed, default_mode, rrls, default_simulation)
    sim_root = directory / "rrls-simulation"
    env = child_env(environ, choice, rrls, installed, directory, sim_root)
    if rrls and choice.simulation:
        prepare_sim_root(sim_root)
    return choice, root / choice.mode / BINARY, env


def describe(choice: Choice, rrls: bool) -> str:
    sim = ""
    if choice.simulation:
        sim = " with a SIMULATED paired primary (no radio output)" if rrls else " in internal flight simulation"
    return f"{'RRLS' if rrls else 'Ground station'} mode {choice.mode}{sim} ({choice.reason})"


def supervise(repo_root: Path, root: Path, default_mode: str, rrls: bool, cwd: Path, environ: Mapping[str, str] | None = None, popen: Callable = subprocess.Popen, default_simulation: bool | None = None) -> int:
    """Run the selected binary; restart into the new selection on
    RESTART_EXIT_CODE, otherwise return the backend's own exit status."""
    environ = dict(os.environ if environ is None else environ)
    while True:
        choice, binary, env = plan(repo_root, root, environ, default_mode, rrls, default_simulation)
        print(f"Starting {describe(choice, rrls)}: {binary}")
        process = popen([str(binary)], cwd=cwd, env=env)
        try:
            code = process.wait()
        except KeyboardInterrupt:
            process.send_signal(signal.SIGINT)
            try:
                code = process.wait(timeout=12)
            except subprocess.TimeoutExpired:
                process.terminate()
                code = process.wait()
            return code
        if code != RESTART_EXIT_CODE:
            return code
        print("Backend requested a station mode restart; starting the selected build.")


def exec_selected(repo_root: Path, root: Path, default_mode: str, rrls: bool) -> None:
    """For systemd: replace this process with the selected binary. The unit
    restarts it after RESTART_EXIT_CODE and this runs again."""
    choice, binary, env = plan(repo_root, root, os.environ, default_mode, rrls)
    print(f"Starting {describe(choice, rrls)}: {binary}", flush=True)
    os.execve(binary, [str(binary)], env)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    build = sub.add_parser("build", help="build one backend binary per mode")
    build.add_argument("--modes", default="all", help="comma list of normal,hitl,test_fire,testing, or all")
    build.add_argument("--debug", action="store_true")
    build.add_argument("--pi", action="store_true", help="add the raspberry_pi feature (primary on a Pi)")
    for name in ("exec", "list"):
        command = sub.add_parser(name)
        command.add_argument("--rrls", action="store_true")
        command.add_argument("--debug", action="store_true")
        command.add_argument("--default-mode", default="normal")
    args = parser.parse_args(argv)
    repo_root = Path(__file__).resolve().parent
    if args.command == "build":
        build_modes(repo_root, parse_modes(args.modes), args.debug, args.pi)
        return 0
    root = binary_root(repo_root, args.debug)
    default_mode = parse_modes(args.default_mode)[0]
    if args.command == "list":
        directory = state_dir(repo_root, os.environ)
        print(json.dumps({"installed": installed_modes(root), "selection": read_selection(directory)}, indent=2))
        return 0
    exec_selected(repo_root, root, default_mode, args.rrls)
    return 0


if __name__ == "__main__":
    sys.exit(main())
