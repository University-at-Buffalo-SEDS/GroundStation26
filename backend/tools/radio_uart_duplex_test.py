#!/usr/bin/env python3
import argparse
import binascii
import fcntl
import os
import select
import sys
import termios
import time


SYNC_0 = 0xA5
SYNC_1 = 0x5A
RAW_HEADER_SIZE = 4


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


def set_blocking(fd: int, blocking: bool) -> None:
    flags = fcntl.fcntl(fd, fcntl.F_GETFL)
    if blocking:
        flags &= ~os.O_NONBLOCK
    else:
        flags |= os.O_NONBLOCK
    fcntl.fcntl(fd, fcntl.F_SETFL, flags)


def parse_hex_bytes(raw: str) -> bytes:
    cleaned = raw.replace(" ", "").replace(":", "")
    if len(cleaned) % 2 != 0:
        raise ValueError("hex string must have an even number of nybbles")
    return binascii.unhexlify(cleaned)


def build_frame(payload: bytes) -> bytes:
    if not payload or len(payload) > 0xFFFF:
        raise ValueError("payload must be between 1 and 65535 bytes")
    return bytes((SYNC_0, SYNC_1)) + len(payload).to_bytes(2, "little") + payload


def extract_frames(buffer: bytearray) -> list[bytes]:
    out: list[bytes] = []
    while True:
        sync_pos = -1
        for idx in range(max(0, len(buffer) - 1)):
            if buffer[idx] == SYNC_0 and buffer[idx + 1] == SYNC_1:
                sync_pos = idx
                break
        if sync_pos < 0:
            if buffer and buffer[-1] == SYNC_0:
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
            del buffer[:1]
            continue
        if len(buffer) < total_len:
            break
        out.append(bytes(buffer[RAW_HEADER_SIZE:total_len]))
        del buffer[:total_len]
    return out


def maybe_preview(data: bytes, limit: int) -> str:
    return data[:limit].hex(" ")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Own a UART, receive aggressively, optionally transmit on quiet windows."
    )
    parser.add_argument("--port", default="/dev/ttyAMA0")
    parser.add_argument("--baud", type=int, default=9600)
    parser.add_argument("--duration", type=float, default=15.0)
    parser.add_argument("--read-chunk", type=int, default=512)
    parser.add_argument("--preview-bytes", type=int, default=32)
    parser.add_argument(
        "--tx-hex",
        help="inner serialized payload as hex; script wraps it in A5 5A raw-uart framing",
    )
    parser.add_argument(
        "--tx-raw-frame-hex",
        help="full raw UART frame bytes as hex; sent as-is without wrapping",
    )
    parser.add_argument("--tx-interval-ms", type=int, default=1000)
    parser.add_argument(
        "--tx-quiet-ms",
        type=int,
        default=150,
        help="only transmit if no RX bytes arrived for this long",
    )
    parser.add_argument(
        "--print-frames",
        action="store_true",
        help="print each extracted raw-uart frame payload preview",
    )
    parser.add_argument(
        "--rx-only",
        action="store_true",
        help="disable transmit even if tx bytes are provided",
    )
    args = parser.parse_args()

    tx_bytes = b""
    if args.tx_raw_frame_hex:
        tx_bytes = parse_hex_bytes(args.tx_raw_frame_hex)
    elif args.tx_hex:
        tx_bytes = build_frame(parse_hex_bytes(args.tx_hex))

    fd = os.open(args.port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    try:
        configure_raw_serial(fd, args.baud)
        set_blocking(fd, False)

        start = time.monotonic()
        deadline = start + args.duration
        last_rx_time = start
        last_tx_time = 0.0
        last_stats_time = 0.0
        rx_buffer = bytearray()
        rx_bytes = 0
        rx_frames = 0
        tx_count = 0

        print(
            f"listening on {args.port} @ {args.baud}; duration={args.duration}s"
            + ("; tx enabled" if tx_bytes and not args.rx_only else "; rx only")
        )

        while time.monotonic() < deadline:
            timeout = 0.05
            readable, _, _ = select.select([fd], [], [], timeout)
            now = time.monotonic()
            if readable:
                try:
                    chunk = os.read(fd, args.read_chunk)
                except BlockingIOError:
                    chunk = b""
                if chunk:
                    last_rx_time = now
                    rx_bytes += len(chunk)
                    rx_buffer.extend(chunk)
                    frames = extract_frames(rx_buffer)
                    rx_frames += len(frames)
                    if args.print_frames:
                        for frame in frames:
                            print(
                                f"rx frame len={len(frame)} preview={maybe_preview(frame, args.preview_bytes)}"
                            )

            should_tx = (
                tx_bytes
                and not args.rx_only
                and (now - last_tx_time) * 1000.0 >= args.tx_interval_ms
                and (now - last_rx_time) * 1000.0 >= args.tx_quiet_ms
            )
            if should_tx:
                written = os.write(fd, tx_bytes)
                tx_count += 1
                last_tx_time = now
                print(f"tx write #{tx_count}: {written} bytes")

            if now - last_stats_time >= 1.0:
                print(
                    f"stats rx_bytes={rx_bytes} rx_frames={rx_frames} tx_count={tx_count} "
                    f"idle_ms={(now - last_rx_time) * 1000.0:.0f} buffered={len(rx_buffer)}"
                )
                last_stats_time = now

        print(
            f"done rx_bytes={rx_bytes} rx_frames={rx_frames} tx_count={tx_count} buffered={len(rx_buffer)}"
        )
        return 0
    finally:
        os.close(fd)


if __name__ == "__main__":
    sys.exit(main())
