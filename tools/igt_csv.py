#!/usr/bin/env python3
"""Create and check integrated-ground-test CSVs for the FC's SD card.

See docs/integrated-ground-test.md (section 4). The FC replays these files
into its estimator after a real Launch on the test stand:

    IGT/IMU0.CSV   t_ms,ax_mps2,ay_mps2,az_mps2,gx_dps,gy_dps,gz_dps
    IGT/BARO0.CSV  t_ms,alt_m,pressure_pa,temp_c
    IGT/GPS0.CSV   t_ms,lat_deg,lon_deg,alt_msl_m      (optional)

t_ms counts from T0, the FC's acceptance of Launch. Several segments per
sensor (IMU0..IMU9) are read in order.

Commands:
  model      deterministic 1-D flight from a thrust/mass/drag model, with the
             drogue at apogee and the main at the reef altitude
  recording  convert a GS recording CSV export or an FC SD log (irec*.csv)
  check      validate a folder exactly like the FC does before arming

Standard library only.
"""
from __future__ import annotations

import argparse
import csv
import json
import math
import random
import sys
from dataclasses import dataclass
from pathlib import Path

G = 9.80665
HEADERS = {
    'IMU': 't_ms,ax_mps2,ay_mps2,az_mps2,gx_dps,gy_dps,gz_dps',
    'BARO': 't_ms,alt_m,pressure_pa,temp_c',
    'GPS': 't_ms,lat_deg,lon_deg,alt_msl_m',
}
FIELDS = {'IMU': 6, 'BARO': 3, 'GPS': 3}
SEGMENTS = 10
LINE_MAX = 120
MIN_COVER_MS = 5000
REEF_TARGET_ALT = 457.2  # FC fcconfig.h (flight configuration)


@dataclass
class Limits:
    """The FC's validator ranges (Core/Inc/fcconfig.h, flight configuration)."""
    site_lat: float = 31.758
    site_lon: float = -102.046
    site_msl: float = 890.016
    max_acc: float = G * 24.0
    max_dps: float = 2000.0
    min_alt: float = -30.0
    max_alt: float = 4572.0
    min_prs: float = 57200.0
    max_prs: float = 110000.0

    def ok(self, sensor: str, v: list[float]) -> bool:
        if sensor == 'IMU':
            return all(abs(x) <= self.max_acc for x in v[:3]) and all(abs(x) <= self.max_dps for x in v[3:])
        if sensor == 'BARO':
            return self.min_alt <= v[0] <= self.max_alt and self.min_prs <= v[1] <= self.max_prs
        return (abs(v[0] - self.site_lat) <= 1.0 and abs(v[1] - self.site_lon) <= 1.0 and
                self.site_msl - 100.0 <= v[2] <= self.site_msl + self.max_alt)


def isa_pressure(msl_m: float) -> float:
    return 101325.0 * (1.0 - 2.25577e-5 * msl_m) ** 5.25588


def isa_density(msl_m: float) -> float:
    temp = 288.15 - 0.0065 * msl_m
    return isa_pressure(msl_m) / (287.05 * temp)


def fmt(x: float, places: int) -> str:
    text = f'{x:.{places}f}'
    if '.' in text:
        text = text.rstrip('0').rstrip('.')
    return '0' if text in ('-0', '') else text


# ---------------------------------------------------------------- model

