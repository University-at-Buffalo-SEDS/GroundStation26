# Mission-setting history prototype

This is the earlier standalone history prototype, retained with its tests.
Production paired-station synchronization uses `shared/paired_settings` and
`backend/src/mission_sync.rs`, including the integrated conflict UI and radio
budget. This prototype is not a backend dependency. The integration work
described below is historical context for this prototype.

Bounded, per-setting causal revisions for paired GroundStation/RRLS offline
edits. Independent settings merge. Concurrent values remain separate heads
until the operator explicitly resolves the reviewed versions; stale resolution
requests fail. Revision replay is idempotent and cannot restore an older value.
Stores retain their installation identity and persist through atomic replacement.

This crate does not authorize peers, validate calibration physics, publish
settings or actuate outputs. The backend must authenticate incoming documents,
validate typed values, apply them only at a safe boundary and expose conflicts.
Those runtime integration and radio qualification gates remain on the active
work plan; passing these history tests alone does not establish synchronization.

The anti-entropy transport sends a small inventory while stable, catalogs only
when revisions differ, and requests only mismatched documents. Document chunks
are512 bytes, with at most four outbound transfers and three inbound assemblies
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
conflicting heads. Runtime adapters and the conflict UI are still required.
