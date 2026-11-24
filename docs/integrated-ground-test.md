# Integrated ground test

An integrated ground test (IGT) is a real fill and a real motor fire on the
stand with the whole rocket powered as for flight. Two bodies take part: the
motor with its tank and plumbing is on the test stand (VB, AB, GB, DAQ, the
load cells, the chamber PT and the umbilical/QD behave as in a test fire), and
the rocket's avionics (FC, RF, PB) lie separately on the ground, without the
motor, linked to the station over the radio. No thrust or vibration reaches
the FC's real sensors: they read a vehicle at rest (often lying horizontally),
the ambient pressure and the real GPS at the site. After Launch the flight
computer (FC) stops using its real flight sensors and replays a recorded or
generated flight from CSV files on its SD card. Its real Kalman filter, state
machine and deployment logic run on that replayed data, and the drogue and
main events reach the real pyro channels. By default those
channels are dry-run (logged, not fired). With LIVE PYRO armed they fire.

The test shows that the fill, the fire, the flight software and the recovery
hardware work together, without a launch.

Consequences of the separate avionics:

- The FC's barometer drift tracker keeps tracking through the fire (its real
  IMU is quiet) and freezes only at the FC's Launch, as in flight.
- The rest checks at Launch see a vehicle at rest and pass.
- FC, RF and PB reach the station over the radio, never the umbilical; nothing
  in IGT depends on the FC being on the umbilical or the continuity loop. The VB
  vent countdown keys off the station link and the stand's continuity wire,
  which stays intact.
- The pyro outputs fire into the separate rocket's recovery hardware (or are
  logged in DRY-RUN).

| Piece | Where |
| --- | --- |
| GS station mode `integrated_ground_test` | GroundStation26 `backend/src/runtime_mode.rs`, `station_mode*.rs` |
| FC replay, mode and pyro gate | FlightComputer26 `Core/Src/ground_test.c`, `Core/Inc/ground_test.h` |
| FC build option | `INTEGRATED_GROUND_TEST` (CMake), `--ground-test` (build.py); OFF by default |
| CSV tools | GroundStation26 `tools/igt_csv.py` |
| Native fast-sim scenario | FirmwareSimulator `scripts/test-full-network.py --scenario igt_flow` |

## 1. Hardware and channel mapping

The FC has exactly two pyro outputs, called **drogue** and **main** everywhere
in the UI and the docs:

| Event | FC command | Output | Flight trigger |
| --- | --- | --- | --- |
| Drogue | `Deploy_Parachute` | CO2, PB5 | `detect_descent` after apogee |
| Main | `Expand_Parachute` | REEF, PB6 | `detect_reefing` below `REEF_TARGET_ALT` |

## 2. What runs for real and what is replayed

| | Normal flight | Test fire | Integrated ground test |
| --- | --- | --- | --- |
| GSE fill (VB, AB, GB, DAQ) | real | real | real |
| Launch goes to | FC + VB | VB only | FC + VB |
| VB pilot / AB igniter | real | real | real |
| FC, PB, RF present and required | yes | no (hidden) | yes |
| FC sensors before Launch | real | n/a | real |
| FC IMU / baro / GPS after Launch | real | n/a | replayed from SD CSV until landed, then real |
| FC estimator, state machine, deploy decisions | real | n/a | real code, replayed inputs |
| FC pyro outputs | real | n/a | dry-run (default) or LIVE PYRO |
| GS launch GPS-fix gate | required | not required | not required |
| Mission phase shows InFlight / Landed | yes | collapsed to Launch | yes, labelled SIMULATED |

## 3. Mode state machines

### 3.1 GS

`integrated_ground_test` is a fifth station mode (wire code 4 wherever a mode
code is sent; Normal=0, Hitl=1, TestFire=2, Testing=3). It is chosen through
the existing Settings > Station prepare/confirm switch. Only an Admin, or an
operator allowed the `StationMode` command, can switch, and only while the
station is idle. The selection persists in `selection.json` like the other
modes. The GS treats the mode as:

- fill/fire like `test_fire`: the fill sequences, turbocharge, QD and
  overpressure handling, and the automatic test-fire CSV export on recording
  close. The **mock wet dress** setting (section 7.2) switches the fill to the
  flight steps instead;
- the AV bay like `normal`: FC, RF and PB are required and visible, FC
  warnings and refusal notices are shown, and the estimator is expected;
