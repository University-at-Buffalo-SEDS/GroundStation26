#!/usr/bin/env python3

"""Standalone fill-system TUI using the backend comms config."""

from __future__ import annotations

import argparse
import curses
import ctypes
import errno
import json
import os
import select
import sys
import threading
import time
from collections import Counter, deque
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

try:
    import serial
except ModuleNotFoundError as e:
    raise SystemExit(
        "Missing dependency 'pyserial'. Install it with `python -m pip install pyserial`."
    ) from e

try:
    import sedsprintf_rs_2026 as seds
except ModuleNotFoundError:
    try:
        import sedsprintf_rs as seds
    except ModuleNotFoundError as e:
        raise SystemExit(
            "Missing dependency 'sedsprintf_rs_2026' or 'sedsprintf_rs'. Build/install the Python module first."
        ) from e


DT = seds.DataType
EP = seds.DataEndpoint
RM = seds.RouterMode

SERIAL_DEFAULT_BAUD = 57_600
SERIAL_OVERRIDE_DEFAULT_BAUD = 115_200
I2C_DEFAULT_BUS = 1
I2C_DEFAULT_ADDR = 0x55
I2C_DEFAULT_CHUNK_DELAY_MS = 1
I2C_DEFAULT_INITIAL_WAIT_MS = 10
RAW_UART_MAX_FRAME_BYTES = 4096
I2C_SLOT_SIZE = 32
I2C_SLOT_HEADER_SIZE = 18
I2C_SLOT_PAYLOAD_SIZE = I2C_SLOT_SIZE - I2C_SLOT_HEADER_SIZE
I2C_SLOT_MAGIC_0 = 0x49
I2C_SLOT_MAGIC_1 = 0x32
I2C_SLOT_VERSION = 1
I2C_KIND_IDLE = 0
I2C_KIND_DATA = 1
I2C_KIND_ERROR = 127
I2C_FLAG_START = 0x01
I2C_FLAG_END = 0x02
I2C_M_RD = 0x0001
I2C_RDWR = 0x0707
LIBC = ctypes.CDLL(None, use_errno=True)


def _enum_value(enum_cls: object, *names: str) -> int:
    for name in names:
        if hasattr(enum_cls, name):
            return int(getattr(enum_cls, name))
    available = [name for name in dir(enum_cls) if not name.startswith("_")]
    raise AttributeError(
        f"{enum_cls.__class__.__name__} missing names {names}. Available: {available}"
    )


FLIGHT_COMMAND_TYPE = _enum_value(DT, "FLIGHT_COMMAND", "FlightCommand")
VALVE_COMMAND_TYPE = _enum_value(DT, "VALVE_COMMAND", "ValveCommand")
ACTUATOR_COMMAND_TYPE = _enum_value(DT, "ACTUATOR_COMMAND", "ActuatorCommand")
GROUNDSTATION_ENDPOINT = _enum_value(EP, "GROUND_STATION", "GroundStation")
FLIGHT_CONTROLLER_ENDPOINT = _enum_value(EP, "FLIGHT_CONTROLLER", "FlightController")
VALVE_BOARD_ENDPOINT = _enum_value(EP, "VALVE_BOARD", "ValveBoard")
ACTUATOR_BOARD_ENDPOINT = _enum_value(EP, "ACTUATOR_BOARD", "ActuatorBoard")
DISCOVERY_ENDPOINT = _enum_value(EP, "DISCOVERY", "Discovery")
HEARTBEAT_ENDPOINT = _enum_value(EP, "HEART_BEAT", "HEARTBEAT", "Heartbeat", "HeartBeat")
FLIGHT_STATE_ENDPOINT = _enum_value(EP, "FLIGHT_STATE", "FlightState")
ABORT_ENDPOINT = _enum_value(EP, "ABORT", "Abort")
SD_CARD_ENDPOINT = _enum_value(EP, "SD_CARD", "SdCard")


def now_ms() -> int:
    return int(time.time() * 1000)


def attr_or_call(obj: object, name: str, default: object = None) -> object:
    if not hasattr(obj, name):
        return default
    value = getattr(obj, name)
    if callable(value):
        try:
            return value()
        except TypeError:
            return default
    return value


def hex_preview(data: bytes, limit: int = 16) -> str:
    preview = " ".join(f"{byte:02x}" for byte in data[:limit])
    if len(data) > limit:
        return f"{preview} ..."
    return preview


def safe_decode_ascii(data: bytes, limit: int = 48) -> str:
    text = "".join(chr(b) if 32 <= b <= 126 else "." for b in data[:limit])
    if len(data) > limit:
        return f"{text}..."
    return text


def is_valid_serialized_frame(data: bytes) -> bool:
    serialize_mod = getattr(seds, "serialize", None)
    peek = None
    if serialize_mod is not None and hasattr(serialize_mod, "peek_frame_info"):
        peek = getattr(serialize_mod, "peek_frame_info")
    elif hasattr(seds, "peek_frame_info"):
        peek = getattr(seds, "peek_frame_info")
    elif hasattr(seds, "peek_frame_info_py"):
        peek = getattr(seds, "peek_frame_info_py")

    if peek is not None:
        try:
            peek(data)
            return True
        except Exception:
            return False

    try:
        pkt = seds.deserialize_packet_py(data)
    except Exception:
        return False
    return int(pkt.wire_size()) == len(data)


@dataclass(frozen=True)
class CommandSpec:
    label: str
    data_type: int
    endpoint: int
    command_id: int
    detail: str


@dataclass
class PacketEvent:
    timestamp_ms: int
    sender: str
    data_type: str
    endpoints: list[str]
    payload_len: int
    payload_hex: str
    payload_text: str


