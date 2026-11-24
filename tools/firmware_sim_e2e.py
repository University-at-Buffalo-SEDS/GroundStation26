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
BAY_STALL_S = 60.0


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
        self.bay_status = out / "station-mode" / "firmware-sim" / "run" / "status.json"
        self.v0 = None

    def now(self) -> float:
        """Seconds on the station's clock: the bay's virtual time when the station follows it (a bay slowed by host
        load then slows every wait and pause of this check with it), else the wall clock. Continuous across the
        switch from the wall (no bay status yet) to the bay's clock."""
        wall = time.monotonic()
        try:
            st = self.bay_status.stat()
            status = json.loads(self.bay_status.read_text())
        except (OSError, ValueError):
            status = None
        if not status or status.get("station_clock") != "bay":
            return wall if self.v0 is None else self.v0[0] + self.v0[1]
        # the status is written once per virtual second: interpolate at most one second past it
        v = status.get("virtual_ms", 0) / 1000.0 + min(max(time.time() - st.st_mtime, 0.0), 1.0)
        if self.v0 is None:
            self.v0 = (wall - v, v)  # (offset onto the wall scale at the switch, latest virtual seconds)
            self.v_moved = wall
        if v > self.v0[1]:
            self.v0 = (self.v0[0], v)  # never backwards
            self.v_moved = wall
        # a bay that stopped (crashed, or stalled for over a minute) must not hang the check: the wall takes over
        return self.v0[0] + self.v0[1] + max(0.0, wall - self.v_moved - BAY_STALL_S)

    def sleep(self, seconds: float):
        end = self.now() + seconds
        while self.now() < end:
            time.sleep(min(0.2, seconds))

    def log(self, what: str, **extra):
        row = {"t_s": round(time.monotonic() - self.t0, 2), "event": what, **extra}
        if self.v0 is not None:
            row["virtual_s"] = round(self.v0[1], 1)
        self.timeline.write(json.dumps(row, default=str) + "\n")
        self.timeline.flush()
        print(f"[e2e {row['t_s']:7.1f}{' v' + format(row['virtual_s'], '7.1f') if 'virtual_s' in row else ''}] {what} "
              f"{json.dumps(extra, default=str)[:300] if extra else ''}", flush=True)

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
        code, rows = self.api("/api/recent?display=true&max_rows=6000")
        return rows if code == 200 and isinstance(rows, list) else []

    def latest(self, data_type: str, sender: str | None = None):
        best = None
        for r in self.recent():
            if r.get("data_type") == data_type and (sender is None or r.get("sender_id") == sender):
                if best is None or r.get("timestamp_ms", 0) >= best.get("timestamp_ms", 0):
                    best = r
        return best

    def wait(self, pred, timeout_s, label, poll=0.5):
        end = self.now() + timeout_s
        last = None
        while self.now() < end:
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
    if a.mode == "integrated_ground_test" and "FIRMWARE_SIM_CONTROL" not in os.environ:
        # the IGT card goes in with a dataset before power-on; the scenario then swaps it for another (Retry SD)
        os.environ["FIRMWARE_SIM_CONTROL"] = json.dumps({"igt": {"dataset": IGT_POWER_ON_DATASET}})
    proc, key = start_station(out, a.mode, a.port)
    st = Station(f"http://127.0.0.1:{a.port}", key, out)
    st.mode = a.mode
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
        SCENARIOS[a.mode](st, check, result)
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
    r = st.latest("OX_TANK_PRESSURE", "VB")
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


def boards_up(st: Station, check, boards=BOARDS):
    def all_seen():
        code, b = st.api("/api/boards")
        seen = {x["sender_id"] for x in (b or {}).get("boards", []) if x.get("seen") and x.get("packet_count")}
        return set(boards) <= seen and seen
    seen = st.wait(all_seen, 180, "all boards heard", poll=2)
    check(f"boards heard over the simulated links ({', '.join(boards)})", seen, seen=sorted(seen or []))
    # pace since the bay started (start-up costs settle within a minute)
    st.wait(lambda: (st.sim().get("bay") or {}).get("pace_actual", 0) > 0.9, 90, "pace")
    sim = st.sim()
    check("bay running at real time", (sim.get("bay") or {}).get("pace_actual", 0) > 0.9 and sim.get("bay_running"),
          pace=(sim.get("bay") or {}).get("pace_actual"), firmware={k: (v or {}).get("short") for k, v in ((sim.get("bay") or {}).get("firmware") or {}).items()})

