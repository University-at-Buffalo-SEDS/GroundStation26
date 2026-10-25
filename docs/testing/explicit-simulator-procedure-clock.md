# Explicit simulator procedure clocks

This adapter is enabled only when the existing `GS_SIM_CLOCK_FILE` provides a
valid simulator monotonic clock. It does not change ordinary deployment or HITL
runs without that explicit clock. Authentication, telemetry receipt freshness,
UI connection timeouts, media retention and recording playback positions remain
on the host wall clock.

Procedure anchors and DAQ logging deadlines use the same model UTC epoch as host
network-time production (`GS_SIM_NETWORK_EPOCH_MS`, or the existing first-sample
epoch). This prevents a slow or paused MCU simulation from finishing a countdown
on host time and prevents late Ascent reports from moving DAQ's stop deadline.
The canonical launch clock remains anchored internally and is the only form
passed to DAQ. No SDK or on-wire schema changes are required.

API and WebSocket launch-clock snapshots in this explicit mode use the existing
unanchored duration fields to hold either remaining T-minus or elapsed T-plus.
A client samples at most every 250 ms, sends only changed samples, and sends no
additional clock messages while the model is paused. That path never calls
`set_launch_clock`, a DAQ publisher or the SEDSnet router. Actual clock changes
still use the normal publication path. Failed, partial and decreasing model
clock-file reads retain the last accepted monotonic value.

The audience programme computes its clock text using purpose time but retains
host-wall timestamps for media scheduling and replay retention. Dashboard T-plus
recognizes an unanchored elapsed sample and does not extrapolate it using browser
time. Ordinary anchored countdown, T-plus and missing-time behavior is retained.

## Saved recording boundary

A slow simulation has two time domains: model network UTC and host receipt time.
The recording's primary axis stays host receipt time, matching sensor rows and
camera retention. Explicit-model FLIGHT_STATE rows now use that same axis and
retain original model UTC in `source_timestamp_ms`. This lets saved trajectory
and phase queries find Ascent in the same window as its sensor data. Launch-clock
messages themselves are transient, not new persisted recording metadata.

Saved slow-model recordings therefore replay at receipt-time speed by default;
playback speed controls can adjust that. This change does not reinterpret old
recordings or merge virtual UTC anchors into receipt-time SQL windows.

## Qualification

Pure tests cover unchanged no-model clock fields, model pause/slow progression,
T-minus/T-plus presentation, stable DAQ deadlines and programme clock text. An
isolated child test uses temporary fake paths and an in-memory SQLite database to
exercise clock-file failure/rollback handling, late Ascent then pilot-open,
stale clock rejection, no presentation republication, and actual saved replay
phase queries aligned to sensor receipt timestamps.

The frontend's `tests/sampled-procedure-clock-browser.mjs` exercises real Wasm
against fake API/WebSocket endpoints: held T-minus/T-plus, explicit progression
and reconnect. No physical output commands or real settings are used.

Network qualification belongs to the separately frozen network image; these
procedure-clock tests are additional simulator-fidelity evidence. They do not
retroactively modify or replace the immutable network-soak source/binary proof.

## Operational Fire feedback

A completed fill stays in the engine's active Paused hold after Fire is committed.
Pilot feedback may advance an existing T-minus clock for that committed ordinary
Fire even while the hold still owns controls. An ordinary uncommitted Paused
fill/self-test keeps its guard; HITL retains the prior active-engine guard. This
changes only clock interpretation of actual pilot reports, not valve commands,
qualification, abort policy or firmware countdown timing.