def build_command_groups(include_hitl: bool) -> list[tuple[str, list[CommandSpec]]]:
    rocket = [
        CommandSpec("Launch", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 3, "FlightCommands::Launch"),
    ]
    if include_hitl:
        rocket.extend(
            [
                CommandSpec("DeployParachute", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 0, "FlightComputerCommands"),
                CommandSpec("ExpandParachute", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 1, "FlightComputerCommands"),
                CommandSpec("ReinitSensors", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 2, "FlightComputerCommands"),
                CommandSpec("LaunchSignal", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 3, "FlightComputerCommands"),
                CommandSpec("EvaluationRelax", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 4, "FlightComputerCommands"),
                CommandSpec("EvaluationFocus", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 5, "FlightComputerCommands"),
                CommandSpec("EvaluationAbort", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 6, "FlightComputerCommands"),
                CommandSpec("ReinitBarometer", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 7, "FlightComputerCommands"),
                CommandSpec("EnableIMU", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 8, "FlightComputerCommands"),
                CommandSpec("DisableIMU", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 9, "FlightComputerCommands"),
                CommandSpec("MonitorAltitude", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 10, "FlightComputerCommands"),
                CommandSpec("RevokeMonitorAltitude", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 11, "FlightComputerCommands"),
                CommandSpec("ConsecutiveSamples", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 12, "FlightComputerCommands"),
                CommandSpec("RevokeConsecutiveSamples", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 13, "FlightComputerCommands"),
                CommandSpec("ResetFailures", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 14, "FlightComputerCommands"),
                CommandSpec("RevokeResetFailures", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 15, "FlightComputerCommands"),
                CommandSpec("ValidateMeasms", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 16, "FlightComputerCommands"),
                CommandSpec("RevokeValidateMeasms", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 17, "FlightComputerCommands"),
                CommandSpec("AbortAfter15", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 18, "FlightComputerCommands"),
                CommandSpec("AbortAfter40", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 19, "FlightComputerCommands"),
                CommandSpec("AbortAfter70", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 20, "FlightComputerCommands"),
                CommandSpec("ReinitAfter12", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 21, "FlightComputerCommands"),
                CommandSpec("ReinitAfter26", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 22, "FlightComputerCommands"),
                CommandSpec("ReinitAfter44", FLIGHT_COMMAND_TYPE, FLIGHT_CONTROLLER_ENDPOINT, 23, "FlightComputerCommands"),
            ]
        )

    valve = [
        CommandSpec("PilotOpen", VALVE_COMMAND_TYPE, VALVE_BOARD_ENDPOINT, 0, "ValveBoardCommands"),
        CommandSpec("NormallyOpenOpen", VALVE_COMMAND_TYPE, VALVE_BOARD_ENDPOINT, 1, "ValveBoardCommands"),
        CommandSpec("DumpOpen", VALVE_COMMAND_TYPE, VALVE_BOARD_ENDPOINT, 2, "ValveBoardCommands"),
        CommandSpec("PilotClose", VALVE_COMMAND_TYPE, VALVE_BOARD_ENDPOINT, 3, "ValveBoardCommands"),
        CommandSpec("NormallyOpenClose", VALVE_COMMAND_TYPE, VALVE_BOARD_ENDPOINT, 4, "ValveBoardCommands"),
        CommandSpec("DumpClose", VALVE_COMMAND_TYPE, VALVE_BOARD_ENDPOINT, 5, "ValveBoardCommands"),
        CommandSpec("Sequence", VALVE_COMMAND_TYPE, VALVE_BOARD_ENDPOINT, 6, "ValveBoardCommands"),
    ]
    actuator = [
        CommandSpec("IgniterOn", ACTUATOR_COMMAND_TYPE, ACTUATOR_BOARD_ENDPOINT, 7, "ActuatorBoardCommands"),
        CommandSpec("RetractPlumbing", ACTUATOR_COMMAND_TYPE, ACTUATOR_BOARD_ENDPOINT, 8, "ActuatorBoardCommands"),
        CommandSpec("NitrogenOpen", ACTUATOR_COMMAND_TYPE, ACTUATOR_BOARD_ENDPOINT, 9, "ActuatorBoardCommands"),
        CommandSpec("NitrousOpen", ACTUATOR_COMMAND_TYPE, ACTUATOR_BOARD_ENDPOINT, 10, "ActuatorBoardCommands"),
        CommandSpec("IgniterOff", ACTUATOR_COMMAND_TYPE, ACTUATOR_BOARD_ENDPOINT, 11, "ActuatorBoardCommands"),
        CommandSpec("NitrogenClose", ACTUATOR_COMMAND_TYPE, ACTUATOR_BOARD_ENDPOINT, 12, "ActuatorBoardCommands"),
        CommandSpec("NitrousClose", ACTUATOR_COMMAND_TYPE, ACTUATOR_BOARD_ENDPOINT, 13, "ActuatorBoardCommands"),
        CommandSpec("IgniterSequence", ACTUATOR_COMMAND_TYPE, ACTUATOR_BOARD_ENDPOINT, 14, "ActuatorBoardCommands"),
    ]
    return [("Rocket", rocket), ("Valve", valve), ("Actuator", actuator)]


def backend_root_from_script(script_path: Path) -> Path:
    return script_path.resolve().parents[2]


def comms_config_path(backend_root: Path) -> Path:
    env_path = os.environ.get("GS_COMMS_LINK_CONFIG") or os.environ.get("GS_RADIO_LINK_CONFIG")
    if env_path:
        return Path(env_path)
    return backend_root / "backend" / "comms" / "comms.json"


def load_fill_link_config(backend_root: Path) -> dict:
    path = comms_config_path(backend_root)
    if not path.exists():
        raise SystemExit(f"Comms config not found: {path}")
    with path.open("r", encoding="utf-8") as fh:
        raw = json.load(fh)

    fill = raw.get("fill_box") or {}
    interface = fill.get("interface")
    if not interface:
        raise SystemExit(f"fill_box.interface missing in comms config: {path}")
    if interface in {"serial", "raspberry_pi_gpio_uart", "custom_serial"}:
        protocol = fill.get("protocol", "raw_uart")
        return {
            "interface": interface,
            "protocol": protocol,
            "port": fill.get("port", "/dev/ttyUSB2"),
            "baud_rate": int(fill.get("baud_rate", SERIAL_DEFAULT_BAUD)),
        }
    if interface == "i2c":
        bus = fill.get("bus")
        if bus is None:
            port = str(fill.get("port", "")).strip()
            if port.startswith("/dev/i2c-"):
                try:
                    bus = int(port.rsplit("-", 1)[1], 10)
                except (IndexError, ValueError):
                    raise SystemExit(f"Invalid fill_box I2C port in comms config: {port}") from None
        return {
            "interface": interface,
            "bus": int(bus if bus is not None else I2C_DEFAULT_BUS),
            "addr": int(fill.get("addr", I2C_DEFAULT_ADDR)),
            "chunk_delay_ms": int(fill.get("chunk_delay_ms", I2C_DEFAULT_CHUNK_DELAY_MS)),
            "initial_wait_ms": int(fill.get("initial_wait_ms", I2C_DEFAULT_INITIAL_WAIT_MS)),
        }
    raise SystemExit(f"Unsupported fill_box interface in comms config: {interface}")


