#!/usr/bin/env python3
"""Station mode supervisor for the single backend binary.

One backend binary holds every station mode (normal, HITL, test fire,
testing). The backend switches mode inside its own process while the station
is idle. Only a Simulation change needs a restart: the backend's
/api/station-modes Confirm saves ``<state dir>/selection.json`` and exits with
RESTART_EXIT_CODE, and this supervisor (or systemd, through ``exec``) starts
the binary again with the selected mode and Simulation flag.

  python3 station_modes.py build                         # build the binary
  python3 station_modes.py exec [--rrls] [--default-mode hitl]   # systemd ExecStart

A selection with ``firmware_sim`` runs the backend against the real firmware
of every board, built for this machine (tools/firmware_sim.py,
docs/firmware-sim.md): the supervisor starts the native bay first, points the
backend's links at it, isolates its state and recordings, and stops the bay
when the backend exits.

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
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Mapping

RESTART_EXIT_CODE = 75
# Mode ids (backend station_mode::Mode, snake_case), passed to the backend as
# GS_STATION_MODE. Every mode is in the one binary.
MODES = ("normal", "hitl", "test_fire", "testing", "integrated_ground_test")
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


# Modes whose firmware simulation is offered (backend station_mode_switch FIRMWARE_SIM_MODES).
FIRMWARE_SIM_MODES = MODES
# Backend state and configuration a firmware simulation keeps apart from the hardware installation (relative to
# <state dir>/firmware-sim/state, fresh at every start).
FIRMWARE_SIM_ISOLATED = {
    "GS_LAUNCH_CLOCK_STATE": "proof/launch_clock.json",
    "GS_BOARD_ABORT_HISTORY_DIR": "proof/board_abort_history",
    "GS_GSE_QUALIFICATION": "proof/gse_qualification.json",
    "GS_NETWORK_VARIABLE_CACHE": "config/network_variables.json",
    "GS_FILL_TARGETS_CONFIG": "config/fill_targets.json",
    "GS_FLIGHT_SETUP_CONFIG": "config/flight_setup.json",
    "GS_LOADCELL_CALIBRATION_PATH": "config/loadcell_calibration.json",
    "GS_CALIBRATION_CONFIG_PATH": "config/calibration_config.json",
    "GS_AUTO_ZERO_CONFIG": "config/auto_zero.json",
    "GS_GSE_CONFIG": "config/gse_sequence.json",
    "GS_CHECKLISTS_FILE": "config/checklists.json",
    "GS_ACTUATOR_PLUMBING_CONFIG": "config/plumbing.json",
    "GS_BOARD_CONFIG_SYNC_PATH": "config/board_config_sync.json",
    "GS_MISSION_SETTINGS_PATH": "config/mission_settings.json",
    "GS_ZERO_CAPTURE_DIR": "calibration/zero_captures",
    "GS_LOG_DIR": "logs",
    "GS_COMMS_LINK_CONFIG": "config/comms.json",
}
# Seeds matching the bay's plant (FirmwareSimulator native_plant.rs): the DAQ load cell at 100 kg/V, a nitrogen
# supply of 1000 psi and nitrous at 745 psi (so targets stay below them and below the VB relief at 850 psi).
FIRMWARE_SIM_SEEDS = {
    # KG1000 (ch1) 545.8 kg/V, zero 0.2126 V; KG50 12 915 kg/V, zero 0.2112 V (docs/loadcell-investigation-2026-11.md),
    # on the calibration's legacy-raw scale (volts x 921/16000, DAQ MCP3564R_LEGACY_RAW_TO_VOLTS): KG1000 is then the
    # recorded 9481.62 raw - 116.03. Operators can recalibrate against the sim's calibration masses.
    "GS_LOADCELL_CALIBRATION_PATH": {"version": 1, "ch1": {"m": 9481.62, "b": -116.03}, "iadc": {"m": 1, "b": 0}, "kg50_input": "amp2",
                                     "extra_channels": {"kg50": {"linear": {"m": 224366.0, "b": -2727.65}, "zero_raw": None, "points": [], "fit": None}}},
    "GS_GSE_CONFIG": {"nitrogen_target_psi": 500, "pressure_step_psi": 50, "pressure_ceiling_psi": 800, "maximum_zero_offset_psi": 20,
                      "dry_self_test_confirmed": False, "grouped_panel": True, "turbocharge_max_added_mass_kg": 0.5,
                      "turbocharge_pressure_deadband_psi": 12},
    "GS_FILL_TARGETS_CONFIG": {"version": 1, "fill_source": "kg50", "nitrogen": {"target_mass_kg": 0.5, "target_pressure_psi": 500},
                               "nitrous": {"target_mass_kg": 8, "target_pressure_psi": 700}},
    "GS_CHECKLISTS_FILE": {"revision": 0, "enforce_before_launch": False, "checklists": []},
    # Links are always the bay's (simulator overrides below); nothing here names a physical device.
    "GS_COMMS_LINK_CONFIG": {"version": 1, "av_bay": {"interface": "serial", "port": "/dev/null", "baud_rate": 115200, "protocol": "raw_uart"},
                             "fill_box": {"interface": "pico_fi_tcp", "mode": "server", "address": "127.0.0.1:0", "discovery": True}},
}


def firmware_sim_root(directory: Path) -> Path:
    return directory / "firmware-sim"


def firmware_sim_env(base: Mapping[str, str], root: Path, endpoints: dict | None) -> dict[str, str]:
    """The backend's environment against a running bay (``endpoints``: its endpoints.json, or None when the bay
    did not start: the links then point nowhere and the station shows the bay as not running)."""
    env = dict(base)
    state = root / "state"
    for key, relative in FIRMWARE_SIM_ISOLATED.items():
        env[key] = str(state / relative)
    env["GS_FIRMWARE_SIM"] = "1"
    env["GS_FIRMWARE_SIM_DIR"] = str(root / "run")
    env["GS_RECORDINGS_DIR"] = str(root / "recordings")
    env["GS_SIMULATED_SERIAL_PTY"] = "1"
    env["GS_SIM_MODE"] = "0"
    env["GS_VIDEO_MANAGED"] = "0"
    env["GS_VIDEO_DISCOVERY"] = "0"
    for key in ("GS_AV_BAY_SERIAL_PORT", "GS_FILL_PICO_DISCOVERY_PORT", "GS_FILL_PICO_TCP_ADDRESS", "GS_FILL_SERIAL_PORT",
                "GS_RRLS_ROLE", "GS_EXECUTION_MODE", "GS_LAYOUT_PATH"):
        env.pop(key, None)
    links = ((endpoints or {}).get("hosts") or {}).get("groundstation") or {}
    env["GS_AV_BAY_SERIAL_PORT"] = links.get("GS_AV_BAY_SERIAL_PORT", "/dev/null")
    # a closed loopback port: discovery finds no Pico-Fi when the bay is not running
    env["GS_FILL_PICO_DISCOVERY_PORT"] = links.get("GS_FILL_PICO_DISCOVERY_PORT", "9")
    return env


def prepare_firmware_sim_state(root: Path) -> None:
    """Fresh isolated state and configuration at every start; recordings persist (they are all simulated)."""
    state = root / "state"
    if state.exists():
        shutil.rmtree(state)
    for key, relative in FIRMWARE_SIM_ISOLATED.items():
        path = state / relative
        # Only parents: some owners (the board abort history) treat an existing empty directory as theirs.
        path.parent.mkdir(parents=True, exist_ok=True)
    for key, seed in FIRMWARE_SIM_SEEDS.items():
        (state / FIRMWARE_SIM_ISOLATED[key]).write_text(json.dumps(seed, indent=2) + "\n", encoding="utf-8")
    (root / "recordings").mkdir(parents=True, exist_ok=True)


class FirmwareSimBay:
    """The native bay (every board's real firmware plus the plant), started before the backend and stopped after
    it. ``tools/firmware_sim.py run`` resolves the build cache and FirmwareSimulator's launcher."""

    def __init__(self, repo_root: Path, root: Path, popen: Callable = subprocess.Popen, timeout_s: float = 90.0, mode: str = "normal"):
        self.repo_root, self.root, self.popen, self.timeout_s, self.mode = repo_root, root, popen, timeout_s, mode
        self.process = None

    def start(self) -> dict | None:
        run = self.root / "run"
        if run.exists():
            shutil.rmtree(run)
        run.mkdir(parents=True)
        command = [sys.executable, str(self.repo_root / "tools" / "firmware_sim.py"), "run", "--out", str(run), "--mode", self.mode]
        print(f"Starting the firmware simulation bay: {' '.join(command)}", flush=True)
        log = (run / "supervisor-bay.log").open("ab")
        self.process = self.popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        deadline = time.monotonic() + self.timeout_s
        endpoints = run / "endpoints.json"
        while time.monotonic() < deadline:
            if endpoints.exists():
                try:
                    return json.loads(endpoints.read_text(encoding="utf-8"))
                except ValueError:
                    pass
            if self.process.poll() is not None:
                print(f"Warning: the firmware simulation bay exited ({self.process.returncode}); see {run / 'supervisor-bay.log'}",
                      file=sys.stderr)
                return None
            time.sleep(0.2)
        print("Warning: the firmware simulation bay did not publish its endpoints in time", file=sys.stderr)
        return None

    def stop(self) -> None:
        if self.process is None or self.process.poll() is not None:
            return
        self.process.send_signal(signal.SIGTERM)
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()


def pi_build(root: Path) -> bool:
    try:
        return "raspberry_pi" in json.loads((root / "build.json").read_text(encoding="utf-8")).get("features", [])
    except (OSError, ValueError):
        return False


def simulation_supported(mode: str, rrls: bool) -> bool:
    """The primary's simulation is testing mode's flight model; an RRLS can
    simulate its paired primary in any mode."""
    return rrls or mode == "testing"


def parse_modes(text: str) -> list[str]:
    if text.strip() == "all":
        return list(MODES)
    modes = []
    for item in text.split(","):
        item = item.strip().replace("-", "_")
        if item in ("hitl_mode", "test_fire_mode", "integrated_ground_test_mode"):
            item = item.removesuffix("_mode")
        if item in ("igt", "igt_mode"):
            item = "integrated_ground_test"
        if item not in MODES:
            raise ValueError(f"unknown station mode {item!r}; choose from {', '.join(MODES)} or all")
        if item not in modes:
            modes.append(item)
    return modes


def binary_root(repo_root: Path, debug: bool = False) -> Path:
    return repo_root / "target" / "station-modes" / ("debug" if debug else "release")


def binary_path(repo_root: Path, debug: bool = False) -> Path:
    return binary_root(repo_root, debug) / BINARY


def installed_modes(root: Path) -> list[str]:
    """Every mode when the binary is installed, else none."""
    binary = root / BINARY
    return list(MODES) if binary.is_file() and os.access(binary, os.X_OK) else []


def state_dir(repo_root: Path, environ: Mapping[str, str]) -> Path:
    configured = environ.get("GS_STATION_MODE_DIR")
    return Path(configured) if configured else repo_root / "data" / "station-mode"


@dataclass(frozen=True)
class Choice:
    mode: str
    simulation: bool
    reason: str
    firmware_sim: bool = False


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
        elif mode not in MODES or not isinstance(simulation, bool):
            print("Warning: ignoring malformed station mode selection", file=sys.stderr)
        elif mode not in installed:
            print(f"Warning: selected mode {mode} is not installed; starting the default {default_mode}", file=sys.stderr)
        elif simulation and not simulation_supported(mode, rrls):
            print(f"Warning: {mode} has no simulation on this station; starting it without", file=sys.stderr)
            return Choice(mode, False, "selection (simulation unsupported)")
        elif selection.get("firmware_sim") is True:
            if rrls or mode not in FIRMWARE_SIM_MODES:
                print(f"Warning: firmware simulation is not offered for {role} {mode}; starting it without", file=sys.stderr)
                return Choice(mode, False, "selection (firmware simulation unsupported)")
            return Choice(mode, False, "selection", firmware_sim=True)
        else:
            return Choice(mode, simulation, "selection")
    return Choice(default_mode, bool(default_simulation), "default")


def child_env(base: Mapping[str, str], choice: Choice, rrls: bool, installed: list[str], directory: Path, sim_root: Path) -> dict[str, str]:
    env = dict(base)
    env["GS_STATION_MODE_SUPERVISOR"] = "1"
    env["GS_STATION_MODE"] = choice.mode
    env.pop("GS_STATION_MODE_AVAILABLE", None)
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


def build_features(pi: bool = False, firmware_sim: bool = False) -> list[str]:
    return (["raspberry_pi"] if pi else []) + (["firmware_sim"] if firmware_sim else [])


def cargo_command(debug: bool, pi: bool, firmware_sim: bool = False) -> list[str]:
    command = ["cargo", "build", "-p", BINARY]
    if not debug:
        command.insert(2, "--release")
    features = build_features(pi, firmware_sim)
    if features:
        command.extend(["--features", ",".join(features)])
    return command


def build_modes(repo_root: Path, modes: list[str] | None = None, debug: bool = False, pi: bool = False, run: Callable = subprocess.run, firmware_sim: bool = False) -> Path:
    """Build the one backend binary (it holds every mode) and install a copy.
    ``modes`` is accepted for older callers and ignored."""
    root = binary_root(repo_root, debug)
    built = repo_root / "target" / ("debug" if debug else "release") / BINARY
    command = cargo_command(debug, pi, firmware_sim)
    print(f"Building the station backend (all modes): {' '.join(command)}")
    run(command, cwd=repo_root, check=True)
    destination = root / BINARY
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = destination.with_name(BINARY + ".new")
    shutil.copy2(built, staging)
    os.replace(staging, destination)
    (root / "build.json").write_text(json.dumps({"modes": list(MODES), "features": build_features(pi, firmware_sim), "profile": "debug" if debug else "release"}) + "\n", encoding="utf-8")
    return root


def plan(repo_root: Path, root: Path, environ: Mapping[str, str], default_mode: str, rrls: bool, default_simulation: bool | None = None) -> tuple[Choice, Path, dict[str, str]]:
    installed = installed_modes(root)
    if default_mode not in installed:
        raise FileNotFoundError(f"the station backend is not built under {root}")
    directory = state_dir(repo_root, environ)
    choice = choose(read_selection(directory), installed, default_mode, rrls, default_simulation)
    sim_root = directory / "rrls-simulation"
    env = child_env(environ, choice, rrls, installed, directory, sim_root)
    if rrls and choice.simulation:
        prepare_sim_root(sim_root)
    return choice, root / BINARY, env


def describe(choice: Choice, rrls: bool) -> str:
    sim = ""
    if choice.firmware_sim:
        sim = " against SIMULATED FIRMWARE (native board builds, no physical links)"
    elif choice.simulation:
        sim = " with a SIMULATED paired primary (no radio output)" if rrls else " in internal flight simulation"
    return f"{'RRLS' if rrls else 'Ground station'} mode {choice.mode}{sim} ({choice.reason})"


def supervise(repo_root: Path, root: Path, default_mode: str, rrls: bool, cwd: Path, environ: Mapping[str, str] | None = None, popen: Callable = subprocess.Popen, default_simulation: bool | None = None) -> int:
    """Run the selected binary; restart into the new selection on
    RESTART_EXIT_CODE, otherwise return the backend's own exit status."""
    environ = dict(os.environ if environ is None else environ)
    while True:
        choice, binary, env = plan(repo_root, root, environ, default_mode, rrls, default_simulation)
        bay = None
        if choice.firmware_sim:
            if pi_build(root):
                print("Warning: firmware simulation is not run by a Raspberry Pi build; starting the mode without it", file=sys.stderr)
                choice = Choice(choice.mode, False, "selection (firmware simulation unsupported on the Pi)")
            else:
                sim_root = firmware_sim_root(state_dir(repo_root, environ))
                prepare_firmware_sim_state(sim_root)
                bay = FirmwareSimBay(repo_root, sim_root, popen, mode=choice.mode)
                env = firmware_sim_env(env, sim_root, bay.start())
        print(f"Starting {describe(choice, rrls)}: {binary}")
        code, interrupted = run_backend(popen, binary, cwd, env, bay)
        if interrupted or code != RESTART_EXIT_CODE:
            return code
        print("Backend requested a restart for a Simulation change; starting the selected mode.")


def exec_selected(repo_root: Path, root: Path, default_mode: str, rrls: bool) -> None:
    """For systemd: replace this process with the selected binary. The unit
    restarts it after RESTART_EXIT_CODE and this runs again."""
    choice, binary, env = plan(repo_root, root, os.environ, default_mode, rrls)
    if choice.firmware_sim:
        # The bay is a child that must outlive nothing: supervise this one start, then exit with the backend's
        # status (the unit restarts after RESTART_EXIT_CODE and this runs again).
        sys.exit(supervise_once(repo_root, root, default_mode, rrls))
    print(f"Starting {describe(choice, rrls)}: {binary}", flush=True)
    os.execve(binary, [str(binary)], env)


def supervise_once(repo_root: Path, root: Path, default_mode: str, rrls: bool, popen: Callable = subprocess.Popen) -> int:
    environ = dict(os.environ)
    choice, binary, env = plan(repo_root, root, environ, default_mode, rrls)
    sim_root = firmware_sim_root(state_dir(repo_root, environ))
    prepare_firmware_sim_state(sim_root)
    bay = FirmwareSimBay(repo_root, sim_root, popen, mode=choice.mode)
    env = firmware_sim_env(env, sim_root, bay.start())
    print(f"Starting {describe(choice, rrls)}: {binary}", flush=True)
    return run_backend(popen, binary, Path.cwd(), env, bay)[0]


def run_backend(popen: Callable, binary: Path, cwd: Path, env: Mapping[str, str], bay: "FirmwareSimBay | None") -> tuple[int, bool]:
    """Run the backend once; (exit status, interrupted). With a bay, SIGTERM to this supervisor is passed to the
    backend (its clean shutdown) and the bay always stops after the backend, whatever ends it."""
    process = popen([str(binary)], cwd=cwd, env=env)
    previous = None
    if bay is not None:
        def on_term(_signum, _frame):
            try:
                process.send_signal(signal.SIGTERM)
            except (OSError, AttributeError):
                pass
        try:
            previous = signal.signal(signal.SIGTERM, on_term)
        except ValueError:  # not the main thread
            previous = None
    try:
        return process.wait(), False
    except KeyboardInterrupt:
        process.send_signal(signal.SIGINT)
        try:
            code = process.wait(timeout=12)
        except subprocess.TimeoutExpired:
            process.terminate()
            code = process.wait()
        return code, True
    finally:
        if bay is not None:
            bay.stop()
        if previous is not None:
            signal.signal(signal.SIGTERM, previous)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    build = sub.add_parser("build", help="build the backend binary (it holds every mode)")
    build.add_argument("--modes", default="all", help="accepted for older callers and ignored: one binary holds every mode")
    build.add_argument("--debug", action="store_true")
    build.add_argument("--pi", action="store_true", help="add the raspberry_pi feature (primary on a Pi)")
    build.add_argument("--firmware-sim", action="store_true", help="add the firmware_sim feature (MCU simulator clocks)")
    for name in ("exec", "list"):
        command = sub.add_parser(name)
        command.add_argument("--rrls", action="store_true")
        command.add_argument("--debug", action="store_true")
        command.add_argument("--default-mode", default="normal")
    args = parser.parse_args(argv)
    repo_root = Path(__file__).resolve().parent
    if args.command == "build":
        parse_modes(args.modes)  # still rejects unknown names
        build_modes(repo_root, None, args.debug, args.pi, firmware_sim=args.firmware_sim)
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
