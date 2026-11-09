# Station modes

One backend binary holds every station mode: `normal`, `hitl`, `test_fire`
and `testing`. Exactly one mode is in force at a time
(`backend/src/runtime_mode.rs`). Each mode behaves exactly as its old
separate build did: launch gates, abort scope, test fire ignoring the AV bay,
HITL manual-only phase controls, overpressure handling and recovery. The same
applies on the primary ground station and on the RRLS.

`firmware_sim` and `raspberry_pi` are still Cargo features.

## Mode at start

1. The saved selection `<GS_STATION_MODE_DIR>/selection.json`
   (default `data/station-mode`), if its role (`primary` or `rrls`) matches.
2. Otherwise `GS_STATION_MODE`: `normal`, `hitl`, `test_fire` or `testing`.
   The old spellings `hitl_mode`, `test_fire_mode`, `hitl-mode` and
   `test-fire-mode` are accepted too.
3. Otherwise `normal`.

`run_groundstation.py --testing|--hitl-mode|--test-fire-mode` sets
`GS_STATION_MODE`. Delete `selection.json` to fall back to the variable.

## Switching from the UI

1. Settings > Station shows **Operating mode and Simulation**. The UI reads
   `GET /api/station-modes` (wire V2, see the UI's `station_mode_contract.rs`).
   It lists all four modes, the Simulation flag where it is supported, and why
   a change is refused right now.
2. **Review mode change** calls `POST /api/station-modes/prepare` with the
   status it read (instance, policy epoch, revision, current and target). The
   server re-checks the rules below. It returns a single-use 15 s review ticket
   and a phrase to type: `SWITCH TO HITL` for an in-process switch, or
   `RESTART TESTING-SIMULATION` for a Simulation change.
3. **Confirm mode change** calls `POST /api/station-modes/confirm`. The server
   checks the ticket, phrase, session and rules again. It writes
   `selection.json` (fsync and atomic rename) and answers `accepted` with a
   pending transition. Then:
   - **Same Simulation flag (in process):** about 0.2 s later the backend
     takes the runtime-mode gate exclusively. While it holds the gate, no
     mode-dependent operation can start. It checks again that the station is
     idle, then publishes the new mode with a higher epoch and resets the
     mode-dependent state (see below). Sign-ins and connections are kept. If
     the station became active, nothing changes, the previous selection is
     restored and the transition is reported as `failed` with the reason.
   - **Simulation change (restart):** about 0.6 s later the backend checks that
     the station is still idle. It then shuts down through the normal SIGTERM
     path and exits with status **75**. The supervisor restarts it into the
     selection. The new process reports the transition as `applied`.

Every option has `transition_kind: "restart"`, meaning the server fences the
switch itself. An option also has `in_process: true` when no restart is
needed. `epoch_fencing_supported` stays `false`: clients never need the
per-request epoch protocol.

`policy_epoch` is `<boot id>:epoch-<n>`, and `n` grows with every switch. A
review or confirmation made under an older epoch is refused. Any operation
admitted under the old mode carries the old epoch and is refused before it
commits.

## What a switch resets

The rule is: whatever a fresh start in the new mode would have.

- HITL operator interlocks (button, launch, safety and physical-launch source)
  are released.
- HITL phase control returns to the ground station. The phase-control revision
  grows.
- GSE qualification state is rebuilt as at start. Outside HITL it is restored
  from the saved qualification file, including the automatic-abort latch. The
  engine status and claim are rebuilt with it.
- Turbocharge evidence and its binding are cleared.
- The sequence step starts over. The testing-mode modeled retraction is
  dropped.
- The load-cell calibration is reloaded when the switch enters or leaves
  testing, since testing uses `backend/calibration/loadcell_calibration_testing.json`.
  The reloaded calibration is then published to the DAQ.
- The action policy is re-derived for the new mode.
- Mode-specific tasks follow the mode epoch on their own:
  - The safety task idles in HITL and test fire and restarts fresh otherwise.
  - The sequence task runs the HITL loop or the automatic loop.
  - The paired settings and shared checklist workers pause in HITL.
- Connected UIs receive a websocket
  `{"ty":"StationModeChanged","data":{"mode","label","policy_epoch","layout_revision"}}`
  frame, then a fresh `ActionPolicy`. They then refetch `/api/layout`, which
  always follows the mode in force.

The abort and launch indicator latches are kept until the process restarts, as
before.

## Who may switch, and when

- An authenticated administrator may switch. So may an operator with Send
  commands whose command list allows `StationMode` (an empty list allows every
  command, as for other commands). Anonymous sessions never may.
- The station must be idle:
  - no GSE sequence, turbocharge, or claimed or manual valve control;
  - no pending Launch;
  - flight state Startup or Idle, and the T clock idle;
  - outputs settled: no pending abort, no cold flow, and no queued abort, valve,
    actuator or flight commands;
  - no board recovery or reset;
  - on an RRLS, not relaying recovery traffic.

  The same check runs at review, at confirm and at the switch itself.
- Normal is never entered while a link uses a DummyComms stand-in. Normal
  refuses to start without its configured links. Fix the link and restart.
- A Simulation change needs the supervisor. A backend started directly can
  still switch modes in process.
- An isolated FlightSim session cannot switch.
- An RRLS switches only itself. The primary's mode is never changed from it.

## What stays fixed at start

- The links opened at start: a missing link becomes a stand-in outside normal.
- The RFD modem startup check runs once, under the boot mode. It is skipped by
  default in HITL and testing. The operator check stays available.
- The simulator clock source (`GS_SIM_CLOCK_FILE`, HITL or `firmware_sim`
  only).
- The Simulation flag.

## Simulation

- **Primary:** Simulation is testing mode's internal flight model
  (`GS_SIM_MODE`, on by default in testing; `GS_SIM_MODE=0` runs testing on
  hardware links). It is offered only with `testing` and is fixed at start.
  A process running the model must restart to leave testing, because several
  checks trust the model flag alone.
- **RRLS:** Simulation is offered in every mode and simulates the paired
  primary (`backend/src/rrls_sim_primary.rs`).
  - The RRLS radio stack is unchanged, but opens the slave side of a local
    pseudo-terminal instead of the radio.
  - On the other side a simulated primary runs:
    - a primary-role recovery mesh (HEALTH, session);
    - the signed station-mode responder;
    - the command-proxy responder. Every forwarded command is answered
      `Unavailable` and never executed.
    - an idle telemetry stream.
  - It uses a fresh simulated installation pairing created at each start.
  - The supervisor points every paired or primary-derived state path into
    `<GS_STATION_MODE_DIR>/rrls-simulation`. No radio is opened.

## Running it

Interactive:

```sh
python3 run_groundstation.py --hitl-mode --switchable-modes all
python3 run_rrls.py --switchable-modes all --radio-port /dev/ttyUSB0
```

`--switchable-modes` builds the backend once (the mode list is accepted for
older callers) and supervises it. The usual mode flag is the default when no
switch has been saved. The binary is copied to
`target/station-modes/<profile>/groundstation_backend`. On a Pi the primary
adds `raspberry_pi`; the RRLS does not.

systemd: build once, then let the unit exec the binary. systemd restarts the
unit after status 75 and the launcher runs again:

```sh
python3 station_modes.py build            # primary on a Pi: add --pi
```

```ini
ExecStart=/usr/bin/python3 /opt/rrls/GroundStation26/station_modes.py exec --rrls --default-mode normal
Restart=on-failure
```

For the primary, use `exec --default-mode <mode>` without `--rrls`. See
[rrls.service](rrls.service). `python3 station_modes.py list` prints the
installed modes and the saved selection.

The supervisor passes `GS_STATION_MODE`, `GS_STATION_MODE_SUPERVISOR=1` and
`GS_STATION_MODE_DIR`, plus `GS_SIM_MODE` for a selected testing mode.

## Cost on a Raspberry Pi (estimate)

Release builds use fat LTO with one codegen unit. One backend link takes about
3 minutes on a 4-core x86-64 host once dependencies are built. A Pi 5 is
roughly 3 to 5 times slower, and a Pi 4 roughly twice that again. There is
only one binary to build. A stripped release binary is about 30 MB.