def resolve_fill_link_config(args: argparse.Namespace, backend_root: Path) -> dict:
    if args.config:
        os.environ["GS_COMMS_LINK_CONFIG"] = args.config
    cfg = load_fill_link_config(backend_root)

    if args.interface is None:
        return cfg

    interface = args.interface
    if interface in {"serial", "raspberry_pi_gpio_uart", "custom_serial"}:
        protocol = args.protocol or "raw_uart"
        return {
            "interface": interface,
            "protocol": protocol,
            "tx_protocol": args.tx_protocol or protocol,
            "rx_protocol": args.rx_protocol or protocol,
            "port": args.port or "/dev/ttyUSB2",
            "baud_rate": int(
                args.baud_rate
                if args.baud_rate is not None
                else SERIAL_OVERRIDE_DEFAULT_BAUD
            ),
        }

    if interface == "i2c":
        bus = args.bus if args.bus is not None else cfg.get("bus", I2C_DEFAULT_BUS)
        if args.port:
            port = args.port.strip()
            if not port.startswith("/dev/i2c-"):
                raise SystemExit(f"--port must look like /dev/i2c-N for --interface i2c, got: {port}")
            try:
                bus = int(port.rsplit("-", 1)[1], 10)
            except (IndexError, ValueError):
                raise SystemExit(f"Invalid I2C port: {port}") from None
        return {
            "interface": interface,
            "bus": int(bus),
            "addr": int(args.addr if args.addr is not None else cfg.get("addr", I2C_DEFAULT_ADDR)),
            "chunk_delay_ms": int(
                args.chunk_delay_ms
                if args.chunk_delay_ms is not None
                else cfg.get("chunk_delay_ms", I2C_DEFAULT_CHUNK_DELAY_MS)
            ),
            "initial_wait_ms": int(
                args.initial_wait_ms
                if args.initial_wait_ms is not None
                else cfg.get("initial_wait_ms", I2C_DEFAULT_INITIAL_WAIT_MS)
            ),
        }

    raise SystemExit(f"Unsupported interface override: {interface}")


class Transport:
    def describe(self) -> str:
        raise NotImplementedError

    def send_serialized(self, payload: bytes) -> None:
        raise NotImplementedError

    def read_serialized(self, timeout: float) -> bytes | None:
        raise NotImplementedError

    def close(self) -> None:
        raise NotImplementedError

    def activity_snapshot(self) -> dict[str, object]:
        return {}


class SerialTransport(Transport):
    def __init__(self, cfg: dict) -> None:
        self.protocol = cfg["protocol"]
        self.tx_protocol = cfg.get("tx_protocol", self.protocol)
        self.rx_protocol = cfg.get("rx_protocol", self.protocol)
        self.rx_buf = bytearray()
        self.raw_reads = 0
        self.raw_bytes = 0
        self.last_raw_ms: int | None = None
        self.last_chunk_hex = ""
        self.tx_writes = 0
        self.tx_bytes = 0
        self.last_tx_ms: int | None = None
        self.last_tx_hex = ""
        self.ser = serial.Serial(
            port=cfg["port"],
            baudrate=cfg["baud_rate"],
            timeout=0.05,
            inter_byte_timeout=0.02,
            write_timeout=1.0,
            bytesize=serial.EIGHTBITS,
            parity=serial.PARITY_NONE,
            stopbits=serial.STOPBITS_ONE,
            xonxoff=False,
            rtscts=False,
            dsrdtr=False,
        )

    def describe(self) -> str:
        if self.tx_protocol == self.rx_protocol:
            return f"serial {self.ser.port} {self.ser.baudrate} {self.tx_protocol}"
        return (
            f"serial {self.ser.port} {self.ser.baudrate} "
            f"tx={self.tx_protocol} rx={self.rx_protocol}"
        )

    def send_serialized(self, payload: bytes) -> None:
        wire = payload
        if self.tx_protocol == "packet_framed":
            wire = len(payload).to_bytes(2, "little") + payload
        self.ser.write(wire)
        self.ser.flush()
        self.tx_writes += 1
        self.tx_bytes += len(wire)
        self.last_tx_ms = now_ms()
        self.last_tx_hex = hex_preview(wire, limit=24)

    def _take_raw_uart_packet(self) -> bytes | None:
        if self.rx_buf and is_valid_serialized_frame(bytes(self.rx_buf)):
            payload = bytes(self.rx_buf)
            self.rx_buf.clear()
            return payload
        scan_len = min(len(self.rx_buf), RAW_UART_MAX_FRAME_BYTES)
        for start in range(scan_len):
            for end in range(start + 1, scan_len + 1):
                candidate = bytes(self.rx_buf[start:end])
                if not is_valid_serialized_frame(candidate):
                    continue
                del self.rx_buf[:end]
                return candidate
        if len(self.rx_buf) > RAW_UART_MAX_FRAME_BYTES:
            del self.rx_buf[: len(self.rx_buf) - RAW_UART_MAX_FRAME_BYTES]
        return None

    def _read_burst(self, timeout: float) -> bytes:
        deadline = time.monotonic() + timeout
        buf = bytearray()
        while time.monotonic() < deadline:
            chunk = self.ser.read(4096)
            if chunk:
                buf.extend(chunk)
                continue
            if buf:
                break
        return bytes(buf)

    def _take_packet_framed_packet(self) -> bytes | None:
        if len(self.rx_buf) < 2:
            return None
        frame_len = int.from_bytes(self.rx_buf[:2], "little")
        if len(self.rx_buf) < 2 + frame_len:
            return None
        payload = bytes(self.rx_buf[2 : 2 + frame_len])
        del self.rx_buf[: 2 + frame_len]
        return payload

    def read_serialized(self, timeout: float) -> bytes | None:
        packet = (
            self._take_packet_framed_packet()
            if self.rx_protocol == "packet_framed"
            else self._take_raw_uart_packet()
        )
        if packet is not None:
            return packet
        ready, _, _ = select.select([self.ser.fileno()], [], [], timeout)
        if not ready:
            return None
        chunk = self._read_burst(timeout)
        if chunk:
            self.rx_buf.extend(chunk)
            self.raw_reads += 1
            self.raw_bytes += len(chunk)
            self.last_raw_ms = now_ms()
            self.last_chunk_hex = hex_preview(chunk, limit=24)
        return (
            self._take_packet_framed_packet()
            if self.rx_protocol == "packet_framed"
            else self._take_raw_uart_packet()
        )

    def close(self) -> None:
        self.ser.close()

    def activity_snapshot(self) -> dict[str, object]:
        return {
            "kind": "serial",
            "raw_reads": self.raw_reads,
            "raw_bytes": self.raw_bytes,
            "last_raw_ms": self.last_raw_ms,
            "last_chunk_hex": self.last_chunk_hex,
            "buffered_bytes": len(self.rx_buf),
            "tx_writes": self.tx_writes,
            "tx_bytes": self.tx_bytes,
            "last_tx_ms": self.last_tx_ms,
            "last_tx_hex": self.last_tx_hex,
        }


