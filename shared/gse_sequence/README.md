# Deterministic GSE procedures

This crate issues desired valve operations; it never operates transport or hardware.
Callers must retain fresh topology and hazardous-flow supervision, including after
manual cancellation, and must feed fresh board valve reports rather than delivery ACKs.
These reports may reflect commanded servo state; they do not prove mechanical position.

`Config.nitrogen_steps` optionally replaces the legacy pressure increments. Each
step saves `target_psi`, `settle_ms`, and `hold_ms`. Targets ascend strictly; there
are at most 32 steps, each wait is 1–600 seconds, and total waits are at most one
hour. The last step is the pressure target. An empty list preserves the existing
120 psi target / 50 psi increments and 2-second settle / 5-second hold defaults.
Baseline, hardware-ceiling, leak, pressure-freshness and interlock checks remain.

Normal target changes dispatch one valve at a time: supply closes, remaining
closes, then requested opens. Every dispatched operation requires a fresh board
report at or after dispatch within five seconds. Waiting does not count toward
the settle/hold period. Emergency faults request all safety outputs immediately.

`fill_trim::Controller` begins after the network fill cutoff reaches the signed
mass threshold `target + target.signum() * fill_overfill_kg`. It closes supply and
vent, waits for all acknowledgements, and then waits `fill_settle_ms` (default
3 seconds). A fresh, stable one-second mass window and latest sample must agree
with the target within `fill_tolerance_kg` (default ±0.07 kg). Overshoot opens vent
for a 100 ms pulse measured from its acknowledgement, closes it, and settles again.
Twenty pulses or failure to settle ends in a fault. Underfill requires manual
review; the controller never reopens supply. Completion requires every valve
closed. Overfill allowance is capped at 0.1 kg; physical stopping overshoot depends
on sensor/transport/valve response and cannot be guaranteed by a command threshold.

Manual override emits no outputs and cancels pending automation; it preserves
nitrogen qualification, self-test lock and the nitrous-session hazard latch.
Settings edits use targeted qualification invalidation and increment the generation
so a late result from an earlier run cannot restore invalid evidence. The backend
must invalidate linked checklist receipts and persist the resulting state.

Run `cargo test -p gse_sequence --lib` from the workspace. These deterministic
fault/ACK simulations do not replace hardware procedure qualification.
