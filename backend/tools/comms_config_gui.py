#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import platform
import sys
from dataclasses import dataclass
from pathlib import Path

try:
    import tkinter as tk
    from tkinter import messagebox, ttk

    TK_AVAILABLE = True
except ImportError:
    tk = None  # type: ignore[assignment]
    messagebox = None  # type: ignore[assignment]
    ttk = None  # type: ignore[assignment]
    TK_AVAILABLE = False

DEFAULT_BAUD_RATE = 57_600
DEFAULT_SPI_SPEED_HZ = 1_000_000
DEFAULT_SPI_MODE = 0
DEFAULT_SPI_BITS_PER_WORD = 8

INTERFACE_OPTIONS = [
    "usb_serial",
    "raspberry_pi_gpio_uart",
    "custom_serial",
    "spi",
    "can",
]

DEFAULT_CONFIG = {
    "version": 1,
    "av_bay": {
        "interface": "usb_serial",
        "port": "/dev/ttyUSB1",
        "baud_rate": DEFAULT_BAUD_RATE,
        "spi_speed_hz": DEFAULT_SPI_SPEED_HZ,
        "spi_mode": DEFAULT_SPI_MODE,
        "spi_bits_per_word": DEFAULT_SPI_BITS_PER_WORD,
        "can_tx_id": 0x121,
        "can_rx_id": 0x221,
    },
    "fill_box": {
        "interface": "usb_serial",
        "port": "/dev/ttyUSB2",
        "baud_rate": DEFAULT_BAUD_RATE,
        "spi_speed_hz": DEFAULT_SPI_SPEED_HZ,
        "spi_mode": DEFAULT_SPI_MODE,
        "spi_bits_per_word": DEFAULT_SPI_BITS_PER_WORD,
        "can_tx_id": 0x122,
        "can_rx_id": 0x222,
    },
}

GPIO_PORT_CANDIDATES = ["/dev/serial0", "/dev/ttyAMA0", "/dev/ttyS0", "/dev/serial1"]
GENERIC_SERIAL_CANDIDATES = [
    "/dev/ttyUSB0",
    "/dev/ttyUSB1",
    "/dev/ttyUSB2",
    "/dev/ttyACM0",
    "/dev/ttyACM1",
    "/dev/serial0",
    "/dev/ttyAMA0",
    "/dev/ttyS0",
]
GENERIC_SPI_CANDIDATES = ["/dev/spidev0.0", "/dev/spidev0.1", "/dev/spidev1.0"]
GENERIC_CAN_CANDIDATES = ["can0", "can1", "vcan0"]
USB_PORT_PREFIXES = ("/dev/ttyUSB", "/dev/ttyACM", "/dev/cu.usb", "/dev/cu.serial")


@dataclass
class EnvironmentInfo:
    os_label: str
    pi_model: str | None
    pi_detected: bool
    serial_ports: list[str]
    spi_devices: list[str]
    can_interfaces: list[str]


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def default_config_path() -> Path:
    return repo_root() / "backend" / "comms" / "coms.json"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Configure GroundStation radio links.")
    parser.add_argument("--config", default=str(default_config_path()), help="Path to coms.json.")
    parser.add_argument("--gui", action="store_true", help="Force GUI mode.")
    parser.add_argument("--tui", action="store_true", help="Force interactive terminal mode.")
    parser.add_argument("--cli", action="store_true", help="Apply settings from CLI flags and save without prompts.")
    for prefix in ("av-bay", "fill-box"):
        parser.add_argument(f"--{prefix}-interface", choices=INTERFACE_OPTIONS)
        parser.add_argument(f"--{prefix}-port")
        parser.add_argument(f"--{prefix}-baud")
        parser.add_argument(f"--{prefix}-spi-speed-hz")
        parser.add_argument(f"--{prefix}-spi-mode")
        parser.add_argument(f"--{prefix}-spi-bits")
        parser.add_argument(f"--{prefix}-can-tx-id")
        parser.add_argument(f"--{prefix}-can-rx-id")
    return parser.parse_args()