class I2cMsg(ctypes.Structure):
    _fields_ = [
        ("addr", ctypes.c_uint16),
        ("flags", ctypes.c_uint16),
        ("len", ctypes.c_uint16),
        ("buf", ctypes.c_uint64),
    ]


class I2cRdwrIoctlData(ctypes.Structure):
    _fields_ = [
        ("msgs", ctypes.c_uint64),
        ("nmsgs", ctypes.c_uint32),
    ]


@dataclass
class I2cSlot:
    kind: int
    flags: int
    transfer_id: int
    offset: int
    total_len: int
    data: bytes


class I2cTransport(Transport):
    def __init__(self, cfg: dict) -> None:
        if sys.platform != "linux":
            raise SystemExit("I2C fill link support is only available on Linux.")
        self.path = f"/dev/i2c-{cfg['bus']}"
        self.addr = cfg["addr"]
        self.chunk_delay = cfg["chunk_delay_ms"] / 1000.0
        self.initial_wait = cfg["initial_wait_ms"] / 1000.0
        self.fd = os.open(self.path, os.O_RDWR)
        self.io_lock = threading.Lock()
        self.rx_payload_buf = bytearray()
        self.rx_assembly: dict | None = None
        self.tx_transfer_id = 1
        self.tx_backoff_until = 0.0
        self.raw_reads = 0
        self.raw_slots = 0
        self.invalid_slots = 0
        self.last_raw_ms: int | None = None
        self.last_slot_hex = ""
        self.transfer_starts = 0
        self.transfer_completes = 0
        self.transfer_resets = 0
        self.last_transfer_len = 0
        self.last_transfer_hex = ""
        self.tx_writes = 0
        self.tx_bytes = 0
        self.last_tx_ms: int | None = None
        self.last_tx_hex = ""

    def describe(self) -> str:
        return f"i2c {self.path} addr=0x{self.addr:02x}"

    def _ioctl(self, msg: I2cMsg) -> None:
        data = I2cRdwrIoctlData(msgs=ctypes.addressof(msg), nmsgs=1)
        rc = LIBC.ioctl(self.fd, I2C_RDWR, ctypes.byref(data))
        if rc < 0:
            err = ctypes.get_errno()
            raise OSError(err, os.strerror(err))

    def _transfer_write(self, payload: bytes) -> None:
        buf = ctypes.create_string_buffer(payload, len(payload))
        msg = I2cMsg(addr=self.addr, flags=0, len=len(payload), buf=ctypes.addressof(buf))
        self._ioctl(msg)
        self.tx_writes += 1
        self.tx_bytes += len(payload)
        self.last_tx_ms = now_ms()
        self.last_tx_hex = hex_preview(payload, limit=16)

    def _tx_backoff_active(self) -> bool:
        return time.monotonic() < self.tx_backoff_until

    def _arm_tx_backoff(self) -> None:
        self.tx_backoff_until = time.monotonic() + 1.0

    def _next_transfer_id(self) -> int:
        current = self.tx_transfer_id
        self.tx_transfer_id = ((self.tx_transfer_id + 1) & 0xFFFF) or 1
        return current

    def _transfer_read(self) -> bytes | None:
        raw = ctypes.create_string_buffer(I2C_SLOT_SIZE)
        msg = I2cMsg(addr=self.addr, flags=I2C_M_RD, len=I2C_SLOT_SIZE, buf=ctypes.addressof(raw))
        try:
            self._ioctl(msg)
        except OSError as err:
            if err.errno in {errno.EREMOTEIO, errno.ENXIO, errno.EIO, errno.ETIMEDOUT}:
                return None
            raise
        payload = raw.raw
        self.raw_reads += 1
        self.raw_slots += 1
        self.last_raw_ms = now_ms()
        self.last_slot_hex = hex_preview(payload, limit=16)
        return payload

    def _encode_slot(
        self, kind: int, flags: int, transfer_id: int, offset: int, total_len: int, data: bytes
    ) -> bytes:
        data = data[:I2C_SLOT_PAYLOAD_SIZE]
        slot = bytearray(I2C_SLOT_SIZE)
        slot[0] = I2C_SLOT_MAGIC_0
        slot[1] = I2C_SLOT_MAGIC_1
        slot[2] = I2C_SLOT_VERSION
        slot[3] = kind & 0xFF
        slot[4] = flags & 0xFF
        slot[5] = 0
        slot[6:10] = int(offset).to_bytes(4, "little")
        slot[10:14] = int(total_len).to_bytes(4, "little")
        slot[14:16] = len(data).to_bytes(2, "little")
        slot[16:18] = int(transfer_id).to_bytes(2, "little")
        slot[I2C_SLOT_HEADER_SIZE : I2C_SLOT_HEADER_SIZE + len(data)] = data
        return bytes(slot)

    def _decode_slot(self, raw: bytes) -> I2cSlot | None:
        if len(raw) != I2C_SLOT_SIZE:
            raise ValueError("invalid i2c slot size")
        if all(byte == 0x00 for byte in raw) or all(byte == 0xFF for byte in raw):
            return None
        if raw[0] == 0x00 and raw[1] == 0x00:
            return None
        if raw[0] != I2C_SLOT_MAGIC_0 or raw[1] != I2C_SLOT_MAGIC_1:
            raise ValueError("invalid i2c slot magic")
        if raw[2] != I2C_SLOT_VERSION:
            raise ValueError("invalid i2c slot version")
        kind = raw[3]
        if kind == I2C_KIND_IDLE:
            return None
        data_len = int.from_bytes(raw[14:16], "little")
        if data_len > I2C_SLOT_PAYLOAD_SIZE:
            raise ValueError("invalid i2c slot payload length")
        return I2cSlot(
            kind=kind,
            flags=raw[4],
            transfer_id=int.from_bytes(raw[16:18], "little"),
            offset=int.from_bytes(raw[6:10], "little"),
            total_len=int.from_bytes(raw[10:14], "little"),
            data=raw[I2C_SLOT_HEADER_SIZE : I2C_SLOT_HEADER_SIZE + data_len],
        )

    def _ingest_slot(self, slot: I2cSlot) -> tuple[int, bytes] | None:
        if slot.flags & I2C_FLAG_START:
            if slot.offset != 0:
                self.transfer_resets += 1
                self.rx_assembly = None
                return None
            if slot.total_len < len(slot.data):
                self.transfer_resets += 1
                self.rx_assembly = None
                return None
            self.transfer_starts += 1
            self.rx_assembly = {
                "kind": slot.kind,
                "transfer_id": slot.transfer_id,
                "total_len": slot.total_len,
                "next_offset": len(slot.data),
                "payload": bytearray(slot.data),
            }
            if slot.flags & I2C_FLAG_END:
                payload = bytes(self.rx_assembly["payload"])
                self.transfer_completes += 1
                self.last_transfer_len = len(payload)
                self.last_transfer_hex = hex_preview(payload, limit=24)
                self.rx_assembly = None
                return slot.kind, payload
            return None
        if self.rx_assembly is None:
            return None
        if slot.kind != self.rx_assembly["kind"]:
            self.transfer_resets += 1
            self.rx_assembly = None
            return None
        if slot.transfer_id != self.rx_assembly["transfer_id"]:
            self.transfer_resets += 1
            self.rx_assembly = None
            return None
        if slot.offset != self.rx_assembly["next_offset"]:
            self.transfer_resets += 1
            self.rx_assembly = None
            return None
        if len(self.rx_assembly["payload"]) + len(slot.data) > self.rx_assembly["total_len"]:
            self.transfer_resets += 1
            self.rx_assembly = None
            return None
        self.rx_assembly["payload"].extend(slot.data)
        self.rx_assembly["next_offset"] += len(slot.data)
        if slot.flags & I2C_FLAG_END:
            if len(self.rx_assembly["payload"]) != self.rx_assembly["total_len"]:
                self.transfer_resets += 1
                self.rx_assembly = None
                return None
            payload = bytes(self.rx_assembly["payload"])
            self.transfer_completes += 1
            self.last_transfer_len = len(payload)
            self.last_transfer_hex = hex_preview(payload, limit=24)
            self.rx_assembly = None
            return slot.kind, payload
        return None

    def _take_buffered_packet(self) -> bytes | None:
        scan_len = min(len(self.rx_payload_buf), RAW_UART_MAX_FRAME_BYTES)
        for start in range(scan_len):
            for end in range(start + 1, scan_len + 1):
                candidate = bytes(self.rx_payload_buf[start:end])
                if not is_valid_serialized_frame(candidate):
                    continue
                del self.rx_payload_buf[:end]
                return candidate
        if len(self.rx_payload_buf) > RAW_UART_MAX_FRAME_BYTES:
            del self.rx_payload_buf[: len(self.rx_payload_buf) - RAW_UART_MAX_FRAME_BYTES]
        return None

    def send_serialized(self, payload: bytes) -> None:
        with self.io_lock:
            if self._tx_backoff_active():
                return
            transfer_id = self._next_transfer_id()
            try:
                if not payload:
                    self._transfer_write(
                        self._encode_slot(I2C_KIND_DATA, I2C_FLAG_START | I2C_FLAG_END, transfer_id, 0, 0, b"")
                    )
                else:
                    offset = 0
                    while offset < len(payload):
                        end = min(offset + I2C_SLOT_PAYLOAD_SIZE, len(payload))
                        flags = 0
                        if offset == 0:
                            flags |= I2C_FLAG_START
                        if end >= len(payload):
                            flags |= I2C_FLAG_END
                        self._transfer_write(
                            self._encode_slot(
                                I2C_KIND_DATA, flags, transfer_id, offset, len(payload), payload[offset:end]
                            )
                        )
                        offset = end
                        if offset < len(payload) and self.chunk_delay > 0:
                            time.sleep(self.chunk_delay)
                if self.initial_wait > 0:
                    time.sleep(self.initial_wait)
                self.tx_backoff_until = 0.0
            except OSError as err:
                if err.errno in {errno.EREMOTEIO, errno.ENXIO, errno.ETIMEDOUT}:
                    self._arm_tx_backoff()
                    return
                raise

    def read_serialized(self, timeout: float) -> bytes | None:
        packet = self._take_buffered_packet()
        if packet is not None:
            return packet
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.io_lock:
                packet = self._take_buffered_packet()
                if packet is not None:
                    return packet
                raw = self._transfer_read()
            if raw is None:
                time.sleep(self.chunk_delay if self.chunk_delay > 0 else 0.001)
                continue
            try:
                slot = self._decode_slot(raw)
            except ValueError:
                self.invalid_slots += 1
                continue
            if slot is None:
                time.sleep(self.chunk_delay if self.chunk_delay > 0 else 0.001)
                continue
            if slot.flags & I2C_FLAG_START:
                assembled = self._ingest_slot(slot)
                if slot.flags & I2C_FLAG_END:
                    assert assembled is not None
                else:
                    time.sleep(self.chunk_delay if self.chunk_delay > 0 else 0.001)
                    continue
            else:
                if self.rx_assembly is None:
                    raise ValueError("slot arrived without an active transfer")
                assembled = self._ingest_slot(slot)
                if assembled is None:
                    time.sleep(self.chunk_delay if self.chunk_delay > 0 else 0.001)
                    continue
            kind, payload = assembled
            if kind == I2C_KIND_ERROR:
                return None
            if kind != I2C_KIND_DATA:
                continue
            if is_valid_serialized_frame(payload):
                return payload
            self.rx_payload_buf.extend(payload)
            packet = self._take_buffered_packet()
            if packet is not None:
                return packet
        return None

    def close(self) -> None:
        os.close(self.fd)

    def activity_snapshot(self) -> dict[str, object]:
        return {
            "kind": "i2c",
            "raw_reads": self.raw_reads,
            "raw_slots": self.raw_slots,
            "invalid_slots": self.invalid_slots,
            "last_raw_ms": self.last_raw_ms,
            "last_slot_hex": self.last_slot_hex,
            "transfer_starts": self.transfer_starts,
            "transfer_completes": self.transfer_completes,
            "transfer_resets": self.transfer_resets,
            "last_transfer_len": self.last_transfer_len,
            "last_transfer_hex": self.last_transfer_hex,
            "buffered_bytes": len(self.rx_payload_buf),
            "tx_writes": self.tx_writes,
            "tx_bytes": self.tx_bytes,
            "last_tx_ms": self.last_tx_ms,
            "last_tx_hex": self.last_tx_hex,
        }


