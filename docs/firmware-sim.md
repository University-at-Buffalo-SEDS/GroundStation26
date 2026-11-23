# Firmware simulation mode (SITL)

The station can run against the **real firmware of every board** (FC, RF, PB, GB, AB, VB, DAQ), compiled for the
host by FirmwareSimulator's native SITL engine, instead of its own simplified `flight_sim`. Commands go through the
boards' actual code; responses, safety latches, timing and faults are the firmware's own. A physical plant closes the
loop: tank pressure, load cell, chamber pressure and the flight itself respond to what the boards' outputs do.

It is a sim backend for **every station mode** (Normal, HITL, Test fire, Testing, Integrated ground test): the
station mode switch offers "<mode> · SIMULATED FIRMWARE" next to each mode, selectable by an administrator (or an
operator allowed the `StationMode` command), clearly bannered, and never entered while real hardware is connected.
The selected mode's own logic then runs against the simulated board network exactly as against hardware; switching
between modes inside the simulation is in process, entering or leaving it restarts the backend.

## 1. Does it make sense?

Yes, on a development machine. The comparison:

| | HITL / testing `flight_sim` (in the station) | FirmwareSimulator harness (native fullnet) | Firmware sim mode (this) |
|---|---|---|---|
| Board logic | none: the station fabricates telemetry and acks | the real firmware | the real firmware |
| Who drives | the operator in the UI | a scripted scenario controller that owns the station | the operator in the UI (or any API client) |
| Station clock | wall | the bay's virtual clock (simclock), up to ~10x | wall (the bay runs at 1x) |
| Plant | `flight_sim/ground_plant.rs` from the station's own valve state | scenario stimulus files, plant from the station's reported valves | plant from the boards' own output pins and servo registers |
| Flight | open-loop profile in the station | FC sensors at rest natively (Renode replays a fixed profile) | 1-D flight driven into the FC's real baro/IMU; descent follows the FC's real pyro outputs |
| Setup cost | none | a cohort, a station image, a scenario | one `tools/firmware_sim.py build` (about 2.5 min), then a mode switch |

What it catches that the existing paths do not:

* The firmware's actual answer to an operator sequence: refusals, interlocks, safety latches (VB overpressure
  relief and abort, continuity loss, heartbeat timeouts), watchdog and reset behaviour, the AB/VB launch sequence
  timing, the FC's state machine, Kalman filter and deployment logic, DAQ input-health faults. `flight_sim` only
  shows what the station believes the boards would do.
* Wire-level integration with the station: the real SEDSnet protocol, router, timesync, discovery, bundles and
  config sync over the same radio PTY and Pico-Fi umbilical transports the native fullnet uses, but with the
  station on its normal wall clock, as on the pad.
* Physical consistency failures: a pressure that cannot rise with the QD off, a fill that never reaches its
  target with a stuck valve, a flight with no deployment (ballistic), a dead chamber PT during the burn.
* Exploratory testing with the real UI by people who do not write scenario scripts.

