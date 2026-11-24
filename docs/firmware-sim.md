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
| Station clock | wall | the bay's virtual clock (simclock), up to ~10x | the bay's virtual clock (simclock), the bay paced at 1x |
| Plant | `flight_sim/ground_plant.rs` from the station's own valve state | scenario stimulus files, plant from the station's reported valves | plant from the boards' own output pins and servo registers |
| Flight | open-loop profile in the station | FC sensors at rest natively (Renode replays a fixed profile) | 1-D flight driven into the FC's real baro/IMU; descent follows the FC's real pyro outputs |
| Setup cost | none | a cohort, a station image, a scenario | one `tools/firmware_sim.py build` (about 2.5 min), then a mode switch |

What it catches that the existing paths do not:

* The firmware's actual answer to an operator sequence: refusals, interlocks, safety latches (VB overpressure
  relief and abort, continuity loss, heartbeat timeouts), watchdog and reset behaviour, the AB/VB launch sequence
  timing, the FC's state machine, Kalman filter and deployment logic, DAQ input-health faults. `flight_sim` only
  shows what the station believes the boards would do.
* Wire-level integration with the station: the real SEDSnet protocol, router, timesync, discovery, bundles and
  config sync over the same radio PTY and Pico-Fi umbilical transports the native fullnet uses, with the bay paced
  at real time and the station following its virtual clock (section 2, "Station clock").
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
    supervisor: backend with GS_FIRMWARE_SIM=1, the bay's endpoints, isolated paths, simclock preloaded on the
                bay's clock page (endpoints.json station_clock)
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

### Station clock

In firmware-sim mode the station runs on the **bay's virtual clock**, so host load can slow the whole simulation but
cannot skew its clocks. The bay (`scripts/gs_bay.py`, `--station-clock follow`, the default) publishes its clock page
(FirmwareSimulator `native/simclock`: the current lockstep window and its measured rate) and names the page, its epoch
(the wall clock at virtual 0) and the simclock library in `endpoints.json`. The supervisor preloads the library into
the backend (`LD_PRELOAD`, `SIMCLOCK_FILE`, `SIMCLOCK_EPOCH_MS`, `GS_STATION_CLOCK=firmware_sim_bay`). Every clock the
backend reads (`Instant`, `SystemTime`, tokio timers) and every timed wait it makes then follow virtual time, without
station code changes:

* network time, valve-report freshness, heartbeats, acknowledgement timeouts and the GSE sequence timers are judged
  on the boards' own time base. A bay that slips under load stops the station's clock with it, so no report looks
  stale or ahead;
* the bay still drops lag beyond 20 ms instead of racing to catch up (`slipped_ms`), which spares the host the
  catch-up burst. The station's dates fall behind the wall by the slipped time (the UI accepts rows up to 20 min old);
* when the bay stops or crashes, `gs_bay.py` releases the page and the station's clock runs on at the wall rate from
  where it stood, so the backend never freezes;
* `tools/firmware_sim.py build` builds `bin/simclock.so` into the cache (bundles carry it); without it the bay builds
  one from the engine sources, and without a compiler the station stays on the wall clock;
* only a firmware-sim start can do this. The supervisor strips any simclock preload or page from every other start
  (`station_modes.without_station_clock`), and a backend that is not in firmware-sim mode refuses to start with one
  (`firmware_sim_mode::refuse_foreign_clock`). `GET /api/firmware-sim` reports `station_clock` (`firmware_sim_bay` or
  `wall`), and the bay's `status.json` stamps `updated_unix_ms` on the station's clock.

Tests: `tests/test_station_modes.py` (only firmware-sim follows the bay; every other mode, RRLS included, gets the
wall clock), `firmware_sim_mode::tests::only_firmware_simulation_follows_the_bay_clock`, and FirmwareSimulator
`tests/test_gs_bay_station_clock.py` (simclock passes the wall clock through without a page, holds a stalled bay's
time, runs on after release). `tools/firmware_sim_e2e.py` waits on the same clock (its pauses and timeouts are virtual
seconds read from `status.json`, with a wall fallback once the bay has stalled for a minute).

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

