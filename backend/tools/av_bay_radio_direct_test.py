#!/usr/bin/env python3

"""Interactive AV-bay radio direct test TUI."""

from __future__ import annotations

import argparse
import binascii
import curses
import fcntl
import json
import os
import select
import sys
import termios
import threading
import time
from collections import Counter, deque
from dataclasses import dataclass
from pathlib import Path

RAW_DATA_SYNC = (0xA5, 0x5A)
RAW_COMMAND_SYNC = (0xA6, 0x5B)
RAW_ASCII_SYNC = (0xA7, 0x7A)
RAW_HEADER_SIZE = 4
RADIO_SCHED_MAGIC = (0x52, 0x53)
RADIO_SCHED_VERSION = 1
RADIO_SCHED_FLAG_HAS_MORE = 0x01
RADIO_SCHED_FLAG_YIELD = 0x02

FLIGHT_COMMANDS = {
    "Launch": 1,
    "MonitorAltitude": 2,
    "RevokeMonitorAltitude": 3,
    "ConsecutiveSamples": 4,
    "RevokeConsecutiveSamples": 5,
    "ResetFailures": 6,
    "RevokeResetFailures": 7,
    "ValidateMeasms": 8,
    "RevokeValidateMeasms": 9,
    "DeployParachute": 12,
    "ExpandParachute": 13,
    "EvaluationRelax": 14,
    "EvaluationFocus": 15,
    "EvaluationAbort": 16,
    "ReinitSensors": 17,
    "ReinitBarometer": 18,
    "EnableIMU": 19,
    "DisableIMU": 20,
    "AbortAfter40": 23,
}


def import_seds() -> object:
    try:
        import sedsprintf_rs_2026 as seds

        return seds
    except ModuleNotFoundError:
        try:
            import sedsprintf_rs as seds

            return seds
        except ModuleNotFoundError as e:
            raise SystemExit(
                "Missing dependency 'sedsprintf_rs_2026' or 'sedsprintf_rs'. Build/install the Python module first."
            ) from e


def enum_value(enum_cls: object, *names: str) -> int:
    for name in names:
        if hasattr(enum_cls, name):
            return int(getattr(enum_cls, name))
    available = [name for name in dir(enum_cls) if not name.startswith("_")]
    raise AttributeError(f"{enum_cls.__class__.__name__} missing {names}. Available: {available}")


@dataclass(frozen=True)
class CommandSpec:
    label: str
    kind: str
    data_type: int | None
    endpoint: int | None
    command_id: int | None
    detail: str


@dataclass
class RadioWindow:
    kind: str
    seq: int
    credit: int
    flags: int


@dataclass
class RxEvent:
    timestamp_ms: int
    kind: str
    summary: str
    payload_len: int
    payload_hex: str


@dataclass
class TxEvent:
    timestamp_ms: int
    label: str
    mode: str
    packet_len: int
    frame_len: int
    seq: int | None


def backend_root_from_script(script_path: Path) -> Path:
    return script_path.resolve().parents[2]


def comms_config_path(backend_root: Path, explicit: str | None) -> Path:
    if explicit:
        return Path(explicit)
    env_path = os.environ.get("GS_COMMS_LINK_CONFIG") or os.environ.get("GS_RADIO_LINK_CONFIG")
    if env_path:
        return Path(env_path)
    return backend_root / "backend" / "comms" / "comms.json"


def load_av_bay_serial_config(path: Path) -> dict[str, object]:
    with path.open("r", encoding="utf-8") as fh:
        raw = json.load(fh)
    av_bay = raw.get("av_bay") or {}
    interface = av_bay.get("interface")
    if interface not in {"serial", "raspberry_pi_gpio_uart", "custom_serial"}:
        raise SystemExit(f"av_bay must be a serial-style radio link, got interface={interface!r}")
    protocol = av_bay.get("protocol", "packet_framed")
    if protocol != "raw_uart":
        raise SystemExit(f"av_bay protocol must be raw_uart for the radio driver, got {protocol!r}")
    return {
        "port": av_bay.get("port", "/dev/ttyAMA0"),
        "baud_rate": int(av_bay.get("baud_rate", 9600)),
    }


