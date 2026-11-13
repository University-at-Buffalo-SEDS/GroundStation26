#!/usr/bin/env python3
"""Bench RFD link characterisation with no dependencies beyond the stdlib.

Run with the station service and RRLS stopped (they own the serial ports).

  regs  PORT                 read the modem's ATI/ATI5 registers (+++ / ATO)
  set   PORT S5=1 S6=10 ...  write registers, AT&W and reboot (ATZ)
  echo  PORT                 answer every PING with a PONG; report receive stats
  ping  PORT --rate R ...    send PINGs, report loss and round-trip latency
  duplex PORT --rate R ...   run on both ends at once: send PINGs, count the peer's
  slotted PORT --station K   duplex, but each end writes only in its own slot of the
                             host wall clock (run NTP/chrony on both hosts)

Frames: A5 5A | kind u8 | seq u32 | sent_ns u64 | pad_len u16 | pad | crc32.
A frame is pad + 21 bytes.

slotted: the cycle is --stations slots of --slot-ms; slot k belongs to station k,
counted from the Unix epoch (plus --epoch-offset-ms), so two hosts with
synchronised clocks agree with no handshake. In its own slot a station waits
--guard-ms, then writes everything queued (up to --slot-bytes) as one
contiguous write, so the modem sends it as one air packet. It never writes
outside its slot except --abort-every frames, which go out at once
(the ABORT bypass) and are counted separately. Use --free to keep the same
traffic but write each frame as it is produced (today's free-for-all), and
--clock-error-ms to shift this host's slot clock on purpose (guard test).
Run the same command on both ends with --station 0 and --station 1.
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
PING, PONG, SLOT, ABORT = 1, 2, 3, 4
KINDS = (PING, PONG, SLOT, ABORT)
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
            if kind not in KINDS or pad_len > 4000:
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


UART_MS_PER_B = 10 / 115.2  # 115200 baud, 10 bits per byte
AIR_MS_PER_B = 8 / 64.0  # S2=64 kbit/s
PACKET_FIXED_MS = 17  # S6 (10 ms) plus modem TX setup (4-7 ms)


def default_slot_bytes(slot_ms, guard_ms):
    # Conservative: the modem waits for the whole write plus S6 before it
    # transmits, then sends it at the air rate, and must end a guard before
    # the slot does.
    usable = slot_ms - 2 * guard_ms - PACKET_FIXED_MS
    return max(64, int(usable / (UART_MS_PER_B + AIR_MS_PER_B)))


def cmd_slotted(args):
    import random
    from collections import deque

    port = Port(args.port, args.baud)
    parser = Parser()
    pad = bytes(args.size)
    frame_len = args.size + 21
    cycle_ms = args.slot_ms * args.stations
    slot_bytes = args.slot_bytes or default_slot_bytes(args.slot_ms, args.guard_ms)
    shift_ns = int((args.clock_error_ms + args.epoch_offset_ms) * 1e6)
    me = args.station
    rng = random.Random(me * 7919 + int(time.time()))

    def wall_ms():
        return (time.time_ns() + shift_ns) / 1e6

    print(f"slotted station={me}/{args.stations} slot={args.slot_ms}ms guard={args.guard_ms}ms "
          f"cycle={cycle_ms}ms slot_bytes={slot_bytes} frame={frame_len}B rate={args.rate}/s "
          f"({args.rate * frame_len / 1000:.2f} KB/s offered) mode={'free' if args.free else 'slotted'} "
          f"clock_error={args.clock_error_ms}ms", flush=True)
    if not args.free and frame_len > slot_bytes:
        raise SystemExit(f"a {frame_len} B frame does not fit a {slot_bytes} B slot")

    queue = deque()
    stats = {"sent": 0, "aborts_sent": 0, "queue_drops": 0, "writes": 0, "late_slots": 0}
    write_late, lat, abort_lat, phase = [], [], [], []
    got = {}  # peer station -> [count, max_seq, aborts]
    in_own_slot = 0
    rx_pad = {}
    seq = 0
    abort_seq = 0
    start = time.monotonic()
    end = start + args.duration
    next_frame = start
    next_abort = start + (rng.uniform(0, args.abort_every) if args.abort_every else float("inf"))
    served_slot = None
    last = start

    def emit(kind, n, pad_bytes):
        return frame(kind, (me << 24) | n, int(wall_ms() * 1e6), pad_bytes)

    while time.monotonic() < end + args.drain:
        now = time.monotonic()
        sending = now < end
        # Offered traffic, paced on the monotonic clock.
        while sending and now >= next_frame:
            if len(queue) * frame_len >= args.queue_bytes:
                stats["queue_drops"] += 1
            else:
                queue.append(seq)
            seq += 1
            next_frame += 1.0 / args.rate
            if args.free:
                break
        if sending and now >= next_abort:
            # ABORT bypass: written at once, whatever the slot.
            port.write(emit(ABORT, abort_seq, bytes(args.abort_size)))
            abort_seq += 1
            stats["aborts_sent"] += 1
            next_abort += args.abort_every
        t = wall_ms()
        slot_index = int(t // args.slot_ms)
        into_slot = t - slot_index * args.slot_ms
        owner = slot_index % args.stations
        if sending and args.free:
            if queue:
                n = queue.popleft()
                port.write(emit(SLOT, n, pad))
                stats["sent"] += 1
                stats["writes"] += 1
        elif sending and owner == me and served_slot != slot_index and into_slot >= args.guard_ms:
            served_slot = slot_index
            if into_slot > args.guard_ms + args.late_ms:
                stats["late_slots"] += 1  # woke too late: skip rather than run into the next slot
            elif queue:
                batch = []
                while queue and (len(batch) + 1) * frame_len <= slot_bytes:
                    batch.append(emit(SLOT, queue.popleft(), pad))
                write_late.append(into_slot - args.guard_ms)
                port.write(b"".join(batch))
                stats["sent"] += len(batch)
                stats["writes"] += 1
        # Receive until the next thing to do (at most 5 ms).
        for kind, rseq, sent_ns, rpad in parser.feed(port.read(0.002 if args.free else 0.005)):
            peer, n = rseq >> 24, rseq & 0xFFFFFF
            if peer == me:
                continue
            arrival = wall_ms()
            entry = got.setdefault(peer, [0, -1, 0, -1, n])
            if kind == SLOT:
                entry[0] += 1
                entry[1] = max(entry[1], n)
                entry[4] = min(entry[4], n)
                rx_pad[peer] = rx_pad.get(peer, 0) + rpad
                lat.append(arrival - sent_ns / 1e6)
                # Where in the cycle the frame arrived, relative to the sender's slot start.
                phase.append((arrival - peer * args.slot_ms) % cycle_ms)
                if int(arrival // args.slot_ms) % args.stations == me:
                    in_own_slot += 1
            elif kind == ABORT:
                entry[2] += 1
                entry[3] = max(entry[3], n)
                abort_lat.append(arrival - sent_ns / 1e6)
        if time.monotonic() - last >= 1.0:
            rx = sum(v[0] for v in got.values())
            report("slotted", start, {**stats, "queued": len(queue), "peer_frames": rx, "in_own_slot": in_own_slot,
                                      "bad_crc": parser.bad_crc, "foreign_B": parser.foreign})
            last = time.monotonic()

    for peer, (count, max_seq, aborts, max_abort, min_seq) in sorted(got.items()):
        peer_sent = max_seq - min_seq + 1
        print(f"RESULT slotted station={me} peer={peer} mode={'free' if args.free else 'slotted'} "
              f"slot={args.slot_ms}ms guard={args.guard_ms}ms slot_bytes={slot_bytes} frame={frame_len}B "
              f"rate={args.rate}/s duration={args.duration}s sent={stats['sent']} "
              f"peer_frames={count} peer_highest_seq={max_seq} "
              f"delivered={100 * count / max(peer_sent, 1):.1f}% (of seq {min_seq}..{max_seq}) "
              f"goodput={rx_pad.get(peer, 0) / args.duration:.0f}B/s "
              f"lat_p50={pct(lat, .5)} lat_p95={pct(lat, .95)} lat_max={pct(lat, 1.0)} ms "
              f"aborts_rx={aborts}/{max_abort + 1} abort_lat_p50={pct(abort_lat, .5)} abort_lat_max={pct(abort_lat, 1.0)} ms "
              f"arrival_phase_p5={pct(phase, .05)} p50={pct(phase, .5)} p95={pct(phase, .95)} ms "
              f"in_own_slot={in_own_slot} bad_crc={parser.bad_crc} foreign_B={parser.foreign} "
              f"late_slots={stats['late_slots']} write_late_p50={pct(write_late, .5)} "
              f"write_late_max={pct(write_late, 1.0)} ms queue_drops={stats['queue_drops']} "
              f"aborts_sent={stats['aborts_sent']}")
    if not got:
        print(f"RESULT slotted station={me} sent={stats['sent']} peer_frames=0 bad_crc={parser.bad_crc} "
              f"foreign_B={parser.foreign}")
    print("note: latencies are wall-clock one-way and include the hosts' clock offset; "
          "the mean of the two ends' lat_p50 cancels it.")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("regs", "set", "echo", "ping", "duplex", "slotted"):
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
        if name == "slotted":
            p.add_argument("--station", type=int, required=True, help="this end's slot, 0 .. stations-1")
            p.add_argument("--stations", type=int, default=2, help="slots per cycle")
            p.add_argument("--slot-ms", type=float, default=200)
            p.add_argument("--guard-ms", type=float, default=15, help="wait this long into the slot before writing")
            p.add_argument("--slot-bytes", type=int, default=0,
                           help="most bytes written per slot (default: what fits the slot, conservative)")
            p.add_argument("--rate", type=float, default=5, help="frames per second offered")
            p.add_argument("--size", type=int, default=491, help="pad bytes per frame (491 = 512 B frames)")
            p.add_argument("--queue-bytes", type=int, default=16384, help="drop new frames beyond this backlog")
            p.add_argument("--late-ms", type=float, default=10,
                           help="skip the slot if the write would start later than guard + this")
            p.add_argument("--abort-every", type=float, default=0,
                           help="also send a small ABORT frame at once, ignoring slots, every N s (0 = off)")
            p.add_argument("--abort-size", type=int, default=16)
            p.add_argument("--free", action="store_true", help="ignore slots: write each frame as produced")
            p.add_argument("--clock-error-ms", type=float, default=0, help="shift this host's slot clock")
            p.add_argument("--epoch-offset-ms", type=float, default=0, help="shift the slot grid on both ends")
            p.add_argument("--duration", type=float, default=20)
            p.add_argument("--drain", type=float, default=2, help="seconds to keep counting after sending")
    args = ap.parse_args()
    {"regs": cmd_regs, "set": cmd_set, "echo": cmd_echo, "ping": cmd_ping, "duplex": cmd_duplex,
     "slotted": cmd_slotted}[args.cmd](args)


if __name__ == "__main__":
    sys.exit(main())