- Launch like `normal` (`send_flight_computer_launch`: FC Launch and VB
  Sequence), except that it needs no RF GPS fix and needs the FC to report IGT
  `Ready` (below).

### 3.2 FC

The FC mode lives in RAM only. It is never written to flash, the watchdog
checkpoint or the SD card. Any reset or power cycle starts in `Off`.

```
             IgtEnter (Startup, no Postinit/Launch, GS heard, flight not admitted)
   Off ───────────────────────────────────────────▶ Validating
    ▲                                                 │  SD task scans every CSV
    │ IgtExit (not Sim)                               ├─ error ─▶ Fault(code) ── IgtExit / IgtEnter
    │ Stop / Abort / reset to Startup                 ▼
    │ any reset or power cycle                      Ready  (rings primed, dry-run unless IgtLivePyro)
    │                                                 │  Launch accepted AND stationary
    │                                                 ├─ not stationary ─▶ Fault(8), real sensors
    │                                                 ▼
    │                                                Sim   (IMU/baro/GPS replayed, t = now − T0)
    │                                                 │  FC reaches Landed, or data ended + 30 s
    │                                                 ▼
    └──────────────────────────────────────────────  Done  (real sensors, real GPS)
```

| Command | Byte (compat) | `fc_msg` | Accepted when | Effect |
| --- | --- | --- | --- | --- |
| `IgtEnter` | 29 | `Actionable_Decrees + 17` | FC `Startup`, no Postinit/Launch request, GS not lost, no admitted flight, mode Off/Fault/Done | `Validating`, dry-run |
| `IgtLivePyro` | 30 | `+ 18` | mode `Ready`, no Launch request | LIVE PYRO |
| `IgtDryRun` | 31 | `+ 19` | always (it only makes things safer) | dry-run |
| `IgtExit` | 32 | `+ 20` | any mode except `Sim` | `Off` |
| `FcSdRetry` | 33 | `+ 21` | always (both images) | re-attempt the SD mount and log file; in the ground-test image a `Fault` with reason 1 or 2 re-runs the CSV validation |

A flight build (option OFF) ignores bytes 29–32 and never sets the
"supported" bit, so the GS refuses to send them. Both images accept byte 33.

`IgtEnter` while the SD card is not ready (section 7.5) faults at once with
reason 1 instead of waiting in `Validating`.

The FC refuses replayed data unless all of these hold:

1. the mode was entered from `Startup`, before Postinit and Launch;
2. the CSV set validated (`Ready`) before Launch;
3. at Launch acceptance the latest real samples show the vehicle at rest:
   |a| within g ± 1.5 m/s², every gyro axis below 15 °/s, and |baro alt|
   below 20 m;
4. no reset has happened since step 1.

A rejected check leaves the FC in `Fault` on real sensors. Launch itself is
not refused: the VB/AB fire continues, and the FC runs its normal flight logic
on real stand data, which never leaves the pad states.

## 4. Replayed data source: SD-card CSV

### 4.1 Why on-board files

The replay lives on the FC, not on the GS link. A radio dropout during the
fire cannot starve the filter or change a deploy decision, and the same files
give the same flight every time. CSVs can be exported from a real recording or
generated by a model, and a person can read and edit them.

### 4.2 Files and naming

All files go in the `IGT` directory at the root of the FC's SD card (FAT,
8.3 names):

| Sensor | Files (segments, read in order) | Header (exact) |
| --- | --- | --- |
| IMU | `IMU0.CSV` … `IMU9.CSV` | `t_ms,ax_mps2,ay_mps2,az_mps2,gx_dps,gy_dps,gz_dps` |
| Barometer | `BARO0.CSV` … `BARO9.CSV` | `t_ms,alt_m,pressure_pa,temp_c` |
| GPS (optional) | `GPS0.CSV` … `GPS9.CSV` | `t_ms,lat_deg,lon_deg,alt_msl_m` |

- Segments are read from 0 upward and stop at the first missing index.
  `IMU0.CSV` and `BARO0.CSV` must exist. Without `GPS0.CSV` the FC keeps using
  real GPS throughout.
- `t_ms` is an integer number of milliseconds after T0, the FC's acceptance of
  Launch. It must be ≥ 0 and strictly increasing across every row of every
  segment of a sensor.
- Axes and units are the FC's own: body frame, z along the vehicle axis (up on
  the pad), acceleration as specific force in m/s² (+9.81 on z at rest), gyro
  in °/s. `alt_m` is height above the pad. GPS altitude is above mean sea
  level.