def configure_raw_serial(fd: int, baud: int) -> None:
    attrs = termios.tcgetattr(fd)
    attrs[0] = 0
    attrs[1] = 0
    attrs[2] = attrs[2] & ~(termios.PARENB | termios.CSTOPB | termios.CSIZE | termios.CRTSCTS)
    attrs[2] |= termios.CS8 | termios.CREAD | termios.CLOCAL
    attrs[3] = 0
    attrs[6][termios.VMIN] = 0
    attrs[6][termios.VTIME] = 0

    baud_attr = {
        9600: termios.B9600,
        19200: termios.B19200,
        38400: termios.B38400,
        57600: termios.B57600,
        115200: termios.B115200,
    }.get(baud)
    if baud_attr is None:
        raise ValueError(f"unsupported baud rate for termios: {baud}")

    attrs[4] = baud_attr
    attrs[5] = baud_attr
    termios.tcsetattr(fd, termios.TCSANOW, attrs)


def set_nonblocking(fd: int) -> None:
    flags = fcntl.fcntl(fd, fcntl.F_GETFL)
    fcntl.fcntl(fd, fcntl.F_SETFL, flags | os.O_NONBLOCK)


def now_ms() -> int:
    return int(time.time() * 1000)


def hex_preview(data: bytes, limit: int = 48) -> str:
    preview = data[:limit].hex(" ")
    return preview + (" ..." if len(data) > limit else "")


def parse_hex(raw: str) -> bytes:
    cleaned = raw.replace(" ", "").replace(":", "").replace("_", "")
    if len(cleaned) % 2 != 0:
        raise ValueError("hex payload must have an even number of nybbles")
    return binascii.unhexlify(cleaned)


def build_raw_frame(sync: tuple[int, int], payload: bytes) -> bytes:
    if not payload:
        raise ValueError("raw UART payload must not be empty")
    if len(payload) > 0xFFFF:
        raise ValueError("raw UART payload is too large")
    return bytes(sync) + len(payload).to_bytes(2, "little") + payload


def extract_raw_frames(buffer: bytearray) -> list[tuple[str, bytes]]:
    frames: list[tuple[str, bytes]] = []
    sync_to_kind = {
        RAW_DATA_SYNC: "data",
        RAW_COMMAND_SYNC: "command",
        RAW_ASCII_SYNC: "ascii",
    }
    while True:
        sync_pos = -1
        for idx in range(max(0, len(buffer) - 1)):
            pair = (buffer[idx], buffer[idx + 1])
            if pair in sync_to_kind:
                sync_pos = idx
                break
        if sync_pos < 0:
            keep = (
                1
                if buffer
                and buffer[-1] in {RAW_DATA_SYNC[0], RAW_COMMAND_SYNC[0], RAW_ASCII_SYNC[0]}
                else 0
            )
            if keep:
                del buffer[:-1]
            else:
                buffer.clear()
            break
        if sync_pos > 0:
            del buffer[:sync_pos]
        if len(buffer) < RAW_HEADER_SIZE:
            break
        payload_len = int.from_bytes(buffer[2:4], "little")
        total_len = RAW_HEADER_SIZE + payload_len
        if payload_len == 0:
            del buffer[:RAW_HEADER_SIZE]
            continue
        if len(buffer) < total_len:
            break
        kind = sync_to_kind[(buffer[0], buffer[1])]
        frames.append((kind, bytes(buffer[RAW_HEADER_SIZE:total_len])))
        del buffer[:total_len]
    return frames


def parse_radio_window(payload: bytes) -> RadioWindow | None:
    if (
        len(payload) < 7
        or payload[0] != RADIO_SCHED_MAGIC[0]
        or payload[1] != RADIO_SCHED_MAGIC[1]
        or payload[2] != RADIO_SCHED_VERSION
    ):
        return None
    if payload[3] == 0:
        kind = "downlink"
    elif payload[3] == 1:
        kind = "uplink"
    else:
        return None
    return RadioWindow(kind=kind, seq=payload[4], credit=max(payload[5], 1), flags=payload[6])


