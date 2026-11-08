# Mission phases

The station shows six operator phases: **Startup, Idle, Fill, Launch, In flight, Landed**.
They are a ground-station presentation. Each phase is derived from the detailed
`FlightState` byte that all seven boards share, plus a little station context. The wire
value, the firmware enums and every safety rule keyed on the detailed state are unchanged.

`backend/src/mission_phase.rs::mission_phase` is the single mapping function. A test
checks it against this table for every mode, every detailed state and every context
input.

## Mapping

| Detailed state (wire) | Flight build | Test-fire build | HITL build |
|---|---|---|---|
| Startup (0) | Startup | Startup | Startup |
| Idle (1), required boards not all seen | Startup | Startup | Idle |
| Idle (1), fill activity | Fill | Fill | Idle |
| Idle (1) | Idle | Idle | Idle |
| PreFill, FillTest, NitrogenFill, NitrousFill, Armed (2-6) | Fill | Fill | Fill |
| Launch (7) | Launch, or Landed through the comms-loss fallback | Launch | Launch |
| Ascent, Coast, Apogee, Descent, Reefing (8-12) | In flight, or Landed through the comms-loss fallback | Launch | In flight |
| Landed, Recovery (13-14) | Landed | Launch | Landed |
| Aborted (15) | Idle (Startup if boards are unseen); stays In flight or Landed if that was the previous phase | same, but flight phases show as Launch | Idle, or the previous In flight/Landed |

Notes on the inputs:

- **Required boards.** These come from `AppState::board_required_for_progression`. Test-fire
  builds ignore the AV bay (FC, RF, PB).
- **Fill activity.** True while any of these hold:
  - the GSE engine is in a nitrogen-test, fill, pause or turbocharge phase, including the
    nitrogen test's passed pressure hold;
  - turbocharge is active;
  - a cold-flow session is active;
  - qualification is in the Fill or Armed stage, meaning propellant is loaded.

  Test-fire builds stay at detailed Idle through the whole fill, so this input is what
  shows Fill there.
- **HITL is manual-only.** The phase follows the operator-assigned detailed state, and
  board presence and GSE activity are ignored.
- **Test-fire builds have no flight.** Launch covers the launch admission and the burn.
  In flight and Landed are never shown. The operator reset returns the detailed state to
  Idle, which brings the phase back to Idle.

The phase is published on WebSocket as `{"ty":"MissionPhase"}` and on `GET /api/mission_phase`.
The payload carries `phase`, `label`, `detailed` (for the details view), `mode`,
`manual_only`, `aborted`, `fill_activity`, `disconnect_aborts_armed`, `comms_loss_landed`
and `since_ms`.

## Safety boundaries

- **Disconnect aborts** (a board offline for 6 s after the 12 s offline threshold) are
  armed in the **Idle** and **Fill** phases. Before this change they were armed from
  PreFill. Startup is the only pre-Launch phase without them. Two existing detailed-state
  rules still apply on top:
  - Aborted and post-Launch states are suppressed;
  - the Valve Board is exempt.

  The gate is in `safety_task.rs:liveness_abort_allowed_in_state` and
  `gse/automatic_abort.rs:claim_from_source`.
- **No automatic aborts after Launch.** From Launch admission onward (Launch, In flight,
  Landed) no automatic abort of any kind is sent. Operator Abort and Stop stay available.
  The enforcement stays keyed on the detailed state in
  `gse::automatic_abort_allowed` / `stage_allows_automatic_abort`.
  `gse/automatic_abort.rs::no_automatic_abort_path_acts_after_launch` enumerates every
  automatic path:
  - liveness abort;
  - general fault/ADC abort;
  - overpressure abort, including the Actuator Board overpressure abort
    (`gse/overpressure_tests.rs::no_actuator_board_abort_after_launch_admission`);
  - queued automatic abort;
  - automatic supply closes;
  - dispatch filter;
  - observed automatic abort.
- **Rules that stay keyed on the detailed state, unchanged:**
  - launch lock and reset;
  - Abort blocks Launch;
  - turbocharge;
  - overpressure;
  - pause-on-failure;
  - sequence policy.

## FC owns landing

The GS follows the FC-reported state. In flight changes to Landed only when the FC reports
Landed or Recovery. There is one comms-loss fallback, in flight builds only:

- **When it triggers:** the T clock is past T+5 min and the FC has then been silent for
  2.5 continuous minutes. Silence that began before T+5 counts from T+5. The phase then
  shows Landed and the station notifies "FC contact lost for 2.5 min after T+5 min;
  marking Landed."
- **When contact returns:** the FC's reported state wins again. Descent goes back to In
  flight, and Landed stays Landed. The station notifies that contact was restored.
