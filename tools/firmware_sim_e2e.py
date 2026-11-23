#!/usr/bin/env python3
"""End-to-end check of the station's firmware simulation mode (docs/firmware-sim.md) on a development machine.

Starts the station exactly as the supervisor does for a saved "<mode>-firmware-sim" selection (station_modes.py
exec: the native bay with every board's real firmware first, then the backend on the bay's links), then drives it
through the ordinary authenticated HTTP API only and checks the firmware's own responses:

  1. every board (FC, RF, PB, GB, AB, VB, DAQ) is heard over the simulated radio and Pico-Fi umbilical;
  2. fill rules: with the vent closed, the VB tank PT reported by the VB firmware rises while the AB nitrogen valve
     is open, stops rising when the QD is disconnected (plant input), and falls on dump; nitrous raises the DAQ
     load cell (fill mass);
  3. Armed (HITL flight-state steps), then Launch: the boards run their own launch sequence (igniter, pilot), the
     plant lights the motor and flies; the FC's own estimator and recovery logic must reach Ascent, Apogee and
     fire its pyro outputs (drogue, then main), and the descent follows them to Landed.

  python3 tools/firmware_sim_e2e.py --out DIR [--mode hitl] [--port 3300] [--keep]

Needs a built cache (tools/firmware_sim.py build) and an installed backend (station_modes.py build). Writes
DIR/result.json, DIR/timeline.jsonl; the bay's board logs are under DIR/station-mode/firmware-sim/run/diagnostics.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import hmac
import json
import os
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
GS_ROOT = HERE.parent
USER, PASSWORD = "simadmin", "firmware-sim-e2e"
BOARDS = ["FC", "RF", "PB", "GB", "AB", "VB", "DAQ"]


def fixture_users():
    salt = b"firmware-sim-e2e"
    key = hashlib.pbkdf2_hmac("sha256", PASSWORD.encode(), salt, 120000)
    return key, {"version": 1, "anonymous": {"view_data": False, "send_commands": False}, "users": [
        {"username": USER, "roles": ["admin"], "password": {"algorithm": "pbkdf2_sha256", "iterations": 120000,
         "salt_b64": base64.b64encode(salt).decode(), "hash_b64": base64.b64encode(key).decode()},
         "permissions": {"view_data": True, "send_commands": True}, "disabled": False}]}


class Station:
    def __init__(self, origin: str, key: bytes, out: Path):
        self.origin, self.key, self.out, self.token = origin, key, out, None
        self.timeline = (out / "timeline.jsonl").open("a")
        self.t0 = time.monotonic()

    def log(self, what: str, **extra):
        row = {"t_s": round(time.monotonic() - self.t0, 2), "event": what, **extra}
        self.timeline.write(json.dumps(row, default=str) + "\n")
        self.timeline.flush()
        print(f"[e2e {row['t_s']:7.1f}] {what} {json.dumps(extra, default=str)[:300] if extra else ''}", flush=True)

    def api(self, path, body=None, timeout=15):
        headers = {"Content-Type": "application/json"}
        if self.token:
            headers["Authorization"] = "Bearer " + self.token
        req = urllib.request.Request(self.origin + path, None if body is None else json.dumps(body).encode(), headers)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                data = r.read(16 * 1024 * 1024)
                try:
                    return r.status, json.loads(data) if data else None
                except ValueError:
                    return r.status, data.decode(errors="replace")
        except urllib.error.HTTPError as e:
            return e.code, e.read(4096).decode(errors="replace")
        except (urllib.error.URLError, ConnectionError, TimeoutError, OSError) as e:
            return 0, str(e)

    def login(self):
        code, c = self.api("/api/auth/challenge", {"username": USER})
        if code != 200:
            return False
        nonce = base64.urlsafe_b64encode(os.urandom(32)).decode().rstrip("=")
        msg = {"username": USER, "challenge_id": c["challenge_id"], "server_nonce_b64": c["server_nonce_b64"],
               "client_nonce_b64": nonce, "remember_me": False}
        proof = base64.urlsafe_b64encode(hmac.new(self.key, json.dumps(msg, separators=(",", ":")).encode(),
                                                  hashlib.sha256).digest()).decode().rstrip("=")
        code, r = self.api("/api/auth/login", {**msg, "proof_b64": proof})
        if code == 200:
            self.token = r["token"]
        return code == 200

    def command(self, name):
        code, body = self.api("/api/command", name)
        self.log("command", cmd=name, http=code, body=body)
        return code, body

    def sim(self):
        code, s = self.api("/api/firmware-sim")
        return s if code == 200 and isinstance(s, dict) else {}

    def plant(self):
        return ((self.sim().get("bay") or {}).get("plant")) or {}

    def recent(self):
        code, rows = self.api("/api/recent")
        return rows if code == 200 and isinstance(rows, list) else []

    def latest(self, data_type: str, sender: str | None = None):
        best = None
        for r in self.recent():
            if r.get("data_type") == data_type and (sender is None or r.get("sender_id") == sender):
                if best is None or r.get("timestamp_ms", 0) >= best.get("timestamp_ms", 0):
                    best = r
        return best

    def wait(self, pred, timeout_s, label, poll=0.5):
        end = time.monotonic() + timeout_s
        last = None
        while time.monotonic() < end:
            try:
                last = pred()
                if last:
                    return last
            except (KeyError, TypeError, ValueError, IndexError):
                pass
            time.sleep(poll)
        self.log("timeout", label=label, last=last)
        return None


def start_station(out: Path, mode: str, port: int):
    state = out / "station-mode"
    state.mkdir(parents=True, exist_ok=True)
    (state / "selection.json").write_text(json.dumps({
        "version": 1, "role": "primary", "mode": mode, "simulation": False, "firmware_sim": True,
        "target_id": f"{mode}-firmware-sim", "transition_id": "e2e", "requested_by": "user:e2e", "requested_at_ms": 1}))
    key, users = fixture_users()
    (out / "users.json").write_text(json.dumps(users))
    cwd = out / "cwd"
    (cwd / "data").mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "GS_STATION_MODE_DIR": str(state), "GS_USERS_FILE": str(out / "users.json"),
           "GS_BIND_ADDRESS": f"127.0.0.1:{port}", "GS_HTTPS_BIND_ADDRESS": "off", "GS_STATION_DISCOVERY_BIND": "127.0.0.1:0",
           "GS_VIDEO_DISCOVERY_BIND": "127.0.0.1:0", "GS_TLS_DIR": str(out / "tls"), "GS_STATION_PAIRING_DIR": str(out / "pairing"),
           "GS_STATION_PAIRING_FILE": str(out / "pairing.json"), "RUST_LOG": os.environ.get("RUST_LOG", "info")}
    log = (out / "station.log").open("ab")
    proc = subprocess.Popen([sys.executable, str(GS_ROOT / "station_modes.py"), "exec", "--default-mode", mode],
                            cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    return proc, key


def stop_station(proc):
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGTERM)
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--mode", default="hitl")
    p.add_argument("--port", type=int, default=3300)
    p.add_argument("--keep", action="store_true", help="leave the station running at the end (Ctrl-C stops it)")
    p.add_argument("--interactive", action="store_true", help="only start the station and log in, then wait")
    a = p.parse_args()
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    proc, key = start_station(out, a.mode, a.port)
    st = Station(f"http://127.0.0.1:{a.port}", key, out)
    result = {"checks": [], "mode": a.mode}

    def check(name, ok, **extra):
        result["checks"].append({"name": name, "pass": bool(ok), **extra})
        st.log("CHECK " + ("PASS " if ok else "FAIL ") + name, **extra)
        (out / "result.json").write_text(json.dumps(result, indent=2, default=str))
        return ok

    try:
        if not st.wait(st.login, 120, "login", poll=1):
            check("station up and login", False)
            return 1
        sim = st.sim()
        check("station reports SIMULATED FIRMWARE", sim.get("active") and sim.get("banner") == "SIMULATED FIRMWARE",
              provenance=sim.get("provenance"), firmware={k: (v or {}).get("short") for k, v in ((sim.get("bay") or {}).get("firmware") or {}).items()})
        if a.interactive:
            (out / "token").write_text(st.token)
            st.log("interactive: station running", origin=st.origin, token_file=str(out / "token"))
            proc.wait()
            return 0
        run_scenario(st, check, result)
        return 0 if all(c["pass"] for c in result["checks"]) else 1
    finally:
        if a.keep:
            st.log("keeping the station running", origin=st.origin)
            try:
                proc.wait()
            except KeyboardInterrupt:
                pass
        stop_station(proc)


def vb_psi(st: Station):
    r = st.latest("FUEL_TANK_PRESSURE", "VB")
    return r["values"][0] if r else None


def set_valve(st: Station, name: str, want_open: bool, command: str, timeout_s=10):
    """Vent/dump: the station's toggle commands, until the plant sees the VB servo where we want it."""
    for _ in range(3):
        if st.plant().get("valves", {}).get(name) == want_open:
            return True
        st.command(command)
        if st.wait(lambda: st.plant().get("valves", {}).get(name) == want_open, timeout_s, f"{name} -> {want_open}"):
            return True
    return False