- Numbers are plain decimals (`-12.5`, `0.031`). Exponents are not accepted.
  Lines are at most 120 characters and end in LF or CRLF. Blank lines and lines
  starting with `#` are skipped.
- Every value must lie inside the FC's validator ranges (`MAX_ACC`,
  `MAX_DPS`, `MIN_ALT..MAX_ALT`, `MIN_PRS..MAX_PRS`, launch site ± 1° for GPS).
- Each of IMU and baro needs at least 2 rows, with the last row at least
  5000 ms after T0.
- Recommended rates: IMU 100 Hz, baro 50 Hz, GPS 10 Hz. Higher rates work, but
  they take longer to validate.
- Ignition on the stand comes about 10 s after Launch (VB pilot at
  Launch + 10 s, AB igniter at + 5 s, i.e. T−5 s). Put the replayed liftoff at about
  `t_ms = 10000`, with the vehicle at rest before it.

### 4.3 Replay

- The SD task owns FileX. It validates every file on `IgtEnter`. In `Ready` it
  primes three small single-producer rings of parsed rows (IMU 32, baro 32,
  GPS 16) and keeps them topped up. No CSV text is parsed on the sensor path.
- Pacing comes from the real sensors: every time a real IMU or baro sample
  arrives, its values are replaced with the replay interpolated linearly at
  t = now − T0. The real sample is kept for telemetry.
- GPS is interpolated and delivered at 10 Hz in place of RF fixes. The rail
  reference moves to the replay's first fix, and the real one is restored when
  the replay ends.
- When a sensor's rows run out, its last row is held. If baro and IMU have both
  run out for 30 s without a landing, the replay ends (`Done`). The FC then
  continues on real stand data, where it detects Landed normally.
- If a ring runs empty because the card is slow, the last row is held and the
  underrun is counted. This is reported as code 10, which is informational.

### 4.4 GPS switch at landing

When the FC enters `Landed`, the replay ends. IMU, baro and GPS all switch to
real sensors, and the rail reference is restored. The status bit "GPS
simulated" clears, and the FC sends `IGT GPS: real` as an ordered message. The
recovery report (`report_lowpass_gps`) and the GS map use real GPS from then
on. The replayed GPS track on the map is drawn separately (SIMULATED) and stops
at the switch.

## 5. Pyro gate, dry-run and LIVE PYRO

`deployment_pulse_start` is the single gate for both outputs. While the FC
mode is anything other than `Off` and LIVE PYRO is not armed, a pulse is
dry-run:

- no GPIO write, no watchdog deployment record, no `*_Asserted` bit;
- `Parachute_Deployed` / `Parachute_Expanded` are still set, so the state
  machine runs exactly as it would after a real deployment;
- an SD event `pyro_dry_run` is written, and status bits 10/11 are set;
- the FC sends `IGT dry-run: drogue not fired` (or `main not fired`) as an
  ordered message.

Dry-run also covers the operator's manual Deploy/Expand while the mode is on.
On the stand, "log only" means log only.

LIVE PYRO:

- **FC:** `IgtLivePyro` is accepted only in `Ready` (validated, before Launch).
  `IgtDryRun` is accepted at any time, including in flight.
- **GS:** LIVE PYRO goes through `POST /api/igt/prepare` and
  `POST /api/igt/confirm`. The ticket is single-use and lasts 15 s, the caller
  must be an **Admin**, and the operator must type `LIVE PYRO`. `IgtEnter` uses
  the same ticket flow without a typed phrase; the caller needs the `IgtEnter`
  command permission. The plain command path refuses `IgtEnter` and
  `IgtLivePyro`, but accepts `IgtDryRun` and `IgtExit`.

There is no real-motion override. A ground-test image can never be launched
for real, because the GS latches Launch closed in the flight modes while one is
on the network (section 7.1).

## 6. Wire flags

### 6.1 FC status: BOARD_HEALTH word 14

FC BOARD_HEALTH word 14 already has the tag `0x4F55` in bits 16..31 and the
cosmetic output bits 0..3. Bits 4..15 carry the IGT status. They are valid only
when bit 7 is set.