def fill(st: Station, check, nitrous_kg=5.0):
    check("vent and dump closed by the VB firmware", set_valve(st, "vent", False, "NormallyOpen") and set_valve(st, "dump", False, "Dump"),
          valves=st.plant().get("valves"))
    p0 = st.wait(lambda: vb_psi(st) is not None and vb_psi(st) + 0.001, 10, "VB pressure") or 0
    st.command("Nitrogen")
    st.wait(lambda: st.plant().get("valves", {}).get("nitrogen"), 10, "AB nitrogen valve open")
    st.sleep(6)
    p1 = vb_psi(st)
    check("VB tank PT rises with the AB nitrogen valve open and the QD connected", p1 is not None and p1 > p0 + 50, before=p0, after=p1)
    code, _ = st.api("/api/firmware-sim/control", {"plant": {"qd_connected": False}})
    st.sleep(1.5)
    q0 = vb_psi(st)
    st.sleep(5)
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
    filled = st.wait(lambda: st.plant().get("tank", {}).get("liquid_kg", 0) >= nitrous_kg, 240, "nitrous fill", poll=2)
    st.command("NitrousClose")
    set_valve(st, "vent", False, "NormallyOpen")
    st.wait(lambda: not st.plant().get("valves", {}).get("nitrous"), 10, "AB nitrous valve closed")
    st.sleep(2)
    m1 = (st.latest("LOADCELL_50_WEIGHT_KG", "DAQ") or {}).get("values", [0])[0] or 0
    check("nitrous fill: DAQ load cell and VB tank PT follow the fill", filled and m1 > m0 + 1 and (vb_psi(st) or 0) > 300,
          load_cell_before=m0, load_cell_after=m1, tank_psi=vb_psi(st), plant=st.plant().get("tank"))

def arm_and_launch(st: Station, check):
    state = None
    if st.mode == "test_fire":
        # a test fire has no FC to arm: the station never publishes Armed there (gse/operational_policy.rs); the
        # qualification's mission stage is Armed once the fill reached its target, and Launch goes from Idle
        stage = st.wait(lambda: (gse(st).get("qualification") or {}).get("fill_target"), 30, "qualification Armed")
        check("GSE qualification reaches Armed (fill target)", stage, qualification=gse(st).get("qualification"))
    for _ in range(20 if st.mode != "test_fire" else 0):
        code, f = st.api("/flightstate")
        state = (f or {}).get("state") if isinstance(f, dict) else None
        if state == "Armed":
            break
        # HITL steps the flight state; other modes reach Armed from the fill itself
        code, _ = st.command("AdvanceFlightState") if st.mode == "hitl" else (0, None)
        st.sleep(3)
    if st.mode != "test_fire":
        check("station and boards reach Armed", state == "Armed", state=state, fc=fc_phase(st))
    if st.mode == "normal":
        return normal_launch(st, check)
    launched = False
    for _ in range(8):
        st.api("/api/board_abort/refresh", {})
        st.sleep(2)
        code, body = st.command("Launch")
        if code == 200:
            launched = True
            break
        if "etract" in str(body):
            st.command("RetractPlumbing")
            st.sleep(6)
    check("Launch accepted by the station", launched)
    return launched


