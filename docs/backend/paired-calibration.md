# Paired settings

GroundStation and RRLS exchange channel-scoped calibration and saved mission
documents using the existing station pairing identity. The application signing key is independent
of HTTP, TLS certificates and reverse proxies. Configure each station with its
own `GS_STATION_PAIRING_FILE`; paired station IDs and installation trust must
match. Private signing files remain on their servers.

Each station keeps durable causal history in
`data/mission-settings-<station-id>.json`. `GS_MISSION_SETTINGS_PATH` overrides
that path. Preserve this file alongside the local calibration file across
service restarts. The primary initially publishes its saved calibration and
mission settings; RRLS initial defaults do not overwrite it. Edits on either disconnected station
become local revisions. Concurrent edits are retained until an authorized user
chooses a revision in Settings → Paired settings. The choice includes the
exact reviewed heads: a changed conflict requires a fresh review.

The calibration adapter supports the 1000 kg load cell, tank pressure, the
50 kg load cell with its physical-input profiles, and the full-mass reference.
Saved mission settings include:

- Fill targets and the selected fill source.
- GSE pressure limits and panel grouping.
- Saved flight profiles and the selected profile.
- The auto-zero preference.
- Saved GroundStation and RRLS positions.

Device configuration, credentials, user permissions, abort latches and active
sequence state are not imported. Shared checklist definitions and progress use
a separate durable journal over the same paired radio allowance; see
[paired checklists](paired-checklists.md). Importing GSE limits does not import
the dry self-test confirmation or other operational qualification.

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
- `GET /api/mission-settings/conflicts`: view-data permission required;
  calibration documents additionally require calibration-view permission.
- `POST /api/mission-settings/resolve`: view-data permission plus calibration-edit
  permission for calibration, administrator access for saved positions, or
  send-commands permission for the other saved mission settings. The body includes
  `key`, exact `expected` document, and selected `value`.

The allowance counts signed envelopes, including inventories and chunk headers.
Cold-start history transfer can therefore take minutes when calibration,
checklists and saved profiles share the link. API readiness and a locally ready
checklist journal do not by themselves prove that the peer has received its
initial history. Command and network-variable receipt deadlines are separate
from this background settings allowance.

Native and real-Wasm browser checks cover the typed adapters and conflict UI.
Whole-system reconnect, long-run and physical-device checks remain separate
acceptance requirements.

Run the isolated service acceptance against a normal native backend:

```sh
cargo build --locked -p groundstation_backend
python3 tests/paired_mission_runtime_acceptance.py --binary target/debug/groundstation_backend
```

The harness starts two separate processes on loopback and connects synthetic
PTYs. It checks all nine initial documents, checklist progress, independent
offline edits, concurrent same-setting conflicts, both service restarts,
reviewed resolution and stale-resolution rejection. Each run uses disposable
state and an artificial operator account; it does not open physical devices.
Allow several minutes for the paced background histories. Progress and artifacts
are written to a temporary directory, or the new directory supplied with
`--artifacts-dir`; keep them outside tracked source.