What it does not replace: Renode stays the reference for instruction-level timing, flash fit and stack depth,
the bootloader/OTA path and peripheral register behaviour (FirmwareSimulator `docs/native-sitl.md`, "What stays
Renode-only"); the scripted native fullnet stays the regression harness (fast, deterministic, pass/fail).

Costs:

* **Build**: about 150 s for all seven boards from clean on jupiter (24 cores, 8 jobs), about 1 s each
  incrementally; the coordinator about 20 s. Needs the ARM toolchain (configure only), host `gcc -m32`, Rust with
  the `i686-unknown-linux-gnu` target and local clones (or network access) of the board repos, SEDSnet and
  SEDSLaunchCore.
* **Disk**: the cache is about 145 MB built (seven unstripped executables, symbols, board layouts, the coordinator). A build
  peaks at about 1.3 GB under `<cache>/work`, deleted afterwards unless `--keep-work`.
* **CPU**: at 1x pace the bay uses well under one core on jupiter (seven boards at 100x faster than Renode). A
  Raspberry Pi 5 could likely *run* it but cannot reasonably *build* it (toolchains, `-m32` multilib, 1+ GB of
  intermediates), and the Pi's job on the pad is the real station.
* **Two sims to maintain**: `flight_sim` stays (zero-setup training and UI work, runs on the Pi). This mode adds
  no station-side model: the station side is a mode flag, the supervisor's bay lifecycle and a status/control API.
  The plant lives in FirmwareSimulator next to the engine (`src/bay/native_plant.rs`) and reuses the same fill
  physics as `flight_sim/ground_plant.rs` with the supply path made explicit.

**Recommendation: run it on a development machine (or a lab server); dev-machine-only for now.** The Pi build does
not offer the mode. A later step can let a Pi station connect to a bay on a server (option c below) or run CI-built
native binaries; neither is needed for the main use (firmware and station developers testing reactions).

## 2. Architecture

Options considered:

* **(a) the station spawns the native bay as a managed subprocess and connects to it like hardware** - chosen.
  Nothing firmware-specific enters the station process; the station uses its ordinary transports (the simulator
  serial overrides it already has for the fullnet: the radio as a PTY, the fill box as the modelled Pico-Fi on a
  loopback discovery port). One switch of `GS_SIMULATED_SERIAL_PTY`/endpoint variables and the same backend binary
  talks to simulated boards.
* (b) embedding the models in the station: would mean linking 32-bit firmware images, ThreadX coroutines and the
  lockstep coordinator into the tokio process. Rejected: no isolation (a firmware crash takes the station down), a
  32/64-bit mismatch, and the native engine's design assumes one process per board.
* (c) a remote sim server: the bay already speaks over a PTY and UDP/TCP; exposing the radio PTY through a TCP
  bridge would let a Pi station use a bay on a server. Deferred (follow-up), and it needs care so a networked sim can
  never be mistaken for the pad.

How it fits together:

```
station mode switch (admin)  --"hitl-firmware-sim" selection, exit 75-->  station_modes.py (supervisor)
    supervisor: fresh isolated state, then tools/firmware_sim.py run --out <state>/firmware-sim/run
        -> FirmwareSimulator scripts/gs_bay.py -> firmware-sim native-bay (pace 1, external station)
              7 native boards + CAN buses + RFD radio medium + Pico-Fi model + plant
              writes endpoints.json (radio PTY, Pico-Fi discovery port), status.json (1/s); reads control.json
    supervisor: backend with GS_FIRMWARE_SIM=1, the bay's endpoints, isolated paths
    backend exits -> supervisor stops the bay (SIGTERM)
```

### Fetch, build, cache

`python3 tools/firmware_sim.py build` fetches each board at a ref and builds its native target into the cache with
a version manifest:

* refs: every board at `--ref` (default `dev`), per-board overrides `--board flight=<ref>`, FirmwareSimulator at
  `--fs-ref` (default `dev`) or a local checkout `--fs-dir`, SEDSnet at `--sedsnet-ref` (default `main`, as the
  boards build it);
* sources come from the local clones next to this checkout (fetched first) or from blobless mirrors cloned into
  `<cache>/mirrors`; each board is `git archive`d at the resolved SHA and built with FirmwareSimulator's
  `native/build_board.py` (no board source is modified; nothing touches flight images);
* the cache is `$GS_FIRMWARE_SIM_CACHE` or `~/.cache/seds-firmware-sim`: `boards/<node>/` (executable, symbols,
  `board.json`), `bin/firmware-sim`, `firmware-simulator/` (engine sources at the chosen ref) and `manifest.json`
  (per board: repo, ref, commit, subject, commit date, SEDSnet and engine commits, build time). Up-to-date boards
  are skipped; `--force` rebuilds; `tools/firmware_sim.py status` prints the manifest; `clean [--all]` removes
  intermediates and mirrors (or everything).

The UI shows the manifest (`GET /api/firmware-sim`: `manifest`, and `bay.firmware` for the running bay), so
operators see exactly which firmware revisions are being simulated.

### Station side

* `backend/src/firmware_sim_mode.rs`: the mode flag (`GS_FIRMWARE_SIM`, fixed at start), availability (not on an
  RRLS, not in a `raspberry_pi` build, only with a complete cache), the real-hardware check, recording provenance,
  and the API: `GET /api/firmware-sim` (status for any signed-in viewer: banner, manifest, bay pace, nodes, resets,
  plant, control) and `POST /api/firmware-sim/control` (administrators only: plant inputs and faults; validated,
  unknown keys refused, every change logged).
* `station_mode_switch.rs`: one more target per mode, `<mode>-firmware-sim` (normal, hitl, test_fire, testing,
  integrated_ground_test), labelled "<mode> · SIMULATED FIRMWARE ..."; entering or leaving restarts the backend (like
  Simulation), switching between them is in process. **Entering is refused while real hardware is connected**: any board heard on this
  station's links in the last 30 s refuses the switch with the boards named. Inside the simulation the check does not
  apply (the boards it hears are simulated). The current mode carries `"banner": "SIMULATED FIRMWARE"`.
* `station_modes.py`: a selection with `firmware_sim` starts the bay before the backend, passes its endpoints and
  a comms config naming no physical device, isolates state and configuration under
  `<station-mode dir>/firmware-sim/state` (fresh at every start: launch clock, board abort history, GSE
  qualification, network variable cache, fill targets, flight setup, calibrations, checklists, plumbing, board
  config sync, mission settings, zero captures, logs) seeded to match the plant, records into
  `<station-mode dir>/firmware-sim/recordings` (kept), and stops the bay when the backend exits. If the bay cannot
  start, the backend still starts in firmware-sim mode with its links pointing nowhere (never at hardware) and the
  status shows the bay as not running. Raspberry Pi builds never run it.
* Start / stop / reset: start = select a firmware-sim mode; stop = select any hardware mode; reset = select the
  firmware-sim mode again from a hardware mode, or restart the station (each start is a fresh power-on of every
  board and a fresh plant).
* Recordings: every capture records `provenance` (`hardware`, `firmware_sim`, `internal_flight_sim`); a saved
  capture's manifest carries `provenance`, `simulated` and, for firmware sim, the simulated firmware manifest under
  `versions.simulated_firmware`. The firmware-sim rolling store is a separate directory, so simulated data never
  mixes with the hardware ring.
* Everything else (proximity lock, launch admission, checklists, alerts, recovery) is the ordinary station code
  path: the mode only changes where the links lead.

## 3. Plant and peripheral models

`FirmwareSimulator/src/bay/native_plant.rs` (wiring) and `native_physics.rs` (physics, every assumption and source
in its header), stepped every 10 ms of virtual time by the native coordinator. Inputs are the boards' outputs, never
their internal state:

* AB `N2_SIG` (PB13), `N20_SIG` (PB14), `IGNITER_SIG` (PB12) and VB `Solenoid` (PB11, pilot) from the boards' GPIO
  reports; VB vent (normally-open servo, TIM2 CH2) and dump (TIM2 CH1) from the timer compare registers in the VB's
  mapped peripheral page, open below 860 us (open ~530 us, closed ~1200 us on the VB's 500-2500 us/270 degree
  mapping); FC `CO2` (PB5, drogue) and `REEF` (PB6, main) pyro outputs.
* Operator inputs (`POST /api/firmware-sim/control`, administrators): `plant` (QD connected, nitrogen bottle open,
  nitrous bottle open), `conditions` (ambient temperature, wind speed, direction, gusts), `loads` (calibration
  masses on the KG50 and KG1000), `faults` (below). Every change is logged and the current control is written into
  saved captures (`versions.simulated_conditions`).

Fill system:

* **Nitrous**: saturated liquid/vapour in a 12 L tank with ESDU 91022 property correlations (vapour pressure,
  liquid/vapour density, latent heat, liquid heat capacity; checked against NIST at -20/0/20 C), equilibrium
  liquid/vapour split and ullage, a lumped liquid+wall temperature with heat from ambient. Liquid flows in from a
  22.7 kg bottle at ambient (orifice flow, 15 psi head) only while the AB valve is open, the QD connected and the
  bottle open, and only while the bottle is above the tank, so a same-temperature transfer needs the vent: venting
  evaporates liquid and cools the tank (pressure follows the saturation curve down). Liquid-full is hydraulic (the
  fill stops). Vent: choked vapour; dump: liquid from the bottom, then gas. Ambient temperature moves the vapour
  pressure (35 C is far above 20 C) and the fill behaviour.
* **Nitrogen**: a 49 L K bottle at 2200 psi behind a regulator (setpoint 1000 psi default, configurable in the plant
  config), polytropic blowdown (n = 1.3) cooling the bottle, ideal gas in the tank ullage (compressed by rising
  liquid, isothermal at the tank temperature).
* Tank pressure is the N2O vapour pressure plus the N2 partial pressure; the VB tank PT (ADC3, 0-2000 psi over 0-3103
  counts) reads it. The VB's own overpressure relief (850 psi) fires in sim exactly as on the pad (seen in testing:
  an N2-pressurised tank topped with nitrous tripped it).