def normal_launch(st: Station, check):
    """Normal mode after the fill (FirmwareSimulator test-operational-flight-sim.py): sign the checkout checklist,
    retract the QD (RetractPlumbing, refreshing the actuator readback until the station reports QD motion), then
    Launch once every required board's state proof and the retraction proof are fresh (/api/board_abort)."""
    code, doc = st.api("/api/checklists")
    if code == 200 and isinstance(doc, dict):
        code, body = st.api("/api/checklists/sign", {"checklist_id": "checkout", "revision": doc.get("revision")})
        st.log("checklist sign-off", http=code, body=body)

    def retracting():
        g = gse(st)
        if g.get("qd") not in (None, "NotStarted"):
            return g.get("qd")
        st.api("/api/actuator/plumbing/query", {})
        st.command("RetractPlumbing")
        return None
    qd = st.wait(retracting, 90, "QD retract dispatched", poll=3)
    check("the QD retracts (RetractPlumbing through the AB)", qd, qd=qd, umbilical=st.plant().get("umbilical_attached"))
    launched = [None]
    last_refresh = [st.now() - 30]

    def launch():
        if st.now() - last_refresh[0] >= 10:
            last_refresh[0] = st.now()
            st.api("/api/board_abort/refresh", {})
        code, snap = st.api("/api/board_abort")
        if code != 200 or not isinstance(snap, dict):
            return None
        missing = snap.get("missing_required_state") or []
        retraction = snap.get("retraction") or {}
        if missing or not retraction.get("allowed"):
            if not missing:
                st.api("/api/actuator/plumbing/query", {})
            launched[0] = {"missing": missing, "retraction": retraction.get("phase")}
            return None
        code, body = st.command("Launch")
        launched[0] = {"http": code, "body": body}
        return code == 200
    # the AB drives the stepper the full retract (ActuatorBoard26 main_task.c: 200000 motor steps x 100 gearbox at
    # 30 us, 600 s) before it reports the retraction complete
    ok = st.wait(launch, 900, "Launch admitted", poll=2)
    check("Launch accepted by the station", ok, last=launched[0])
    return bool(ok)


def flight(st: Station, check, result):
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
        st.sleep(1)
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



def scenario_hitl(st, check, result):
    boards_up(st, check)
    fill(st, check)
    arm_and_launch(st, check)
    flight(st, check, result)


def scenario_testing(st, check, result):
    # testing mode's own logic against the simulated boards: the links and the fill rules (its launch flow is not
    # driven here)
    boards_up(st, check)
    # the station declares its Startup FLIGHT_STATE in Testing, so the DAQ allows DAC output here too
    dac_patch(st, check, result)
    fill(st, check)


def dac_patch(st: Station, check, result, frequency_hz=20.0, target=None, attenuation=None):
    """The DAQ's DAC waveform engine through the simulated loopback cable: a sine on DAC channel 0 patched onto a DAQ
    input (default the KG1000 amplifier through a 1/1000 attenuator), measured by the DAQ's frame engine and judged by
    the station's DAC validation, then the station's linearity sweep. (The MCP CH2-CH7 auxiliary inputs publish NaN
    in the sim: docs/firmware-sim.md, DAC patch.)"""
    target = target or os.environ.get("FIRMWARE_SIM_E2E_DAC_TARGET", "loadcell_kg1000")
    attenuation = attenuation if attenuation is not None else float(os.environ.get(
        "FIRMWARE_SIM_E2E_DAC_ATTENUATION", "0.001" if target.startswith("loadcell") else "1.0"))
    if target.startswith("loadcell"):
        # the load-cell items are 20 ms bin means (50 Hz): a 20 Hz sine would read sinc(0.4 pi) = 0.76 of its
        # amplitude, so stay well below a quarter of the stream rate
        frequency_hz = min(frequency_hz, 2.0)
    st.api("/api/firmware-sim/control", {"patch": {"dac1": target, "dac1_attenuation": attenuation}})
    config = {"channel": 0, "waveform": "sine", "amplitude_v": 1.0, "offset_v": 1.5, "frequency_hz": frequency_hz}

    def configured():
        # the DAQ refuses DAC output until it knows the flight state (shortly after the network comes up)
        code, body = st.api("/api/daq/dac/config", config)
        st.log("DAC config", http=code, body=body)
        return code in (200, 202)
    st.wait(configured, 90, "DAC output accepted", poll=5)
    code, body = st.api("/api/daq/dac/validation", {"target": target, "channel": 0, "attenuation": attenuation})
    st.log("DAC validation", http=code, body=body)

    def analysed():
        code, v = st.api("/api/daq/dac/validation")
        a = (v or {}).get("analysis") if code == 200 and isinstance(v, dict) else None
        return v if a and (a.get("samples") or 0) >= 500 and a.get("measured_frequency_hz") else None
    v = st.wait(analysed, 60, "DAC validation analysis", poll=2) or {}
    a = v.get("analysis") or {}
    result["dac"] = {"analysis": a, "warnings": v.get("warnings")}
    f = a.get("measured_frequency_hz") or 0
    amp, want = a.get("measured_amplitude") or 0, a.get("expected_amplitude") or 0
    check("DAC sine plays through the patch into the DAQ (frequency and amplitude)",
          abs(f - frequency_hz) < 0.05 * frequency_hz and want and abs(amp - want) < 0.1 * want,
          analysis=a, warnings=v.get("warnings"))
    st.api("/api/daq/dac/disable", {"channel": 0})
    # the station's validation sweep (linearity: DC steps through the patch, judged against the target's model)
    code, body = st.api("/api/daq/dac/sweep", {"target": target, "channel": 0, "attenuation": attenuation, "steps": 11, "dwell_ms": 1000})
    st.log("DAC sweep", http=code, body=body)

    def swept():
        code, v = st.api("/api/daq/dac/sweep")
        if code != 200 or not isinstance(v, dict):
            return None
        return v if str(v.get("state", "")).lower() in ("done", "failed", "cancelled") else None
    sw = st.wait(swept, 180, "DAC validation sweep", poll=3) or {}
    rep = sw.get("report") or {}
    result["dac_sweep"] = {k: rep.get(k) for k in ("pass", "offset_error", "gain_error_pct", "max_nonlinearity_pct_fs", "noise_rms", "checks", "message")}
    check("the DAC validation sweep passes through the patch", rep.get("pass") is True, state=sw.get("state"),
          sweep_message=sw.get("message"), **result["dac_sweep"])
    st.api("/api/daq/dac/disable", {"channel": 0})
    st.api("/api/firmware-sim/control", {"patch": {"dac1": None}})