| Bits | Meaning |
| --- | --- |
| 4..6 | mode: 0 Off, 1 Validating, 2 Ready, 3 Sim, 4 Done, 5 Fault |
| 7 | FC built with ground-test support. The ground-test image sets it in every BOARD_HEALTH from boot; this is the image flag of section 7.1 |
| 8 | LIVE PYRO armed (clear = dry-run) |
| 9 | GPS simulated (the replay is feeding GPS) |
| 10 | drogue dry-run event recorded |
| 11 | main dry-run event recorded |
| 12..15 | reason: 0 none, 1 SD unavailable, 2 file missing, 3 bad header, 4 parse error / line too long, 5 time not increasing, 6 value out of range, 7 too short, 8 not stationary at Launch, 9 unused (was real motion), 10 ring underrun (info), 11 refused (wrong state), 12 data ended before landing (info) |

A change sends BOARD_HEALTH at once (at most every 250 ms) and again 1 s
later, the same as an estimator phase change. Detail, such as the file and
line of a CSV error, comes as ordered messages beginning `IGT `.

### 6.1a FC SD state: BOARD_HEALTH word 6 bits 29..31 (both images)

| Bit | Meaning |
| --- | --- |
| 29 | the image serves the SD read service (P2P port 4511); both images set it now |
| 30 | SD not ready: the card is not mounted, or the log file cannot be opened or written |
| 31 | an `FcSdRetry` is pending, not yet taken by the SD task |

The FC uses bits 0..23 for the watchdog task mask and bits 24..28 for the command
gate. Bits 29..31 were unused before. With bit 29 clear (older firmware, or an
image built without the service) the automatic FC SD collection ends with a
"not available" notice instead of retrying (`docs/data-pipeline.md` §3.4a).

### 6.1b Launch site: `LAUNCH_SITE`

`LAUNCH_SITE` is a new SEDSnet type: Static Float32 × 3 (lat_deg, lon_deg,
alt_msl_m). The GS sends it to `FLIGHT_CONTROLLER`, and the FC echoes it to
`GROUND_STATION`.

- **FC:** on every `LAUNCH_SITE` it receives, from the station controller only
  (the same admission as `ASCENT_BIASES`), the FC applies the site when all of
  these hold:
  - no Launch request, the flight state at Armed or earlier, and no replay
    running;
  - |lat| ≤ 90, |lon| ≤ 180, and alt within −500..9000 m.

  Either way it echoes the site it now uses. The site is RAM only: a reset
  returns to the compiled `LAUNCH_SITE_*`. It sets the centre of the ±1° GPS
  acceptance window, the altitude window and the replay CSV checks.
- **GS:** see section 7.4.

### 6.2 Simulated samples: `SIM_SAMPLE`

`SIM_SAMPLE` is a new SEDSnet type: Static Float32 × 8, sent to
`GROUND_STATION`. RF relays it from the FC's runtime schema announcement. Each
sample is:

| index | 0 | 1 | 2..7 |
| --- | --- | --- | --- |
| | kind | t (s since T0) | values |

| kind | Replaces | values |
| --- | --- | --- |
| 1 | `IMU_DATA` | ax, ay, az, gx, gy, gz |
| 2 | `BAROMETER_DATA` | alt, pressure, temperature, 0, 0, 0 |
| 3 | `GPS_DATA` (replayed fix) | lat, lon, alt_msl, 0, 0, 0 |
| 4 | `ASCENT_STATE` | q0, q1, q2, q3, alt, vel |
| 5 | `DESCENT_STATE` | lon, lat, alt, vel, 0, 0 |

While the replay runs (mode `Sim`), the FC sends every replayed sensor value
and every estimator output computed from them **only** as `SIM_SAMPLE`. The
normal types keep carrying the real sensors at their normal rates. A normal
type never carries replayed data, in any mode. On the SD card, replayed rows
are labelled `SIM`; the real raw sensor rows are unchanged.

`FLIGHT_STATE` is a managed network variable that VB, AB, RF and PB consume,
so it keeps its normal type. The GS marks FC flight states as simulated while
the FC reports mode `Sim`, or `Done` after a replay, and in IGT recordings.

## 7. GS behaviour

- **Banner:** a red **INTEGRATED GROUND TEST — SIMULATED FLIGHT DATA** bar on
  every view whenever the station mode is IGT. It shows the FC mode,
  DRY-RUN or LIVE PYRO, and GPS simulated or real.
- **Actions:** a "Ground test" section (`ControlSection::GroundTest`) holds
  `IgtEnter`, `IgtLivePyro`, `IgtDryRun` and `IgtExit`. It is first in IGT and
  absent from every other mode.