@dataclass
class Model:
    thrust_n: float = 2200.0
    burn_s: float = 3.5
    wet_kg: float = 28.0
    dry_kg: float = 22.0
    cd: float = 0.55
    diameter_m: float = 0.157
    drogue_mps: float = 25.0
    main_mps: float = 6.0
    reef_alt_m: float = REEF_TARGET_ALT
    wind_mps: float = 4.0
    liftoff_ms: int = 10000
    rest_after_s: float = 30.0
    site_lat: float = 31.758
    site_lon: float = -102.046
    site_msl: float = 890.016
    noise: float = 0.0
    seed: int = 1
    wind_from_deg: float = 300.0   # meteorological: the direction the wind blows from
    tilt_deg: float = 5.0          # launch rail angle from vertical
    azimuth_deg: float = 60.0      # downrange bearing of the rail tilt

    def fly(self, step_ms: int = 1):
        """Yield (t_ms, h, v, specific_force_z, north_m, east_m, apogee) at 1 ms.

        Altitude carries the motion; the ground track moves less: downrange
        along the rail tilt during ascent, then with the wind under canopy."""
        area = math.pi * (self.diameter_m / 2.0) ** 2
        dt = step_ms / 1000.0
        h = v = north = east = 0.0
        tilt = math.tan(math.radians(self.tilt_deg))
        az = math.radians(self.azimuth_deg)
        to = math.radians(self.wind_from_deg + 180.0)
        t = 0
        phase = 'pad'
        landed_at = None
        apogee = 0.0
        while True:
            if phase == 'pad':
                force = G
                if t >= self.liftoff_ms:
                    phase = 'boost'
            if phase in ('boost', 'coast'):
                tb = (t - self.liftoff_ms) / 1000.0
                thrust = self.thrust_n if tb < self.burn_s else 0.0
                mass = self.wet_kg - (self.wet_kg - self.dry_kg) * min(tb / self.burn_s, 1.0)
                drag = 0.5 * isa_density(self.site_msl + h) * v * abs(v) * self.cd * area
                force = (thrust - drag) / mass
                a = force - G
                if tb >= self.burn_s:
                    phase = 'coast'
                v += a * dt
                h += v * dt
                if v > 0.0:
                    north += v * tilt * math.cos(az) * dt
                    east += v * tilt * math.sin(az) * dt
                if phase == 'coast' and v <= 0.0:
                    phase = 'drogue'
                    apogee = h
                if h < 0.0 and phase == 'boost':
                    h = v = 0.0  # still on the rail
                    force = G
            elif phase in ('drogue', 'main'):
                target = -(self.drogue_mps if phase == 'drogue' else self.main_mps)
                a = (target - v) * 1.5  # first-order approach to the canopy's rate
                force = a + G
                v += a * dt
                h += v * dt
                north += self.wind_mps * math.cos(to) * dt
                east += self.wind_mps * math.sin(to) * dt
                if phase == 'drogue' and h <= self.reef_alt_m:
                    phase = 'main'
                if h <= 0.0:
                    h = v = 0.0
                    phase = 'landed'
                    landed_at = t
            elif phase == 'landed':
                force = G
                if t - landed_at >= self.rest_after_s * 1000.0:
                    return
            yield t, h, v, force, north, east, apogee
            t += step_ms

    def rows(self, imu_hz: int, baro_hz: int, gps_hz: int):
        rng = random.Random(self.seed)
        n = self.noise
        imu_step, baro_step, gps_step = 1000 // imu_hz, 1000 // baro_hz, 1000 // gps_hz
        out = {'IMU': [], 'BARO': [], 'GPS': []}
        summary = {'apogee_m': 0.0, 'landed_ms': 0}
        lat_m = 111320.0
        lon_m = 111320.0 * math.cos(math.radians(self.site_lat))
        summary['track_m'] = 0.0
        for t, h, v, force, north, east, apogee in self.fly():
            summary['apogee_m'] = max(summary['apogee_m'], h)
            summary['landed_ms'] = t
            if t % imu_step == 0:
                out['IMU'].append([t, rng.gauss(0, 0.05 * n), rng.gauss(0, 0.05 * n),
                                   force + rng.gauss(0, 0.1 * n),
                                   rng.gauss(0, 0.2 * n), rng.gauss(0, 0.2 * n), rng.gauss(0, 0.2 * n)])
            if t % baro_step == 0:
                alt = h + rng.gauss(0, 0.3 * n)
                msl = self.site_msl + h
                out['BARO'].append([t, alt, isa_pressure(msl), 15.0 - 0.0065 * msl])
            if t % gps_step == 0:
                out['GPS'].append([t, self.site_lat + north / lat_m, self.site_lon + east / lon_m,
                                   self.site_msl + h + rng.gauss(0, 1.0 * n)])
                summary['track_m'] = max(summary['track_m'], math.hypot(north, east))
        return out, summary


def tune_thrust(model: Model, apogee_m: float) -> Model:
    lo, hi = 50.0, 20000.0
    for _ in range(40):
        model.thrust_n = (lo + hi) / 2.0
        top = 0.0
        for _, h, v, _, _, _, _ in model.fly(step_ms=5):
            top = max(top, h)
            if v < 0 and h > 1:
                break
        if top < apogee_m:
            lo = model.thrust_n
        else:
            hi = model.thrust_n
    return model