class FillLinkApp:
    def __init__(self, transport: Transport, sender: str, commands: list[tuple[str, list[CommandSpec]]]) -> None:
        self.transport = transport
        self.sender = sender
        self.command_groups = commands
        self.group_index = 0
        self.command_index = 0
        self.packet_events: deque[PacketEvent] = deque(maxlen=200)
        self.packet_counts: Counter[tuple[str, str]] = Counter()
        self.sent_log: deque[str] = deque(maxlen=16)
        self.status = "Connected"
        self.error: str | None = None
        self.raw_rx_count = 0
        self.last_rx_ms: int | None = None
        self.lock = threading.Lock()
        self.stop_event = threading.Event()
        self.router = seds.Router(
            now_ms=now_ms,
            handlers=[
                (GROUNDSTATION_ENDPOINT, self._on_packet, None),
                (FLIGHT_CONTROLLER_ENDPOINT, self._on_packet, None),
                (VALVE_BOARD_ENDPOINT, self._on_packet, None),
                (ACTUATOR_BOARD_ENDPOINT, self._on_packet, None),
                (DISCOVERY_ENDPOINT, self._on_packet, None),
                (HEARTBEAT_ENDPOINT, self._on_packet, None),
                (FLIGHT_STATE_ENDPOINT, self._on_packet, None),
                (ABORT_ENDPOINT, self._on_packet, None),
                (SD_CARD_ENDPOINT, self._on_packet, None),
            ],
            mode=RM.Sink,
        )
        self.side_id = self.router.add_side_serialized("fill_link", self.transport.send_serialized)
        self.rx_thread = threading.Thread(target=self._rx_loop, daemon=True)

    def start(self) -> None:
        self.rx_thread.start()

    def stop(self) -> None:
        self.stop_event.set()
        self.transport.close()
        self.rx_thread.join(timeout=1.0)

    def _extract_event(self, pkt: object) -> PacketEvent:
        endpoints = []
        for endpoint in attr_or_call(pkt, "endpoints", []):
            endpoints.append(str(endpoint))
        payload = b""
        try:
            payload = bytes(attr_or_call(pkt, "data", b""))
        except Exception:
            try:
                payload = bytes(attr_or_call(pkt, "payload", b""))
            except Exception:
                payload = b""
        return PacketEvent(
            timestamp_ms=int(attr_or_call(pkt, "timestamp_ms", now_ms())),
            sender=str(attr_or_call(pkt, "sender", "?")),
            data_type=str(attr_or_call(pkt, "ty", "?")),
            endpoints=endpoints,
            payload_len=len(payload),
            payload_hex=hex_preview(payload, limit=24),
            payload_text=safe_decode_ascii(payload, limit=40),
        )

    def _on_packet(self, pkt: object) -> None:
        event = self._extract_event(pkt)
        with self.lock:
            self.packet_events.appendleft(event)
            self.packet_counts[(event.sender, event.data_type)] += 1
            self.last_rx_ms = now_ms()
            self.status = f"RX packet {event.sender}:{event.data_type}"

    def _rx_loop(self) -> None:
        while not self.stop_event.is_set():
            try:
                packet = self.transport.read_serialized(timeout=0.1)
                if packet is None:
                    continue
                with self.lock:
                    self.raw_rx_count += 1
                    self.last_rx_ms = now_ms()
                    self.status = f"RX bytes {len(packet)}"
                self.router.receive_serialized_queue_from_side(self.side_id, packet)
                self.router.process_all_queues()
            except Exception as err:
                with self.lock:
                    self.error = str(err)
                    self.status = "RX error"
                time.sleep(0.2)

    def send_command(self, spec: CommandSpec) -> None:
        packet = seds.make_packet(
            ty=spec.data_type,
            sender=self.sender,
            endpoints=[spec.endpoint],
            timestamp_ms=now_ms(),
            payload=bytes([spec.command_id]),
        )
        wire = bytes(packet.serialize())
        self.transport.send_serialized(wire)
        with self.lock:
            self.sent_log.appendleft(f"{spec.label} -> id={spec.command_id} ep={spec.endpoint}")
            self.status = f"Sent {spec.label}"

    def clear_packets(self) -> None:
        with self.lock:
            self.packet_events.clear()
            self.packet_counts.clear()
            self.sent_log.clear()

    def group(self) -> tuple[str, list[CommandSpec]]:
        return self.command_groups[self.group_index]

    def selected(self) -> CommandSpec:
        return self.group()[1][self.command_index]

    def move_group(self, delta: int) -> None:
        self.group_index = (self.group_index + delta) % len(self.command_groups)
        self.command_index = min(self.command_index, len(self.group()[1]) - 1)

    def move_command(self, delta: int) -> None:
        commands = self.group()[1]
        self.command_index = (self.command_index + delta) % len(commands)