def detect_os_label() -> str:
    release_path = Path("/etc/os-release")
    if release_path.exists():
        data: dict[str, str] = {}
        for line in release_path.read_text(encoding="utf-8", errors="ignore").splitlines():
            if "=" not in line:
                continue
            key, value = line.split("=", 1)
            data[key] = value.strip().strip('"')
        if data.get("PRETTY_NAME"):
            return data["PRETTY_NAME"]
    return platform.platform()


def detect_pi_model() -> str | None:
    for candidate in (Path("/proc/device-tree/model"), Path("/sys/firmware/devicetree/base/model")):
        if candidate.exists():
            text = candidate.read_text(encoding="utf-8", errors="ignore").strip("\x00 \n\t")
            if text:
                return text
    return None


def is_probably_raspberry_pi() -> bool:
    model = detect_pi_model()
    return bool(model and "raspberry pi" in model.lower())


def existing_paths(paths: list[str]) -> list[str]:
    return [path for path in paths if Path(path).exists()]


def detect_serial_ports() -> list[str]:
    candidates: list[str] = []
    by_id = Path("/dev/serial/by-id")
    if by_id.exists():
        for child in sorted(by_id.iterdir()):
            try:
                resolved = str(child.resolve())
            except OSError:
                resolved = str(child)
            for value in (str(child), resolved):
                if value not in candidates:
                    candidates.append(value)

    for pattern in (
            "/dev/ttyUSB*",
            "/dev/ttyACM*",
            "/dev/ttyAMA*",
            "/dev/ttyS*",
            "/dev/serial*",
            "/dev/cu.usb*",
            "/dev/cu.serial*",
    ):
        for path in sorted(Path("/").glob(pattern.lstrip("/"))):
            value = str(path)
            if value not in candidates:
                candidates.append(value)

    for value in GENERIC_SERIAL_CANDIDATES:
        if value not in candidates:
            candidates.append(value)
    return candidates


def detect_spi_devices() -> list[str]:
    devices = [str(path) for path in sorted(Path("/dev").glob("spidev*"))]
    for value in GENERIC_SPI_CANDIDATES:
        if value not in devices:
            devices.append(value)
    return devices


def detect_can_interfaces() -> list[str]:
    found: list[str] = []
    net_root = Path("/sys/class/net")
    if net_root.exists():
        for child in sorted(net_root.iterdir()):
            type_path = child / "type"
            try:
                if type_path.read_text(encoding="utf-8", errors="ignore").strip() == "280":
                    found.append(child.name)
            except OSError:
                continue
    for value in GENERIC_CAN_CANDIDATES:
        if value not in found:
            found.append(value)
    return found


def collect_environment() -> EnvironmentInfo:
    return EnvironmentInfo(
        os_label=detect_os_label(),
        pi_model=detect_pi_model(),
        pi_detected=is_probably_raspberry_pi(),
        serial_ports=detect_serial_ports(),
        spi_devices=detect_spi_devices(),
        can_interfaces=detect_can_interfaces(),
    )


def load_config(path: Path) -> dict:
    if not path.exists():
        return json.loads(json.dumps(DEFAULT_CONFIG))
    with path.open("r", encoding="utf-8") as handle:
        return json.load(handle)


