# Flight-computer operator commands

The station sends the FC one-byte commands on its compatibility protocol
(`FlightComputerCommands`, `backend/src/rocket_commands.rs`; the FC's
`remote_cmd_compat` and `extmap`). The operator set is deliberately small.

| Group | Control | Command | Wire byte | FC decree | Where |
| --- | --- | --- | --- | --- | --- |
| Sensors | Reinit Sensors | `ReinitSensors` | 17 | `Reinit_Sensors` | HITL layout; FC accepts in any flight state |
| Sensors | Reinit Barometer | `ReinitBarometer` | 18 | `Reinit_Barometer` | HITL layout; any flight state |
| Sensors | Reinit IMU | `ReinitIMU` (formerly `EnableIMU`) | 19 | `Reinit_IMU` | HITL layout; any flight state |
| Sensors | Disable IMU | `DisableIMU` | 20 | `Disable_IMU` | HITL layout; any flight state |
| Recovery | Deploy drogue | `DeployParachute` | 12 | `Deploy_Parachute` | HITL layout; manual, any flight state (unchanged) |
| Recovery | Deploy main (reef release) | `ExpandParachute` | 13 | `Expand_Parachute` | HITL layout; manual, any flight state (unchanged) |
| Logging | Retry FC SD | `FcSdRetry` | 33 | `SD_Retry` | every mode with an FC (below) |
| Flight state | State +1 / State -1 | `AdvanceFlightState` / `RewindFlightState` | 21 / 22 | `Advance_State` / `Rewind_State` | HITL only (test fire steps the station's own state) |
| Ground test | Enter / LIVE PYRO / Dry-run / Exit | `IgtEnter` .. `IgtExit` | 29..32 | `IGT_*` | integrated ground test mode (and HITL with the replay); below |
| Sequence | none (no button) | `Postinit`, `Launch` | 0, 1 | `Postinit_Signal`, `Launch_Signal` | sent by the station's launch sequence |

Reset all boards (board recovery op 3, `RESET_STARTUP_ANY`) returns the FC to
Startup from any state. That reset also cancels the FC's evaluation (what the
retired Evaluation Abort did): the estimator is stopped and its flight memory
(filter history, extremes, plausibility verdicts, break-up detector, drogue
timer, sensor-failure counts and thresholds) returns to its boot values, so
nothing from a previous evaluation survives into the next flight. The station
sends no separate FC command for it.

While the operator Abort is latched the station sends the FC no command at
all (`gse.rs` `abort_safe_control`) until Reset all clears it.

## Retired bytes

These bytes are reserved and never reused. The FC refuses them (acknowledged,
never executed) and keeps its compiled defaults:

| Bytes | Retired command | FC default kept |
| --- | --- | --- |
| 2 / 3 | MonitorAltitude / Revoke (`Vigilant_Mode`) | on (the FC also sets it itself on a GPS fallback) |
| 4 / 5 | ConsecutiveSamples / Revoke (`Eval_Successive`) | off |
| 6 / 7 | ResetFailures / Revoke (`Reset_Failures`) | on (the FC also sets it on ground-station loss) |
| 8 / 9 | ValidateMeasms / Revoke (`Measm_Reports`) | on (the FC also sets it on a GPS fallback) |
| 10 / 11 | velocity checks / Revoke (`Velocity_Checks`, never sent by the GS) | on |
| 14 / 15 | EvaluationRelax / EvaluationFocus | - |
| 16 | EvaluationAbort | replaced by the reset's evaluation cancel |
| 23..25 | AbortAfter40 / 100 / 250 (`Abortion_Thresholds`) | `TO_ABORT` 120 failures |
| 26..28 | ReinitAfter15 / 30 / 50 (`Reinit_Thresholds`) | `TO_REINIT` 40 failures |

Custom layouts and per-command account grants that name a retired command no
longer match any control; an account grant of `EnableIMU` is read as
`ReinitIMU`.

Tests cover every visible flight command's payload and discovered destination
using captured packets only. They do not trigger hardware outputs.

## Integrated ground test controls

The integrated ground test (`docs/integrated-ground-test.md`) adds four FC
commands on the same one-byte protocol. They exist in the
`integrated_ground_test` station mode (Actions > Ground test) and in HITL,
where they are refused unless the HITL replay setting is on. They are refused
and not offered in every other mode. A flight FC image ignores them.

| Control | Command | Wire byte | FC compat name | Station path |
| --- | --- | --- | --- | --- |
| Enter ground test | `IgtEnter` | 29 | `Compat_IGT_Enter` | review only: `POST /api/igt/prepare` then `/api/igt/confirm` |
| LIVE PYRO | `IgtLivePyro` | 30 | `Compat_IGT_Live_Pyro` | review only, Admin, typed `LIVE PYRO` |
| Dry-run | `IgtDryRun` | 31 | `Compat_IGT_Dry_Run` | plain command |
| Exit ground test | `IgtExit` | 32 | `Compat_IGT_Exit` | plain command |

The plain command path (`POST /api/command`, the RRLS proxy) refuses
`IgtEnter` and `IgtLivePyro`.

## Retry FC SD

`FcSdRetry` (FC byte 33, FC compat name `Compat_SD_Retry`) asks the FC to
re-attempt its SD mount and log file. Both FC images accept it. It is offered
in every station mode with an FC (normal, testing, HITL and the integrated
ground test; not test fire) in the Flight computer section, on the plain
command path. It needs the `FcSdRetry` command permission and a fresh FC
BOARD_HEALTH, has a 5 s cooldown, and is refused while word 6 bit 31 shows a
retry already pending. Word 6 bit 30 raises the alert "FC SD card not mounted
/ not writable" until it clears.
