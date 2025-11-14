#!/usr/bin/env python3
import os
import time
import random
import numpy as np
import multiprocessing as mp
from queue import Empty

import sedsprintf_rs_2026 as seds

DT = seds.DataType
EP = seds.DataEndpoint
EK = seds.ElemKind

# ---------------- Enum helpers ----------------
def enum_to_int(obj):
    try:
        return int(obj)
    except Exception:
        return obj

def deep_coerce_enums(x):
    if isinstance(x, dict):
        return {deep_coerce_enums(k): deep_coerce_enums(v) for k, v in x.items()}
    if isinstance(x, (list, tuple)):
        t = type(x)
        return t(deep_coerce_enums(v) for v in x)
    return enum_to_int(x)

# ---------------- Router callbacks ----------------
def _now_ms() -> int:
    return int(time.time() * 1000)

def _tx(_bytes_buf: bytes):
    # Transmission stub (no-op)
    pass

def _on_packet(pkt: seds.Packet):
    print("[RX Packet]")
    print(pkt)  # pretty header + summary

def _on_serialized(data: bytes):
    print(f"[RX Serialized] {len(data)} bytes: {data.hex()}")

# ---------------- Server (single-threaded) ----------------
def router_server(cmd_q: mp.Queue, _done_evt_unused: mp.Event,
                  pump_period_ms: int = 2,
                  drain_grace_seconds: float = 3.0,
                  max_total_seconds: float = 60.0):
    """
    Single-threaded server:
      - Polls cmd_q for work
      - Calls router.log* directly
      - Pumps router periodically
      - On 'shutdown', drains briefly and exits
    This avoids thread pools / extra threads entirely → deterministic teardown.
    """
    handlers = [
        (int(EP.SD_CARD), _on_packet, None),
        (int(EP.RADIO),   None,       _on_serialized),
    ]
    router = seds.Router(tx=_tx, now_ms=_now_ms, handlers=handlers)
    print(f"[SERVER] Router up. PID={os.getpid()}")

    shutting_down = False
    drain_deadline = None
    last_pump = 0.0
    start_time = time.time()

    def pump_now():
        nonlocal last_pump
        try:
            router.process_all_queues()
        finally:
            last_pump = time.time()

    # Main loop
    while True:
        now = time.time()

        # Safety cap so we don't run forever if something external misbehaves
        if now - start_time > max_total_seconds:
            print("[SERVER] Max runtime reached; forcing shutdown.")
            break

        # Periodic pump
        if (now - last_pump) * 1000.0 >= pump_period_ms:
            pump_now()

        # Drain a bit of work from the command queue
        try:
            op, payload = cmd_q.get(timeout=0.05)
        except Empty:
            op, payload = None, None

        if op == "shutdown":
            # Start drain window
            shutting_down = True
            if drain_deadline is None:
                drain_deadline = time.time() + drain_grace_seconds
        elif op is not None:
            # Execute immediately in this process (no threads)
            try:
                if op == "log":
                    router.log(ty=payload["ty"], data=payload["data"],
                               elem_size=payload["elem_size"], elem_kind=payload["elem_kind"])
                elif op == "log_f32":
                    router.log_f32(ty=payload["ty"], values=payload["values"])
                elif op == "log_bytes":
                    router.log_bytes(ty=payload["ty"], data=payload["data"])
            except Exception as e:
                print(f"[SERVER] worker op error: {e!r}")

        # If in shutdown, keep pumping and stop when quiet or grace expires
        if shutting_down:
            pump_now()
            # Router queues are internal; we can't query length here,
            # so just wait for the grace window to pass.
            if time.time() >= drain_deadline:
                break

    # Final drain
    try:
        pump_now()
    except Exception as e:
        print(f"[SERVER] final drain error: {e!r}")
    print("[SERVER] Shutdown complete.")

# ---------------- Producer processes ----------------
def producer_proc(name: str, cmd_q: mp.Queue, n_iters: int, seed: int):
    random.seed(seed + os.getpid())
    print(f"[{name}] start PID={os.getpid()} iters={n_iters}")
    for i in range(n_iters):
        which = random.randint(0, 2)
        if which == 0:
            msg = f"{name} hello there {i}".encode("utf-8")
            cmd_q.put(deep_coerce_enums(("log_bytes", {
                "ty": DT.MESSAGE_DATA, "data": msg
            })))
        elif which == 1:
            vals = [101325.0 + random.random() * 100.0,
                    20.0 + random.random() * 10.0,
                    -0.5 + random.random()]
            cmd_q.put(deep_coerce_enums(("log_f32", {
                "ty": DT.BAROMETER_DATA, "values": vals
            })))
        else:
            arr = np.array([random.randint(0, 1000) for _ in range(8)], dtype=np.uint16)
            cmd_q.put(deep_coerce_enums(("log", {
                "ty": DT.GPS_DATA, "data": arr,
                "elem_size": 2, "elem_kind": EK.UNSIGNED
            })))
        time.sleep(random.random() * 0.002)  # 0–2ms jitter
    print(f"[{name}] done")

# ---------------- Main ----------------
def main():
    mp.set_start_method("spawn", force=True)

    cmd_q    = mp.Queue(maxsize=8192)
    done_evt = mp.Event()

    server = mp.Process(target=router_server, args=(cmd_q, done_evt, 2, 3.0, 120.0), daemon=False)
    server.start()

    n_producers = 6
    iters_per   = 500

    procs = []
    for i in range(n_producers):
        p = mp.Process(target=producer_proc, args=(f"P{i}", cmd_q, iters_per, 1337 + i), daemon=False)
        p.start()
        procs.append(p)

    # Wait for producers
    for p in procs:
        p.join()

    # Tell server to finish once queue is drained
    cmd_q.put(("shutdown", {}))
    cmd_q.close()
    cmd_q.join_thread()

    # Give it a short window to shut down gracefully; then enforce
    server.join(timeout=10)
    if server.is_alive():
        print("[MAIN] Server still alive after timeout; terminating…")
        server.terminate()
        server.join(timeout=5)

    if server.exitcode not in (0, None):
        print(f"[MAIN] Server exit code: {server.exitcode} (non-zero)")
        raise SystemExit(1)

    print("[MAIN] All done ✔️")

if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\n[MAIN] Interrupted")