def draw_box(stdscr: curses.window, y: int, x: int, h: int, w: int, title: str) -> None:
    max_y, max_x = stdscr.getmaxyx()
    if h < 2 or w < 2 or y < 0 or x < 0 or y >= max_y or x >= max_x:
        return

    right = min(x + w - 1, max_x - 1)
    bottom = min(y + h - 1, max_y - 1)
    inner_w = max(0, right - x - 1)
    inner_h = max(0, bottom - y - 1)

    try:
        if x + 2 <= right:
            stdscr.addnstr(y, x + 2, f" {title} ", max(0, right - (x + 2)), curses.A_BOLD)
        stdscr.hline(y, x + 1, curses.ACS_HLINE, inner_w)
        if bottom > y:
            stdscr.hline(bottom, x + 1, curses.ACS_HLINE, inner_w)
        stdscr.vline(y + 1, x, curses.ACS_VLINE, inner_h)
        if right > x:
            stdscr.vline(y + 1, right, curses.ACS_VLINE, inner_h)
        stdscr.addch(y, x, curses.ACS_ULCORNER)
        if right > x:
            stdscr.addch(y, right, curses.ACS_URCORNER)
        if bottom > y:
            stdscr.addch(bottom, x, curses.ACS_LLCORNER)
        # Avoid writing the terminal's lower-right cell; curses often rejects it.
        if bottom > y and right > x and not (bottom == max_y - 1 and right == max_x - 1):
            stdscr.addch(bottom, right, curses.ACS_LRCORNER)
    except curses.error:
        return


