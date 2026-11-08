# Station mode switch

Station modes are compile-time builds: `normal` (default features),
`hitl` (`hitl_mode`), `test_fire` (`test_fire_mode`) and `testing` (`testing`).
A process never changes its own mode. Instead the station keeps one installed
binary per mode and switches by a controlled restart. This works the same way
on the primary ground station and on the RRLS.

## End to end

1. Settings > Station shows **Operating mode and Simulation**. The UI reads
   `GET /api/station-modes` (wire V2, see the UI's `station_mode_contract.rs`).
   It lists only the modes this installation has built, each with a
   Simulation flag where that build supports one, and why a change is refused
   right now.
2. **Review mode change** calls `POST /api/station-modes/prepare` with the
   status it read (instance, epoch, revision, current and target). The server
   re-checks the rules below and returns a single-use, 15 s review ticket and a
   phrase to type (`RESTART HITL`, `RESTART TESTING-SIMULATION`, ...).
3. **Confirm mode change** calls `POST /api/station-modes/confirm`. The server
   checks the ticket, phrase, session and rules again, writes
   `<GS_STATION_MODE_DIR>/selection.json` (fsync and atomic rename), answers
   `accepted` with a pending transition, and about 0.6 s later checks that the
   station is still idle. If it is, the backend shuts down through the normal
   SIGTERM path (same drain and GPIO reset) and exits with status **75**. If
   the station became active, the restart is cancelled, the previous selection
   is restored and the transition is reported as failed with the reason.
4. The supervisor (`station_modes.py`) sees 75, reads the selection and starts
   the selected binary. The new process reports the transition as `applied`
   (or `failed` if a different build had to start). The UI's sign-in survives
   (sessions are in `data/users.db`), it reconnects on its own and shows the
   new mode. A refusal shows the server's reason.

Options use `transition_kind: "restart"`. The restart replaces the whole
backend, so it is its own fence: the server reports
`epoch_fencing_supported: false` and never closes ordinary command admission,
and the UI does not wait for the in-place epoch protocol for restart options.

## Who may switch, and when

- An authenticated administrator, or an operator with Send commands whose
  command list allows `StationMode` (an empty list allows every command, as for
  other commands). Anonymous sessions never may.
- Only when the station is idle: no GSE sequence, turbocharge or claimed/manual
  valve control; no pending Launch; flight state Startup or Idle; T clock idle;
  outputs settled (no pending abort, cold flow, or queued abort/valve/actuator/
  flight commands); no board recovery or reset; on an RRLS, not relaying
  recovery traffic. The same check runs at review, at confirm and just before
  the restart.
- Only under the supervisor, with at least two builds installed. A backend
  started directly (`cargo run`, an old unit) reports the switch as unavailable.
- An RRLS switches only itself. Nothing is sent to the primary, the primary
  protocol cannot be changed remotely, and the RRLS's displayed primary mode
  still follows the primary exactly as before.

## Simulation

- **Primary:** Simulation is the testing build's internal flight model
  (`GS_SIM_MODE=1`), so it is offered only with `testing`. HITL, test-fire and
  normal builds compile the model out, by design: a flight or test-fire build
  cannot be put into simulation by a runtime flag. `testing` without
  Simulation runs with `GS_SIM_MODE=0`.
- **RRLS:** Simulation is offered in every build and simulates the paired
  primary (`backend/src/rrls_sim_primary.rs`). The RRLS radio stack is
  unchanged but opens the slave side of a local pseudo-terminal instead of the
  radio. On the other side a simulated primary runs a primary-role recovery
  mesh (HEALTH, session), the signed station-mode responder, the command-proxy
  responder (capability with an all-unknown idle observation; every forwarded
  command is answered `Unavailable` and never executed) and an idle telemetry
  stream (flight state, pad GPS, battery). Settings, T-clock and pairing
  exchanges are not simulated. It uses a fresh simulated installation pairing
  (`simulated-primary`/`simulated-rrls`) created at each start, and the
  supervisor points every paired or primary-derived state path into
  `<GS_STATION_MODE_DIR>/rrls-simulation`; the backend refuses to start the
  simulation otherwise. No radio is opened, so no radio output is possible.
  The UI marks it with the Simulation banner, the `SIMULATED paired primary`
  option label and `RRLS observer · SIMULATED paired primary (no radio)`.

## Running it

Interactive:

```sh
python3 run_groundstation.py --hitl-mode --switchable-modes all
python3 run_rrls.py --switchable-modes all --radio-port /dev/ttyUSB0
```

The usual mode flag is the default when no switch has been saved, so behaviour
is unchanged until someone switches. `--switchable-modes` takes a comma list
(`normal,hitl,test_fire,testing`) or `all`. Each mode is built with the same
features as `build.py` and copied to
`target/station-modes/<profile>/<mode>/groundstation_backend`. Cargo keeps
per-feature artifacts, so an unchanged mode is not rebuilt on the next start.
On a Pi the primary adds `raspberry_pi`; the RRLS does not.

systemd: build once, then let the unit exec the selected binary. systemd
restarts the unit after status 75 and the launcher runs again:

```sh
python3 station_modes.py build --modes all            # primary on a Pi: add --pi
```

```ini
ExecStart=/usr/bin/python3 /opt/rrls/GroundStation26/station_modes.py exec --rrls --default-mode normal
Restart=on-failure
```

For the primary use `exec --default-mode <mode>` without `--rrls`. See
[rrls.service](rrls.service). `python3 station_modes.py list` prints the
installed builds and the saved selection. Delete `selection.json` to return to
the default.

## Cost per mode on a Raspberry Pi (estimate)

Release builds use fat LTO with one codegen unit. On a 4-core x86-64 host one
mode's backend link takes about 3 minutes (one LTO core) once dependencies are built. A Pi 5
is roughly 3 to 5 times slower (about 10 to 15 minutes per mode), a Pi 4
roughly twice that again. Dependencies are shared between modes, so the first
mode also pays the one-time dependency build. Disk: a stripped release binary is about
30 MB; with the copy in `target/station-modes` and Cargo's own per-feature
artifact, allow about 70 MB per mode, under 300 MB for all four.