# ---------------------------------------------------------------- writing

def row_text(sensor: str, row: list[float]) -> str:
    t = int(row[0])
    if sensor == 'IMU':
        vals = [fmt(x, 4) for x in row[1:]]
    elif sensor == 'BARO':
        vals = [fmt(row[1], 3), fmt(row[2], 1), fmt(row[3], 2)]
    else:
        vals = [fmt(row[1], 7), fmt(row[2], 7), fmt(row[3], 2)]
    return f'{t},' + ','.join(vals)


def write_set(out: Path, rows: dict[str, list[list[float]]], segment_s: float | None = None) -> None:
    folder = out / 'IGT' if out.name.upper() != 'IGT' else out
    folder.mkdir(parents=True, exist_ok=True)
    for old in folder.glob('*.CSV'):
        old.unlink()
    for sensor, data in rows.items():
        if not data:
            continue
        segments: list[list[list[float]]] = [[]]
        for row in data:
            if segment_s and segments[-1] and row[0] >= segment_s * 1000 * len(segments):
                segments.append([])
            segments[-1].append(row)
        if len(segments) > SEGMENTS:
            raise SystemExit(f'{sensor}: {len(segments)} segments; the FC reads at most {SEGMENTS}')
        for index, seg in enumerate(segments):
            text = HEADERS[sensor] + '\n' + '\n'.join(row_text(sensor, r) for r in seg) + '\n'
            (folder / f'{sensor}{index}.CSV').write_text(text, newline='\n')
    print(f'wrote {folder}')


# ---------------------------------------------------------------- check

def parse_number(text: str, integer: bool) -> float | None:
    """The FC's parser: [+-]digits[.digits], 9 significant digits, no exponent."""
    i, neg = 0, False
    if not integer and text[:1] in '+-':
        neg, i = text[0] == '-', 1
    mantissa = digits = 0
    frac = -1
    while i < len(text):
        c = text[i]
        if c.isdigit():
            if digits < 9:
                mantissa = mantissa * 10 + int(c)
                digits += 1
                if frac >= 0:
                    frac += 1
            elif frac < 0:
                return None
        elif c == '.' and not integer and frac < 0:
            frac = 0
        else:
            return None
        i += 1
    if digits == 0:
        return None
    value = mantissa / (10 ** frac) if frac > 0 else float(mantissa)
    return -value if neg else value


def check_set(folder: Path, limits: Limits) -> list[str]:
    folder = folder / 'IGT' if (folder / 'IGT').is_dir() else folder
    problems, report = [], []
    for sensor in ('IMU', 'BARO', 'GPS'):
        rows = last = 0
        for seg in range(SEGMENTS):
            path = folder / f'{sensor}{seg}.CSV'
            if not path.exists():
                if seg == 0 and sensor != 'GPS':
                    problems.append(f'{path.name}: file missing')
                break
            header_seen = False
            for number, raw in enumerate(path.read_bytes().split(b'\n'), 1):
                line = raw.rstrip(b'\r').decode('ascii', 'replace')
                if len(line) > LINE_MAX:
                    problems.append(f'{path.name}:{number} parse error (line longer than {LINE_MAX})')
                    break
                if not line or line.startswith('#'):
                    continue
                if not header_seen:
                    if line != HEADERS[sensor]:
                        problems.append(f'{path.name}:{number} bad header')
                        break
                    header_seen = True
                    continue
                parts = line.split(',')
                if len(parts) != FIELDS[sensor] + 1:
                    problems.append(f'{path.name}:{number} parse error')
                    break
                t = parse_number(parts[0], True)
                vals = [parse_number(p, False) for p in parts[1:]]
                if t is None or any(v is None for v in vals):
                    problems.append(f'{path.name}:{number} parse error')
                    break
                if rows and t <= last:
                    problems.append(f'{path.name}:{number} time not increasing')
                    break
                if not limits.ok(sensor, vals):
                    problems.append(f'{path.name}:{number} value out of range')
                    break
                rows, last = rows + 1, t
            if not header_seen and not any(p.startswith(path.name) for p in problems):
                problems.append(f'{path.name}:0 bad header')
        if rows or sensor != 'GPS':
            if rows < 2 or (sensor != 'GPS' and last < MIN_COVER_MS):
                problems.append(f'{sensor}: too short ({rows} rows to {last} ms)')
        report.append(f'{sensor} {rows} rows to {last / 1000:.1f} s')
    print('; '.join(report))
    return problems