def draw_tui(stdscr: curses.window, app: FillLinkApp) -> None:
    curses.curs_set(0)
    stdscr.nodelay(True)
    stdscr.timeout(100)
    while not app.stop_event.is_set():
        stdscr.erase()
        h, w = stdscr.getmaxyx()
        if h < 20 or w < 90:
            stdscr.addstr(0, 0, "Terminal too small. Need at least 90x20.")
            stdscr.refresh()
            time.sleep(0.1)
            continue

        left_w = int(w * 0.58)
        right_w = w - left_w - 1
        top_h = h - 8
        draw_box(stdscr, 0, 0, top_h, left_w, "Fill Telemetry")
        draw_box(stdscr, 0, left_w, top_h, right_w, "Commands")
        draw_box(stdscr, top_h, 0, h - top_h, w, "Status")

        with app.lock:
            packet_events = list(app.packet_events)
            counts = app.packet_counts.copy()
            sent_log = list(app.sent_log)
            status = app.status
            error = app.error
            raw_rx_count = app.raw_rx_count
            last_rx_ms = app.last_rx_ms
        transport_activity = app.transport.activity_snapshot()

        stdscr.addstr(1, 2, f"Link: {app.transport.describe()}")
        stdscr.addstr(2, 2, "Recent received packets")
        max_packets = top_h - 5
        if packet_events:
            for idx, event in enumerate(packet_events[:max_packets]):
                line = (
                    f"{event.sender:<3} {event.data_type:<18} len={event.payload_len:<3} "
                    f"ep={','.join(event.endpoints[:2]) or '-':<18} "
                    f"{event.payload_hex:<30} {event.payload_text}"
                )
                stdscr.addnstr(3 + idx, 2, line, left_w - 4)
        else:
            raw_lines = ["No decoded packets yet."]
            if transport_activity.get("kind") == "serial":
                raw_lines.append(
                    "Serial RX "
                    f"reads={transport_activity.get('raw_reads', 0)} "
                    f"bytes={transport_activity.get('raw_bytes', 0)} "
                    f"buffered={transport_activity.get('buffered_bytes', 0)}"
                )
                chunk_hex = str(transport_activity.get("last_chunk_hex", ""))
                if chunk_hex:
                    raw_lines.append(f"Last serial chunk: {chunk_hex}")
            elif transport_activity.get("kind") == "i2c":
                raw_lines.append(
                    "I2C RX "
                    f"slots={transport_activity.get('raw_slots', 0)} "
                    f"invalid={transport_activity.get('invalid_slots', 0)} "
                    f"buffered={transport_activity.get('buffered_bytes', 0)}"
                )
                slot_hex = str(transport_activity.get("last_slot_hex", ""))
                if slot_hex:
                    raw_lines.append(f"Last I2C slot: {slot_hex}")
            for idx, line in enumerate(raw_lines[:max_packets]):
                stdscr.addnstr(3 + idx, 2, line, left_w - 4)

        group_name, group_commands = app.group()
        stdscr.addstr(1, left_w + 2, f"Group: {group_name} ({app.group_index + 1}/{len(app.command_groups)})", curses.A_BOLD)
        stdscr.addstr(2, left_w + 2, "Tab/Shift-Tab switch group. Up/Down select. Enter sends.")
        cmd_rows = min(len(group_commands), top_h - 5)
        for idx in range(cmd_rows):
            spec = group_commands[idx]
            attr = curses.A_REVERSE if idx == app.command_index else curses.A_NORMAL
            line = f"{spec.label:<24} id={spec.command_id:<2} {spec.detail}"
            stdscr.addnstr(4 + idx, left_w + 2, line, right_w - 4, attr)

        base_y = top_h + 1
        stdscr.addstr(base_y, 2, f"Status: {status}")
        if error:
            stdscr.addnstr(base_y + 1, 2, f"Error: {error}", w - 4, curses.A_BOLD)
        status_lines = [
            f"Selected: {app.selected().label} ({app.selected().detail})",
        ]
        rx_line = f"RX seen={raw_rx_count}"
        if last_rx_ms is not None:
            rx_line += f" last_ms={last_rx_ms}"
        status_lines.append(rx_line)

        transport_line = "Transport RX: "
        if transport_activity.get("kind") == "i2c":
            transport_line += (
                f"slots={transport_activity.get('raw_slots', 0)} "
                f"invalid={transport_activity.get('invalid_slots', 0)} "
                f"buffered={transport_activity.get('buffered_bytes', 0)}"
            )
        elif transport_activity.get("kind") == "serial":
            transport_line += (
                f"reads={transport_activity.get('raw_reads', 0)} "
                f"bytes={transport_activity.get('raw_bytes', 0)} "
                f"buffered={transport_activity.get('buffered_bytes', 0)}"
            )
        else:
            transport_line += "n/a"
        status_lines.append(transport_line)
        if transport_activity.get("kind") == "i2c":
            status_lines.append(
                "Transfers: "
                f"starts={transport_activity.get('transfer_starts', 0)} "
                f"done={transport_activity.get('transfer_completes', 0)} "
                f"resets={transport_activity.get('transfer_resets', 0)} "
                f"last_len={transport_activity.get('last_transfer_len', 0)}"
            )
        tx_line = (
            f"Transport TX: writes={transport_activity.get('tx_writes', 0)} "
            f"bytes={transport_activity.get('tx_bytes', 0)}"
        )
        status_lines.append(tx_line)
        status_lines.append(
            "Counts: " + ", ".join(f"{sender}:{dtype}={count}" for (sender, dtype), count in counts.most_common(4))
        )

        raw_preview = str(transport_activity.get("last_slot_hex", "") or transport_activity.get("last_chunk_hex", ""))
        if raw_preview:
            status_lines.append(f"Last RX raw: {raw_preview}")
        transfer_preview = str(transport_activity.get("last_transfer_hex", ""))
        if transfer_preview:
            status_lines.append(f"Last transfer: {transfer_preview}")
        tx_preview = str(transport_activity.get("last_tx_hex", ""))
        if tx_preview:
            status_lines.append(f"Last TX raw: {tx_preview}")
        for line in sent_log[:2]:
            status_lines.append(f"Sent: {line}")

        status_row = base_y + 2
        max_status_row = h - 2
        for line in status_lines:
            if status_row > max_status_row:
                break
            stdscr.addnstr(status_row, 2, line, w - 4)
            status_row += 1
        stdscr.addnstr(h - 1, 2, "q quit | c clear | Enter send", w - 4)
        stdscr.refresh()

        key = stdscr.getch()
        if key == -1:
            continue
        if key in {ord("q"), ord("Q")}:
            app.stop_event.set()
            return
        if key in {ord("c"), ord("C")}:
            app.clear_packets()
            continue
        if key in {curses.KEY_UP, ord("k")}:
            app.move_command(-1)
            continue
        if key in {curses.KEY_DOWN, ord("j")}:
            app.move_command(1)
            continue
        if key == 9:
            app.move_group(1)
            continue
        if key == curses.KEY_BTAB:
            app.move_group(-1)
            continue
        if key in {10, 13, curses.KEY_ENTER}:
            try:
                app.send_command(app.selected())
            except Exception as err:
                with app.lock:
                    app.error = str(err)
                    app.status = "TX error"