def build_scheduler_yield(seq: int, has_more: bool) -> bytes:
    flags = RADIO_SCHED_FLAG_YIELD | (RADIO_SCHED_FLAG_HAS_MORE if has_more else 0)
    payload = bytes((RADIO_SCHED_MAGIC[0], RADIO_SCHED_MAGIC[1], RADIO_SCHED_VERSION, 1, seq, 0, flags))
    return build_raw_frame(RAW_COMMAND_SYNC, payload)


class PacketBuilder:
    def __init__(self, sender: str) -> None:
        self.seds = import_seds()
        self.sender = sender
        self.flight_controller_endpoint = enum_value(
            self.seds.DataEndpoint, "FLIGHT_CONTROLLER", "FlightController"
        )
        self.heartbeat_type = enum_value(self.seds.DataType, "HEARTBEAT", "Heartbeat")
        self.flight_command_type = enum_value(self.seds.DataType, "FLIGHT_COMMAND", "FlightCommand")

    def specs(self) -> list[tuple[str, list[CommandSpec]]]:
        return [
            (
                "Radio",
                [
                    CommandSpec(
                        "Heartbeat",
                        "heartbeat",
                        self.heartbeat_type,
                        self.flight_controller_endpoint,
                        None,
                        "HEARTBEAT to FlightController",
                    ),
                ],
            ),
            (
                "Flight Commands",
                [
                    CommandSpec(
                        label,
                        "flight-command",
                        self.flight_command_type,
                        self.flight_controller_endpoint,
                        command_id,
                        f"FLIGHT_COMMAND id={command_id}",
                    )
                    for label, command_id in FLIGHT_COMMANDS.items()
                ],
            ),
        ]

    def build(self, spec: CommandSpec) -> bytes:
        if spec.kind == "heartbeat":
            payload = b""
            ty = self.heartbeat_type
        elif spec.kind == "flight-command":
            if spec.command_id is None:
                raise ValueError("flight command spec missing command_id")
            payload = bytes((spec.command_id,))
            ty = self.flight_command_type
        else:
            raise ValueError(f"unsupported command kind: {spec.kind}")
        packet = self.seds.make_packet(
            ty=ty,
            sender=self.sender,
            endpoints=[self.flight_controller_endpoint],
            timestamp_ms=now_ms(),
            payload=payload,
        )
        return bytes(packet.serialize())