def save_config(path: Path, cfg: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        json.dump(cfg, handle, indent=2)
        handle.write("\n")


def normalize_config(cfg: dict) -> dict:
    merged = json.loads(json.dumps(DEFAULT_CONFIG))
    merged.update({k: v for k, v in cfg.items() if k not in {"av_bay", "fill_box"}})
    for name in ("av_bay", "fill_box"):
        merged[name].update(cfg.get(name, {}))
        merged[name]["interface"] = str(merged[name]["interface"])
        merged[name]["port"] = str(merged[name]["port"])
        merged[name]["baud_rate"] = int(merged[name].get("baud_rate", DEFAULT_BAUD_RATE))
        merged[name]["spi_speed_hz"] = int(merged[name].get("spi_speed_hz", DEFAULT_SPI_SPEED_HZ))
        merged[name]["spi_mode"] = int(merged[name].get("spi_mode", DEFAULT_SPI_MODE))
        merged[name]["spi_bits_per_word"] = int(
            merged[name].get("spi_bits_per_word", DEFAULT_SPI_BITS_PER_WORD)
        )
        merged[name]["can_tx_id"] = int(merged[name].get("can_tx_id", DEFAULT_CONFIG[name]["can_tx_id"]))
        merged[name]["can_rx_id"] = int(merged[name].get("can_rx_id", DEFAULT_CONFIG[name]["can_rx_id"]))
    return merged


def parse_int(value: str, label: str) -> int:
    try:
        return int(value.strip(), 0)
    except ValueError as exc:
        raise ValueError(f"{label} must be an integer") from exc


def validate_config(cfg: dict) -> dict:
    cfg = normalize_config(cfg)
    for name in ("av_bay", "fill_box"):
        item = cfg[name]
        if item["interface"] not in INTERFACE_OPTIONS:
            raise ValueError(f"{name} interface is invalid")
        if not item["port"].strip():
            raise ValueError(f"{name} device / port / iface cannot be empty")
        if int(item["baud_rate"]) <= 0:
            raise ValueError(f"{name} baud_rate must be positive")
        if int(item["spi_speed_hz"]) <= 0:
            raise ValueError(f"{name} spi_speed_hz must be positive")
        if int(item["spi_mode"]) not in (0, 1, 2, 3):
            raise ValueError(f"{name} spi_mode must be 0, 1, 2, or 3")
        if int(item["spi_bits_per_word"]) <= 0:
            raise ValueError(f"{name} spi_bits_per_word must be positive")
        if int(item["can_tx_id"]) < 0 or int(item["can_rx_id"]) < 0:
            raise ValueError(f"{name} CAN IDs must be non-negative")
    return cfg


def shutil_which(name: str) -> str | None:
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        candidate = Path(directory) / name
        if candidate.exists() and os.access(candidate, os.X_OK):
            return str(candidate)
    return None


def systemctl_unit_active(unit: str) -> bool | None:
    if not shutil_which("systemctl"):
        return None
    rc = os.system(f"systemctl is-active --quiet {unit} >/dev/null 2>&1")
    if os.WIFEXITED(rc):
        return os.WEXITSTATUS(rc) == 0
    return None


def uart_config_path() -> Path | None:
    for candidate in (Path("/boot/firmware/config.txt"), Path("/boot/config.txt")):
        if candidate.exists():
            return candidate
    return None


def boot_config_lines(path: Path | None) -> list[str]:
    if path is None or not path.exists():
        return []
    return path.read_text(encoding="utf-8", errors="ignore").lower().splitlines()


def uart_enabled() -> bool | None:
    for raw_line in boot_config_lines(uart_config_path()):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("enable_uart="):
            return line.split("=", 1)[1].strip() == "1"
    return None


def spi_enabled() -> bool | None:
    for raw_line in boot_config_lines(uart_config_path()):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("dtparam=spi="):
            return line.split("=", 2)[2].strip() in {"on", "1", "true"}
    return None


def cmdline_uses_serial_console() -> bool | None:
    path = Path("/boot/firmware/cmdline.txt")
    if not path.exists():
        path = Path("/boot/cmdline.txt")
    if not path.exists():
        return None
    text = path.read_text(encoding="utf-8", errors="ignore")
    return "console=serial0," in text or "console=ttyAMA0," in text


def interface_candidates(interface_name: str, env: EnvironmentInfo) -> list[str]:
    if interface_name == "raspberry_pi_gpio_uart":
        values = existing_paths(GPIO_PORT_CANDIDATES)
        for fallback in GPIO_PORT_CANDIDATES:
            if fallback not in values:
                values.append(fallback)
        return values
    if interface_name == "usb_serial":
        values = [port for port in env.serial_ports if port.startswith(USB_PORT_PREFIXES)]
        return values or env.serial_ports
    if interface_name == "custom_serial":
        return env.serial_ports
    if interface_name == "spi":
        return env.spi_devices
    if interface_name == "can":
        return env.can_interfaces
    return []


def build_help_lines(cfg: dict, env: EnvironmentInfo) -> list[str]:
    def yes_no_unknown(value: bool | None) -> str:
        if value is None:
            return "unknown"
        return "yes" if value else "no"

    lines = [
        "General:",
        f"- Default config path: {default_config_path()}",
        "- Raspberry Pi UART and SPI both use generic Linux device nodes. They do not require rppal for link "
        "transport.",
        "- Prefer stable device names such as /dev/serial/by-id when they exist.",
        "",
    ]

    wants_gpio_uart = any(cfg[name]["interface"] == "raspberry_pi_gpio_uart" for name in ("av_bay", "fill_box"))
    wants_spi = any(cfg[name]["interface"] == "spi" for name in ("av_bay", "fill_box"))
    wants_can = any(cfg[name]["interface"] == "can" for name in ("av_bay", "fill_box"))

    if wants_gpio_uart:
        uart_enabled_status = yes_no_unknown(uart_enabled())
        serial_console_status = yes_no_unknown(cmdline_uses_serial_console())
        getty_ttyama0_status = yes_no_unknown(systemctl_unit_active("serial-getty@ttyAMA0.service"))
        getty_serial0_status = yes_no_unknown(systemctl_unit_active("serial-getty@serial0.service"))
        lines.extend(
            [
                "Raspberry Pi GPIO UART setup:",
                "- Use a Linux serial device such as /dev/serial0, /dev/ttyAMA0, or /dev/ttyS0.",
                f"- enable_uart=1: {uart_enabled_status}",
                f"- Serial console in cmdline: {serial_console_status}",
                f"- serial-getty@ttyAMA0 active: {getty_ttyama0_status}",
                f"- serial-getty@serial0 active: {getty_serial0_status}",
                "1. Add or verify `enable_uart=1` in /boot/firmware/config.txt or /boot/config.txt.",
                "2. Disable the serial login console and serial-getty if they are using the same UART.",
                "3. Reboot after changing boot config or cmdline.",
                "4. Confirm the selected /dev/serial* or /dev/tty* path exists before starting the backend.",
                "",
            ]
        )

    if wants_spi:
        spi_enabled_status = yes_no_unknown(spi_enabled())
        lines.extend(
            [
                "Raspberry Pi / generic Linux SPI setup:",
                "- Use a Linux spidev node such as /dev/spidev0.0.",
                f"- SPI enabled in boot config: {spi_enabled_status}",
                "1. Enable SPI in the boot config or board firmware so /dev/spidev* appears.",
                "2. Confirm mode, bits-per-word, and speed match the attached device.",
                "3. Reboot if boot-config changes were required.",
                "4. Confirm the selected /dev/spidev* node exists before starting the backend.",
                "",
            ]
        )

    if wants_can:
        lines.extend(
            [
                "CAN setup:",
                "- Use an interface such as can0 or vcan0.",
                "1. Load modules if needed: `sudo modprobe can can_raw`.",
                "2. For vcan: `sudo modprobe vcan && sudo ip link add dev vcan0 type vcan && sudo ip link set vcan0 "
                "up`.",
                "3. For hardware CAN: `sudo ip link set can0 down ; sudo ip link set can0 type can bitrate 500000 ; "
                "sudo ip link set can0 up`.",
                "4. Confirm with `ip -details link show can0`.",
                "",
            ]
        )

    if not (wants_gpio_uart or wants_spi or wants_can):
        lines.extend(
            [
                "Serial setup:",
                "- Plug the device in, refresh detection, and select the correct serial node.",
                "- Prefer /dev/serial/by-id entries when available.",
            ]
        )

    return lines


def print_help_block(cfg: dict, env: EnvironmentInfo) -> None:
    print("\nDetected interfaces:")
    print("Serial:", ", ".join(env.serial_ports) or "none")
    print("SPI   :", ", ".join(env.spi_devices) or "none")
    print("CAN   :", ", ".join(env.can_interfaces) or "none")
    print()
    print("\n".join(build_help_lines(cfg, env)))


def display_available() -> bool:
    if not TK_AVAILABLE:
        return False
    if sys.platform == "linux":
        return bool(os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY"))
    return True


def set_link_field(cfg: dict, link_name: str, field_name: str, value: str | None) -> None:
    if value is None:
        return
    key = link_name.replace("-", "_")
    if field_name in {"baud_rate", "spi_speed_hz", "spi_mode", "spi_bits_per_word", "can_tx_id", "can_rx_id"}:
        cfg[key][field_name] = int(value, 0)
    else:
        cfg[key][field_name] = value


def apply_cli_overrides(cfg: dict, args: argparse.Namespace) -> dict:
    cfg = normalize_config(cfg)
    mapping = {
        "av_bay": "av-bay",
        "fill_box": "fill-box",
    }
    for key, prefix in mapping.items():
        set_link_field(cfg, key, "interface", getattr(args, f"{prefix.replace('-', '_')}_interface"))
        set_link_field(cfg, key, "port", getattr(args, f"{prefix.replace('-', '_')}_port"))
        set_link_field(cfg, key, "baud_rate", getattr(args, f"{prefix.replace('-', '_')}_baud"))
        set_link_field(cfg, key, "spi_speed_hz", getattr(args, f"{prefix.replace('-', '_')}_spi_speed_hz"))
        set_link_field(cfg, key, "spi_mode", getattr(args, f"{prefix.replace('-', '_')}_spi_mode"))
        set_link_field(cfg, key, "spi_bits_per_word", getattr(args, f"{prefix.replace('-', '_')}_spi_bits"))
        set_link_field(cfg, key, "can_tx_id", getattr(args, f"{prefix.replace('-', '_')}_can_tx_id"))
        set_link_field(cfg, key, "can_rx_id", getattr(args, f"{prefix.replace('-', '_')}_can_rx_id"))
    return validate_config(cfg)


def prompt(label: str, default: str) -> str:
    answer = input(f"{label} [{default}]: ").strip()
    return answer or default


def prompt_choice(label: str, options: list[str], default: str) -> str:
    print(f"{label}:")
    for idx, option in enumerate(options, start=1):
        marker = " (default)" if option == default else ""
        print(f"  {idx}. {option}{marker}")
    answer = input("> ").strip()
    if not answer:
        return default
    if answer.isdigit():
        idx = int(answer) - 1
        if 0 <= idx < len(options):
            return options[idx]
    if answer in options:
        return answer
    print("Invalid choice, using default.")
    return default


def configure_link_tui(link_label: str, link_key: str, cfg: dict, env: EnvironmentInfo) -> None:
    current = cfg[link_key]
    interface_name = prompt_choice(
        f"{link_label} interface",
        INTERFACE_OPTIONS,
        current["interface"],
    )
    current["interface"] = interface_name
    candidates = interface_candidates(interface_name, env)
    if candidates:
        print(f"{link_label} detected candidates: {', '.join(candidates)}")
    current["port"] = prompt(f"{link_label} device / port / iface", current["port"])
    current["baud_rate"] = parse_int(prompt(f"{link_label} baud rate", str(current["baud_rate"])),
                                     f"{link_label} baud rate")
    current["spi_speed_hz"] = parse_int(
        prompt(f"{link_label} SPI speed Hz", str(current["spi_speed_hz"])),
        f"{link_label} SPI speed Hz",
    )
    current["spi_mode"] = parse_int(prompt(f"{link_label} SPI mode", str(current["spi_mode"])),
                                    f"{link_label} SPI mode")
    current["spi_bits_per_word"] = parse_int(
        prompt(f"{link_label} SPI bits per word", str(current["spi_bits_per_word"])),
        f"{link_label} SPI bits per word",
    )
    current["can_tx_id"] = parse_int(prompt(f"{link_label} CAN tx id", hex(current["can_tx_id"])),
                                     f"{link_label} CAN tx id")
    current["can_rx_id"] = parse_int(prompt(f"{link_label} CAN rx id", hex(current["can_rx_id"])),
                                     f"{link_label} CAN rx id")


def run_tui(config_path: Path) -> int:
    env = collect_environment()
    cfg = normalize_config(load_config(config_path))
    print(f"GroundStation radio link setup (TUI)\nConfig file: {config_path}\nOS: {env.os_label}")
    if env.pi_model:
        print(f"Detected board: {env.pi_model}")
    configure_link_tui("AV bay", "av_bay", cfg, env)
    configure_link_tui("Fill box", "fill_box", cfg, env)
    cfg = validate_config(cfg)
    print_help_block(cfg, env)
    answer = input("\nSave configuration? [Y/n]: ").strip().lower()
    if answer in {"", "y", "yes"}:
        save_config(config_path, cfg)
        print(f"Saved {config_path}")
        return 0
    print("Configuration not saved.")
    return 1


if TK_AVAILABLE:
    @dataclass
    class LinkWidgets:
        interface_var: tk.StringVar
        port_var: tk.StringVar
        baud_var: tk.StringVar
        spi_speed_var: tk.StringVar
        spi_mode_var: tk.StringVar
        spi_bits_var: tk.StringVar
        can_tx_var: tk.StringVar
        can_rx_var: tk.StringVar
        port_combo: ttk.Combobox


    class RadioConfigGui(tk.Tk):
        def __init__(self, config_path: Path) -> None:
            super().__init__()
            self.title("GroundStation Radio Link Setup")
            self.geometry("1280x780")
            self.config_path = config_path
            self.env = collect_environment()
            self.cfg = normalize_config(load_config(config_path))
            self.path_var = tk.StringVar(value=str(config_path))
            self.status_var = tk.StringVar(value="Ready")
            self.link_widgets: dict[str, LinkWidgets] = {}

            self.columnconfigure(0, weight=1)
            self.rowconfigure(2, weight=1)
            self._build_header()
            self._build_links()
            self._build_help()
            self._build_footer()
            self._refresh_help()

        def _build_header(self) -> None:
            frame = ttk.Frame(self, padding=12)
            frame.grid(row=0, column=0, sticky="ew")
            frame.columnconfigure(1, weight=1)
            ttk.Label(frame, text="Config file").grid(row=0, column=0, sticky="w")
            ttk.Entry(frame, textvariable=self.path_var).grid(row=0, column=1, sticky="ew", padx=(8, 0))
            pi_text = self.env.pi_model or "Not detected"
            summary = (
                f"OS: {self.env.os_label}\n"
                f"Raspberry Pi: {'yes' if self.env.pi_detected else 'no'}\n"
                f"Model: {pi_text}"
            )
            ttk.Label(frame, text=summary, justify="left").grid(
                row=1, column=0, columnspan=2, sticky="w", pady=(10, 0)
            )

        def _build_links(self) -> None:
            frame = ttk.LabelFrame(self, text="Link Selection", padding=12)
            frame.grid(row=1, column=0, sticky="ew", padx=12)
            for col in range(9):
                frame.columnconfigure(col, weight=1)
            headers = ["Link", "Interface", "Device / Port / Iface", "Baud", "SPI Hz", "SPI mode", "SPI bits",
                       "CAN Tx ID", "CAN Rx ID"]
            for idx, label in enumerate(headers):
                ttk.Label(frame, text=label).grid(row=0, column=idx, sticky="w", padx=(0, 8))
            self._build_link_row(frame, 1, "av_bay", "AV bay")
            self._build_link_row(frame, 2, "fill_box", "Fill box")
            ttk.Button(frame, text="Refresh detected interfaces", command=self._refresh_devices).grid(row=3, column=0,
                                                                                                      sticky="w",
                                                                                                      pady=(12, 0))

        def _build_link_row(self, parent: ttk.Frame, row: int, key: str, label: str) -> None:
            cfg = self.cfg[key]
            widgets = LinkWidgets(
                interface_var=tk.StringVar(value=cfg["interface"]),
                port_var=tk.StringVar(value=cfg["port"]),
                baud_var=tk.StringVar(value=str(cfg["baud_rate"])),
                spi_speed_var=tk.StringVar(value=str(cfg["spi_speed_hz"])),
                spi_mode_var=tk.StringVar(value=str(cfg["spi_mode"])),
                spi_bits_var=tk.StringVar(value=str(cfg["spi_bits_per_word"])),
                can_tx_var=tk.StringVar(value=hex(cfg["can_tx_id"])),
                can_rx_var=tk.StringVar(value=hex(cfg["can_rx_id"])),
                port_combo=ttk.Combobox(parent),
            )
            ttk.Label(parent, text=label).grid(row=row, column=0, sticky="w", padx=(0, 8), pady=6)
            interface_combo = ttk.Combobox(parent, textvariable=widgets.interface_var, values=INTERFACE_OPTIONS,
                                           state="readonly")
            interface_combo.grid(row=row, column=1, sticky="ew", padx=(0, 8), pady=6)
            widgets.port_combo.configure(textvariable=widgets.port_var)
            widgets.port_combo.grid(row=row, column=2, sticky="ew", padx=(0, 8), pady=6)
            ttk.Entry(parent, textvariable=widgets.baud_var).grid(row=row, column=3, sticky="ew", padx=(0, 8), pady=6)
            ttk.Entry(parent, textvariable=widgets.spi_speed_var).grid(row=row, column=4, sticky="ew", padx=(0, 8),
                                                                       pady=6)
            ttk.Entry(parent, textvariable=widgets.spi_mode_var).grid(row=row, column=5, sticky="ew", padx=(0, 8),
                                                                      pady=6)
            ttk.Entry(parent, textvariable=widgets.spi_bits_var).grid(row=row, column=6, sticky="ew", padx=(0, 8),
                                                                      pady=6)
            ttk.Entry(parent, textvariable=widgets.can_tx_var).grid(row=row, column=7, sticky="ew", padx=(0, 8), pady=6)
            ttk.Entry(parent, textvariable=widgets.can_rx_var).grid(row=row, column=8, sticky="ew", pady=6)
            self.link_widgets[key] = widgets
            interface_combo.bind("<<ComboboxSelected>>", lambda _event, name=key: self._sync_port_options(name))
            for variable in (
                    widgets.interface_var,
                    widgets.port_var,
                    widgets.baud_var,
                    widgets.spi_speed_var,
                    widgets.spi_mode_var,
                    widgets.spi_bits_var,
                    widgets.can_tx_var,
                    widgets.can_rx_var,
            ):
                variable.trace_add("write", lambda *_args: self._refresh_help())
            self._sync_port_options(key)

        def _build_help(self) -> None:
            frame = ttk.LabelFrame(self, text="Detected Devices And Setup Guidance", padding=12)
            frame.grid(row=2, column=0, sticky="nsew", padx=12, pady=12)
            frame.columnconfigure(0, weight=1)
            frame.rowconfigure(1, weight=1)
            self.detected_label = ttk.Label(frame, justify="left")
            self.detected_label.grid(row=0, column=0, sticky="ew")
            self.help_text = tk.Text(frame, wrap="word", height=22)
            self.help_text.grid(row=1, column=0, sticky="nsew", pady=(10, 0))
            self.help_text.configure(state="disabled")

        def _build_footer(self) -> None:
            frame = ttk.Frame(self, padding=(12, 0, 12, 12))
            frame.grid(row=3, column=0, sticky="ew")
            frame.columnconfigure(1, weight=1)
            ttk.Button(frame, text="Save config", command=self._save).grid(row=0, column=0, sticky="w")
            ttk.Label(frame, textvariable=self.status_var).grid(row=0, column=1, sticky="ew", padx=12)
            ttk.Button(frame, text="Quit", command=self.destroy).grid(row=0, column=2, sticky="e")

        def _refresh_devices(self) -> None:
            self.env = collect_environment()
            for key in self.link_widgets:
                self._sync_port_options(key)
            self._refresh_help()

        def _sync_port_options(self, key: str) -> None:
            widgets = self.link_widgets[key]
            candidates = interface_candidates(widgets.interface_var.get(), self.env)
            widgets.port_combo["values"] = candidates
            if not widgets.port_var.get() and candidates:
                widgets.port_var.set(candidates[0])
            self._refresh_help()

        def _collect_config(self) -> dict:
            cfg = {"version": 1}
            for key, widgets in self.link_widgets.items():
                cfg[key] = {
                    "interface": widgets.interface_var.get().strip(),
                    "port": widgets.port_var.get().strip(),
                    "baud_rate": parse_int(widgets.baud_var.get(), f"{key} baud rate"),
                    "spi_speed_hz": parse_int(widgets.spi_speed_var.get(), f"{key} spi_speed_hz"),
                    "spi_mode": parse_int(widgets.spi_mode_var.get(), f"{key} spi_mode"),
                    "spi_bits_per_word": parse_int(widgets.spi_bits_var.get(), f"{key} spi_bits_per_word"),
                    "can_tx_id": parse_int(widgets.can_tx_var.get(), f"{key} can_tx_id"),
                    "can_rx_id": parse_int(widgets.can_rx_var.get(), f"{key} can_rx_id"),
                }
            return validate_config(cfg)

        def _save(self) -> None:
            self.config_path = Path(self.path_var.get()).expanduser().resolve()
            try:
                cfg = self._collect_config()
                save_config(self.config_path, cfg)
            except Exception as err:
                assert messagebox is not None
                messagebox.showerror("Save failed", str(err))
                self.status_var.set(f"Save failed: {err}")
                return
            self.status_var.set(f"Saved {self.config_path}")
            assert messagebox is not None
            messagebox.showinfo("Saved", f"Saved {self.config_path}")

        def _refresh_help(self) -> None:
            cfg = self._collect_config_safe()
            detected_text = (
                    "Serial: " + ", ".join(self.env.serial_ports) + "\n"
                                                                    "SPI: " + ", ".join(self.env.spi_devices) + "\n"
                                                                                                                "CAN: "
                                                                                                                "" +
                    ", ".join(
                        self.env.can_interfaces)
            )
            self.detected_label.configure(text=detected_text)
            self.help_text.configure(state="normal")
            self.help_text.delete("1.0", "end")
            self.help_text.insert("1.0", "\n".join(build_help_lines(cfg, self.env)))
            self.help_text.configure(state="disabled")

        def _collect_config_safe(self) -> dict:
            try:
                return self._collect_config()
            except Exception:
                return normalize_config(self.cfg)


def run_gui(config_path: Path) -> int:
    assert TK_AVAILABLE
    app = RadioConfigGui(config_path)
    app.mainloop()
    return 0


def run_cli(config_path: Path, args: argparse.Namespace) -> int:
    cfg = apply_cli_overrides(load_config(config_path), args)
    save_config(config_path, cfg)
    env = collect_environment()
    print(f"Saved {config_path}")
    print_help_block(cfg, env)
    return 0


def main() -> int:
    args = parse_args()
    config_path = Path(args.config).expanduser().resolve()

    if args.gui and not TK_AVAILABLE:
        print("Error: tkinter is not available, so GUI mode cannot start. Use --tui or --cli.", file=sys.stderr)
        return 2
    if args.gui and not display_available():
        print("Error: no display detected for GUI mode. Use --tui or --cli instead.", file=sys.stderr)
        return 2

    if args.cli:
        return run_cli(config_path, args)
    if args.tui:
        return run_tui(config_path)
    if args.gui:
        return run_gui(config_path)

    if display_available():
        return run_gui(config_path)

    print("No display detected. Falling back to terminal configuration mode.\n")
    return run_tui(config_path)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        print("\nAborted.")
        raise SystemExit(130)