- **Before T+5 min:** comms loss never changes the phase.
- **Scope:** the fallback changes only the displayed phase. It sends nothing and leaves
  the detailed state alone.

Both durations are measured on the procedure clock: the explicit model clock in
firmware simulation, wall time otherwise.

## Deployment notifications

The FC reports each deployment twice:

- as an `ORDERED_MESSAGE`: "PD approx altitude: N" for the drogue and "PR approx altitude:
  N" for the main;
- as the next `FLIGHT_STATE`: Descent for the drogue and Reefing for the main.

`backend/src/recovery_events.rs` turns whichever report arrives first into one persistent
notification per deployment per launch cycle, for example "Drogue parachute deployed at
812 m (FC), 18:04:31 UTC, T+31.2 s". It is also written to the message log. A report
that comes only as a state change uses the latest received altitude. The FC firmware was
not changed.

## Flight maxima

`backend/src/flight_maxima.rs` tracks maximum altitude, speed and acceleration while the
T clock counts up. It keeps one value per source, each with its T+ time:

- **Altitude:** FC Kalman, then barometer, then GPS.
- **Speed:** FC Kalman velocity, then GS vehicle speed.
- **Acceleration:** raw accelerometer magnitude.

The FC's own "EV apogee guess" and "EV maxvel guess" messages are kept alongside these
and shown as authoritative.

The maxima are served on `GET /api/flight_maxima` and on WebSocket as `FlightMaxima`. They
are written to `backend/data/flight-maxima.json`, so they survive a restart as long as the
T+ anchor is the same. They are also recorded as `GS_FLIGHT_MAXIMA` rows at most once a
second. A T-clock reset, which Reset all boards also performs, clears them.

## Board behaviour

About safety has a Board behaviour section, inventoried from the firmware source. It
lists what each board does on its own: heartbeat and fault aborts, the abort latch,
overpressure, watchdog resets and igniter timing.

Tank overpressure before Launch puts the Actuator Board in its Abort safe state
(Nitrogen and Nitrous closed, igniter off, fill-line retraction stopped and
unpowered, all AB outputs off) at both 850 and 950 psi. The Valve Board sends the
AB the automatic overpressure-abort command (`ACTUATOR_COMMAND` 16 with the 0x80
automatic tag, byte 0x90) at its 850 psi relief, and the AB also treats the Valve
Board's tank overpressure Abort report as overpressure, in Startup too. The
station sends the same command at its own 850 psi relief and immediately at
950 psi; its broadcast Abort still follows the 3 s main-valve window. Nothing is
sent or obeyed after Launch. Recovery is Reset all boards, as for any Abort.

The inventory found these paths that could act after Launch. Status after the follow-up firmware fixes:

- **FC vigilant watchdog.** Fixed: it now corrects only to Apogee, so the normal detectors fire
  the drogue and main.
- **FC watchdog-reset recovery.** Partly fixed: automatic pyro stays inhibited after a rejected
  resume, but operator forced deploy commands are accepted (drogue before main).
- **AB/VB launch-latch and backup-domain failures.** Fixed: they keep only an in-RAM launch
  inhibit and never latch Abort.
- **VB reset defaults.** Fixed for Launch: after an accepted, unaborted Launch a reset keeps Vent
  and Dump closed; otherwise they open at boot as before.
- **FC auto-Launch.** Removed: the FC no longer sends itself a Launch on GS loss.

## Later firmware migration

1. **Extend the schema.** Add an optional `MISSION_PHASE` network variable, one byte with
   values 0-5, next to `FLIGHT_STATE` in `telemetry_config.json`, the FC's
   `config/sedsnet.json` and `sedsnet_config.h`. Leave `FLIGHT_STATE` unchanged.
2. **GS publishes the phase.** The GS publishes `MISSION_PHASE` from `mission_phase()`.
   Boards that do not know the variable ignore it.
3. **Boards consume it.** Boards move one at a time from comparisons on detailed states
   to phase comparisons, mainly the disconnect, heartbeat and abort gating and the
   Valve Board's "between Idle and Launch" rule. Each move needs its own HIL test.
4. **FC deployment event.** Add a structured `RECOVERY_EVENT` (kind, altitude, FC tick).
   Fix the FC paths that report a deployment the pulse refused: `deployment_pulse_start`
   returns void, and the `"%u"` format string in the reef path is wrong. The GS can then
   stop parsing text. The FC is at 98.6% flash, so check the size first.
5. **Retire.** Remove the GS-side derivation only after every board has switched over and
   a full flight simulation passes. Keep the detailed state for logs and the details view.