Motor and stand (11-16 static fire, `tools/flight_profile/motors/testfire_20261116.json`): oxidiser through the
injector from tank conditions (liquid, then a choked-vapour tail), thrust = 107 s delivered Isp x g0 x (1 + 1/OF 5)
x mdot_ox (5.6 kg gives about 7 kN s, liquid phase about 2.7 s, unit-tested), chamber pressure from a fixed C_F A_t
(2 kN reads ~350 psi) on the DAQ chamber PT (CH6, PX309-1KG5V, 200 psi/V, connector/11). `motor.impulse_scale`
scales thrust (about 1.8 reaches 10 kft with the assumed airframe, as `tools/flight_profile` nominal_10k).

Load cells (`docs/loadcell-investigation-2026-11.md`): KG1000 545.8 kg/V at the ADC pin, zero 0.2126 V; KG50
12 915 kg/V, zero 0.2112 V; Gaussian noise (0.05 / 0.1 kg per sample), the 4 Hz and 10 Hz DAQ scheduling lines,
first-order settling (1.5 s / 50 ms), optional thermal drift with ambient. They go through the real DAQ volts path
(MCP3564R codes). Stand load path: thrust first unloads the KG50 (the hanging rocket plus propellant), whose tared
reading goes negative as the motor lifts the rocket; once the gap closes the KG1000 sees thrust minus the hanging
weight, with a damped 25 Hz impact transient, so F_KG1000 + (W_KG50_preburn - W_KG50(t)) g is the total thrust
(unit-tested). Faults: load bypass fraction (rod friction), KG1000 supply-dip top clamp, open lead (half sensitivity,
0.6 kg noise) on either cell. The isolated calibration is seeded with those slopes; operators can run the real
calibration flow by placing calibration masses from the control.