def peak(st: Station, data_type: str, seconds: float, index=0):
    best = None
    end = st.now() + seconds
    while st.now() < end:
        for r in st.recent():
            if r.get("data_type") == data_type and r.get("values") and r["values"][index] is not None:
                v = r["values"][index]
                best = v if best is None else max(best, v)
        st.sleep(1)
    return best


def gse(st: Station):
    code, g = st.api("/api/gse/status")
    return g if code == 200 and isinstance(g, dict) else {}


def command_accepted(st: Station, name: str, timeout_s=60):
    """Sends a GSE command until the station accepts it (an interlock such as a fresh tank PT sample can refuse it
    for a moment after start-up); the refusals stay in the log."""
    return st.wait(lambda: st.command(name)[0] == 200, timeout_s, f"{name} accepted", poll=3)


def gse_procedure(st: Station, check, result):
    """The operator's GSE procedure: valve self-test, nitrogen test (dump at its hold), fill to the target; the
    station goes Armed when the fill target is reached (FirmwareSimulator test-operational-flight-sim.py gse_step)."""
    ready = st.wait(lambda: gse(st).get("topology_stable") and gse(st).get("phase") in ("idle", "cancelled", None) and gse(st), 120, "GSE topology stable", poll=2)
    check("GSE topology stable", ready, phase=gse(st).get("phase"))
    # a dry valve self-test: the operator isolates the gas supplies first (the confirmation says so), then opens them
    st.api("/api/firmware-sim/control", {"plant": {"nitrogen_bottle_open": False, "nitrous_bottle_open": False}})
    st.api("/api/gse/self-test-confirmation", {"confirmed": True})
    command_accepted(st, "ValveSelfTest")
    ok = st.wait(lambda: (gse(st).get("qualification") or {}).get("self_test"), 180, "valve self-test", poll=2)
    check("valve self-test passes through the real VB/AB", ok, phase=gse(st).get("phase"), message=gse(st).get("message"))
    st.api("/api/firmware-sim/control", {"plant": {"nitrogen_bottle_open": True, "nitrous_bottle_open": True}})
    command_accepted(st, "NitrogenTest")
    def n2():
        g = gse(st)
        if (g.get("qualification") or {}).get("nitrogen_test") and g.get("phase") == "passed":
            return g
        live = [c for c in (g.get("sequence_progress") or {}).get("controls", []) if c.get("hold_choice")]
        if live and g.get("phase") == "paused" and (live[0]["hold_choice"].get("dump") or {}).get("allowed"):
            st.api("/api/gse/sequences/dump", live[0]["key"])
        return None
    ok = st.wait(n2, 400, "nitrogen test", poll=2)
    check("nitrogen test passes (pressure steps on the plant, dump at the hold)", ok, phase=gse(st).get("phase"), message=gse(st).get("message"))
    command_accepted(st, "StartFill")
    last_resume = [st.now()]
    def filled():
        g = gse(st)
        if (g.get("qualification") or {}).get("fill_target"):
            return g
        stalled = g.get("phase") == "paused" and "below target" in str(g.get("message"))
        # Start fill first zeroes the fill load cell; a preparation that expired (a notification asks to press Fill
        # again) leaves the sequence where it was, so press it again
        not_started = g.get("phase") == "passed" and not (g.get("qualification") or {}).get("filling_started")
        if (stalled and st.now() - last_resume[0] > 5) or (not_started and st.now() - last_resume[0] > 30):
            last_resume[0] = st.now()
            st.command("StartFill")
        return None
    ok = st.wait(filled, 600, "fill target", poll=2)
    check("fill reaches its target through the station's own fill sequence", ok, phase=gse(st).get("phase"),
          message=gse(st).get("message"), plant=st.plant().get("tank"))
    result["gse"] = gse(st)