- **Launch gate (IGT only):** the FC routes are heard, FC IGT is `Ready`, and
  the normal Launch admission applies, except the GPS fix and the QD rule
  (handled as in test_fire). The safety catalog lists each rule with
  `Modes.igt`.
- **Mission phase and recovery:** InFlight and Landed show with a SIMULATED tag.
  Drogue and main notifications read
  `SIMULATED flight: drogue DRY-RUN (not fired) at 812 m`, or `LIVE drogue
  fired`.
- **Estimator notices:** these are expected in IGT. While the FC reports
  `Sim`, every notice is prefixed `SIMULATED input:`.
- **API:** `GET /api/igt` and the websocket frame `IgtStatus`, both carrying
  `{supported, fc_mode, live_pyro, gps_simulated, drogue_dry_run,
  main_dry_run, reason, reason_text, station_mode_igt, updated_ms}` plus:
  - `fc_image`: `"ground_test"`, `"flight"` or `"unknown"`;
  - `launch_latched` (bool) and `launch_latch_reason` (string or null);
  - `image_notice`: `{level: "warning"|"info", text}` or null;
  - `igt_active`: true in integrated_ground_test, or in HITL with `hitl_replay`
    on;
  - `mock_wet_dress` and `hitl_replay` (bools);
  - `sd_ready` (bool or null) and `sd_retry_pending` (bool).
- **Settings:** `GET /api/igt/settings` returns `{mock_wet_dress, hitl_replay}`.
  `PUT` takes the same shape, partial fields allowed, Admin only.
- **Launch site:** `GET /api/fc/launch-site` returns:

  ```
  {configured: {lat_deg, lon_deg, alt_msl_m} | null,
   compiled_default: {lat_deg: 31.758, lon_deg: -102.046, alt_msl_m: 890.016},
   fc_reported: {...} | null, confirmed: bool, pending: bool}
  ```

  `PUT` takes `{lat_deg, lon_deg, alt_msl_m}` or `{reset: true}`, Admin only.
- **Commands:** `FcSdRetry` (FC byte 33) uses the normal command path.

### 7.1 Ground-test image identification and the Launch latch

Word 14 bit 7 (section 6.1) identifies an FC running the ground-test image. A
flight image reports the tag with bits 4..15 clear.

| Station mode | Ground-test image on the network | Flight image on the network |
| --- | --- | --- |
| normal / flight | **warning** + **Launch latch** | normal |
| test fire | **warning** + **Launch latch** | normal |
| HITL (with or without the HITL replay) | informational notification, no latch | normal; with the HITL replay on: **warning** "FC is running a flight image: integrated ground test unavailable, flash the ground-test image" |
| integrated_ground_test | expected: informational indicator only | **warning**: the same text |

- **Latch:** the Launch latch is set the first time a ground-test image is seen
  in normal or test fire.
  - It blocks Launch (and GroundStationLaunch) with the reason "FC is running a
    ground-test image (dry-run capable): Launch is blocked until the FC reports
    a flight image".
  - It clears only when a fresh FC BOARD_HEALTH reports a flight image. A
    missing or silent FC does not clear it.
  - The reason appears in the action policy (the Actions tab and Launch
    controls) and in the safety catalog.
- **Warnings:** a warning appears as soon as the mismatch is seen, and clears
  when it ends.

### 7.2 Mock wet dress (admin setting)

`mock_wet_dress` (default off) is an admin-only GS setting, shown in the Ground
test section. When it is on, the integrated ground test runs the flight fill
steps instead of the test-fire fill sequence: the nitrogen fill step and the
normally-open checks. It persists with the other GS settings.

### 7.3 HITL with the ground-test replay

`hitl_replay` (default off) is an admin-only GS setting, available in HITL
mode. When it is on, HITL keeps its own rules for the fill side, and the FC's
CSV replay is the flight data source:

- **Fill:** the HITL GSE path, HITL interlocks, and the bench or simulated plant.
- **Ground test:** the Ground test section, the IGT commands, the IGT status,
  SIMULATED labels and the replay recording, all as in `integrated_ground_test`.
- **Launch:** goes to the FC and VB, and needs FC IGT `Ready`.
- **Phase controls:** Advance and Rewind are refused while the FC reports
  `Sim`, because the replayed flight drives the phase. They work again once it
  reports `Done`.