Liftoff and flight: the motor lights once the igniter has fired (not required in HITL and testing, where the AB
fires no igniter) and the pilot opens with at least 0.5 kg of liquid; the main valve is assumed to stay open to
burnout once lit (HITL opens the pilot for 1.5 s). On the test stand (test fire mode) the rocket stays put. In
flight: the umbilical separates (the VB leaves the fill CAN bus, the continuity loop opens, the QD disconnects);
1-D vertical dynamics with the vehicle of `tools/flight_profile/vehicle.json` (31.6 kg dry, 1.1 kg fuel, 6 in, Cd
0.5), drag with ISA density shifted by the ambient temperature, horizontal wind drift with gusts (no weathercocking);
descent 25 m/s under the drogue and 5.8 m/s under the main after the FC fires each output, ballistic without them.
The FC barometer reads true ISA pressure at the geometric altitude above the 890 m pad (FlightComputer26 baro_agl,
659440e); the accelerometer reads the specific force on body Z; before liftoff the FC reads the stationary pad.

Fault injection (control `faults`): stuck valves (`nitrogen`, `nitrous`, `vent`, `dump`, `pilot` open/closed
regardless of the board's output), dead tank PT (open input), dead chamber PT, radio link drop (both directions),
`kg50_bypass`/`kg1000_bypass` (0-1), `kg1000_supply_dip`, `kg50_open_lead`/`kg1000_open_lead`,
`kg50_drift_kg_per_c`/`kg1000_drift_kg_per_c`.

Not modelled yet: thermocouples, igniter current and continuity resistance sensing, power rails and battery
discharge (LTC2990 monitors read static values), RF link quality versus range (only a full drop), the RF board's
NEO-M9N GPS, an SD card missing, weathercocking/3-DOF attitude, weather-shifted pad pressure, the DAQ DAC to ADC
patch (section 7).

## 4. FC simulated flight

The FC runs its real estimator and recovery logic. The coordinator's flight model drives the FC's BMP390/BMI088
models through the native shim (`sim_flight_sensors.c`, "flightsens" device, from the first plant step): the pad
before liftoff, then the flight. Pyro outputs are observed and change the descent model. The vehicle parameters
mirror `tools/flight_profile/vehicle.json` (15 ft x 6 in, 31.6 kg dry) and the 11-16 motor; with the measured
motor the vehicle reaches about 1.4 km, `motor.impulse_scale` about 1.8 gives the 10 kft profile.

## 5. Raspberry Pi 5 (aarch64), bundles and the remote build

Status: **analysed, not ported or verified on aarch64.** What the port needs, from reading the engine:

* The board executables are deliberately **32-bit** (`gcc -m32`, SEDSnet for `i686-unknown-linux-gnu`): the firmware
  keeps pointers in 32-bit fields (ThreadX `ULONG`, DMA and register addresses) and the shim maps the MCU address map
  at its real addresses (flash 0x08000000, SRAM 0x20000000, peripherals 0x40000000-0x5FFFFFFF, PPB 0xE0000000), so the
  ARM counterpart is **AArch32 (armhf) user space**, not 64-bit aarch64. A 64-bit port would touch every
  pointer-in-uint32 site of seven boards and lose the 32-bit layout that makes native faithful.
* **Pi 5 page size**: the Cortex-A76 runs AArch32 at EL0, but the Pi 5's default kernel (`kernel_2712.img`) uses
  16 KB pages and arm64 Linux offers 32-bit compat only with 4 KB pages. A Pi 5 that runs the board executables
  must boot the 4 KB kernel (`kernel=kernel8.img` in `config.txt`, a supported Raspberry Pi OS option; Docker then
  runs armhf binaries natively). The fixed MCU mappings are 16 KB-aligned and their sizes are page multiples, so
  they would also be fine under 16 KB pages; the compat requirement is the blocker.
* x86-specific pieces: `native/build_board.py` (`-m32`, the i686 SEDSnet target, `-Ttext-segment=0x70000000`,
  `nm`); `sim_stubs.c`'s `.set _estack` (portable GAS); the CMSIS layer in `sim_preinclude.h` (only a compiler
  barrier, portable); ThreadX coroutines on `makecontext/swapcontext` (present in glibc armhf, absent in musl). The
  simclock LD_PRELOAD (`native/simclock`) is x86_64-only, and so is the thread-level lockstep, but **the
  station-managed mode uses neither**: the bay runs at pace 1 and the station on its own wall clock. The
  coordinator talks to the boards over Unix sockets with blocking messages (no shared-memory lock-free protocol on
  this path), so there is no x86 memory-ordering assumption here. The coordinator and the plant are portable Rust.