class AvBayRadioApp:
    def __init__(
        self,
        fd: int,
        port: str,
        baud: int,
        builder: PacketBuilder,
        no_yield: bool,
        preview_bytes: int,
    ) -> None:
        self.fd = fd
        self.port = port
        self.baud = baud
        self.builder = builder
        self.no_yield = no_yield
        self.preview_bytes = preview_bytes
        self.command_groups = builder.specs()
        self.group_index = 0
        self.command_index = 0
        self.lock = threading.Lock()
        self.io_lock = threading.Lock()
        self.stop_event = threading.Event()
        self.rx_thread = threading.Thread(target=self._rx_loop, daemon=True)
        self.rx_buffer = bytearray()
        self.pending: deque[tuple[str, bytes, bytes]] = deque()
        self.rx_events: deque[RxEvent] = deque(maxlen=200)
        self.tx_events: deque[TxEvent] = deque(maxlen=100)
        self.packet_counts: Counter[str] = Counter()
        self.status = "Connected"
        self.error: str | None = None
        self.raw_rx_bytes = 0
        self.raw_rx_frames = 0
        self.radio_windows = 0
        self.sent_packets = 0
        self.sent_yields = 0
        self.last_window: RadioWindow | None = None

    def start(self) -> None:
        self.rx_thread.start()

    def stop(self) -> None:
        self.stop_event.set()
        self.rx_thread.join(timeout=1.0)

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

    def clear(self) -> None:
        with self.lock:
            self.rx_events.clear()
            self.tx_events.clear()
            self.packet_counts.clear()
            self.error = None
            self.status = "Cleared"

    def queue_selected(self) -> None:
        spec = self.selected()
        packet = self.builder.build(spec)
        frame = build_raw_frame(RAW_DATA_SYNC, packet)
        with self.lock:
            self.pending.append((spec.label, packet, frame))
            self.status = f"Queued {spec.label}"

    def force_send_selected(self) -> None:
        spec = self.selected()
        packet = self.builder.build(spec)
        frame = build_raw_frame(RAW_DATA_SYNC, packet)
        self._write_frame(frame)
        with self.lock:
            self._record_tx(spec.label, "forced", len(packet), len(frame), None)
            self.status = f"Forced TX {spec.label}"

    def _write_frame(self, frame: bytes) -> None:
        with self.io_lock:
            total = 0
            while total < len(frame) and not self.stop_event.is_set():
                _, writable, _ = select.select([], [self.fd], [], 0.25)
                if not writable:
                    continue
                total += os.write(self.fd, frame[total:])
            termios.tcdrain(self.fd)

    def _record_tx(
        self,
        label: str,
        mode: str,
        packet_len: int,
        frame_len: int,
        seq: int | None,
    ) -> None:
        self.sent_packets += 1
        self.tx_events.appendleft(TxEvent(now_ms(), label, mode, packet_len, frame_len, seq))

    def _record_rx(self, kind: str, summary: str, payload: bytes) -> None:
        self.raw_rx_frames += 1
        self.packet_counts[kind] += 1
        self.rx_events.appendleft(
            RxEvent(now_ms(), kind, summary, len(payload), hex_preview(payload, self.preview_bytes))
        )

    def _send_pending_for_window(self, window: RadioWindow) -> None:
        sent_this_window = 0
        while sent_this_window < window.credit:
            with self.lock:
                if not self.pending:
                    break
                label, packet, frame = self.pending.popleft()
            self._write_frame(frame)
            sent_this_window += 1
            with self.lock:
                self._record_tx(label, "uplink", len(packet), len(frame), window.seq)
                self.status = f"TX {label} during uplink seq={window.seq}"
        if not self.no_yield:
            with self.lock:
                has_more = bool(self.pending)
            self._write_frame(build_scheduler_yield(window.seq, has_more))
            with self.lock:
                self.sent_yields += 1
                self.tx_events.appendleft(
                    TxEvent(now_ms(), "SchedulerYield", "command", 7, RAW_HEADER_SIZE + 7, window.seq)
                )

    def _rx_loop(self) -> None:
        while not self.stop_event.is_set():
            try:
                readable, _, _ = select.select([self.fd], [], [], 0.05)
                if not readable:
                    continue
                while True:
                    try:
                        chunk = os.read(self.fd, 512)
                    except BlockingIOError:
                        break
                    if not chunk:
                        break
                    with self.lock:
                        self.raw_rx_bytes += len(chunk)
                    self.rx_buffer.extend(chunk)
                    frames = extract_raw_frames(self.rx_buffer)
                    for kind, payload in frames:
                        if kind == "data":
                            with self.lock:
                                self._record_rx(kind, "data frame", payload)
                                self.status = f"RX data frame len={len(payload)}"
                            continue
                        if kind == "ascii":
                            text = payload.decode("utf-8", errors="replace").strip()
                            with self.lock:
                                self._record_rx(kind, f"ascii {text!r}", payload)
                                self.status = "RX ascii frame"
                            continue
                        window = parse_radio_window(payload)
                        if window is None:
                            with self.lock:
                                self._record_rx(kind, "command frame", payload)
                                self.status = "RX command frame"
                            continue
                        with self.lock:
                            self.radio_windows += 1
                            self.last_window = window
                            self._record_rx(
                                "window",
                                f"{window.kind} seq={window.seq} credit={window.credit} flags=0x{window.flags:02x}",
                                payload,
                            )
                            self.status = f"RX {window.kind} window seq={window.seq}"
                        if window.kind == "uplink":
                            self._send_pending_for_window(window)
            except Exception as err:
                with self.lock:
                    self.error = str(err)
                    self.status = "RX error"
                time.sleep(0.2)


def safe_addnstr(stdscr: curses.window, y: int, x: int, text: str, n: int, attr: int = 0) -> None:
    try:
        stdscr.addnstr(y, x, text, max(0, n), attr)
    except curses.error:
        pass