# ---------------------------------------------------------------- recording

FC_FLIGHT_STATE_LAUNCH = 7


def floats(text: str) -> list[float]:
    text = text.strip()
    if not text:
        return []
    if text.startswith('['):
        value = json.loads(text)
        return [float(x) for x in value] if isinstance(value, list) else []
    return [float(x) for x in text.replace(',', ' ').split()]


def from_recording(path: Path, t0: str, imu_sender: str, gps_sender: str) -> dict[str, list[list[float]]]:
    """GS generic export (data_type/values_json) or FC SD log (sensor/value)."""
    with path.open(newline='') as handle:
        reader = csv.DictReader(handle)
        fields = reader.fieldnames or []
        samples: dict[str, list[tuple[float, list[float]]]] = {'IMU': [], 'BARO': [], 'GPS': []}
        launch = None
        sd_log = 'sensor' in fields
        for row in reader:
            if sd_log:
                stamp = float(row['monotonic_ms'])
                sensor, value = row['sensor'], floats(row['value'])
                if sensor == 'launch_accepted' and launch is None:
                    launch = stamp
                elif sensor == 'IMU' and len(value) == 6:
                    samples['IMU'].append((stamp, value))
                elif sensor == 'BARO' and len(value) == 3:
                    samples['BARO'].append((stamp, value))
                elif sensor == 'SIM' and len(value) == 8 and int(value[0]) in (1, 2, 3):
                    kind = {1: 'IMU', 2: 'BARO', 3: 'GPS'}[int(value[0])]
                    samples[kind].append((stamp, value[2:2 + FIELDS[kind]]))
                continue
            stamp = float(row.get('source_timestamp_ms') or row['received_timestamp_ms'])
            dtype, sender = row['data_type'], row.get('sender_id', '')
            value = floats(row.get('values_json', ''))
            if dtype == 'FLIGHT_STATE' and value and int(value[0]) == FC_FLIGHT_STATE_LAUNCH and launch is None:
                launch = stamp
            elif dtype == 'IMU_DATA' and len(value) == 6 and sender in (imu_sender, ''):
                samples['IMU'].append((stamp, value))
            elif dtype == 'BAROMETER_DATA' and len(value) >= 3 and sender in (imu_sender, ''):
                samples['BARO'].append((stamp, value[:3]))
            elif dtype == 'GPS_DATA' and len(value) >= 3 and sender in (gps_sender, ''):
                samples['GPS'].append((stamp, value[:3]))
            elif dtype == 'SIM_SAMPLE' and len(value) == 8 and int(value[0]) in (1, 2, 3):
                kind = {1: 'IMU', 2: 'BARO', 3: 'GPS'}[int(value[0])]
                samples[kind].append((stamp, value[2:2 + FIELDS[kind]]))
    if t0 == 'launch':
        if launch is None:
            raise SystemExit('no Launch in the recording; pass --t0 <timestamp_ms>')
        origin = launch
    elif t0 == 'first':
        origin = min(s[0][0] for s in samples.values() if s)
    else:
        origin = float(t0)
    out: dict[str, list[list[float]]] = {}
    for sensor, data in samples.items():
        rows, last = [], -1
        for stamp, value in sorted(data, key=lambda x: x[0]):
            t = int(round(stamp - origin))
            if t < 0 or t <= last:
                continue
            rows.append([t, *value[:FIELDS[sensor]]])
            last = t
        out[sensor] = rows
    return out


# ---------------------------------------------------------------- CLI

