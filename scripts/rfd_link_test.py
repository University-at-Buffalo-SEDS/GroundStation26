#!/usr/bin/env python3
"""Bench RFD link characterisation with no dependencies beyond the stdlib.

Run with the station service and RRLS stopped (they own the serial ports).

  regs  PORT                 read the modem's ATI/ATI5 registers (+++ / ATO)
  set   PORT S5=1 S6=10 ...  write registers, AT&W and reboot (ATZ)
  echo  PORT                 answer every PING with a PONG; report receive stats
  ping  PORT --rate R ...    send PINGs, report loss and round-trip latency
  duplex PORT --rate R ...   run on both ends at once: send PINGs, count the peer's

Frames: A5 5A | kind u8 | seq u32 | sent_ns u64 | pad_len u16 | pad | crc32.
Anything else on the air (RF board telemetry, SEDSnet) is counted as foreign
bytes and skipped, so the test runs alongside live board traffic.
"""
import argparse
import os
import select
import struct
import sys
import termios
import time
import zlib

MAGIC = b"\xa5\x5a"
HEAD = struct.Struct("<BIQH")
PING, PONG = 1, 2
BAUDS = {57600: termios.B57600, 115200: termios.B115200, 230400: getattr(termios, "B230400", 0)}


class Port:
    def __init__(self, path, baud):
        self.fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        attrs = termios.tcgetattr(self.fd)
        attrs[0] = 0  # iflag: raw, no XON/XOFF
        attrs[1] = 0  # oflag
        attrs[2] = termios.CS8 | termios.CREAD | termios.CLOCAL  # no CRTSCTS
        attrs[3] = 0  # lflag
        attrs[4] = attrs[5] = BAUDS[baud]
        attrs[6][termios.VMIN] = 0
        attrs[6][termios.VTIME] = 0
        termios.tcsetattr(self.fd, termios.TCSANOW, attrs)
        termios.tcflush(self.fd, termios.TCIOFLUSH)

    def write(self, data):
        view = memoryview(data)
        while view:
            select.select([], [self.fd], [], 1.0)
            try:
                view = view[os.write(self.fd, view):]
            except BlockingIOError:
                pass

    def read(self, timeout):
        ready, _, _ = select.select([self.fd], [], [], timeout)
        if not ready:
            return b""
        try:
            return os.read(self.fd, 4096)
        except BlockingIOError:
            return b""


def frame(kind, seq, sent_ns, pad):
    body = HEAD.pack(kind, seq, sent_ns, len(pad)) + pad
    return MAGIC + body + struct.pack("<I", zlib.crc32(body))


class Parser:
    def __init__(self):
        self.buf = bytearray()
        self.foreign = 0
        self.bad_crc = 0

    def feed(self, data):
        self.buf += data
        out = []
        while True:
            at = self.buf.find(MAGIC)
            if at < 0:
                keep = 1 if self.buf[-1:] == MAGIC[:1] else 0
                self.foreign += len(self.buf) - keep
                del self.buf[: len(self.buf) - keep]
                return out
            self.foreign += at
            del self.buf[:at]
            if len(self.buf) < 2 + HEAD.size:
                return out
            kind, seq, sent_ns, pad_len = HEAD.unpack_from(self.buf, 2)
            total = 2 + HEAD.size + pad_len + 4
            if kind not in (PING, PONG) or pad_len > 4000:
                self.foreign += 1
                del self.buf[:1]
                continue
            if len(self.buf) < total:
                return out
            body = bytes(self.buf[2 : total - 4])
            (crc,) = struct.unpack_from("<I", self.buf, total - 4)
            if zlib.crc32(body) != crc:
                self.bad_crc += 1
                del self.buf[:1]
                continue
            del self.buf[:total]
            out.append((kind, seq, sent_ns, len(body) - HEAD.size))


def command_mode(port):
    time.sleep(1.2)
    port.write(b"+++")
    reply = collect(port, 1.5)
    if b"OK" not in reply:
        raise SystemExit(f"no OK after +++ (got {reply!r}); is another program using the port?")


def at(port, cmd, wait=0.6):
    port.write(cmd.encode() + b"\r")
    return collect(port, wait).decode(errors="replace")


def collect(port, seconds):
    end = time.monotonic() + seconds
    data = b""
    while time.monotonic() < end:
        data += port.read(end - time.monotonic())
    return data


def cmd_regs(args):
    port = Port(args.port, args.baud)
    command_mode(port)
    for cmd in ("ATI", "ATI5", "ATI6", "ATI7"):
        print(f"--- {cmd}\n{at(port, cmd, 1.0).strip()}")
    print(at(port, "ATO").strip())


def cmd_set(args):
    port = Port(args.port, args.baud)
    command_mode(port)
    for item in args.values:
        reg, value = item.upper().split("=")
        print(f"AT{reg}={value}:", at(port, f"AT{reg}={value}").strip())
    print("AT&W:", at(port, "AT&W", 1.0).strip())
    print("ATZ:", at(port, "ATZ", 2.0).strip() or "(rebooting)")


def report(label, start, stats):
    elapsed = time.monotonic() - start
    print(f"[{label} {elapsed:6.1f}s] " + " ".join(f"{k}={v}" for k, v in stats.items()), flush=True)