def draw_box(stdscr: curses.window, y: int, x: int, h: int, w: int, title: str) -> None:
    max_y, max_x = stdscr.getmaxyx()
    if h < 2 or w < 4 or y >= max_y or x >= max_x:
        return
    right = min(x + w - 1, max_x - 1)
    bottom = min(y + h - 1, max_y - 1)
    inner_w = max(0, right - x - 1)
    inner_h = max(0, bottom - y - 1)
    try:
        stdscr.hline(y, x + 1, curses.ACS_HLINE, inner_w)
        stdscr.hline(bottom, x + 1, curses.ACS_HLINE, inner_w)
        stdscr.vline(y + 1, x, curses.ACS_VLINE, inner_h)
        stdscr.vline(y + 1, right, curses.ACS_VLINE, inner_h)
        stdscr.addch(y, x, curses.ACS_ULCORNER)
        stdscr.addch(y, right, curses.ACS_URCORNER)
        stdscr.addch(bottom, x, curses.ACS_LLCORNER)
        if not (bottom == max_y - 1 and right == max_x - 1):
            stdscr.addch(bottom, right, curses.ACS_LRCORNER)
        safe_addnstr(stdscr, y, x + 2, f" {title} ", max(0, right - x - 3), curses.A_BOLD)
    except curses.error:
        return


def draw_tui(stdscr: curses.window, app: AvBayRadioApp) -> None:
    curses.curs_set(0)
    stdscr.nodelay(True)
    stdscr.timeout(100)
    while not app.stop_event.is_set():
        stdscr.erase()
        h, w = stdscr.getmaxyx()
        if h < 20 or w < 110:
            safe_addnstr(stdscr, 0, 0, "Terminal too small. Need at least 110x20.", w - 1)
            stdscr.refresh()
            time.sleep(0.1)
            continue

        left_w = max(42, int(w * 0.43))
        tx_w = max(30, int(w * 0.25))
        cmd_w = w - left_w - tx_w - 2
        cmd_x = left_w
        tx_x = left_w + cmd_w + 1
        top_h = h - 8
        draw_box(stdscr, 0, 0, top_h, left_w, "AV Bay Radio")
        draw_box(stdscr, 0, cmd_x, top_h, cmd_w, "Commands")
        draw_box(stdscr, 0, tx_x, top_h, tx_w, "TX")
        draw_box(stdscr, top_h, 0, h - top_h, w, "Status")

        with app.lock:
            rx_events = list(app.rx_events)
            tx_events = list(app.tx_events)
            counts = app.packet_counts.copy()
            pending_len = len(app.pending)
            status = app.status
            error = app.error
            raw_rx_bytes = app.raw_rx_bytes
            raw_rx_frames = app.raw_rx_frames
            radio_windows = app.radio_windows
            sent_packets = app.sent_packets
            sent_yields = app.sent_yields
            last_window = app.last_window

        safe_addnstr(stdscr, 1, 2, f"Link: {app.port} @ {app.baud} raw_uart", left_w - 4)
        safe_addnstr(stdscr, 2, 2, "Recent RX", left_w - 4, curses.A_BOLD)
        if rx_events:
            for idx, event in enumerate(rx_events[: max(0, top_h - 4)]):
                line = (
                    f"{event.kind:<7} len={event.payload_len:<4} "
                    f"{event.summary:<38} {event.payload_hex}"
                )
                safe_addnstr(stdscr, 3 + idx, 2, line, left_w - 4)
        else:
            safe_addnstr(stdscr, 3, 2, "No RX frames yet.", left_w - 4)

        group_name, group_commands = app.group()
        safe_addnstr(
            stdscr,
            1,
            cmd_x + 2,
            f"Group: {group_name} ({app.group_index + 1}/{len(app.command_groups)})",
            cmd_w - 4,
            curses.A_BOLD,
        )
        safe_addnstr(stdscr, 2, cmd_x + 2, "Tab group. Up/Down select. Enter queue. f force.", cmd_w - 4)
        for idx, spec in enumerate(group_commands[: max(0, top_h - 5)]):
            attr = curses.A_REVERSE if idx == app.command_index else curses.A_NORMAL
            cmd_id = "-" if spec.command_id is None else str(spec.command_id)
            line = f"{spec.label:<24} id={cmd_id:<3} {spec.detail}"
            safe_addnstr(stdscr, 4 + idx, cmd_x + 2, line, cmd_w - 4, attr)

        if tx_events:
            for idx, event in enumerate(tx_events[: max(0, top_h - 3)]):
                seq = "-" if event.seq is None else str(event.seq)
                line = (
                    f"{event.label[:15]:<15} {event.mode:<8} pkt={event.packet_len:<4} "
                    f"wire={event.frame_len:<4} seq={seq:<3}"
                )
                safe_addnstr(stdscr, 1 + idx, tx_x + 2, line, tx_w - 4)
        else:
            safe_addnstr(stdscr, 1, tx_x + 2, "No TX yet.", tx_w - 4)

        base_y = top_h + 1
        safe_addnstr(stdscr, base_y, 2, f"Status: {status}", w - 4)
        if error:
            safe_addnstr(stdscr, base_y + 1, 2, f"Error: {error}", w - 4, curses.A_BOLD)
        selected = app.selected()
        status_lines = [
            f"Selected: {selected.label} ({selected.detail})",
            f"Pending={pending_len} TX packets={sent_packets} yields={sent_yields}",
            f"RX bytes={raw_rx_bytes} frames={raw_rx_frames} windows={radio_windows}",
        ]
        if last_window is not None:
            status_lines.append(
                f"Last window: {last_window.kind} seq={last_window.seq} credit={last_window.credit} flags=0x{last_window.flags:02x}"
            )
        if counts:
            status_lines.append(
                "Counts: " + ", ".join(f"{kind}={count}" for kind, count in counts.most_common(5))
            )
        row = base_y + 2
        for line in status_lines:
            if row >= h - 1:
                break
            safe_addnstr(stdscr, row, 2, line, w - 4)
            row += 1
        safe_addnstr(stdscr, h - 1, 2, "q quit | c clear | Enter queue | f force send now | Tab group", w - 4)
        stdscr.refresh()

        key = stdscr.getch()
        if key == -1:
            continue
        if key in {ord("q"), ord("Q")}:
            app.stop_event.set()
            return
        if key in {ord("c"), ord("C")}:
            app.clear()
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
        if key in {ord("f"), ord("F")}:
            try:
                app.force_send_selected()
            except Exception as err:
                with app.lock:
                    app.error = str(err)
                    app.status = "Forced TX error"
            continue
        if key in {10, 13, curses.KEY_ENTER}:
            try:
                app.queue_selected()
            except Exception as err:
                with app.lock:
                    app.error = str(err)
                    app.status = "Queue error"