def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='cmd', required=True)

    m = sub.add_parser('model', help='generate a flight from a parametric model')
    m.add_argument('--out', type=Path, required=True)
    m.add_argument('--apogee-m', type=float, help='tune thrust to reach this apogee')
    m.add_argument('--thrust-n', type=float, default=Model.thrust_n)
    m.add_argument('--burn-s', type=float, default=Model.burn_s)
    m.add_argument('--wet-kg', type=float, default=Model.wet_kg)
    m.add_argument('--dry-kg', type=float, default=Model.dry_kg)
    m.add_argument('--cd', type=float, default=Model.cd)
    m.add_argument('--diameter-m', type=float, default=Model.diameter_m)
    m.add_argument('--drogue-mps', type=float, default=Model.drogue_mps)
    m.add_argument('--main-mps', type=float, default=Model.main_mps)
    m.add_argument('--reef-alt-m', type=float, default=Model.reef_alt_m)
    m.add_argument('--wind-mps', type=float, default=Model.wind_mps)
    m.add_argument('--wind-from-deg', type=float, default=Model.wind_from_deg, help='bearing the wind blows from (default 300)')
    m.add_argument('--tilt-deg', type=float, default=Model.tilt_deg, help='launch rail angle from vertical (default 5)')
    m.add_argument('--azimuth-deg', type=float, default=Model.azimuth_deg, help='downrange bearing of the tilt (default 60)')
    m.add_argument('--liftoff-ms', type=int, default=Model.liftoff_ms)
    m.add_argument('--rest-after-s', type=float, default=Model.rest_after_s)
    m.add_argument('--site', default='31.758,-102.046,890.016', help='lat,lon,msl of the replayed pad')
    m.add_argument('--noise', type=float, default=0.0, help='sensor noise scale (0 = none)')
    m.add_argument('--seed', type=int, default=1)
    m.add_argument('--imu-hz', type=int, default=100)
    m.add_argument('--baro-hz', type=int, default=50)
    m.add_argument('--gps-hz', type=int, default=10)
    m.add_argument('--no-gps', action='store_true')
    m.add_argument('--segment-s', type=float, help='split each sensor into segments of this length')

    r = sub.add_parser('recording', help='convert a GS recording export or an FC SD log')
    r.add_argument('csv', type=Path)
    r.add_argument('--out', type=Path, required=True)
    r.add_argument('--t0', default='launch', help="'launch' (default), 'first' or a timestamp in ms")
    r.add_argument('--fc-sender', default='FC')
    r.add_argument('--gps-sender', default='RF')
    r.add_argument('--segment-s', type=float)

    c = sub.add_parser('check', help='validate a folder like the FC does')
    c.add_argument('folder', type=Path)

    for p in (m, r, c):
        p.add_argument('--fc-site', default=None,
                       help="the FC's launch site lat,lon,msl: the station's Launch site setting, else the "
                            "compiled 31.758,-102.046,890.016 (model: defaults to --site)")
    args = parser.parse_args(argv)
    if args.cmd == 'model':
        lat, lon, msl = (float(x) for x in args.site.split(','))
        model = Model(args.thrust_n, args.burn_s, args.wet_kg, args.dry_kg, args.cd, args.diameter_m,
                      args.drogue_mps, args.main_mps, args.reef_alt_m, args.wind_mps, args.liftoff_ms,
                      args.rest_after_s, lat, lon, msl, args.noise, args.seed,
                      args.wind_from_deg, args.tilt_deg, args.azimuth_deg)
        if args.apogee_m:
            tune_thrust(model, args.apogee_m)
        rows, summary = model.rows(args.imu_hz, args.baro_hz, args.gps_hz)
        if args.no_gps:
            rows['GPS'] = []
        write_set(args.out, rows, args.segment_s)
        print(f"thrust {model.thrust_n:.0f} N, apogee {summary['apogee_m']:.0f} m, "
              f"ground track {summary['track_m']:.0f} m, "
              f"landed and rested by t={summary['landed_ms'] / 1000:.1f} s")
    elif args.cmd == 'recording':
        rows = from_recording(args.csv, args.t0, args.fc_sender, args.gps_sender)
        write_set(args.out, rows, args.segment_s)
    folder = args.out if args.cmd != 'check' else args.folder
    fc_site = args.fc_site or (args.site if args.cmd == 'model' else '31.758,-102.046,890.016')
    lat, lon, msl = (float(x) for x in fc_site.split(','))
    limits = Limits(lat, lon, msl)
    problems = check_set(folder, limits)
    for p in problems:
        print('IGT ' + p, file=sys.stderr)
    return 1 if problems else 0


if __name__ == '__main__':
    sys.exit(main())
