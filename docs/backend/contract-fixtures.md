# Frontend contract fixtures

The backend and the UI (Seds-Ground-Station-Frontend) are separate repositories
that agree on JSON shapes. `backend/src/contract_fixtures.rs` records the real
replies of the actual handlers and types, plus the websocket frames, in
`backend/tests/contract-fixtures/`. The UI decodes the same files with its own
serde types, so a shape change on either side fails a test instead of reaching
an operator. (Example: the station mode challenge shipped without
`"version": 2` and every in-app mode change failed with "Unsupported mode
challenge".)

Covered: station mode capability, review (in place and restart), confirmation
receipt, snapshot after a switch and the `StationModeChanged` frame;
`/api/station_mode`; `/api/board-recovery/status` (viewer and operator);
`/api/action_policy` and the `ActionPolicy` frame (with `network_note`);
`/api/alerts`, `/api/alerts/ack`, the `AlertAckState` frame (with
`history_cleared_through_ms`) and `/api/alerts/history/clear`;
`/api/network_diagnostics` `radio_tx_budget`; `/api/radio/modem`;
`/api/rrls/status`; `/api/auth/session` (signed in and anonymous) and
`/api/auth/challenge`.

Values that change from run to run (tickets, boot epochs, random ids, clock
readings, process-wide counters) are replaced by fixed values of the same type
and format. Routes that read process-wide state other tests change (radio modem,
RRLS radio) are recorded from their response type, and the test checks that the
route serves the same fields.

## Run

```bash
cargo test -p groundstation_backend contract_fixtures
```

The test fails when a checked-in fixture differs from what the backend now
produces, or when a fixture is no longer produced.

## Regenerate after an intended change

```bash
GS_UPDATE_CONTRACT_FIXTURES=1 cargo test -p groundstation_backend contract_fixtures
git diff backend/tests/contract-fixtures
```

Commit the fixtures with the change, then in the UI repository refresh its copy
and run its contract tests:

```bash
python3 scripts/check_backend_contract_fixtures.py --backend ../GroundStation26/backend/tests/contract-fixtures --update
cargo test backend_contract -- --test-threads=1
```

Or point the UI tests at this directory without copying:

```bash
GS_CONTRACT_FIXTURES_DIR=../GroundStation26/backend/tests/contract-fixtures cargo test backend_contract -- --test-threads=1
```

A new fixture must also get a decoding test in the UI (its
`src/backend_contract.rs` lists every covered fixture and fails on any other).