* Toolchain: jupiter has the Rust `armv7-unknown-linux-gnueabihf` and `aarch64-unknown-linux-gnu` targets and
  `qemu-arm`/`qemu-aarch64`, but no armhf/aarch64 C cross compiler and no binfmt registration (registering it is a
  system change this work did not make), so nothing ran under emulation yet.
* Resources measured on jupiter (x86, 1x, seven boards + plant, during the end-to-end run): the coordinator (lockstep,
  radio medium, Pico-Fi model, plant) about 13 % of one core, the seven board processes about 14 % together, about
  47 MB RSS in total. A Pi 5 core is roughly a third to a half of a jupiter core, so the bay should need about one
  Pi core at 1x (estimate, not measured on a Pi), next to the station.
  Board processes run at nice 10 (`FIRMWARE_SIM_NODE_NICE`); on a Pi the bay should additionally run in its own
  cgroup (`docker run --cpus=1.5` or a systemd `CPUQuota=150%` slice) and stay at pace 1 (the station-managed mode
  never runs faster than real time).
* Safety on a Pi: the backend refuses to enter firmware simulation while any board was heard in the last 30 s; in
  the mode the comms config names no device; the supervisor stops the bay whenever the backend exits (SIGTERM is
  forwarded and the bay is a child that also dies with its parent), and a bay crash leaves the backend running with
  the bay shown as not running. The Pi build currently does **not** offer the mode (`raspberry_pi` feature).