def build_packet_from_args(args: argparse.Namespace) -> tuple[str, bytes]:
    if args.packet_hex:
        return "raw packet hex", parse_hex(args.packet_hex)
    builder = PacketBuilder(args.sender)
    if args.kind == "heartbeat":
        spec = builder.specs()[0][1][0]
    else:
        spec = next(item for item in builder.specs()[1][1] if item.label == args.flight_command)
    return spec.label, builder.build(spec)


def write_frame(fd: int, frame: bytes) -> None:
    total = 0
    while total < len(frame):
        _, writable, _ = select.select([], [fd], [], 0.25)
        if not writable:
            continue
        total += os.write(fd, frame[total:])
    termios.tcdrain(fd)


def read_available(fd: int, rx_buffer: bytearray, read_chunk: int) -> list[tuple[str, bytes]]:
    frames: list[tuple[str, bytes]] = []
    while True:
        readable, _, _ = select.select([fd], [], [], 0)
        if not readable:
            break
        try:
            chunk = os.read(fd, read_chunk)
        except BlockingIOError:
            break
        if not chunk:
            break
        rx_buffer.extend(chunk)
        frames.extend(extract_raw_frames(rx_buffer))
    return frames


def run_once(args: argparse.Namespace, port: str, baud: int) -> int:
    label, packet = build_packet_from_args(args)
    data_frame = build_raw_frame(RAW_DATA_SYNC, packet)
    fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        configure_raw_serial(fd, baud)
        set_nonblocking(fd)
        print(f"opened av_bay radio {port} @ {baud}")
        print(f"packet: {label}; serialized={len(packet)} bytes; frame={len(data_frame)} bytes")
        print(f"packet preview: {hex_preview(packet, args.preview_bytes)}")

        rx_buffer = bytearray()
        sent = 0
        windows = 0
        rx_data_frames = 0
        deadline = time.monotonic() + args.duration

        if args.send_without_window:
            for idx in range(args.count):
                write_frame(fd, data_frame)
                sent += 1
                print(f"tx #{idx + 1}: sent without scheduler window")
                time.sleep(0.05)

        while time.monotonic() < deadline and sent < args.count:
            readable, _, _ = select.select([fd], [], [], 0.05)
            if not readable:
                continue
            for kind, payload in read_available(fd, rx_buffer, args.read_chunk):
                if kind == "data":
                    rx_data_frames += 1
                    print(f"rx data frame len={len(payload)} preview={hex_preview(payload, args.preview_bytes)}")
                    continue
                if kind == "ascii":
                    text = payload.decode("utf-8", errors="replace").strip()
                    print(f"rx ascii frame len={len(payload)} text={text!r}")
                    continue
                window = parse_radio_window(payload)
                if window is None:
                    print(f"rx command frame len={len(payload)} preview={hex_preview(payload, args.preview_bytes)}")
                    continue
                windows += 1
                print(
                    f"rx radio window kind={window.kind} seq={window.seq} "
                    f"credit={window.credit} flags=0x{window.flags:02x}"
                )
                if window.kind != "uplink":
                    continue
                sends_this_window = min(window.credit, args.count - sent)
                for _ in range(sends_this_window):
                    write_frame(fd, data_frame)
                    sent += 1
                    print(f"tx #{sent}: sent during uplink seq={window.seq}")
                if not args.no_yield:
                    write_frame(fd, build_scheduler_yield(window.seq, sent < args.count))
                    print(f"tx scheduler yield seq={window.seq} has_more={sent < args.count}")

        print(f"done sent={sent}/{args.count} radio_windows={windows} rx_data_frames={rx_data_frames}")
        if sent < args.count:
            print("timed out before enough uplink windows arrived", file=sys.stderr)
            return 2
        return 0
    finally:
        os.close(fd)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Interactive AV-bay raw UART radio direct test."
    )
    parser.add_argument("--config", help="Override comms config path.")
    parser.add_argument("--port", help="Override av_bay serial port.")
    parser.add_argument("--baud", type=int, help="Override av_bay baud rate.")
    parser.add_argument("--sender", default="GS")
    parser.add_argument("--preview-bytes", type=int, default=64)
    parser.add_argument("--no-yield", action="store_true")
    parser.add_argument("--once", action="store_true", help="Run one scripted send instead of the TUI.")
    parser.add_argument("--kind", choices=("heartbeat", "flight-command"), default="heartbeat")
    parser.add_argument("--flight-command", choices=sorted(FLIGHT_COMMANDS), default="MonitorAltitude")
    parser.add_argument("--packet-hex", help="Serialized SEDS packet hex for --once mode.")
    parser.add_argument("--count", type=int, default=1)
    parser.add_argument("--duration", type=float, default=15.0)
    parser.add_argument("--read-chunk", type=int, default=512)
    parser.add_argument(
        "--send-without-window",
        action="store_true",
        help="In --once mode, transmit immediately instead of waiting for an uplink window.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    backend_root = backend_root_from_script(Path(__file__))
    cfg = load_av_bay_serial_config(comms_config_path(backend_root, args.config))
    port = args.port or str(cfg["port"])
    baud = args.baud or int(cfg["baud_rate"])

    if args.once:
        return run_once(args, port, baud)

    builder = PacketBuilder(args.sender)
    fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        configure_raw_serial(fd, baud)
        set_nonblocking(fd)
        app = AvBayRadioApp(fd, port, baud, builder, args.no_yield, args.preview_bytes)
        app.start()
        try:
            curses.wrapper(draw_tui, app)
        finally:
            app.stop()
        return 0
    finally:
        os.close(fd)


if __name__ == "__main__":
    raise SystemExit(main())