def extremes(st: Station, seconds: float, specs: dict) -> dict:
    """Over `seconds` of station time, the extreme of each DAQ series in `specs` ({data_type: max|min}) among the
    station's recent rows (the recent window holds the whole burn)."""
    out = {k: None for k in specs}
    end = st.now() + seconds
    while True:
        for r in st.recent():
            k = r.get("data_type")
            if k in specs and r.get("values") and r["values"][0] is not None:
                v = r["values"][0]
                out[k] = v if out[k] is None else specs[k](out[k], v)
        if st.now() >= end:
            return out
        st.sleep(1)


def scenario_test_fire(st, check, result):
    # test fire uses the fill-side boards on the umbilical (the AV bay is not on the stand's radio)
    boards_up(st, check, ["GB", "AB", "VB", "DAQ"])
    # the operator's procedure from an empty tank (the manual fill rules are checked in the other modes)
    gse_procedure(st, check, result)
    kg1000_rest = (st.latest("KG1000", "DAQ") or {}).get("values", [None])[0]
    kg50_rest = (st.latest("LOADCELL_50_WEIGHT_KG", "DAQ") or {}).get("values", [None])[0]
    arm_and_launch(st, check)
    fired = st.wait(lambda: st.plant().get("igniter_fired"), 60, "AB igniter (PB7)")
    check("the AB fires the igniter on its drive enable (PB7)", fired,
          events=[e["event"] for e in st.plant().get("events", [])][-6:])
    lit = st.wait(lambda: st.plant().get("motor", {}).get("burning") or st.plant().get("motor", {}).get("burned_out"), 90, "ignition on the stand")
    check("igniter and pilot light the hybrid motor on the stand", lit, model=st.plant().get("motor", {}).get("model"),
          events=[e["event"] for e in st.plant().get("events", [])][-6:])
    ex = extremes(st, 12, {"COMBUSTION_CHAMBER_PRESSURE": max, "KG1000": max, "LOADCELL_50_WEIGHT_KG": min})
    chamber, kg1000, kg50_min = ex["COMBUSTION_CHAMBER_PRESSURE"], ex["KG1000"], ex["LOADCELL_50_WEIGHT_KG"]
    plant = st.plant()
    result["test_fire"] = {"chamber_psi_peak": chamber, "kg1000_v_peak": kg1000, "kg1000_v_rest": kg1000_rest,
                           "kg50_kg_rest": kg50_rest, "kg50_kg_min": kg50_min, "motor": plant.get("motor")}
    check("DAQ reports the chamber pressure of the burn (chamber PT)", (chamber or 0) > 100, chamber_psi_peak=chamber)
    # the stand: thrust first unloads the KG50 (the hanging rocket and its propellant) ...
    check("the KG50 unloads as the motor lifts the rocket", kg50_min is not None and kg50_rest is not None and kg50_min < kg50_rest - 2,
          rest_kg=kg50_rest, min_kg=kg50_min)
    # ... then the gap closes and the KG1000 carries the thrust
    check("the KG1000 sees the thrust once the gap closes", kg1000 is not None and kg1000_rest is not None and kg1000 > kg1000_rest + 0.5,
          rest_v=kg1000_rest, peak_v=kg1000)
    check("the rocket stays on the stand", plant.get("flight", {}).get("phase") == "pad", phase=plant.get("flight", {}).get("phase"))
    check("the motor burns out", st.wait(lambda: st.plant().get("motor", {}).get("burned_out"), 30, "burnout"))
    burn_report(st, check, result, after_provisional=lambda: return_to_idle(st, check))