def cmd_echo(args):
    port = Port(args.port, args.baud)
    parser = Parser()
    start = last = time.monotonic()
    stats = {"pings": 0, "seq_gaps": 0, "foreign_B": 0, "bad_crc": 0}
    expected = None
    missing = []
    end = start + args.duration if args.duration else None
    while end is None or time.monotonic() < end:
        for kind, seq, sent_ns, pad in parser.feed(port.read(0.05)):
            if kind != PING:
                continue
            stats["pings"] += 1
            if expected is not None and seq > expected:
                stats["seq_gaps"] += seq - expected
                missing.extend(range(expected, seq))
            expected = seq + 1
            if not args.silent:
                port.write(frame(PONG, seq, sent_ns, b"\x00" * min(pad, args.pong_size)))
        if time.monotonic() - last >= 1.0:
            stats["foreign_B"], stats["bad_crc"] = parser.foreign, parser.bad_crc
            report("echo", start, stats)
            last = time.monotonic()
    print("missing seq:", missing)


def pct(values, q):
    if not values:
        return "-"
    ordered = sorted(values)
    return f"{ordered[min(len(ordered) - 1, int(q * len(ordered)))]:.0f}"


def cmd_ping(args):
    port = Port(args.port, args.baud)
    parser = Parser()
    pad = bytes(range(256)) * (args.size // 256 + 1)
    pad = pad[: args.size]
    interval = 1.0 / args.rate
    start = time.monotonic()
    next_send, last, seq = start, start, 0
    sent_at, rtts, window = {}, [], []
    end = start + args.duration
    while time.monotonic() < end + args.drain:
        now = time.monotonic()
        if now < end and now >= next_send:
            # --burst frames written back to back, as a paced writer's burst.
            port.write(b"".join(frame(PING, seq + k, time.monotonic_ns(), pad) for k in range(args.burst)))
            for k in range(args.burst):
                sent_at[seq + k] = now
            seq += args.burst
            next_send += interval
        for kind, rseq, sent_ns, _ in parser.feed(port.read(min(0.02, max(0.0, next_send - now)))):
            if kind == PONG and rseq in sent_at:
                rtt = (time.monotonic_ns() - sent_ns) / 1e6
                rtts.append(rtt)
                window.append(rtt)
                del sent_at[rseq]
        if time.monotonic() - last >= 1.0:
            report("ping", start, {"sent": seq, "answered": len(rtts), "rtt_p50": pct(window, .5),
                                   "rtt_p95": pct(window, .95), "rtt_max": pct(window, 1.0),
                                   "foreign_B": parser.foreign})
            window, last = [], time.monotonic()
    lost = len(sent_at)
    print(f"\nRESULT rate={args.rate}/s burst={args.burst} size={args.size}B duration={args.duration}s sent={seq} "
          f"answered={len(rtts)} lost={lost} ({100 * lost / max(seq, 1):.1f}%) "
          f"rtt_p50={pct(rtts, .5)} rtt_p95={pct(rtts, .95)} rtt_max={pct(rtts, 1.0)} ms "
          f"foreign_B={parser.foreign} bad_crc={parser.bad_crc}")


def cmd_duplex(args):
    # Run on both ends at once: each sends PINGs and counts the peer's.
    port = Port(args.port, args.baud)
    parser = Parser()
    pad = bytes(args.size)
    start = time.monotonic()
    next_send, seq, got = start, 0, 0
    while time.monotonic() < start + args.duration + args.drain:
        now = time.monotonic()
        if now < start + args.duration and now >= next_send:
            port.write(frame(PING, seq, time.monotonic_ns(), pad))
            seq += 1
            next_send += 1.0 / args.rate
        got += sum(kind == PING for kind, *_ in parser.feed(port.read(0.005)))
    print(f"RESULT duplex rate={args.rate}/s size={args.size}B duration={args.duration}s sent={seq} "
          f"received_from_peer={got} foreign_B={parser.foreign} bad_crc={parser.bad_crc}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("regs", "set", "echo", "ping", "duplex"):
        p = sub.add_parser(name)
        p.add_argument("port")
        p.add_argument("--baud", type=int, default=115200)
        if name == "set":
            p.add_argument("values", nargs="+", help="e.g. S5=1 S6=10")
        if name == "echo":
            p.add_argument("--duration", type=float, default=0)
            p.add_argument("--pong-size", type=int, default=0, help="pad bytes in each PONG")
            p.add_argument("--silent", action="store_true", help="count PINGs only; never transmit")
        if name == "ping":
            p.add_argument("--rate", type=float, default=5, help="writes per second")
            p.add_argument("--burst", type=int, default=1, help="frames per write")
            p.add_argument("--size", type=int, default=32, help="pad bytes per PING")
            p.add_argument("--duration", type=float, default=20)
            p.add_argument("--drain", type=float, default=5, help="seconds to wait for late PONGs")
        if name == "duplex":
            p.add_argument("--rate", type=float, default=5, help="PINGs per second")
            p.add_argument("--size", type=int, default=32, help="pad bytes per PING")
            p.add_argument("--duration", type=float, default=12)
            p.add_argument("--drain", type=float, default=1, help="seconds to keep counting after sending")
    args = ap.parse_args()
    {"regs": cmd_regs, "set": cmd_set, "echo": cmd_echo, "ping": cmd_ping, "duplex": cmd_duplex}[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