HITL itself simulates no flight sensors on the GS. The GS FlightSim exists only
in Testing mode with Simulation on, so it never competes with the FC replay.

### 7.4 Launch site (admin setting)

- **Setting:** an admin sets the site as lat_deg, lon_deg, alt_msl_m, or resets
  it to the FC's compiled default.
- **Sending:** the GS persists the site and sends it as `LAUNCH_SITE` when:
  - the setting changes;
  - the FC is first seen or reboots;
  - every 10 s, until the FC's echo matches.

  This mirrors the other board config resyncs. With no setting, the GS sends
  nothing, and the FC keeps its compiled site.
- **Status:** the GS shows the site as confirmed or pending, with the FC's
  reported values.

### 7.5 FC SD card health and Retry SD

This mirrors the DAQ's SD health and `DaqSdRetry`.

- **Alert:** word 6 bit 30 raises the GS alert "FC SD card not mounted / not
  writable". It clears when the bit clears.
- **Retry:** `FcSdRetry` (FC byte 33) is in the Flight computer section, and also
  in the Ground test section of the ground-test modes. It needs the
  `FcSdRetry` command permission, has a 5 s cooldown, and is refused while
  bit 31 shows a retry already pending.
- **FC side:** the SD task re-attempts the mount and the log file. In the
  ground-test image, a `Fault` caused by the card (reason 1 or 2) returns to
  `Validating`, and the CSVs are checked again.

## 8. Recording and provenance

- **Recording start:** the GS writes a `GS_STATION_MODE` row
  (`{"mode":"integrated_ground_test"}`), and a `GS_IGT` row on every FC status
  change.
- **`SIM_SAMPLE` rows:** stored under their own data type, so real and replayed
  data sit side by side in one recording.
- **CSV export:** in an IGT recording the generic export has a trailing
  `data_provenance` column:
  - `simulated`: SIM_SAMPLE rows, and FC FLIGHT_STATE rows received while the
    FC reported `Sim`;
  - `real`: board data;
  - `ground_station`: GS rows.

  Other modes keep the old header.
- **Test-fire CSV:** the GSE CSV is also exported on recording close in IGT,
  so the motor fire can be reviewed as in a test fire.
- **SD card:** the FC SD card holds the real raw sensor rows next to the `SIM`
  rows and the `pyro_dry_run` / `pyro_gpio_*` events.
- **FC SD collection:** after the simulated landing (Landed held 5 s, or
  Recovery) the station collects the FC's launch file over P2P port 4511, as
  after a flight, and attaches it to the `ground_test` capture
  (`board_sd/FC/<file>`, provenance `network_download` and the link). It also
  records the `/IGT` dataset that was on the card: the FC lists it read-only
  (SD directory 1) and answers each file's SHA-256; the station matches it
  against `test-data/igt` and stores names, sizes, SHA-256 and the matched
  set in the manifest entry (`igt_dataset`) and the job. The ground-test
  image serves the card only in its allowed states (pad Startup/Idle,
  Landed, Recovery), never during the replayed flight, and the service and
  the replay share the FC's SD thread (FC `docs/data-pipeline.md` 8.2).
  Copying the card by hand still works.

## 9. Interactions

| System | Behaviour in IGT | Change needed |
| --- | --- | --- |
| VB launch sequence | Pilot at Launch + 10 s for 1.5 s, exactly as in test fire. VB does not check FC state. | none |
| VB post-Launch auto-vent | Starts only on GS heartbeat loss (5 s) or a broken continuity wire. Vents at max(trigger + 150 s, Launch + 60 s). It does not depend on FC flight state, so the simulated InFlight/Landed has no effect. **Keep the GS link and the continuity wire intact**, or VB vents mid-test, which is safe but ends the test. | none |
| VB/AB FLIGHT_STATE | They accept InFlight and Landed forward-only after Launch. The effects are the same as in flight: automatic and heartbeat aborts are already off after Launch, and Landed allows OTA reset. | none |
| AB igniter | Fires at Launch + 5 s (T−5 s) for 10 s, exactly as in test fire. It does not depend on the FC. | none |
| FC pyro gate | Section 5. Watchdog deployment records are skipped for dry-run pulses. | FC |
| RRLS takeover | Needs 6 s without primary HEALTH. With the GS alive it never takes over, whatever the FC reports. If it does take over, it can only send Abort. | none |
| RF | Time-sync steps are blocked from Armed through Landed, as in flight. It relays `SIM_SAMPLE` from the runtime schema. | none |
| PB | Sensor board only; no state-driven outputs. | none |
| Abort | Unchanged. A manual ABORT goes to every board. On the FC the Stop path cancels pulses and sets the mode to `Off`. After Launch, VB needs the ABORT command (a FLIGHT_STATE write is refused), as in flight. | FC hook only |
| Watchdog resume | No checkpoint is written while the mode is anything but `Off`, so a reset during the test cannot resume a replayed flight. It boots rejected (no autonomous pyro) and needs a new flight admission. | FC |

