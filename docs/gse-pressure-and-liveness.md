# GSE pressure limits and connection status

Automatic nitrogen checkout uses the GSE nitrogen target plus **100 psi** as
its failure threshold. Nitrous filling uses the nitrous pressure target from
the fill-target settings plus **50 psi**. Nitrous pressure is captured when a
new fill starts, and retained while paused/resumed. A configured hardware
pressure ceiling can only lower either threshold. Reaching the threshold
requests supply closure and dump/vent opening; it is not a substitute for
independent hardware pressure protection.

The optional hardware ceiling is no longer required just to enable automation.
The maximum acceptable empty-tank pressure-transducer offset still must be
configured by the operator. Fresh pressure, valve confirmations, interlocks,
dry-self-test confirmation and nitrogen-test completion remain enforced.
The GSE settings panel exposes the offset and hardware ceiling. Action-panel
diagnostics show configuration errors and the last rejection reason.

Software command duplicate suppression uses monotonic elapsed time, independent
of GPS/network time corrections. Rejected duplicate clicks do not extend the
window. Different command payloads, including explicit close commands, have
separate keys, and Abort is never suppressed by this check.

Cached topology is not evidence that downstream boards remain powered. Board
status uses actual sender observations and aged discovery announcers, not
repeated reads of connection names. All seven remote boards expire using the
same liveness timeout. The graph retains known offline nodes and displays their
last-seen timestamp and age; these are not connection-session uptime values.

Tests cover target boundaries, lower hardware caps, invalid targets, stale
pressure, missing valve acknowledgements, command debounce, and expiration of
every remote board. Hardware actuation and Raspberry Pi UI validation remain
separate deployment checks.

## Nitrogen and Auto Fill regression tests

Run `cargo test -p gse_sequence` and
`cargo test -p groundstation_backend --features hitl_mode`.
The nitrogen PT trace exercises 50, 100 and 120 psi steps: each overshoots by
4 psi, settles to target +2 psi during the two-second settling window, then
holds with small noise for five seconds. It cannot advance during that hold.
A continuing pressure decline after settling must fail without advancing.
Completion still requires dumping back to the measured empty-tank band.

Backend button tests decode the frontend command names, exercise the actual
command handler and send valve commands through two in-memory SEDSNet routers
with discovery and ordered protocol acknowledgements. Valve-state confirmations
and PT/mass readings are supplied by the test, not physical hardware. Tests
check setup commands, fill prerequisites, waiting for valve confirmations,
opening nitrous, and closing at the target mass. Frontend unit tests check the
button command names and WebSocket message payloads. These are software tests,
not browser-driven clicks or qualification of a physical pressure system.

## Operational qualification and restart

Outside HITL, checkout proceeds in this order: valve self-test, nitrogen test,
nitrous fill to the saved target, launch eligibility. Completed checks and the
measured nitrogen baseline are stored atomically in config/gse_qualification.json
(override with GS_GSE_QUALIFICATION). They survive backend and board restarts.
A corrupt or unwritable record blocks automation.

Use **Reset operation checks** in Actions to clear them deliberately. The
backend requires command permission, valve-self-test access, Startup or Idle,
and no running sequence. Resetting checks does not actuate an output or replay
a queued launch. Changing pressure-test limits after checkout requires resetting
the checks first.

Startup configuration waits for fresh pressure, the operating interlock and
two seconds of stable fill-system routes. Empty checkout closes supplies and
pilot and opens dump/vent. A previously qualified filled tank is held closed
after a service restart. Test-fire requires gateway, valve, actuator and DAQ;
it does not require the absent AV bay.

During nitrogen pressurization all other valves are closed; nitrogen closes
during settling and hold periods. Nitrous filling opens vent/nitrous and closes
pilot/dump/nitrogen. Unexpected nitrogen flow during nitrous fill, or nitrous
flow during nitrogen checkout/filled hold, latches a fault immediately on the
report rather than waiting for the five-second acknowledgement timeout.
Steady pilot/dump/vent mismatches request the intended sequence state; supply
valves are never automatically reopened from an unsolicited report.

Before Launch, connectivity loss during an operation latches an abort. Flight
launch additionally requires fresh RF rocket GPS, live FC command/return
routes and fresh closed-valve reports. Retract QD must receive a new positive
actuator acknowledgement; the five-second settling interval starts at that
acknowledgement. An early launch click queues once, blinks the T-clock and
reports the remaining wait. Live prerequisites are checked again before dispatch.

Automatic GroundStation abort/correction stops upon entry to Launch, including
countdown, as specified by the operating procedure. Manual Abort and FC's
independent recovery/watchdog behavior are unchanged. Existing ignition timing
is retained. HITL retains its existing full-testing behavior.

Software acceptance covers engine transitions and confirmations, restart
storage, prerequisite order, wrong/cached QD acknowledgements, target versus
stabilization cutoff, topology loss, cross-flow faults, steady valve correction
and the Launch boundary. It does not qualify physical valve timing, sensors,
wiring or the pressure system.