Bundles: the cache layout is the bundle (`manifest.json` with every board's repo/ref/commit, `boards/<node>/`
executable + `board.json` + symbols, `bin/firmware-sim`, `firmware-simulator/` scripts and shim sources). On x86 it
is about 140 MB of board executables (unstripped, debug info) plus 3 MB coordinator; stripped it is a fraction of
that. A station container would take it as a volume (`-v <bundle>:/opt/firmware-sim`, `GS_FIRMWARE_SIM_CACHE`).

Remote build (`build.py --remote HOST`, `scripts/remote_build.py`): **not implemented.** The plan: a `sim-bundle`
target that rsyncs the board checkouts at their resolved refs (per-build copies, since SEDSnet's prepare step rewrites
its source directory; no credentials), adds an armhf C cross compiler, cmake/ninja and the Rust
`armv7-unknown-linux-gnueabihf` target to the `gs-station-builder` image, parametrises `build_board.py` for
`--host-cc arm-linux-gnueabihf-gcc --host-target armv7-unknown-linux-gnueabihf`, builds the coordinator for
aarch64, and brings back the bundle; `build.py --remote HOST --sim` would add it to the station build.

DAQ DAC to ADC patching (requested): **not implemented.** Design: the plant reads the DAQ's DAC data-holding
register from the DAQ node's mapped peripheral page each step (so whatever waveform the real firmware generates,
from the shared waveform spec or any table, is what appears), applies the DAC resolution/range and the P10 buffer
(with the swapped-inputs fault as an option), and substitutes that voltage on the patched ADC channel(s); the patch
is a control section, and the UI patch diagram is gated on `/api/firmware-sim` `active`.

## 6. Running it

Where: a Linux x86_64 development machine with the ARM toolchain (configure only), host `gcc` with `-m32`
multilib, Rust with the `i686-unknown-linux-gnu` target, and local clones of the board repos, SEDSnet and
SEDSLaunchCore next to this checkout (jupiter has all of these). Not on the Pi yet (section 5).

```sh
# 1. fetch and build every board's native firmware (about 2.5 min clean, seconds when up to date)
python3 tools/firmware_sim.py build                       # boards at dev, SEDSnet main, FirmwareSimulator dev
python3 tools/firmware_sim.py build --board flight=659440e --ref main   # pin revisions
python3 tools/firmware_sim.py status                      # what will be simulated

# 2. build the station and run it under the supervisor (any station mode as the default)
python3 station_modes.py build
python3 run_groundstation.py --switchable-modes --hitl-mode      # or the station-mode systemd unit

# 3. in the UI, Station mode: pick "<mode> · SIMULATED FIRMWARE" (every mode has one), review, confirm.
#    The station restarts against the bay; GET /api/firmware-sim shows the banner, revisions, pace and plant.
#    Leave by picking any hardware mode; re-enter (or restart) for a fresh power-on of every board.

# plant inputs, conditions, calibration masses and faults (administrators)
curl -X POST .../api/firmware-sim/control -d '{"plant":{"qd_connected":false},
    "conditions":{"ambient_c":35,"wind_mps":9,"wind_from_deg":300,"gust_mps":3},
    "loads":{"kg50_kg":10}, "faults":{"stuck_valves":{"vent":"closed"},"kg1000_supply_dip":true}}'

# the end-to-end check (HITL: fill rules, Armed, Launch, flight through the real FC, recovery events)
python3 tools/firmware_sim_e2e.py --out /tmp/fsim-e2e
python3 tools/firmware_sim.py clean --all                 # remove the cache
```