def return_to_idle(st: Station, check):
    """After the fire the operator steps the station back to Idle (a test fire has no FC to land): that ends the
    launch capture, and SD downloads (the DAQ launch file for the final burn report) run only in Idle."""
    def state():
        code, f = st.api("/flightstate")
        return (f or {}).get("state") if code == 200 and isinstance(f, dict) else None
    for _ in range(10):
        if state() == "Idle":
            break
        st.command("RewindFlightState")
        st.sleep(3)
    check("the station steps back to Idle after the fire", state() == "Idle", state=state())


def burn_report(st: Station, check, result, after_provisional=None):
    """The station's burn report (thrust, impulse): provisional from the capture of the burn, final once the DAQ's SD
    launch file is merged (the station fetches it after the burn)."""
    def report(final=False):
        code, r = st.api("/api/burn_reports")
        reports = r.get("reports") if code == 200 and isinstance(r, dict) else None
        if not reports:
            return None
        return reports[0] if not final or reports[0].get("status") == "final" else None
    def thrust_report():
        r = report()
        return r if r and (r.get("peak_thrust_n") or 0) > 100 else None
    r = st.wait(thrust_report, 240, "burn report", poll=5) or report()
    check("the station computes the burn report (thrust and impulse)",
          r and (r.get("peak_thrust_n") or 0) > 100 and (r.get("total_impulse_ns") or 0) > 100,
          report={k: (r or {}).get(k) for k in ("status", "peak_thrust_n", "total_impulse_ns", "burn_time_s", "isp_s", "flags")})
    if after_provisional:
        after_provisional()
    final = st.wait(lambda: report(True), 1500, "final burn report (DAQ SD launch file merged)", poll=10)
    code, daq = st.api("/api/sd_download")
    result["burn_report"] = final or report()
    check("the burn report goes final (DAQ SD launch file merged)", final,
          status=(final or report() or {}).get("status"), coverage=(final or report() or {}).get("coverage"),
          daq_sd=daq if code == 200 else code)


def fc_sd_collection(st: Station, check, result, timeout_s=1200):
    """The station's automatic FC SD collection after landing (Landed held 5 s or Recovery): a job is queued, the
    newest irec_LAUNCH file of the flight is listed and downloaded over the link and attached to the capture."""
    def status():
        code, s = st.api("/api/sd_download/fc")
        return s if code == 200 and isinstance(s, dict) else {}
    queued = st.wait(lambda: status().get("jobs") and status(), 90, "FC SD collection job queued", poll=3)
    s = status()
    check("FC SD auto-collection queues a job after landing", queued, enabled=s.get("enabled"),
          job={k: ((s.get("jobs") or [{}])[0]).get(k) for k in ("trigger", "phase", "message", "launch_file")})
    if not queued:
        result["fc_sd"] = s
        return
    last = [st.now() - 60]

    def collected():
        s = status()
        job = (s.get("jobs") or [{}])[0]
        if st.now() - last[0] >= 60:
            last[0] = st.now()
            a = s.get("active") or {}
            st.log("FC SD collection", phase=job.get("phase"), file=job.get("launch_file"), done=a.get("bytes_done"),
                   total=a.get("total_bytes"), bps=a.get("bytes_per_second"), paused=a.get("paused_reason"), message=job.get("message"))
        # downloaded and attached to the capture; the optional 60 s lead-up stage that follows may wait behind
        # other SD transfers (the DAQ's own lead-up) and is not part of this check
        return job if job.get("downloaded_at_ms") and job.get("capture") else None
    job = st.wait(collected, timeout_s, "FC SD launch file collected", poll=5)
    s = status()
    result["fc_sd"] = s
    j = job or (s.get("jobs") or [{}])[0]
    check("FC SD auto-collection downloads the flight's launch file and attaches it", job and job.get("capture"),
          file=j.get("launch_file"), size=j.get("launch_size"), capture=j.get("capture"), crc_errors=j.get("crc_errors"),
          phase=j.get("phase"), message=j.get("message"), active=s.get("active"))


