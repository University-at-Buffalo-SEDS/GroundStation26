"""Explicit FlightSim launch environment; never reuse a hardware runtime directory."""
from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
import stat

PATHS = {
    "GS_USERS_FILE": "users/users.json",
    "GS_BOARD_ABORT_HISTORY_DIR": "proof/board_abort_history",
    "GS_LAUNCH_CLOCK_STATE": "proof/launch_clock.json",
    "GS_NETWORK_VARIABLE_CACHE": "config/network_variables.json",
    "GS_FILL_TARGETS_CONFIG": "config/fill_targets.json",
    "GS_FLIGHT_SETUP_CONFIG": "config/flight_setup.json",
    "GS_LOADCELL_CALIBRATION_PATH": "config/loadcell_calibration.json",
    "GS_CALIBRATION_CONFIG_PATH": "config/calibration_config.json",
    "GS_AUTO_ZERO_CONFIG": "config/auto_zero.json",
    "GS_GSE_CONFIG": "config/gse_sequence.json",
    "GS_GSE_QUALIFICATION": "proof/gse_qualification.json",
    "GS_CHECKLISTS_FILE": "config/checklists.json",
    "GS_ACTUATOR_PLUMBING_CONFIG": "config/plumbing.json",
    "GS_STATION_POSITIONS_PATH": "config/station_positions.json",
    "GS_LAYOUT_PATH": "config/layout.json",
    "GS_ZERO_CAPTURE_DIR": "calibration/zero_captures",
    "GS_LOG_DIR": "logs",
    "GS_TLS_DIR": "tls",
    "GS_VIDEO_RUNTIME_DIR": "video/runtime",
    "GS_VIDEO_RECORDINGS_DIR": "video/recordings",
    "GS_STAGE_MODELS_DIR": "stage_models",
}
BUILD_KEYS = ("GS_FRONTEND_SOURCE_DIR", "GS_BROWSER_ASSETS_CACHE")


def prepare_environment(base: Path, inherited: dict[str, str], public_assets: Path, users_snapshot: Path | None = None) -> tuple[Path, dict[str, str]]:
    """Create one fresh session; keep recordings/proofs separate even on restart."""
    users_bytes = read_users_snapshot(users_snapshot) if users_snapshot is not None else None
    public_assets = public_assets.resolve(strict=True)
    if not (public_assets / "index.html").is_file():
        raise ValueError("FlightSim requires the built frontend before creating a session")
    forbidden = {key for key in inherited if key.startswith("GS_RRLS_") and not (key == "GS_RRLS_ROLE" and inherited[key] == "off")}
    forbidden.update(key for key in ("GS_STATION_PAIRING_FILE", "GS_COMMS_LINK_CONFIG", "GS_RADIO_LINK_CONFIG", "GS_GPSD_ADDR", "GS_SIMULATED_SERIAL_PTY", "GS_SIM_CLOCK_FILE") if key in inherited)
    if forbidden:
        raise ValueError("FlightSim refuses hardware/proxy configuration: " + ", ".join(sorted(forbidden)))
    base = base.expanduser().resolve()
    base.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="session-", dir=base)).resolve()
    root.chmod(0o700)
    (root / "frontend/dist").mkdir(parents=True)
    (root / "frontend/dist/public").symlink_to(public_assets, target_is_directory=True)
    env = {key: value for key, value in inherited.items() if not key.startswith("GS_")}
    for key in BUILD_KEYS:
        if key in inherited:
            env[key] = inherited[key]
    for key, relative in PATHS.items():
        destination = root / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        env[key] = str(destination)
    for directory in ("data", "backend/data", "logs", "tls", "video/runtime", "video/recordings", "stage_models", "calibration/zero_captures"):
        (root / directory).mkdir(parents=True, exist_ok=True)
    env.update({
        "GS_EXECUTION_MODE": "flight_sim", "GS_FLIGHT_SIM_ROOT": str(root),
        "GS_FLIGHT_SIM_ASSETS": str(public_assets),
        "GS_SIM_MODE": "1", "GS_RRLS_ROLE": "off",
        "GS_VIDEO_MANAGED": "0", "GS_VIDEO_DISCOVERY": "0",
        "GS_BIND_ADDRESS": "127.0.0.1:3000", "GS_HTTPS_BIND_ADDRESS": "off",
    })
    if users_bytes is not None:
        destination = root / PATHS["GS_USERS_FILE"]
        fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as output:
            output.write(users_bytes)
    (root / "flight-sim-session.json").write_text(json.dumps({
        "version": 1, "execution_mode": "flight_sim", "output_domain": "simulated",
        "session": root.name, "physical_outputs": False,
    }) + "\n", encoding="utf-8")
    return root, env


def read_users_snapshot(path: Path) -> bytes:
    """Explicit account configuration only, never sessions, tokens or proofs."""
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(descriptor, "rb") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 1024 * 1024:
            raise ValueError("FlightSim account snapshot must be a regular file <=1MiB")
        raw = source.read(1024 * 1024 + 1)
    if len(raw) > 1024 * 1024:
        raise ValueError("FlightSim account snapshot exceeds1MiB")
    try:
        value = json.loads(raw)
        if not isinstance(value, dict) or value.get("version", 1) != 1 or not isinstance(value.get("users", []), list):
            raise ValueError()
        if any(key not in {"version", "session_ttl_seconds", "anonymous", "anonymous_command_access", "anonymous_calibration_access", "users"} for key in value):
            raise ValueError()
        for user in value.get("users", []):
            if not isinstance(user, dict) or not isinstance(user.get("username"), str) or not isinstance(user.get("password"), dict):
                raise ValueError()
            if any(key not in {"roles", "username", "password", "permissions", "command_access", "calibration_access", "disabled"} for key in user):
                raise ValueError()
            password = user["password"]
            if any(key not in {"algorithm", "iterations", "salt_b64", "hash_b64"} for key in password):
                raise ValueError()
            if password.get("algorithm", "pbkdf2_sha256") != "pbkdf2_sha256" or not isinstance(password.get("iterations"), int) or password["iterations"] <= 0:
                raise ValueError()
            if not all(isinstance(password.get(key), str) and password[key] for key in ("salt_b64", "hash_b64")):
                raise ValueError()
    except (ValueError, TypeError, KeyError):
        raise ValueError("Invalid FlightSim account configuration") from None
    return raw
