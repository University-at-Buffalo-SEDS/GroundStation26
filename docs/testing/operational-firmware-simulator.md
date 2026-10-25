# Operational firmware simulator boundary

`firmware_sim` enables only the existing explicit `GS_SIM_CLOCK_FILE` clock
adapter. It is independent of `hitl_mode`: `test_fire_mode,firmware_sim` retains
the ordinary GSE operational qualification, persistence, checklist, valve, route,
pressure and launch gates. It does not turn on `testing` or fabricate telemetry.
Without the explicit clock file, ordinary wall timing remains unchanged.

Operational board liveness in an explicit model uses the monotonic time of
actual received board packets, stored alongside existing board receipt state.
Pausing or slowing the simulator therefore does not age a received board on host
wall time. Advancing virtual time beyond the existing3000ms limit still fails
the gate; receiving a real packet renews it. UI/authentication/recording and
ordinary board health age remain on their existing wall clock. No wall-to-model
age arithmetic or direct qualification override is used.

The isolated clock test covers a paused model, expiry after3001virtual ms,
renewal through `mark_board_seen`, and malformed/backward clock-file reads. An
isolated no-clock subprocess checks wall fallback even when this feature is
compiled. Existing HITL behavior remains separate and unchanged.

A linked operational acceptance scenario must supply real firmware ADC samples,
learned routes, command acknowledgements and persisted procedure receipts. A
sensor stimulus represents assumed plant behavior only; it cannot establish
physical pneumatic calibration, valve timing, pressure stability or field
safety. Do not describe host helper tests or the HITL network startup as a full
linked operational procedure acceptance run.