def scenario_normal(st, check, result):
    boards_up(st, check)
    def fix():
        code, a = st.api("/api/antenna/status")
        rocket = (a or {}).get("rocket") if code == 200 and isinstance(a, dict) else None
        return rocket if rocket and not rocket.get("stale") and rocket.get("latitude") is not None else None
    rocket = st.wait(fix, 120, "RF GPS fix at the GS", poll=2)
    check("RF NEO-M9N fix reaches the station (GPS gates)", rocket, fix=rocket)
    gps = st.latest("GPS_STATUS", "RF")
    result["normal"] = {"rocket_fix": rocket, "gps_status": gps}
    # the DAQ DAC through the simulated loopback cable while the station is Idle (before the GSE procedure)
    if os.environ.get("FIRMWARE_SIM_E2E_DAC", "1") != "0":
        dac_patch(st, check, result)
    # the full launch procedure: the station's own GSE procedure to Armed, then QD retract and Launch
    gse_procedure(st, check, result)
    code, pol = st.api("/api/action_policy")
    reason = ""
    if code == 200 and isinstance(pol, dict):
        launch = next((c for c in pol.get("controls", []) if c.get("cmd") == "Launch"), {})
        reason = launch.get("disabled_reason") or ""
    result["normal"]["launch_gate"] = reason
    check("Launch is not held by the GPS gate", "gps" not in reason.lower(), launch_gate=reason)
    if not arm_and_launch(st, check):
        return
    track = []

    def follow():
        code, a = st.api("/api/antenna/status")
        r = (a or {}).get("rocket") if code == 200 and isinstance(a, dict) else None
        if r and r.get("latitude") is not None and not r.get("stale"):
            point = (r["latitude"], r["longitude"], r.get("altitude_m") or r.get("altitude"))
            if not track or track[-1] != point:
                track.append(point)
        return None
    import threading
    stop = threading.Event()
    def poll():
        while not stop.is_set():
            follow()
            stop.wait(1)
    th = threading.Thread(target=poll, daemon=True)
    th.start()
    try:
        flight(st, check, result)
    finally:
        stop.set()
        th.join(5)
    result["normal"]["track"] = track
    import math
    def dist(a, b):
        k = 111_320.0
        return math.hypot((a[0] - b[0]) * k, (a[1] - b[1]) * k * math.cos(math.radians(a[0])))
    drift = dist(track[0], track[-1]) if len(track) > 1 else 0
    alts = [p[2] for p in track if isinstance(p[2], (int, float))]
    check("the map track follows the flight (RF GPS positions at the station)",
          len(track) >= 20 and drift > 50 and (not alts or max(alts) - min(alts) > 300),
          points=len(track), drift_m=round(drift), alt_span_m=round(max(alts) - min(alts)) if alts else None)
    fc_sd_collection(st, check, result)


IGT_POWER_ON_DATASET = "measured_motor"
IGT_SWAP_DATASET = "nominal_10k"