def build_transport(cfg: dict) -> Transport:
    if cfg["interface"] == "i2c":
        return I2cTransport(cfg)
    return SerialTransport(cfg)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Standalone fill-system TUI")
    parser.add_argument(
        "--sender",
        default="GS",
        help="Sender ID used when emitting packets (default: GS)",
    )
    parser.add_argument(
        "--include-hitl",
        action="store_true",
        help="Include HITL flight-computer commands in the rocket command list.",
    )
    parser.add_argument(
        "--config",
        help="Override comms config path. Defaults to backend behavior.",
    )
    parser.add_argument(
        "--interface",
        choices=["serial", "raspberry_pi_gpio_uart", "custom_serial", "i2c"],
        help="Override transport interface instead of using fill_box.interface from config.",
    )
    parser.add_argument(
        "--port",
        help="Override serial port, or use /dev/i2c-N with --interface i2c.",
    )
    parser.add_argument(
        "--baud-rate",
        type=int,
        help="Override serial baud rate.",
    )
    parser.add_argument(
        "--protocol",
        choices=["raw_uart", "packet_framed"],
        help="Override both serial TX and RX framing protocol.",
    )
    parser.add_argument(
        "--tx-protocol",
        choices=["raw_uart", "packet_framed"],
        help="Override serial transmit framing only.",
    )
    parser.add_argument(
        "--rx-protocol",
        choices=["raw_uart", "packet_framed"],
        help="Override serial receive framing only.",
    )
    parser.add_argument(
        "--bus",
        type=int,
        help="Override I2C bus number.",
    )
    parser.add_argument(
        "--addr",
        type=lambda value: int(value, 0),
        help="Override I2C address, e.g. 0x55.",
    )
    parser.add_argument(
        "--chunk-delay-ms",
        type=int,
        help="Override I2C inter-slot delay in milliseconds.",
    )
    parser.add_argument(
        "--initial-wait-ms",
        type=int,
        help="Override I2C post-send wait in milliseconds.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    backend_root = backend_root_from_script(Path(__file__))
    cfg = resolve_fill_link_config(args, backend_root)
    transport = build_transport(cfg)
    app = FillLinkApp(transport, sender=args.sender, commands=build_command_groups(args.include_hitl))
    app.start()
    try:
        curses.wrapper(draw_tui, app)
    finally:
        app.stop()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
