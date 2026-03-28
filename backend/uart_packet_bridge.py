#!/usr/bin/env python3

"""Bind to a UART, send telemetry packets, and print received packets."""

import argparse
import select
import struct
import sys
import termios
import threading
import time
import tty

try:
    import serial
except ModuleNotFoundError as e:
    raise SystemExit(
        "Missing dependency 'pyserial'. Install it with `python -m pip install pyserial` and retry."
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


def _enum_value(enum_cls, *names: str) -> int:
    for name in names:
        if hasattr(enum_cls, name):
            return int(getattr(enum_cls, name))
    available = [name for name in dir(enum_cls) if not name.startswith("_")]
    raise AttributeError(
        f"{enum_cls.__name__} is missing expected names {names}. Available names: {available}"
    )


GPS_TYPE = _enum_value(DT, "GPS_DATA", "GpsData")
MESSAGE_TYPE = _enum_value(DT, "MESSAGE_DATA", "MessageData", "GENERIC_ERROR", "GenericError")
DEFAULT_ENDPOINT = _enum_value(
    EP,
    "GROUND_STATION",
    "GroundStation",
    "SD_CARD",
    "SdCard",
)

BASE_LAT = 31.7619
BASE_LON = -106.4850


def _now_ms() -> int:
    return int(time.time() * 1000)


def _hex(data: bytes) -> str:
    return " ".join(f"{byte:02x}" for byte in data)


class UartPacketBridge:
    def __init__(self, port: str, baud: int, sender: str, interval: float, startup_delay: float) -> None:
        self.port = port
        self.baud = baud
        self.sender = sender
        self.interval = interval
        self.startup_delay = startup_delay
        self.stop_event = threading.Event()
        self.serial_lock = threading.Lock()
        self.rx_buffer = bytearray()
        self.gps_index = 0
        self.send_warning_next = False

        self.ser = serial.Serial(
            port=port,
            baudrate=baud,
            timeout=0.10,
            inter_byte_timeout=0.02,
            write_timeout=1.0,
        )

    def _write_packet(self, packet: seds.Packet) -> None:
        wire = bytes(packet.serialize())
        # print(wire)
        with self.serial_lock:
            self.ser.write(wire)
            self.ser.flush()

    def _build_gps_packet(self) -> seds.Packet:
        offset = self.gps_index * 0.0001
        payload = struct.pack(
            "<fff",
            BASE_LAT + offset,
            BASE_LON + (offset * 0.8),
            30.0 + self.gps_index,
        )
        self.gps_index += 1
        return seds.make_packet(
            ty=GPS_TYPE,
            sender=self.sender,
            endpoints=[DEFAULT_ENDPOINT],
            timestamp_ms=_now_ms(),
            payload=payload,
        )

    def _build_warning_packet(self) -> seds.Packet:
        warning = f"WARNING: UART link check #{self.gps_index}".encode("utf-8")
        return seds.make_packet(
            ty=MESSAGE_TYPE,
            sender=self.sender,
            endpoints=[DEFAULT_ENDPOINT],
            timestamp_ms=_now_ms(),
            payload=warning,
        )

    def _send_once(self) -> None:
        # The wire format has no explicit frame length prefix, so this example
        # sends one frame per burst and relies on UART idle gaps for framing.
        if self.send_warning_next:
            packet = self._build_warning_packet()
        else:
            packet = self._build_gps_packet()
        self.send_warning_next = not self.send_warning_next
        self._write_packet(packet)

    def send_warning_now(self) -> None:
        packet = self._build_warning_packet()
        self._write_packet(packet)

    def send_gps_now(self) -> None:
        packet = self._build_gps_packet()
        self._write_packet(packet)

    def _drain_rx_buffer(self) -> None:
        try:
            pkt = seds.deserialize_packet_py(bytes(self.rx_buffer))
        except Exception:
            return

        frame = bytes(pkt.serialize())
        self.rx_buffer.clear()
        print(f"[RX Wire] {len(frame)} bytes: {_hex(frame)}")
        print("[RX Packet]")
        print(pkt)

    def rx_loop(self) -> None:
        while not self.stop_event.is_set():
            try:
                chunk = self.ser.read(4096)
            except serial.SerialException as e:
                print(f"UART read failed: {e}", file=sys.stderr)
                self.stop_event.set()
                return

            if not chunk:
                continue

            self.rx_buffer.extend(chunk)
            self._drain_rx_buffer()

    def tx_loop(self) -> None:
        while not self.stop_event.is_set():
            try:
                self._send_once()
            except Exception as e:
                print(f"Packet send failed: {e}", file=sys.stderr)
                self.stop_event.set()
                return
            self.stop_event.wait(self.interval)

    def key_loop(self) -> None:
        fd = sys.stdin.fileno()
        old = termios.tcgetattr(fd)
        try:
            tty.setcbreak(fd)
            while not self.stop_event.is_set():
                ready, _, _ = select.select([fd], [], [], 0.1)
                if not ready:
                    continue
                ch = sys.stdin.read(1).lower()
                if ch == "w":
                    self.send_warning_now()
                elif ch == "g":
                    self.send_gps_now()
                elif ch == "q":
                    self.stop_event.set()
                    return
        finally:
            termios.tcsetattr(fd, termios.TCSADRAIN, old)

    def run(self) -> int:
        print(f"Listening on {self.port} at {self.baud} baud as sender '{self.sender}'.")
        print("Keys: 'w' sends a GPS warning, 'g' sends GPS now, 'q' quits.")
        if self.startup_delay > 0:
            print(f"Waiting {self.startup_delay:.1f}s for Pico boot/bridge startup before sending.")
            time.sleep(self.startup_delay)
        rx_thread = threading.Thread(target=self.rx_loop, name="uart-rx", daemon=True)
        tx_thread = threading.Thread(target=self.tx_loop, name="uart-tx", daemon=True)
        rx_thread.start()
        tx_thread.start()

        try:
            self.key_loop()
        except KeyboardInterrupt:
            print("\nInterrupted by user.", file=sys.stderr)
            self.stop_event.set()

        self.stop_event.set()
        rx_thread.join(timeout=1.0)
        tx_thread.join(timeout=1.0)
        self.ser.close()
        return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Bind to a UART, generate packets, send them, and print received packets."
    )
    parser.add_argument("--port", required=True, help="UART device path, for example /dev/ttyUSB0")
    parser.add_argument("--baud", type=int, default=115200, help="UART baud rate")
    parser.add_argument("--sender", default="PY_UART_NODE", help="Telemetry sender name")
    parser.add_argument(
        "--interval",
        type=float,
        default=1.0,
        help="Seconds between generated packet bursts",
    )
    parser.add_argument(
        "--startup-delay",
        type=float,
        default=3.5,
        help="Seconds to wait after opening UART before first send",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    bridge = UartPacketBridge(
        port=args.port,
        baud=args.baud,
        sender=args.sender,
        interval=args.interval,
        startup_delay=args.startup_delay,
    )
    return bridge.run()


if __name__ == "__main__":
    raise SystemExit(main())