* AB drive enables `N2_Valve_EN` (PB3), `N20_Valve_EN` (PB4), `Igniter_EN` (PB7) (PB12-14, the `*_Sig` lines, are ribbon inputs) and VB `Solenoid` (PB11, pilot) from the boards' GPIO
  reports; VB vent (normally-open servo, TIM2 CH2) and dump (TIM2 CH1) from the timer compare registers in the VB's
  mapped peripheral page, open below 860 us (open ~530 us, closed ~1200 us on the VB's 500-2500 us/270 degree
  mapping); FC `CO2` (PB5, drogue) and `REEF` (PB6, main) pyro outputs.
* Operator inputs (`POST /api/firmware-sim/control`, administrators): `plant` (QD connected, nitrogen bottle open,
  nitrous bottle open), `conditions` (ambient temperature, wind speed, direction, gusts), `loads` (calibration
  masses on the KG50 and KG1000), `faults` (below). Every change is logged and the current control is written into
  saved captures (`versions.simulated_conditions`).

Fill system:

* **Nitrous**: saturated liquid/vapour in the 15.85 L run tank (measured 967.45 in³, `vehicle.json` `run_tank_volume_l`) with ESDU 91022 property correlations (vapour pressure,
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
1-D vertical dynamics with the vehicle of `tools/flight_profile/vehicle.json` (33.2 kg dry, 1.1 kg fuel, 6 in, Cd
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

Additions since the MVP:

* **Vehicle and motor from `tools/flight_profile`** (read at every bay start, so the apogee planner's and the fire
  archive's updates flow through): dry mass, diameter, drogue/main rates, launch site from `vehicle.json`; the
  motor's delivered Isp from the motor summary (impulse per kg of N2O, e.g. 9.19 kN s / 5.6 kg gives about 140 s);
  `motor.total_impulse_ns` in `vehicle.json` rescales it. The injector flow and the chamber pressure are solved
  together. The provenance (files, impulse, Isp) is in the status `firmware._flight_profile`.
* **Stand behaviour**: the KG50 carries the hanging rocket minus thrust until lift-off and sits at its zero-load
  reading while lifted; the KG1000 sees thrust minus the current mass after an 80 ms gap with a contact spike; the
  rocket drops back onto the KG50 at tail-off, so the vapour tail shows only there.
* **Winds aloft**: `conditions.wind_levels` (`[{alt_m, speed_mps, from_deg}]`, the apogee planner's weather
  snapshot) or the `vehicle.json` power law (exponent 0.14, cap 1.4) drift the vehicle.
* **RF NEO-M9N GPS** (`sim_gps.c`, SPI): GGA and RMC once a second from the flight model's position (launch site plus
  drift, altitude), UBX ACK for configuration; fault `gps_no_fix`. Normal mode's GPS gate passes and the rocket fix
  reaches the map.
* **FC SD card**: a FAT32 image written by the launcher (`scripts/fat32_image.py`, no mkfs) carrying the selected
  `test-data/igt/<set>/IGT/*.CSV`; fault `fc_sd_missing` pulls the card at run time; a dataset chosen while the card
  is out is written then (swap: card out, dataset, card in, `FcSdRetry`). IGT mode runs the FC ground-test variant
  (`INTEGRATED_GROUND_TEST=ON`, built alongside) as two bodies: the motor, tank and plumbing burn on the stand as in a
  test fire, while the avionics bay lies still on the ground apart from it (FC sensors at rest, horizontal by default:
  plant `fc_attitude`, ambient pressure, GPS at the site; no thrust or vibration coupling). The FC's flight comes
  only from the CSV replay.
* **DAQ DAC to ADC patch** (simulated loopback cable): the plant reads the DAQ's own DAC1 registers, trigger timers,
  GPDMA source table and SystemCoreClock, so any waveform or table the firmware plays reaches the patched input
  through the real acquisition path (MCP CH2-5, HVAIN1/2, P6 differential, KG1000/KG50 amplifiers with an
  attenuator; SAR inputs are not modelled). Control `patch: {dac1, dac2, dac1_attenuation, dac2_attenuation}`;
  fault `p10_inputs_swapped` latches the buffer to a rail (the unreworked board). The table is sampled every 10 ms of
  plant time normally and every 1 ms while a patch is active (see the DAQ frame engine section).
* **AB current sense**: the N2O/N2 solenoid and igniter channels (ADC1) read current while energized; fault
  `igniter_open` reads no igniter current.
* **RF link quality**: packet loss grows with the slant range (none to 3 km, about 30 % at 12 km).
* Control sections merge: a POST replaces only the sections it sends.

## 4. FC simulated flight

The FC runs its real estimator and recovery logic. The coordinator's flight model drives the FC's BMP390/BMI088
models through the native shim (`sim_flight_sensors.c`, "flightsens" device, from the first plant step): the pad
before liftoff, then the flight. Pyro outputs are observed and change the descent model. The vehicle parameters
mirror `tools/flight_profile/vehicle.json` (15 ft x 6 in, 33.2 kg dry) and the 11-16 motor; with the measured
motor the vehicle reaches about 1.4 km, `motor.impulse_scale` about 1.8 gives the 10 kft profile.

## 5. Raspberry Pi 5, bundles and the remote build

### Pi 5 setup

The board executables are 32-bit (the firmware keeps pointers in 32-bit fields and the shim maps the MCU address
map at its real addresses), so on an aarch64 station they are **armhf, statically linked** (no armhf libraries on the
host or in the station image), next to an aarch64 coordinator. They need the kernel's 32-bit compat, which arm64
Linux offers only with 4 KB pages:

* **Ubuntu (raspi kernel, as the station runs: 6.8.0-*-raspi, `CONFIG_ARM64_4K_PAGES=y`, `CONFIG_COMPAT=y`)**: no
  kernel change; armhf binaries run as they are.
* **Raspberry Pi OS** with its default 16 KB-page Pi 5 kernel: set `kernel=kernel8.img` in
  `/boot/firmware/config.txt` (4 KB pages, 32-bit compat) and reboot.

The launcher checks at start and refuses with that message (`run/refused.txt`, shown in `GET /api/firmware-sim`
`bay_refused`) when the page size is not 4 KB or the kernel cannot execute a 32-bit binary. A `raspberry_pi` build
offers the mode only when an armhf bundle is installed (manifest `host_abi: "armhf"`). The bay processes run at nice
10; in Docker give the station container the bundle as a volume and leave the bay inside the station's container
(`-v ~/.cache/seds-firmware-sim:/root/.cache/seds-firmware-sim`, or set `GS_FIRMWARE_SIM_CACHE`); a `--cpus` quota
on that container bounds the station and the bay together (one core is enough for the bay at 1x by the jupiter
measurement below). A bay crash never takes the backend down; stopping the backend stops the bay.

### Bundles

`python3 tools/firmware_sim.py bundle --out FILE` packs the built cache as a relocatable bundle (board executables
with debug info stripped, board layouts, symbols, the coordinator, the launcher scripts and `manifest.json` with every
board's repo/ref/commit/subject, SEDSnet, the engine and `host_abi`): 15 MB compressed, 38 MB installed for x86_64.
`python3 tools/firmware_sim.py install FILE` unpacks one into the cache. `stage --out DIR` writes the sources at
their resolved refs as plain trees (no git, a copy per build because SEDSnet's prepare step rewrites its tree) and
`build --sources DIR` builds from them, which is what container and remote builds use.

### Remote build

`./build.py --remote HOST sim-bundle` stages the sources locally, sends them with the station checkout, builds in the
station builder image on HOST (now also carrying `gcc-multilib`, `gcc-arm-none-eabi`, `gcc-arm-linux-gnueabihf`
with the armhf libc, ninja and the Rust `i686`, `armv7-unknown-linux-gnueabihf` and thumb targets) into a cache kept
on HOST, and copies the bundle back to `target/firmware-sim-bundle.tar.gz` and installs it. Run from the Pi it builds
an armhf bundle (`FIRMWARE_SIM_HOST=armhf`, aarch64 coordinator); from an x86_64 machine an i686 one.
`./build.py --remote HOST --sim ...` adds the bundle to an ordinary station build. No credentials leave the local
side (the password stays in SSH_ASKPASS). Without `--remote`, `./build.py sim-bundle` builds locally.

Verified: the x86_64 bundle/install round trip and a git-free staged build of a board on jupiter; the remote command
sequence by mocked tests (`tests/test_remote_build.py`). **Not verified**: an actual remote run, the armhf cross build
(jupiter has no armhf cross compiler; the builder image has one) and running on a Pi.

### Appendix: a 64-bit port (not planned)

Running the boards as aarch64 processes would mean auditing every place the firmware or its libraries keep a pointer
in a 32-bit field: ThreadX (`ULONG` thread entry inputs, timer and queue message words, `tx_byte_pool` block links),
DMA descriptors and peripheral registers written with `(uint32_t)(uintptr_t)` addresses (GPDMA linked lists, SDMMC,
the DAQ DAC table), FileX media/driver words, SEDSnet's embedded-target allocator interface and any packed structs
with pointers, plus the shim's own MMIO and ThreadX port. Estimated at weeks per board family and it would lose the
32-bit memory layout that makes native runs faithful. The armhf route above needs none of it.

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

Per mode (the e2e: `python3 tools/firmware_sim_e2e.py --out DIR --mode <mode>`):

* **HITL**: fill by hand (Nitrogen/Nitrous/NormallyOpen/Dump), AdvanceFlightState to Armed, Launch: the VB opens the
  pilot at T+10 s, the plant lights the motor (no igniter in HITL) and flies; the FC runs its estimator and fires its
  pyro outputs.
* **Test fire**: the fill-side boards on the umbilical. Armed comes from the station's own GSE procedure (valve
  self-test, nitrogen test, fill to target), not from manual valve commands; the e2e drives that procedure: a dry
  valve self-test (the e2e closes both supply bottles first, as the self-test confirmation asks), the nitrogen test
  (dump at the hold), Start fill (the automatic KG50 zero, then the fill to its mass target), Launch, ignition on
  the stand (AB igniter, VB pilot), the burn, burnout and the station's burn report.
* **Normal**: the RF NEO-M9N fix reaches the station, so the GPS gate passes; the e2e then drives the full
  procedure (self-test, nitrogen test, fill, QD retract, Launch) and follows the flight on the map
  (`/api/antenna/status` rocket positions from the RF GPS).
* **IGT**: the FC ground-test variant; pick a dataset in the sim panel (card out, dataset, card in, Retry SD), Ground
  test > Enter ground test, fill, Launch: the FC (lying apart on the ground) replays the CSV flight while the motor
  burns on the stand. The FC refuses Enter (reason 11, "refused (wrong state)") while it still counts the station as
  lost, e.g. a few seconds after the radio path came back at boot; press it again. Swapping the card's contents while
  the FC runs (card out, another dataset, card in, Retry SD) currently ends in an FC fault "CSV parse error" (code 4)
  although the same files pass when the card holds them from power-on: the FC reads the new card through what it
  cached of the old one (FileX media not reopened on Retry SD; to confirm on hardware with a real card swap). The
  e2e therefore powers on with the dataset on the card (`FIRMWARE_SIM_CONTROL={"igt":{"dataset":...}}`).
  After the replayed landing the station collects the FC's SD launch file over port 4511 as after a flight (the
  ground-test variant carries the SD read service) and records which `test-data/igt` dataset was on the card
  (`docs/data-pipeline.md` §3.4a).
* **Testing**: the mode's own logic against the simulated boards (the e2e checks the links, the DAC waveform engine
  through the simulated loopback cable (a 20 Hz sine on DAC1 patched onto MCP3564R CH2, judged by the station's DAC
  validation) and the fill rules).

Sim panel: Settings shows it while the station runs simulated firmware (revisions, pace, plant for everyone; plant
inputs, conditions, calibration masses, faults and the IGT dataset for administrators). The DAQ DAC page shows the
simulated loopback cable for patching the DAC outputs onto DAQ inputs.

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

Verified 2026-11-23 (r5, single bay): test fire through the valve self-test (dry), the nitrogen test, the automatic
KG50 zero and the fill to its 5 kg target; IGT to FC `Ready` with the dataset on the card from power-on. Not verified
end to end yet (host overloaded by other builds while these ran; see the next section): test-fire ignition and burn
report with the igniter on PB7, Normal through Launch and the map track, IGT Launch and replay, the DAC patch
validation, aarch64, the remote build.

### Valve-report time stamps and the bay's pace (2026-11-23)

Test-fire runs showed the VB/AB valve-state reports refused as clock-skewed (+100 ms to -5 s window around station
network time), VB/AB valve state STALE and the valve self-test pausing on "Valve acknowledgement timed out ... no
fresh report", together with "requesting RRLS relay" and "rocket_comms send_data failed: operation would block" on
the radio PTY. Traced with every received stamp `(epoch, timestamp)` against the station clock:

* the station's network clock never stepped (epoch changes were only "renamed", delta 0); the boards' clocks moved;
* the bay had fallen behind its pace (the first virtual second took 12 s while every board booted; later a slow
  FC boot or host load) and then ran 3-4x real time to catch up. Every board's clock follows the station's on
  virtual time, so during the catch-up the boards ran ahead of the station's wall clock (up to +3.7 s), named
  diverged epochs (the station adopted them, delta 0) and kept small offsets (+100 to +300 ms) for minutes; the
  valve reports were then refused as ahead;
* while the bay lagged, its radio PTY was read at most 8 KiB per (slow) step: the station's writes failed with
  "operation would block" and the AV-bay frames came late, hence the RRLS relay requests;
* the GPS model plays no part (the same with no fix).

**Mostly a sim artifact, plus one real station bug.** No hardware oscillator runs 3-4x fast: that part is fixed in
FirmwareSimulator (the station-managed bay drops lag beyond 20 ms instead of racing, `slipped_ms` and `slip_events`
in `run/status.json`; the radio PTY is drained every step and fed to the modem at the UART baud; the shim keeps the
DAQ frame engine on time). The real bug: the station's periodic network-time refresh read the host clock before the
step policy and its locks and applied the reading as "now"; a refresh that waited 1.5 s (here a host under memory
pressure) stepped the station's network clock back 1485 ms (new epoch) and a second later forward again (another
epoch), and every board re-anchored twice. Steps are allowed in Startup/Idle, i.e. during the whole GSE procedure of
a test fire, so on hardware any stall of the router worker longer than 1 s (a blocked radio write, a busy Pi) does the
same. Fixed in the station (`system_clock::refresh_network_source_now`, the wait added back on the router's
monotonic clock); the -7.6 s then +1.0 s jumps reported earlier fit this pattern. The SEDSnet epoch rules (stamps
translated across steps, `network_stamp_is_fresh`) behaved as designed and were not changed.