Per mode: **HITL** - fill by hand (Nitrogen/Nitrous/NormallyOpen/Dump), step to Armed, Launch: the VB opens the pilot
at T+10 s, the plant lights the motor (no igniter in HITL) and flies; the FC runs its estimator and fires its pyro
outputs. **Normal** - the full procedure; RF GPS is not modelled yet, so GPS-gated admission blocks Launch.
**Test fire** - the rocket stays on the stand; the igniter and pilot light the motor and the KG50/KG1000/chamber PT
show the burn. **Testing** and **IGT** - the mode's own logic against the simulated boards; loading the IGT CSV
datasets onto the simulated FC SD card is a follow-up (the native FC reports its SD card as not mounted).

Logs: `<station-mode dir>/firmware-sim/run/diagnostics/` (`native-<board>.log`, `native-gpio.log` with plant
events and control changes, `bay.log`, `native-report.json`), live `run/status.json`; isolated station state under
`firmware-sim/state`, simulated recordings under `firmware-sim/recordings`.

### Verified on jupiter (2026-11-23)

`tools/firmware_sim_e2e.py` in HITL against FC 659440e, RF a2fc87f, PB 22250a7, GB 87f824e, AB 25e19bb, VB b641993,
DAQ 6f29611 (all dev), SEDSnet main ebdb444, FirmwareSimulator dev + this work; 17 of 17 checks pass:

* the supervisor starts the bay and the backend; all seven boards are heard over the simulated radio and Pico-Fi
  umbilical within 11 s; the bay holds pace 1.00;
* the VB firmware closes the vent and dump servos on command; the VB-reported tank pressure rises (0 to 177 psi)
  with the AB nitrogen valve open, holds (179 to 178 psi) with the QD disconnected while the valve stays open, falls
  on dump; a vented nitrous fill raises the DAQ fill load cell from 0.1 to 6.1 kg and the tank to 745 psi;
* HITL steps to Armed, Launch is accepted, the VB opens the pilot at T+10 s and the plant lights the motor with 5.2 kg
  of nitrous; the real FC estimator goes Launch, In flight, Ascent, Coast, Apogee, Descent, Reefing, Landed
  (Recovery in an earlier run); apogee 1509 m; the FC fires its drogue output at T+21.0 s and its main at T+63.3 s
  (about 430 m); the descent follows them to landing with 550 m of wind drift;
* stopping the supervisor (SIGTERM) stops the backend and then the bay; no process or scratch directory is left.

Also seen: an N2-pressurised tank topped up with nitrous trips the station's 850 psi overpressure relief, which closes
the supplies and opens dump and vent through the real boards.

Not verified: Normal (RF GPS not modelled), Test fire, Testing and IGT end to end (the modes are offered and the plant
follows them, but only HITL was driven through), aarch64, the remote build, the DAC patch.

## 7. Follow-ups

* Frontend (the separate frontend repo, untouched here): a SIMULATED FIRMWARE banner on every view from
  `current.banner` (station modes) and `/api/firmware-sim` `banner`, a sim panel (firmware revisions, pace, plant,
  conditions, calibration masses) and the admin fault and patch controls. The backend API is in place.
* Raspberry Pi 5: the armhf board build (`build_board.py` host-compiler/target options), the aarch64 coordinator,
  a qemu/arm64-container smoke test, prebuilt bundles from jupiter or CI, the `build.py --remote --sim` target, a
  cgroup quota for the bay, then enabling the mode in Pi builds (section 5).
* IGT: put the `test-data/igt` datasets on the simulated FC SD card (the native FC's SD image) for the CSV replay.
* RF NEO-M9N GPS model (the launch site is in the plant config) so Normal-mode GPS gates pass.
* DAQ DAC to ADC patch (section 5), thermocouples and tank wall temperature telemetry, igniter current and continuity
  sensing, power rails and battery discharge, RF link quality versus range, SD-missing fault, weather-shifted pad
  pressure, weathercocking/3-DOF (reuse `tools/flight_profile/flightsim.py`'s atmosphere and wind directly).
* Read `tools/flight_profile/vehicle.json` and the motor curve at bay start instead of the mirrored defaults.