def fc_phase(st: Station):
    code, f = st.api("/flightstate")
    if code != 200 or not isinstance(f, dict):
        return None
    return (f.get("fc_phase") or {}).get("label") or f.get("state")


def run_scenario(st: Station, check, result):
    # 1. every board on the simulated links
    def all_seen():
        code, b = st.api("/api/boards")
        seen = {x["sender_id"] for x in (b or {}).get("boards", []) if x.get("seen") and x.get("packet_count")}
        return set(BOARDS) <= seen and seen
    seen = st.wait(all_seen, 180, "all boards heard", poll=2)
    check("all seven boards heard over the simulated radio and umbilical", seen, seen=sorted(seen or []))
    sim = st.sim()
    check("bay running at real time", (sim.get("bay") or {}).get("pace_actual", 0) > 0.9 and sim.get("bay_running"),
          pace=(sim.get("bay") or {}).get("pace_actual"), firmware={k: (v or {}).get("short") for k, v in ((sim.get("bay") or {}).get("firmware") or {}).items()})

    # 2. fill rules through the real VB/AB firmware
    check("vent and dump closed by the VB firmware", set_valve(st, "vent", False, "NormallyOpen") and set_valve(st, "dump", False, "Dump"),
          valves=st.plant().get("valves"))
    p0 = st.wait(lambda: vb_psi(st) is not None and vb_psi(st) + 0.001, 10, "VB pressure") or 0
    st.command("Nitrogen")
    st.wait(lambda: st.plant().get("valves", {}).get("nitrogen"), 10, "AB nitrogen valve open")
    time.sleep(6)
    p1 = vb_psi(st)
    check("VB tank PT rises with the AB nitrogen valve open and the QD connected", p1 is not None and p1 > p0 + 50, before=p0, after=p1)
    code, _ = st.api("/api/firmware-sim/control", {"plant": {"qd_connected": False}})
    time.sleep(1.5)
    q0 = vb_psi(st)
    time.sleep(5)
    q1 = vb_psi(st)
    check("VB tank PT does not rise with the QD disconnected (valve still open)", code == 200 and q1 is not None and abs(q1 - q0) < 8,
          before=q0, after=q1, ab_valve_open=st.plant().get("valves", {}).get("nitrogen"))
    st.api("/api/firmware-sim/control", {"plant": {"qd_connected": True}})
    st.command("NitrogenClose")
    st.wait(lambda: not st.plant().get("valves", {}).get("nitrogen"), 10, "AB nitrogen valve closed")
    set_valve(st, "dump", True, "Dump")
    def dumped():
        psi = vb_psi(st)
        return psi is not None and psi < 20 and {"psi": psi}
    low = st.wait(dumped, 30, "dumped")
    check("VB tank PT falls on dump", low is not None, after=vb_psi(st))
    set_valve(st, "dump", False, "Dump")
    m0 = (st.latest("LOADCELL_50_WEIGHT_KG", "DAQ") or {}).get("values", [0])[0] or 0
    # a same-temperature nitrous transfer needs the vent open: evaporation cools the tank below the bottle
    set_valve(st, "vent", True, "NormallyOpen")
    st.command("Nitrous")
    filled = st.wait(lambda: st.plant().get("tank", {}).get("liquid_kg", 0) >= 5.0, 240, "nitrous fill", poll=2)
    st.command("NitrousClose")
    set_valve(st, "vent", False, "NormallyOpen")
    st.wait(lambda: not st.plant().get("valves", {}).get("nitrous"), 10, "AB nitrous valve closed")
    time.sleep(2)
    m1 = (st.latest("LOADCELL_50_WEIGHT_KG", "DAQ") or {}).get("values", [0])[0] or 0
    check("nitrous fill: DAQ load cell and VB tank PT follow the fill", filled and m1 > m0 + 1 and (vb_psi(st) or 0) > 300,
          load_cell_before=m0, load_cell_after=m1, tank_psi=vb_psi(st), plant=st.plant().get("tank"))

    # 3. Armed, Launch, flight through the real FC
    for _ in range(12):
        phase = fc_phase(st)
        code, f = st.api("/flightstate")
        state = (f or {}).get("state") if isinstance(f, dict) else None
        if state == "Armed":
            break
        st.command("AdvanceFlightState")
        time.sleep(3)
    check("station and boards reach Armed (HITL steps)", state == "Armed", state=state, fc=fc_phase(st))
    launched = False
    for _ in range(8):
        st.api("/api/board_abort/refresh", {})
        time.sleep(2)
        code, body = st.command("Launch")
        if code == 200:
            launched = True
            break
        if "etract" in str(body):
            st.command("RetractPlumbing")
            time.sleep(6)
    check("Launch accepted by the station", launched)
    lift = st.wait(lambda: st.plant().get("flight", {}).get("phase") not in (None, "pad"), 90, "liftoff (igniter + pilot from the boards)")
    plant = st.plant()
    check("boards fire the igniter and open the pilot: liftoff", lift, events=[e["event"] for e in plant.get("events", [])][-8:])
    phases = []

    def track():
        ph = fc_phase(st)
        if ph and (not phases or phases[-1] != ph):
            phases.append(ph)
            st.log("FC phase", phase=ph, plant=st.plant().get("flight"))
        return st.plant().get("flight", {}).get("phase") == "landed"
    landed = st.wait(track, 420, "landed", poll=1)
    for _ in range(10):
        track()
        time.sleep(1)
    flight = st.plant().get("flight", {})
    result["fc_phases"] = phases
    result["flight"] = flight
    check("FC estimator reaches Ascent and Apogee", any("Ascent" in p or "Coast" in p for p in phases) and any("Apogee" in p or "Descent" in p for p in phases), phases=phases)
    # the measured 11-16 motor on the assumed airframe: about 1.4 km (tools/flight_profile measured_motor)
    check("apogee consistent with the measured motor", 800 < flight.get("apogee_m", 0) < 2500, apogee_m=flight.get("apogee_m"))
    check("FC fires its drogue output after apogee", flight.get("drogue_at_s") is not None, drogue_at_s=flight.get("drogue_at_s"))
    check("FC fires its main output", flight.get("main_at_s") is not None, main_at_s=flight.get("main_at_s"))
    check("descent follows the deployments to landing", landed is not None, phase=flight.get("phase"))
    check("FC reports Landed or Recovery", any(p in ("Landed", "Recovery") for p in phases), phases=phases)


if __name__ == "__main__":
    sys.exit(main())