def scenario_igt(st, check, result):
    boards_up(st, check)
    datasets = st.sim().get("igt_datasets") or []
    check("IGT datasets offered", {IGT_POWER_ON_DATASET, IGT_SWAP_DATASET} <= set(datasets), datasets=datasets)

    def igt():
        code, g = st.api("/api/igt")
        return g if code == 200 and isinstance(g, dict) else {}
    # the card holds a dataset from power-on (FIRMWARE_SIM_CONTROL, set by main)
    ready = st.wait(lambda: igt().get("sd_ready") is True and igt(), 90, "FC SD ready at power-on", poll=2)
    check("the FC mounts the card it powered on with", ready, sd_card=st.sim().get("sd_card"), igt=igt())
    check("the FC runs the ground-test image", igt().get("fc_image") == "ground_test", fc_image=igt().get("fc_image"))
    # swap the card for another dataset while the FC runs: out, choose, in, Retry SD (the FC mounts the new card
    # fresh, FlightComputer26 34c7835; Enter then loads its CSVs)
    st.api("/api/firmware-sim/control", {"faults": {"fc_sd_missing": True}})
    st.wait(lambda: igt().get("sd_ready") is False, 20, "FC sees the card out", poll=1)
    st.api("/api/firmware-sim/control", {"igt": {"dataset": IGT_SWAP_DATASET}})
    loaded = st.wait(lambda: (st.sim().get("sd_card") or {}).get("dataset") == IGT_SWAP_DATASET and st.sim()["sd_card"], 20, "card rewritten")
    check("the swapped card carries the other dataset", loaded, sd_card=st.sim().get("sd_card"))
    st.api("/api/firmware-sim/control", {"faults": {}})
    st.sleep(2)
    st.command("FcSdRetry")
    ready = st.wait(lambda: igt().get("sd_ready") is True and not igt().get("sd_retry_pending") and igt(), 60, "FC SD ready after Retry SD", poll=2)
    check("Retry SD mounts the swapped card", ready, igt=igt())

    def enter():
        code, challenge = st.api("/api/igt/prepare", {"action": "enter"})
        ticket = challenge.get("ticket") if isinstance(challenge, dict) else None
        code, body = st.api("/api/igt/confirm", {"ticket": ticket}) if ticket else (code, challenge)
        st.log("IGT enter", http=code, body=body)
    enter()
    # the FC refuses Enter while it still counts the station as lost (a radio path that came back seconds ago:
    # reason 11, "refused (wrong state)"); the operator presses it again
    for _ in range(5):
        g = st.wait(lambda: igt() if str(igt().get("fc_mode", "")).lower() != "off" or igt().get("reason") == 11 else None,
                    20, "FC answers Enter", poll=2) or {}
        if str(g.get("fc_mode", "")).lower() != "off":
            break
        st.sleep(5)
        enter()
    fc_ready = st.wait(lambda: str(igt().get("fc_mode", "")).lower() in ("ready", "fault") and igt(), 60, "FC IGT Ready", poll=2)
    check("FC IGT reaches Ready with the swapped card's CSVs (no parse fault)", fc_ready and str(fc_ready.get("fc_mode")).lower() == "ready",
          igt=igt())
    result["igt"] = igt()
    if not fc_ready or str(fc_ready.get("fc_mode")).lower() != "ready":
        return
    # the stand body: the station's own GSE procedure to Armed (as in test fire)
    gse_procedure(st, check, result)
    if not arm_and_launch(st, check):
        return
    phases, seen = [], {"gps_simulated": False, "dry_run": None, "live_pyro": False}

    def track():
        ph = fc_phase(st)
        g = igt()
        if g.get("gps_simulated"):
            seen["gps_simulated"] = True
        if str(g.get("fc_mode", "")).lower() == "sim":
            # the FC reports both outputs dry-run while it flies the replay (the first report of Sim may predate it)
            if g.get("drogue_dry_run") and g.get("main_dry_run"):
                seen["dry_run"] = True
        seen["live_pyro"] |= bool(g.get("live_pyro"))
        if ph and (not phases or phases[-1] != ph):
            phases.append(ph)
            st.log("FC phase", phase=ph, fc_mode=g.get("fc_mode"), gps_simulated=g.get("gps_simulated"))
        return any(p in ("Landed", "Recovery") for p in phases)
    st.wait(track, 500, "replayed flight landed", poll=1)
    result["igt_phases"] = phases
    check("the FC replays the flight from the CSV (Ascent, Apogee, Landed)",
          any("Ascent" in p or "Coast" in p for p in phases) and any(p in ("Landed", "Recovery") for p in phases), phases=phases)
    flight = st.plant().get("flight", {})
    check("the replay flies with dry-run pyros: no deployment reaches the plant", seen["dry_run"] and not seen["live_pyro"]
          and flight.get("drogue_at_s") is None and flight.get("main_at_s") is None,
          dry_run=seen["dry_run"], live_pyro=seen["live_pyro"], drogue_at_s=flight.get("drogue_at_s"), main_at_s=flight.get("main_at_s"))
    real = st.wait(lambda: seen["gps_simulated"] and not igt().get("gps_simulated") and igt(), 120, "GPS back to real", poll=2)
    check("the FC flew replayed GPS and switches back to real GPS after the replay", real,
          replayed=seen["gps_simulated"], igt=igt())
    # the ground-test image serves its card too (FlightComputer26 ec999d7): collected as after a flight
    fc_sd_collection(st, check, result)
    st.command("IgtExit")
    off = st.wait(lambda: str(igt().get("fc_mode", "")).lower() == "off" and igt(), 60, "IGT exit", poll=2)
    check("IGT exit: the FC leaves ground test (mode Off)", off, igt=igt())


SCENARIOS = {"hitl": scenario_hitl, "testing": scenario_testing, "test_fire": scenario_test_fire, "normal": scenario_normal,
             "integrated_ground_test": scenario_igt}


if __name__ == "__main__":
    sys.exit(main())