## 10. Failure cases

| Failure | Result |
| --- | --- |
| CSV missing, malformed or out of range | `Fault` with a reason code and an `IGT <file>:<line> <problem>` message. The GS refuses Launch until IGT is `Ready`. |
| SD card missing or unreadable | Word 6 bit 30 and the GS alert. `IgtEnter` faults at once with reason 1. `FcSdRetry` re-mounts and re-validates. |
| Card read stalls during replay | Ring underrun: the last value is held, reported as info code 10 |
| Data ends before landing | The last value is held. After 30 s the replay ends, the FC uses real sensors and detects Landed on the stand (code 12, info). |
| Vehicle moving at Launch | `Fault` 8. No replay, normal FC logic on real data. |
| Ground-test image on the FC in normal or test fire | Warning and Launch latch, until the FC reports a flight image (section 7.1) |
| Flight image on the FC in integrated_ground_test (or HITL with the replay) | Warning; IgtEnter and the IGT Launch gate refused |
| FC reset or power cycle | Mode `Off`, no replay, resume rejected. Re-entry needs reset to Startup and a new `IgtEnter`. |
| GS link lost | VB auto-vent countdown (section 9). The FC continues its replayed flight and gate on its own. |
| Operator Abort at any point | Normal abort on every board. FC mode `Off`. |
| Flight image on the FC | No IGT support bit. The GS refuses `IgtEnter` and the IGT Launch gate. |
| Normal or test fire station mode | The IGT commands are not offered and are refused by policy. HITL offers them only with the HITL replay on. The FC rejects `IgtEnter` after Postinit or Launch in any case. |

## 11. Creating the CSV files

`tools/igt_csv.py` (Python 3, standard library only):

```sh
# From a parametric model (deterministic). --site is the pad; the GPS track
# drifts a few hundred metres downrange and with the wind:
python3 tools/igt_csv.py model --out igt/ --apogee-m 900 --burn-s 3.5 \
    --liftoff-ms 10000 --site 31.758,-102.046,890 --wind-mps 4 --wind-from-deg 270

# From a GS recording CSV export (Settings > Recordings > CSV):
python3 tools/igt_csv.py recording rec.csv --out igt/ --t0 launch

# Check a set the way the FC will:
python3 tools/igt_csv.py check igt/
```

Copy the `IGT` folder to the root of the FC's SD card. On the card:

```
/IGT/IMU0.CSV
/IGT/BARO0.CSV
/IGT/GPS0.CSV
```

The FC's own `irec*.csv` logs stay at the root. FileX reads FAT12, FAT16 and
FAT32. Use 8.3 names; do not rename the folder.

### 11.1 Using the committed mock flights

`test-data/igt/` holds ready-made flights of the SWEET_V6 vehicle (6 in, 4.39 m, 40.84 kg dry; `tools/flight_profile/vehicle/SWEET_V6.ork`). Their
README gives each one's apogee, Mach, accelerations and event times:

| Folder | Use |
| --- | --- |
| `nominal_10k` | the default: a nominal flight to 10,000 ft AGL |
| `measured_motor` | the 2026-11-16 motor as measured (9.2 kN s with the fill-cell unloading, about 5,000 ft on this vehicle) |
| `high_wind` | 9 m/s wind with gusts: weathercocking and drift |
| `early_apogee_noise` | Kalman stress: baro noise, outliers, Mach error and a false-descent step before apogee |
| `off_nominal` | FC logic: a late main, an IMU saturation glitch, a frozen baro and a GPS dropout |

To load one:

1. Power the FC down and take out its SD card. Mount it on a computer.
2. Delete any old `/IGT` folder on the card. Then copy the dataset's `IGT`
   folder, not its parent, to the card root, so the card has
   `/IGT/IMU0.CSV`, `/IGT/BARO0.CSV` and `/IGT/GPS0.CSV`. Leave the
   `irec*.csv` logs where they are.
   ```sh
   rm -rf /media/$USER/SDCARD/IGT
   cp -r test-data/igt/nominal_10k/IGT /media/$USER/SDCARD/
   sync
   ```