A single bay on an otherwise quiet host holds pace with a few hundred ms slipped per run and no refused valve report.
The limit stays the host: with three bays and builds at once (or heavy swap) the bay slipped 12-65 s in a few minutes
and the boards' disciplined clocks wandered (+100 to +270 ms ahead, or seconds behind), so valve reports were refused
again. Run one bay at a time and watch `slipped_ms`; making the station follow the bay's virtual clock would remove
this limit (follow-up).

### DAQ frame engine (DAQ-Board 99db2e6 and later)

The DAQ's MCP3564R frame engine runs natively: the shim emulates TIM2 at register level (a live CNT from virtual
time, CCRx compare matches and EGR events setting SR.CCxIF with rc_w0 clears, calling `mcp3564r_timer_irq`, the body
of the DAQ's replaced TIM2 vector), and the MCP3564R model converts the SCAN list once per START in one-shot mode
(CONFIG3 0xB0) and returns to standby. A third of the frames used to time out: the slot compare (CCR2/DIER written
from the SPI DMA completion) was only scheduled once virtual time moved, and the idle skip jumped past it to the next
frame tick. FirmwareSimulator now refreshes the live registers before every idle skip: 0 timed-out scans, 0 skipped
frames and slots in 5000 frames (`g_mcp3564r_diag`, before 1742/1577/1576), so the KG50 and slow items arrive at
their full rates (KG50 20 Hz, KG1000 50 Hz).

DAC patch sampling: while a DAC output is patched the plant steps every 1 ms (else every 10 ms), so with the 1 kHz
frame engine waveforms up to about 250 Hz come through faithfully (500 Hz Nyquist).

## 7. Follow-ups

* Run the remaining e2e scenarios on a quiet host (test-fire burn and burn report, Normal launch and map track, IGT
  Launch and replay, Testing DAC patch); the station on the bay's virtual clock, so host load slows the whole sim
  instead of skewing its clocks.
* FC: a card swapped while running (Retry SD) is read through FileX's cache of the old card (IGT CSV parse fault).
* Thermocouples and tank wall temperature telemetry, power rails and battery discharge (the LTC2990 monitors still
  read static values), weathercocking/3-DOF attitude, SAR inputs in the DAC patch.
* An actual remote bundle run, the armhf cross build and a run on the Pi station.
