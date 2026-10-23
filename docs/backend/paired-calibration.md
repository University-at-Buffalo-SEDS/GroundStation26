# Paired calibration

GroundStation and RRLS exchange channel-scoped calibration documents using the
existing station pairing identity. The application signing key is independent
of HTTP, TLS certificates and reverse proxies. Configure each station with its
own `GS_STATION_PAIRING_FILE`; paired station IDs and installation trust must
match. Private signing files remain on their servers.

Each station keeps durable causal history in
`data/mission-settings-<station-id>.json`. `GS_MISSION_SETTINGS_PATH` overrides
that path. Preserve this file alongside the local calibration file across
service restarts. The primary initially publishes its saved calibration; RRLS
initial defaults do not overwrite it. Edits on either disconnected station
become local revisions. Concurrent edits are retained until an authorized user
chooses a revision in Settings → Paired calibration. The choice includes the
exact reviewed heads: a changed conflict requires a fresh review.

The current adapter supports the 1000 kg load cell, tank pressure, the 50 kg
load cell with its physical-input profiles, and the full-mass reference. It
does not synchronize device configuration, credentials, user permissions,
abort latches, sequence completion evidence, or other mission settings yet.

Received calibration is validated before merging and written durably before
changing live state. Application waits for Idle, no running sequence/zero
capture, and cleared sequence qualification. A physical 50 kg input change
also waits until recording is Idle and clears old source samples. Importing
calibration never marks a self-test or nitrogen test complete. HITL does not
start this synchronization worker.

The radio scheduler shares a 256-byte/second signed-envelope allowance across
paired peers, retains bounded pending work on backpressure, and queues settings
below commands, GPS, voice and live telemetry. A disconnected peer cannot
consume an unbounded queue. Standby RRLS can still exchange settings without
activating fill-system command forwarding.

Authenticated API endpoints:

- `GET /api/mission-settings`: enabled/link/pending/conflict/deferred status.
- `GET /api/mission-settings/conflicts`: calibration-view permission required.
- `POST /api/mission-settings/resolve`: calibration-edit permission required;
  body includes `key`, exact `expected` document, and selected `value`.

These changes have isolated native and real-Wasm browser tests. Full paired
firmware-simulator acceptance and source promotion are still required before
claiming release qualification.