3. Check the copy on the card the way the FC will. The sets are built for the
   compiled IREC site (31.758, -102.046, 890 m). If the station's Launch site
   setting is different, pass it, or regenerate with `--site`:
   ```sh
   python3 tools/igt_csv.py check /media/$USER/SDCARD --fc-site 31.758,-102.046,890.016
   ```
4. Eject the card, put it back in the FC, and follow section 12. Enter ground
   test should report `IGT CSV ok: IMU 10322 243s, BARO 12173 243s, GPS 2326`
   for `nominal_10k`. Validation reads about 1 MB, so it takes a few seconds.

Each set starts with 10 s at rest on a 5° rail, because the motor lights
about 10 s after Launch. The drogue is near apogee (about 36.8 s), the main
at 457 m (about 140 s), and the vehicle lands about 215 s after Launch. After
landing there are 30 s at rest, so the FC reaches Landed and ends the replay.

The replayed `alt_m` column is what the board computes from the replayed
pressure: the FC's `baro_agl()` (the ISA pressure altitude of the sample minus
that of the pad, i.e. the hypsometric equation with the ISA pad temperature).
At the 890 m site it matches the model's height above the pad to about 0.1%
(the ISA altitude is geopotential, about 2 m short of the geometric height at
3 km). Datasets made before 2026-11-22 used the old sea-level form
`44330 * (1 - (p/p0)^0.1903)`, which read about 2% high at this site.

### 11.2 Generating new mock flights

`tools/flight_profile/generate.py` (needs numpy) is a 3-DOF model with a
weathercock and pendulum attitude approximation. It covers the rail,
component drag build-up versus Mach and Reynolds number, the measured
hybrid thrust curve, roll, the drogue and main with opening shocks and swing,
landing and tip-over. Its output goes through datasheet models of the BMI088,
the BMP390 (with port lag, IIR and static-port error) and GPS (with a dropout
during boost). It writes and checks the same layout. The output is
deterministic for a given seed.

Every assumption, with its source, is in `tools/flight_profile/vehicle.json`.
When the team has a measured value, put it there. The 10 kN s question, for
example, is `motor.total_impulse_ns`. Then run `--all` to regenerate the
committed sets and their README.

```sh
python3 tools/flight_profile/generate.py --all                     # test-data/igt/*
python3 tools/flight_profile/generate.py --variant nominal_10k --out /tmp/igt \
    --wind 6 --wind-from 270 --gust 2 --seed 7                    # a new draw
python3 tools/flight_profile/generate.py --out /tmp/igt --site 32.99,-106.97,1401 \
    --apogee-m 3048 --no-noise                                    # another pad, ideal sensors
python3 tools/flight_profile/generate.py --solve-dry-mass 3048     # dry mass for 10k ft
python3 tools/flight_profile/generate.py --out /tmp/igt --set vehicle.dry_mass_kg=28 \
    --set recovery.main_descent_mps=7
```

To see what the FC will decide before a test, run FlightComputer26
`tools/igt_replay_host.py <folder>`. It replays the set through the FC's
real replay, estimator and state-machine code on the host.

## 12. Procedure

1. Load an FC image built with `--ground-test`, and put the CSVs on the SD
   card. While this image is on the network, normal and test fire refuse
   Launch.
2. If the test site is not the FC's compiled launch site, set Settings >
   Launch site (Admin). The replayed GPS must lie within ±1° of it.
3. In Settings > Station, switch the station to **Integrated ground test**
   (Admin), or use HITL with the HITL replay on.
4. Power the full stack, and wait for FC, RF and PB to show ready.
5. Ground test > **Enter ground test** (confirm). Wait for FC IGT `Ready`; a
   CSV problem shows as a fault with its file and line.
6. Optional: Ground test > **LIVE PYRO** (Admin, typed confirmation). Otherwise
   it stays DRY-RUN.
7. Fill as in a test fire, or with the flight steps if **Mock wet dress** is on.
8. Launch. The motor fires for real. The FC runs the replayed flight, and the
   UI shows the SIMULATED banner, phase and track.
9. Drogue and main events happen (dry-run or live). At landing the GPS switches
   to real.
10. Close the recording. Afterwards, IgtExit or reset the FC to Startup.
