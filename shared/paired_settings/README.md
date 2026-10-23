# Mission-setting history

Bounded, per-setting causal revisions for paired GroundStation/RRLS offline
edits. Independent settings merge. Concurrent values remain separate heads
until the operator explicitly resolves the reviewed versions; stale resolution
requests fail. Revision replay is idempotent and cannot restore an older value.
Stores retain their installation identity and persist through atomic replacement.

This crate does not authorize peers, validate calibration physics, publish
settings or actuate outputs. The backend must authenticate incoming documents,
validate typed values, apply them only at a safe boundary and expose conflicts.
The runtime adapter lives in `backend/src/mission_sync.rs`; its conflict UI
lives in the frontend Settings. Passing history tests alone does not establish
end-to-end radio or firmware qualification.

The anti-entropy transport sends a small inventory while stable, catalogs only
when revisions differ, and requests only mismatched documents. Document chunks
are 128 bytes, with at most four outbound transfers and three inbound assemblies
per peer; incomplete assemblies expire. Lost, repeated or reordered chunks are
safe and retry through later inventory comparison. The backend still must sign
messages, enforce a shared low-priority radio budget and validate typed values
before merging a completed candidate. It must not send video through this path.

Application envelopes bind installation, issuer and recipient using a distinct
Ed25519 signature domain. They reject unpaired issuers, wrong keys, tampering,
wrong recipients and oversized bodies before the protocol accepts a message.
The installation pairing key is separate from the server HTTPS identity.
Document/catalog hashes canonicalize nested JSON object keys.

The coordinator records local observations separately from imported revisions.
RRLS defaults cannot overwrite primary calibration. A remote value is returned
as pending; the backend must validate it and compare the expected local value
under its settings lock before applying, then checkpoint. A service restart
between application and checkpoint recognizes the imported matching value
without manufacturing another edit. Local edits racing reconnect remain
conflicting heads. The backend adapter validates each supported calibration
document and holds its settings lock across durable compare-and-apply.

The radio scheduler shares one 256-byte/second allowance across configured
peers, measured from actual signed envelope lengths. It retains one pending
frame per peer on backpressure, rotates peers after each attempt, and cannot
accumulate credit during idle periods. Failed sends are paced too. This module
is wired into the backend mesh below commands, GPS, voice and telemetry.
Full paired firmware-simulator qualification remains a release gate.
