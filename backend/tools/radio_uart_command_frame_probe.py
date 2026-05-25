#!/usr/bin/env python3
import argparse
import binascii
import fcntl
import os
import select
import sys
import termios
import time


COMMAND_SYNC = (0xA6, 0x5B)
DATA_SYNC = (0xA5, 0x5A)
ASCII_SYNC = (0xA7, 0x7A)
HEADER_SIZE = 4


def configure_raw_serial(fd: int, baud: int) -> None:
    attrs = termios.tcgetattr(fd)
    attrs[0] = 0
    attrs[1] = 0
    attrs[2] = attrs[2] & ~(
        termios.PARENB | termios.CSTOPB | termios.CSIZE | termios.CRTSCTS
    )
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


def parse_hex(raw: str) -> bytes:
    cleaned = raw.replace(" ", "").replace(":", "").replace("_", "")
    if len(cleaned) % 2 != 0:
        raise ValueError("hex payload must have an even number of nybbles")
    return binascii.unhexlify(cleaned)


def build_frame(sync: tuple[int, int], payload: bytes) -> bytes:
    if not payload:
        raise ValueError("payload must not be empty")
    if len(payload) > 0xFFFF:
        raise ValueError("payload too large for raw UART frame")
    return bytes(sync) + len(payload).to_bytes(2, "little") + payload


def frame_sync(name: str) -> tuple[int, int]:
    if name == "command":
        return COMMAND_SYNC
    if name == "data":
        return DATA_SYNC
    if name == "ascii":
        return ASCII_SYNC
    raise ValueError(f"unknown frame type: {name}")


def hex_preview(data: bytes, limit: int = 32) -> str:
    preview = data[:limit].hex(" ")
    return preview + (" ..." if len(data) > limit else "")


def drain_rx(fd: int, duration_s: float, read_chunk: int, preview_bytes: int) -> int:
    end = time.monotonic() + duration_s
    total = 0
    while time.monotonic() < end:
        readable, _, _ = select.select([fd], [], [], 0.05)
        if not readable:
            continue
        try:
            chunk = os.read(fd, read_chunk)
        except BlockingIOError:
            continue
        if not chunk:
            continue
        total += len(chunk)
        print(f"rx {len(chunk)} bytes: {hex_preview(chunk, preview_bytes)}")
    return total


def default_radio_window_payload(uplink: bool, duration_ms: int) -> bytes:
    if duration_ms < 0 or duration_ms > 0xFFFF:
        raise ValueError("--window-ms must fit in u16")
    return bytes((0x01, 0x01 if uplink else 0x00)) + duration_ms.to_bytes(2, "little")


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Send raw UART command frames directly to the RF board. "
            "This bypasses the backend scheduler/router and tests the radio driver framing path."
        )
    )
    parser.add_argument("--port", default="/dev/ttyAMA0")
    parser.add_argument("--baud", type=int, default=9600)
    parser.add_argument(
        "--frame-type",
        choices=("command", "data", "ascii"),
        default="command",
        help="raw UART frame sync to use; command sends A6 5B",
    )
    parser.add_argument(
        "--payload-hex",
        help=(
            "payload bytes as hex. If omitted, sends radio-window control "
            "payload 01 01 <window-ms-le>."
        ),
    )
    parser.add_argument(
        "--window",
        choices=("uplink", "downlink"),
        default="uplink",
        help="default radio-window command payload direction when --payload-hex is omitted",
    )
    parser.add_argument("--window-ms", type=int, default=75)
    parser.add_argument("--count", type=int, default=10)
    parser.add_argument("--interval-ms", type=int, default=500)
    parser.add_argument("--read-after-ms", type=int, default=150)
    parser.add_argument("--read-chunk", type=int, default=512)
    parser.add_argument("--preview-bytes", type=int, default=48)
    args = parser.parse_args()

    payload = (
        parse_hex(args.payload_hex)
        if args.payload_hex
        else default_radio_window_payload(args.window == "uplink", args.window_ms)
    )
    frame = build_frame(frame_sync(args.frame_type), payload)

    fd = os.open(args.port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        configure_raw_serial(fd, args.baud)
        set_nonblocking(fd)
        print(
            f"opened {args.port} @ {args.baud}; sending {args.frame_type} frame "
            f"{args.count} time(s)"
        )
        print(f"payload {len(payload)} bytes: {hex_preview(payload, args.preview_bytes)}")
        print(f"frame   {len(frame)} bytes: {hex_preview(frame, args.preview_bytes)}")

        for idx in range(args.count):
            written = os.write(fd, frame)
            termios.tcdrain(fd)
            print(f"tx #{idx + 1}: wrote/drained {written} bytes")
            rx_total = drain_rx(
                fd,
                args.read_after_ms / 1000.0,
                args.read_chunk,
                args.preview_bytes,
            )
            if rx_total == 0:
                print("rx: no bytes during read-after window")
            if idx + 1 < args.count:
                time.sleep(args.interval_ms / 1000.0)
        return 0
    finally:
        os.close(fd)


if __name__ == "__main__":
    sys.exit(main())
